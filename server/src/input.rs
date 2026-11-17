//! Replays the viewer's mouse and keyboard on Windows with SendInput.

use windows::Win32::UI::Input::KeyboardAndMouse::*;

pub struct Input {
    /// Desktop size, to turn pixel positions into SendInput's 0..65535 range.
    width: u32,
    height: u32,
    /// Browser `buttons` mask last sent (1 = left, 2 = right, 4 = middle).
    buttons: u8,
}

impl Input {
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height, buttons: 0 }
    }

    /// Payload: u16 x, u16 y, u8 buttons, i16 wheel_y, i16 wheel_x (Windows wheel units, + = up/right).
    pub fn pointer(&mut self, p: &[u8]) {
        if p.len() < 9 {
            return;
        }
        let x = u16::from_le_bytes([p[0], p[1]]) as i32;
        let y = u16::from_le_bytes([p[2], p[3]]) as i32;
        let buttons = p[4];
        let wheel_y = i16::from_le_bytes([p[5], p[6]]) as i32;
        let wheel_x = i16::from_le_bytes([p[7], p[8]]) as i32;

        let norm = |v: i32, size: u32| (v.clamp(0, size as i32 - 1) * 65535) / (size as i32 - 1).max(1);
        let mut events = vec![mouse(norm(x, self.width), norm(y, self.height), 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE)];
        for (bit, down, up) in [
            (1u8, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
            (2, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
            (4, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
        ] {
            let (was, is) = (self.buttons & bit != 0, buttons & bit != 0);
            if was != is {
                events.push(mouse(0, 0, 0, if is { down } else { up }));
            }
        }
        self.buttons = buttons;
        if wheel_y != 0 {
            events.push(mouse(0, 0, wheel_y as u32, MOUSEEVENTF_WHEEL));
        }
        if wheel_x != 0 {
            events.push(mouse(0, 0, wheel_x as u32, MOUSEEVENTF_HWHEEL));
        }
        send(&events);
    }

    /// Payload: u16 scan code (set 1; 0xE0xx = extended), u8 down.
    pub fn key(&mut self, p: &[u8]) {
        if p.len() < 3 {
            return;
        }
        let scan = u16::from_le_bytes([p[0], p[1]]);
        let down = p[2] != 0;
        let mut flags = KEYEVENTF_SCANCODE;
        if scan & 0xFF00 == 0xE000 {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if !down {
            flags |= KEYEVENTF_KEYUP;
        }
        let ki = KEYBDINPUT { wVk: VIRTUAL_KEY(0), wScan: scan & 0xFF, dwFlags: flags, time: 0, dwExtraInfo: 0 };
        send(&[INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki } }]);
    }
}

fn mouse(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    let mi = MOUSEINPUT { dx, dy, mouseData: data, dwFlags: flags, time: 0, dwExtraInfo: 0 };
    INPUT { r#type: INPUT_MOUSE, Anonymous: INPUT_0 { mi } }
}

fn send(events: &[INPUT]) {
    let sent = unsafe { SendInput(events, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != events.len() {
        // Typically UIPI: an elevated window has focus, or a secure desktop (UAC/lock screen) is up.
        eprintln!("desktop-server: SendInput delivered {sent}/{} events", events.len());
    }
}
