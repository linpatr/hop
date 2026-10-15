//! Keyboard grab for the viewer: a low-level hook that sends Win, Alt+Tab, Alt+F4 etc. to the
//! remote computer instead of acting locally while the viewer is in front.

use std::sync::OnceLock;

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_NUMLOCK, VK_PAUSE};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetForegroundWindow, GetWindowThreadProcessId, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_EXTENDED,
    LLKHF_INJECTED, LLKHF_UP, SetWindowsHookExW, WH_KEYBOARD_LL,
};

use crate::keymap;

pub type KeyGrabFn = Box<dyn Fn(u16, bool, bool) + Send + Sync>;

static CALLBACK: OnceLock<KeyGrabFn> = OnceLock::new();
static DOWN: std::sync::Mutex<[bool; 256]> = std::sync::Mutex::new([false; 256]);

fn we_are_in_front() -> bool {
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut pid));
        pid == GetCurrentProcessId()
    }
}

unsafe extern "system" fn hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && we_are_in_front() {
        let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        if kb.flags.0 & LLKHF_INJECTED.0 == 0
            && let Some(cb) = CALLBACK.get()
        {
            let down = kb.flags.0 & LLKHF_UP.0 == 0;
            let hid = if kb.vkCode == VK_PAUSE.0 as u32 {
                Some(0x48)
            } else if kb.vkCode == VK_NUMLOCK.0 as u32 {
                Some(0x53)
            } else {
                let ext = if kb.flags.0 & LLKHF_EXTENDED.0 != 0 { 0xE000 } else { 0 };
                keymap::windows_scancode_to_hid(kb.scanCode as u16 | ext)
            };
            if let Some(hid) = hid {
                let repeat = {
                    let mut d = DOWN.lock().unwrap();
                    let i = (hid & 0xFF) as usize;
                    let was = d[i];
                    d[i] = down;
                    down && was
                };
                cb(hid, down, repeat);
                return LRESULT(1);
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Must be called on a thread that pumps window messages (the viewer's event loop thread).
pub fn start_key_grab(f: KeyGrabFn) -> bool {
    if CALLBACK.set(f).is_err() {
        return true;
    }
    unsafe {
        let module = GetModuleHandleW(None).ok();
        SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), module.map(|m| m.into()), 0).is_ok()
    }
}
