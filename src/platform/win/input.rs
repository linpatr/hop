//! Keyboard and mouse injection with SendInput. Keys go in as scancodes, which games read directly.

use std::collections::HashSet;

use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN,
};

use crate::keymap;
use crate::platform::Injector;
use crate::proto::Input;

pub struct WinInjector {
    pressed: HashSet<u16>,
    buttons: [bool; 5],
}

impl WinInjector {
    pub fn new() -> Self {
        Self { pressed: HashSet::new(), buttons: [false; 5] }
    }
}

fn send(inputs: &[INPUT]) {
    unsafe {
        SendInput(inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

fn key_input(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

fn mouse_input(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

fn key_event(hid: u16, down: bool) {
    let Some(sc) = keymap::hid_to_windows_scancode(hid) else { return };
    let up = if down { KEYBD_EVENT_FLAGS(0) } else { KEYEVENTF_KEYUP };
    let i = match sc {
        // Pause and Num Lock don't round-trip through scancodes reliably; use virtual keys.
        0xE11D => key_input(VK_PAUSE, 0, up),
        0x45 => key_input(VK_NUMLOCK, 0x45, up | KEYEVENTF_EXTENDEDKEY),
        _ => {
            let ext = if sc & 0xFF00 == 0xE000 { KEYEVENTF_EXTENDEDKEY } else { KEYBD_EVENT_FLAGS(0) };
            key_input(VIRTUAL_KEY(0), sc & 0xFF, KEYEVENTF_SCANCODE | ext | up)
        }
    };
    send(&[i]);
}

impl Injector for WinInjector {
    fn input(&mut self, i: Input) {
        match i {
            Input::Key { hid, down, .. } => {
                if down {
                    self.pressed.insert(hid);
                } else {
                    self.pressed.remove(&hid);
                }
                key_event(hid, down);
            }
            Input::MouseAbs { x, y } => unsafe {
                // The streamed display is the primary monitor, at (0,0) of the virtual desktop.
                let (pw, ph) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
                let (vx, vy) = (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN));
                let (vw, vh) = (GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2), GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2));
                let px = x as i64 * (pw as i64 - 1) / 65535;
                let py = y as i64 * (ph as i64 - 1) / 65535;
                let nx = ((px - vx as i64) * 65535 / (vw as i64 - 1)) as i32;
                let ny = ((py - vy as i64) * 65535 / (vh as i64 - 1)) as i32;
                send(&[mouse_input(nx, ny, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK)]);
            },
            Input::MouseRel { dx, dy } => send(&[mouse_input(dx as i32, dy as i32, 0, MOUSEEVENTF_MOVE)]),
            Input::Button { button, down } => {
                let idx = button as usize - 1;
                if idx >= 5 {
                    return;
                }
                self.buttons[idx] = down;
                let (flags, data) = match (button, down) {
                    (1, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                    (1, false) => (MOUSEEVENTF_LEFTUP, 0),
                    (2, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                    (2, false) => (MOUSEEVENTF_RIGHTUP, 0),
                    (3, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                    (3, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                    (4, d) => (if d { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, 1),
                    (_, d) => (if d { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, 2),
                };
                send(&[mouse_input(0, 0, data, flags)]);
            }
            Input::Wheel { dx, dy, .. } => {
                if dy != 0 {
                    send(&[mouse_input(0, 0, dy as i32, MOUSEEVENTF_WHEEL)]);
                }
                if dx != 0 {
                    send(&[mouse_input(0, 0, dx as i32, MOUSEEVENTF_HWHEEL)]);
                }
            }
            Input::ReleaseAll => {
                for hid in std::mem::take(&mut self.pressed) {
                    key_event(hid, false);
                }
                for b in 0..5 {
                    if self.buttons[b] {
                        self.input(Input::Button { button: b as u8 + 1, down: false });
                    }
                }
            }
        }
    }
}

impl Drop for WinInjector {
    fn drop(&mut self) {
        self.input(Input::ReleaseAll);
    }
}
