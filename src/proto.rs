//! Wire protocol: ports, control messages and input events.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

pub const DISCOVERY_PORT: u16 = 47120;
pub const CONTROL_PORT: u16 = 47121;
pub const MEDIA_PORT: u16 = 47122;
pub const INSTALL_PORT: u16 = 47123;
pub const PROTOCOL_VERSION: u32 = 1;

/// Plaintext hello exchanged before the key exchange.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Hello {
    pub hop: u32,
    pub id: String,
    pub name: String,
    pub os: String,
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
    Bye,
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
}

pub fn decode_message(b: &[u8]) -> Result<Message> {
    let Some((&kind, rest)) = b.split_first() else { bail!("empty message") };
    match kind {
        MSG_CTRL => Ok(Message::Ctrl(serde_json::from_slice(rest)?)),
        MSG_INPUT => Ok(Message::Input(decode_input(rest)?)),
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
