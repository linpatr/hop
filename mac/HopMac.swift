// Mac platform layer for Hop: screen/audio capture (ScreenCaptureKit), H.264 encode/decode
// (VideoToolbox), input injection (CGEvent) and permission helpers. Exposed to Rust as C functions.

import AppKit
import ApplicationServices
import CoreGraphics
import CoreMedia
import CoreVideo
import Foundation
import IOKit.pwr_mgt
import QuartzCore
import ScreenCaptureKit
import VideoToolbox

public typealias HopVideoCB = @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<UInt8>?, Int, Int32, UInt64) -> Void
public typealias HopAudioCB = @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<Int16>?, Int) -> Void
public typealias HopDecodedCB = @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<UInt8>?, Int, UnsafePointer<UInt8>?, Int, Int32, Int32) -> Void

private func nowUs() -> UInt64 { UInt64(CACurrentMediaTime() * 1_000_000) }

@_cdecl("hop_mac_now_us")
public func hop_mac_now_us() -> UInt64 { nowUs() }

private func writeErr(_ msg: String, _ buf: UnsafeMutablePointer<CChar>?, _ len: Int) {
    guard let buf, len > 0 else { return }
    let bytes = Array(msg.utf8.prefix(len - 1))
    for (i, b) in bytes.enumerated() { buf[i] = CChar(bitPattern: b) }
    buf[bytes.count] = 0
}

struct HopError: Error, CustomStringConvertible {
    let description: String
    init(_ d: String) { description = d }
}

// MARK: - Display info

@_cdecl("hop_mac_display_size")
public func hop_mac_display_size(_ w: UnsafeMutablePointer<Int32>, _ h: UnsafeMutablePointer<Int32>) {
    let id = CGMainDisplayID()
    if let mode = CGDisplayCopyDisplayMode(id) {
        w.pointee = Int32(mode.pixelWidth)
        h.pointee = Int32(mode.pixelHeight)
    } else {
        w.pointee = Int32(CGDisplayPixelsWide(id))
        h.pointee = Int32(CGDisplayPixelsHigh(id))
    }
}

// MARK: - Host: capture + encode

final class HostSession: NSObject, SCStreamOutput, SCStreamDelegate {
    let ctx: UnsafeMutableRawPointer?
    let videoCB: HopVideoCB
    let audioCB: HopAudioCB
    let width: Int32, height: Int32, fps: Int32
    var bitrateKbps: Int32
    let wantAudio: Bool

    var stream: SCStream?
    var encoder: VTCompressionSession?
    let videoQueue = DispatchQueue(label: "hop.capture.video", qos: .userInteractive)
    let audioQueue = DispatchQueue(label: "hop.capture.audio", qos: .userInteractive)
    let lock = NSLock()
    var stopped = false
    var forceKey = true
    var lastPixelBuffer: CVPixelBuffer?
    var lastPts = CMTime.zero
    var out = [UInt8]()
    var pcm = [Int16]()

    init(w: Int32, h: Int32, fps: Int32, kbps: Int32, audio: Bool, ctx: UnsafeMutableRawPointer?, v: @escaping HopVideoCB, a: @escaping HopAudioCB) {
        self.width = w; self.height = h; self.fps = fps; self.bitrateKbps = kbps
        self.wantAudio = audio; self.ctx = ctx; self.videoCB = v; self.audioCB = a
    }

    func start() throws {
        try setupEncoder()

        let sem = DispatchSemaphore(value: 0)
        var content: SCShareableContent?
        var err: Error?
        SCShareableContent.getExcludingDesktopWindows(false, onScreenWindowsOnly: true) { c, e in
            content = c; err = e; sem.signal()
        }
        sem.wait()
        guard let content else {
            throw HopError("Screen Recording permission is needed (\(err.map { "\($0)" } ?? "no content"))")
        }
        let mainID = CGMainDisplayID()
        guard let display = content.displays.first(where: { $0.displayID == mainID }) ?? content.displays.first else {
            throw HopError("no display found")
        }
        let filter = SCContentFilter(display: display, excludingApplications: [], exceptingWindows: [])
        let cfg = SCStreamConfiguration()
        cfg.width = Int(width)
        cfg.height = Int(height)
        cfg.minimumFrameInterval = CMTime(value: 1, timescale: CMTimeScale(fps))
        cfg.pixelFormat = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        cfg.colorMatrix = CGDisplayStream.yCbCrMatrix_ITU_R_709_2
        cfg.colorSpaceName = CGColorSpace.itur_709
        cfg.showsCursor = true
        cfg.queueDepth = 5
        cfg.scalesToFit = true
        if wantAudio {
            cfg.capturesAudio = true
            cfg.sampleRate = 48000
            cfg.channelCount = 2
            cfg.excludesCurrentProcessAudio = true
        }
        let s = SCStream(filter: filter, configuration: cfg, delegate: self)
        try s.addStreamOutput(self, type: .screen, sampleHandlerQueue: videoQueue)
        if wantAudio {
            try s.addStreamOutput(self, type: .audio, sampleHandlerQueue: audioQueue)
        }
        var startErr: Error?
        s.startCapture { e in startErr = e; sem.signal() }
        sem.wait()
        if let startErr { throw HopError("startCapture failed: \(startErr)") }
        stream = s
    }

    func setupEncoder() throws {
        let env = ProcessInfo.processInfo.environment
        var spec: [CFString: Any] = [:]
        // Apple's low-latency rate control measured ~1.7 ms slower per frame on M4, so it is opt-in.
        if env["HOP_VT_LL"] != nil { spec[kVTVideoEncoderSpecification_EnableLowLatencyRateControl] = true }
        var session: VTCompressionSession?
        let st = VTCompressionSessionCreate(
            allocator: nil, width: width, height: height, codecType: kCMVideoCodecType_H264,
            encoderSpecification: spec as CFDictionary, imageBufferAttributes: nil,
            compressedDataAllocator: nil, outputCallback: nil, refcon: nil, compressionSessionOut: &session)
        guard st == noErr, let session else { throw HopError("VTCompressionSessionCreate failed: \(st)") }
        func set(_ k: CFString, _ v: Any) { VTSessionSetProperty(session, key: k, value: v as CFTypeRef) }
        set(kVTCompressionPropertyKey_RealTime, kCFBooleanTrue!)
        set(kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse!)
        if VTSessionSetProperty(session, key: kVTCompressionPropertyKey_ProfileLevel, value: kVTProfileLevel_H264_ConstrainedHigh_AutoLevel) != noErr {
            set(kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_H264_High_AutoLevel)
        }
        set(kVTCompressionPropertyKey_AverageBitRate, NSNumber(value: Int(bitrateKbps) * 1000))
        set(kVTCompressionPropertyKey_ExpectedFrameRate, NSNumber(value: fps))
        set(kVTCompressionPropertyKey_MaxKeyFrameInterval, NSNumber(value: Int(fps) * 3600))
        set(kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, NSNumber(value: 3600))
        set(kVTCompressionPropertyKey_ColorPrimaries, kCVImageBufferColorPrimaries_ITU_R_709_2)
        set(kVTCompressionPropertyKey_TransferFunction, kCVImageBufferTransferFunction_ITU_R_709_2)
        set(kVTCompressionPropertyKey_YCbCrMatrix, kCVImageBufferYCbCrMatrix_ITU_R_709_2)
        if env["HOP_VT_NOSPEED"] == nil { set(kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality, kCFBooleanTrue!) }
        if let mfd = env["HOP_VT_MFD"], let n = Int(mfd) { set(kVTCompressionPropertyKey_MaxFrameDelayCount, NSNumber(value: n)) }
        VTCompressionSessionPrepareToEncodeFrames(session)
        encoder = session
    }

    func setBitrate(_ kbps: Int32) {
        lock.lock(); defer { lock.unlock() }
        bitrateKbps = kbps
        if let encoder {
            VTSessionSetProperty(encoder, key: kVTCompressionPropertyKey_AverageBitRate, value: NSNumber(value: Int(kbps) * 1000))
        }
    }

    func requestKeyframe() {
        lock.lock(); forceKey = true; lock.unlock()
        // If the screen is static no new frame will arrive, so re-encode the last one.
        videoQueue.asyncAfter(deadline: .now() + .milliseconds(25)) { [weak self] in
            guard let self else { return }
            self.lock.lock()
            let pending = self.forceKey && !self.stopped
            let pb = self.lastPixelBuffer
            self.lock.unlock()
            if pending, let pb {
                self.encode(pb, captureUs: nowUs())
            }
        }
    }

    func stop() {
        lock.lock(); stopped = true; lock.unlock()
        if let stream {
            let sem = DispatchSemaphore(value: 0)
            stream.stopCapture { _ in sem.signal() }
            _ = sem.wait(timeout: .now() + 2)
        }
        stream = nil
        if let encoder {
            VTCompressionSessionCompleteFrames(encoder, untilPresentationTimeStamp: .invalid)
            VTCompressionSessionInvalidate(encoder)
        }
        encoder = nil
        videoQueue.sync {}
        audioQueue.sync {}
        lock.lock(); lastPixelBuffer = nil; lock.unlock()
    }

    func stream(_ stream: SCStream, didStopWithError error: Error) {
        NSLog("hop: capture stopped: \(error)")
    }

    func stream(_ stream: SCStream, didOutputSampleBuffer sb: CMSampleBuffer, of type: SCStreamOutputType) {
        guard sb.isValid else { return }
        switch type {
        case .screen: handleVideo(sb)
        case .audio: handleAudio(sb)
        default: break
        }
    }

    func handleVideo(_ sb: CMSampleBuffer) {
        guard let arr = CMSampleBufferGetSampleAttachmentsArray(sb, createIfNecessary: false) as? [[SCStreamFrameInfo: Any]],
              let raw = arr.first?[.status] as? Int,
              let status = SCFrameStatus(rawValue: raw), status == .complete,
              let pb = CMSampleBufferGetImageBuffer(sb)
        else { return }
        encode(pb, captureUs: nowUs())
    }

    func encode(_ pb: CVPixelBuffer, captureUs: UInt64) {
        lock.lock()
        if stopped { lock.unlock(); return }
        let fk = forceKey
        forceKey = false
        lastPixelBuffer = pb
        var pts = CMClockGetTime(CMClockGetHostTimeClock())
        if CMTimeCompare(pts, lastPts) <= 0 { pts = CMTimeAdd(lastPts, CMTime(value: 1, timescale: 1_000_000)) }
        lastPts = pts
        let enc = encoder
        lock.unlock()
        guard let enc else { return }
        let props: CFDictionary? = fk ? ([kVTEncodeFrameOptionKey_ForceKeyFrame: true] as CFDictionary) : nil
        VTCompressionSessionEncodeFrame(enc, imageBuffer: pb, presentationTimeStamp: pts, duration: .invalid,
                                        frameProperties: props, infoFlagsOut: nil) { [weak self] status, _, sbuf in
            guard status == noErr, let sbuf, let self else { return }
            self.emit(sbuf, captureUs: captureUs)
        }
    }

    func emit(_ sb: CMSampleBuffer, captureUs: UInt64) {
        guard let bb = CMSampleBufferGetDataBuffer(sb) else { return }
        var isKey = true
        if let arr = CMSampleBufferGetSampleAttachmentsArray(sb, createIfNecessary: false) as? [[CFString: Any]],
           let notSync = arr.first?[kCMSampleAttachmentKey_NotSync] as? Bool {
            isKey = !notSync
        }
        lock.lock()
        defer { lock.unlock() }
        if stopped { return }
        out.removeAll(keepingCapacity: true)
        let startCode: [UInt8] = [0, 0, 0, 1]
        if isKey, let fd = CMSampleBufferGetFormatDescription(sb) {
            var count = 0
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(fd, parameterSetIndex: 0, parameterSetPointerOut: nil,
                                                               parameterSetSizeOut: nil, parameterSetCountOut: &count,
                                                               nalUnitHeaderLengthOut: nil)
            for i in 0..<count {
                var p: UnsafePointer<UInt8>?
                var n = 0
                if CMVideoFormatDescriptionGetH264ParameterSetAtIndex(fd, parameterSetIndex: i, parameterSetPointerOut: &p,
                                                                      parameterSetSizeOut: &n, parameterSetCountOut: nil,
                                                                      nalUnitHeaderLengthOut: nil) == noErr, let p {
                    out.append(contentsOf: startCode)
                    out.append(contentsOf: UnsafeBufferPointer(start: p, count: n))
                }
            }
        }
        let total = CMBlockBufferGetDataLength(bb)
        var avcc = [UInt8](repeating: 0, count: total)
        guard CMBlockBufferCopyDataBytes(bb, atOffset: 0, dataLength: total, destination: &avcc) == noErr else { return }
        var i = 0
        while i + 4 <= total {
            let n = Int(avcc[i]) << 24 | Int(avcc[i + 1]) << 16 | Int(avcc[i + 2]) << 8 | Int(avcc[i + 3])
            i += 4
            if n <= 0 || i + n > total { break }
            out.append(contentsOf: startCode)
            out.append(contentsOf: avcc[i..<(i + n)])
            i += n
        }
        out.withUnsafeBufferPointer { p in
            videoCB(ctx, p.baseAddress, p.count, isKey ? 1 : 0, captureUs)
        }
    }

    func handleAudio(_ sb: CMSampleBuffer) {
        guard let fd = CMSampleBufferGetFormatDescription(sb),
              let asbdPtr = CMAudioFormatDescriptionGetStreamBasicDescription(fd) else { return }
        let asbd = asbdPtr.pointee
        guard asbd.mFormatFlags & kAudioFormatFlagIsFloat != 0, asbd.mBitsPerChannel == 32 else { return }
        let frames = CMSampleBufferGetNumSamples(sb)
        if frames <= 0 { return }
        let chans = Int(asbd.mChannelsPerFrame)
        let nonInterleaved = asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0
        let ablPtr = AudioBufferList.allocate(maximumBuffers: max(chans, 1))
        defer { free(ablPtr.unsafeMutablePointer) }
        var block: CMBlockBuffer?
        let st = CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
            sb, bufferListSizeNeededOut: nil, bufferListOut: ablPtr.unsafeMutablePointer,
            bufferListSize: AudioBufferList.sizeInBytes(maximumBuffers: max(chans, 1)),
            blockBufferAllocator: nil, blockBufferMemoryAllocator: nil,
            flags: kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment, blockBufferOut: &block)
        guard st == noErr else { return }
        pcm.removeAll(keepingCapacity: true)
        pcm.reserveCapacity(frames * 2)
        func s16(_ f: Float) -> Int16 { Int16(max(-1, min(1, f)) * 32767) }
        if nonInterleaved {
            guard let l = ablPtr[0].mData?.assumingMemoryBound(to: Float.self) else { return }
            let r = (chans > 1 ? ablPtr[1].mData?.assumingMemoryBound(to: Float.self) : nil) ?? l
            for i in 0..<frames { pcm.append(s16(l[i])); pcm.append(s16(r[i])) }
        } else {
            guard let d = ablPtr[0].mData?.assumingMemoryBound(to: Float.self) else { return }
            for i in 0..<frames {
                let l = d[i * chans]
                let r = chans > 1 ? d[i * chans + 1] : l
                pcm.append(s16(l)); pcm.append(s16(r))
            }
        }
        lock.lock()
        let stop = stopped
        lock.unlock()
        if stop { return }
        pcm.withUnsafeBufferPointer { p in audioCB(ctx, p.baseAddress, frames) }
    }
}

@_cdecl("hop_mac_host_start")
public func hop_mac_host_start(_ w: Int32, _ h: Int32, _ fps: Int32, _ kbps: Int32, _ audio: Int32,
                               _ ctx: UnsafeMutableRawPointer?, _ vcb: HopVideoCB, _ acb: HopAudioCB,
                               _ err: UnsafeMutablePointer<CChar>?, _ errLen: Int) -> UnsafeMutableRawPointer? {
    let s = HostSession(w: w, h: h, fps: fps, kbps: kbps, audio: audio != 0, ctx: ctx, v: vcb, a: acb)
    do {
        try s.start()
    } catch {
        s.stop()
        writeErr("\(error)", err, errLen)
        return nil
    }
    return Unmanaged.passRetained(s).toOpaque()
}

@_cdecl("hop_mac_host_stop")
public func hop_mac_host_stop(_ h: UnsafeMutableRawPointer) {
    let s = Unmanaged<HostSession>.fromOpaque(h).takeRetainedValue()
    s.stop()
}

@_cdecl("hop_mac_host_request_keyframe")
public func hop_mac_host_request_keyframe(_ h: UnsafeMutableRawPointer) {
    Unmanaged<HostSession>.fromOpaque(h).takeUnretainedValue().requestKeyframe()
}

@_cdecl("hop_mac_host_set_bitrate")
public func hop_mac_host_set_bitrate(_ h: UnsafeMutableRawPointer, _ kbps: Int32) {
    Unmanaged<HostSession>.fromOpaque(h).takeUnretainedValue().setBitrate(kbps)
}

// MARK: - Client: decode

final class Decoder {
    let ctx: UnsafeMutableRawPointer?
    let cb: HopDecodedCB
    var session: VTDecompressionSession?
    var format: CMVideoFormatDescription?
    var sps = [UInt8]()
    var pps = [UInt8]()
    var avcc = [UInt8]()

    init(ctx: UnsafeMutableRawPointer?, cb: @escaping HopDecodedCB) { self.ctx = ctx; self.cb = cb }

    func reset() {
        if let session {
            VTDecompressionSessionWaitForAsynchronousFrames(session)
            VTDecompressionSessionInvalidate(session)
        }
        session = nil
        format = nil
    }

    func makeSession() -> Bool {
        reset()
        var fd: CMFormatDescription?
        let st = sps.withUnsafeBufferPointer { s in
            pps.withUnsafeBufferPointer { p in
                let ptrs = [s.baseAddress!, p.baseAddress!]
                let sizes = [s.count, p.count]
                return CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    allocator: nil, parameterSetCount: 2, parameterSetPointers: ptrs, parameterSetSizes: sizes,
                    nalUnitHeaderLength: 4, formatDescriptionOut: &fd)
            }
        }
        guard st == noErr, let fd else { NSLog("hop: bad parameter sets \(st)"); return false }
        let attrs: [CFString: Any] = [
            kCVPixelBufferPixelFormatTypeKey: kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            kCVPixelBufferIOSurfacePropertiesKey: [:] as [CFString: Any],
        ]
        let spec: [CFString: Any] = [
            kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder: true,
        ]
        var s: VTDecompressionSession?
        let st2 = VTDecompressionSessionCreate(allocator: nil, formatDescription: fd, decoderSpecification: spec as CFDictionary,
                                               imageBufferAttributes: attrs as CFDictionary, outputCallback: nil,
                                               decompressionSessionOut: &s)
        guard st2 == noErr, let s else { NSLog("hop: VTDecompressionSessionCreate failed \(st2)"); return false }
        VTSessionSetProperty(s, key: kVTDecompressionPropertyKey_RealTime, value: kCFBooleanTrue)
        session = s
        format = fd
        return true
    }

    // Returns 0 = ok, 1 = need keyframe (no session yet), negative = decode error.
    func decode(_ data: UnsafePointer<UInt8>, _ len: Int) -> Int32 {
        avcc.removeAll(keepingCapacity: true)
        var newSps: [UInt8]?
        var newPps: [UInt8]?
        var hasIdr = false
        // Split Annex B into NAL units.
        var i = 0
        var starts = [(Int, Int)]()  // (payload start, start code length)
        while i + 3 <= len {
            if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                starts.append((i + 3, 3)); i += 3
            } else if i + 4 <= len && data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0 && data[i + 3] == 1 {
                starts.append((i + 4, 4)); i += 4
            } else {
                i += 1
            }
        }
        for (idx, (s, _)) in starts.enumerated() {
            var e = len
            if idx + 1 < starts.count { e = starts[idx + 1].0 - starts[idx + 1].1 }
            // Trim trailing zero bytes (part of the next start code).
            while e > s && data[e - 1] == 0 { e -= 1 }
            let n = e - s
            if n <= 0 { continue }
            let type = data[s] & 0x1F
            switch type {
            case 7: newSps = Array(UnsafeBufferPointer(start: data + s, count: n))
            case 8: newPps = Array(UnsafeBufferPointer(start: data + s, count: n))
            case 9, 10, 11, 12: break
            default:
                if type == 5 { hasIdr = true }
                avcc.append(UInt8((n >> 24) & 0xFF)); avcc.append(UInt8((n >> 16) & 0xFF))
                avcc.append(UInt8((n >> 8) & 0xFF)); avcc.append(UInt8(n & 0xFF))
                avcc.append(contentsOf: UnsafeBufferPointer(start: data + s, count: n))
            }
        }
        if let newSps, let newPps, newSps != sps || newPps != pps || session == nil {
            sps = newSps; pps = newPps
            if !makeSession() { return -1 }
        }
        guard let session, let format else { return 1 }
        if avcc.isEmpty { return 0 }
        _ = hasIdr

        var block: CMBlockBuffer?
        var st = CMBlockBufferCreateWithMemoryBlock(allocator: nil, memoryBlock: nil, blockLength: avcc.count,
                                                    blockAllocator: nil, customBlockSource: nil, offsetToData: 0,
                                                    dataLength: avcc.count, flags: 0, blockBufferOut: &block)
        guard st == noErr, let block else { return -2 }
        st = avcc.withUnsafeBytes { CMBlockBufferReplaceDataBytes(with: $0.baseAddress!, blockBuffer: block, offsetIntoDestination: 0, dataLength: avcc.count) }
        guard st == noErr else { return -2 }
        var sb: CMSampleBuffer?
        var size = avcc.count
        st = CMSampleBufferCreateReady(allocator: nil, dataBuffer: block, formatDescription: format, sampleCount: 1,
                                       sampleTimingEntryCount: 0, sampleTimingArray: nil, sampleSizeEntryCount: 1,
                                       sampleSizeArray: &size, sampleBufferOut: &sb)
        guard st == noErr, let sb else { return -2 }
        if let arr = CMSampleBufferGetSampleAttachmentsArray(sb, createIfNecessary: true) as NSArray?,
           let dict = arr.firstObject as? NSMutableDictionary {
            dict[kCMSampleAttachmentKey_DisplayImmediately] = true
        }
        var result: Int32 = 0
        let dst = VTDecompressionSessionDecodeFrame(session, sampleBuffer: sb, flags: [], infoFlagsOut: nil) { [self] status, _, image, _, _ in
            guard status == noErr, let image else { result = -3; return }
            self.deliver(image)
        }
        if dst == kVTInvalidSessionErr {
            reset()
            return 1
        }
        if dst != noErr { return -4 }
        VTDecompressionSessionWaitForAsynchronousFrames(session)
        return result
    }

    func deliver(_ pb: CVPixelBuffer) {
        CVPixelBufferLockBaseAddress(pb, .readOnly)
        defer { CVPixelBufferUnlockBaseAddress(pb, .readOnly) }
        guard CVPixelBufferGetPlaneCount(pb) >= 2,
              let y = CVPixelBufferGetBaseAddressOfPlane(pb, 0),
              let uv = CVPixelBufferGetBaseAddressOfPlane(pb, 1) else { return }
        cb(ctx, y.assumingMemoryBound(to: UInt8.self), CVPixelBufferGetBytesPerRowOfPlane(pb, 0),
           uv.assumingMemoryBound(to: UInt8.self), CVPixelBufferGetBytesPerRowOfPlane(pb, 1),
           Int32(CVPixelBufferGetWidth(pb)), Int32(CVPixelBufferGetHeight(pb)))
    }
}

@_cdecl("hop_mac_dec_create")
public func hop_mac_dec_create(_ ctx: UnsafeMutableRawPointer?, _ cb: HopDecodedCB) -> UnsafeMutableRawPointer {
    Unmanaged.passRetained(Decoder(ctx: ctx, cb: cb)).toOpaque()
}

@_cdecl("hop_mac_dec_decode")
public func hop_mac_dec_decode(_ h: UnsafeMutableRawPointer, _ data: UnsafePointer<UInt8>, _ len: Int) -> Int32 {
    Unmanaged<Decoder>.fromOpaque(h).takeUnretainedValue().decode(data, len)
}

@_cdecl("hop_mac_dec_destroy")
public func hop_mac_dec_destroy(_ h: UnsafeMutableRawPointer) {
    let d = Unmanaged<Decoder>.fromOpaque(h).takeRetainedValue()
    d.reset()
}

// MARK: - Input injection

final class Injector {
    static let shared = Injector()
    let src = CGEventSource(stateID: .hidSystemState)
    var pos = CGPoint.zero
    var buttons = [Bool](repeating: false, count: 5)
    var flags = CGEventFlags()
    var lastClick = [TimeInterval](repeating: 0, count: 5)
    var clickCount = [Int64](repeating: 0, count: 5)
    var lastClickPos = CGPoint.zero
    var downKeys = Set<UInt16>()
    var scrollRemainder = CGPoint.zero
    let lock = NSLock()

    init() {
        pos = CGEvent(source: nil)?.location ?? .zero
    }

    var bounds: CGRect { CGDisplayBounds(CGMainDisplayID()) }

    func clamp(_ p: CGPoint) -> CGPoint {
        let b = bounds
        return CGPoint(x: min(max(p.x, b.minX), b.maxX - 1), y: min(max(p.y, b.minY), b.maxY - 1))
    }

    func postMove(dx: Int64, dy: Int64) {
        let type: CGEventType
        let button: CGMouseButton
        if buttons[0] { type = .leftMouseDragged; button = .left }
        else if buttons[1] { type = .rightMouseDragged; button = .right }
        else if buttons[2] || buttons[3] || buttons[4] { type = .otherMouseDragged; button = .center }
        else { type = .mouseMoved; button = .left }
        guard let e = CGEvent(mouseEventSource: src, mouseType: type, mouseCursorPosition: pos, mouseButton: button) else { return }
        e.setIntegerValueField(.mouseEventDeltaX, value: dx)
        e.setIntegerValueField(.mouseEventDeltaY, value: dy)
        e.flags = flags
        e.post(tap: .cghidEventTap)
    }

    func mouseAbs(_ nx: Double, _ ny: Double) {
        lock.lock(); defer { lock.unlock() }
        let b = bounds
        let old = pos
        pos = clamp(CGPoint(x: b.minX + nx * b.width, y: b.minY + ny * b.height))
        postMove(dx: Int64((pos.x - old.x).rounded()), dy: Int64((pos.y - old.y).rounded()))
    }

    func mouseRel(_ dx: Int32, _ dy: Int32) {
        lock.lock(); defer { lock.unlock() }
        pos = clamp(CGPoint(x: pos.x + CGFloat(dx), y: pos.y + CGFloat(dy)))
        postMove(dx: Int64(dx), dy: Int64(dy))
    }

    func button(_ b: Int32, _ down: Bool) {
        lock.lock(); defer { lock.unlock() }
        let idx = Int(b) - 1
        guard idx >= 0 && idx < 5 else { return }
        buttons[idx] = down
        let type: CGEventType
        let button: CGMouseButton
        switch idx {
        case 0: type = down ? .leftMouseDown : .leftMouseUp; button = .left
        case 1: type = down ? .rightMouseDown : .rightMouseUp; button = .right
        default: type = down ? .otherMouseDown : .otherMouseUp; button = CGMouseButton(rawValue: UInt32(idx == 2 ? 2 : idx)) ?? .center
        }
        if down {
            let now = ProcessInfo.processInfo.systemUptime
            let near = abs(pos.x - lastClickPos.x) < 5 && abs(pos.y - lastClickPos.y) < 5
            if now - lastClick[idx] < NSEvent.doubleClickInterval && near {
                clickCount[idx] += 1
            } else {
                clickCount[idx] = 1
            }
            lastClick[idx] = now
            lastClickPos = pos
        }
        guard let e = CGEvent(mouseEventSource: src, mouseType: type, mouseCursorPosition: pos, mouseButton: button) else { return }
        e.setIntegerValueField(.mouseEventClickState, value: max(clickCount[idx], 1))
        if idx >= 2 { e.setIntegerValueField(.mouseEventButtonNumber, value: Int64(idx)) }
        e.flags = flags
        e.post(tap: .cghidEventTap)
    }

    // dx/dy in wheel units (120 = one notch). precise = trackpad-style pixel scrolling.
    func scroll(_ dx: Int32, _ dy: Int32, _ precise: Bool) {
        lock.lock(); defer { lock.unlock() }
        let e: CGEvent?
        if precise {
            let py = Double(dy) * 50.0 / 120.0 + scrollRemainder.y
            let px = Double(dx) * 50.0 / 120.0 + scrollRemainder.x
            let iy = py.rounded(.towardZero), ix = px.rounded(.towardZero)
            scrollRemainder = CGPoint(x: px - ix, y: py - iy)
            e = CGEvent(scrollWheelEvent2Source: src, units: .pixel, wheelCount: 2, wheel1: Int32(iy), wheel2: Int32(ix), wheel3: 0)
        } else {
            let ly = Double(dy) / 120.0 + scrollRemainder.y
            let lx = Double(dx) / 120.0 + scrollRemainder.x
            let iy = ly.rounded(.towardZero), ix = lx.rounded(.towardZero)
            scrollRemainder = CGPoint(x: lx - ix, y: ly - iy)
            if iy == 0 && ix == 0 { return }
            e = CGEvent(scrollWheelEvent2Source: src, units: .line, wheelCount: 2, wheel1: Int32(iy), wheel2: Int32(ix), wheel3: 0)
        }
        guard let e else { return }
        e.flags = flags
        e.post(tap: .cghidEventTap)
    }

    func modifierFlag(_ vk: UInt16) -> CGEventFlags? {
        switch vk {
        case 0x37, 0x36: return .maskCommand
        case 0x38, 0x3C: return .maskShift
        case 0x3A, 0x3D: return .maskAlternate
        case 0x3B, 0x3E: return .maskControl
        case 0x3F: return .maskSecondaryFn
        default: return nil
        }
    }

    func key(_ vk: UInt16, _ down: Bool, _ isRepeat: Bool) {
        lock.lock(); defer { lock.unlock() }
        if down { downKeys.insert(vk) } else { downKeys.remove(vk) }
        var isMod = false
        if let f = modifierFlag(vk) {
            isMod = true
            // Keep the flag while either the left or right variant is held.
            let pair: [UInt16] = switch vk {
            case 0x37, 0x36: [0x37, 0x36]
            case 0x38, 0x3C: [0x38, 0x3C]
            case 0x3A, 0x3D: [0x3A, 0x3D]
            case 0x3B, 0x3E: [0x3B, 0x3E]
            default: [vk]
            }
            if pair.contains(where: { downKeys.contains($0) }) { flags.insert(f) } else { flags.remove(f) }
        } else if vk == 0x39 {
            isMod = true
            if down { if flags.contains(.maskAlphaShift) { flags.remove(.maskAlphaShift) } else { flags.insert(.maskAlphaShift) } }
        }
        guard let e = CGEvent(keyboardEventSource: src, virtualKey: vk, keyDown: down) else { return }
        if isMod { e.type = .flagsChanged }
        e.flags = flags
        if isRepeat { e.setIntegerValueField(.keyboardEventAutorepeat, value: 1) }
        e.post(tap: .cghidEventTap)
    }

    func releaseAll() {
        let keys = { lock.lock(); defer { lock.unlock() }; return downKeys }()
        for k in keys { key(k, false, false) }
        for i in 0..<5 where buttons[i] { button(Int32(i + 1), false) }
        lock.lock(); flags = []; lock.unlock()
    }
}

@_cdecl("hop_mac_inject_key")
public func hop_mac_inject_key(_ vk: UInt16, _ down: Int32, _ rep: Int32) { Injector.shared.key(vk, down != 0, rep != 0) }
@_cdecl("hop_mac_inject_mouse_abs")
public func hop_mac_inject_mouse_abs(_ x: Double, _ y: Double) { Injector.shared.mouseAbs(x, y) }
@_cdecl("hop_mac_inject_mouse_rel")
public func hop_mac_inject_mouse_rel(_ dx: Int32, _ dy: Int32) { Injector.shared.mouseRel(dx, dy) }
@_cdecl("hop_mac_inject_button")
public func hop_mac_inject_button(_ b: Int32, _ down: Int32) { Injector.shared.button(b, down != 0) }
@_cdecl("hop_mac_inject_scroll")
public func hop_mac_inject_scroll(_ dx: Int32, _ dy: Int32, _ precise: Int32) { Injector.shared.scroll(dx, dy, precise != 0) }
@_cdecl("hop_mac_inject_release_all")
public func hop_mac_inject_release_all() { Injector.shared.releaseAll() }

// MARK: - Permissions & power

@_cdecl("hop_mac_screen_access")
public func hop_mac_screen_access(_ request: Int32) -> Int32 {
    if CGPreflightScreenCaptureAccess() { return 1 }
    if request != 0 { return CGRequestScreenCaptureAccess() ? 1 : 0 }
    return 0
}

@_cdecl("hop_mac_accessibility")
public func hop_mac_accessibility(_ prompt: Int32) -> Int32 {
    let opts = [kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: prompt != 0] as CFDictionary
    return AXIsProcessTrustedWithOptions(opts) ? 1 : 0
}

@_cdecl("hop_mac_open_privacy")
public func hop_mac_open_privacy(_ kind: Int32) {
    let anchor = kind == 0 ? "Privacy_ScreenCapture" : "Privacy_Accessibility"
    if let url = URL(string: "x-apple.systempreferences:com.apple.preference.security?\(anchor)") {
        NSWorkspace.shared.open(url)
    }
}

private var activityToken: NSObjectProtocol?

@_cdecl("hop_mac_begin_activity")
public func hop_mac_begin_activity() {
    if activityToken == nil {
        activityToken = ProcessInfo.processInfo.beginActivity(
            options: [.userInitiated, .latencyCritical], reason: "Hop remote desktop")
    }
}

private var sleepAssertion: IOPMAssertionID = 0

@_cdecl("hop_mac_prevent_sleep")
public func hop_mac_prevent_sleep(_ on: Int32) {
    if on != 0 {
        if sleepAssertion == 0 {
            IOPMAssertionCreateWithName(kIOPMAssertionTypePreventUserIdleDisplaySleep as CFString,
                                        IOPMAssertionLevel(kIOPMAssertionLevelOn),
                                        "Hop remote session" as CFString, &sleepAssertion)
        }
    } else if sleepAssertion != 0 {
        IOPMAssertionRelease(sleepAssertion)
        sleepAssertion = 0
    }
}

// MARK: - Keyboard grab (viewer): send Cmd+Tab, Cmd+Space etc. to the remote computer

public typealias HopKeyCB = @convention(c) (UnsafeMutableRawPointer?, UInt16, Int32, Int32) -> Void

final class KeyTap {
    let ctx: UnsafeMutableRawPointer?
    let cb: HopKeyCB
    var tap: CFMachPort?
    var source: CFRunLoopSource?
    var enabled = true

    init(ctx: UnsafeMutableRawPointer?, cb: @escaping HopKeyCB) { self.ctx = ctx; self.cb = cb }

    // Device-dependent modifier bits, so left and right keys are told apart.
    static func modifierMask(_ vk: UInt16) -> UInt64? {
        switch vk {
        case 0x3B: return 0x0001     // left control
        case 0x3E: return 0x2000     // right control
        case 0x38: return 0x0002     // left shift
        case 0x3C: return 0x0004     // right shift
        case 0x37: return 0x0008     // left command
        case 0x36: return 0x0010     // right command
        case 0x3A: return 0x0020     // left option
        case 0x3D: return 0x0040     // right option
        case 0x3F: return 0x800000   // fn
        default: return nil
        }
    }
}

private var keyTap: KeyTap?

private func keyTapCallback(proxy: CGEventTapProxy, type: CGEventType, event: CGEvent, refcon: UnsafeMutableRawPointer?) -> Unmanaged<CGEvent>? {
    guard let refcon else { return Unmanaged.passUnretained(event) }
    let kt = Unmanaged<KeyTap>.fromOpaque(refcon).takeUnretainedValue()
    if type == .tapDisabledByTimeout || type == .tapDisabledByUserInput {
        if let tap = kt.tap { CGEvent.tapEnable(tap: tap, enable: true) }
        return Unmanaged.passUnretained(event)
    }
    guard kt.enabled, NSRunningApplication.current.isActive else { return Unmanaged.passUnretained(event) }
    let vk = UInt16(event.getIntegerValueField(.keyboardEventKeycode))
    switch type {
    case .keyDown:
        kt.cb(kt.ctx, vk, 1, event.getIntegerValueField(.keyboardEventAutorepeat) != 0 ? 1 : 0)
    case .keyUp:
        kt.cb(kt.ctx, vk, 0, 0)
    case .flagsChanged:
        if vk == 0x39 {
            // Caps Lock reports state changes, not presses: send a full tap.
            kt.cb(kt.ctx, vk, 1, 0)
            kt.cb(kt.ctx, vk, 0, 0)
        } else if let m = KeyTap.modifierMask(vk) {
            kt.cb(kt.ctx, vk, (event.flags.rawValue & m) != 0 ? 1 : 0, 0)
        } else {
            return Unmanaged.passUnretained(event)
        }
    default:
        return Unmanaged.passUnretained(event)
    }
    return nil
}

/// Starts swallowing keyboard events while this app is frontmost and reporting them to `cb`.
/// Needs Accessibility permission; returns 0 if the tap couldn't be created.
@_cdecl("hop_mac_keytap_start")
public func hop_mac_keytap_start(_ ctx: UnsafeMutableRawPointer?, _ cb: HopKeyCB) -> Int32 {
    if keyTap != nil { return 1 }
    let kt = KeyTap(ctx: ctx, cb: cb)
    let mask = (1 << CGEventType.keyDown.rawValue) | (1 << CGEventType.keyUp.rawValue) | (1 << CGEventType.flagsChanged.rawValue)
    guard let tap = CGEvent.tapCreate(tap: .cghidEventTap, place: .headInsertEventTap, options: .defaultTap,
                                      eventsOfInterest: CGEventMask(mask), callback: keyTapCallback,
                                      userInfo: Unmanaged.passUnretained(kt).toOpaque()) else { return 0 }
    kt.tap = tap
    kt.source = CFMachPortCreateRunLoopSource(nil, tap, 0)
    CFRunLoopAddSource(CFRunLoopGetMain(), kt.source, .commonModes)
    CGEvent.tapEnable(tap: tap, enable: true)
    keyTap = kt
    return 1
}

@_cdecl("hop_mac_keytap_enable")
public func hop_mac_keytap_enable(_ on: Int32) {
    keyTap?.enabled = on != 0
}
