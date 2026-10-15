//! `hop selftest`: captures and encodes a few seconds of the screen, decodes it back and reports
//! timings. Also hosts the Windows installer download.

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::platform::{self, CaptureConfig, DecodeStatus, FrameSink, Nv12Frame};

#[derive(Default)]
struct Collect {
    frames: Mutex<Vec<(Vec<u8>, bool, u64)>>,
    audio_samples: AtomicU64,
}

impl FrameSink for Collect {
    fn video(&self, data: &[u8], keyframe: bool, capture_us: u64) {
        let lat = platform::now_us().saturating_sub(capture_us);
        self.frames.lock().push((data.to_vec(), keyframe, lat));
    }
    fn audio(&self, pcm: &[i16]) {
        self.audio_samples.fetch_add(pcm.len() as u64 / 2, Ordering::Relaxed);
    }
}

macro_rules! say {
    ($($t:tt)*) => {{ let s = format!($($t)*); println!("{s}"); log::info!("{s}"); }};
}

pub fn run() -> i32 {
    let mut ok = true;
    say!("Hop {} selftest on {}", env!("CARGO_PKG_VERSION"), crate::proto::os_name());
    let native = platform::native_display_size();
    let (w, h) = platform::stream_size(native, (1920, 1080));
    say!("display {}x{} -> streaming {w}x{h}", native.0, native.1);

    let sink = Arc::new(Collect::default());
    let cfg = CaptureConfig { width: w, height: h, fps: 60, bitrate_kbps: 30_000, audio: true };
    let t0 = Instant::now();
    match platform::start_capture(&cfg, sink.clone()) {
        Ok(cap) => {
            say!("capture started in {:.0} ms with {}", t0.elapsed().as_secs_f64() * 1000.0, cap.encoder_name());
            for i in 0..30 {
                std::thread::sleep(Duration::from_millis(100));
                if i == 15 {
                    cap.request_keyframe();
                }
            }
            drop(cap);
        }
        Err(e) => {
            say!("FAIL capture: {e:#}");
            return 1;
        }
    }
    let frames = std::mem::take(&mut *sink.frames.lock());
    let keys = frames.iter().filter(|f| f.1).count();
    let bytes: usize = frames.iter().map(|f| f.0.len()).sum();
    let avg_lat = frames.iter().map(|f| f.2).sum::<u64>() as f64 / frames.len().max(1) as f64 / 1000.0;
    say!(
        "encoded {} frames in 3 s ({} keyframes, {:.1} Mbps, capture->encoded {:.1} ms avg)",
        frames.len(),
        keys,
        bytes as f64 * 8.0 / 3.0 / 1e6,
        avg_lat
    );
    let mut lats: Vec<u64> = frames.iter().skip(5).map(|f| f.2).collect();
    lats.sort_unstable();
    if !lats.is_empty() {
        let pct = |p: usize| lats[(lats.len() - 1) * p / 100] as f64 / 1000.0;
        say!("capture->encoded latency: min {:.1} / median {:.1} / p95 {:.1} / max {:.1} ms", pct(0), pct(50), pct(95), pct(100));
    }
    say!("captured {} audio frames ({})", sink.audio_samples.load(Ordering::Relaxed), if sink.audio_samples.load(Ordering::Relaxed) > 0 { "sound is playing" } else { "silence or no audio capture" });
    if frames.is_empty() {
        say!("FAIL no frames (on a static screen this is normal only if nothing changed; move the mouse and retry)");
        ok = false;
    } else if !frames[0].1 {
        say!("FAIL first frame is not a keyframe");
        ok = false;
    }

    match platform::new_decoder() {
        Ok(mut dec) => {
            let mut out = Nv12Frame::default();
            let (mut decoded, mut total) = (0, Duration::ZERO);
            for (data, _, _) in &frames {
                let t = Instant::now();
                match dec.decode(data, &mut out) {
                    Ok(DecodeStatus::Frame) => {
                        decoded += 1;
                        total += t.elapsed();
                    }
                    Ok(_) => {}
                    Err(e) => {
                        say!("FAIL decode error: {e:#}");
                        ok = false;
                        break;
                    }
                }
            }
            say!(
                "decoded {decoded}/{} frames with {} at {}x{}, {:.2} ms avg",
                frames.len(),
                dec.name(),
                out.width,
                out.height,
                total.as_secs_f64() * 1000.0 / decoded.max(1) as f64
            );
            if decoded == 0 && !frames.is_empty() {
                ok = false;
            }
            if decoded > 0 {
                let mid = out.y[out.y.len() / 2];
                say!("sample luma at center: {mid}");
            }
        }
        Err(e) => {
            say!("FAIL decoder: {e:#}");
            ok = false;
        }
    }

    for (name, r) in [
        ("TCP 47121", TcpListener::bind((Ipv4Addr::UNSPECIFIED, crate::proto::CONTROL_PORT)).map(|_| ())),
        ("UDP 47122", std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, crate::proto::MEDIA_PORT)).map(|_| ())),
    ] {
        match r {
            Ok(()) => say!("port {name} free"),
            Err(e) => say!("port {name} busy ({e}) — fine if Hop is already running"),
        }
    }
    say!("{}", if ok { "PASS" } else { "FAILED" });
    say!("log: {}", crate::logging::path("hop-selftest.log").display());
    if ok { 0 } else { 1 }
}

/// Folder with the Windows build (Hop.exe + install.ps1) to hand out on the LAN, if bundled.
fn installer_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    [dir.join("../Resources/windows"), dir.join("windows")].into_iter().find(|d| d.join("Hop.exe").exists())
}

/// Serves the Windows installer over HTTP on the LAN (so the PC can `iwr ... | iex` it).
pub fn start_installer_server() -> Option<()> {
    let dir = installer_dir()?;
    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, crate::proto::INSTALL_PORT)).ok()?;
    std::thread::Builder::new()
        .name("installer-http".into())
        .spawn(move || {
            for s in listener.incoming().flatten() {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
                    let mut r = BufReader::new(&s);
                    let mut line = String::new();
                    if r.read_line(&mut line).is_err() {
                        return;
                    }
                    let mut host = String::new();
                    loop {
                        let mut h = String::new();
                        if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                            break;
                        }
                        if let Some(v) = h.strip_prefix("Host:").or_else(|| h.strip_prefix("host:")) {
                            host = v.trim().to_string();
                        }
                    }
                    let path = line.split_whitespace().nth(1).unwrap_or("/");
                    let (body, ctype) = match path {
                        "/install.ps1" | "/" => match std::fs::read_to_string(dir.join("install.ps1")) {
                            Ok(t) => (t.replace("{{BASE}}", &format!("http://{host}")).into_bytes(), "text/plain; charset=utf-8"),
                            Err(_) => (Vec::new(), ""),
                        },
                        "/Hop.exe" => (std::fs::read(dir.join("Hop.exe")).unwrap_or_default(), "application/octet-stream"),
                        _ => (Vec::new(), ""),
                    };
                    let mut w = &s;
                    if body.is_empty() {
                        let _ = w.write_all(b"HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\n\r\n");
                        log::info!("installer: {path} not found ({:?})", s.peer_addr());
                        return;
                    } else {
                        let _ = write!(w, "HTTP/1.0 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\r\n", body.len());
                        let _ = w.write_all(&body);
                    }
                    log::info!("installer: served {path} to {:?}", s.peer_addr());
                });
            }
        })
        .ok()?;
    Some(())
}
