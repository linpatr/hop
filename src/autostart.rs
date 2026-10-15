//! "Start at login": a LaunchAgent on macOS, the HKCU Run key on Windows.

#[cfg(target_os = "macos")]
mod imp {
    use std::path::PathBuf;

    fn plist() -> Option<PathBuf> {
        Some(dirs::home_dir()?.join("Library/LaunchAgents/com.patricklin.hop.plist"))
    }

    fn app_bundle() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let app = exe.parent()?.parent()?.parent()?;
        (app.extension()? == "app").then(|| app.to_path_buf())
    }

    pub fn enabled() -> bool {
        plist().is_some_and(|p| p.exists())
    }

    pub fn available() -> bool {
        app_bundle().is_some()
    }

    pub fn set(on: bool) -> std::io::Result<()> {
        let path = plist().ok_or_else(|| std::io::Error::other("no home folder"))?;
        if !on {
            return match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        let app = app_bundle().ok_or_else(|| std::io::Error::other("not running from Hop.app"))?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(
            &path,
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.patricklin.hop</string>
  <key>ProgramArguments</key>
  <array><string>/usr/bin/open</string><string>-g</string><string>-a</string><string>{}</string><string>--args</string><string>--minimized</string></array>
  <key>RunAtLoad</key><true/>
</dict>
</plist>
"#,
                app.display()
            ),
        )
    }
}

#[cfg(windows)]
mod imp {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    const KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    const NO_WINDOW: u32 = 0x0800_0000;

    fn reg(args: &[&str]) -> bool {
        Command::new("reg").args(args).creation_flags(NO_WINDOW).output().map(|o| o.status.success()).unwrap_or(false)
    }

    pub fn enabled() -> bool {
        reg(&["query", KEY, "/v", "Hop"])
    }

    pub fn available() -> bool {
        true
    }

    pub fn set(on: bool) -> std::io::Result<()> {
        let ok = if on {
            let exe = std::env::current_exe()?;
            let value = format!("\"{}\" --minimized", exe.display());
            reg(&["add", KEY, "/v", "Hop", "/t", "REG_SZ", "/d", &value, "/f"])
        } else {
            reg(&["delete", KEY, "/v", "Hop", "/f"]) || !enabled()
        };
        if ok { Ok(()) } else { Err(std::io::Error::other("registry update failed")) }
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod imp {
    pub fn enabled() -> bool {
        false
    }
    pub fn available() -> bool {
        false
    }
    pub fn set(_on: bool) -> std::io::Result<()> {
        Ok(())
    }
}

pub use imp::{available, enabled, set};
