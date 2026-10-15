//! The stream view: shows the remote screen and forwards keyboard and mouse. Used by the main
//! Hop window (which switches between home and stream) and by `hop connect`.
//!
//! Local shortcuts (Ctrl+Alt+Shift, i.e. Ctrl+Option+Shift on a Mac):
//!   Q  disconnect      F  toggle fullscreen      M  toggle mouse lock (games)      S  toggle stats

use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::PhysicalKey;
use winit::monitor::MonitorHandle;
use winit::window::{CursorGrabMode, CustomCursor, Fullscreen, Window, WindowId};

use crate::client::{self, ClientShared, ConnectError, ConnectOptions, Connection, Event};
use crate::keymap;
use crate::proto::{Hello, Input};
use crate::render::{Overlay, Renderer};

pub enum UserEvent {
    Frame,
    Cursor,
    /// A key captured by the system-wide keyboard grab.
    Key { hid: u16, down: bool, repeat: bool },
    /// The session `gen` ended (empty reason: closed on purpose).
    Closed { generation: u64, reason: String },
    Reconnected { generation: u64, conn: Box<Connection> },
    GaveUp { generation: u64, reason: String },
    /// A connection started from the home screen finished (Err: wrong code?, message).
    Connected { generation: u64, result: Result<Box<Connection>, (bool, String)> },
}

#[derive(Clone)]
pub struct StreamSettings {
    pub connect: ConnectOptions,
    pub fullscreen: bool,
    /// Name of the display to go fullscreen on ("" = the one the window is on).
    pub fullscreen_display: String,
    pub vsync: bool,
    pub swap_ctrl_cmd: bool,
    pub max_height_pref: u32,
    /// Watch only: never send keyboard or mouse input.
    pub view_only: bool,
    /// Send system shortcuts (Cmd+Tab, Win key, Alt+Tab...) to the remote computer.
    pub grab_keyboard: bool,
}

impl StreamSettings {
    /// Caps the requested stream size by the screen it will be shown on and the user's limit.
    pub fn fit_to(&mut self, screen: (u32, u32)) {
        let c = &mut self.connect;
        c.max_width = screen.0;
        c.max_height = screen.1;
        if self.max_height_pref > 0 && self.max_height_pref < c.max_height {
            c.max_width = c.max_width * self.max_height_pref / c.max_height;
            c.max_height = self.max_height_pref;
        }
    }
}

/// How a stream ended, for whoever shows it.
pub enum StreamEnd {
    /// The user disconnected, or the host ended the session cleanly.
    Closed,
    Failed(String),
    WrongCode,
}

/// Process exit codes for `hop connect`.
pub const EXIT_OK: i32 = 0;
pub const EXIT_ERROR: i32 = 1;
pub const EXIT_WRONG_CODE: i32 = 2;

const RECONNECT_FOR: Duration = Duration::from_secs(30);

pub fn shortcut_prefix() -> &'static str {
    if cfg!(target_os = "macos") { "Ctrl+Option+Shift" } else { "Ctrl+Alt+Shift" }
}

/// The display with this name, or the one the window is on.
pub fn find_display(el: &ActiveEventLoop, window: Option<&Window>, name: &str) -> Option<MonitorHandle> {
    if !name.is_empty()
        && let Some(m) = el.available_monitors().find(|m| m.name().as_deref() == Some(name))
    {
        return Some(m);
    }
    window.and_then(|w| w.current_monitor()).or_else(|| el.primary_monitor()).or_else(|| el.available_monitors().next())
}

/// Window size (logical) that shows a `vw`x`vh` stream as large as possible on `monitor`.
pub fn fitted_size(monitor: Option<&MonitorHandle>, vw: u32, vh: u32) -> LogicalSize<f64> {
    let (screen, scale) = monitor.map(|m| (m.size(), m.scale_factor())).unwrap_or((winit::dpi::PhysicalSize::new(1920, 1080), 1.0));
    let (sw, sh) = (screen.width as f64 / scale * 0.9, screen.height as f64 / scale * 0.9);
    let (lw, lh) = (vw as f64 / scale, vh as f64 / scale);
    let s = (sw / lw).min(sh / lh).min(1.0);
    LogicalSize::new(lw * s, lh * s)
}

/// One connected session being shown in a window.
pub struct StreamView {
    pub settings: StreamSettings,
    pub host: Hello,
    generation: u64,
    proxy: EventLoopProxy<UserEvent>,
    session: Option<Arc<ClientShared>>,
    encoder: String,
    focused: bool,
    /// Physical keys held on this computer (before any Ctrl/Cmd swap).
    local_down: std::collections::HashSet<u16>,
    /// Keys swallowed as local shortcuts, so their release isn't sent either.
    hotkey_keys: std::collections::HashSet<u16>,
    pub key_grab: bool,
    mouse_locked: bool,
    /// The host's pointer is showing (apps like video players and games hide it).
    remote_cursor_visible: bool,
    /// Only trust "hidden" once the host has reported a visible pointer: Windows reports it hidden
    /// on PCs with no mouse plugged in, and the pointer must never just vanish.
    remote_cursor_seen: bool,
    /// Pointer images from the host, by content hash.
    cursors: std::collections::HashMap<u64, CustomCursor>,
    show_stats: bool,
    toast: Option<(Vec<String>, Instant)>,
    reconnecting: Option<Instant>,
    got_frame: bool,
    last_abs: Option<(u16, u16)>,
    rel_accum: (f64, f64),
    wheel_accum: (f64, f64),
    pressed: std::collections::HashSet<u16>,
    /// Needs a repaint.
    pub dirty: bool,
    end: Option<StreamEnd>,
}

impl StreamView {
    /// Starts showing `conn` in `window`.
    pub fn start(
        settings: StreamSettings,
        conn: Connection,
        generation: u64,
        window: &Window,
        proxy: EventLoopProxy<UserEvent>,
        key_grab: bool,
    ) -> Result<StreamView, String> {
        let mut v = StreamView {
            settings,
            host: conn.host.clone(),
            generation,
            proxy,
            session: None,
            encoder: String::new(),
            focused: true,
            local_down: Default::default(),
            hotkey_keys: Default::default(),
            key_grab,
            mouse_locked: false,
            remote_cursor_visible: true,
            remote_cursor_seen: false,
            cursors: Default::default(),
            show_stats: false,
            toast: None,
            reconnecting: None,
            got_frame: false,
            last_abs: None,
            rel_accum: (0.0, 0.0),
            wheel_accum: (0.0, 0.0),
            pressed: Default::default(),
            dirty: true,
            end: None,
        };
        v.attach(window, conn)?;
        let p = shortcut_prefix();
        let mut lines = vec![
            format!("Connected to {}", v.host.name),
            format!("{p}+Q disconnect  ·  {p}+F fullscreen  ·  {p}+M lock mouse  ·  {p}+S stats"),
        ];
        if v.settings.grab_keyboard && !key_grab && cfg!(target_os = "macos") {
            lines.push("Cmd+Tab stays on this Mac: give Hop Accessibility permission to send it across.".into());
        }
        v.toast(lines, 7);
        Ok(v)
    }

    /// Starts streaming on a fresh connection (first connect or after a reconnect).
    fn attach(&mut self, window: &Window, conn: Connection) -> Result<(), String> {
        self.host = conn.host.clone();
        self.encoder = conn.info.encoder.clone();
        let proxy = self.proxy.clone();
        let generation = self.generation;
        let s = client::start(conn, move |e| {
            let _ = proxy.send_event(match e {
                Event::Frame => UserEvent::Frame,
                Event::Cursor => UserEvent::Cursor,
                Event::Closed(reason) => UserEvent::Closed { generation, reason },
            });
        })
        .map_err(|e| format!("{e:#}"))?;
        // Draw the pointer here, without lag; the host sends its shape.
        s.send_ctrl(&crate::proto::Ctrl::CursorMode { local: !self.mouse_locked });
        self.session = Some(s);
        self.reconnecting = None;
        self.update_cursor_visibility(window);
        Ok(())
    }

    /// How the stream ended, once it has.
    pub fn take_end(&mut self) -> Option<StreamEnd> {
        self.end.take()
    }

    /// Ends the session and puts the window's pointer back to normal.
    pub fn close(&mut self, window: &Window) {
        self.release_all();
        if let Some(s) = self.session.take() {
            s.close();
        }
        // Make late events from this session (and its reconnect thread) be ignored.
        self.generation = u64::MAX;
        if self.mouse_locked {
            let _ = window.set_cursor_grab(CursorGrabMode::None);
            self.mouse_locked = false;
        }
        window.set_cursor(winit::window::CursorIcon::Default);
        window.set_cursor_visible(true);
    }

    fn toast(&mut self, lines: Vec<String>, secs: u64) {
        self.toast = Some((lines, Instant::now() + Duration::from_secs(secs)));
        self.dirty = true;
    }

    fn start_reconnect(&mut self, reason: String) {
        log::info!("{reason}; reconnecting");
        self.session = None;
        self.pressed.clear();
        self.local_down.clear();
        self.hotkey_keys.clear();
        self.reconnecting = Some(Instant::now());
        let opts = self.settings.connect.clone();
        let proxy = self.proxy.clone();
        let generation = self.generation;
        std::thread::spawn(move || {
            let deadline = Instant::now() + RECONNECT_FOR;
            loop {
                std::thread::sleep(Duration::from_millis(700));
                match client::connect(&opts) {
                    Ok(conn) => {
                        let _ = proxy.send_event(UserEvent::Reconnected { generation, conn: Box::new(conn) });
                        return;
                    }
                    Err(ConnectError::WrongCode) => {
                        let reason = "The access code changed".into();
                        let _ = proxy.send_event(UserEvent::GaveUp { generation, reason });
                        return;
                    }
                    Err(e) if Instant::now() > deadline => {
                        let _ = proxy.send_event(UserEvent::GaveUp { generation, reason: format!("{reason} ({e})") });
                        return;
                    }
                    Err(_) => {}
                }
            }
        });
        self.dirty = true;
    }

    fn send(&self, i: Input) {
        if self.settings.view_only {
            return;
        }
        if let Some(s) = &self.session {
            s.send_input(i);
        }
    }

    fn release_all(&mut self) {
        self.pressed.clear();
        self.local_down.clear();
        self.hotkey_keys.clear();
        self.send(Input::ReleaseAll);
    }

    fn set_mouse_lock(&mut self, window: &Window, on: bool) {
        if on {
            let ok = window
                .set_cursor_grab(CursorGrabMode::Locked)
                .or_else(|_| window.set_cursor_grab(CursorGrabMode::Confined))
                .is_ok();
            self.mouse_locked = ok;
        } else {
            let _ = window.set_cursor_grab(CursorGrabMode::None);
            self.mouse_locked = false;
        }
        self.rel_accum = (0.0, 0.0);
        // While locked (games), the host draws its pointer into the video instead.
        if let Some(s) = &self.session {
            s.send_ctrl(&crate::proto::Ctrl::CursorMode { local: !self.mouse_locked });
        }
        self.update_cursor_visibility(window);
        if self.mouse_locked {
            self.toast(vec!["Mouse locked to this window".into(), format!("{}+M to release", shortcut_prefix())], 3);
        } else if on {
            self.toast(vec!["Couldn't lock the mouse".into()], 3);
        } else {
            self.toast(vec!["Mouse released".into()], 2);
        }
    }

    fn update_cursor_visibility(&self, window: &Window) {
        let remote_hidden = self.remote_cursor_seen && !self.remote_cursor_visible;
        window.set_cursor_visible(!self.mouse_locked && !remote_hidden);
    }

    /// Mirrors the host's pointer shape and visibility on our own cursor.
    fn apply_cursor(&mut self, el: &ActiveEventLoop, window: &Window) {
        let Some(msg) = self.session.as_ref().and_then(|s| s.cursor.lock().take()) else { return };
        if let Some(shape) = msg.shape {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            (&shape.rgba, shape.w, shape.h, shape.hot_x, shape.hot_y).hash(&mut h);
            let key = h.finish();
            if !self.cursors.contains_key(&key) {
                match CustomCursor::from_rgba(shape.rgba, shape.w, shape.h, shape.hot_x, shape.hot_y) {
                    Ok(src) => {
                        if self.cursors.len() > 64 {
                            self.cursors.clear();
                        }
                        self.cursors.insert(key, el.create_custom_cursor(src));
                    }
                    Err(e) => log::warn!("bad pointer image: {e}"),
                }
            }
            if let Some(c) = self.cursors.get(&key) {
                window.set_cursor(c.clone());
            }
        }
        self.remote_cursor_seen |= msg.visible;
        if msg.visible != self.remote_cursor_visible {
            self.remote_cursor_visible = msg.visible;
            self.update_cursor_visibility(window);
        }
    }

    fn toggle_fullscreen(&self, window: &Window) {
        let fs = if window.fullscreen().is_some() { None } else { Some(Fullscreen::Borderless(None)) };
        window.set_fullscreen(fs);
    }

    fn hotkey(&mut self, window: &Window, hid: u16) -> bool {
        let held = |a: u16, b: u16| self.local_down.contains(&a) || self.local_down.contains(&b);
        if !(held(0xE0, 0xE4) && held(0xE2, 0xE6) && held(0xE1, 0xE5)) {
            return false;
        }
        match hid {
            0x14 => {
                // Q: disconnect
                self.release_all();
                self.end = Some(StreamEnd::Closed);
            }
            0x09 => self.toggle_fullscreen(window),                    // F
            0x10 => self.set_mouse_lock(window, !self.mouse_locked), // M
            0x16 => {
                // S
                self.show_stats = !self.show_stats;
                self.dirty = true;
            }
            _ => return false,
        }
        true
    }

    fn on_key(&mut self, window: &Window, hid: u16, down: bool, repeat: bool) {
        if down {
            self.local_down.insert(hid);
        } else {
            self.local_down.remove(&hid);
        }
        if down && !repeat && self.hotkey(window, hid) {
            self.hotkey_keys.insert(hid);
            return;
        }
        if self.hotkey_keys.contains(&hid) {
            if !down {
                self.hotkey_keys.remove(&hid);
            }
            return;
        }
        let hid = self.map_key(hid);
        if down {
            self.pressed.insert(hid);
        } else if !self.pressed.remove(&hid) {
            return; // release of a key we never sent
        }
        self.send(Input::Key { hid, down, repeat });
    }

    fn map_key(&self, hid: u16) -> u16 {
        let cross_os = self.host.os != crate::proto::os_name();
        if self.settings.swap_ctrl_cmd && cross_os { keymap::swap_ctrl_meta(hid) } else { hid }
    }

    fn mouse_abs(&mut self, renderer: &Renderer, p: PhysicalPosition<f64>) {
        let (x, y, w, h) = renderer.video_rect();
        let nx = ((p.x as f32 - x) / w).clamp(0.0, 1.0);
        let ny = ((p.y as f32 - y) / h).clamp(0.0, 1.0);
        let pos = ((nx * 65535.0).round() as u16, (ny * 65535.0).round() as u16);
        if self.last_abs != Some(pos) {
            self.last_abs = Some(pos);
            self.send(Input::MouseAbs { x: pos.0, y: pos.1 });
        }
    }

    pub fn overlay(&mut self) -> Overlay {
        let mut ov = Overlay::default();
        if let Some((lines, until)) = &self.toast {
            if Instant::now() < *until {
                ov.toast = Some(lines.clone());
            } else {
                self.toast = None;
            }
        }
        if let Some(since) = self.reconnecting {
            let left = RECONNECT_FOR.saturating_sub(since.elapsed()).as_secs();
            ov.banner = Some(("Connection lost".into(), format!("Reconnecting to {}…  ({left}s)", self.host.name)));
        } else if !self.got_frame
            && let Some(s) = &self.session
        {
            ov.banner = Some(("Waiting for video…".into(), s.notice.lock().clone()));
        }
        if self.show_stats
            && let Some(s) = &self.session
        {
            let st = s.stats.lock().clone();
            let net = st.rtt_ms / 2.0;
            ov.stats = Some(vec![
                format!("{}×{}  {:.0} fps  {:.1} Mbps", st.width, st.height, st.fps, st.mbps),
                format!("host {:.1} ms + network {:.1} ms + decode {:.1} ms", st.host_ms, net, st.decode_ms),
                format!("≈ {:.0} ms before display", st.host_ms + net + st.decode_ms),
                format!("round trip {:.1} ms · lost {} · repaired {}", st.rtt_ms, st.lost, st.recovered),
                format!("{} → {}", self.encoder, st.decoder),
            ]);
        }
        ov
    }

    /// Overlay content that changes on its own (countdowns, stats, toasts) needs periodic repaints.
    pub fn overlay_is_live(&self) -> bool {
        self.toast.is_some() || self.reconnecting.is_some() || self.show_stats || (!self.got_frame && self.session.is_some())
    }

    /// Handles a stream event. Returns false if it belongs to an older session.
    pub fn user_event(&mut self, el: &ActiveEventLoop, window: &Window, renderer: &mut Renderer, ev: UserEvent) {
        match ev {
            UserEvent::Frame => {
                let Some(s) = &self.session else { return };
                let Some(f) = s.frame.lock().take() else { return };
                renderer.upload(&f);
                s.recycle(f);
                self.got_frame = true;
                self.dirty = true;
            }
            UserEvent::Cursor => self.apply_cursor(el, window),
            UserEvent::Key { hid, down, repeat } => {
                if self.focused {
                    self.on_key(window, hid, down, repeat);
                }
            }
            UserEvent::Closed { generation, reason } if generation == self.generation => {
                // The host deliberately ended things (or we did): don't fight it.
                if reason.is_empty() {
                    self.end = Some(StreamEnd::Closed);
                } else if reason.contains("ended the session") || reason.contains("permission") {
                    self.end = Some(StreamEnd::Failed(reason));
                } else {
                    self.start_reconnect(reason);
                }
            }
            UserEvent::Reconnected { generation, conn } if generation == self.generation => {
                match self.attach(window, *conn) {
                    Ok(()) => {
                        let name = self.host.name.clone();
                        self.toast(vec![format!("Reconnected to {name}")], 3);
                    }
                    Err(e) => self.end = Some(StreamEnd::Failed(e)),
                }
            }
            UserEvent::GaveUp { generation, reason } if generation == self.generation => {
                self.end = Some(if reason.contains("access code") {
                    StreamEnd::WrongCode
                } else {
                    StreamEnd::Failed(reason)
                });
            }
            _ => {}
        }
    }

    pub fn window_event(&mut self, window: &Window, renderer: &mut Renderer, event: &WindowEvent) {
        match event {
            WindowEvent::Focused(f) => {
                self.focused = *f;
                if !f {
                    self.release_all();
                    if self.mouse_locked {
                        self.set_mouse_lock(window, false);
                    }
                }
            }
            WindowEvent::KeyboardInput { event, is_synthetic, .. } => {
                if *is_synthetic {
                    return;
                }
                let PhysicalKey::Code(code) = event.physical_key else { return };
                let Some(hid) = keymap::keycode_to_hid(code) else { return };
                self.on_key(window, hid, event.state == ElementState::Pressed, event.repeat);
            }
            WindowEvent::CursorMoved { position, .. } => {
                if !self.mouse_locked {
                    self.mouse_abs(renderer, *position);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let b = match button {
                    MouseButton::Left => 1,
                    MouseButton::Right => 2,
                    MouseButton::Middle => 3,
                    MouseButton::Back => 4,
                    MouseButton::Forward => 5,
                    MouseButton::Other(_) => return,
                };
                self.send(Input::Button { button: b, down: *state == ElementState::Pressed });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy, precise) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (*x as f64 * 120.0, *y as f64 * 120.0, false),
                    MouseScrollDelta::PixelDelta(p) => {
                        let s = window.scale_factor();
                        // 50 logical pixels = one wheel notch (120 units).
                        (p.x / s * 120.0 / 50.0, p.y / s * 120.0 / 50.0, true)
                    }
                };
                self.wheel_accum.0 += dx;
                self.wheel_accum.1 += dy;
                let (ix, iy) = (self.wheel_accum.0.trunc(), self.wheel_accum.1.trunc());
                if ix != 0.0 || iy != 0.0 {
                    self.wheel_accum.0 -= ix;
                    self.wheel_accum.1 -= iy;
                    self.send(Input::Wheel {
                        dx: ix.clamp(-32000.0, 32000.0) as i16,
                        dy: iy.clamp(-32000.0, 32000.0) as i16,
                        precise,
                    });
                }
            }
            _ => {}
        }
    }

    pub fn device_event(&mut self, event: &DeviceEvent) {
        if let DeviceEvent::MouseMotion { delta } = event
            && self.mouse_locked
            && self.focused
        {
            self.rel_accum.0 += delta.0;
            self.rel_accum.1 += delta.1;
            let (ix, iy) = (self.rel_accum.0.trunc(), self.rel_accum.1.trunc());
            if ix != 0.0 || iy != 0.0 {
                self.rel_accum.0 -= ix;
                self.rel_accum.1 -= iy;
                self.send(Input::MouseRel { dx: ix as i16, dy: iy as i16 });
            }
        }
    }
}

/// Builds the event loop. On a Mac, without the default menu, so Cmd+Q and friends reach the
/// remote computer while streaming.
pub fn event_loop() -> Result<EventLoop<UserEvent>, String> {
    #[allow(unused_mut)]
    let mut builder = EventLoop::<UserEvent>::with_user_event();
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::EventLoopBuilderExtMacOS;
        builder.with_default_menu(false);
    }
    builder.build().map_err(|e| e.to_string())
}

/// Starts capturing system shortcuts for the stream (once per process); off until enabled.
pub fn start_key_grab(proxy: &EventLoopProxy<UserEvent>) -> bool {
    let proxy = proxy.clone();
    let ok = crate::platform::start_key_grab(Box::new(move |hid, down, repeat| {
        let _ = proxy.send_event(UserEvent::Key { hid, down, repeat });
    }));
    log::info!("system shortcut capture: {}", if ok { "available" } else { "unavailable" });
    ok
}

// ---------------------------------------------------------------------------------------------
// `hop connect`: a window that only shows one stream.

struct ViewerApp {
    settings: StreamSettings,
    proxy: EventLoopProxy<UserEvent>,
    view: Option<StreamView>,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    exit_code: i32,
    exit_message: Option<String>,
    last_paint: Instant,
}

/// Tells a parent process (which reads our stderr) what state we're in.
fn report(state: &str, host: Option<&Hello>) {
    let mut v = serde_json::json!({ "state": state });
    if let Some(h) = host {
        v["id"] = h.id.clone().into();
        v["name"] = h.name.clone().into();
        v["os"] = h.os.clone().into();
    }
    eprintln!("HOP_STATUS {v}");
}

pub fn run(settings: StreamSettings) -> i32 {
    let event_loop = match event_loop() {
        Ok(e) => e,
        Err(e) => {
            log::error!("event loop: {e}");
            return EXIT_ERROR;
        }
    };
    let mut app = ViewerApp {
        settings,
        proxy: event_loop.create_proxy(),
        view: None,
        window: None,
        renderer: None,
        exit_code: EXIT_OK,
        exit_message: None,
        last_paint: Instant::now(),
    };
    if let Err(e) = event_loop.run_app(&mut app) {
        log::error!("event loop: {e}");
    }
    if let (Some(v), Some(w)) = (&mut app.view, &app.window) {
        v.close(w);
    }
    if let Some(m) = &app.exit_message {
        log::info!("viewer closed: {m}");
        eprintln!("{m}");
    }
    app.exit_code
}

impl ViewerApp {
    fn fail(&mut self, el: &ActiveEventLoop, code: i32, msg: String) {
        self.exit_code = code;
        self.exit_message = Some(msg);
        el.exit();
    }

    fn connect(&mut self, el: &ActiveEventLoop) {
        let monitor = find_display(el, None, &self.settings.fullscreen_display);
        let screen = monitor.as_ref().map(|m| m.size()).map(|s| (s.width, s.height)).unwrap_or((1920, 1080));
        self.settings.fit_to(screen);
        let conn = match client::connect(&self.settings.connect) {
            Ok(c) => c,
            Err(ConnectError::WrongCode) => return self.fail(el, EXIT_WRONG_CODE, "Wrong access code".into()),
            Err(e) => return self.fail(el, EXIT_ERROR, e.to_string()),
        };
        let mut attrs = Window::default_attributes()
            .with_title(format!("Hop — {}", conn.host.name))
            .with_inner_size(fitted_size(monitor.as_ref(), conn.info.width, conn.info.height));
        if self.settings.fullscreen {
            attrs = attrs.with_fullscreen(Some(Fullscreen::Borderless(monitor.clone())));
        }
        let window = match el.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => return self.fail(el, EXIT_ERROR, format!("can't open window: {e}")),
        };
        let renderer = match Renderer::new(window.clone(), self.settings.vsync) {
            Ok(r) => r,
            Err(e) => return self.fail(el, EXIT_ERROR, format!("graphics init failed: {e:#}")),
        };
        let key_grab = self.settings.grab_keyboard && start_key_grab(&self.proxy);
        crate::platform::set_key_grab(key_grab);
        window.focus_window();
        #[cfg(target_os = "macos")]
        crate::platform::activate_self();
        report("connected", Some(&conn.host));
        match StreamView::start(self.settings.clone(), conn, 1, &window, self.proxy.clone(), key_grab) {
            Ok(v) => self.view = Some(v),
            Err(e) => return self.fail(el, EXIT_ERROR, e),
        }
        self.window = Some(window);
        self.renderer = Some(renderer);
    }

    fn paint(&mut self) {
        let (Some(v), Some(w), Some(r)) = (&mut self.view, &self.window, &mut self.renderer) else { return };
        let ov = v.overlay();
        w.pre_present_notify();
        r.render(&ov, w.scale_factor() as f32);
        v.dirty = false;
        self.last_paint = Instant::now();
    }

    fn after_event(&mut self, el: &ActiveEventLoop) {
        let Some(v) = &mut self.view else { return };
        match v.take_end() {
            Some(StreamEnd::Closed) => el.exit(),
            Some(StreamEnd::WrongCode) => self.fail(el, EXIT_WRONG_CODE, "Wrong access code".into()),
            Some(StreamEnd::Failed(m)) => self.fail(el, EXIT_ERROR, m),
            None => {
                if v.dirty {
                    self.paint();
                }
            }
        }
    }
}

impl ApplicationHandler<UserEvent> for ViewerApp {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_none() && self.exit_message.is_none() {
            self.connect(el);
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if self.view.as_ref().is_some_and(|v| v.overlay_is_live()) {
            if self.last_paint.elapsed() >= Duration::from_millis(250) {
                self.paint();
            }
            el.set_control_flow(ControlFlow::WaitUntil(self.last_paint + Duration::from_millis(250)));
        } else {
            el.set_control_flow(ControlFlow::Wait);
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, ev: UserEvent) {
        if let UserEvent::Reconnected { conn, .. } = &ev {
            report("connected", Some(&conn.host));
        }
        if let (Some(v), Some(w), Some(r)) = (&mut self.view, &self.window, &mut self.renderer) {
            v.user_event(el, w, r, ev);
        }
        self.after_event(el);
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match &event {
            WindowEvent::CloseRequested => {
                if let Some(v) = &mut self.view {
                    v.release_all();
                }
                return el.exit();
            }
            WindowEvent::Resized(s) => {
                if let Some(r) = &mut self.renderer {
                    r.resize(s.width, s.height);
                }
                self.paint();
            }
            WindowEvent::RedrawRequested | WindowEvent::ScaleFactorChanged { .. } => self.paint(),
            WindowEvent::Focused(f) => crate::platform::set_key_grab(*f && self.view.as_ref().is_some_and(|v| v.key_grab)),
            _ => {}
        }
        if let (Some(v), Some(w), Some(r)) = (&mut self.view, &self.window, &mut self.renderer) {
            v.window_event(w, r, &event);
        }
        self.after_event(el);
    }

    fn device_event(&mut self, _el: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        if let Some(v) = &mut self.view {
            v.device_event(&event);
        }
    }
}
