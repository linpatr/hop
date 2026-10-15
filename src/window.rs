//! The streaming window: shows the remote screen and forwards keyboard and mouse.
//!
//! Local shortcuts (Ctrl+Alt+Shift, i.e. Ctrl+Option+Shift on a Mac):
//!   Q  disconnect      F  toggle fullscreen      M  toggle mouse lock (games)      S  toggle stats
//!
//! Status lines starting with `HOP_STATUS ` on stderr tell the launcher what's happening.

use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::PhysicalKey;
use winit::window::{CursorGrabMode, CustomCursor, Fullscreen, Window, WindowId};

use crate::client::{self, ClientShared, ConnectError, ConnectOptions, Connection, Event};
use crate::keymap;
use crate::proto::Input;
use crate::render::{Overlay, Renderer};

pub enum UserEvent {
    Frame,
    Cursor,
    Closed(String),
    /// A key captured by the system-wide keyboard grab.
    Key { hid: u16, down: bool, repeat: bool },
    Reconnected(Box<Connection>),
    GaveUp(String),
}

pub struct StreamSettings {
    pub connect: ConnectOptions,
    pub fullscreen: bool,
    pub vsync: bool,
    pub swap_ctrl_cmd: bool,
    pub max_height_pref: u32,
    /// Watch only: never send keyboard or mouse input.
    pub view_only: bool,
    /// Send system shortcuts (Cmd+Tab, Win key, Alt+Tab...) to the remote computer.
    pub grab_keyboard: bool,
}

/// Process exit codes, read by the launcher.
pub const EXIT_OK: i32 = 0;
pub const EXIT_ERROR: i32 = 1;
pub const EXIT_WRONG_CODE: i32 = 2;

const RECONNECT_FOR: Duration = Duration::from_secs(30);

fn shortcut_prefix() -> &'static str {
    if cfg!(target_os = "macos") { "Ctrl+Option+Shift" } else { "Ctrl+Alt+Shift" }
}

/// Tells the launcher (which reads our stderr) what state we're in.
fn report(state: &str, host: Option<&crate::proto::Hello>) {
    let mut v = serde_json::json!({ "state": state });
    if let Some(h) = host {
        v["id"] = h.id.clone().into();
        v["name"] = h.name.clone().into();
        v["os"] = h.os.clone().into();
    }
    eprintln!("HOP_STATUS {v}");
}

struct App {
    settings: StreamSettings,
    proxy: EventLoopProxy<UserEvent>,
    session: Option<Arc<ClientShared>>,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    exit_code: i32,
    exit_message: Option<String>,
    host_name: String,
    host_os: String,
    encoder: String,
    focused: bool,
    /// Physical keys held on this computer (before any Ctrl/Cmd swap).
    local_down: std::collections::HashSet<u16>,
    /// Keys swallowed as local shortcuts, so their release isn't sent either.
    hotkey_keys: std::collections::HashSet<u16>,
    key_grab: bool,
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
    last_paint: Instant,
    last_abs: Option<(u16, u16)>,
    rel_accum: (f64, f64),
    wheel_accum: (f64, f64),
    pressed: std::collections::HashSet<u16>,
}

pub fn run(settings: StreamSettings) -> i32 {
    #[allow(unused_mut)]
    let mut builder = EventLoop::<UserEvent>::with_user_event();
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::EventLoopBuilderExtMacOS;
        // Without this, Cmd+Q would quit the viewer instead of reaching the remote computer.
        builder.with_default_menu(false);
    }
    let event_loop = match builder.build() {
        Ok(e) => e,
        Err(e) => {
            log::error!("event loop: {e}");
            return EXIT_ERROR;
        }
    };
    let mut app = App {
        settings,
        proxy: event_loop.create_proxy(),
        session: None,
        window: None,
        renderer: None,
        exit_code: EXIT_OK,
        exit_message: None,
        host_name: String::new(),
        host_os: String::new(),
        encoder: String::new(),
        focused: true,
        local_down: Default::default(),
        hotkey_keys: Default::default(),
        key_grab: false,
        mouse_locked: false,
        remote_cursor_visible: true,
        remote_cursor_seen: false,
        cursors: Default::default(),
        show_stats: false,
        toast: None,
        reconnecting: None,
        got_frame: false,
        last_paint: Instant::now(),
        last_abs: None,
        rel_accum: (0.0, 0.0),
        wheel_accum: (0.0, 0.0),
        pressed: Default::default(),
    };
    if let Err(e) = event_loop.run_app(&mut app) {
        log::error!("event loop: {e}");
    }
    if let Some(s) = &app.session {
        s.close();
    }
    if let Some(m) = &app.exit_message {
        log::info!("viewer closed: {m}");
        eprintln!("{m}");
    }
    app.exit_code
}

impl App {
    fn fail(&mut self, el: &ActiveEventLoop, code: i32, msg: String) {
        self.exit_code = code;
        self.exit_message = Some(msg);
        el.exit();
    }

    fn toast(&mut self, lines: Vec<String>, secs: u64) {
        self.toast = Some((lines, Instant::now() + Duration::from_secs(secs)));
        self.paint();
    }

    fn connect(&mut self, el: &ActiveEventLoop) {
        let monitor = el.primary_monitor().or_else(|| el.available_monitors().next());
        let screen = monitor.as_ref().map(|m| m.size()).map(|s| (s.width, s.height)).unwrap_or((1920, 1080));
        let c = &mut self.settings.connect;
        c.max_width = screen.0;
        c.max_height = screen.1;
        if self.settings.max_height_pref > 0 && self.settings.max_height_pref < c.max_height {
            c.max_width = c.max_width * self.settings.max_height_pref / c.max_height;
            c.max_height = self.settings.max_height_pref;
        }
        let conn = match client::connect(&self.settings.connect) {
            Ok(c) => c,
            Err(ConnectError::WrongCode) => return self.fail(el, EXIT_WRONG_CODE, "Wrong access code".into()),
            Err(e) => return self.fail(el, EXIT_ERROR, e.to_string()),
        };
        let (vw, vh) = (conn.info.width, conn.info.height);
        let mut attrs = Window::default_attributes().with_title(format!("Hop — {}", conn.host.name));
        // Open at the stream's size, shrunk to fit the screen.
        let scale = monitor.as_ref().map(|m| m.scale_factor()).unwrap_or(1.0);
        let (sw, sh) = (screen.0 as f64 / scale * 0.9, screen.1 as f64 / scale * 0.9);
        let (lw, lh) = (vw as f64 / scale, vh as f64 / scale);
        let s = (sw / lw).min(sh / lh).min(1.0);
        attrs = attrs.with_inner_size(LogicalSize::new(lw * s, lh * s));
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
        if self.settings.grab_keyboard {
            let proxy = self.proxy.clone();
            self.key_grab = crate::platform::start_key_grab(Box::new(move |hid, down, repeat| {
                let _ = proxy.send_event(UserEvent::Key { hid, down, repeat });
            }));
            log::info!("system shortcut capture: {}", if self.key_grab { "on" } else { "unavailable" });
        }
        window.focus_window();
        #[cfg(target_os = "macos")]
        crate::platform::activate_self();
        self.window = Some(window);
        self.renderer = Some(renderer);
        if !self.attach(el, conn) {
            return;
        }
        let p = shortcut_prefix();
        let mut lines = vec![
            format!("Connected to {}", self.host_name),
            format!("{p}+Q disconnect  ·  {p}+F fullscreen  ·  {p}+M lock mouse  ·  {p}+S stats"),
        ];
        if self.settings.grab_keyboard && !self.key_grab && cfg!(target_os = "macos") {
            lines.push("Cmd+Tab stays on this Mac: give Hop Accessibility permission to send it across.".into());
        }
        self.toast(lines, 7);
    }

    /// Starts streaming on a fresh connection (first connect or after a reconnect).
    fn attach(&mut self, el: &ActiveEventLoop, conn: Connection) -> bool {
        self.host_name = conn.host.name.clone();
        self.host_os = conn.host.os.clone();
        self.encoder = conn.info.encoder.clone();
        report("connected", Some(&conn.host));
        let proxy = self.proxy.clone();
        match client::start(conn, move |e| {
            let _ = proxy.send_event(match e {
                Event::Frame => UserEvent::Frame,
                Event::Cursor => UserEvent::Cursor,
                Event::Closed(r) => UserEvent::Closed(r),
            });
        }) {
            Ok(s) => {
                // Draw the pointer here, without lag; the host sends its shape.
                s.send_ctrl(&crate::proto::Ctrl::CursorMode { local: !self.mouse_locked });
                self.session = Some(s);
                self.reconnecting = None;
                self.update_cursor_visibility();
                true
            }
            Err(e) => {
                self.fail(el, EXIT_ERROR, format!("{e:#}"));
                false
            }
        }
    }

    fn start_reconnect(&mut self, reason: String) {
        log::info!("{reason}; reconnecting");
        report("reconnecting", None);
        self.session = None;
        self.pressed.clear();
        self.local_down.clear();
        self.hotkey_keys.clear();
        self.reconnecting = Some(Instant::now());
        let opts = self.settings.connect.clone();
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let deadline = Instant::now() + RECONNECT_FOR;
            loop {
                std::thread::sleep(Duration::from_millis(700));
                match client::connect(&opts) {
                    Ok(conn) => {
                        let _ = proxy.send_event(UserEvent::Reconnected(Box::new(conn)));
                        return;
                    }
                    Err(ConnectError::WrongCode) => {
                        let _ = proxy.send_event(UserEvent::GaveUp("The access code changed".into()));
                        return;
                    }
                    Err(e) if Instant::now() > deadline => {
                        let _ = proxy.send_event(UserEvent::GaveUp(format!("{reason} ({e})")));
                        return;
                    }
                    Err(_) => {}
                }
            }
        });
        self.paint();
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

    fn set_mouse_lock(&mut self, on: bool) {
        let Some(w) = &self.window else { return };
        if on {
            let ok = w
                .set_cursor_grab(CursorGrabMode::Locked)
                .or_else(|_| w.set_cursor_grab(CursorGrabMode::Confined))
                .is_ok();
            self.mouse_locked = ok;
        } else {
            let _ = w.set_cursor_grab(CursorGrabMode::None);
            self.mouse_locked = false;
        }
        self.rel_accum = (0.0, 0.0);
        // While locked (games), the host draws its pointer into the video instead.
        if let Some(s) = &self.session {
            s.send_ctrl(&crate::proto::Ctrl::CursorMode { local: !self.mouse_locked });
        }
        self.update_cursor_visibility();
        if self.mouse_locked {
            self.toast(vec!["Mouse locked to this window".into(), format!("{}+M to release", shortcut_prefix())], 3);
        } else if on {
            self.toast(vec!["Couldn't lock the mouse".into()], 3);
        } else {
            self.toast(vec!["Mouse released".into()], 2);
        }
    }

    fn update_cursor_visibility(&self) {
        if let Some(w) = &self.window {
            let remote_hidden = self.remote_cursor_seen && !self.remote_cursor_visible;
            w.set_cursor_visible(!self.mouse_locked && !remote_hidden);
        }
    }

    /// Mirrors the host's pointer shape and visibility on our own cursor.
    fn apply_cursor(&mut self, el: &ActiveEventLoop) {
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
            if let (Some(c), Some(w)) = (self.cursors.get(&key), &self.window) {
                w.set_cursor(c.clone());
            }
        }
        self.remote_cursor_seen |= msg.visible;
        if msg.visible != self.remote_cursor_visible {
            self.remote_cursor_visible = msg.visible;
            self.update_cursor_visibility();
        }
    }

    fn toggle_fullscreen(&self) {
        if let Some(w) = &self.window {
            let fs = if w.fullscreen().is_some() { None } else { Some(Fullscreen::Borderless(None)) };
            w.set_fullscreen(fs);
        }
    }

    fn hotkey(&mut self, el: &ActiveEventLoop, hid: u16) -> bool {
        let held = |a: u16, b: u16| self.local_down.contains(&a) || self.local_down.contains(&b);
        if !(held(0xE0, 0xE4) && held(0xE2, 0xE6) && held(0xE1, 0xE5)) {
            return false;
        }
        match hid {
            0x14 => {
                // Q: disconnect
                self.release_all();
                el.exit();
            }
            0x09 => self.toggle_fullscreen(),                  // F
            0x10 => self.set_mouse_lock(!self.mouse_locked), // M
            0x16 => {
                // S
                self.show_stats = !self.show_stats;
                self.paint();
            }
            _ => return false,
        }
        true
    }

    fn on_key(&mut self, el: &ActiveEventLoop, hid: u16, down: bool, repeat: bool) {
        if down {
            self.local_down.insert(hid);
        } else {
            self.local_down.remove(&hid);
        }
        if down && !repeat && self.hotkey(el, hid) {
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
        let cross_os = self.host_os != crate::proto::os_name();
        if self.settings.swap_ctrl_cmd && cross_os { keymap::swap_ctrl_meta(hid) } else { hid }
    }

    fn mouse_abs(&mut self, p: PhysicalPosition<f64>) {
        let Some(r) = &self.renderer else { return };
        let (x, y, w, h) = r.video_rect();
        let nx = ((p.x as f32 - x) / w).clamp(0.0, 1.0);
        let ny = ((p.y as f32 - y) / h).clamp(0.0, 1.0);
        let pos = ((nx * 65535.0).round() as u16, (ny * 65535.0).round() as u16);
        if self.last_abs != Some(pos) {
            self.last_abs = Some(pos);
            self.send(Input::MouseAbs { x: pos.0, y: pos.1 });
        }
    }

    fn overlay(&mut self) -> Overlay {
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
            ov.banner = Some(("Connection lost".into(), format!("Reconnecting to {}…  ({left}s)", self.host_name)));
        } else if !self.got_frame && self.session.is_some() {
            ov.banner = Some(("Waiting for video…".into(), String::new()));
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

    fn paint(&mut self) {
        let ov = self.overlay();
        let scale = self.window.as_ref().map(|w| w.scale_factor() as f32).unwrap_or(1.0);
        if let Some(w) = &self.window {
            w.pre_present_notify();
        }
        if let Some(r) = &mut self.renderer {
            r.render(&ov, scale);
        }
        self.last_paint = Instant::now();
    }

    /// Overlay content that changes on its own (countdowns, stats, toasts) needs periodic repaints.
    fn overlay_is_live(&self) -> bool {
        self.toast.is_some() || self.reconnecting.is_some() || self.show_stats || (!self.got_frame && self.session.is_some())
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_none() && self.exit_message.is_none() {
            self.connect(el);
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if self.overlay_is_live() {
            let next = self.last_paint + Duration::from_millis(250);
            if Instant::now() >= next {
                self.paint();
            }
            el.set_control_flow(ControlFlow::WaitUntil(self.last_paint + Duration::from_millis(250)));
        } else {
            el.set_control_flow(ControlFlow::Wait);
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, ev: UserEvent) {
        match ev {
            UserEvent::Frame => {
                let (Some(s), Some(r)) = (&self.session, &mut self.renderer) else { return };
                let Some(f) = s.frame.lock().take() else { return };
                r.upload(&f);
                s.recycle(f);
                self.got_frame = true;
                self.paint();
            }
            UserEvent::Cursor => self.apply_cursor(el),
            UserEvent::Key { hid, down, repeat } => {
                if self.focused {
                    self.on_key(el, hid, down, repeat);
                }
            }
            UserEvent::Closed(reason) => {
                if self.session.as_ref().is_some_and(|s| s.stop.load(std::sync::atomic::Ordering::Relaxed))
                    && reason.is_empty()
                {
                    return el.exit();
                }
                // The host deliberately ended things: don't fight it.
                let deliberate = reason.is_empty()
                    || reason.contains("ended the session")
                    || reason.contains("display is now")
                    || reason.contains("permission");
                if deliberate {
                    let code = if reason.is_empty() { EXIT_OK } else { EXIT_ERROR };
                    self.fail(el, code, reason);
                } else {
                    self.start_reconnect(reason);
                }
            }
            UserEvent::Reconnected(conn) => {
                if self.attach(el, *conn) {
                    self.toast(vec![format!("Reconnected to {}", self.host_name)], 3);
                }
            }
            UserEvent::GaveUp(reason) => self.fail(el, EXIT_ERROR, reason),
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.release_all();
                el.exit();
            }
            WindowEvent::Resized(s) => {
                if let Some(r) = &mut self.renderer {
                    r.resize(s.width, s.height);
                }
                self.paint();
            }
            WindowEvent::RedrawRequested | WindowEvent::ScaleFactorChanged { .. } => self.paint(),
            WindowEvent::Focused(f) => {
                self.focused = f;
                if !f {
                    self.release_all();
                    if self.mouse_locked {
                        self.set_mouse_lock(false);
                    }
                }
            }
            WindowEvent::KeyboardInput { event, is_synthetic, .. } => {
                if is_synthetic {
                    return;
                }
                let PhysicalKey::Code(code) = event.physical_key else { return };
                let Some(hid) = keymap::keycode_to_hid(code) else { return };
                self.on_key(el, hid, event.state == ElementState::Pressed, event.repeat);
            }
            WindowEvent::CursorMoved { position, .. } => {
                if !self.mouse_locked {
                    self.mouse_abs(position);
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
                self.send(Input::Button { button: b, down: state == ElementState::Pressed });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy, precise) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x as f64 * 120.0, y as f64 * 120.0, false),
                    MouseScrollDelta::PixelDelta(p) => {
                        let s = self.window.as_ref().map(|w| w.scale_factor()).unwrap_or(1.0);
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

    fn device_event(&mut self, _el: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
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
