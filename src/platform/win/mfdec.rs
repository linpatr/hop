//! H.264 decoding with Microsoft's Media Foundation decoder in low-latency mode.

use anyhow::{Context, Result};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::core::{GUID, Interface};

use super::mfenc::variant_bool;
use crate::platform::{DecodeStatus, Nv12Frame, VideoDecoder};

/// CLSID_CMSH264DecoderMFT
const CLSID_MS_H264_DECODER: GUID = GUID::from_u128(0x62ce7e72_4c71_4d20_b15d_452831a87d9d);

pub struct MfDecoder {
    t: IMFTransform,
    alloc_w: u32,
    alloc_h: u32,
    vis_w: u32,
    vis_h: u32,
    out_size: u32,
    provides_samples: bool,
    frame_no: i64,
}

impl MfDecoder {
    pub fn new() -> Result<Self> {
        unsafe {
            let t: IMFTransform =
                CoCreateInstance(&CLSID_MS_H264_DECODER, None, CLSCTX_INPROC_SERVER).context("create H.264 decoder")?;
            if let Ok(a) = t.GetAttributes() {
                let _ = a.SetUINT32(&MF_LOW_LATENCY, 1);
            }
            if let Ok(c) = t.cast::<ICodecAPI>() {
                let _ = c.SetValue(&CODECAPI_AVLowLatencyMode, &variant_bool(true));
            }
            let inp = MFCreateMediaType()?;
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            t.SetInputType(0, &inp, 0).context("decoder input type")?;
            let mut d =
                Self { t, alloc_w: 0, alloc_h: 0, vis_w: 0, vis_h: 0, out_size: 0, provides_samples: false, frame_no: 0 };
            d.select_output()?;
            d.t.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            d.t.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(d)
        }
    }

    fn select_output(&mut self) -> Result<()> {
        unsafe {
            let mut i = 0;
            loop {
                let ty = self.t.GetOutputAvailableType(0, i).context("no NV12 output from decoder")?;
                i += 1;
                if ty.GetGUID(&MF_MT_SUBTYPE)? != MFVideoFormat_NV12 {
                    continue;
                }
                self.t.SetOutputType(0, &ty, 0)?;
                let size = ty.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
                self.alloc_w = (size >> 32) as u32;
                self.alloc_h = size as u32;
                self.vis_w = self.alloc_w;
                self.vis_h = self.alloc_h;
                if let Ok(n) = ty.GetBlobSize(&MF_MT_MINIMUM_DISPLAY_APERTURE)
                    && n as usize >= 16
                {
                    let mut b = vec![0u8; n as usize];
                    if ty.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, &mut b, None).is_ok() {
                        let cx = i32::from_le_bytes(b[8..12].try_into().unwrap());
                        let cy = i32::from_le_bytes(b[12..16].try_into().unwrap());
                        if cx > 0 && cy > 0 {
                            self.vis_w = cx as u32;
                            self.vis_h = cy as u32;
                        }
                    }
                }
                let info = self.t.GetOutputStreamInfo(0)?;
                self.provides_samples = info.dwFlags
                    & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
                    != 0;
                self.out_size = info.cbSize.max(self.alloc_w * self.alloc_h * 3 / 2);
                return Ok(());
            }
        }
    }

    fn pull(&mut self, out: &mut Nv12Frame) -> Result<Option<bool>> {
        unsafe {
            let mut odb = MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, ..Default::default() };
            if !self.provides_samples {
                let s = MFCreateSample()?;
                s.AddBuffer(&MFCreateMemoryBuffer(self.out_size)?)?;
                odb.pSample = std::mem::ManuallyDrop::new(Some(s));
            }
            let mut status = 0u32;
            let r = self.t.ProcessOutput(0, std::slice::from_mut(&mut odb), &mut status);
            let sample = std::mem::ManuallyDrop::take(&mut odb.pSample);
            drop(std::mem::ManuallyDrop::take(&mut odb.pEvents));
            match r {
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    self.select_output()?;
                    return Ok(Some(false));
                }
                Err(e) => return Err(e.into()),
                Ok(()) => {}
            }
            let Some(sample) = sample else { return Ok(Some(false)) };
            let buf = sample.GetBufferByIndex(0)?;
            if let Ok(b2) = buf.cast::<IMF2DBuffer>() {
                let mut scan0 = std::ptr::null_mut();
                let mut pitch = 0i32;
                b2.Lock2D(&mut scan0, &mut pitch)?;
                let pitch = pitch.unsigned_abs() as usize;
                let uv = scan0.add(pitch * self.alloc_h as usize);
                out.copy_from(self.vis_w, self.vis_h, scan0, pitch, uv, pitch);
                b2.Unlock2D()?;
            } else {
                let mut p = std::ptr::null_mut();
                buf.Lock(&mut p, None, None)?;
                let pitch = self.alloc_w as usize;
                out.copy_from(self.vis_w, self.vis_h, p, pitch, p.add(pitch * self.alloc_h as usize), pitch);
                buf.Unlock()?;
            }
            Ok(Some(true))
        }
    }
}

impl VideoDecoder for MfDecoder {
    fn decode(&mut self, data: &[u8], out: &mut Nv12Frame) -> Result<DecodeStatus> {
        unsafe {
            let buf = MFCreateMemoryBuffer(data.len() as u32)?;
            let mut p = std::ptr::null_mut();
            buf.Lock(&mut p, None, None)?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
            buf.Unlock()?;
            buf.SetCurrentLength(data.len() as u32)?;
            let s = MFCreateSample()?;
            s.AddBuffer(&buf)?;
            self.frame_no += 1;
            s.SetSampleTime(self.frame_no * 166_667)?;
            let mut produced = false;
            loop {
                match self.t.ProcessInput(0, &s, 0) {
                    Ok(()) => break,
                    Err(e) if e.code() == MF_E_NOTACCEPTING => {
                        // Output pending: drain it, then retry.
                        match self.pull(out)? {
                            Some(p) => produced |= p,
                            None => break,
                        }
                    }
                    Err(e) => {
                        log::warn!("decoder rejected input: {e}");
                        return Ok(DecodeStatus::NeedKeyframe);
                    }
                }
            }
            while let Some(p) = self.pull(out)? {
                produced |= p;
            }
            Ok(if produced { DecodeStatus::Frame } else { DecodeStatus::NoFrame })
        }
    }

    fn name(&self) -> String {
        "Media Foundation".into()
    }
}
