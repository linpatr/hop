//! Windows self-install: lets a Hop.exe downloaded from GitHub put itself in
//! %LOCALAPPDATA%\Programs\Hop with a Start menu shortcut, sign-in entry and firewall rule.

#[cfg(windows)]
mod imp {
    use std::os::windows::process::CommandExt;
    use std::path::PathBuf;
    use std::process::Command;

    use anyhow::{Context, Result};

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

    fn powershell(script: &str) -> bool {
        Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-Command", script])
            .creation_flags(NO_WINDOW)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Copies this exe into place and sets it up. Returns the installed path.
    pub fn install() -> Result<PathBuf> {
        let dir = install_dir().context("LOCALAPPDATA not set")?;
        std::fs::create_dir_all(&dir)?;
        let dest = dir.join("Hop.exe");
        let me = std::env::current_exe()?;
        std::fs::copy(&me, &dest).context("couldn't copy Hop.exe (is an installed Hop still running? quit it first)")?;
        // Drop the downloaded-from-the-internet mark so SmartScreen doesn't ask again.
        let _ = std::fs::remove_file(format!("{}:Zone.Identifier", dest.display()));
        let (d, dd) = (dest.display().to_string(), dir.display().to_string());
        powershell(&format!(
            "$s=(New-Object -ComObject WScript.Shell).CreateShortcut([Environment]::GetFolderPath('Programs')+'\\Hop.lnk');\
             $s.TargetPath='{d}';$s.WorkingDirectory='{dd}';$s.Description='Hop remote desktop';$s.Save()"
        ));
        let _ = Command::new("reg")
            .args([
                "add",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                "/v",
                "Hop",
                "/t",
                "REG_SZ",
                "/d",
                &format!("\"{d}\" --minimized"),
                "/f",
            ])
            .creation_flags(NO_WINDOW)
            .status();
        // One admin prompt for the firewall rule; fine if declined (Windows asks on first run instead).
        let firewall = powershell(&format!(
            "Start-Process cmd -Verb RunAs -WindowStyle Hidden -Wait -ArgumentList '/c netsh advfirewall firewall delete rule name=Hop & netsh advfirewall firewall add rule name=Hop dir=in action=allow program=\"{d}\" enable=yes profile=any'"
        ));
        log::info!("installed to {d} (firewall rule: {firewall})");
        Ok(dest)
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
}

pub use imp::{install, needs_install};
