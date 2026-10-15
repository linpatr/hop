//! What each OS has to provide: capture + encode, decode, and input injection.

use std::sync::Arc;

use anyhow::Result;

use crate::proto::{CursorShape, Input};

#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "macos")]
pub use mac::*;

#[cfg(windows)]
pub mod win;
#[cfg(windows)]
pub use win::*;

pub struct CaptureConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub audio: bool,
}

/// Receives encoded output from a capturer. Called from capture/encoder threads.
pub trait FrameSink: Send + Sync {
    /// One H.264 access unit in Annex B format.
    fn video(&self, data: &[u8], keyframe: bool, capture_us: u64);
    /// Interleaved stereo PCM at 48 kHz.
    fn audio(&self, pcm: &[i16]);
    /// The host's mouse pointer was shown/hidden or changed shape (`shape` is set when it changed).
    fn cursor(&self, _visible: bool, _shape: Option<&CursorShape>) {}
}

/// A running screen capture + encoder. Dropping it stops capture.
pub trait Capturer: Send {
    /// Whether the mouse pointer is drawn into the video (off when the viewer draws it locally).
    fn set_cursor_in_video(&self, on: bool);
    fn request_keyframe(&self);
    fn set_bitrate(&self, kbps: u32);
    fn encoder_name(&self) -> String;
    /// False once capture has stopped by itself (e.g. the screen changed); the host restarts it.
    fn is_alive(&self) -> bool {
        true
    }
}

/// A decoded frame in NV12 with tightly packed rows (stride == width).
#[derive(Default)]
pub struct Nv12Frame {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub uv: Vec<u8>,
}

impl Nv12Frame {
    /// Copies planes with arbitrary strides into this frame, reusing its buffers.
    ///
    /// # Safety
    /// The pointers must be valid for `height` rows (luma) / `height / 2` rows (chroma) of their strides.
    pub unsafe fn copy_from(&mut self, w: u32, h: u32, y: *const u8, ys: usize, uv: *const u8, uvs: usize) {
        let (wu, hu) = (w as usize, h as usize);
        self.width = w;
        self.height = h;
        self.y.resize(wu * hu, 0);
        self.uv.resize(wu * hu.div_ceil(2), 0);
        for r in 0..hu {
            unsafe { std::ptr::copy_nonoverlapping(y.add(r * ys), self.y.as_mut_ptr().add(r * wu), wu) };
        }
        for r in 0..hu.div_ceil(2) {
            unsafe { std::ptr::copy_nonoverlapping(uv.add(r * uvs), self.uv.as_mut_ptr().add(r * wu), wu) };
        }
    }
}

pub enum DecodeStatus {
    Frame,
    NoFrame,
    /// The decoder lost its reference state; ask the host for a keyframe.
    NeedKeyframe,
}

pub trait VideoDecoder {
    fn decode(&mut self, annexb: &[u8], out: &mut Nv12Frame) -> Result<DecodeStatus>;
    fn name(&self) -> String;
}

pub trait Injector {
    fn input(&mut self, i: Input);
}

/// Largest even size with the native aspect ratio that fits within `max`.
#[cfg_attr(windows, allow(dead_code))]
pub fn fit_size(native: (u32, u32), max: (u32, u32)) -> (u32, u32) {
    let (nw, nh) = native;
    let (mw, mh) = (max.0.max(320), max.1.max(240));
    let (w, h) = if nw <= mw && nh <= mh {
        (nw, nh)
    } else {
        let s = (mw as f64 / nw as f64).min(mh as f64 / nh as f64);
        ((nw as f64 * s).round() as u32, (nh as f64 * s).round() as u32)
    };
    (w & !1, h & !1)
}

pub type SharedSink = Arc<dyn FrameSink>;

#[cfg(test)]
mod tests {
    use super::fit_size;

    #[test]
    fn fit() {
        assert_eq!(fit_size((3840, 2160), (2560, 1440)), (2560, 1440));
        assert_eq!(fit_size((1920, 1080), (2560, 1600)), (1920, 1080));
        assert_eq!(fit_size((5120, 2880), (1920, 1200)), (1920, 1080));
        assert_eq!(fit_size((1366, 768), (4000, 4000)), (1366, 768));
    }
}
