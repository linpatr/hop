//! Wire protocol: ports, control messages and input events.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

const DISCOVERY_PORT: u16 = 47120;
const CONTROL_PORT: u16 = 47121;
const MEDIA_PORT: u16 = 47122;
pub const INSTALL_PORT: u16 = 47123;

/// HOP_PORT_OFFSET shifts every port, so a test copy can run beside the real one.
fn port_offset() -> u16 {
    static OFFSET: std::sync::OnceLock<u16> = std::sync::OnceLock::new();
    *OFFSET.get_or_init(|| std::env::var("HOP_PORT_OFFSET").ok().and_then(|v| v.parse().ok()).unwrap_or(0))
}

pub fn discovery_port() -> u16 {
    DISCOVERY_PORT + port_offset()
}

pub fn control_port() -> u16 {
    CONTROL_PORT + port_offset()
}

pub fn media_port() -> u16 {
    MEDIA_PORT + port_offset()
}
pub const PROTOCOL_VERSION: u32 = 2;

/// Plaintext hello exchanged before the key exchange.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Hello {
    pub hop: u32,
    pub id: String,
    pub name: String,
    pub os: String,
    /// Set by a host that refuses the connection (e.g. after too many wrong codes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Encrypted JSON control messages (both directions).
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Ctrl {
    /// Host -> client, first encrypted message.
    Welcome { native_width: u32, native_height: u32 },
    /// Client -> host: start streaming with these limits.
    Start {
        max_width: u32,
        max_height: u32,
        fps: u32,
        bitrate_kbps: u32,
        audio: bool,
        fec_percent: u32,
        #[serde(default)]
        clipboard: bool,
    },
    /// Host -> client: actual stream parameters.
    StreamInfo { width: u32, height: u32, fps: u32, encoder: String },
    /// Host -> client: streaming failed to start.
    Error { message: String },
    RequestKeyframe,
    SetBitrate { kbps: u32 },
    Ping { ts: u64 },
    Pong { ts: u64 },
    Clipboard { text: String },
    /// Client -> host: `local` = the viewer draws the mouse pointer itself (desktop use), so the
    /// host leaves it out of the video and sends pointer shapes instead. Off while the mouse is
    /// locked for games.
    CursorMode { local: bool },
    /// Host -> client: something the user should know while waiting (empty clears it).
    Notice { message: String },
    /// Client -> host: send the end of your log (for troubleshooting from the other computer).
    LogRequest,
    Log { text: String },
    Bye,
}

/// A mouse pointer image (straight-alpha RGBA) and its hotspot.
#[derive(Clone, Debug, PartialEq)]
pub struct CursorShape {
    pub w: u16,
    pub h: u16,
    pub hot_x: u16,
    pub hot_y: u16,
    pub rgba: Vec<u8>,
}

/// Host -> client pointer update: visibility, plus the new shape when it changed.
#[derive(Clone, Debug)]
pub struct CursorMsg {
    pub visible: bool,
    pub shape: Option<CursorShape>,
}

/// Input events, client -> host. Key codes are USB HID usages (keyboard page 0x07).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Input {
    Key { hid: u16, down: bool, repeat: bool },
    /// Absolute position normalized to 0..=65535 over the streamed display.
    MouseAbs { x: u16, y: u16 },
    MouseRel { dx: i16, dy: i16 },
    /// 1 = left, 2 = right, 3 = middle, 4 = back (X1), 5 = forward (X2)
    Button { button: u8, down: bool },
    /// Wheel units: 120 = one notch. Positive y = scroll up, positive x = scroll right.
    Wheel { dx: i16, dy: i16, precise: bool },
    ReleaseAll,
}

pub const MSG_CTRL: u8 = 0;
pub const MSG_INPUT: u8 = 1;
pub const MSG_CURSOR: u8 = 2;

pub fn encode_cursor(visible: bool, shape: Option<&CursorShape>) -> Vec<u8> {
    let mut v = vec![MSG_CURSOR, visible as u8];
    if let Some(c) = shape {
        for x in [c.w, c.h, c.hot_x, c.hot_y] {
            v.extend_from_slice(&x.to_le_bytes());
        }
        v.extend_from_slice(&c.rgba);
    }
    v
}

fn decode_cursor(b: &[u8]) -> Result<CursorMsg> {
    let Some(&vis) = b.first() else { bail!("short cursor message") };
    if b.len() < 9 {
        return Ok(CursorMsg { visible: vis != 0, shape: None });
    }
    let u = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    let (w, h) = (u(1), u(3));
    let rgba = b[9..].to_vec();
    if rgba.len() != w as usize * h as usize * 4 || w == 0 || h == 0 {
        bail!("bad cursor image");
    }
    Ok(CursorMsg { visible: vis != 0, shape: Some(CursorShape { w, h, hot_x: u(5), hot_y: u(7), rgba }) })
}

pub fn encode_ctrl(c: &Ctrl) -> Vec<u8> {
    let mut v = vec![MSG_CTRL];
    serde_json::to_writer(&mut v, c).expect("serialize ctrl");
    v
}

pub fn encode_input(i: &Input) -> Vec<u8> {
    let mut v = Vec::with_capacity(8);
    v.push(MSG_INPUT);
    match *i {
        Input::Key { hid, down, repeat } => {
            v.push(1);
            v.extend_from_slice(&hid.to_le_bytes());
            v.push(down as u8 | (repeat as u8) << 1);
        }
        Input::MouseAbs { x, y } => {
            v.push(2);
            v.extend_from_slice(&x.to_le_bytes());
            v.extend_from_slice(&y.to_le_bytes());
        }
        Input::MouseRel { dx, dy } => {
            v.push(3);
            v.extend_from_slice(&dx.to_le_bytes());
            v.extend_from_slice(&dy.to_le_bytes());
        }
        Input::Button { button, down } => {
            v.push(4);
            v.push(button);
            v.push(down as u8);
        }
        Input::Wheel { dx, dy, precise } => {
            v.push(5);
            v.extend_from_slice(&dx.to_le_bytes());
            v.extend_from_slice(&dy.to_le_bytes());
            v.push(precise as u8);
        }
        Input::ReleaseAll => v.push(6),
    }
    v
}

pub enum Message {
    Ctrl(Ctrl),
    Input(Input),
    Cursor(CursorMsg),
}

pub fn decode_message(b: &[u8]) -> Result<Message> {
    let Some((&kind, rest)) = b.split_first() else { bail!("empty message") };
    match kind {
        MSG_CTRL => Ok(Message::Ctrl(serde_json::from_slice(rest)?)),
        MSG_INPUT => Ok(Message::Input(decode_input(rest)?)),
        MSG_CURSOR => Ok(Message::Cursor(decode_cursor(rest)?)),
        _ => bail!("unknown message kind {kind}"),
    }
}

fn decode_input(b: &[u8]) -> Result<Input> {
    let u16_at = |i: usize| -> Result<u16> {
        Ok(u16::from_le_bytes(b.get(i..i + 2).ok_or_else(|| anyhow::anyhow!("short input"))?.try_into()?))
    };
    let byte = |i: usize| -> Result<u8> { b.get(i).copied().ok_or_else(|| anyhow::anyhow!("short input")) };
    Ok(match byte(0)? {
        1 => {
            let f = byte(3)?;
            Input::Key { hid: u16_at(1)?, down: f & 1 != 0, repeat: f & 2 != 0 }
        }
        2 => Input::MouseAbs { x: u16_at(1)?, y: u16_at(3)? },
        3 => Input::MouseRel { dx: u16_at(1)? as i16, dy: u16_at(3)? as i16 },
        4 => Input::Button { button: byte(1)?, down: byte(2)? != 0 },
        5 => Input::Wheel { dx: u16_at(1)? as i16, dy: u16_at(3)? as i16, precise: byte(5)? != 0 },
        6 => Input::ReleaseAll,
        t => bail!("unknown input type {t}"),
    })
}

pub fn os_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(windows) {
        "windows"
    } else {
        "linux"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_roundtrip() {
        let shape = CursorShape { w: 2, h: 3, hot_x: 1, hot_y: 2, rgba: (0..24).collect() };
        match decode_message(&encode_cursor(true, Some(&shape))).unwrap() {
            Message::Cursor(c) => {
                assert!(c.visible);
                assert_eq!(c.shape, Some(shape));
            }
            _ => panic!(),
        }
        match decode_message(&encode_cursor(false, None)).unwrap() {
            Message::Cursor(c) => assert!(!c.visible && c.shape.is_none()),
            _ => panic!(),
        }
    }

    #[test]
    fn input_roundtrip() {
        let cases = [
            Input::Key { hid: 0xE3, down: true, repeat: false },
            Input::Key { hid: 0x04, down: false, repeat: true },
            Input::MouseAbs { x: 65535, y: 12 },
            Input::MouseRel { dx: -300, dy: 7 },
            Input::Button { button: 5, down: true },
            Input::Wheel { dx: -120, dy: 360, precise: true },
            Input::ReleaseAll,
        ];
        for c in cases {
            match decode_message(&encode_input(&c)).unwrap() {
                Message::Input(d) => assert_eq!(c, d),
                _ => panic!(),
            }
        }
    }
}
