//! Virtual display support, so the PC can be streamed while its monitor is off.
//!
//! Windows only draws the desktop to displays that are on; with the monitor off (or unplugged)
//! there's nothing to capture. The free, open-source Virtual Display Driver
//! (github.com/VirtualDrivers/Virtual-Display-Driver, MIT, signed via SignPath) adds a virtual
//! monitor. Hop keeps it detached while a real monitor is in use; when the real one goes away,
//! Windows switches to the virtual one by itself, and Hop sets it to the viewer's resolution.
//!
//! Installing needs admin once. Everything else here works as a normal user.

use std::os::windows::process::CommandExt;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use windows::Win32::Graphics::Gdi::{
    CDS_NORESET, CDS_TYPE, CDS_UPDATEREGISTRY, ChangeDisplaySettingsExW, DEVMODEW, DISP_CHANGE_SUCCESSFUL,
    DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICEW, DM_DISPLAYFREQUENCY,
    DM_PELSHEIGHT, DM_PELSWIDTH, DM_POSITION, ENUM_DISPLAY_SETTINGS_MODE, EnumDisplayDevicesW, EnumDisplaySettingsW,
};
use windows::core::PCWSTR;

const NO_WINDOW: u32 = 0x0800_0000;

const DRIVER_URL: &str =
    "https://github.com/VirtualDrivers/Virtual-Display-Driver/releases/download/25.7.23/VirtualDisplayDriver-x86.Driver.Only.zip";
const DRIVER_SHA256: &str = "E24210692B442B39AF763536330CE78B423F19342B7A7792C26DE3944E418B3A";
const NEFCON_URL: &str = "https://github.com/nefarius/nefcon/releases/download/v1.14.0/nefcon_v1.14.0.zip";
const NEFCON_SHA256: &str = "A15557DA24A9EFCA203158DE3B43B0EAF982DB231F0194031F1ED428BC13E669";

#[derive(Clone, Debug)]
struct Display {
    /// e.g. \\.\DISPLAY3
    name: String,
    adapter: String,
    attached: bool,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide(w: &[u16]) -> String {
    String::from_utf16_lossy(&w[..w.iter().position(|&c| c == 0).unwrap_or(w.len())])
}

fn is_virtual_adapter(adapter: &str) -> bool {
    let a = adapter.to_lowercase();
    a.contains("virtual display driver") || a.contains("iddsampledriver") || a.contains("mttvdd")
}

fn displays() -> Vec<Display> {
    let mut v = Vec::new();
    for i in 0.. {
        let mut d = DISPLAY_DEVICEW { cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32, ..Default::default() };
        if !unsafe { EnumDisplayDevicesW(PCWSTR::null(), i, &mut d, 0) }.as_bool() {
            break;
        }
        v.push(Display {
            name: from_wide(&d.DeviceName),
            adapter: from_wide(&d.DeviceString),
            attached: d.StateFlags.0 & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP.0 != 0,
        });
    }
    v
}

fn virtual_display() -> Option<Display> {
    displays().into_iter().find(|d| is_virtual_adapter(&d.adapter))
}

/// The driver is installed (its display shows up, attached or not).
pub fn is_installed() -> bool {
    virtual_display().is_some()
}

/// A real monitor is part of the desktop.
fn physical_attached() -> bool {
    displays().iter().any(|d| d.attached && !is_virtual_adapter(&d.adapter))
}

fn apply(name: &str, dm: &DEVMODEW) -> Result<()> {
    let n = wide(name);
    unsafe {
        let r = ChangeDisplaySettingsExW(
            PCWSTR(n.as_ptr()),
            Some(dm),
            None,
            CDS_TYPE(CDS_UPDATEREGISTRY.0 | CDS_NORESET.0),
            None,
        );
        if r != DISP_CHANGE_SUCCESSFUL {
            bail!("display change for {name} failed ({})", r.0);
        }
        let r = ChangeDisplaySettingsExW(PCWSTR::null(), None, None, CDS_TYPE(0), None);
        if r != DISP_CHANGE_SUCCESSFUL {
            bail!("applying display changes failed ({})", r.0);
        }
    }
    Ok(())
}

/// Takes the virtual display off the desktop (it stays installed). Same as "Disconnect this
/// display" in Settings; no admin needed.
fn detach(d: &Display) -> Result<()> {
    let mut dm = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
    dm.dmFields = DM_POSITION | DM_PELSWIDTH | DM_PELSHEIGHT;
    dm.dmPelsWidth = 0;
    dm.dmPelsHeight = 0;
    apply(&d.name, &dm)
}

/// Keeps the virtual display out of the way while a real monitor is in use (it gets added to
/// the desktop when the driver is installed, and Windows remembers whatever we choose here).
pub fn tidy() {
    if let Some(v) = virtual_display()
        && v.attached
        && physical_attached()
    {
        match detach(&v) {
            Ok(()) => log::info!("detached the virtual display (a real monitor is in use)"),
            Err(e) => log::warn!("couldn't detach the virtual display: {e:#}"),
        }
    }
}

/// If the virtual display is what's being streamed (the real monitor is off), sets it to the
/// mode closest to the viewer's screen. Returns true if it changed anything.
pub fn prepare_for_stream(max: (u32, u32), fps: u32) -> bool {
    let Some(v) = virtual_display() else { return false };
    if !v.attached || physical_attached() {
        return false;
    }
    let n = wide(&v.name);
    let mut best: Option<DEVMODEW> = None;
    let mut best_score = i64::MIN;
    let mut current = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
    unsafe {
        let _ = EnumDisplaySettingsW(PCWSTR(n.as_ptr()), ENUM_DISPLAY_SETTINGS_MODE(u32::MAX), &mut current);
    }
    for i in 0.. {
        let mut dm = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
        if !unsafe { EnumDisplaySettingsW(PCWSTR(n.as_ptr()), ENUM_DISPLAY_SETTINGS_MODE(i), &mut dm) }.as_bool() {
            break;
        }
        let (w, h, hz) = (dm.dmPelsWidth, dm.dmPelsHeight, dm.dmDisplayFrequency);
        if w > max.0 || h > max.1 {
            continue;
        }
        // Biggest area that fits, then the refresh rate closest to (but not under) the stream's.
        let hz_score = if hz >= fps { 1000 - (hz - fps) as i64 } else { hz as i64 - 1000 };
        let score = (w as i64 * h as i64) * 10_000 + hz_score;
        if score > best_score {
            best_score = score;
            best = Some(dm);
        }
    }
    let Some(mut dm) = best else { return false };
    if dm.dmPelsWidth == current.dmPelsWidth
        && dm.dmPelsHeight == current.dmPelsHeight
        && dm.dmDisplayFrequency == current.dmDisplayFrequency
    {
        return false;
    }
    dm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;
    match apply(&v.name, &dm) {
        Ok(()) => {
            log::info!(
                "virtual display set to {}x{} @ {} Hz for the stream",
                dm.dmPelsWidth,
                dm.dmPelsHeight,
                dm.dmDisplayFrequency
            );
            true
        }
        Err(e) => {
            log::warn!("couldn't set the virtual display mode: {e:#}");
            false
        }
    }
}

fn script(action: &str) -> String {
    format!(
        r#"$ErrorActionPreference = 'Stop'
$result = Join-Path $env:TEMP 'hop-vdd.result'
Remove-Item $result -ErrorAction SilentlyContinue
try {{
    $ProgressPreference = 'SilentlyContinue'
    $tmp = Join-Path $env:TEMP 'HopVDD'
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Path $tmp | Out-Null
    function Get-Checked($url, $sha, $out) {{
        Invoke-WebRequest -Uri $url -OutFile $out -UseBasicParsing
        if ((Get-FileHash $out -Algorithm SHA256).Hash -ne $sha) {{ throw "download didn't match its checksum: $url" }}
    }}
    Get-Checked '{NEFCON_URL}' '{NEFCON_SHA256}' "$tmp\nefcon.zip"
    Expand-Archive "$tmp\nefcon.zip" -DestinationPath "$tmp\nefcon" -Force
    $nefcon = "$tmp\nefcon\x64\nefconc.exe"
    if ('{action}' -eq 'remove') {{
        & $nefcon --remove-device-node --hardware-id 'Root\MttVDD' --class-guid '4D36E968-E325-11CE-BFC1-08002BE10318'
        if ($LASTEXITCODE -ne 0) {{ throw "removing the driver failed (nefcon exit $LASTEXITCODE)" }}
    }} else {{
        Get-Checked '{DRIVER_URL}' '{DRIVER_SHA256}' "$tmp\vdd.zip"
        Expand-Archive "$tmp\vdd.zip" -DestinationPath "$tmp\vdd" -Force
        $drv = "$tmp\vdd\VirtualDisplayDriver"
        New-Item -ItemType Directory -Path 'C:\VirtualDisplayDriver' -Force | Out-Null
        if (-not (Test-Path 'C:\VirtualDisplayDriver\vdd_settings.xml')) {{
            Copy-Item "$drv\vdd_settings.xml" 'C:\VirtualDisplayDriver\vdd_settings.xml'
        }}
        # Trust the driver's signer (SignPath), as the project's own installer does, so Windows
        # doesn't stop to ask.
        $certs = New-Object System.Security.Cryptography.X509Certificates.X509Certificate2Collection
        $certs.Import([IO.File]::ReadAllBytes("$drv\mttvdd.cat"))
        $store = New-Object System.Security.Cryptography.X509Certificates.X509Store('TrustedPublisher', 'LocalMachine')
        $store.Open('ReadWrite')
        foreach ($c in $certs) {{ $store.Add($c) }}
        $store.Close()
        & $nefcon install "$drv\MttVDD.inf" 'Root\MttVDD'
        if ($LASTEXITCODE -ne 0) {{ throw "installing the driver failed (nefcon exit $LASTEXITCODE)" }}
    }}
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
    'OK' | Set-Content $result
}} catch {{
    ($_ | Out-String) | Set-Content $result
    exit 1
}}
"#
    )
}

/// Installs ("install") or removes ("remove") the driver. Shows one admin prompt and blocks
/// until it's done.
pub fn run_setup(action: &str) -> Result<()> {
    let temp = std::env::temp_dir();
    let ps1 = temp.join(format!("hop-vdd-{action}.ps1"));
    std::fs::write(&ps1, script(action)).context("writing the setup script")?;
    let result = temp.join("hop-vdd.result");
    let _ = std::fs::remove_file(&result);
    let elevate = format!(
        "Start-Process powershell -Verb RunAs -Wait -WindowStyle Hidden -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File','{}'",
        ps1.display().to_string().replace('\'', "''")
    );
    let status = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-Command", &elevate])
        .creation_flags(NO_WINDOW)
        .status()
        .context("starting PowerShell")?;
    let _ = std::fs::remove_file(&ps1);
    let outcome = std::fs::read_to_string(&result).unwrap_or_default();
    let _ = std::fs::remove_file(&result);
    if outcome.trim() == "OK" {
        log::info!("virtual display driver {action} finished");
    } else if outcome.trim().is_empty() {
        bail!(if status.success() { "setup was cancelled" } else { "setup didn't run (was the admin prompt declined?)" });
    } else {
        return Err(anyhow!("{}", outcome.trim()));
    }
    if action == "install" {
        // The driver adds its display to the desktop; take it off again while the real
        // monitor is in use.
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline && !is_installed() {
            std::thread::sleep(Duration::from_millis(500));
        }
        if !is_installed() {
            bail!("the driver installed, but its display hasn't appeared yet; restarting Windows may help");
        }
        std::thread::sleep(Duration::from_secs(2));
        tidy();
    }
    Ok(())
}
