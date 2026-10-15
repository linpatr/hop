//! H.264 encoding with Media Foundation. Prefers the GPU's hardware encoder (NVENC, AMD VCE/VCN,
//! Intel Quick Sync all ship as async MFTs) and falls back to Microsoft's software encoder.

use std::mem::ManuallyDrop;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};
use windows::Win32::Foundation::VARIANT_TRUE;
use windows::core::{GUID, Interface, PWSTR};

use super::{CaptureShared, RawFrame, now_us};
use crate::platform::SharedSink;

pub(crate) fn variant_u32(v: u32) -> VARIANT {
    let mut var = VARIANT::default();
    unsafe {
        let inner = &mut *var.Anonymous.Anonymous;
        inner.vt = VT_UI4;
        inner.Anonymous.ulVal = v;
    }
    var
}

pub(crate) fn variant_bool(v: bool) -> VARIANT {
    let mut var = VARIANT::default();
    unsafe {
        let inner = &mut *var.Anonymous.Anonymous;
        inner.vt = VT_BOOL;
        inner.Anonymous.boolVal = if v { VARIANT_TRUE } else { Default::default() };
    }
    var
}

fn friendly_name(a: &IMFActivate) -> String {
    unsafe {
        let mut p = PWSTR::null();
        let mut n = 0u32;
        if a.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut p, &mut n).is_ok() && !p.is_null() {
            let s = p.to_string().unwrap_or_default();
            CoTaskMemFree(Some(p.0 as *const _));
            return s;
        }
    }
    "unknown encoder".into()
}

fn enumerate(hardware: bool) -> Vec<(IMFActivate, String)> {
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
    let base = if hardware { MFT_ENUM_FLAG_HARDWARE } else { MFT_ENUM_FLAG_SYNCMFT };
    let flags = MFT_ENUM_FLAG(base.0 | MFT_ENUM_FLAG_SORTANDFILTER.0);
    let mut out = Vec::new();
    unsafe {
        let mut pp: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut n = 0u32;
        if MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, flags, Some(&input), Some(&output), &mut pp, &mut n).is_err() || pp.is_null() {
            return out;
        }
        for a in std::slice::from_raw_parts_mut(pp, n as usize) {
            if let Some(a) = a.take() {
                let name = friendly_name(&a);
                out.push((a, name));
            }
        }
        CoTaskMemFree(Some(pp as *const _));
    }
    out
}

fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

struct Encoder {
    t: IMFTransform,
    codec: Option<ICodecAPI>,
    events: Option<IMFMediaEventGenerator>,
    provides_samples: bool,
    out_size: u32,
    frame_dur: i64,
    bitrate: u32,
    seq_header: Vec<u8>,
    last_ts: u64,
    /// Input requests the async encoder made that haven't been answered yet.
    owed_inputs: u32,
    /// Outputs at or before this timestamp come from the startup probe and are dropped.
    probe_ts: u64,
    pub name: String,
    pub hardware: bool,
}

impl Encoder {
    fn new(a: &IMFActivate, name: &str, hardware: bool, w: u32, h: u32, fps: u32, kbps: u32) -> Result<Self> {
        unsafe {
            let t: IMFTransform = a.ActivateObject().context("activate")?;
            let attrs = t.GetAttributes().ok();
            let is_async = attrs.as_ref().and_then(|a| a.GetUINT32(&MF_TRANSFORM_ASYNC).ok()).unwrap_or(0) != 0;
            if let Some(a) = &attrs {
                if is_async {
                    a.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).context("unlock async MFT")?;
                }
                let _ = a.SetUINT32(&MF_LOW_LATENCY, 1);
            }
            let codec: Option<ICodecAPI> = t.cast().ok();
            if let Some(c) = &codec {
                let set = |g: &GUID, v: VARIANT| {
                    if let Err(e) = c.SetValue(g, &v) {
                        log::debug!("{name}: codec property {g:?} not set: {e}");
                    }
                };
                set(&CODECAPI_AVEncCommonRateControlMode, variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32));
                set(&CODECAPI_AVEncCommonMeanBitRate, variant_u32(kbps * 1000));
                set(&CODECAPI_AVLowLatencyMode, variant_bool(true));
                set(&CODECAPI_AVEncMPVDefaultBPictureCount, variant_u32(0));
                set(&CODECAPI_AVEncMPVGOPSize, variant_u32(fps * 60));
                if !hardware {
                    set(&CODECAPI_AVEncCommonQualityVsSpeed, variant_u32(0));
                }
            }

            let out = MFCreateMediaType()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, kbps * 1000)?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h))?;
            out.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
            t.SetOutputType(0, &out, 0).context("set output type")?;

            let inp = MFCreateMediaType()?;
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            inp.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h))?;
            inp.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            inp.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            let _ = inp.SetUINT32(&MF_MT_DEFAULT_STRIDE, w);
            let _ = inp.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
            let _ = inp.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32);
            t.SetInputType(0, &inp, 0).context("set input type")?;

            let info = t.GetOutputStreamInfo(0)?;
            let provides_samples = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
                != 0;
            let out_size = info.cbSize.max(w * h * 3 / 2);

            let mut seq_header = Vec::new();
            if let Ok(cur) = t.GetOutputCurrentType(0)
                && let Ok(n) = cur.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER)
                && n > 0
            {
                seq_header.resize(n as usize, 0);
                if cur.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut seq_header, None).is_err() {
                    seq_header.clear();
                }
            }

            t.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            t.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            t.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            let events = if is_async { Some(t.cast::<IMFMediaEventGenerator>()?) } else { None };
            Ok(Self {
                t,
                codec,
                events,
                provides_samples,
                out_size,
                frame_dur: 10_000_000 / fps.max(1) as i64,
                bitrate: kbps,
                seq_header,
                last_ts: 0,
                owed_inputs: 0,
                probe_ts: 0,
                name: name.to_string(),
                hardware,
            })
        }
    }

    fn set_bitrate(&mut self, kbps: u32) {
        if kbps == self.bitrate {
            return;
        }
        self.bitrate = kbps;
        if let Some(c) = &self.codec {
            let _ = unsafe { c.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &variant_u32(kbps * 1000)) };
        }
    }

    fn feed(&mut self, nv12: &[u8], ts_us: u64, force_key: bool) -> Result<()> {
        let ts = ts_us.max(self.last_ts + 1);
        self.last_ts = ts;
        unsafe {
            let buf = MFCreateMemoryBuffer(nv12.len() as u32)?;
            let mut p = std::ptr::null_mut();
            buf.Lock(&mut p, None, None)?;
            std::ptr::copy_nonoverlapping(nv12.as_ptr(), p, nv12.len());
            buf.Unlock()?;
            buf.SetCurrentLength(nv12.len() as u32)?;
            let s = MFCreateSample()?;
            s.AddBuffer(&buf)?;
            s.SetSampleTime(ts as i64 * 10)?;
            s.SetSampleDuration(self.frame_dur)?;
            if force_key && let Some(c) = &self.codec {
                let _ = c.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &variant_u32(1));
            }
            self.t.ProcessInput(0, &s, 0)?;
        }
        Ok(())
    }

    /// Returns (annex-b data, keyframe, timestamp_us), or None if the encoder wants more input.
    fn pull(&mut self) -> Result<Option<(Vec<u8>, bool, u64)>> {
        unsafe {
            let mut odb = MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, ..Default::default() };
            if !self.provides_samples {
                let s = MFCreateSample()?;
                s.AddBuffer(&MFCreateMemoryBuffer(self.out_size)?)?;
                odb.pSample = ManuallyDrop::new(Some(s));
            }
            let mut status = 0u32;
            let r = self.t.ProcessOutput(0, std::slice::from_mut(&mut odb), &mut status);
            let sample = ManuallyDrop::take(&mut odb.pSample);
            drop(ManuallyDrop::take(&mut odb.pEvents));
            match r {
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    let ty = self.t.GetOutputAvailableType(0, 0)?;
                    self.t.SetOutputType(0, &ty, 0)?;
                    return Ok(None);
                }
                Err(e) => return Err(e.into()),
                Ok(()) => {}
            }
            let sample = sample.ok_or_else(|| anyhow!("encoder returned no sample"))?;
            let buf = sample.ConvertToContiguousBuffer()?;
            let mut p = std::ptr::null_mut();
            let mut len = 0u32;
            buf.Lock(&mut p, None, Some(&mut len))?;
            let mut data = std::slice::from_raw_parts(p, len as usize).to_vec();
            buf.Unlock()?;
            let ts = (sample.GetSampleTime().unwrap_or(0) / 10).max(0) as u64;
            let (key, has_sps) = scan_nals(&data);
            if key && !has_sps && !self.seq_header.is_empty() {
                let mut v = self.seq_header.clone();
                v.extend_from_slice(&data);
                data = v;
            }
            Ok(Some((data, key, ts)))
        }
    }

    fn drain_outputs(&mut self, sink: &SharedSink) -> Result<usize> {
        let mut n = 0;
        while let Some((data, key, ts)) = self.pull()? {
            if ts > self.probe_ts {
                sink.video(&data, key, ts);
            }
            n += 1;
        }
        Ok(n)
    }

    /// Feeds a gray test frame and waits for output, so a broken encoder is caught at startup.
    fn probe(&mut self, w: u32, h: u32) -> Result<()> {
        let mut gray = vec![128u8; (w * h * 3 / 2) as usize];
        gray[..(w * h) as usize].fill(100);
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut fed = 0;
        let null_sink: SharedSink = Arc::new(NullSink);
        match self.events.clone() {
            None => {
                while Instant::now() < deadline && fed < 8 {
                    self.feed(&gray, now_us(), fed == 0)?;
                    fed += 1;
                    if self.drain_outputs(&null_sink)? > 0 {
                        log::info!("{}: first output after {fed} input(s)", self.name);
                        self.probe_ts = self.last_ts;
                        return Ok(());
                    }
                }
            }
            Some(ev) => unsafe {
                while Instant::now() < deadline {
                    let e = match ev.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                        Ok(e) => e,
                        Err(_) => {
                            std::thread::sleep(Duration::from_millis(2));
                            continue;
                        }
                    };
                    let ty = MF_EVENT_TYPE(e.GetType()? as i32);
                    if ty == METransformNeedInput {
                        if fed < 8 {
                            self.feed(&gray, now_us(), fed == 0)?;
                            fed += 1;
                        } else {
                            self.owed_inputs += 1;
                        }
                    } else if ty == METransformHaveOutput && self.pull()?.is_some() {
                        log::info!("{}: first output after {fed} input(s)", self.name);
                        self.probe_ts = self.last_ts;
                        return Ok(());
                    }
                }
            },
        }
        bail!("{} produced no output", self.name)
    }

    fn run(&mut self, sh: &CaptureShared, sink: &SharedSink) -> Result<()> {
        let mut last_seq = 0u64;
        let mut last: Option<Arc<RawFrame>> = None;
        match self.events.clone() {
            None => loop {
                let Some((f, ts, force)) = sh.next_input(&mut last_seq, &mut last) else { return Ok(()) };
                self.set_bitrate(sh.bitrate_kbps.load(Ordering::Relaxed));
                self.feed(&f.nv12, ts, force)?;
                self.drain_outputs(sink)?;
            },
            Some(ev) => loop {
                if sh.stop.load(Ordering::Relaxed) {
                    return Ok(());
                }
                while self.owed_inputs > 0 {
                    let Some((f, ts, force)) = sh.next_input(&mut last_seq, &mut last) else { return Ok(()) };
                    self.feed(&f.nv12, ts, force)?;
                    self.owed_inputs -= 1;
                }
                // Blocks until the encoder wants input or has output; both happen every frame.
                let e = unsafe { ev.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0))? };
                let ty = MF_EVENT_TYPE(unsafe { e.GetType()? } as i32);
                if ty == METransformNeedInput {
                    let Some((f, ts, force)) = sh.next_input(&mut last_seq, &mut last) else { return Ok(()) };
                    self.set_bitrate(sh.bitrate_kbps.load(Ordering::Relaxed));
                    self.feed(&f.nv12, ts, force)?;
                } else if ty == METransformHaveOutput
                    && let Some((data, key, ts)) = self.pull()?
                    && ts > self.probe_ts
                {
                    sink.video(&data, key, ts);
                }
            },
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.t.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            if let Ok(s) = self.t.cast::<IMFShutdown>() {
                let _ = s.Shutdown();
            }
        }
    }
}

struct NullSink;
impl crate::platform::FrameSink for NullSink {
    fn video(&self, _: &[u8], _: bool, _: u64) {}
    fn audio(&self, _: &[i16]) {}
}

/// (has IDR slice, has SPS) for an Annex B buffer.
fn scan_nals(d: &[u8]) -> (bool, bool) {
    let (mut idr, mut sps) = (false, false);
    let mut i = 0;
    while i + 3 < d.len() {
        if d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1 {
            match d[i + 3] & 0x1F {
                5 => idr = true,
                7 => sps = true,
                _ => {}
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    (idr, sps)
}

/// Finds a working encoder (hardware first) and runs it until capture stops.
pub fn encode_thread(sh: Arc<CaptureShared>, sink: SharedSink, w: u32, h: u32, fps: u32, ready: Sender<Result<String>>) {
    super::com_init();
    let kbps = sh.bitrate_kbps.load(Ordering::Relaxed);
    let mut candidates: Vec<(IMFActivate, String, bool)> =
        enumerate(true).into_iter().map(|(a, n)| (a, n, true)).collect();
    candidates.extend(enumerate(false).into_iter().map(|(a, n)| (a, n, false)));
    if let Ok(force) = std::env::var("HOP_ENCODER") {
        candidates.retain(|c| c.1.to_lowercase().contains(&force.to_lowercase()));
    }
    log::info!("H.264 encoders: {:?}", candidates.iter().map(|c| &c.1).collect::<Vec<_>>());

    let mut errors = Vec::new();
    let mut enc = None;
    for (a, name, hw) in &candidates {
        match Encoder::new(a, name, *hw, w, h, fps, kbps).and_then(|mut e| e.probe(w, h).map(|_| e)) {
            Ok(e) => {
                enc = Some(e);
                break;
            }
            Err(e) => {
                log::warn!("encoder {name} unusable: {e:#}");
                errors.push(format!("{name}: {e:#}"));
                unsafe {
                    let _ = a.ShutdownObject();
                }
            }
        }
    }
    let Some(mut enc) = enc else {
        let _ = ready.send(Err(anyhow!("no working H.264 encoder ({})", errors.join("; "))));
        return;
    };
    let label = format!("{} ({})", enc.name, if enc.hardware { "hardware" } else { "software" });
    log::info!("encoding {w}x{h}@{fps} with {label}");
    let _ = ready.send(Ok(label));
    // The probe frames may have used up the initial keyframe request.
    sh.keyframe.store(true, Ordering::Release);
    if let Err(e) = enc.run(&sh, &sink) {
        log::error!("encoder stopped: {e:#}");
        sh.stop.store(true, Ordering::SeqCst);
    }
}
