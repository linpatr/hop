//! Virtual display support, so the PC can be streamed while its monitor is off.
//!
//! Windows only draws the desktop to displays that are on; with the monitor off (or unplugged)
//! there's nothing to capture. The free, open-source Virtual Display Driver
//! (github.com/VirtualDrivers/Virtual-Display-Driver, MIT, signed via SignPath) adds a virtual
//! monitor. Hop keeps it detached while a real monitor is in use; when the real one goes away,
//! Windows switches to the virtual one by itself, and Hop sets it to the viewer's resolution.
//!
//! Installing needs admin once (Windows asks, then asks whether to install the driver).
//! Everything else here works as a normal user.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use windows::Win32::Graphics::Gdi::{
    CDS_NORESET, CDS_TYPE, CDS_UPDATEREGISTRY, ChangeDisplaySettingsExW, DEVMODEW, DISP_CHANGE_SUCCESSFUL,
    DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICEW, DM_DISPLAYFREQUENCY,
    DM_PELSHEIGHT, DM_PELSWIDTH, DM_POSITION, ENUM_DISPLAY_SETTINGS_MODE, EnumDisplayDevicesW, EnumDisplaySettingsW,
};
use windows::core::PCWSTR;

use super::shell;

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

fn sha256_hex(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let data = std::fs::read(path)?;
    Ok(Sha256::digest(&data).iter().map(|b| format!("{b:02X}")).collect())
}

/// Downloads a file and checks it against a fixed SHA-256, so a tampered download is refused.
fn fetch_checked(url: &str, sha256: &str, dest: &Path) -> Result<()> {
    shell::download(url, dest)?;
    let got = sha256_hex(dest)?;
    if got != sha256 {
        let _ = std::fs::remove_file(dest);
        bail!("the download from {url} didn't match its expected checksum, so it wasn't used");
    }
    Ok(())
}

fn extract(zip_path: &Path, name: &str, dest: &Path) -> Result<()> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(zip_path)?)?;
    let mut f = zip.by_name(name).with_context(|| format!("{name} missing from {}", zip_path.display()))?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::io::copy(&mut f, &mut std::fs::File::create(dest)?)?;
    Ok(())
}

/// Installs ("install") or removes ("remove") the driver, using the signed device installer
/// nefcon. Windows shows an admin prompt, and when installing, asks whether to install the
/// driver. Blocks until done.
pub fn run_setup(action: &str) -> Result<()> {
    let work = std::env::temp_dir().join("HopVDD");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;
    let result = setup_in(&work, action);
    let _ = std::fs::remove_dir_all(&work);
    result
}

fn setup_in(work: &Path, action: &str) -> Result<()> {
    let nefcon_zip = work.join("nefcon.zip");
    fetch_checked(NEFCON_URL, NEFCON_SHA256, &nefcon_zip)?;
    let nefconc = work.join("nefconc.exe");
    extract(&nefcon_zip, "x64/nefconc.exe", &nefconc)?;

    if action == "remove" {
        let code = shell::run_elevated(
            &nefconc,
            r"--remove-device-node --hardware-id Root\MttVDD --class-guid 4D36E968-E325-11CE-BFC1-08002BE10318",
        )?;
        if code != 0 {
            bail!("removing the driver failed (nefcon exit code {code})");
        }
        log::info!("virtual display driver removed");
        return Ok(());
    }

    let driver_zip = work.join("vdd.zip");
    fetch_checked(DRIVER_URL, DRIVER_SHA256, &driver_zip)?;
    let drv = work.join("VirtualDisplayDriver");
    for f in ["MttVDD.inf", "MttVDD.dll", "mttvdd.cat", "vdd_settings.xml"] {
        extract(&driver_zip, &format!("VirtualDisplayDriver/{f}"), &drv.join(f))?;
    }
    // The driver reads its settings (resolutions etc.) from here. Users may create folders in
    // C:\, so this doesn't need admin.
    let conf = Path::new(r"C:\VirtualDisplayDriver");
    std::fs::create_dir_all(conf).context(r"couldn't create C:\VirtualDisplayDriver")?;
    if !conf.join("vdd_settings.xml").exists() {
        std::fs::copy(drv.join("vdd_settings.xml"), conf.join("vdd_settings.xml"))
            .context("couldn't write the virtual display settings")?;
    }
    let inf = drv.join("MttVDD.inf");
    let code = shell::run_elevated(&nefconc, &format!("install \"{}\" Root\\MttVDD", inf.display()))?;
    if code != 0 && code != 3010 {
        bail!("installing the driver failed (nefcon exit code {code}); if Windows asked whether to install it, click Install");
    }
    log::info!("virtual display driver installed (exit code {code})");

    // The driver adds its display to the desktop; take it off again while the real monitor is
    // in use.
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && !is_installed() {
        std::thread::sleep(Duration::from_millis(500));
    }
    if !is_installed() {
        bail!("the driver installed, but its display hasn't appeared yet; restarting Windows may help");
    }
    std::thread::sleep(Duration::from_secs(2));
    tidy();
    Ok(())
}
