//! Clipboard sync. Not implemented on macOS yet: text is dropped.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

/// Larger text is not synced.
pub const MAX_TEXT: usize = 1024 * 1024;

pub fn run(incoming: Receiver<String>, closed: &AtomicBool, _send: impl FnMut(&str) -> std::io::Result<()>) {
    while !closed.load(Ordering::Relaxed) {
        if let Err(RecvTimeoutError::Disconnected) = incoming.recv_timeout(Duration::from_millis(250)) {
            break;
        }
    }
}
