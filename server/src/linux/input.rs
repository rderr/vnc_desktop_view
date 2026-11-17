//! Replays the viewer's mouse and keyboard on X11 with the XTEST extension.
//!
//! Keys arrive as PC scan codes (set 1, 0xE0xx = extended), as on Windows.
//! X keycodes on today's X servers (evdev / libinput) are Linux input event
//! codes + 8, and those equal the set 1 codes for the main block of keys; the
//! rest is mapped below. The remote's keyboard layout decides the character.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, KEY_PRESS_EVENT, KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT, Window};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

/// One wheel notch in the viewer's (Windows) units.
const WHEEL_NOTCH: i32 = 120;

pub struct Input {
    /// Desktop size (width << 32 | height), so positions can be clamped to the
    /// screen. Shared with the capture loop, which updates it on resolution changes.
    size: Arc<AtomicU64>,
    x: Option<(RustConnection, Window)>,
    /// Browser `buttons` mask last sent (1 = left, 2 = right, 4 = middle).
    buttons: u8,
    /// Wheel movement not yet sent: X scrolls in whole notches (buttons 4-7).
    wheel: (i32, i32),
    warned: bool,
}

impl Input {
    pub fn new(size: Arc<AtomicU64>) -> Self {
        Self { size, x: None, buttons: 0, wheel: (0, 0), warned: false }
    }

    /// Payload: u16 x, u16 y, u8 buttons, i16 wheel_y, i16 wheel_x (Windows wheel units, + = up/right).
    pub fn pointer(&mut self, p: &[u8]) {
        if p.len() < 9 {
            return;
        }
        let size = self.size.load(Ordering::Relaxed);
        let (width, height) = ((size >> 32) as i32, size as u32 as i32);
        if width == 0 || height == 0 {
            return; // not capturing yet
        }
        let x = (u16::from_le_bytes([p[0], p[1]]) as i32).clamp(0, width - 1) as i16;
        let y = (u16::from_le_bytes([p[2], p[3]]) as i32).clamp(0, height - 1) as i16;
        let buttons = p[4];
        self.wheel.0 += i16::from_le_bytes([p[5], p[6]]) as i32;
        self.wheel.1 += i16::from_le_bytes([p[7], p[8]]) as i32;

        let mut events = vec![(MOTION_NOTIFY_EVENT, 0u8, x, y)];
        // Browser bit -> X button: left 1, middle 2, right 3.
        for (bit, button) in [(1u8, 1u8), (2, 3), (4, 2)] {
            let (was, is) = (self.buttons & bit != 0, buttons & bit != 0);
            if was != is {
                events.push((if is { BUTTON_PRESS_EVENT } else { BUTTON_RELEASE_EVENT }, button, 0, 0));
            }
        }
        self.buttons = buttons;
        // Wheel: button 4 up, 5 down, 6 left, 7 right, one click per notch.
        for (amount, positive, negative) in [(&mut self.wheel.0, 4u8, 5u8), (&mut self.wheel.1, 7, 6)] {
            while amount.abs() >= WHEEL_NOTCH {
                let button = if *amount > 0 { positive } else { negative };
                events.push((BUTTON_PRESS_EVENT, button, 0, 0));
                events.push((BUTTON_RELEASE_EVENT, button, 0, 0));
                *amount -= WHEEL_NOTCH * amount.signum();
            }
        }
        self.send(&events);
    }

    /// Payload: u16 scan code (set 1; 0xE0xx = extended), u8 down.
    pub fn key(&mut self, p: &[u8]) {
        if p.len() < 3 {
            return;
        }
        let Some(code) = evdev_code(u16::from_le_bytes([p[0], p[1]])) else { return };
        let kind = if p[2] != 0 { KEY_PRESS_EVENT } else { KEY_RELEASE_EVENT };
        self.send(&[(kind, (code + 8) as u8, 0, 0)]);
    }

    /// Sends XTEST events (type, detail, x, y), connecting to the X server first if needed.
    fn send(&mut self, events: &[(u8, u8, i16, i16)]) {
        if self.x.is_none() {
            match connect() {
                Ok(x) => self.x = Some(x),
                Err(e) => {
                    if !std::mem::replace(&mut self.warned, true) {
                        eprintln!("desktop-server: input unavailable: {e:#}");
                    }
                    return;
                }
            }
        }
        let (conn, root) = self.x.as_ref().unwrap();
        let result = (|| -> anyhow::Result<()> {
            for &(kind, detail, x, y) in events {
                conn.xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, *root, x, y, 0)?;
            }
            conn.flush()?;
            Ok(())
        })();
        if let Err(e) = result {
            eprintln!("desktop-server: sending input failed ({e:#}); reconnecting");
            self.x = None;
        }
    }
}

fn connect() -> anyhow::Result<(RustConnection, Window)> {
    let (conn, screen) = RustConnection::connect(None)?;
    conn.xtest_get_version(2, 2)?.reply().map_err(|e| anyhow::anyhow!("the X server lacks XTEST ({e})"))?;
    let root = conn.setup().roots[screen].root;
    Ok((conn, root))
}

/// PC scan code (set 1) -> Linux input event code (linux/input-event-codes.h).
fn evdev_code(scan: u16) -> Option<u16> {
    Some(match scan {
        0x01..=0x58 => scan, // Escape .. F12: the same numbers
        0x59 => 117,         // keypad =
        0x73 => 89,          // Japanese ろ (Ro)
        0x7d => 124,         // Japanese ¥ (Yen)
        0xe01c => 96,        // keypad Enter
        0xe01d => 97,        // right Ctrl
        0xe035 => 98,        // keypad /
        0xe037 => 99,        // Print Screen
        0xe038 => 100,       // right Alt (AltGr)
        0xe047 => 102,       // Home
        0xe048 => 103,       // Up
        0xe049 => 104,       // Page Up
        0xe04b => 105,       // Left
        0xe04d => 106,       // Right
        0xe04f => 107,       // End
        0xe050 => 108,       // Down
        0xe051 => 109,       // Page Down
        0xe052 => 110,       // Insert
        0xe053 => 111,       // Delete
        0xe05b => 125,       // left Super (Windows / ⌘ key)
        0xe05c => 126,       // right Super
        0xe05d => 127,       // Menu
        _ => return None,
    })
}
