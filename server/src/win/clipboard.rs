//! Text clipboard sync on Windows.
//!
//! One thread owns all clipboard work. It polls the clipboard's change counter
//! and reports new text, and applies text the viewer sends. Text we set
//! ourselves is not reported back, so it can't bounce between the machines.
//! (Windows only lets a window own clipboard data, so the thread creates a
//! hidden message-only window.)

use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::Memory::*;
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, HWND_MESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE};
use windows::core::w;

/// Larger clipboard contents are not synced.
pub const MAX_TEXT: usize = 1024 * 1024;
const POLL: Duration = Duration::from_millis(250);

/// Runs until `closed` is set. Text arriving on `incoming` is put on the
/// clipboard; `send` is called with clipboard text changed by anyone else.
pub fn run(incoming: Receiver<String>, closed: &AtomicBool, mut send: impl FnMut(&str) -> std::io::Result<()>) {
    let hwnd = match unsafe {
        CreateWindowExW(WINDOW_EX_STYLE(0), w!("STATIC"), w!("desktop-server clipboard"), WINDOW_STYLE(0), 0, 0, 0, 0, Some(HWND_MESSAGE), None, None, None)
    } {
        Ok(h) => h,
        Err(e) => {
            eprintln!("desktop-server: clipboard sync disabled: {e}");
            return;
        }
    };
    // Don't send whatever was on the clipboard before we connected.
    let mut last_seq = unsafe { GetClipboardSequenceNumber() };
    while !closed.load(Ordering::Relaxed) {
        match incoming.recv_timeout(POLL) {
            Ok(text) => match set_text(hwnd, &text) {
                Ok(()) => last_seq = unsafe { GetClipboardSequenceNumber() },
                Err(e) => eprintln!("desktop-server: setting clipboard: {e:#}"),
            },
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        let seq = unsafe { GetClipboardSequenceNumber() };
        if seq != last_seq {
            last_seq = seq;
            if let Some(text) = get_text(hwnd) {
                if text.len() <= MAX_TEXT && send(&text).is_err() {
                    break;
                }
            }
        }
    }
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

/// Another program may have the clipboard open briefly; retry a few times.
fn open(hwnd: HWND) -> Result<()> {
    for _ in 0..10 {
        if unsafe { OpenClipboard(Some(hwnd)) }.is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    unsafe { OpenClipboard(Some(hwnd)) }.context("OpenClipboard")
}

fn get_text(hwnd: HWND) -> Option<String> {
    unsafe {
        IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32).ok()?; // e.g. an image was copied
        open(hwnd).ok()?;
        let text = (|| {
            let h = HGLOBAL(GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?.0);
            let ptr = GlobalLock(h) as *const u16;
            if ptr.is_null() {
                return None;
            }
            let max = GlobalSize(h) / 2;
            let units = std::slice::from_raw_parts(ptr, max);
            let len = units.iter().position(|&c| c == 0).unwrap_or(max);
            let s = String::from_utf16_lossy(&units[..len]);
            let _ = GlobalUnlock(h);
            Some(s)
        })();
        let _ = CloseClipboard();
        text
    }
}

fn set_text(hwnd: HWND, text: &str) -> Result<()> {
    let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE, utf16.len() * 2)?;
        let ptr = GlobalLock(h) as *mut u16;
        std::ptr::copy_nonoverlapping(utf16.as_ptr(), ptr, utf16.len());
        let _ = GlobalUnlock(h);
        if let Err(e) = open(hwnd) {
            let _ = GlobalFree(Some(h));
            return Err(e);
        }
        let result = EmptyClipboard().and_then(|_| SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(h.0))));
        let _ = CloseClipboard();
        if result.is_err() {
            let _ = GlobalFree(Some(h)); // on success the clipboard owns the memory
        }
        result.map(|_| ()).context("SetClipboardData")
    }
}
