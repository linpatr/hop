//! The streaming window: shows the remote screen and forwards keyboard and mouse.
//!
//! Local shortcuts (Ctrl+Alt+Shift, i.e. Ctrl+Option+Shift on a Mac):
//!   Q  disconnect      F  toggle fullscreen      M  toggle mouse lock (games)      S  toggle stats

use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::PhysicalKey;
use winit::window::{CursorGrabMode, Fullscreen, Window, WindowId};

use crate::client::{self, ClientShared, ConnectError, ConnectOptions, Event};
use crate::keymap;
use crate::proto::Input;
use crate::render::Renderer;

pub enum UserEvent {
    Frame,
    Closed(String),
    /// A key captured by the system-wide keyboard grab.
    Key { hid: u16, down: bool, repeat: bool },
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
    focused: bool,
    /// Physical keys held on this computer (before any Ctrl/Cmd swap).
    local_down: std::collections::HashSet<u16>,
    /// Keys swallowed as local shortcuts, so their release isn't sent either.
    hotkey_keys: std::collections::HashSet<u16>,
    key_grab: bool,
    mouse_locked: bool,
    show_stats: bool,
    last_abs: Option<(u16, u16)>,
    rel_accum: (f64, f64),
    wheel_accum: (f64, f64),
    pressed: std::collections::HashSet<u16>,
    last_title: Instant,
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
        focused: true,
        local_down: Default::default(),
        hotkey_keys: Default::default(),
        key_grab: false,
        mouse_locked: false,
        show_stats: true,
        last_abs: None,
        rel_accum: (0.0, 0.0),
        wheel_accum: (0.0, 0.0),
        pressed: Default::default(),
        last_title: Instant::now(),
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
        self.host_name = conn.host.name.clone();
        self.host_os = conn.host.os.clone();
        let (vw, vh) = (conn.info.width, conn.info.height);

        let mut attrs = Window::default_attributes().with_title(format!("Hop — {}", self.host_name));
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
        let proxy = self.proxy.clone();
        let session = match client::start(conn, move |e| {
            let _ = proxy.send_event(match e {
                Event::Frame => UserEvent::Frame,
                Event::Closed(r) => UserEvent::Closed(r),
            });
        }) {
            Ok(s) => s,
            Err(e) => return self.fail(el, EXIT_ERROR, format!("{e:#}")),
        };
        if self.settings.grab_keyboard {
            let proxy = self.proxy.clone();
            self.key_grab = crate::platform::start_key_grab(Box::new(move |hid, down, repeat| {
                let _ = proxy.send_event(UserEvent::Key { hid, down, repeat });
            }));
            log::info!("system shortcut capture: {}", if self.key_grab { "on" } else { "unavailable" });
        }
        window.set_cursor_visible(false);
        window.focus_window();
        self.window = Some(window);
        self.renderer = Some(renderer);
        self.session = Some(session);
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
        w.set_cursor_visible(false);
        self.rel_accum = (0.0, 0.0);
        self.update_title(true);
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
                self.update_title(true);
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

    fn update_title(&mut self, force: bool) {
        if !force && self.last_title.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_title = Instant::now();
        let (Some(w), Some(s)) = (&self.window, &self.session) else { return };
        let mut t = format!("Hop — {}", self.host_name);
        if self.mouse_locked {
            t.push_str("  [mouse locked: Ctrl+Alt+Shift+M to release]");
        }
        if self.show_stats {
            let st = s.stats.lock().clone();
            t.push_str(&format!(
                "  ·  {}x{} {:.0} fps  ·  {:.1} Mbps  ·  host {:.1} ms  ·  rtt {:.1} ms  ·  decode {:.1} ms  ·  lost {} (fixed {})",
                st.width, st.height, st.fps, st.mbps, st.host_ms, st.rtt_ms, st.decode_ms, st.lost, st.recovered
            ));
        }
        w.set_title(&t);
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.session.is_none() && self.exit_message.is_none() {
            self.connect(el);
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, ev: UserEvent) {
        match ev {
            UserEvent::Frame => {
                let (Some(s), Some(r)) = (&self.session, &mut self.renderer) else { return };
                let Some(f) = s.frame.lock().take() else { return };
                r.upload(&f);
                s.recycle(f);
                if let Some(w) = &self.window {
                    w.pre_present_notify();
                }
                r.render();
                self.update_title(false);
            }
            UserEvent::Key { hid, down, repeat } => {
                if self.focused {
                    self.on_key(el, hid, down, repeat);
                }
            }
            UserEvent::Closed(reason) => {
                let code = if reason.is_empty() { EXIT_OK } else { EXIT_ERROR };
                self.fail(el, code, reason);
            }
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
                    r.render();
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(r) = &mut self.renderer {
                    r.render();
                }
            }
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
            WindowEvent::CursorEntered { .. } => {
                if let Some(w) = &self.window {
                    w.set_cursor_visible(false);
                }
            }
            WindowEvent::CursorLeft { .. } => {
                if let Some(w) = &self.window {
                    w.set_cursor_visible(true);
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
