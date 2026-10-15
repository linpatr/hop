//! macOS: thin wrappers over the Swift layer in mac/HopMac.swift.

use std::ffi::{CStr, c_char, c_void};

use anyhow::{Result, bail};

use super::{CaptureConfig, Capturer, DecodeStatus, Injector, Nv12Frame, SharedSink, VideoDecoder, fit_size};
use crate::keymap;
use crate::proto::Input;

type VideoCb = extern "C" fn(*mut c_void, *const u8, usize, i32, u64);
type AudioCb = extern "C" fn(*mut c_void, *const i16, usize);
type DecodedCb = extern "C" fn(*mut c_void, *const u8, usize, *const u8, usize, i32, i32);
type KeyCb = extern "C" fn(*mut c_void, u16, i32, i32);

unsafe extern "C" {
    fn hop_mac_now_us() -> u64;
    fn hop_mac_display_size(w: *mut i32, h: *mut i32);
    fn hop_mac_host_start(
        w: i32,
        h: i32,
        fps: i32,
        kbps: i32,
        audio: i32,
        ctx: *mut c_void,
        vcb: VideoCb,
        acb: AudioCb,
        err: *mut c_char,
        err_len: usize,
    ) -> *mut c_void;
    fn hop_mac_host_stop(h: *mut c_void);
    fn hop_mac_host_request_keyframe(h: *mut c_void);
    fn hop_mac_host_set_bitrate(h: *mut c_void, kbps: i32);
    fn hop_mac_dec_create(ctx: *mut c_void, cb: DecodedCb) -> *mut c_void;
    fn hop_mac_dec_decode(h: *mut c_void, data: *const u8, len: usize) -> i32;
    fn hop_mac_dec_destroy(h: *mut c_void);
    fn hop_mac_inject_key(vk: u16, down: i32, rep: i32);
    fn hop_mac_inject_mouse_abs(x: f64, y: f64);
    fn hop_mac_inject_mouse_rel(dx: i32, dy: i32);
    fn hop_mac_inject_button(b: i32, down: i32);
    fn hop_mac_inject_scroll(dx: i32, dy: i32, precise: i32);
    fn hop_mac_inject_release_all();
    fn hop_mac_screen_access(request: i32) -> i32;
    fn hop_mac_accessibility(prompt: i32) -> i32;
    fn hop_mac_open_privacy(kind: i32);
    fn hop_mac_begin_activity();
    fn hop_mac_prevent_sleep(on: i32);
    fn hop_mac_keytap_start(ctx: *mut c_void, cb: KeyCb) -> i32;
}

/// Called with (hid, down, repeat) for every key while the viewer is frontmost.
pub type KeyGrabFn = Box<dyn Fn(u16, bool, bool) + Send + Sync>;

extern "C" fn key_cb(ctx: *mut c_void, vk: u16, down: i32, rep: i32) {
    let f = unsafe { &*(ctx as *const KeyGrabFn) };
    if let Some(hid) = keymap::mac_vk_to_hid(vk) {
        f(hid, down != 0, rep != 0);
    }
}

/// Captures system shortcuts (Cmd+Tab, Cmd+Space, ...) for the remote computer. Returns false
/// without Accessibility permission, in which case normal window key events are used.
pub fn start_key_grab(f: KeyGrabFn) -> bool {
    let ctx = Box::into_raw(Box::new(f)) as *mut c_void; // lives for the rest of the process
    unsafe { hop_mac_keytap_start(ctx, key_cb) != 0 }
}

pub fn now_us() -> u64 {
    unsafe { hop_mac_now_us() }
}

pub fn native_display_size() -> (u32, u32) {
    let (mut w, mut h) = (0i32, 0i32);
    unsafe { hop_mac_display_size(&mut w, &mut h) };
    (w.max(2) as u32, h.max(2) as u32)
}

/// ScreenCaptureKit scales for free, so any size works.
pub fn stream_size(native: (u32, u32), max: (u32, u32)) -> (u32, u32) {
    fit_size(native, max)
}

pub fn prevent_sleep(on: bool) {
    unsafe { hop_mac_prevent_sleep(on as i32) }
}

pub fn platform_init() {
    unsafe { hop_mac_begin_activity() }
}

pub struct Permissions {
    pub screen: bool,
    pub accessibility: bool,
}

pub fn host_permissions() -> Permissions {
    unsafe { Permissions { screen: hop_mac_screen_access(0) != 0, accessibility: hop_mac_accessibility(0) != 0 } }
}

pub fn request_screen_permission() -> bool {
    unsafe { hop_mac_screen_access(1) != 0 }
}

pub fn request_accessibility_permission() -> bool {
    unsafe { hop_mac_accessibility(1) != 0 }
}

pub fn open_privacy_settings(accessibility: bool) {
    unsafe { hop_mac_open_privacy(accessibility as i32) }
}

struct MacCapturer {
    handle: *mut c_void,
    sink: *mut SharedSink,
}

unsafe impl Send for MacCapturer {}

extern "C" fn video_cb(ctx: *mut c_void, data: *const u8, len: usize, key: i32, capture_us: u64) {
    if data.is_null() {
        return;
    }
    let sink = unsafe { &*(ctx as *const SharedSink) };
    sink.video(unsafe { std::slice::from_raw_parts(data, len) }, key != 0, capture_us);
}

extern "C" fn audio_cb(ctx: *mut c_void, pcm: *const i16, frames: usize) {
    if pcm.is_null() {
        return;
    }
    let sink = unsafe { &*(ctx as *const SharedSink) };
    sink.audio(unsafe { std::slice::from_raw_parts(pcm, frames * 2) });
}

pub fn start_capture(cfg: &CaptureConfig, sink: SharedSink) -> Result<Box<dyn Capturer>> {
    let sink = Box::into_raw(Box::new(sink));
    let mut err = [0 as c_char; 512];
    let handle = unsafe {
        hop_mac_host_start(
            cfg.width as i32,
            cfg.height as i32,
            cfg.fps as i32,
            cfg.bitrate_kbps as i32,
            cfg.audio as i32,
            sink as *mut c_void,
            video_cb,
            audio_cb,
            err.as_mut_ptr(),
            err.len(),
        )
    };
    if handle.is_null() {
        drop(unsafe { Box::from_raw(sink) });
        let msg = unsafe { CStr::from_ptr(err.as_ptr()) }.to_string_lossy().into_owned();
        bail!("screen capture failed: {msg}");
    }
    Ok(Box::new(MacCapturer { handle, sink }))
}

impl Capturer for MacCapturer {
    fn request_keyframe(&self) {
        unsafe { hop_mac_host_request_keyframe(self.handle) }
    }
    fn set_bitrate(&self, kbps: u32) {
        unsafe { hop_mac_host_set_bitrate(self.handle, kbps as i32) }
    }
    fn encoder_name(&self) -> String {
        "VideoToolbox H.264".into()
    }
}

impl Drop for MacCapturer {
    fn drop(&mut self) {
        unsafe {
            hop_mac_host_stop(self.handle);
            drop(Box::from_raw(self.sink));
        }
    }
}

struct DecodeTarget {
    out: *mut Nv12Frame,
    produced: bool,
}

pub struct MacDecoder {
    handle: *mut c_void,
    target: Box<DecodeTarget>,
}

unsafe impl Send for MacDecoder {}

extern "C" fn decoded_cb(ctx: *mut c_void, y: *const u8, ys: usize, uv: *const u8, uvs: usize, w: i32, h: i32) {
    let t = unsafe { &mut *(ctx as *mut DecodeTarget) };
    if t.out.is_null() || y.is_null() || uv.is_null() {
        return;
    }
    unsafe { (*t.out).copy_from(w as u32, h as u32, y, ys, uv, uvs) };
    t.produced = true;
}

pub fn new_decoder() -> Result<Box<dyn VideoDecoder>> {
    let mut target = Box::new(DecodeTarget { out: std::ptr::null_mut(), produced: false });
    let handle = unsafe { hop_mac_dec_create(&mut *target as *mut DecodeTarget as *mut c_void, decoded_cb) };
    Ok(Box::new(MacDecoder { handle, target }))
}

impl VideoDecoder for MacDecoder {
    fn decode(&mut self, data: &[u8], out: &mut Nv12Frame) -> Result<DecodeStatus> {
        self.target.out = out;
        self.target.produced = false;
        let rc = unsafe { hop_mac_dec_decode(self.handle, data.as_ptr(), data.len()) };
        self.target.out = std::ptr::null_mut();
        Ok(match rc {
            0 if self.target.produced => DecodeStatus::Frame,
            0 => DecodeStatus::NoFrame,
            1 => DecodeStatus::NeedKeyframe,
            e => {
                log::warn!("decode error {e}");
                DecodeStatus::NeedKeyframe
            }
        })
    }

    fn name(&self) -> String {
        "VideoToolbox".into()
    }
}

impl Drop for MacDecoder {
    fn drop(&mut self) {
        unsafe { hop_mac_dec_destroy(self.handle) }
    }
}

pub struct MacInjector;

pub fn new_injector() -> Result<Box<dyn Injector>> {
    if unsafe { hop_mac_accessibility(1) } == 0 {
        log::warn!("Accessibility permission missing: remote mouse and keyboard won't work until it's granted");
    }
    Ok(Box::new(MacInjector))
}

impl Injector for MacInjector {
    fn input(&mut self, i: Input) {
        unsafe {
            match i {
                Input::Key { hid, down, repeat } => {
                    if let Some(vk) = keymap::hid_to_mac_vk(hid) {
                        hop_mac_inject_key(vk, down as i32, repeat as i32)
                    }
                }
                Input::MouseAbs { x, y } => hop_mac_inject_mouse_abs(x as f64 / 65535.0, y as f64 / 65535.0),
                Input::MouseRel { dx, dy } => hop_mac_inject_mouse_rel(dx as i32, dy as i32),
                Input::Button { button, down } => hop_mac_inject_button(button as i32, down as i32),
                Input::Wheel { dx, dy, precise } => hop_mac_inject_scroll(dx as i32, dy as i32, precise as i32),
                Input::ReleaseAll => hop_mac_inject_release_all(),
            }
        }
    }
}

impl Drop for MacInjector {
    fn drop(&mut self) {
        unsafe { hop_mac_inject_release_all() }
    }
}
