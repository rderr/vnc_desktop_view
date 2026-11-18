//! Text clipboard sync on macOS (the general pasteboard).
//!
//! The pasteboard's change count is polled; new text is reported. Text the
//! viewer sends is put on the pasteboard, and the change count that produces
//! is remembered, so it isn't reported back (and can't bounce between machines).

use objc2::rc::autoreleasepool;
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::NSString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

/// Larger clipboard contents are not synced.
pub const MAX_TEXT: usize = 1024 * 1024;
const POLL: Duration = Duration::from_millis(250);

/// Runs until `closed` is set. Text arriving on `incoming` is put on the
/// pasteboard; `send` is called with text copied by anyone else.
pub fn run(incoming: Receiver<String>, closed: &AtomicBool, mut send: impl FnMut(&str) -> std::io::Result<()>) {
    let pasteboard = NSPasteboard::generalPasteboard();
    // Only changes from now on count; what's already there isn't news.
    let mut seen = pasteboard.changeCount();
    let mut last_text: Option<String> = None;
    while !closed.load(Ordering::Relaxed) {
        match incoming.recv_timeout(POLL) {
            Ok(text) => {
                autoreleasepool(|_| unsafe {
                    pasteboard.clearContents();
                    pasteboard.setString_forType(&NSString::from_str(&text), NSPasteboardTypeString);
                });
                seen = pasteboard.changeCount();
                last_text = Some(text);
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }
        let count = pasteboard.changeCount();
        if count == seen {
            continue;
        }
        seen = count;
        let text = autoreleasepool(|_| unsafe { pasteboard.stringForType(NSPasteboardTypeString).map(|s| s.to_string()) });
        if let Some(text) = text
            && !text.is_empty()
            && text.len() <= MAX_TEXT
            && last_text.as_deref() != Some(text.as_str())
        {
            if send(&text).is_err() {
                return;
            }
            last_text = Some(text);
        }
    }
}
