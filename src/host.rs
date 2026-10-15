//! Host: accepts one remote viewer at a time, streams the screen and injects its input.

use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use parking_lot::Mutex;
use socket2::{Domain, Protocol, Socket, Type};

use crate::clipboard::ClipState;
use crate::crypto::{PacketCipher, StreamCipher, host_handshake, host_refuse, read_frame, write_frame};
use crate::media::{AUDIO_FRAMES_PER_PACKET, KIND_AUDIO, KIND_PUNCH, KIND_VIDEO, Packetizer, encode_audio};
use crate::platform::{self, CaptureConfig, Capturer, FrameSink};
use crate::proto::{CONTROL_PORT, Ctrl, Hello, Input, MEDIA_PORT, Message, PROTOCOL_VERSION, decode_message, encode_ctrl};

pub struct HostOptions {
    pub id: String,
    pub name: String,
    pub code: Arc<Mutex<String>>,
}

#[derive(Clone)]
pub struct SessionInfo {
    pub client: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub encoder: String,
    pub sent_bytes: Arc<AtomicU64>,
    pub since: Instant,
}

#[derive(Clone, Default)]
pub struct HostStatus {
    pub running: bool,
    pub error: Option<String>,
    pub session: Option<SessionInfo>,
    pub last_event: Option<String>,
}

struct Active {
    id: u64,
    punch: PacketCipher,
    client_addr: Mutex<Option<SocketAddr>>,
    tcp: TcpStream,
    capturer: Mutex<Option<Box<dyn Capturer>>>,
}

impl Active {
    fn kick(&self) {
        let _ = self.tcp.shutdown(std::net::Shutdown::Both);
    }
}

struct Shared {
    id: String,
    name: String,
    code: Arc<Mutex<String>>,
    status: Arc<Mutex<HostStatus>>,
    media: Arc<UdpSocket>,
    active: Mutex<Option<Arc<Active>>>,
    next_id: AtomicU64,
    failures: Mutex<(u32, Option<Instant>)>,
    stop: Arc<AtomicBool>,
}

pub struct Host {
    pub status: Arc<Mutex<HostStatus>>,
    shared: Arc<Shared>,
}

impl Host {
    /// Ends the current remote session, if any.
    pub fn disconnect(&self) {
        if let Some(a) = self.shared.active.lock().clone() {
            a.kick();
        }
    }

    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(a) = self.shared.active.lock().clone() {
            a.kick();
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.stop();
    }
}

fn media_socket() -> Result<UdpSocket> {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    let _ = s.set_send_buffer_size(4 << 20);
    let _ = s.set_recv_buffer_size(1 << 20);
    // DSCP AF41: Wi-Fi access points put this in the video queue.
    let _ = s.set_tos_v4(0x88);
    s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, MEDIA_PORT)).into())
        .with_context(|| format!("UDP port {MEDIA_PORT} is in use (is Hop already running?)"))?;
    let s: UdpSocket = s.into();
    s.set_read_timeout(Some(Duration::from_millis(250)))?;
    Ok(s)
}

pub fn start_host(opts: HostOptions) -> Result<Host> {
    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, CONTROL_PORT))
        .with_context(|| format!("TCP port {CONTROL_PORT} is in use (is Hop already running?)"))?;
    listener.set_nonblocking(true)?;
    let media = Arc::new(media_socket()?);
    let status = Arc::new(Mutex::new(HostStatus { running: true, ..Default::default() }));
    let stop = Arc::new(AtomicBool::new(false));
    let shared = Arc::new(Shared {
        id: opts.id.clone(),
        name: opts.name.clone(),
        code: opts.code,
        status: status.clone(),
        media,
        active: Mutex::new(None),
        next_id: AtomicU64::new(1),
        failures: Mutex::new((0, None)),
        stop: stop.clone(),
    });

    crate::discovery::start_beacon(opts.id, opts.name, stop.clone());

    let sh = shared.clone();
    std::thread::Builder::new().name("host-accept".into()).spawn(move || accept_loop(sh, listener))?;
    let sh = shared.clone();
    std::thread::Builder::new().name("host-punch".into()).spawn(move || punch_loop(sh))?;

    log::info!("hosting \"{}\" on TCP {CONTROL_PORT} / UDP {MEDIA_PORT}", shared.name);
    Ok(Host { status, shared })
}

fn accept_loop(shared: Arc<Shared>, listener: TcpListener) {
    while !shared.stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((s, peer)) => {
                // Accepted sockets inherit the listener's non-blocking mode on macOS.
                let _ = s.set_nonblocking(false);
                let sh = shared.clone();
                let _ = std::thread::Builder::new().name("host-session".into()).spawn(move || {
                    if let Err(e) = handle_client(&sh, s, peer) {
                        log::warn!("session with {peer} ended: {e:#}");
                        sh.status.lock().last_event = Some(format!("{e:#}"));
                    }
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => {
                log::warn!("accept: {e}");
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
    shared.status.lock().running = false;
}

fn punch_loop(shared: Arc<Shared>) {
    let mut buf = [0u8; 2048];
    while !shared.stop.load(Ordering::Relaxed) {
        let Ok((n, from)) = shared.media.recv_from(&mut buf) else { continue };
        let Some(a) = shared.active.lock().clone() else { continue };
        if let Some((KIND_PUNCH, _, pt)) = a.punch.open(&buf[..n])
            && pt == b"punch"
        {
            let mut addr = a.client_addr.lock();
            if *addr != Some(from) {
                log::info!("media path to {from} open");
                *addr = Some(from);
                drop(addr);
                if let Some(c) = a.capturer.lock().as_ref() {
                    c.request_keyframe();
                }
            }
        }
    }
}

fn send_ctrl(s: &mut TcpStream, tx: &mut StreamCipher, c: &Ctrl) -> Result<()> {
    write_frame(s, &tx.seal(&encode_ctrl(c)))
}

fn handle_client(shared: &Arc<Shared>, mut s: TcpStream, peer: SocketAddr) -> Result<()> {
    s.set_nodelay(true)?;
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    let code = shared.code.lock().clone();
    let me = Hello {
        hop: PROTOCOL_VERSION,
        id: shared.id.clone(),
        name: shared.name.clone(),
        os: crate::proto::os_name().into(),
        error: None,
    };
    let locked = shared.failures.lock().1.filter(|until| Instant::now() < *until);
    if let Some(until) = locked {
        let secs = (until - Instant::now()).as_secs() + 1;
        let _ = host_refuse(&mut s, &me, &format!("Too many wrong access codes. Try again in {secs} s."));
        bail!("refusing {peer}: locked after wrong access codes");
    }
    let (client, keys) = host_handshake(&mut s, &code, &me)?;
    let mut tx = StreamCipher::new(&keys.s2c);
    let mut rx = StreamCipher::new(&keys.c2s);
    let native = platform::native_display_size();
    send_ctrl(&mut s, &mut tx, &Ctrl::Welcome { native_width: native.0, native_height: native.1 })?;

    // A client with the wrong code can't read our Welcome and hangs up, so a dropped connection
    // here counts as a failed attempt too (otherwise guessing would never be rate limited).
    let start = match read_frame(&mut s).ok().and_then(|f| rx.open(&f).ok()) {
        Some(b) => b,
        None => {
            {
                let mut f = shared.failures.lock();
                f.0 += 1;
                if f.0 >= 5 {
                    f.0 = 0;
                    f.1 = Some(Instant::now() + Duration::from_secs(60));
                }
            }
            // Slow down guessing.
            std::thread::sleep(Duration::from_secs(1));
            bail!("{} ({peer}) used the wrong access code", client.name);
        }
    };
    *shared.failures.lock() = (0, None);
    let Message::Ctrl(Ctrl::Start { max_width, max_height, fps, bitrate_kbps, audio, fec_percent, clipboard }) =
        decode_message(&start)?
    else {
        bail!("expected start message");
    };
    log::info!("{} ({peer}) connected: max {max_width}x{max_height} @{fps} {bitrate_kbps}kbps", client.name);

    let active = Arc::new(Active {
        id: shared.next_id.fetch_add(1, Ordering::Relaxed),
        punch: PacketCipher::new(&keys.punch),
        client_addr: Mutex::new(None),
        tcp: s.try_clone()?,
        capturer: Mutex::new(None),
    });
    if let Some(old) = shared.active.lock().replace(active.clone()) {
        log::info!("replacing previous session");
        old.kick();
    }

    let writer = Arc::new(Mutex::new((s.try_clone()?, tx)));
    let result = run_session(shared, &active, &mut s, &writer, &mut rx, &client, native, &keys.media, StartParams {
        max: (max_width, max_height),
        fps,
        bitrate_kbps,
        audio,
        fec_percent,
        clipboard,
    });

    // Tear down: stop capture before releasing the session slot.
    active.capturer.lock().take();
    {
        let mut a = shared.active.lock();
        if a.as_ref().map(|x| x.id) == Some(active.id) {
            *a = None;
            let mut st = shared.status.lock();
            st.session = None;
            st.last_event = Some(format!("{} disconnected", client.name));
            platform::prevent_sleep(false);
        }
    }
    log::info!("{} disconnected", client.name);
    result
}

struct StartParams {
    max: (u32, u32),
    fps: u32,
    bitrate_kbps: u32,
    audio: bool,
    fec_percent: u32,
    clipboard: bool,
}

type Writer = Arc<Mutex<(TcpStream, StreamCipher)>>;

fn send_shared(w: &Writer, c: &Ctrl) -> Result<()> {
    let mut g = w.lock();
    let (s, tx) = &mut *g;
    send_ctrl(s, tx, c)
}

#[allow(clippy::too_many_arguments)]
fn run_session(
    shared: &Arc<Shared>,
    active: &Arc<Active>,
    s: &mut TcpStream,
    writer: &Writer,
    rx: &mut StreamCipher,
    client: &Hello,
    native: (u32, u32),
    media_key: &[u8; 32],
    p: StartParams,
) -> Result<()> {
    let (width, height) = platform::stream_size(native, p.max);
    let cfg = CaptureConfig {
        width,
        height,
        fps: p.fps.clamp(10, 240),
        bitrate_kbps: p.bitrate_kbps.clamp(1000, 500_000),
        audio: p.audio,
    };
    let sent_bytes = Arc::new(AtomicU64::new(0));
    let sink = Arc::new(HostSink {
        sock: shared.media.clone(),
        active: active.clone(),
        cipher: PacketCipher::new(media_key),
        seq: AtomicU64::new(0),
        video: Mutex::new((Packetizer::new(p.fec_percent.min(100)), Vec::with_capacity(1500))),
        audio: Mutex::new(AudioState::default()),
        bytes: sent_bytes.clone(),
    });
    let capturer = match platform::start_capture(&cfg, sink) {
        Ok(c) => c,
        Err(e) => {
            let _ = send_shared(writer, &Ctrl::Error { message: format!("{e:#}") });
            shared.status.lock().error = Some(format!("{e:#}"));
            return Err(e);
        }
    };
    let encoder = capturer.encoder_name();
    *active.capturer.lock() = Some(capturer);
    send_shared(writer, &Ctrl::StreamInfo { width, height, fps: cfg.fps, encoder: encoder.clone() })?;
    log::info!("streaming {width}x{height}@{} with {encoder} to {}", cfg.fps, client.name);
    {
        let mut st = shared.status.lock();
        st.error = None;
        st.session = Some(SessionInfo {
            client: client.name.clone(),
            width,
            height,
            fps: cfg.fps,
            encoder,
            sent_bytes,
            since: Instant::now(),
        });
        st.last_event = Some(format!("{} connected", client.name));
    }
    platform::prevent_sleep(true);

    let mut injector = platform::new_injector()?;
    let clip_stop = Arc::new(AtomicBool::new(false));
    let clip = p.clipboard.then(|| {
        let c = ClipState::new();
        let w = writer.clone();
        c.watch(clip_stop.clone(), move |text| {
            let _ = send_shared(&w, &Ctrl::Clipboard { text });
        });
        c
    });
    // The client pings every second, so a long silence means it's gone.
    s.set_read_timeout(Some(Duration::from_secs(15)))?;
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        let frame = match read_frame(s) {
            Ok(f) => f,
            Err(_) => break,
        };
        match decode_message(&rx.open(&frame)?)? {
            Message::Input(i) => injector.input(i),
            Message::Ctrl(Ctrl::RequestKeyframe) => {
                if let Some(c) = active.capturer.lock().as_ref() {
                    c.request_keyframe();
                }
            }
            Message::Ctrl(Ctrl::SetBitrate { kbps }) => {
                if let Some(c) = active.capturer.lock().as_ref() {
                    c.set_bitrate(kbps.clamp(1000, 500_000));
                }
            }
            Message::Ctrl(Ctrl::Ping { ts }) => send_shared(writer, &Ctrl::Pong { ts })?,
            Message::Ctrl(Ctrl::Clipboard { text }) => {
                if let Some(c) = &clip {
                    c.apply(text);
                }
            }
            Message::Ctrl(Ctrl::Bye) => break,
            Message::Ctrl(_) => {}
        }
    }
    clip_stop.store(true, Ordering::SeqCst);
    injector.input(Input::ReleaseAll);
    Ok(())
}

#[derive(Default)]
struct AudioState {
    pending: Vec<i16>,
    payload: Vec<u8>,
    packet: Vec<u8>,
}

struct HostSink {
    sock: Arc<UdpSocket>,
    active: Arc<Active>,
    cipher: PacketCipher,
    seq: AtomicU64,
    video: Mutex<(Packetizer, Vec<u8>)>,
    audio: Mutex<AudioState>,
    bytes: Arc<AtomicU64>,
}

impl HostSink {
    fn send(&self, kind: u8, payload: &[u8], out: &mut Vec<u8>, addr: SocketAddr) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        self.cipher.seal_into(kind, seq, payload, out);
        for _ in 0..50 {
            match self.sock.send_to(out, addr) {
                Ok(_) => break,
                // The interface queue is full (big keyframe burst): wait a moment and retry.
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || matches!(e.raw_os_error(), Some(55 | 10055)) => {
                    std::thread::sleep(Duration::from_micros(200))
                }
                Err(_) => break,
            }
        }
        self.bytes.fetch_add(out.len() as u64, Ordering::Relaxed);
    }
}

impl FrameSink for HostSink {
    fn video(&self, data: &[u8], keyframe: bool, capture_us: u64) {
        let Some(addr) = *self.active.client_addr.lock() else { return };
        let latency = platform::now_us().saturating_sub(capture_us).min(u32::MAX as u64) as u32;
        let mut g = self.video.lock();
        let (pk, out) = &mut *g;
        pk.packetize(data, keyframe, latency, |payload| self.send(KIND_VIDEO, payload, out, addr));
    }

    fn audio(&self, pcm: &[i16]) {
        let Some(addr) = *self.active.client_addr.lock() else { return };
        let mut g = self.audio.lock();
        let st = &mut *g;
        st.pending.extend_from_slice(pcm);
        let chunk = AUDIO_FRAMES_PER_PACKET * 2;
        let mut off = 0;
        while st.pending.len() - off >= chunk {
            encode_audio(&st.pending[off..off + chunk], &mut st.payload);
            self.send(KIND_AUDIO, &st.payload, &mut st.packet, addr);
            off += chunk;
        }
        st.pending.drain(..off);
        // Never let audio queue up: drop anything older than ~50 ms.
        if st.pending.len() > 48 * 50 * 2 {
            st.pending.clear();
        }
    }
}
