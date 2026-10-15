//! Settings persisted as JSON in the per-user config folder.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct StreamPrefs {
    /// 0 = match the client's screen, otherwise a cap on the streamed height (e.g. 1080).
    pub max_height: u32,
    pub fps: u32,
    pub bitrate_mbps: u32,
    pub fec_percent: u32,
    pub audio: bool,
    pub fullscreen: bool,
    pub vsync: bool,
    /// Swap Ctrl and Cmd/Win when the two computers run different OSes.
    pub swap_ctrl_cmd: bool,
    /// Send system shortcuts (Cmd+Tab, Win key, Alt+Tab) to the remote computer.
    pub grab_keyboard: bool,
    /// Share copied text both ways.
    pub clipboard: bool,
}

impl Default for StreamPrefs {
    fn default() -> Self {
        Self {
            max_height: 0,
            fps: 60,
            bitrate_mbps: 40,
            fec_percent: 15,
            audio: true,
            fullscreen: false,
            vsync: false,
            swap_ctrl_cmd: false,
            grab_keyboard: true,
            clipboard: true,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct KnownHost {
    pub name: String,
    pub addr: String,
    pub os: String,
    pub code: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    pub id: String,
    pub name: String,
    pub access_code: String,
    pub hosting: bool,
    pub known_hosts: HashMap<String, KnownHost>,
    pub stream: StreamPrefs,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            access_code: String::new(),
            hosting: true,
            known_hosts: HashMap::new(),
            stream: StreamPrefs::default(),
        }
    }
}

pub fn dir() -> PathBuf {
    let d = dirs::config_dir().unwrap_or_else(std::env::temp_dir).join("Hop");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn path() -> PathBuf {
    dir().join("config.json")
}

impl Config {
    pub fn load() -> Config {
        let mut c: Config =
            std::fs::read(path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let mut changed = false;
        if c.id.is_empty() {
            use rand::Rng;
            c.id = format!("{:016x}", rand::thread_rng().r#gen::<u64>());
            changed = true;
        }
        if c.access_code.is_empty() {
            c.access_code = crate::crypto::random_code();
            changed = true;
        }
        if c.name.is_empty() {
            c.name = default_name();
        }
        if changed {
            c.save();
        }
        c
    }

    pub fn save(&self) {
        let tmp = path().with_extension("json.tmp");
        if let Ok(b) = serde_json::to_vec_pretty(self)
            && std::fs::write(&tmp, b).is_ok()
        {
            let _ = std::fs::rename(&tmp, path());
        }
    }
}

pub fn default_name() -> String {
    let h = gethostname::gethostname().to_string_lossy().into_owned();
    let h = h.trim_end_matches(".local").trim_end_matches(".lan").to_string();
    if h.is_empty() { "Computer".into() } else { h }
}
