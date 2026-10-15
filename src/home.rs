//! The home screen of the Hop window: this computer (hosting, access code, permissions), the
//! other computers on the network, and settings. Connecting turns the same window into the
//! remote screen (see app.rs).

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use eframe::egui::{self, RichText};
use parking_lot::Mutex;

use crate::config::{Config, KnownHost};
use crate::discovery::{self, Browser};
use crate::host::{self, Host, HostOptions};
use crate::proto::Hello;
use crate::theme::{self, ACCENT, GREEN, RED};
use crate::window::{StreamEnd, StreamSettings};

/// Things the home screen asks the app to do.
pub enum HomeAction {
    Connect { host_id: String, name: String, addr: String, code: String },
    CancelConnect,
    Quit,
}

enum Prompt {
    None,
    Code { host_id: String, name: String, addr: String, os: String, code: String, error: Option<String>, focused: bool },
}

/// Virtual display driver state (Windows).
#[cfg(windows)]
#[derive(Clone)]
enum VdState {
    NotInstalled,
    Installed,
    Busy(&'static str),
    Failed(String),
}

pub struct Home {
    pub cfg: Config,
    code: Arc<Mutex<String>>,
    host: Option<Host>,
    host_error: Option<String>,
    browser: Option<Browser>,
    prompt: Prompt,
    manual_addr: String,
    notice: Option<(String, bool)>,
    ips: Vec<String>,
    #[cfg(target_os = "macos")]
    perms: (crate::platform::Permissions, Instant),
    install_url: Option<String>,
    autostart: bool,
    /// Running from Downloads on Windows: wait for Install (or "use without installing").
    portable_pending: bool,
    renaming: Option<String>,
    last_client: Option<String>,
    copied_at: Option<Instant>,
    icon: Option<egui::TextureHandle>,
    #[cfg(windows)]
    vdisplay: Arc<Mutex<VdState>>,
    #[cfg(windows)]
    last_tidy: Instant,
    /// The computer being connected to (id, name), while connecting.
    pub connecting: Option<(String, String)>,
    /// Names of this computer's displays, for the fullscreen choice.
    pub displays: Vec<String>,
    pub actions: Vec<HomeAction>,
    /// Someone just connected to this computer: draw attention to the window.
    pub wants_attention: bool,
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

fn os_icon(os: &str) -> &str {
    if os == "macos" { "💻" } else { "🖥" }
}

impl Home {
    pub fn new() -> Home {
        let cfg = Config::load();
        let code = Arc::new(Mutex::new(cfg.access_code.clone()));
        let ips: Vec<String> = discovery::local_ips().iter().map(|i| i.to_string()).collect();
        let install_url = crate::selftest::start_installer_server().map(|_| {
            format!(
                "http://{}:{}/install.ps1",
                ips.first().cloned().unwrap_or_else(|| "this-mac".into()),
                crate::proto::INSTALL_PORT
            )
        });
        let portable_pending = crate::install::needs_install();
        let mut home = Home {
            cfg,
            code,
            host: None,
            host_error: None,
            browser: None,
            prompt: Prompt::None,
            manual_addr: String::new(),
            notice: None,
            ips,
            #[cfg(target_os = "macos")]
            perms: (crate::platform::host_permissions(), Instant::now()),
            install_url,
            autostart: crate::autostart::enabled(),
            portable_pending,
            renaming: None,
            last_client: None,
            copied_at: None,
            icon: None,
            #[cfg(windows)]
            vdisplay: Arc::new(Mutex::new(if crate::platform::vdisplay::is_installed() {
                VdState::Installed
            } else {
                VdState::NotInstalled
            })),
            #[cfg(windows)]
            last_tidy: Instant::now(),
            connecting: None,
            displays: Vec::new(),
            actions: Vec::new(),
            wants_attention: false,
        };
        // Opening ports from a not-yet-installed copy would make Windows Firewall ask about the
        // wrong file, so networking waits for Install.
        if !portable_pending {
            home.start_networking();
        }
        home
    }

    /// Hosting is on (closing the window then hides it instead of quitting).
    pub fn is_hosting(&self) -> bool {
        self.host.is_some()
    }

    /// Someone is controlling this computer.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn has_remote_viewer(&self) -> bool {
        self.host.as_ref().is_some_and(|h| h.status.lock().session.is_some())
    }

    pub fn shutdown(&mut self) {
        if let Some(h) = self.host.take() {
            h.stop();
        }
    }

    /// Settings for streaming from `addr`, from the saved preferences.
    pub fn stream_settings(&self, addr: &str, code: &str) -> StreamSettings {
        crate::stream_settings_from(&self.cfg, addr.to_string(), code.to_string(), false)
    }

    /// A connection started from here worked: remember the computer under its real identity.
    pub fn on_connected(&mut self, requested_id: &str, host: &Hello) {
        self.connecting = None;
        self.notice = None;
        if requested_id != host.id
            && let Some(mut k) = self.cfg.known_hosts.remove(requested_id)
        {
            k.name = host.name.clone();
            k.os = host.os.clone();
            self.cfg.known_hosts.insert(host.id.clone(), k);
        }
        if let Some(k) = self.cfg.known_hosts.get_mut(&host.id) {
            k.name = host.name.clone();
            k.os = host.os.clone();
        }
        self.cfg.save();
    }

    pub fn on_connect_failed(&mut self, host_id: &str, name: &str, wrong_code: bool, message: String) {
        self.connecting = None;
        if wrong_code {
            self.ask_code_again(host_id, name);
        } else {
            self.notice = Some((format!("{name}: {message}"), true));
        }
    }

    pub fn on_stream_ended(&mut self, host_id: &str, name: &str, end: StreamEnd) {
        match end {
            StreamEnd::Closed => self.notice = None,
            StreamEnd::WrongCode => self.ask_code_again(host_id, name),
            StreamEnd::Failed(m) => self.notice = Some((format!("{name}: {m}"), true)),
        }
    }

    fn ask_code_again(&mut self, host_id: &str, name: &str) {
        let known = self.cfg.known_hosts.get_mut(host_id).map(|h| {
            h.code.clear();
            (h.addr.clone(), h.os.clone())
        });
        self.cfg.save();
        if let Some((addr, os)) = known {
            self.prompt = Prompt::Code {
                host_id: host_id.to_string(),
                name: name.to_string(),
                addr,
                os,
                code: String::new(),
                error: Some(format!("That code didn't work. Check the code shown in Hop on {name}.")),
                focused: false,
            };
        }
    }

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
        self.cfg.known_hosts.insert(host_id.clone(), KnownHost { name: name.clone(), addr: addr.clone(), os, code: code.clone() });
        self.cfg.save();
        self.notice = None;
        self.connecting = Some((host_id.clone(), name.clone()));
        self.actions.push(HomeAction::Connect { host_id, name, addr, code });
    }

    fn start_connect(&mut self, host_id: String, name: String, addr: String, os: String) {
        if self.connecting.is_some() {
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

    // ----------------------------------------------------------------------------------------
    // UI

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        if self.icon.is_none()
            && let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon-256.png"))
        {
            let img = egui::ColorImage::from_rgba_unmultiplied([icon.width as usize, icon.height as usize], &icon.rgba);
            self.icon = Some(ui.ctx().load_texture("hop-icon", img, egui::TextureOptions::LINEAR));
        }
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            egui::Frame::new().inner_margin(egui::Margin::symmetric(18, 16)).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 12.0;
                self.header(ui);
                if self.portable_pending {
                    self.install_card(ui);
                }
                self.this_computer(ui);
                if let Some((msg, is_err)) = self.notice.clone() {
                    theme::card(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(msg).color(if is_err { RED } else { theme::weak(ui) }));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.small_button("Dismiss").clicked() {
                                    self.notice = None;
                                }
                            });
                        });
                    });
                }
                self.computers(ui);
                self.more(ui);
                self.footer(ui);
            });
        });
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some(icon) = &self.icon {
                ui.add(egui::Image::new(egui::load::SizedTexture::new(icon.id(), [44.0, 44.0])));
            }
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(theme::bold("Hop").size(24.0));
                ui.label(RichText::new("Remote desktop for your computers").small().color(theme::weak(ui)));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(h) = &self.host {
                    match h.status.lock().session.as_ref() {
                        Some(s) => theme::pill(ui, theme::green(ui), &format!("{} connected", s.client)),
                        None => theme::pill(ui, theme::green(ui), "Ready"),
                    }
                } else if self.portable_pending {
                    theme::pill(ui, theme::amber(ui), "Not installed");
                } else {
                    theme::pill(ui, theme::weak(ui), "Hosting off");
                }
            });
        });
    }

    fn install_card(&mut self, ui: &mut egui::Ui) {
        theme::card(ui, |ui| {
            ui.label(theme::bold("Install Hop on this PC").size(16.0));
            ui.label("Puts Hop in your Start menu, starts it when you sign in, and lets it through Windows Firewall (Windows asks for permission once).");
            ui.horizontal(|ui| {
                if theme::primary_button(ui, "Install Hop").clicked() {
                    match crate::install::install() {
                        Ok(dest) => {
                            self.shutdown();
                            std::thread::sleep(Duration::from_millis(300));
                            let _ = std::process::Command::new(dest).spawn();
                            std::process::exit(0);
                        }
                        Err(e) => self.notice = Some((format!("Install failed: {e:#}"), true)),
                    }
                }
                if ui.button("Use without installing").clicked() {
                    self.start_networking();
                }
            });
        });
    }

    fn this_computer(&mut self, ui: &mut egui::Ui) {
        theme::card(ui, |ui| {
            theme::section_title(ui, "This computer");
            // Name, editable.
            let mut rename_done = None;
            match &mut self.renaming {
                Some(buf) => {
                    ui.horizontal(|ui| {
                        let r = ui.add(egui::TextEdit::singleline(buf).desired_width(220.0).font(egui::TextStyle::Heading));
                        r.request_focus();
                        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if theme::primary_button(ui, "Save").clicked() || enter {
                            rename_done = Some(Some(buf.clone()));
                        }
                        if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                            rename_done = Some(None);
                        }
                    });
                }
                None => {
                    ui.horizontal(|ui| {
                        ui.label(theme::bold(&self.cfg.name).size(20.0));
                        if ui.small_button("✏ Rename").on_hover_text("The name your other computers see").clicked() {
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
                ui.label(RichText::new(self.ips.join(" · ")).small().color(theme::weak(ui)));
            }
            ui.separator();

            let mut on = self.cfg.hosting;
            ui.add_enabled_ui(!self.portable_pending, |ui| {
                if theme::toggle_row(ui, &mut on, "Let my other computers control this one", None) {
                    self.set_hosting(on);
                }
            });
            if self.cfg.hosting && !self.portable_pending {
                if let Some(e) = &self.host_error {
                    ui.colored_label(RED, e);
                }
                self.access_code(ui);
                self.host_status(ui);
                #[cfg(target_os = "macos")]
                self.permissions(ui);
                #[cfg(windows)]
                self.virtual_display(ui);
            }
            if crate::autostart::available() && !self.portable_pending {
                let mut on = self.autostart;
                if theme::toggle_row(ui, &mut on, "Start Hop when I sign in", None) {
                    match crate::autostart::set(on) {
                        Ok(()) => self.autostart = on,
                        Err(e) => self.notice = Some((format!("Couldn't change start-at-login: {e}"), true)),
                    }
                }
            }
        });
    }

    fn access_code(&mut self, ui: &mut egui::Ui) {
        let code = self.code.lock().clone();
        ui.horizontal(|ui| {
            egui::Frame::new()
                .fill(ACCENT.linear_multiply(0.12))
                .corner_radius(10)
                .inner_margin(egui::Margin::symmetric(14, 8))
                .show(ui, |ui| {
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.label(theme::bold("ACCESS CODE").size(10.5).color(ACCENT));
                        ui.label(RichText::new(pretty_code(&code)).monospace().size(26.0).strong());
                    });
                });
            ui.vertical(|ui| {
                let copied = self.copied_at.is_some_and(|t| t.elapsed() < Duration::from_secs(2));
                if ui.button(if copied { "✔ Copied" } else { "📋 Copy" }).clicked() {
                    ui.ctx().copy_text(pretty_code(&code));
                    self.copied_at = Some(Instant::now());
                }
                if ui.button("🔄 New code").on_hover_text("Your other computers will need to enter the new code once").clicked() {
                    let c = crate::crypto::random_code();
                    *self.code.lock() = c.clone();
                    self.cfg.access_code = c;
                    self.cfg.save();
                }
            });
        });
        ui.label(
            RichText::new("Enter this code on your other computer the first time it connects.")
                .small()
                .color(theme::weak(ui)),
        );
    }

    fn host_status(&mut self, ui: &mut egui::Ui) {
        let Some(h) = &self.host else { return };
        let st = h.status.lock().clone();
        match &st.session {
            Some(s) => {
                let secs = s.since.elapsed().as_secs_f64().max(0.001);
                let mbps = s.sent_bytes.load(Ordering::Relaxed) as f64 * 8.0 / secs / 1e6;
                let mut kick = false;
                egui::Frame::new().fill(GREEN.linear_multiply(0.12)).corner_radius(10).inner_margin(egui::Margin::same(10)).show(
                    ui,
                    |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.label(theme::bold(format!("{} is controlling this computer", s.client)).color(theme::green(ui)));
                                ui.label(
                                    RichText::new(format!("{}×{} · {} fps · {} · {:.1} Mbps", s.width, s.height, s.fps, s.encoder, mbps))
                                        .small()
                                        .color(theme::weak(ui)),
                                );
                            });
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                kick = ui.button("Disconnect").clicked();
                            });
                        });
                    },
                );
                if kick {
                    h.disconnect();
                }
                if self.last_client.as_deref() != Some(&s.client) {
                    self.wants_attention = true;
                    self.last_client = Some(s.client.clone());
                }
            }
            None => {
                self.last_client = None;
                if let Some(e) = &st.last_event {
                    ui.label(RichText::new(e).small().color(theme::weak(ui)));
                }
            }
        }
        if let Some(e) = &st.error {
            ui.colored_label(RED, e);
        }
    }

    #[cfg(target_os = "macos")]
    fn permissions(&mut self, ui: &mut egui::Ui) {
        if self.perms.1.elapsed() > Duration::from_secs(2) {
            self.perms = (crate::platform::host_permissions(), Instant::now());
        }
        let need_screen = !self.perms.0.screen;
        let need_access = !self.perms.0.accessibility;
        if !need_screen && !need_access {
            return;
        }
        egui::Frame::new().fill(theme::AMBER.linear_multiply(0.12)).corner_radius(10).inner_margin(egui::Margin::same(10)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(theme::bold("macOS needs your permission").color(theme::amber(ui)));
            let row = |ui: &mut egui::Ui, text: &str| ui.horizontal(|ui| {
                ui.label(text);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| ui.button("Allow…").clicked()).inner
            });
            if need_screen
                && row(ui, "Screen Recording, to show this screen").inner
                && !crate::platform::request_screen_permission()
            {
                crate::platform::open_privacy_settings(false);
            }
            if need_access
                && row(ui, "Accessibility, for remote mouse/keyboard and Cmd+Tab").inner
                && !crate::platform::request_accessibility_permission()
            {
                crate::platform::open_privacy_settings(true);
            }
            ui.label(RichText::new("After switching Hop on in System Settings, quit and reopen Hop.").small().color(theme::weak(ui)));
        });
    }

    #[cfg(windows)]
    fn virtual_display(&mut self, ui: &mut egui::Ui) {
        use crate::platform::vdisplay;
        // Keep the virtual display off the desktop while a real monitor is in use.
        if !self.has_remote_viewer() && self.last_tidy.elapsed() > Duration::from_secs(5) {
            self.last_tidy = Instant::now();
            if matches!(*self.vdisplay.lock(), VdState::Installed) {
                vdisplay::tidy();
            }
        }
        let start = |action: &'static str, state: Arc<Mutex<VdState>>| {
            *state.lock() = VdState::Busy(if action == "install" { "Setting up" } else { "Removing" });
            std::thread::spawn(move || {
                let r = vdisplay::run_setup(action);
                *state.lock() = match r {
                    Ok(()) if vdisplay::is_installed() => VdState::Installed,
                    Ok(()) => VdState::NotInstalled,
                    Err(e) => VdState::Failed(format!("{e:#}")),
                };
            });
        };
        let state = self.vdisplay.lock().clone();
        ui.separator();
        match &state {
            VdState::Installed => {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label("Virtual display");
                        ui.label(RichText::new("✔ Ready: used when this PC's monitor is off").small().color(theme::green(ui)));
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Remove").clicked() {
                            start("remove", self.vdisplay.clone());
                        }
                    });
                });
            }
            VdState::Busy(what) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!("{what}… approve the Windows prompts (admin, then Install)."));
                });
                ui.ctx().request_repaint_after(Duration::from_millis(250));
            }
            VdState::NotInstalled | VdState::Failed(_) => {
                ui.label("Virtual display");
                ui.label(
                    RichText::new(
                        "With the monitor off, Windows draws nothing, so there's nothing to stream. A virtual display fixes that. Installs the free, open-source Virtual Display Driver; Windows asks for permission, then whether to install the driver (click Install).",
                    )
                    .small()
                    .color(theme::weak(ui)),
                );
                if let VdState::Failed(e) = &state {
                    ui.colored_label(RED, e);
                }
                if ui.button("Set up virtual display").clicked() {
                    start("install", self.vdisplay.clone());
                }
            }
        }
    }

    fn computers(&mut self, ui: &mut egui::Ui) {
        theme::card(ui, |ui| {
            theme::section_title(ui, "Your computers");
            self.prompt_card(ui);
            let peers = self.browser.as_ref().map(|b| b.list()).unwrap_or_default();
            let mut action = None;
            if self.portable_pending {
                ui.label(RichText::new("Install Hop (or choose \"Use without installing\") to find your other computers.").color(theme::weak(ui)));
            } else if peers.is_empty() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Looking for computers running Hop…").color(theme::weak(ui)));
                });
            }
            for p in &peers {
                if let Some(a) = self.computer_row(ui, &p.beacon.id, &p.beacon.name, &p.beacon.os, &p.ip.to_string(), true) {
                    action = Some((a, p.addr()));
                }
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
                if let Some(a) = self.computer_row(ui, &id, &h.name, &h.os, &h.addr, false) {
                    action = Some((a, h.addr.clone()));
                }
            }
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(RichText::new("Not listed?").color(theme::weak(ui)));
                let r = ui.add(egui::TextEdit::singleline(&mut self.manual_addr).hint_text("IP address, e.g. 192.168.1.20").desired_width(200.0));
                let go = ui.button("Connect").clicked() || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                if go && !self.manual_addr.trim().is_empty() {
                    let addr = self.manual_addr.trim().to_string();
                    action = Some(((format!("ip:{addr}"), addr.clone(), String::new(), RowAction::Connect), addr));
                }
            });
            if let Some(((id, name, os, what), addr)) = action {
                match what {
                    RowAction::Connect => self.start_connect(id, name, addr, os),
                    RowAction::Forget => {
                        self.cfg.known_hosts.remove(&id);
                        self.cfg.save();
                    }
                    RowAction::Cancel => {
                        self.connecting = None;
                        self.actions.push(HomeAction::CancelConnect);
                    }
                }
            }
        });
    }

    /// One computer in the list. Returns (id, name, os, action) if a button was pressed.
    fn computer_row(
        &self,
        ui: &mut egui::Ui,
        id: &str,
        name: &str,
        os: &str,
        addr: &str,
        online: bool,
    ) -> Option<(String, String, String, RowAction)> {
        let mut out = None;
        let connecting = self.connecting.as_ref().is_some_and(|(cid, _)| cid == id);
        egui::Frame::new().fill(theme::background(ui.ctx()).linear_multiply(if ui.visuals().dark_mode { 1.0 } else { 0.6 })).corner_radius(10).inner_margin(egui::Margin::symmetric(12, 10)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(os_icon(os)).size(22.0));
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    let title = theme::bold(name);
                    ui.label(if online { title } else { title.color(theme::weak(ui)) });
                    let mut detail = format!("{} · {addr}", os_label(os));
                    if !online {
                        detail.push_str(" · not seen right now");
                    }
                    ui.label(RichText::new(detail.trim_start_matches(" · ")).small().color(theme::weak(ui)));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if connecting {
                        if ui.button("Cancel").clicked() {
                            out = Some((id.into(), name.into(), os.into(), RowAction::Cancel));
                        }
                        ui.spinner();
                        ui.label(RichText::new("Connecting…").color(theme::weak(ui)));
                    } else if online {
                        if theme::primary_button(ui, "Connect").clicked() {
                            out = Some((id.into(), name.into(), os.into(), RowAction::Connect));
                        }
                    } else {
                        if ui.small_button("Forget").on_hover_text("Also forgets its access code").clicked() {
                            out = Some((id.into(), name.into(), os.into(), RowAction::Forget));
                        }
                        if ui.small_button("Try").clicked() {
                            out = Some((id.into(), name.into(), os.into(), RowAction::Connect));
                        }
                    }
                });
            });
        });
        out
    }

    fn prompt_card(&mut self, ui: &mut egui::Ui) {
        let mut submit = None;
        let mut cancel = false;
        if let Prompt::Code { host_id, name, addr, os, code, error, focused } = &mut self.prompt {
            egui::Frame::new().fill(ACCENT.linear_multiply(0.10)).corner_radius(10).inner_margin(egui::Margin::same(12)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(theme::bold(format!("Connect to {name}")).size(16.0));
                ui.label(RichText::new(format!("Enter the access code shown in Hop on {name}. You only need to do this once.")).color(theme::weak(ui)));
                if let Some(e) = error {
                    ui.colored_label(RED, e.as_str());
                }
                ui.horizontal(|ui| {
                    let r = ui.add(
                        egui::TextEdit::singleline(code)
                            .hint_text("ABC-123")
                            .desired_width(130.0)
                            .font(egui::FontId::monospace(18.0))
                            .char_limit(12),
                    );
                    if !*focused {
                        r.request_focus();
                        *focused = true;
                    }
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let valid = crate::crypto::normalize_code(code).len() >= 4;
                    let clicked = ui.add_enabled_ui(valid, |ui| theme::primary_button(ui, "Connect")).inner.clicked();
                    if valid && (clicked || enter) {
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

    fn more(&mut self, ui: &mut egui::Ui) {
        theme::card(ui, |ui| {
            egui::CollapsingHeader::new(theme::bold("Stream settings"))
                .default_open(std::env::var_os("HOP_PREVIEW_OPEN").is_some())
                .show(ui, |ui| self.settings(ui));
            egui::CollapsingHeader::new(theme::bold("Keyboard shortcuts while connected")).show(ui, |ui| {
                let p = crate::window::shortcut_prefix();
                egui::Grid::new("keys").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
                    for (k, what) in [("Q", "disconnect"), ("F", "fullscreen on/off"), ("M", "lock or release the mouse"), ("S", "show/hide stats")] {
                        ui.label(RichText::new(format!("{p}+{k}")).monospace());
                        ui.label(what);
                        ui.end_row();
                    }
                });
            });
            if let Some(url) = &self.install_url {
                egui::CollapsingHeader::new(theme::bold("Install on a Windows PC")).show(ui, |ui| {
                    ui.label("Open PowerShell on the PC and run:");
                    let cmd = format!("iwr {url} | iex");
                    ui.horizontal(|ui| {
                        ui.code(&cmd);
                        if ui.small_button("📋 Copy").clicked() {
                            ui.ctx().copy_text(cmd.clone());
                        }
                    });
                    ui.label(RichText::new("Or download Hop.exe from github.com/linpatr/hop and click Install.").small().color(theme::weak(ui)));
                });
            }
        });
    }

    fn settings(&mut self, ui: &mut egui::Ui) {
        let displays = self.displays.clone();
        let s = &mut self.cfg.stream;
        let mut changed = false;
        ui.label(RichText::new("Changes apply the next time you connect.").small().color(theme::weak(ui)));
        egui::Grid::new("settings").num_columns(2).spacing([14.0, 8.0]).show(ui, |ui| {
            ui.label("When connected");
            let current = if !s.fullscreen {
                "In a window".to_string()
            } else if s.fullscreen_display.is_empty() {
                "Fullscreen".to_string()
            } else {
                format!("Fullscreen on {}", s.fullscreen_display)
            };
            egui::ComboBox::from_id_salt("display").selected_text(current).width(240.0).show_ui(ui, |ui| {
                if ui.selectable_label(!s.fullscreen, "In a window").clicked() {
                    s.fullscreen = false;
                    changed = true;
                }
                if ui.selectable_label(s.fullscreen && s.fullscreen_display.is_empty(), "Fullscreen on the display Hop is on").clicked() {
                    s.fullscreen = true;
                    s.fullscreen_display.clear();
                    changed = true;
                }
                for d in &displays {
                    if ui.selectable_label(s.fullscreen && &s.fullscreen_display == d, format!("Fullscreen on {d}")).clicked() {
                        s.fullscreen = true;
                        s.fullscreen_display = d.clone();
                        changed = true;
                    }
                }
            });
            ui.end_row();
            ui.label("Resolution");
            egui::ComboBox::from_id_salt("res")
                .selected_text(match s.max_height {
                    0 => "Match my screen".to_string(),
                    h => format!("Up to {h}p"),
                })
                .width(240.0)
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
        });
        ui.add_space(4.0);
        changed |= theme::toggle_row(ui, &mut s.audio, "Play the other computer's sound", None);
        changed |= theme::toggle_row(ui, &mut s.clipboard, "Share copied text both ways", None);
        changed |= theme::toggle_row(ui, &mut s.grab_keyboard, "Send Cmd+Tab / Win key / Alt+Tab across", None);
        let release = format!("For games that look around with the mouse. {}+M releases it.", crate::window::shortcut_prefix());
        changed |= theme::toggle_row(ui, &mut s.auto_mouse_lock, "Capture the mouse when a game hides the pointer", Some(&release));
        changed |= theme::toggle_row(
            ui,
            &mut s.swap_ctrl_cmd,
            "Swap Ctrl and Cmd between Mac and Windows",
            Some("Cmd+C on a Mac keyboard does Ctrl+C on Windows, and the other way round"),
        );
        changed |= theme::toggle_row(ui, &mut s.vsync, "V-Sync", Some("No tearing, a little more latency"));
        if changed {
            self.cfg.save();
        }
    }

    fn footer(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let mut t = format!("Hop {}", env!("CARGO_PKG_VERSION"));
            if self.host.is_some() {
                t.push_str(" · closing this window keeps Hop running so you can connect in");
            }
            ui.label(RichText::new(t).small().color(theme::weak(ui)));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Quit Hop").clicked() {
                    self.actions.push(HomeAction::Quit);
                }
            });
        });
    }
}

enum RowAction {
    Connect,
    Forget,
    Cancel,
}
