//! Windows: Desktop Duplication capture, Media Foundation H.264 encode/decode, SendInput.

mod dxgi;
mod input;
mod keyhook;
mod mfdec;
mod mfenc;
pub mod vdisplay;

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use parking_lot::{Condvar, Mutex};
use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_FULL, MFStartup};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
use windows::Win32::System::Power::{ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, SetThreadExecutionState};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

use super::{CaptureConfig, Capturer, Injector, SharedSink, VideoDecoder};
use crate::convert;

pub use input::WinInjector;
pub use keyhook::start_key_grab;

static START: OnceLock<Instant> = OnceLock::new();

pub fn now_us() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_micros() as u64
}

pub fn platform_init() {
    START.get_or_init(Instant::now);
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        windows::Win32::Media::timeBeginPeriod(1);
        // GUI-subsystem builds have no console; reuse the terminal we were started from, if any.
        if std::env::args().len() > 1 {
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

pub(crate) fn com_init() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
    }
}

pub fn native_display_size() -> (u32, u32) {
    if let Ok(d) = dxgi::primary_output_size() {
        return d;
    }
    unsafe { (GetSystemMetrics(SM_CXSCREEN).max(2) as u32, GetSystemMetrics(SM_CYSCREEN).max(2) as u32) }
}

fn scale_for(native: (u32, u32), max: (u32, u32)) -> usize {
    if native.0 <= max.0 && native.1 <= max.1 { 1 } else { 2 }
}

/// Desktop Duplication can't scale, so the converter halves the size when the screen is too big.
pub fn stream_size(native: (u32, u32), max: (u32, u32)) -> (u32, u32) {
    let s = scale_for(native, max);
    let (w, h) = convert::output_size(native.0 as usize, native.1 as usize, s);
    (w as u32, h as u32)
}

/// Gets the screen ready to stream: wakes a sleeping monitor, or, if the real monitor is off and
/// the virtual display took over, sets that to the viewer's resolution.
pub fn prepare_display(max: (u32, u32), fps: u32) {
    wake_display();
    if vdisplay::prepare_for_stream(max, fps) {
        std::thread::sleep(Duration::from_millis(800));
    }
}

/// Windows only draws the desktop while a display is on, so wake it before capturing.
pub fn wake_display() {
    unsafe {
        SetThreadExecutionState(ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED);
    }
    input::nudge_mouse();
}

pub fn prevent_sleep(on: bool) {
    unsafe {
        let flags = if on { ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED } else { ES_CONTINUOUS };
        SetThreadExecutionState(flags);
    }
}

/// A converted frame waiting for the encoder.
#[allow(dead_code)]
pub(crate) struct RawFrame {
    pub nv12: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub capture_us: u64,
}

pub(crate) struct CaptureShared {
    slot: Mutex<(Option<Arc<RawFrame>>, u64)>,
    cv: Condvar,
    pub stop: AtomicBool,
    pub keyframe: AtomicBool,
    pub bitrate_kbps: AtomicU32,
    pub frames_captured: AtomicU64,
    /// Draw the pointer into the video (off when the viewer draws it locally).
    pub cursor_in_video: AtomicBool,
    /// Re-convert the last frame even without a screen change (e.g. to add/remove the pointer).
    pub redraw: AtomicBool,
}

impl CaptureShared {
    fn publish(&self, f: Arc<RawFrame>) {
        let mut g = self.slot.lock();
        g.0 = Some(f);
        g.1 += 1;
        self.cv.notify_all();
    }

    /// Next frame for the encoder: a new capture, or the previous one again when a keyframe is
    /// wanted or nothing changed for `repeat_after`. Repeating keeps frames moving through
    /// hardware encoders that hold one back (otherwise a still screen can strand the keyframe
    /// the viewer is waiting for). Returns (frame, timestamp_us, force_keyframe).
    pub fn next_input(
        &self,
        last_seq: &mut u64,
        last: &mut Option<Arc<RawFrame>>,
        repeat_after: Duration,
    ) -> Option<(Arc<RawFrame>, u64, bool)> {
        let mut g = self.slot.lock();
        let mut waited = Duration::ZERO;
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return None;
            }
            if g.1 != *last_seq
                && let Some(f) = g.0.clone()
            {
                *last_seq = g.1;
                *last = Some(f.clone());
                let force = self.keyframe.swap(false, Ordering::AcqRel);
                let ts = f.capture_us;
                return Some((f, ts, force));
            }
            if waited >= Duration::from_millis(30)
                && self.keyframe.load(Ordering::Acquire)
                && let Some(f) = last.clone()
            {
                self.keyframe.store(false, Ordering::Release);
                return Some((f, now_us(), true));
            }
            if waited >= repeat_after
                && let Some(f) = last.clone()
            {
                return Some((f, now_us(), false));
            }
            self.cv.wait_for(&mut g, Duration::from_millis(8));
            waited += Duration::from_millis(8);
        }
    }
}

struct WinCapturer {
    shared: Arc<CaptureShared>,
    threads: Vec<JoinHandle<()>>,
    encoder: String,
}

pub fn start_capture(cfg: &CaptureConfig, sink: SharedSink) -> Result<Box<dyn Capturer>> {
    let native = native_display_size();
    let scale = scale_for(native, (cfg.width, cfg.height));
    let shared = Arc::new(CaptureShared {
        slot: Mutex::new((None, 0)),
        cv: Condvar::new(),
        stop: AtomicBool::new(false),
        keyframe: AtomicBool::new(true),
        bitrate_kbps: AtomicU32::new(cfg.bitrate_kbps),
        frames_captured: AtomicU64::new(0),
        cursor_in_video: AtomicBool::new(true),
        redraw: AtomicBool::new(false),
    });
    let (w, h) = (cfg.width, cfg.height);
    let fps = cfg.fps;

    let (cap_tx, cap_rx) = std::sync::mpsc::channel::<Result<()>>();
    let sh = shared.clone();
    let cap_sink = sink.clone();
    let cap = std::thread::Builder::new()
        .name("dxgi-capture".into())
        .spawn(move || dxgi::capture_thread(sh, cap_sink, scale, (w, h), cap_tx))?;
    match cap_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            shared.stop.store(true, Ordering::SeqCst);
            let _ = cap.join();
            return Err(e);
        }
        Err(_) => {
            shared.stop.store(true, Ordering::SeqCst);
            return Err(anyhow!("screen capture didn't start"));
        }
    }

    let (enc_tx, enc_rx) = std::sync::mpsc::channel::<Result<String>>();
    let sh = shared.clone();
    let sink2 = sink.clone();
    let enc = std::thread::Builder::new()
        .name("mf-encode".into())
        .spawn(move || mfenc::encode_thread(sh, sink2, w, h, fps, enc_tx))?;
    let encoder = match enc_rx.recv_timeout(Duration::from_secs(15)) {
        Ok(Ok(name)) => name,
        Ok(Err(e)) => {
            shared.stop.store(true, Ordering::SeqCst);
            shared.cv.notify_all();
            let _ = cap.join();
            let _ = enc.join();
            return Err(e);
        }
        Err(_) => {
            shared.stop.store(true, Ordering::SeqCst);
            return Err(anyhow!("video encoder didn't start"));
        }
    };
    if cfg.audio {
        // Shares the stop flag with capture through a small adapter thread.
        let stop = Arc::new(AtomicBool::new(false));
        crate::audio::spawn_loopback(sink, stop.clone());
        let sh = shared.clone();
        std::thread::spawn(move || {
            while !sh.stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
            }
            stop.store(true, Ordering::SeqCst);
        });
    }
    Ok(Box::new(WinCapturer { shared, threads: vec![cap, enc], encoder }))
}

impl Capturer for WinCapturer {
    fn set_cursor_in_video(&self, on: bool) {
        if self.shared.cursor_in_video.swap(on, Ordering::AcqRel) != on {
            self.shared.redraw.store(true, Ordering::Release);
        }
    }
    fn request_keyframe(&self) {
        self.shared.keyframe.store(true, Ordering::Release);
        self.shared.cv.notify_all();
    }
    fn set_bitrate(&self, kbps: u32) {
        self.shared.bitrate_kbps.store(kbps, Ordering::Release);
    }
    fn encoder_name(&self) -> String {
        self.encoder.clone()
    }
    fn is_alive(&self) -> bool {
        !self.shared.stop.load(Ordering::Relaxed)
    }
}

impl Drop for WinCapturer {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.cv.notify_all();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

pub fn new_decoder() -> Result<Box<dyn VideoDecoder>> {
    com_init();
    Ok(Box::new(mfdec::MfDecoder::new()?))
}

pub fn new_injector() -> Result<Box<dyn Injector>> {
    Ok(Box::new(WinInjector::new()))
}
