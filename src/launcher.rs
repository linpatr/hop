//! The main Hop window: hosts this computer, lists other computers, launches the viewer.

use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
#[cfg(target_os = "macos")]
use std::time::Instant;

use eframe::egui;
use parking_lot::Mutex;

use crate::config::{Config, KnownHost};
use crate::discovery::{self, Browser};
use crate::host::{self, Host, HostOptions};
use crate::window::{EXIT_OK, EXIT_WRONG_CODE};

struct Viewer {
    host_id: String,
    host_name: String,
    child: Child,
    /// Last line the viewer printed (its error message, if it fails).
    last_line: Arc<Mutex<String>>,
}

enum Prompt {
    None,
    Code { host_id: String, name: String, addr: String, os: String, code: String, error: Option<String> },
}

struct Launcher {
    cfg: Config,
    code: Arc<Mutex<String>>,
    host: Option<Host>,
    host_error: Option<String>,
    browser: Browser,
    viewers: Vec<Viewer>,
    prompt: Prompt,
    manual_addr: String,
    notice: Option<(String, bool)>,
    ips: Vec<String>,
    #[cfg(target_os = "macos")]
    perms: (crate::platform::Permissions, Instant),
    install_url: Option<String>,
    show_settings: bool,
    start_minimized: bool,
    autostart: bool,
}

pub fn run() -> i32 {
    let cfg = Config::load();
    let code = Arc::new(Mutex::new(cfg.access_code.clone()));
    let browser = Browser::start(cfg.id.clone());
    let ips: Vec<String> = discovery::local_ips().iter().map(|i| i.to_string()).collect();
    let install_url = crate::selftest::start_installer_server().map(|_| {
        format!("http://{}:{}/install.ps1", ips.first().cloned().unwrap_or_else(|| "this-mac".into()), crate::proto::INSTALL_PORT)
    });
    let mut app = Launcher {
        cfg,
        code,
        host: None,
        host_error: None,
        browser,
        viewers: Vec::new(),
        prompt: Prompt::None,
        manual_addr: String::new(),
        notice: None,
        ips,
        #[cfg(target_os = "macos")]
        perms: (crate::platform::host_permissions(), Instant::now()),
        install_url,
        show_settings: false,
        start_minimized: std::env::args().any(|a| a == "--minimized"),
        autostart: crate::autostart::enabled(),
    };
    if app.cfg.hosting {
        app.start_host();
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Hop")
            .with_inner_size([480.0, 680.0])
            .with_min_inner_size([380.0, 420.0])
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

impl Launcher {
    fn start_host(&mut self) {
        let opts = HostOptions { id: self.cfg.id.clone(), name: self.cfg.name.clone(), code: self.code.clone() };
        match host::start_host(opts) {
            Ok(h) => {
                self.host = Some(h);
                self.host_error = None;
            }
            Err(e) => self.host_error = Some(format!("{e:#}")),
        }
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

    fn connect(&mut self, host_id: String, name: String, addr: String, os: String, code: String) {
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => {
                self.notice = Some((format!("can't find myself: {e}"), true));
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
        match child {
            Ok(mut child) => {
                // Drain the viewer's output continuously so it can never block on a full pipe.
                let last_line = Arc::new(Mutex::new(String::new()));
                if let Some(err) = child.stderr.take() {
                    let last = last_line.clone();
                    std::thread::spawn(move || {
                        use std::io::BufRead;
                        for line in std::io::BufReader::new(err).lines().map_while(Result::ok) {
                            if !line.trim().is_empty() {
                                *last.lock() = line;
                            }
                        }
                    });
                }
                self.cfg.known_hosts.insert(host_id.clone(), KnownHost { name: name.clone(), addr, os, code });
                self.cfg.save();
                self.notice = Some((format!("Connecting to {name}…"), false));
                self.viewers.push(Viewer { host_id, host_name: name, child, last_line });
            }
            Err(e) => self.notice = Some((format!("can't start viewer: {e}"), true)),
        }
    }

    fn start_connect(&mut self, host_id: String, name: String, addr: String, os: String) {
        match self.cfg.known_hosts.get(&host_id).map(|h| h.code.clone()).filter(|c| !c.is_empty()) {
            Some(code) => self.connect(host_id, name, addr, os, code),
            None => self.prompt = Prompt::Code { host_id, name, addr, os, code: String::new(), error: None },
        }
    }

    fn poll_viewers(&mut self) {
        let mut finished = Vec::new();
        for (i, v) in self.viewers.iter_mut().enumerate() {
            if let Ok(Some(status)) = v.child.try_wait() {
                // Give the reader thread a moment to pick up the final line.
                std::thread::sleep(Duration::from_millis(50));
                let err = v.last_line.lock().clone();
                finished.push((i, status.code().unwrap_or(1), err));
            }
        }
        for (i, code, err) in finished.into_iter().rev() {
            let v = self.viewers.remove(i);
            match code {
                EXIT_OK => self.notice = None,
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
                            error: Some("That code didn't work. Check the code shown on the other computer.".into()),
                        };
                    }
                    self.notice = None;
                }
                _ => {
                    let msg = if err.is_empty() { "Connection failed".to_string() } else { err };
                    self.notice = Some((format!("{}: {msg}", v.host_name), true));
                }
            }
        }
    }

    fn host_card(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading(&self.cfg.name);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut on = self.cfg.hosting;
                if ui.checkbox(&mut on, "Allow remote control").changed() {
                    self.set_hosting(on);
                }
            });
        });
        if !self.ips.is_empty() {
            ui.label(egui::RichText::new(format!("This computer: {}", self.ips.join(", "))).weak());
        }
        if crate::autostart::available() {
            let mut on = self.autostart;
            if ui.checkbox(&mut on, "Start Hop when I sign in").changed() {
                match crate::autostart::set(on) {
                    Ok(()) => self.autostart = on,
                    Err(e) => self.notice = Some((format!("Couldn't change start-at-login: {e}"), true)),
                }
            }
        }
        if !self.cfg.hosting {
            ui.label("Hosting is off. Other computers can't connect to this one.");
            return;
        }
        if let Some(e) = &self.host_error {
            ui.colored_label(egui::Color32::from_rgb(220, 80, 60), e);
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label("Access code:");
            let code = self.code.lock().clone();
            let pretty = format!("{}-{}", &code[..3.min(code.len())], &code[3.min(code.len())..]);
            ui.label(egui::RichText::new(pretty).monospace().size(26.0).strong());
            if ui.small_button("New code").clicked() {
                let c = crate::crypto::random_code();
                *self.code.lock() = c.clone();
                self.cfg.access_code = c;
                self.cfg.save();
            }
        });
        ui.label(egui::RichText::new("Type this code on the other computer the first time it connects.").weak());

        if let Some(h) = &self.host {
            let st = h.status.lock().clone();
            ui.add_space(4.0);
            match &st.session {
                Some(s) => {
                    let secs = s.since.elapsed().as_secs_f64().max(0.001);
                    let mbps = s.sent_bytes.load(Ordering::Relaxed) as f64 * 8.0 / secs / 1e6;
                    ui.colored_label(
                        egui::Color32::from_rgb(60, 170, 90),
                        format!(
                            "● {} is connected — {}x{} @ {} fps, {}, {:.1} Mbps avg",
                            s.client, s.width, s.height, s.fps, s.encoder, mbps
                        ),
                    );
                }
                None => {
                    ui.label("● Ready for connections");
                }
            }
            if let Some(e) = &st.error {
                ui.colored_label(egui::Color32::from_rgb(220, 80, 60), e);
            } else if let Some(e) = &st.last_event
                && st.session.is_none()
            {
                ui.label(egui::RichText::new(e).weak());
            }
        }

        #[cfg(target_os = "macos")]
        {
            if self.perms.1.elapsed() > Duration::from_secs(2) {
                self.perms = (crate::platform::host_permissions(), Instant::now());
            }
            let (screen, access) = (self.perms.0.screen, self.perms.0.accessibility);
            if !screen || !access {
                ui.add_space(6.0);
                ui.label(egui::RichText::new("macOS needs two permissions before this Mac can be controlled:").strong());
            }
            if !screen {
                ui.horizontal(|ui| {
                    ui.label("✗ Screen Recording (to show the screen)");
                    if ui.button("Grant…").clicked() && !crate::platform::request_screen_permission() {
                        crate::platform::open_privacy_settings(false);
                    }
                });
            }
            if !access {
                ui.horizontal(|ui| {
                    ui.label("✗ Accessibility (for mouse and keyboard)");
                    if ui.button("Grant…").clicked() && !crate::platform::request_accessibility_permission() {
                        crate::platform::open_privacy_settings(true);
                    }
                });
            }
        }
    }

    fn peers_card(&mut self, ui: &mut egui::Ui) {
        ui.heading("Computers on your network");
        let peers = self.browser.list();
        let mut action = None;
        if peers.is_empty() {
            ui.label(egui::RichText::new("Looking… Open Hop on the other computer.").weak());
        }
        for p in &peers {
            ui.horizontal(|ui| {
                let icon = if p.beacon.os == "windows" { "🖥" } else { "" };
                ui.label(egui::RichText::new(format!("{icon} {}", p.beacon.name)).strong());
                ui.label(egui::RichText::new(format!("{} · {}", p.beacon.os, p.ip)).weak());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let busy = self.viewers.iter().any(|v| v.host_id == p.beacon.id);
                    if ui.add_enabled(!busy, egui::Button::new(if busy { "Connected" } else { "Connect" })).clicked() {
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
                ui.label(egui::RichText::new(&h.name).weak());
                ui.label(egui::RichText::new(format!("{} · {} (not seen)", h.os, h.addr)).weak());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("Forget").clicked() {
                        self.cfg.known_hosts.remove(&id);
                        self.cfg.save();
                    }
                    if ui.small_button("Try").clicked() {
                        action = Some((id.clone(), h.name.clone(), h.addr.clone(), h.os.clone()));
                    }
                });
            });
        }
        ui.horizontal(|ui| {
            ui.label("Or by IP:");
            let r = ui.add(egui::TextEdit::singleline(&mut self.manual_addr).hint_text("192.168.1.20").desired_width(140.0));
            let go = ui.button("Connect").clicked() || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
            if go && !self.manual_addr.trim().is_empty() {
                let addr = self.manual_addr.trim().to_string();
                let id = format!("ip:{addr}");
                action = Some((id, addr.clone(), addr, "unknown".into()));
            }
        });
        if let Some((id, name, addr, os)) = action {
            self.start_connect(id, name, addr, os);
        }
    }

    fn prompt_card(&mut self, ui: &mut egui::Ui) {
        let mut submit = None;
        let mut cancel = false;
        if let Prompt::Code { host_id, name, addr, os, code, error } = &mut self.prompt {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.label(egui::RichText::new(format!("Access code for {name}")).strong());
                ui.label("It's shown in the Hop window on that computer.");
                if let Some(e) = error {
                    ui.colored_label(egui::Color32::from_rgb(220, 80, 60), e.as_str());
                }
                ui.horizontal(|ui| {
                    let r = ui.add(egui::TextEdit::singleline(code).hint_text("ABC-123").desired_width(120.0).font(egui::TextStyle::Monospace));
                    r.request_focus();
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if (ui.button("Connect").clicked() || enter) && !code.trim().is_empty() {
                        submit = Some((host_id.clone(), name.clone(), addr.clone(), os.clone(), crate::crypto::normalize_code(code)));
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        }
        if let Some((id, name, addr, os, code)) = submit {
            self.prompt = Prompt::None;
            self.connect(id, name, addr, os, code);
        } else if cancel {
            self.prompt = Prompt::None;
        }
    }

    fn settings_card(&mut self, ui: &mut egui::Ui) {
        let s = &mut self.cfg.stream;
        let mut changed = false;
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
            changed |= ui.add(egui::Slider::new(&mut s.bitrate_mbps, 5..=150).suffix(" Mbps")).changed();
            ui.end_row();
            ui.label("Loss protection");
            changed |= ui.add(egui::Slider::new(&mut s.fec_percent, 0..=50).suffix(" %")).on_hover_text("Extra packets that repair Wi-Fi packet loss without a hiccup. Use 0 on Ethernet.").changed();
            ui.end_row();
            ui.label("");
            ui.vertical(|ui| {
                changed |= ui.checkbox(&mut s.audio, "Stream sound").changed();
                changed |= ui.checkbox(&mut s.clipboard, "Share clipboard (copied text)").changed();
                changed |= ui
                    .checkbox(&mut s.grab_keyboard, "Send system shortcuts (Cmd+Tab, Win key, Alt+Tab) to the other computer")
                    .changed();
                changed |= ui.checkbox(&mut s.fullscreen, "Open fullscreen").changed();
                changed |= ui.checkbox(&mut s.vsync, "V-Sync (smoother, adds latency)").changed();
                changed |= ui
                    .checkbox(&mut s.swap_ctrl_cmd, "Swap Ctrl and Cmd/Win between Mac and Windows")
                    .on_hover_text("So Cmd+C on a Mac keyboard does Ctrl+C on Windows, and Ctrl+C on a PC keyboard does Cmd+C on a Mac.")
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
        if std::mem::take(&mut self.start_minimized) {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
        self.poll_viewers();
        ui.ctx().request_repaint_after(Duration::from_millis(500));
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.add_space(4.0);
            if crate::install::needs_install() {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.label(egui::RichText::new("Hop isn't installed yet").strong());
                    ui.label("Install it so it has a Start menu entry, starts when you sign in, and is allowed through the firewall (Windows will ask for admin once).");
                    if ui.button("Install Hop").clicked() {
                        match crate::install::install() {
                            Ok(dest) => {
                                if let Some(h) = self.host.take() {
                                    h.stop();
                                }
                                std::thread::sleep(Duration::from_millis(300));
                                let _ = std::process::Command::new(dest).spawn();
                                std::process::exit(0);
                            }
                            Err(e) => self.notice = Some((format!("Install failed: {e:#}"), true)),
                        }
                    }
                });
                ui.add_space(6.0);
            }
            self.host_card(ui);
            ui.separator();
            if !matches!(self.prompt, Prompt::None) {
                self.prompt_card(ui);
                ui.add_space(6.0);
            }
            if let Some((msg, is_err)) = &self.notice {
                if *is_err {
                    ui.colored_label(egui::Color32::from_rgb(220, 80, 60), msg);
                } else {
                    ui.label(msg);
                }
            }
            self.peers_card(ui);
            ui.separator();
            let label = if self.show_settings { "▼ Stream settings" } else { "▶ Stream settings" };
            if ui.selectable_label(false, label).clicked() {
                self.show_settings = !self.show_settings;
            }
            if self.show_settings {
                self.settings_card(ui);
            }
            ui.separator();
            ui.label(egui::RichText::new("While connected (Mac: Ctrl+Option+Shift):").strong());
            ui.label("Ctrl+Alt+Shift+Q  disconnect\nCtrl+Alt+Shift+F  fullscreen\nCtrl+Alt+Shift+M  lock mouse for games\nCtrl+Alt+Shift+S  show/hide stats");
            if let Some(url) = &self.install_url {
                ui.separator();
                ui.label(egui::RichText::new("Install on a Windows PC").strong());
                ui.label("Open PowerShell on the PC and run:");
                let cmd = format!("iwr {url} | iex");
                ui.horizontal(|ui| {
                    ui.code(&cmd);
                    if ui.small_button("Copy").clicked() {
                        ui.ctx().copy_text(cmd.clone());
                    }
                });
            }
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
