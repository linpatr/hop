//! Logs to stderr and to a file in the config folder (Windows GUI builds have no console).

use std::fs::File;
use std::io::Write;
use std::sync::Mutex;
use std::time::SystemTime;

struct Logger {
    file: Mutex<Option<File>>,
    level: log::LevelFilter,
}

impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= self.level && (m.target().starts_with("hop") || m.level() <= log::Level::Warn)
    }

    fn log(&self, r: &log::Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let t = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
        let secs = t.as_secs() % 86400;
        let line = format!(
            "{:02}:{:02}:{:02}.{:03} {:5} [{}] {}\n",
            secs / 3600,
            secs / 60 % 60,
            secs % 60,
            t.subsec_millis(),
            r.level(),
            std::process::id(),
            r.args()
        );
        let _ = std::io::stderr().write_all(line.as_bytes());
        if let Some(f) = self.file.lock().unwrap().as_mut() {
            let _ = f.write_all(line.as_bytes());
        }
    }

    fn flush(&self) {}
}

pub fn init(file_name: &str) {
    let path = crate::config::dir().join(file_name);
    // Keep the log from growing forever.
    if std::fs::metadata(&path).map(|m| m.len() > 5_000_000).unwrap_or(false) {
        let _ = std::fs::remove_file(&path);
    }
    let file = std::fs::OpenOptions::new().create(true).append(true).open(&path).ok();
    let level = match std::env::var("HOP_LOG").as_deref() {
        Ok("debug") => log::LevelFilter::Debug,
        Ok("trace") => log::LevelFilter::Trace,
        _ => log::LevelFilter::Info,
    };
    let _ = log::set_boxed_logger(Box::new(Logger { file: Mutex::new(file), level }));
    log::set_max_level(level);
}

pub fn path(file_name: &str) -> std::path::PathBuf {
    crate::config::dir().join(file_name)
}

/// The last `max` bytes of this computer's logs (the app's and the headless host's).
pub fn tail(max: usize) -> String {
    let mut out = String::new();
    for name in ["hop.log", "hop-host.log"] {
        let Ok(b) = std::fs::read(path(name)) else { continue };
        let start = b.len().saturating_sub(max / 2);
        out.push_str(&format!("===== {name} =====\n"));
        out.push_str(&String::from_utf8_lossy(&b[start..]));
    }
    out
}
