//! Replays the viewer's mouse and keyboard on macOS with Core Graphics events.
//! Needs the Accessibility permission (System Settings > Privacy & Security).
//!
//! Keys arrive as PC scan codes (set 1, 0xE0xx = extended) and are mapped to
//! Mac key codes; the Mac's keyboard layout decides the character. The viewer
//! sends ⌘ as the Windows key (E05B) to a Mac, which becomes ⌘ here. Modifier
//! state is tracked and attached to every event, so ⌘C and ⇧-click work.

use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::{
    CGDisplayBounds, CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGEventType,
    CGMainDisplayID, CGMouseButton, CGScrollEventUnit,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

// CGEventType values.
const LEFT_DOWN: u32 = 1;
const LEFT_UP: u32 = 2;
const RIGHT_DOWN: u32 = 3;
const RIGHT_UP: u32 = 4;
const MOVED: u32 = 5;
const LEFT_DRAGGED: u32 = 6;
const RIGHT_DRAGGED: u32 = 7;
const FLAGS_CHANGED: u32 = 12;
const OTHER_DOWN: u32 = 25;
const OTHER_UP: u32 = 26;
const OTHER_DRAGGED: u32 = 27;

/// Two clicks this close in time and space make a double-click.
const DOUBLE_CLICK_TIME: Duration = Duration::from_millis(500);
const DOUBLE_CLICK_DISTANCE: f64 = 4.0;

pub struct Input {
    /// Size of the frames the viewer sees (width << 32 | height): its
    /// coordinates are in these pixels, mapped here to the display's points.
    size: Arc<AtomicU64>,
    source: Option<CFRetained<CGEventSource>>,
    /// Browser `buttons` mask last sent (1 = left, 2 = right, 4 = middle).
    buttons: u8,
    position: CGPoint,
    /// Modifier flags from the keys held down.
    flags: u64,
    /// For double-clicks: when and where the last press was, which button, and its click count.
    last_click: Option<(Instant, CGPoint, u8, i64)>,
    /// Scrolling not sent yet (fractions of a pixel).
    wheel: (f64, f64),
}

// SAFETY: CGEventSource is a thread-safe CoreFoundation object; Input is only
// ever used by one thread at a time (it's moved to the stdin reader).
unsafe impl Send for Input {}

impl Input {
    pub fn new(size: Arc<AtomicU64>) -> Self {
        let source = CGEventSource::new(CGEventSourceStateID(CGEventSourceStateID::HIDSystemState.0));
        Self { size, source, buttons: 0, position: CGPoint::new(0.0, 0.0), flags: 0, last_click: None, wheel: (0.0, 0.0) }
    }

    /// Payload: u16 x, u16 y, u8 buttons, i16 wheel_y, i16 wheel_x (Windows wheel units, + = up/right).
    pub fn pointer(&mut self, p: &[u8]) {
        if p.len() < 9 {
            return;
        }
        let size = self.size.load(Ordering::Relaxed);
        let (width, height) = ((size >> 32) as f64, size as u32 as f64);
        if width == 0.0 || height == 0.0 {
            return;
        }
        let bounds = CGDisplayBounds(CGMainDisplayID());
        let x = u16::from_le_bytes([p[0], p[1]]) as f64;
        let y = u16::from_le_bytes([p[2], p[3]]) as f64;
        self.position = CGPoint::new(
            bounds.origin.x + (x / width * bounds.size.width).clamp(0.0, bounds.size.width - 1.0),
            bounds.origin.y + (y / height * bounds.size.height).clamp(0.0, bounds.size.height - 1.0),
        );
        let buttons = p[4];
        let wheel_y = i16::from_le_bytes([p[5], p[6]]) as f64;
        let wheel_x = i16::from_le_bytes([p[7], p[8]]) as f64;

        // Move (as a drag while a button is held).
        let (kind, button) = if buttons & 1 != 0 {
            (LEFT_DRAGGED, 0)
        } else if buttons & 2 != 0 {
            (RIGHT_DRAGGED, 1)
        } else if buttons & 4 != 0 {
            (OTHER_DRAGGED, 2)
        } else {
            (MOVED, 0)
        };
        self.mouse(kind, button, 0);

        for (bit, button, down, up) in [(1u8, 0u32, LEFT_DOWN, LEFT_UP), (2, 1, RIGHT_DOWN, RIGHT_UP), (4, 2, OTHER_DOWN, OTHER_UP)] {
            let (was, is) = (self.buttons & bit != 0, buttons & bit != 0);
            if was == is {
                continue;
            }
            let clicks = if is { self.click_count(bit) } else { self.last_click.map_or(1, |c| c.3) };
            self.mouse(if is { down } else { up }, button, clicks);
        }
        self.buttons = buttons;

        // Scrolling: the viewer sends 1.2 units per browser pixel.
        self.wheel.0 += wheel_y / 1.2;
        self.wheel.1 += wheel_x / 1.2;
        let (dy, dx) = (self.wheel.0.trunc(), self.wheel.1.trunc());
        if dy != 0.0 || dx != 0.0 {
            self.wheel.0 -= dy;
            self.wheel.1 -= dx;
            // wheel1: + = up; wheel2: + = left.
            let event = CGEvent::new_scroll_wheel_event2(self.source.as_deref(), CGScrollEventUnit::Pixel, 2, dy as i32, -dx as i32, 0);
            self.post(event);
        }
    }

    /// 1 for a single click, 2 for a double-click, and so on.
    fn click_count(&mut self, button: u8) -> i64 {
        let now = Instant::now();
        let count = match self.last_click {
            Some((at, pos, b, n))
                if b == button
                    && now.duration_since(at) < DOUBLE_CLICK_TIME
                    && (pos.x - self.position.x).hypot(pos.y - self.position.y) < DOUBLE_CLICK_DISTANCE =>
            {
                n + 1
            }
            _ => 1,
        };
        self.last_click = Some((now, self.position, button, count));
        count
    }

    fn mouse(&self, kind: u32, button: u32, clicks: i64) {
        let event = CGEvent::new_mouse_event(self.source.as_deref(), CGEventType(kind), self.position, CGMouseButton(button));
        if clicks > 0 {
            CGEvent::set_integer_value_field(event.as_deref(), CGEventField::MouseEventClickState, clicks);
        }
        self.post(event);
    }

    /// Payload: u16 scan code (set 1; 0xE0xx = extended), u8 down.
    pub fn key(&mut self, p: &[u8]) {
        if p.len() < 3 {
            return;
        }
        let Some(code) = mac_key_code(u16::from_le_bytes([p[0], p[1]])) else { return };
        let down = p[2] != 0;
        let event = CGEvent::new_keyboard_event(self.source.as_deref(), code, down);
        if let Some(flag) = modifier_flag(code) {
            if down {
                self.flags |= flag;
            } else {
                self.flags &= !flag;
            }
            // Modifier keys are "flags changed" events, not key presses.
            CGEvent::set_type(event.as_deref(), CGEventType(FLAGS_CHANGED));
        }
        self.post(event);
    }

    /// Posts an event with the current modifier flags.
    fn post(&self, event: Option<CFRetained<CGEvent>>) {
        let Some(event) = event else { return };
        CGEvent::set_flags(Some(&event), CGEventFlags(self.flags));
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
    }
}

/// The modifier flag a Mac key code sets while held (CGEventFlags masks).
fn modifier_flag(code: u16) -> Option<u64> {
    Some(match code {
        0x38 | 0x3C => 0x0002_0000, // Shift
        0x3B | 0x3E => 0x0004_0000, // Control
        0x3A | 0x3D => 0x0008_0000, // Option
        0x37 | 0x36 => 0x0010_0000, // Command
        _ => return None,
    })
}

/// PC scan code (set 1) -> Mac virtual key code (Carbon kVK_*), by key position.
fn mac_key_code(scan: u16) -> Option<u16> {
    Some(match scan {
        0x01 => 0x35, // Escape
        0x02 => 0x12, // 1
        0x03 => 0x13,
        0x04 => 0x14,
        0x05 => 0x15,
        0x06 => 0x17, // 5
        0x07 => 0x16, // 6
        0x08 => 0x1A, // 7
        0x09 => 0x1C, // 8
        0x0A => 0x19, // 9
        0x0B => 0x1D, // 0
        0x0C => 0x1B, // -
        0x0D => 0x18, // =
        0x0E => 0x33, // Backspace (Mac "Delete")
        0x0F => 0x30, // Tab
        0x10 => 0x0C, // Q
        0x11 => 0x0D, // W
        0x12 => 0x0E, // E
        0x13 => 0x0F, // R
        0x14 => 0x11, // T
        0x15 => 0x10, // Y
        0x16 => 0x20, // U
        0x17 => 0x22, // I
        0x18 => 0x1F, // O
        0x19 => 0x23, // P
        0x1A => 0x21, // [
        0x1B => 0x1E, // ]
        0x1C => 0x24, // Return
        0x1D => 0x3B, // left Control
        0x1E => 0x00, // A
        0x1F => 0x01, // S
        0x20 => 0x02, // D
        0x21 => 0x03, // F
        0x22 => 0x05, // G
        0x23 => 0x04, // H
        0x24 => 0x26, // J
        0x25 => 0x28, // K
        0x26 => 0x25, // L
        0x27 => 0x29, // ;
        0x28 => 0x27, // '
        0x29 => 0x32, // `
        0x2A => 0x38, // left Shift
        0x2B => 0x2A, // backslash
        0x2C => 0x06, // Z
        0x2D => 0x07, // X
        0x2E => 0x08, // C
        0x2F => 0x09, // V
        0x30 => 0x0B, // B
        0x31 => 0x2D, // N
        0x32 => 0x2E, // M
        0x33 => 0x2B, // ,
        0x34 => 0x2F, // .
        0x35 => 0x2C, // /
        0x36 => 0x3C, // right Shift
        0x37 => 0x43, // keypad *
        0x38 => 0x3A, // left Alt = Option
        0x39 => 0x31, // Space
        0x3A => 0x39, // Caps Lock
        0x3B => 0x7A, // F1
        0x3C => 0x78, // F2
        0x3D => 0x63, // F3
        0x3E => 0x76, // F4
        0x3F => 0x60, // F5
        0x40 => 0x61, // F6
        0x41 => 0x62, // F7
        0x42 => 0x64, // F8
        0x43 => 0x65, // F9
        0x44 => 0x6D, // F10
        0x45 => 0x47, // Num Lock = keypad Clear
        0x47 => 0x59, // keypad 7
        0x48 => 0x5B, // keypad 8
        0x49 => 0x5C, // keypad 9
        0x4A => 0x4E, // keypad -
        0x4B => 0x56, // keypad 4
        0x4C => 0x57, // keypad 5
        0x4D => 0x58, // keypad 6
        0x4E => 0x45, // keypad +
        0x4F => 0x53, // keypad 1
        0x50 => 0x54, // keypad 2
        0x51 => 0x55, // keypad 3
        0x52 => 0x52, // keypad 0
        0x53 => 0x41, // keypad .
        0x56 => 0x0A, // ISO key next to left Shift (§ on Mac keyboards)
        0x57 => 0x67, // F11
        0x58 => 0x6F, // F12
        0x59 => 0x51, // keypad =
        0x73 => 0x5E, // JIS Ro
        0x7D => 0x5D, // JIS Yen
        0xE01C => 0x4C, // keypad Enter
        0xE01D => 0x3E, // right Control
        0xE035 => 0x4B, // keypad /
        0xE037 => 0x69, // Print Screen = F13
        0xE038 => 0x3D, // right Alt = right Option
        0xE047 => 0x73, // Home
        0xE048 => 0x7E, // Up
        0xE049 => 0x74, // Page Up
        0xE04B => 0x7B, // Left
        0xE04D => 0x7C, // Right
        0xE04F => 0x77, // End
        0xE050 => 0x7D, // Down
        0xE051 => 0x79, // Page Down
        0xE052 => 0x72, // Insert = Help
        0xE053 => 0x75, // Delete (forward delete)
        0xE05B => 0x37, // left Windows key = Command
        0xE05C => 0x36, // right Windows key = right Command
        _ => return None,
    })
}
