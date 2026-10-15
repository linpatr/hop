//! The Hop window. One window, two modes: the home screen (this computer, your other
//! computers, settings) and, once connected, the other computer's screen. Disconnecting goes
//! back home.

use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{DeviceEvent, DeviceId, ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::window::{Fullscreen, UserAttentionType, Window, WindowId};

use crate::client::{self, ConnectError};
use crate::home::{Home, HomeAction};
use crate::render::Renderer;
use crate::theme;
use crate::window::{self, StreamEnd, StreamSettings, StreamView, UserEvent};

/// A connection being made from the home screen.
struct Pending {
    generation: u64,
    host_id: String,
    name: String,
    settings: StreamSettings,
}

struct HopApp {
    proxy: EventLoopProxy<UserEvent>,
    home: Home,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    egui_state: Option<egui_winit::State>,
    view: Option<StreamView>,
    generation: u64,
    pending: Option<Pending>,
    /// Shortcut capture started (Some(ok)) or not tried yet.
    key_grab: Option<bool>,
    focused: bool,
    mods: ModifiersState,
    /// The home window's size and place, restored after a stream.
    home_geometry: Option<(PhysicalSize<u32>, Option<PhysicalPosition<i32>>)>,
    next_paint: Instant,
    start_minimized: bool,
    /// Testing: HOP_TEST_DISCONNECT_AFTER / HOP_TEST_QUIT_AFTER (seconds).
    test_disconnect_at: Option<Instant>,
    test_quit_at: Option<Instant>,
}

pub fn run() -> i32 {
    let event_loop = match window::event_loop() {
        Ok(e) => e,
        Err(e) => {
            log::error!("event loop: {e}");
            return 1;
        }
    };
    let mut app = HopApp {
        proxy: event_loop.create_proxy(),
        home: Home::new(),
        window: None,
        renderer: None,
        egui_state: None,
        view: None,
        generation: 0,
        pending: None,
        key_grab: None,
        focused: true,
        mods: ModifiersState::empty(),
        home_geometry: None,
        next_paint: Instant::now(),
        start_minimized: std::env::args().any(|a| a == "--minimized"),
        test_disconnect_at: None,
        test_quit_at: std::env::var("HOP_TEST_QUIT_AFTER")
            .ok()
            .and_then(|v| v.parse().ok())
            .map(|s| Instant::now() + Duration::from_secs(s)),
    };
    if let Err(e) = event_loop.run_app(&mut app) {
        log::error!("event loop: {e}");
    }
    if let (Some(v), Some(w)) = (&mut app.view, &app.window) {
        v.close(w);
    }
    app.home.shutdown();
    0
}

fn window_icon() -> Option<winit::window::Icon> {
    let i = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon-256.png")).ok()?;
    winit::window::Icon::from_rgba(i.rgba, i.width, i.height).ok()
}

impl HopApp {
    fn refresh_displays(&mut self, el: &ActiveEventLoop) {
        self.home.displays = el.available_monitors().filter_map(|m| m.name()).collect();
    }

    fn paint(&mut self) {
        let (Some(w), Some(r)) = (&self.window, &mut self.renderer) else { return };
        w.pre_present_notify();
        if let Some(v) = &mut self.view {
            let ov = v.overlay();
            r.render(&ov, w.scale_factor() as f32);
            v.dirty = false;
            self.next_paint =
                Instant::now() + if v.overlay_is_live() { Duration::from_millis(250) } else { Duration::from_secs(3600) };
            return;
        }
        let Some(state) = &mut self.egui_state else { return };
        let ctx = r.egui().clone();
        let raw = state.take_egui_input(w);
        let home = &mut self.home;
        let mut out = ctx.run_ui(raw, |ui| home.ui(ui));
        state.handle_platform_output(w, std::mem::take(&mut out.platform_output));
        let delay = out.viewport_output.get(&egui::ViewportId::ROOT).map(|v| v.repaint_delay).unwrap_or(Duration::MAX);
        r.render_ui(out, theme::background(&ctx));
        // Repaint when egui asks (animations), and at least twice a second for status changes.
        self.next_paint = Instant::now() + delay.min(Duration::from_millis(500));
    }

    fn handle_home_actions(&mut self, el: &ActiveEventLoop) {
        if std::mem::take(&mut self.home.wants_attention)
            && let Some(w) = &self.window
        {
            w.request_user_attention(Some(UserAttentionType::Informational));
        }
        for action in std::mem::take(&mut self.home.actions) {
            match action {
                HomeAction::Connect { host_id, name, addr, code } => self.start_connect(el, host_id, name, addr, code),
                HomeAction::CancelConnect => self.pending = None,
                HomeAction::Quit => el.exit(),
            }
        }
    }

    fn start_connect(&mut self, el: &ActiveEventLoop, host_id: String, name: String, addr: String, code: String) {
        let mut settings = self.home.stream_settings(&addr, &code);
        let display = if settings.fullscreen { settings.fullscreen_display.clone() } else { String::new() };
        let monitor = window::find_display(el, self.window.as_deref(), &display);
        let screen = monitor.as_ref().map(|m| m.size()).map(|s| (s.width, s.height)).unwrap_or((1920, 1080));
        settings.fit_to(screen);
        self.generation += 1;
        let generation = self.generation;
        self.pending = Some(Pending { generation, host_id, name, settings: settings.clone() });
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let result = client::connect(&settings.connect)
                .map(Box::new)
                .map_err(|e| (matches!(e, ConnectError::WrongCode), e.to_string()));
            let _ = proxy.send_event(UserEvent::Connected { generation, result });
        });
    }

    fn enter_stream(&mut self, el: &ActiveEventLoop, conn: client::Connection) {
        let Some(p) = self.pending.take() else { return };
        let (Some(w), Some(r)) = (self.window.clone(), &mut self.renderer) else { return };
        self.home.on_connected(&p.host_id, &conn.host);
        if self.home_geometry.is_none() {
            self.home_geometry = Some((w.inner_size(), w.outer_position().ok()));
        }
        if p.settings.grab_keyboard && self.key_grab.is_none() {
            self.key_grab = Some(window::start_key_grab(&self.proxy));
        }
        let grab = p.settings.grab_keyboard && self.key_grab == Some(true);
        r.clear_video();
        w.set_title(&format!("Hop — {}", conn.host.name));
        w.set_min_inner_size(None::<LogicalSize<f64>>);
        if p.settings.fullscreen {
            let m = window::find_display(el, Some(&w), &p.settings.fullscreen_display);
            w.set_fullscreen(Some(Fullscreen::Borderless(m)));
        } else {
            let m = w.current_monitor();
            let _ = w.request_inner_size(window::fitted_size(m.as_ref(), conn.info.width, conn.info.height));
        }
        let host_name = conn.host.name.clone();
        match StreamView::start(p.settings, conn, p.generation, &w, self.proxy.clone(), grab) {
            Ok(v) => {
                log::info!("showing {host_name}'s screen");
                self.test_disconnect_at = std::env::var("HOP_TEST_DISCONNECT_AFTER")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .map(|s| Instant::now() + Duration::from_secs(s));
                self.view = Some(v);
                crate::platform::set_key_grab(grab && self.focused);
                w.focus_window();
            }
            Err(e) => {
                self.home.on_connect_failed(&p.host_id, &p.name, false, e);
                self.leave_stream_window();
            }
        }
        w.request_redraw();
    }

    /// Ends the current stream (if any) and turns the window back into the home screen.
    fn leave_stream(&mut self, end: StreamEnd) {
        if let (Some(mut v), Some(w)) = (self.view.take(), &self.window) {
            v.close(w);
            crate::platform::set_key_grab(false);
            self.home.on_stream_ended(&v.host.id, &v.host.name, end);
        }
        self.leave_stream_window();
    }

    fn leave_stream_window(&mut self) {
        let Some(w) = &self.window else { return };
        log::info!("back to the home screen");
        if let Some(r) = &mut self.renderer {
            r.clear_video();
        }
        w.set_fullscreen(None);
        w.set_title("Hop");
        w.set_min_inner_size(Some(LogicalSize::new(420.0, 480.0)));
        if let Some((size, pos)) = self.home_geometry.take() {
            let _ = w.request_inner_size(size);
            if let Some(p) = pos {
                w.set_outer_position(p);
            }
        }
        w.request_redraw();
    }

    fn after_stream_event(&mut self) {
        let end = self.view.as_mut().and_then(|v| v.take_end());
        if let Some(end) = end {
            self.leave_stream(end);
        } else if self.view.as_ref().is_some_and(|v| v.dirty) {
            self.paint();
        }
    }
}

impl ApplicationHandler<UserEvent> for HopApp {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("Hop")
            .with_inner_size(LogicalSize::new(540.0, 780.0))
            .with_min_inner_size(LogicalSize::new(420.0, 480.0))
            .with_window_icon(window_icon());
        let w = match el.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                log::error!("can't open window: {e}");
                return el.exit();
            }
        };
        let r = match Renderer::new(w.clone(), false) {
            Ok(r) => r,
            Err(e) => {
                log::error!("graphics init failed: {e:#}");
                return el.exit();
            }
        };
        theme::apply(r.egui());
        self.egui_state = Some(egui_winit::State::new(
            r.egui().clone(),
            egui::ViewportId::ROOT,
            &w,
            Some(w.scale_factor() as f32),
            None,
            Some(8192),
        ));
        if std::mem::take(&mut self.start_minimized) {
            w.set_minimized(true);
        }
        self.window = Some(w.clone());
        self.renderer = Some(r);
        self.refresh_displays(el);
        w.request_redraw();
        // Testing: HOP_AUTOCONNECT=<address> (with HOP_CODE) connects right away.
        if let Ok(addr) = std::env::var("HOP_AUTOCONNECT") {
            let code = std::env::var("HOP_CODE").unwrap_or_default();
            self.home.connecting = Some((format!("ip:{addr}"), addr.clone()));
            self.start_connect(el, format!("ip:{addr}"), addr.clone(), addr, code);
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if self.test_disconnect_at.is_some_and(|t| Instant::now() >= t) {
            self.test_disconnect_at = None;
            self.leave_stream(StreamEnd::Closed);
        }
        if self.test_quit_at.is_some_and(|t| Instant::now() >= t) {
            return el.exit();
        }
        if Instant::now() >= self.next_paint {
            if self.view.is_none() {
                self.refresh_displays(el);
            }
            self.paint();
            self.handle_home_actions(el);
        }
        let mut wake = self.next_paint;
        for t in [self.test_disconnect_at, self.test_quit_at].into_iter().flatten() {
            wake = wake.min(t);
        }
        el.set_control_flow(ControlFlow::WaitUntil(wake));
    }

    fn user_event(&mut self, el: &ActiveEventLoop, ev: UserEvent) {
        match ev {
            UserEvent::Connected { generation, result } => {
                let Some(p) = self.pending.as_ref().filter(|p| p.generation == generation) else {
                    return; // cancelled: dropping the connection closes it
                };
                match result {
                    Ok(conn) => self.enter_stream(el, *conn),
                    Err((wrong_code, message)) => {
                        let (id, name) = (p.host_id.clone(), p.name.clone());
                        self.pending = None;
                        self.home.on_connect_failed(&id, &name, wrong_code, message);
                        self.paint();
                    }
                }
            }
            ev => {
                if let (Some(v), Some(w), Some(r)) = (&mut self.view, &self.window, &mut self.renderer) {
                    v.user_event(el, w, r, ev);
                }
                self.after_stream_event();
            }
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(w) = self.window.clone() else { return };
        match &event {
            WindowEvent::CloseRequested => {
                if self.view.is_some() {
                    self.leave_stream(StreamEnd::Closed);
                } else if self.home.is_hosting() {
                    // Keep hosting in the background; Quit Hop really quits.
                    w.set_minimized(true);
                } else {
                    el.exit();
                }
                return;
            }
            WindowEvent::Resized(s) => {
                if let Some(r) = &mut self.renderer {
                    r.resize(s.width, s.height);
                }
                self.paint();
            }
            WindowEvent::RedrawRequested => {
                self.paint();
                self.handle_home_actions(el);
                return;
            }
            WindowEvent::Focused(f) => {
                self.focused = *f;
                let grab = self.view.as_ref().is_some_and(|v| v.key_grab);
                crate::platform::set_key_grab(*f && grab);
            }
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            _ => {}
        }
        if self.view.is_some() {
            if let (Some(v), Some(r)) = (&mut self.view, &mut self.renderer) {
                v.window_event(&w, r, &event);
            }
            self.after_stream_event();
            return;
        }
        // Home screen. Without the standard Mac menu (it would steal Cmd+Q from the remote
        // computer while streaming), handle Cmd+Q / Cmd+W here.
        if let WindowEvent::KeyboardInput { event: k, .. } = &event
            && k.state == ElementState::Pressed
            && cfg!(target_os = "macos")
            && self.mods.super_key()
        {
            match k.physical_key {
                PhysicalKey::Code(KeyCode::KeyQ) => return el.exit(),
                PhysicalKey::Code(KeyCode::KeyW) => {
                    if self.home.is_hosting() {
                        w.set_minimized(true);
                    } else {
                        el.exit();
                    }
                    return;
                }
                _ => {}
            }
        }
        if let Some(state) = &mut self.egui_state {
            let resp = state.on_window_event(&w, &event);
            if resp.repaint {
                w.request_redraw();
            }
        }
    }

    fn device_event(&mut self, _el: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        if let Some(v) = &mut self.view {
            v.device_event(&event);
        }
    }
}
