//! The main Hop window: hosts this computer, lists other computers, launches the viewer.

use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};
use parking_lot::Mutex;

use crate::config::{Config, KnownHost};
use crate::discovery::{self, Browser};
use crate::host::{self, Host, HostOptions};
use crate::window::{EXIT_OK, EXIT_WRONG_CODE};

const RED: Color32 = Color32::from_rgb(220, 80, 60);
const GREEN: Color32 = Color32::from_rgb(60, 170, 90);

#[derive(Default, Clone)]
struct ViewerState {
    /// "", "connected" or "reconnecting", reported by the viewer on stderr.
    state: String,
    /// The host's real id/name/os, once connected.
    host: Option<(String, String, String)>,
    /// Last non-status line printed (the error message when it fails).
    last_line: String,
}

struct Viewer {
    host_id: String,
    host_name: String,
    child: Child,
    state: Arc<Mutex<ViewerState>>,
    started: Instant,
    yielded: bool,
}

enum Prompt {
    None,
    Code { host_id: String, name: String, addr: String, os: String, code: String, error: Option<String>, focused: bool },
}

struct Launcher {
    cfg: Config,
    code: Arc<Mutex<String>>,
    host: Option<Host>,
    host_error: Option<String>,
    browser: Option<Browser>,
    viewers: Vec<Viewer>,
    prompt: Prompt,
    manual_addr: String,
    notice: Option<(String, bool)>,
    ips: Vec<String>,
    #[cfg(target_os = "macos")]
    perms: (crate::platform::Permissions, Instant),
    install_url: Option<String>,
    start_minimized: bool,
    autostart: bool,
    /// Running from Downloads on Windows: wait for Install (or an explicit "use without installing").
    portable_pending: bool,
    renaming: Option<String>,
    last_client: Option<String>,
    quitting: bool,
    copied_at: Option<Instant>,
}

pub fn run() -> i32 {
    let cfg = Config::load();
    let code = Arc::new(Mutex::new(cfg.access_code.clone()));
    let ips: Vec<String> = discovery::local_ips().iter().map(|i| i.to_string()).collect();
    let install_url = crate::selftest::start_installer_server().map(|_| {
        format!("http://{}:{}/install.ps1", ips.first().cloned().unwrap_or_else(|| "this-mac".into()), crate::proto::INSTALL_PORT)
    });
    let portable_pending = crate::install::needs_install();
    let mut app = Launcher {
        cfg,
        code,
        host: None,
        host_error: None,
        browser: None,
        viewers: Vec::new(),
        prompt: Prompt::None,
        manual_addr: String::new(),
        notice: None,
        ips,
        #[cfg(target_os = "macos")]
        perms: (crate::platform::host_permissions(), Instant::now()),
        install_url,
        start_minimized: std::env::args().any(|a| a == "--minimized"),
        autostart: crate::autostart::enabled(),
        portable_pending,
        renaming: None,
        last_client: None,
        quitting: false,
        copied_at: None,
    };
    // Opening ports from a not-yet-installed copy would make Windows Firewall ask about the wrong
    // file, so networking waits for Install.
    if !portable_pending {
        app.start_networking();
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Hop")
            .with_inner_size([500.0, 700.0])
            .with_min_inner_size([400.0, 440.0])
            .with_icon(eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon-256.png")).unwrap_or_default()),
        ..Default::default()
    };
    match eframe::run_native("Hop", options, Box::new(|_cc| Ok(Box::new(app)))) {
        Ok(()) => 0,
        Err(e) => {
            log::error!("ui: {e}");
            1
        }
    }
}

fn pretty_code(code: &str) -> String {
    let (a, b) = code.split_at(3.min(code.len()));
    format!("{a}-{b}")
}

fn os_label(os: &str) -> &str {
    match os {
        "macos" => "Mac",
        "windows" => "Windows",
        "linux" => "Linux",
        _ => "",
    }
}

impl Launcher {
    fn start_networking(&mut self) {
        self.portable_pending = false;
        if self.browser.is_none() {
            self.browser = Some(Browser::start(self.cfg.id.clone()));
        }
        if self.cfg.hosting && self.host.is_none() {
            self.start_host();
        }
    }

    fn start_host(&mut self) {
        // The previous host may still be letting go of its ports (e.g. right after a rename).
        let mut last_err = None;
        for _ in 0..10 {
            let opts = HostOptions { id: self.cfg.id.clone(), name: self.cfg.name.clone(), code: self.code.clone() };
            match host::start_host(opts) {
                Ok(h) => {
                    self.host = Some(h);
                    self.host_error = None;
                    return;
                }
                Err(e) => last_err = Some(format!("{e:#}")),
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        self.host_error = last_err;
    }

    fn set_hosting(&mut self, on: bool) {
        self.cfg.hosting = on;
        self.cfg.save();
        if on {
            self.start_host();
        } else if let Some(h) = self.host.take() {
            h.stop();
        }
    }

    fn rename(&mut self, name: String) {
        let name = name.trim().to_string();
        if name.is_empty() || name == self.cfg.name {
            return;
        }
        self.cfg.name = name;
        self.cfg.name_customized = true;
        self.cfg.save();
        // The name is in the network announcements, so restart hosting to pick it up.
        if let Some(h) = self.host.take() {
            h.stop();
            std::thread::sleep(Duration::from_millis(300));
            self.start_host();
        }
    }

    fn connect(&mut self, host_id: String, name: String, addr: String, os: String, code: String) {
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => {
                self.notice = Some((format!("Can't start the viewer: {e}"), true));
                return;
            }
        };
        let child = Command::new(exe)
            .arg("connect")
            .arg(&addr)
            .env("HOP_CODE", &code)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                self.notice = Some((format!("Can't start the viewer: {e}"), true));
                return;
            }
        };
        // Read the viewer's output continuously (so it can never block on a full pipe) and pick
        // out its status lines.
        let state = Arc::new(Mutex::new(ViewerState::default()));
        if let Some(err) = child.stderr.take() {
            let st = state.clone();
            std::thread::spawn(move || {
                use std::io::BufRead;
                for line in std::io::BufReader::new(err).lines().map_while(Result::ok) {
                    if let Some(json) = line.strip_prefix("HOP_STATUS ") {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(json) {
                            let mut s = st.lock();
                            s.state = v["state"].as_str().unwrap_or_default().to_string();
                            if let (Some(id), Some(n), Some(o)) = (v["id"].as_str(), v["name"].as_str(), v["os"].as_str()) {
                                s.host = Some((id.into(), n.into(), o.into()));
                            }
                        }
                    } else if !line.trim().is_empty() {
                        st.lock().last_line = line;
                    }
                }
            });
        }
        self.cfg.known_hosts.insert(host_id.clone(), KnownHost { name: name.clone(), addr, os, code });
        self.cfg.save();
        self.notice = None;
        self.viewers.push(Viewer { host_id, host_name: name, child, state, started: Instant::now(), yielded: false });
    }

    fn start_connect(&mut self, host_id: String, name: String, addr: String, os: String) {
        if let Some(v) = self.viewers.iter().find(|v| v.host_id == host_id) {
            self.notice = Some((format!("Already connected to {}", v.host_name), false));
            return;
        }
        match self.cfg.known_hosts.get(&host_id).map(|h| h.code.clone()).filter(|c| !c.is_empty()) {
            Some(code) => self.connect(host_id, name, addr, os, code),
            None => {
                self.prompt =
                    Prompt::Code { host_id, name, addr, os, code: String::new(), error: None, focused: false }
            }
        }
    }

    fn poll_viewers(&mut self) {
        let mut finished = Vec::new();
        for (i, v) in self.viewers.iter_mut().enumerate() {
            // Let the viewer's window come to the front (macOS only allows it if we yield).
            #[cfg(target_os = "macos")]
            if !v.yielded && v.started.elapsed() < Duration::from_secs(10) {
                v.yielded = crate::platform::yield_activation(v.child.id());
            }
            let _ = (&v.yielded, &v.started);
            // A computer added by IP gets its real identity once connected.
            let real = v.state.lock().host.clone();
            if let Some((id, name, os)) = real
                && v.host_id != id
            {
                if let Some(mut k) = self.cfg.known_hosts.remove(&v.host_id) {
                    k.name = name.clone();
                    k.os = os;
                    self.cfg.known_hosts.insert(id.clone(), k);
                    self.cfg.save();
                }
                v.host_id = id;
                v.host_name = name;
            }
            if let Ok(Some(status)) = v.child.try_wait() {
                std::thread::sleep(Duration::from_millis(50));
                finished.push((i, status.code().unwrap_or(1), v.state.lock().last_line.clone()));
            }
        }
        for (i, code, err) in finished.into_iter().rev() {
            let v = self.viewers.remove(i);
            match code {
                EXIT_OK => {}
                EXIT_WRONG_CODE => {
                    let known = self.cfg.known_hosts.get_mut(&v.host_id).map(|h| {
                        h.code.clear();
                        (h.addr.clone(), h.os.clone())
                    });
                    self.cfg.save();
                    if let Some((addr, os)) = known {
                        self.prompt = Prompt::Code {
                            host_id: v.host_id,
                            name: v.host_name.clone(),
                            addr,
                            os,
                            code: String::new(),
                            error: Some("That code didn't work. Check the code shown in Hop on that computer.".into()),
                            focused: false,
                        };
                    }
                }
                _ => {
                    let msg = if err.is_empty() { "the connection failed".to_string() } else { err };
                    self.notice = Some((format!("{}: {msg}", v.host_name), true));
                }
            }
        }
    }

    fn install_card(&mut self, ui: &mut egui::Ui) {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(RichText::new("Install Hop on this PC").strong().size(16.0));
            ui.label("Puts Hop in your Start menu, starts it when you sign in, and lets it through Windows Firewall (Windows asks for admin once).");
            ui.horizontal(|ui| {
                if ui.button(RichText::new("Install Hop").strong()).clicked() {
                    match crate::install::install() {
                        Ok(dest) => {
                            if let Some(h) = self.host.take() {
                                h.stop();
                            }
                            std::thread::sleep(Duration::from_millis(300));
                            let _ = Command::new(dest).spawn();
                            std::process::exit(0);
                        }
                        Err(e) => self.notice = Some((format!("Install failed: {e:#}"), true)),
                    }
                }
                if ui.small_button("Use without installing").clicked() {
                    self.start_networking();
                }
            });
        });
        ui.add_space(8.0);
    }

    fn this_computer(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("THIS COMPUTER").small().weak());
        let mut rename_done = None;
        match &mut self.renaming {
            Some(buf) => {
                ui.horizontal(|ui| {
                    let r = ui.add(egui::TextEdit::singleline(buf).desired_width(220.0).font(egui::TextStyle::Heading));
                    r.request_focus();
                    if ui.button("Save").clicked() || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) {
                        rename_done = Some(Some(buf.clone()));
                    }
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        rename_done = Some(None);
                    }
                });
            }
            None => {
                ui.horizontal(|ui| {
                    ui.heading(&self.cfg.name);
                    if ui.small_button("Rename").on_hover_text("The name other computers see").clicked() {
                        self.renaming = Some(self.cfg.name.clone());
                    }
                });
            }
        }
        if let Some(r) = rename_done {
            self.renaming = None;
            if let Some(name) = r {
                self.rename(name);
            }
        }
        if !self.ips.is_empty() {
            ui.label(RichText::new(self.ips.join(", ")).weak());
        }
        ui.add_space(6.0);

        let mut on = self.cfg.hosting;
        let enabled = !self.portable_pending;
        if ui.add_enabled(enabled, egui::Checkbox::new(&mut on, "Let my other computers control this one")).changed() {
            self.set_hosting(on);
        }
        if !self.cfg.hosting || self.portable_pending {
            return;
        }
        if let Some(e) = &self.host_error {
            ui.colored_label(RED, e);
        }
        ui.horizontal(|ui| {
            ui.label("Access code");
            let code = self.code.lock().clone();
            ui.label(RichText::new(pretty_code(&code)).monospace().size(26.0).strong());
            let copied = self.copied_at.is_some_and(|t| t.elapsed() < Duration::from_secs(2));
            if ui.small_button(if copied { "Copied" } else { "Copy" }).clicked() {
                ui.ctx().copy_text(pretty_code(&code));
                self.copied_at = Some(Instant::now());
            }
            if ui
                .small_button("New code")
                .on_hover_text("Other computers will need to enter the new code once")
                .clicked()
            {
                let c = crate::crypto::random_code();
                *self.code.lock() = c.clone();
                self.cfg.access_code = c;
                self.cfg.save();
            }
        });
        ui.label(RichText::new("Enter this code on your other computer the first time it connects.").weak());

        if let Some(h) = &self.host {
            let st = h.status.lock().clone();
            ui.add_space(4.0);
            match &st.session {
                Some(s) => {
                    let secs = s.since.elapsed().as_secs_f64().max(0.001);
                    let mbps = s.sent_bytes.load(Ordering::Relaxed) as f64 * 8.0 / secs / 1e6;
                    let mut kick = false;
                    ui.horizontal(|ui| {
                        ui.colored_label(GREEN, RichText::new(format!("● {} is controlling this computer", s.client)).strong());
                        kick = ui.button("Disconnect").clicked();
                    });
                    ui.label(
                        RichText::new(format!("{}×{} at {} fps · {} · {:.1} Mbps", s.width, s.height, s.fps, s.encoder, mbps))
                            .weak(),
                    );
                    if kick {
                        h.disconnect();
                    }
                    // Get the user's attention when someone new connects.
                    if self.last_client.as_deref() != Some(&s.client) {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                            egui::UserAttentionType::Informational,
                        ));
                        self.last_client = Some(s.client.clone());
                    }
                }
                None => {
                    self.last_client = None;
                    ui.label(RichText::new("● Ready").color(GREEN));
                    if let Some(e) = &st.last_event {
                        ui.label(RichText::new(e).weak());
                    }
                }
            }
            if let Some(e) = &st.error {
                ui.colored_label(RED, e);
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn permissions(&mut self, ui: &mut egui::Ui) {
        if self.perms.1.elapsed() > Duration::from_secs(2) {
            self.perms = (crate::platform::host_permissions(), Instant::now());
        }
        let need_screen = !self.perms.0.screen && self.cfg.hosting;
        let need_access = !self.perms.0.accessibility;
        if !need_screen && !need_access {
            return;
        }
        ui.add_space(6.0);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(RichText::new("macOS permissions needed").strong());
            if need_screen {
                ui.horizontal(|ui| {
                    ui.label("Screen Recording: lets your other computer see this screen");
                    if ui.button("Allow…").clicked() && !crate::platform::request_screen_permission() {
                        crate::platform::open_privacy_settings(false);
                    }
                });
            }
            if need_access {
                ui.horizontal(|ui| {
                    ui.label("Accessibility: for remote mouse/keyboard, and Cmd+Tab while viewing");
                    if ui.button("Allow…").clicked() && !crate::platform::request_accessibility_permission() {
                        crate::platform::open_privacy_settings(true);
                    }
                });
            }
            ui.label(RichText::new("After switching Hop on in System Settings, quit and reopen Hop.").weak());
        });
    }

    fn sessions(&mut self, ui: &mut egui::Ui) {
        if self.viewers.is_empty() {
            return;
        }
        ui.label(RichText::new("YOUR SESSIONS").small().weak());
        let mut kill = None;
        for (i, v) in self.viewers.iter().enumerate() {
            let state = v.state.lock().state.clone();
            ui.horizontal(|ui| {
                let (text, color) = match state.as_str() {
                    "connected" => (format!("● Controlling {}", v.host_name), GREEN),
                    "reconnecting" => (format!("● Reconnecting to {}…", v.host_name), Color32::from_rgb(220, 160, 40)),
                    _ => (format!("Connecting to {}…", v.host_name), ui.visuals().text_color()),
                };
                ui.colored_label(color, RichText::new(text).strong());
                if ui.small_button("Disconnect").clicked() {
                    kill = Some(i);
                }
            });
        }
        if let Some(i) = kill {
            let _ = self.viewers[i].child.kill();
        }
        ui.add_space(6.0);
    }

    fn prompt_card(&mut self, ui: &mut egui::Ui) {
        let mut submit = None;
        let mut cancel = false;
        if let Prompt::Code { host_id, name, addr, os, code, error, focused } = &mut self.prompt {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.label(RichText::new(format!("Connect to {name}")).strong().size(16.0));
                ui.label(format!("Enter the access code shown in Hop on {name}. You only need to do this once."));
                if let Some(e) = error {
                    ui.colored_label(RED, e.as_str());
                }
                ui.horizontal(|ui| {
                    let r = ui.add(
                        egui::TextEdit::singleline(code)
                            .hint_text("ABC-123")
                            .desired_width(120.0)
                            .font(egui::TextStyle::Monospace)
                            .char_limit(12),
                    );
                    if !*focused {
                        r.request_focus();
                        *focused = true;
                    }
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let valid = crate::crypto::normalize_code(code).len() >= 4;
                    if (ui.add_enabled(valid, egui::Button::new("Connect")).clicked() || (enter && valid)) && valid {
                        submit = Some((host_id.clone(), name.clone(), addr.clone(), os.clone(), crate::crypto::normalize_code(code)));
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
            ui.add_space(6.0);
        }
        if let Some((id, name, addr, os, code)) = submit {
            self.prompt = Prompt::None;
            self.connect(id, name, addr, os, code);
        } else if cancel {
            self.prompt = Prompt::None;
        }
    }

    fn peers_card(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("YOUR OTHER COMPUTERS").small().weak());
        let peers = self.browser.as_ref().map(|b| b.list()).unwrap_or_default();
        let mut action = None;
        if self.portable_pending {
            ui.label(RichText::new("Install Hop (or choose \"Use without installing\") to find your other computers.").weak());
        } else if peers.is_empty() {
            ui.label(RichText::new("Looking for computers running Hop on this network…").weak());
        }
        for p in &peers {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&p.beacon.name).strong());
                ui.label(RichText::new(format!("{} · {}", os_label(&p.beacon.os), p.ip)).weak());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let busy = self.viewers.iter().any(|v| v.host_id == p.beacon.id);
                    let saved = self.cfg.known_hosts.get(&p.beacon.id).is_some_and(|h| !h.code.is_empty());
                    let label = if busy { "Connected" } else { "Connect" };
                    let b = ui.add_enabled(!busy, egui::Button::new(RichText::new(label).strong()));
                    let b = if saved || busy { b } else { b.on_hover_text("You'll be asked for its access code once") };
                    if b.clicked() {
                        action = Some((p.beacon.id.clone(), p.beacon.name.clone(), p.addr(), p.beacon.os.clone()));
                    }
                });
            });
        }
        // Remembered computers that aren't announcing themselves right now.
        let offline: Vec<(String, KnownHost)> = self
            .cfg
            .known_hosts
            .iter()
            .filter(|(id, _)| !peers.iter().any(|p| &p.beacon.id == *id))
            .map(|(id, h)| (id.clone(), h.clone()))
            .collect();
        for (id, h) in offline {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&h.name).weak());
                ui.label(RichText::new(format!("{} · {} · not seen right now", os_label(&h.os), h.addr)).weak().small());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("Forget").on_hover_text("Also forgets its access code").clicked() {
                        self.cfg.known_hosts.remove(&id);
                        self.cfg.save();
                    }
                    if ui.small_button("Try").clicked() {
                        action = Some((id.clone(), h.name.clone(), h.addr.clone(), h.os.clone()));
                    }
                });
            });
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("Not listed? Connect by IP:");
            let r = ui.add(egui::TextEdit::singleline(&mut self.manual_addr).hint_text("192.168.1.20").desired_width(130.0));
            let go = ui.button("Connect").clicked() || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
            if go && !self.manual_addr.trim().is_empty() {
                let addr = self.manual_addr.trim().to_string();
                action = Some((format!("ip:{addr}"), addr.clone(), addr, String::new()));
            }
        });
        if let Some((id, name, addr, os)) = action {
            self.start_connect(id, name, addr, os);
        }
    }

    fn settings_card(&mut self, ui: &mut egui::Ui) {
        let s = &mut self.cfg.stream;
        let mut changed = false;
        ui.label(RichText::new("Changes apply the next time you connect.").weak());
        egui::Grid::new("settings").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label("Resolution");
            egui::ComboBox::from_id_salt("res")
                .selected_text(match s.max_height {
                    0 => "Match my screen".to_string(),
                    h => format!("Up to {h}p"),
                })
                .show_ui(ui, |ui| {
                    for (h, label) in [(0, "Match my screen"), (2160, "Up to 2160p"), (1440, "Up to 1440p"), (1080, "Up to 1080p"), (720, "Up to 720p")] {
                        changed |= ui.selectable_value(&mut s.max_height, h, label).changed();
                    }
                });
            ui.end_row();
            ui.label("Frame rate");
            ui.horizontal(|ui| {
                for f in [30, 60, 90, 120] {
                    changed |= ui.selectable_value(&mut s.fps, f, format!("{f}")).changed();
                }
            });
            ui.end_row();
            ui.label("Bitrate");
            changed |= ui
                .add(egui::Slider::new(&mut s.bitrate_mbps, 5..=150).suffix(" Mbps"))
                .on_hover_text("Higher looks sharper. Wi-Fi is usually happy up to 50; Ethernet can go higher.")
                .changed();
            ui.end_row();
            ui.label("Loss protection");
            changed |= ui
                .add(egui::Slider::new(&mut s.fec_percent, 0..=50).suffix(" %"))
                .on_hover_text("Extra packets that repair Wi-Fi packet loss without a hiccup. 0 is fine on Ethernet.")
                .changed();
            ui.end_row();
            ui.label("");
            ui.vertical(|ui| {
                changed |= ui.checkbox(&mut s.audio, "Play the other computer's sound").changed();
                changed |= ui.checkbox(&mut s.clipboard, "Share copied text both ways").changed();
                changed |= ui
                    .checkbox(&mut s.grab_keyboard, "Send Cmd+Tab / Win key / Alt+Tab to the other computer")
                    .changed();
                changed |= ui.checkbox(&mut s.fullscreen, "Open fullscreen").changed();
                changed |= ui.checkbox(&mut s.vsync, "V-Sync (no tearing, a little more latency)").changed();
                changed |= ui
                    .checkbox(&mut s.swap_ctrl_cmd, "Swap Ctrl and Cmd between Mac and Windows")
                    .on_hover_text("Cmd+C on a Mac keyboard becomes Ctrl+C on Windows, and Ctrl+C on a PC keyboard becomes Cmd+C on a Mac.")
                    .changed();
            });
            ui.end_row();
        });
        if changed {
            self.cfg.save();
        }
    }
}

impl eframe::App for Launcher {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if std::mem::take(&mut self.start_minimized) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
        // Closing the window keeps hosting running; Quit really exits.
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting && self.host.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
        self.poll_viewers();
        ctx.request_repaint_after(Duration::from_millis(500));

        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.add_space(6.0);
            if self.portable_pending {
                self.install_card(ui);
            }
            self.this_computer(ui);
            #[cfg(target_os = "macos")]
            self.permissions(ui);
            if crate::autostart::available() && !self.portable_pending {
                let mut on = self.autostart;
                if ui.checkbox(&mut on, "Start Hop when I sign in").changed() {
                    match crate::autostart::set(on) {
                        Ok(()) => self.autostart = on,
                        Err(e) => self.notice = Some((format!("Couldn't change start-at-login: {e}"), true)),
                    }
                }
            }
            ui.separator();
            self.sessions(ui);
            self.prompt_card(ui);
            if let Some((msg, is_err)) = self.notice.clone() {
                ui.horizontal(|ui| {
                    if is_err {
                        ui.colored_label(RED, msg);
                    } else {
                        ui.label(msg);
                    }
                    if ui.small_button("✕").clicked() {
                        self.notice = None;
                    }
                });
            }
            self.peers_card(ui);
            ui.separator();
            egui::CollapsingHeader::new("Stream settings").show(ui, |ui| self.settings_card(ui));
            egui::CollapsingHeader::new("Keyboard shortcuts while connected").show(ui, |ui| {
                let p = if cfg!(target_os = "macos") { "Ctrl+Option+Shift" } else { "Ctrl+Alt+Shift" };
                egui::Grid::new("keys").num_columns(2).show(ui, |ui| {
                    for (k, what) in [("Q", "disconnect"), ("F", "fullscreen on/off"), ("M", "lock the mouse (games)"), ("S", "show/hide stats")] {
                        ui.label(RichText::new(format!("{p}+{k}")).monospace());
                        ui.label(what);
                        ui.end_row();
                    }
                });
            });
            if let Some(url) = &self.install_url {
                egui::CollapsingHeader::new("Install on a Windows PC").show(ui, |ui| {
                    ui.label("Open PowerShell on the PC and run:");
                    let cmd = format!("iwr {url} | iex");
                    ui.horizontal(|ui| {
                        ui.code(&cmd);
                        if ui.small_button("Copy").clicked() {
                            ui.ctx().copy_text(cmd.clone());
                        }
                    });
                    ui.label(RichText::new("Or download Hop.exe from the GitHub release and click Install.").weak());
                });
            }
            ui.separator();
            ui.horizontal(|ui| {
                if self.host.is_some() {
                    ui.label(RichText::new("Closing this window keeps Hop running so you can connect in.").weak().small());
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("Quit Hop").clicked() {
                        self.quitting = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            });
        });
    }

    fn on_exit(&mut self) {
        for v in &mut self.viewers {
            let _ = v.child.kill();
        }
        if let Some(h) = self.host.take() {
            h.stop();
        }
    }
}
