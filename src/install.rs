//! Windows self-install: lets a Hop.exe downloaded from GitHub put itself in
//! %LOCALAPPDATA%\Programs\Hop with a Start menu shortcut, sign-in entry and firewall rule.

#[cfg(windows)]
mod imp {
    use std::os::windows::process::CommandExt;
    use std::path::PathBuf;
    use std::process::Command;

    use anyhow::{Context, Result};

    use crate::platform::shell;

    const NO_WINDOW: u32 = 0x0800_0000;

    pub fn install_dir() -> Option<PathBuf> {
        Some(PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("Programs").join("Hop"))
    }

    /// True when running from somewhere other than the install folder (e.g. Downloads).
    pub fn needs_install() -> bool {
        let (Ok(exe), Some(dir)) = (std::env::current_exe(), install_dir()) else { return false };
        let exe_dir = exe.parent().map(|p| p.to_string_lossy().to_lowercase());
        exe_dir != Some(dir.to_string_lossy().to_lowercase())
    }

    /// Copies this exe into place and sets it up. Returns the installed path.
    pub fn install() -> Result<PathBuf> {
        let dir = install_dir().context("LOCALAPPDATA not set")?;
        std::fs::create_dir_all(&dir)?;
        let dest = dir.join("Hop.exe");
        let me = std::env::current_exe()?;
        // Updating: close the installed copy (not this one) so it can be replaced.
        if dest.exists() {
            shell::kill_processes_at(&dest);
        }
        let mut copied = Err(std::io::Error::other("not tried"));
        for _ in 0..20 {
            copied = std::fs::copy(&me, &dest);
            if copied.is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        copied.context("couldn't replace the installed Hop.exe (quit Hop and try again)")?;

        if let Some(appdata) = std::env::var_os("APPDATA") {
            let lnk = PathBuf::from(appdata).join(r"Microsoft\Windows\Start Menu\Programs\Hop.lnk");
            if let Err(e) = shell::create_shortcut(&lnk, &dest, &dir, "Hop remote desktop") {
                log::warn!("couldn't create the Start menu shortcut: {e:#}");
            }
        }
        if let Err(e) = shell::set_run_key("Hop", Some(&format!("\"{}\" --minimized", dest.display()))) {
            log::warn!("couldn't set start at sign-in: {e:#}");
        }
        // One admin prompt (for Hop itself) to allow it through Windows Firewall. Fine if
        // declined: Windows then asks the first time Hop listens.
        let firewall = shell::run_elevated(&dest, &format!("admin-firewall \"{}\"", dest.display()));
        log::info!("installed to {} (firewall rule: {:?})", dest.display(), firewall);
        Ok(dest)
    }

    /// `hop admin-firewall <exe>`, run with admin rights: allow `exe` through Windows Firewall.
    pub fn admin_firewall(exe: &str) -> i32 {
        let netsh = |args: &[&str]| {
            Command::new("netsh")
                .args(["advfirewall", "firewall"])
                .args(args)
                .creation_flags(NO_WINDOW)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        let _ = netsh(&["delete", "rule", "name=Hop"]);
        let program = format!("program={exe}");
        let ok = netsh(&["add", "rule", "name=Hop", "dir=in", "action=allow", &program, "enable=yes", "profile=any"]);
        if ok { 0 } else { 1 }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn needs_install() -> bool {
        false
    }
    pub fn install() -> anyhow::Result<std::path::PathBuf> {
        anyhow::bail!("only needed on Windows")
    }
    pub fn admin_firewall(_exe: &str) -> i32 {
        1
    }
}

pub use imp::{admin_firewall, install, needs_install};
