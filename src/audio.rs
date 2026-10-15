//! Audio playback on the client (with a small jitter buffer), and system-audio capture on
//! Windows hosts (WASAPI loopback). macOS hosts capture audio through ScreenCaptureKit.

use std::collections::VecDeque;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use parking_lot::Mutex;

use crate::media::AUDIO_RATE;

/// Linear-interpolating stereo resampler.
pub struct Resampler {
    step: f64,
    t: f64,
    buf: Vec<[f32; 2]>,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        Self { step: from as f64 / to as f64, t: 0.0, buf: Vec::new() }
    }

    pub fn process(&mut self, input: &[[f32; 2]], out: &mut Vec<[f32; 2]>) {
        if (self.step - 1.0).abs() < 1e-9 {
            out.extend_from_slice(input);
            return;
        }
        self.buf.extend_from_slice(input);
        while self.t + 1.0 < self.buf.len() as f64 {
            let i = self.t as usize;
            let f = (self.t - i as f64) as f32;
            let (a, b) = (self.buf[i], self.buf[i + 1]);
            out.push([a[0] + (b[0] - a[0]) * f, a[1] + (b[1] - a[1]) * f]);
            self.t += self.step;
        }
        let consumed = (self.t as usize).min(self.buf.len());
        self.buf.drain(..consumed);
        self.t -= consumed as f64;
    }
}

struct PlayBuf {
    q: VecDeque<[f32; 2]>,
    primed: bool,
    target: usize,
    max: usize,
}

pub struct AudioPlayer {
    _stream: cpal::Stream,
    buf: Arc<Mutex<PlayBuf>>,
    resampler: Resampler,
    scratch: Vec<[f32; 2]>,
    out: Vec<[f32; 2]>,
}

impl AudioPlayer {
    pub fn new() -> Result<Self> {
        let host = cpal::default_host();
        let dev = host.default_output_device().ok_or_else(|| anyhow!("no audio output device"))?;
        let sc = dev.default_output_config()?;
        let rate = sc.sample_rate();
        let channels = sc.channels() as usize;
        let config = sc.config();
        let buf = Arc::new(Mutex::new(PlayBuf {
            q: VecDeque::new(),
            primed: false,
            target: rate as usize * 30 / 1000,
            max: rate as usize * 90 / 1000,
        }));
        let err = |e| log::warn!("audio output error: {e}");
        let stream = match sc.sample_format() {
            cpal::SampleFormat::F32 => {
                let b = buf.clone();
                dev.build_output_stream::<f32, _, _>(config, move |d, _| fill(&b, d, channels, |x| x), err, None)?
            }
            cpal::SampleFormat::I16 => {
                let b = buf.clone();
                dev.build_output_stream::<i16, _, _>(
                    config,
                    move |d, _| fill(&b, d, channels, |x| (x.clamp(-1.0, 1.0) * 32767.0) as i16),
                    err,
                    None,
                )?
            }
            f => bail!("unsupported output sample format {f:?}"),
        };
        stream.play()?;
        log::info!("audio output: {rate} Hz, {channels} ch");
        Ok(Self { _stream: stream, buf, resampler: Resampler::new(AUDIO_RATE, rate), scratch: Vec::new(), out: Vec::new() })
    }

    /// Queues interleaved stereo 48 kHz samples.
    pub fn push(&mut self, pcm: &[i16]) {
        self.scratch.clear();
        self.scratch.extend(pcm.chunks_exact(2).map(|c| [c[0] as f32 / 32768.0, c[1] as f32 / 32768.0]));
        self.out.clear();
        self.resampler.process(&self.scratch, &mut self.out);
        let mut b = self.buf.lock();
        b.q.extend(self.out.iter().copied());
        if b.q.len() > b.max {
            // Fell behind (clock drift or a burst): skip ahead to the target latency.
            let excess = b.q.len() - b.target;
            b.q.drain(..excess);
        }
    }
}

fn fill<T: Copy>(b: &Mutex<PlayBuf>, data: &mut [T], channels: usize, conv: impl Fn(f32) -> T) {
    let mut b = b.lock();
    if !b.primed && b.q.len() >= b.target {
        b.primed = true;
    }
    for frame in data.chunks_mut(channels) {
        let s = if b.primed { b.q.pop_front() } else { None };
        if s.is_none() {
            b.primed = false;
        }
        let [l, r] = s.unwrap_or([0.0, 0.0]);
        match channels {
            1 => frame[0] = conv((l + r) * 0.5),
            _ => {
                frame[0] = conv(l);
                frame[1] = conv(r);
                for x in frame.iter_mut().skip(2) {
                    *x = conv(0.0);
                }
            }
        }
    }
}

/// Captures whatever the default output device is playing (Windows only).
#[cfg(windows)]
pub fn spawn_loopback(sink: crate::platform::SharedSink, stop: Arc<std::sync::atomic::AtomicBool>) {
    use std::sync::atomic::Ordering;
    std::thread::Builder::new()
        .name("audio-loopback".into())
        .spawn(move || {
            let run = || -> Result<cpal::Stream> {
                let host = cpal::default_host();
                let dev = host.default_output_device().ok_or_else(|| anyhow!("no audio output device"))?;
                let sc = dev.default_output_config()?;
                let rate = sc.sample_rate();
                let ch = sc.channels() as usize;
                if sc.sample_format() != cpal::SampleFormat::F32 {
                    bail!("loopback format {:?} not supported", sc.sample_format());
                }
                let mut rs = Resampler::new(rate, AUDIO_RATE);
                let mut frames = Vec::new();
                let mut out = Vec::new();
                let mut pcm = Vec::new();
                let stream = dev.build_input_stream::<f32, _, _>(
                    sc.config(),
                    move |d: &[f32], _| {
                        frames.clear();
                        frames.extend(d.chunks_exact(ch).map(|f| if ch == 1 { [f[0], f[0]] } else { [f[0], f[1]] }));
                        out.clear();
                        rs.process(&frames, &mut out);
                        pcm.clear();
                        for [l, r] in &out {
                            pcm.push((l.clamp(-1.0, 1.0) * 32767.0) as i16);
                            pcm.push((r.clamp(-1.0, 1.0) * 32767.0) as i16);
                        }
                        sink.audio(&pcm);
                    },
                    |e| log::warn!("loopback error: {e}"),
                    None,
                )?;
                stream.play()?;
                log::info!("capturing system audio: {rate} Hz, {ch} ch");
                Ok(stream)
            };
            let stream = match run() {
                Ok(s) => s,
                Err(e) => {
                    log::warn!("system audio capture unavailable: {e:#}");
                    return;
                }
            };
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            drop(stream);
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::Resampler;

    #[test]
    fn resample_rate() {
        let mut r = Resampler::new(48000, 44100);
        let input = vec![[0.5f32, -0.5f32]; 48000];
        let mut out = Vec::new();
        for c in input.chunks(480) {
            r.process(c, &mut out);
        }
        assert!((out.len() as i64 - 44100).abs() < 5, "{}", out.len());
        assert!(out.iter().all(|s| (s[0] - 0.5).abs() < 1e-6));
    }
}
