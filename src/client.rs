//! Client: connects to a host, receives and decodes the stream, sends input back.

use std::net::{Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use parking_lot::Mutex;
use socket2::{Domain, Protocol, Socket, Type};

use crate::clipboard::ClipState;
use crate::crypto::{PacketCipher, SessionKeys, StreamCipher, client_handshake, read_frame, write_frame};
use crate::media::{KIND_AUDIO, KIND_PUNCH, KIND_VIDEO, Reassembler, decode_audio};
use crate::platform::{self, DecodeStatus, Nv12Frame};
use crate::proto::{CONTROL_PORT, Ctrl, Hello, Input, MEDIA_PORT, Message, PROTOCOL_VERSION, decode_message, encode_ctrl, encode_input};

pub struct ConnectOptions {
    pub addr: String,
    pub code: String,
    pub id: String,
    pub name: String,
    pub max_width: u32,
    pub max_height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub audio: bool,
    pub fec_percent: u32,
    pub clipboard: bool,
}

#[derive(Debug)]
pub enum ConnectError {
    WrongCode,
    Host(String),
    Other(anyhow::Error),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::WrongCode => write!(f, "wrong access code"),
            ConnectError::Host(m) => write!(f, "{m}"),
            ConnectError::Other(e) => write!(f, "{e:#}"),
        }
    }
}

impl From<anyhow::Error> for ConnectError {
    fn from(e: anyhow::Error) -> Self {
        ConnectError::Other(e)
    }
}

#[derive(Clone, Debug)]
pub struct StreamInfo {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub encoder: String,
}

pub struct Connection {
    pub host: Hello,
    pub info: StreamInfo,
    tcp: TcpStream,
    tx: StreamCipher,
    rx: StreamCipher,
    keys: SessionKeys,
    host_media: SocketAddr,
    audio: bool,
    clipboard: bool,
}

pub fn resolve(addr: &str) -> Result<SocketAddr> {
    let with_port = if addr.contains(':') { addr.to_string() } else { format!("{addr}:{CONTROL_PORT}") };
    with_port
        .to_socket_addrs()
        .with_context(|| format!("can't resolve {addr}"))?
        .find(|a| a.is_ipv4())
        .ok_or_else(|| anyhow!("no IPv4 address for {addr}"))
}

pub fn connect(o: &ConnectOptions) -> Result<Connection, ConnectError> {
    let addr = resolve(&o.addr)?;
    let mut tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
        .with_context(|| format!("can't reach {addr} (is Hop running there?)"))?;
    tcp.set_nodelay(true).ok();
    tcp.set_read_timeout(Some(Duration::from_secs(10))).ok();
    tcp.set_write_timeout(Some(Duration::from_secs(5))).ok();
    let me = Hello { hop: PROTOCOL_VERSION, id: o.id.clone(), name: o.name.clone(), os: crate::proto::os_name().into() };
    let (host, keys) = client_handshake(&mut tcp, &o.code, &me)?;
    let mut tx = StreamCipher::new(&keys.c2s);
    let mut rx = StreamCipher::new(&keys.s2c);
    let welcome = read_frame(&mut tcp).map_err(|e| ConnectError::Other(e.context("host closed the connection")))?;
    let welcome = rx.open(&welcome).map_err(|_| ConnectError::WrongCode)?;
    let Message::Ctrl(Ctrl::Welcome { native_width, native_height }) = decode_message(&welcome)? else {
        return Err(anyhow!("unexpected message from host").into());
    };
    log::info!("connected to {} ({}), native {native_width}x{native_height}", host.name, host.os);
    let start = Ctrl::Start {
        max_width: o.max_width,
        max_height: o.max_height,
        fps: o.fps,
        bitrate_kbps: o.bitrate_kbps,
        audio: o.audio,
        fec_percent: o.fec_percent,
        clipboard: o.clipboard,
    };
    write_frame(&mut tcp, &tx.seal(&encode_ctrl(&start)))?;
    // Starting capture can take a moment (and may wait on a permission prompt on the host).
    tcp.set_read_timeout(Some(Duration::from_secs(30))).ok();
    let info = loop {
        let f = read_frame(&mut tcp).context("host closed the connection while starting the stream")?;
        match decode_message(&rx.open(&f)?)? {
            Message::Ctrl(Ctrl::StreamInfo { width, height, fps, encoder }) => {
                break StreamInfo { width, height, fps, encoder };
            }
            Message::Ctrl(Ctrl::Error { message }) => return Err(ConnectError::Host(message)),
            _ => {}
        }
    };
    log::info!("stream {}x{}@{} ({})", info.width, info.height, info.fps, info.encoder);
    Ok(Connection {
        host,
        info,
        tcp,
        tx,
        rx,
        keys,
        host_media: SocketAddr::new(addr.ip(), MEDIA_PORT),
        audio: o.audio,
        clipboard: o.clipboard,
    })
}

#[derive(Clone, Default, Debug)]
pub struct ClientStats {
    pub fps: f32,
    pub mbps: f32,
    pub lost: u64,
    pub recovered: u64,
    pub rtt_ms: f32,
    pub host_ms: f32,
    pub decode_ms: f32,
    pub decoder: String,
    pub width: u32,
    pub height: u32,
}

pub enum Event {
    Frame,
    Closed(String),
}

pub struct ClientShared {
    pub frame: Mutex<Option<Nv12Frame>>,
    pub spare: Mutex<Vec<Nv12Frame>>,
    pub stats: Mutex<ClientStats>,
    pub stop: AtomicBool,
    writer: Mutex<(TcpStream, StreamCipher)>,
}

impl ClientShared {
    pub fn send_input(&self, i: Input) {
        let mut w = self.writer.lock();
        let (s, c) = &mut *w;
        let _ = write_frame(s, &c.seal(&encode_input(&i)));
    }

    pub fn send_ctrl(&self, m: &Ctrl) {
        let mut w = self.writer.lock();
        let (s, c) = &mut *w;
        let _ = write_frame(s, &c.seal(&encode_ctrl(m)));
    }

    /// Gives a displayed frame's buffers back for reuse.
    pub fn recycle(&self, f: Nv12Frame) {
        let mut s = self.spare.lock();
        if s.len() < 3 {
            s.push(f);
        }
    }

    pub fn close(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.send_ctrl(&Ctrl::Bye);
        let _ = self.writer.lock().0.shutdown(std::net::Shutdown::Both);
    }
}

fn media_socket() -> Result<UdpSocket> {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    let _ = s.set_recv_buffer_size(8 << 20);
    let _ = s.set_tos_v4(0x88);
    s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)).into())?;
    let s: UdpSocket = s.into();
    s.set_read_timeout(Some(Duration::from_millis(5)))?;
    Ok(s)
}

pub fn start(conn: Connection, notify: impl Fn(Event) + Send + Sync + 'static) -> Result<Arc<ClientShared>> {
    let Connection { info, mut tcp, tx, mut rx, keys, host_media, audio, clipboard, .. } = conn;
    let notify = Arc::new(notify);
    tcp.set_read_timeout(Some(Duration::from_secs(10)))?;
    let shared = Arc::new(ClientShared {
        frame: Mutex::new(None),
        spare: Mutex::new(Vec::new()),
        stats: Mutex::new(ClientStats { width: info.width, height: info.height, ..Default::default() }),
        stop: AtomicBool::new(false),
        writer: Mutex::new((tcp.try_clone()?, tx)),
    });
    let rtt = Arc::new(Mutex::new(0f32));
    let clip = clipboard.then(|| {
        let c = ClipState::new();
        let sh = shared.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let sh2 = shared.clone();
        std::thread::spawn(move || {
            while !sh2.stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(200));
            }
            stop2.store(true, Ordering::SeqCst);
        });
        c.watch(stop, move |text| sh.send_ctrl(&Ctrl::Clipboard { text }));
        c
    });

    // Control channel reader.
    {
        let shared = shared.clone();
        let notify = notify.clone();
        let rtt = rtt.clone();
        std::thread::Builder::new().name("client-ctrl".into()).spawn(move || {
            let reason = loop {
                let f = match read_frame(&mut tcp) {
                    Ok(f) => f,
                    Err(_) if shared.stop.load(Ordering::Relaxed) => break String::new(),
                    Err(_) => break "Lost connection to the host".to_string(),
                };
                let Ok(pt) = rx.open(&f) else { break "Corrupt data from host".into() };
                match decode_message(&pt) {
                    Ok(Message::Ctrl(Ctrl::Pong { ts })) => {
                        let now = platform::now_us();
                        *rtt.lock() = now.saturating_sub(ts) as f32 / 1000.0;
                    }
                    Ok(Message::Ctrl(Ctrl::Clipboard { text })) => {
                        if let Some(c) = &clip {
                            c.apply(text);
                        }
                    }
                    Ok(Message::Ctrl(Ctrl::Error { message })) => break message,
                    Ok(Message::Ctrl(Ctrl::Bye)) => break "The host ended the session".into(),
                    _ => {}
                }
            };
            shared.stop.store(true, Ordering::SeqCst);
            notify(Event::Closed(reason));
        })?;
    }

    // Media receiver + decoder.
    {
        let shared = shared.clone();
        let sock = media_socket()?;
        std::thread::Builder::new().name("client-media".into()).spawn(move || {
            if let Err(e) = media_loop(&shared, sock, &keys, host_media, audio, rtt, &*notify) {
                log::error!("media loop: {e:#}");
                shared.stop.store(true, Ordering::SeqCst);
                notify(Event::Closed(format!("{e:#}")));
            }
        })?;
    }
    Ok(shared)
}

fn media_loop(
    shared: &Arc<ClientShared>,
    sock: UdpSocket,
    keys: &SessionKeys,
    host_media: SocketAddr,
    want_audio: bool,
    rtt: Arc<Mutex<f32>>,
    notify: &(dyn Fn(Event) + Send + Sync),
) -> Result<()> {
    let media = PacketCipher::new(&keys.media);
    let punch = PacketCipher::new(&keys.punch);
    let mut punch_seq = 0u64;
    let mut punch_pkt = Vec::new();
    let mut send_punch = |sock: &UdpSocket| {
        punch.seal_into(KIND_PUNCH, punch_seq, b"punch", &mut punch_pkt);
        punch_seq += 1;
        let _ = sock.send_to(&punch_pkt, host_media);
    };

    let mut decoder = platform::new_decoder()?;
    let decoder_name = decoder.name();
    let mut player = if want_audio {
        match crate::audio::AudioPlayer::new() {
            Ok(p) => Some(p),
            Err(e) => {
                log::warn!("no audio output: {e:#}");
                None
            }
        }
    } else {
        None
    };

    let mut reasm = Reassembler::new();
    let mut buf = vec![0u8; 65536];
    let mut pcm = Vec::new();
    let mut got_media = false;
    let mut last_punch = Instant::now() - Duration::from_secs(10);
    let mut last_ping = Instant::now();
    let mut last_kf_request = Instant::now() - Duration::from_secs(10);
    let mut last_stats = Instant::now();
    let (mut frames, mut decode_us, mut host_us) = (0u32, 0u64, 0u64);
    let mut bytes_at_last = 0u64;
    let mut stats_ticks = 0u32;

    while !shared.stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        let punch_every = if got_media { Duration::from_secs(1) } else { Duration::from_millis(100) };
        if now - last_punch > punch_every {
            send_punch(&sock);
            last_punch = now;
        }
        if now - last_ping > Duration::from_secs(1) {
            shared.send_ctrl(&Ctrl::Ping { ts: platform::now_us() });
            last_ping = now;
        }
        if reasm.expire() {
            log::debug!("frame timed out");
        }
        if reasm.need_keyframe && now - last_kf_request > Duration::from_millis(150) {
            shared.send_ctrl(&Ctrl::RequestKeyframe);
            last_kf_request = now;
        }
        if now - last_stats > Duration::from_secs(1) {
            let dt = (now - last_stats).as_secs_f32();
            let mut s = shared.stats.lock();
            s.fps = frames as f32 / dt;
            s.mbps = (reasm.stats.bytes - bytes_at_last) as f32 * 8.0 / dt / 1e6;
            s.lost = reasm.stats.lost;
            s.recovered = reasm.stats.recovered;
            s.rtt_ms = *rtt.lock();
            if frames > 0 {
                s.decode_ms = decode_us as f32 / frames as f32 / 1000.0;
                s.host_ms = host_us as f32 / frames as f32 / 1000.0;
            }
            s.decoder = decoder_name.clone();
            stats_ticks += 1;
            if stats_ticks % 5 == 0 {
                log::info!(
                    "stats: {:.0} fps, {:.1} Mbps, host {:.1} ms, rtt {:.1} ms, decode {:.1} ms, lost {}, recovered {}",
                    s.fps, s.mbps, s.host_ms, s.rtt_ms, s.decode_ms, s.lost, s.recovered
                );
            }
            bytes_at_last = reasm.stats.bytes;
            frames = 0;
            decode_us = 0;
            host_us = 0;
            last_stats = now;
        }

        let n = match sock.recv_from(&mut buf) {
            Ok((n, from)) if from.ip() == host_media.ip() => n,
            Ok(_) => continue,
            Err(_) => continue,
        };
        let Some((kind, _, payload)) = media.open(&buf[..n]) else { continue };
        got_media = true;
        match kind {
            KIND_VIDEO => {
                let Some(f) = reasm.push(&payload) else { continue };
                let t0 = Instant::now();
                let mut out = shared.spare.lock().pop().unwrap_or_default();
                match decoder.decode(&f.data, &mut out)? {
                    DecodeStatus::Frame => {
                        if reasm.stats.frames == 120
                            && let Ok(path) = std::env::var("HOP_DUMP_FRAME")
                        {
                            dump_frame(&out, &path);
                        }
                        decode_us += t0.elapsed().as_micros() as u64;
                        host_us += f.host_latency_us as u64;
                        frames += 1;
                        if let Some(old) = shared.frame.lock().replace(out) {
                            shared.recycle(old);
                        }
                        notify(Event::Frame);
                    }
                    DecodeStatus::NoFrame => shared.recycle(out),
                    DecodeStatus::NeedKeyframe => {
                        shared.recycle(out);
                        reasm.need_keyframe = true;
                    }
                }
            }
            KIND_AUDIO => {
                if let Some(p) = player.as_mut()
                    && decode_audio(&payload, &mut pcm)
                {
                    p.push(&pcm);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Debug aid (HOP_DUMP_FRAME=path.ppm): writes a decoded frame as RGB, using the same BT.709
/// conversion as the renderer's shader.
fn dump_frame(f: &Nv12Frame, path: &str) {
    let (w, h) = (f.width as usize, f.height as usize);
    let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
    for y in 0..h {
        for x in 0..w {
            let l = (f.y[y * w + x] as f32 / 255.0 - 16.0 / 255.0) * (255.0 / 219.0);
            let c = (y / 2) * w + (x & !1);
            let u = (f.uv[c] as f32 / 255.0 - 128.0 / 255.0) * (255.0 / 224.0);
            let v = (f.uv[c + 1] as f32 / 255.0 - 128.0 / 255.0) * (255.0 / 224.0);
            for ch in [l + 1.5748 * v, l - 0.1873 * u - 0.4681 * v, l + 1.8556 * u] {
                out.push((ch.clamp(0.0, 1.0) * 255.0).round() as u8);
            }
        }
    }
    match std::fs::write(path, out) {
        Ok(()) => log::info!("dumped frame to {path}"),
        Err(e) => log::warn!("dump failed: {e}"),
    }
}
