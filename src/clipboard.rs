//! Shared clipboard (text): each side watches its clipboard and sends changes to the other.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::Mutex;

const MAX_TEXT: usize = 1 << 20;

/// The text we last saw or set, so a remote update isn't echoed straight back.
#[derive(Clone, Default)]
pub struct ClipState {
    last: Arc<Mutex<String>>,
}

impl ClipState {
    pub fn new() -> Self {
        let s = ClipState::default();
        if let Ok(mut c) = arboard::Clipboard::new() {
            *s.last.lock() = c.get_text().unwrap_or_default();
        }
        s
    }

    /// Puts text from the other computer on this clipboard.
    pub fn apply(&self, text: String) {
        if text.len() > MAX_TEXT {
            return;
        }
        *self.last.lock() = text.clone();
        match arboard::Clipboard::new().and_then(|mut c| c.set_text(text)) {
            Ok(()) => log::debug!("clipboard updated from remote"),
            Err(e) => log::warn!("can't set clipboard: {e}"),
        }
    }

    /// Polls the clipboard until `stop`, calling `send` with new text.
    pub fn watch(&self, stop: Arc<AtomicBool>, send: impl Fn(String) + Send + 'static) {
        let last = self.last.clone();
        std::thread::Builder::new()
            .name("clipboard".into())
            .spawn(move || {
                let Ok(mut cb) = arboard::Clipboard::new() else { return };
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(400));
                    let Ok(t) = cb.get_text() else { continue };
                    if t.is_empty() || t.len() > MAX_TEXT {
                        continue;
                    }
                    let mut l = last.lock();
                    if *l != t {
                        *l = t.clone();
                        drop(l);
                        send(t);
                    }
                }
            })
            .ok();
    }
}
