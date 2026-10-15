//! Screen capture with DXGI Desktop Duplication. Frames are copied to a CPU staging texture and
//! converted to NV12 (with the mouse pointer drawn in) for the encoder.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO, IDXGIAdapter1,
    IDXGIFactory1, IDXGIOutput, IDXGIOutput1, IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::core::Interface;

use super::{CaptureShared, RawFrame, now_us};
use crate::convert::{self, CursorImage};
use crate::platform::SharedSink;
use crate::proto::CursorShape;

fn primary_output() -> Result<(IDXGIAdapter1, IDXGIOutput)> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut first = None;
        let mut i = 0;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            let mut j = 0;
            while let Ok(output) = adapter.EnumOutputs(j) {
                j += 1;
                let d = output.GetDesc()?;
                if !d.AttachedToDesktop.as_bool() {
                    continue;
                }
                if d.DesktopCoordinates.left == 0 && d.DesktopCoordinates.top == 0 {
                    return Ok((adapter, output));
                }
                if first.is_none() {
                    first = Some((adapter.clone(), output.clone()));
                }
            }
            i += 1;
        }
        first.ok_or_else(|| anyhow!("no display attached"))
    }
}

pub fn primary_output_size() -> Result<(u32, u32)> {
    let (_, output) = primary_output()?;
    let r = unsafe { output.GetDesc()? }.DesktopCoordinates;
    Ok(((r.right - r.left) as u32, (r.bottom - r.top) as u32))
}

struct Duplicator {
    _device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    dup: IDXGIOutputDuplication,
    staging: Option<ID3D11Texture2D>,
    width: u32,
    height: u32,
    have_frame: bool,
    cursor: Option<CursorImage>,
    cursor_hotspot: (i32, i32),
    cursor_pos: (i32, i32),
    cursor_visible: bool,
    /// Pointer changes not yet reported to the viewer: (visibility changed, shape changed).
    cursor_news: (bool, bool),
    shape_buf: Vec<u8>,
}

impl Duplicator {
    fn new() -> Result<Self> {
        let (adapter, output) = primary_output()?;
        unsafe {
            let mut device = None;
            let mut ctx = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )
            .context("D3D11CreateDevice")?;
            let device = device.ok_or_else(|| anyhow!("no D3D device"))?;
            let ctx = ctx.ok_or_else(|| anyhow!("no D3D context"))?;
            // DuplicateOutput1 asks Windows for an 8-bit copy, which also works on HDR desktops.
            let dup = match output.cast::<IDXGIOutput5>() {
                Ok(o5) => o5.DuplicateOutput1(&device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM]),
                Err(e) => Err(e),
            };
            let dup = match dup {
                Ok(d) => d,
                Err(_) => output.cast::<IDXGIOutput1>()?.DuplicateOutput(&device).context("DuplicateOutput")?,
            };
            let desc = dup.GetDesc();
            if desc.ModeDesc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
                log::warn!("desktop format {:?}; colors may be wrong", desc.ModeDesc.Format);
            }
            Ok(Self {
                _device: device,
                ctx,
                dup,
                staging: None,
                width: desc.ModeDesc.Width,
                height: desc.ModeDesc.Height,
                have_frame: false,
                cursor: None,
                cursor_hotspot: (0, 0),
                cursor_pos: (0, 0),
                cursor_visible: false,
                cursor_news: (false, false),
                shape_buf: Vec::new(),
            })
        }
    }

    /// Waits up to `timeout_ms` for a screen or pointer change. Returns true if there's a new
    /// image to send.
    fn next(&mut self, timeout_ms: u32) -> Result<bool> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut res: Option<IDXGIResource> = None;
        if let Err(e) = unsafe { self.dup.AcquireNextFrame(timeout_ms, &mut info, &mut res) } {
            if e.code() == DXGI_ERROR_WAIT_TIMEOUT {
                return Ok(false);
            }
            return Err(e.into());
        }
        let result = self.process(&info, res);
        unsafe {
            let _ = self.dup.ReleaseFrame();
        }
        result
    }

    fn process(&mut self, info: &DXGI_OUTDUPL_FRAME_INFO, res: Option<IDXGIResource>) -> Result<bool> {
        let mut changed = false;
        if info.LastMouseUpdateTime != 0 {
            let vis = info.PointerPosition.Visible.as_bool();
            let pos = (info.PointerPosition.Position.x, info.PointerPosition.Position.y);
            changed |= vis != self.cursor_visible || (vis && pos != self.cursor_pos);
            if vis != self.cursor_visible {
                self.cursor_news.0 = true;
            }
            self.cursor_visible = vis;
            self.cursor_pos = pos;
        }
        if info.PointerShapeBufferSize > 0 {
            self.shape_buf.resize(info.PointerShapeBufferSize as usize, 0);
            let mut needed = 0u32;
            let mut si = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            unsafe {
                self.dup.GetFramePointerShape(
                    self.shape_buf.len() as u32,
                    self.shape_buf.as_mut_ptr() as *mut _,
                    &mut needed,
                    &mut si,
                )?;
            }
            self.cursor = convert::cursor_from_dxgi(
                si.Type,
                si.Width as usize,
                si.Height as usize,
                si.Pitch as usize,
                &self.shape_buf,
            );
            self.cursor_hotspot = (si.HotSpot.x, si.HotSpot.y);
            self.cursor_news.1 = true;
            if self.cursor.is_none() {
                log::warn!("unsupported pointer shape type {} ({}x{})", si.Type, si.Width, si.Height);
            }
            changed = true;
        }
        if info.LastPresentTime != 0
            && let Some(res) = res
        {
            let tex: ID3D11Texture2D = res.cast()?;
            if self.staging.is_none() {
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                unsafe { tex.GetDesc(&mut desc) };
                desc.Usage = D3D11_USAGE_STAGING;
                desc.BindFlags = 0;
                desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
                desc.MiscFlags = 0;
                desc.MipLevels = 1;
                desc.ArraySize = 1;
                desc.SampleDesc.Count = 1;
                desc.SampleDesc.Quality = 0;
                let mut staging = None;
                unsafe { self._device.CreateTexture2D(&desc, None, Some(&mut staging))? };
                self.staging = staging;
            }
            if let Some(staging) = &self.staging {
                unsafe { self.ctx.CopyResource(staging, &tex) };
                self.have_frame = true;
                changed = true;
            }
        }
        Ok(changed && self.have_frame)
    }

    fn with_mapped<R>(&self, f: impl FnOnce(&[u8], usize) -> R) -> Result<R> {
        let staging = self.staging.as_ref().ok_or_else(|| anyhow!("no frame yet"))?;
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.ctx.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
            let data = std::slice::from_raw_parts(m.pData as *const u8, m.RowPitch as usize * self.height as usize);
            let r = f(data, m.RowPitch as usize);
            self.ctx.Unmap(staging, 0);
            Ok(r)
        }
    }
}

/// Tells the viewer about pointer visibility/shape changes.
fn report_cursor(dup: &mut Duplicator, sink: &SharedSink) {
    let (vis_changed, shape_changed) = std::mem::take(&mut dup.cursor_news);
    if !vis_changed && !shape_changed {
        return;
    }
    let shape = if shape_changed {
        dup.cursor.as_ref().filter(|c| c.w <= 256 && c.h <= 256).map(|c| CursorShape {
            w: c.w as u16,
            h: c.h as u16,
            hot_x: dup.cursor_hotspot.0.clamp(0, c.w as i32 - 1) as u16,
            hot_y: dup.cursor_hotspot.1.clamp(0, c.h as i32 - 1) as u16,
            rgba: convert::cursor_to_rgba(c),
        })
    } else {
        None
    };
    sink.cursor(dup.cursor_visible, shape.as_ref());
}

pub fn capture_thread(
    sh: Arc<CaptureShared>,
    sink: SharedSink,
    scale: usize,
    out: (u32, u32),
    ready: Sender<Result<()>>,
) {
    super::com_init();
    let mut ready = Some(ready);
    let mut pool: Vec<Arc<RawFrame>> = Vec::new();
    let mut patch = Vec::new();
    let mut logged_wait = false;
    let started = std::time::Instant::now();
    while !sh.stop.load(Ordering::Relaxed) {
        let mut dup = match Duplicator::new() {
            Ok(d) => d,
            Err(e) => {
                // A display that's just waking up can take a few seconds to become capturable.
                if started.elapsed() > Duration::from_secs(5)
                    && let Some(r) = ready.take()
                {
                    let _ = r.send(Err(e.context(
                        "can't capture the screen (is the PC's monitor switched off or unplugged?)",
                    )));
                    return;
                }
                // Typical while the lock screen or a UAC prompt (secure desktop) is up.
                if !logged_wait {
                    log::info!("waiting for the desktop: {e:#}");
                    logged_wait = true;
                }
                std::thread::sleep(Duration::from_millis(250));
                continue;
            }
        };
        logged_wait = false;
        let (sw, shh) = (dup.width as usize, dup.height as usize);
        let (ow, oh) = convert::output_size(sw, shh, scale);
        if (ow as u32, oh as u32) != out {
            let msg = format!("display is now {sw}x{shh}");
            log::info!("{msg}; capture will restart at the new size");
            if let Some(r) = ready.take() {
                let _ = r.send(Err(anyhow!(msg)));
            }
            sh.stop.store(true, Ordering::SeqCst);
            return;
        }
        if let Some(r) = ready.take() {
            log::info!("capturing {sw}x{shh}, scale 1/{scale} -> {ow}x{oh} (desktop ready after {:.1} s)", started.elapsed().as_secs_f64());
            let _ = r.send(Ok(()));
        }
        loop {
            if sh.stop.load(Ordering::Relaxed) {
                return;
            }
            let next = dup.next(100);
            report_cursor(&mut dup, &sink);
            let redraw = sh.redraw.swap(false, Ordering::AcqRel) && dup.have_frame;
            match next {
                Ok(false) if !redraw => continue,
                Ok(_) => {
                    let idx = match pool.iter().position(|f| Arc::strong_count(f) == 1) {
                        Some(i) => i,
                        None if pool.len() < 4 => {
                            pool.push(Arc::new(RawFrame {
                                nv12: vec![0; ow * oh * 3 / 2],
                                width: ow as u32,
                                height: oh as u32,
                                capture_us: 0,
                            }));
                            pool.len() - 1
                        }
                        None => continue, // encoder is behind; skip this frame
                    };
                    let frame = Arc::get_mut(&mut pool[idx]).expect("unshared frame");
                    let cursor = if dup.cursor_visible && sh.cursor_in_video.load(Ordering::Relaxed) {
                        dup.cursor.as_ref().map(|c| (c, dup.cursor_pos.0, dup.cursor_pos.1))
                    } else {
                        None
                    };
                    let nv12 = &mut frame.nv12;
                    let r = dup.with_mapped(|data, pitch| {
                        convert::convert_with_cursor(data, pitch, sw, shh, scale, nv12, cursor, &mut patch)
                    });
                    if let Err(e) = r {
                        log::warn!("map failed: {e:#}");
                        break;
                    }
                    frame.capture_us = now_us();
                    sh.publish(pool[idx].clone());
                    sh.frames_captured.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    // Mode change, fullscreen app switch, secure desktop...: start over.
                    log::info!("duplication interrupted: {e:#}");
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}
