//! Small Windows helpers for installing: shortcuts, the Run key, elevation, downloads. All done
//! with plain Win32 calls (no PowerShell), so what Hop does stays visible and simple.

use std::path::Path;

use anyhow::{Result, anyhow, bail};
use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, WAIT_OBJECT_0};
use windows::Win32::System::Com::Urlmon::URLDownloadToFileW;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IPersistFile};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, GetExitCodeProcess, INFINITE, OpenProcess, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess,
    WaitForSingleObject,
};
use windows::Win32::UI::Shell::{IShellLinkW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW, ShellLink};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
use windows::core::{Interface, PCWSTR, PWSTR};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

pub fn run_key_exists(name: &str) -> bool {
    let (k, n) = (wide(RUN_KEY), wide(name));
    let r =
        unsafe { RegGetValueW(HKEY_CURRENT_USER, PCWSTR(k.as_ptr()), PCWSTR(n.as_ptr()), RRF_RT_REG_SZ, None, None, None) };
    r == ERROR_SUCCESS
}

/// Sets (or with `None`, removes) a "start when I sign in" entry for the current user.
pub fn set_run_key(name: &str, command: Option<&str>) -> Result<()> {
    let (k, n) = (wide(RUN_KEY), wide(name));
    let r = unsafe {
        match command {
            Some(c) => {
                let v = wide(c);
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    PCWSTR(k.as_ptr()),
                    PCWSTR(n.as_ptr()),
                    REG_SZ.0,
                    Some(v.as_ptr() as *const _),
                    (v.len() * 2) as u32,
                )
            }
            None => {
                let r = RegDeleteKeyValueW(HKEY_CURRENT_USER, PCWSTR(k.as_ptr()), PCWSTR(n.as_ptr()));
                if run_key_exists(name) { r } else { ERROR_SUCCESS }
            }
        }
    };
    if r != ERROR_SUCCESS {
        bail!("registry update failed ({})", r.0);
    }
    Ok(())
}

/// Creates a .lnk shortcut.
pub fn create_shortcut(lnk: &Path, target: &Path, workdir: &Path, description: &str) -> Result<()> {
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        let (t, w, d) = (wide(&target.to_string_lossy()), wide(&workdir.to_string_lossy()), wide(description));
        link.SetPath(PCWSTR(t.as_ptr()))?;
        link.SetWorkingDirectory(PCWSTR(w.as_ptr()))?;
        link.SetDescription(PCWSTR(d.as_ptr()))?;
        let file: IPersistFile = link.cast()?;
        let l = wide(&lnk.to_string_lossy());
        file.Save(PCWSTR(l.as_ptr()), true)?;
    }
    Ok(())
}

/// Ends other processes running the given executable (not this one).
pub fn kill_processes_at(exe: &Path) {
    let want = exe.to_string_lossy().to_lowercase();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let me = GetCurrentProcessId();
        let mut ok = Process32FirstW(snap, &mut e).is_ok();
        while ok {
            if e.th32ProcessID != me
                && let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE, false, e.th32ProcessID)
            {
                let mut buf = [0u16; 1024];
                let mut len = buf.len() as u32;
                if QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok()
                    && String::from_utf16_lossy(&buf[..len as usize]).to_lowercase() == want
                {
                    let _ = TerminateProcess(h, 0);
                    let _ = WaitForSingleObject(h, 3000);
                }
                let _ = CloseHandle(h);
            }
            ok = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
}

/// Runs a program with admin rights (Windows shows its usual prompt) and waits for it.
/// Returns its exit code.
pub fn run_elevated(exe: &Path, params: &str) -> Result<u32> {
    let (verb, file, args) = (wide("runas"), wide(&exe.to_string_lossy()), wide(params));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(args.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    unsafe {
        ShellExecuteExW(&mut info).map_err(|e| {
            if e.code().0 as u32 == 0x800704C7 {
                anyhow!("the admin prompt was declined")
            } else {
                anyhow!("couldn't start {}: {e}", exe.display())
            }
        })?;
        if info.hProcess.is_invalid() {
            bail!("couldn't start {}", exe.display());
        }
        let w = WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 1u32;
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
        if w != WAIT_OBJECT_0 {
            bail!("waiting for {} failed", exe.display());
        }
        Ok(code)
    }
}

pub fn download(url: &str, dest: &Path) -> Result<()> {
    let (u, d) = (wide(url), wide(&dest.to_string_lossy()));
    unsafe { URLDownloadToFileW(None, PCWSTR(u.as_ptr()), PCWSTR(d.as_ptr()), 0, None) }
        .map_err(|e| anyhow!("download failed ({url}): {e}"))
}
