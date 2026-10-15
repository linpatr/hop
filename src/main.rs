#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod audio;
mod autostart;
mod client;
mod clipboard;
mod config;
mod convert;
mod crypto;
mod discovery;
mod host;
mod install;
mod keymap;
mod home;
mod logging;
mod media;
mod platform;
mod proto;
mod render;
mod selftest;
mod theme;
mod window;

use std::sync::Arc;

use config::Config;

const USAGE: &str = "Hop: low-latency remote desktop for your LAN

  hop                       open the Hop window (hosts this computer and lists others)
  hop host                  host this computer without a window
  hop connect <ip> [opts]   view and control another computer
      --code CODE           its access code (or set HOP_CODE)
      --fullscreen          start fullscreen
      --fps N               frames per second (default 60)
      --mbps N              bitrate in Mbps (default 40)
      --max-height N        cap the stream height, e.g. 1080
      --no-audio            don't stream sound
      --view-only           watch without sending keyboard/mouse
      --no-grab             let Cmd+Tab / Win / Alt+Tab act on this computer
      --no-clipboard        don't share the clipboard
  hop selftest              check capture, encoding and decoding on this computer
";

fn main() {
    platform::platform_init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("host") => {
            logging::init("hop-host.log");
            run_host()
        }
        Some("connect") => {
            logging::init("hop-viewer.log");
            run_connect(&args[1..])
        }
        // Run by the installer with admin rights (one Windows prompt naming Hop).
        Some("admin-firewall") => install::admin_firewall(args.get(1).map(String::as_str).unwrap_or_default()),
        // Development: draw the home screen into a PNG (use HOP_CONFIG_DIR for made-up data).
        Some("render-home") => {
            logging::init("hop-render.log");
            let out = args.get(1).cloned().unwrap_or_else(|| "home.png".into());
            let dark = !args.iter().any(|a| a == "--light");
            let mut home = home::Home::new();
            home.displays = vec!["Built-in Display".into(), "LG UltraFine".into()];
            let height = if std::env::var_os("HOP_PREVIEW_OPEN").is_some() { 1500.0 } else { 900.0 };
            let r = render::render_offscreen((540.0, height), 2.0, dark, |ui| home.ui(ui), std::path::Path::new(&out));
            home.shutdown();
            match r {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("{e:#}");
                    1
                }
            }
        }
        Some("selftest") => {
            logging::init("hop-selftest.log");
            selftest::run()
        }
        Some("-h" | "--help" | "help") => {
            print!("{USAGE}");
            0
        }
        Some("--version") => {
            println!("hop {}", env!("CARGO_PKG_VERSION"));
            0
        }
        _ => {
            logging::init("hop.log");
            app::run()
        }
    };
    std::process::exit(code);
}

fn run_host() -> i32 {
    let cfg = Config::load();
    let opts = host::HostOptions {
        id: cfg.id.clone(),
        name: cfg.name.clone(),
        code: Arc::new(parking_lot::Mutex::new(cfg.access_code.clone())),
    };
    match host::start_host(opts) {
        Ok(_host) => {
            println!("Hosting \"{}\" — access code {}", cfg.name, cfg.access_code);
            for ip in discovery::local_ips() {
                println!("  reachable at {ip}");
            }
            loop {
                std::thread::park();
            }
        }
        Err(e) => {
            eprintln!("can't host: {e:#}");
            1
        }
    }
}

fn run_connect(args: &[String]) -> i32 {
    let mut cfg = Config::load();
    let mut addr = None;
    let mut code = std::env::var("HOP_CODE").ok();
    let mut p = cfg.stream.clone();
    let mut view_only = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let num = |it: &mut std::slice::Iter<String>| it.next().and_then(|v| v.parse::<u32>().ok());
        match a.as_str() {
            "--code" => code = it.next().cloned(),
            "--fullscreen" => p.fullscreen = true,
            "--windowed" => p.fullscreen = false,
            "--fps" => p.fps = num(&mut it).unwrap_or(p.fps),
            "--mbps" => p.bitrate_mbps = num(&mut it).unwrap_or(p.bitrate_mbps),
            "--max-height" => p.max_height = num(&mut it).unwrap_or(p.max_height),
            "--no-audio" => p.audio = false,
            "--vsync" => p.vsync = true,
            "--view-only" => view_only = true,
            "--no-grab" => p.grab_keyboard = false,
            "--no-clipboard" => p.clipboard = false,
            s if !s.starts_with('-') => addr = Some(s.to_string()),
            s => {
                eprintln!("unknown option {s}\n\n{USAGE}");
                return 1;
            }
        }
    }
    let Some(addr) = addr else {
        eprint!("{USAGE}");
        return 1;
    };
    // Fall back to a code remembered for this address.
    let code = code
        .or_else(|| cfg.known_hosts.values().find(|h| h.addr == addr || h.addr.starts_with(&format!("{addr}:"))).map(|h| h.code.clone()))
        .unwrap_or_default();
    cfg.stream = p.clone();
    window::run(stream_settings_from(&cfg, addr, code, view_only))
}

/// Stream settings from the saved preferences.
pub(crate) fn stream_settings_from(cfg: &Config, addr: String, code: String, view_only: bool) -> window::StreamSettings {
    let p = &cfg.stream;
    window::StreamSettings {
        connect: client::ConnectOptions {
            addr,
            code,
            id: cfg.id.clone(),
            name: cfg.name.clone(),
            max_width: 0,
            max_height: 0,
            fps: p.fps,
            bitrate_kbps: p.bitrate_mbps * 1000,
            audio: p.audio,
            fec_percent: p.fec_percent,
            clipboard: p.clipboard && !view_only,
        },
        fullscreen: p.fullscreen,
        fullscreen_display: p.fullscreen_display.clone(),
        vsync: p.vsync,
        swap_ctrl_cmd: p.swap_ctrl_cmd,
        max_height_pref: p.max_height,
        view_only,
        grab_keyboard: p.grab_keyboard && !view_only,
    }
}
