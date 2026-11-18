//! Sound. Not implemented on macOS yet: nothing is sent.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub fn run(_enabled: Arc<AtomicBool>, closed: Arc<AtomicBool>, _start: Instant, _send: impl FnMut(u64, &[u8]) -> std::io::Result<()>) {
    while !closed.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(250));
    }
}
