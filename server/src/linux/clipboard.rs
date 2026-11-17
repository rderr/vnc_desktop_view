//! Text clipboard sync on X11 (the CLIPBOARD selection, what Ctrl+C / Ctrl+V use).
//!
//! X has no clipboard storage: the program that copied owns the selection
//! and hands the data to whoever asks. So this thread
//!  - watches for new owners (XFixes), asks them for their text and reports it;
//!  - for text from the viewer, becomes the owner itself and answers requests.
//! Text that came from the viewer is not reported back, so it can't bounce
//! between the machines.

use anyhow::{Context, Result};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask};
use x11rb::protocol::xproto::*;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// Larger clipboard contents are not synced.
pub const MAX_TEXT: usize = 1024 * 1024;

struct Atoms {
    clipboard: Atom,
    utf8: Atom,
    text: Atom,
    text_plain: Atom,
    targets: Atom,
    incr: Atom,
    /// Property on our window where owners put the text we ask for.
    prop: Atom,
}

/// Runs until `closed` is set. Text arriving on `incoming` is put on the
/// clipboard; `send` is called with clipboard text copied by anyone else.
pub fn run(incoming: Receiver<String>, closed: &AtomicBool, mut send: impl FnMut(&str) -> std::io::Result<()>) {
    let mut reported = false;
    while !closed.load(Ordering::Relaxed) {
        if let Err(e) = serve(&incoming, closed, &mut send) {
            if !std::mem::replace(&mut reported, true) {
                eprintln!("desktop-server: clipboard: {e:#}; retrying");
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
        if matches!(incoming.try_recv(), Err(TryRecvError::Disconnected)) {
            return;
        }
    }
}

fn serve(incoming: &Receiver<String>, closed: &AtomicBool, send: &mut impl FnMut(&str) -> std::io::Result<()>) -> Result<()> {
    let (conn, screen) = RustConnection::connect(None).context("connecting to the X server")?;
    let root = conn.setup().roots[screen].root;
    let intern = |name: &str| -> Result<Atom> { Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom) };
    let atoms = Atoms {
        clipboard: intern("CLIPBOARD")?,
        utf8: intern("UTF8_STRING")?,
        text: intern("TEXT")?,
        text_plain: intern("text/plain;charset=utf-8")?,
        targets: intern("TARGETS")?,
        incr: intern("INCR")?,
        prop: intern("DESKTOP_SERVER_CLIPBOARD")?,
    };
    // Anything bigger has to be sent in pieces (INCR), which we don't do; such text isn't offered.
    let max_property = conn.maximum_request_bytes().saturating_sub(1024);

    let win = conn.generate_id()?;
    conn.create_window(0, win, root, 0, 0, 1, 1, 0, WindowClass::INPUT_ONLY, 0,
        &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE))?;
    conn.xfixes_query_version(5, 0)?.reply().context("the X server lacks XFIXES")?;
    conn.xfixes_select_selection_input(win, atoms.clipboard,
        SelectionEventMask::SET_SELECTION_OWNER | SelectionEventMask::SELECTION_WINDOW_DESTROY | SelectionEventMask::SELECTION_CLIENT_CLOSE)?;
    conn.flush()?;

    // Text we own (from the viewer), and the text both sides last agreed on.
    let mut owned: Option<String> = None;
    let mut synced: Option<String> = None;
    // Text arriving in pieces (INCR), if a transfer is under way.
    let mut incr: Option<Vec<u8>> = None;

    while !closed.load(Ordering::Relaxed) {
        loop {
            match incoming.try_recv() {
                Ok(text) => {
                    conn.set_selection_owner(win, atoms.clipboard, x11rb::CURRENT_TIME)?;
                    conn.flush()?;
                    synced = Some(text.clone());
                    owned = Some(text);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }

        let mut pfd = libc::pollfd { fd: conn.stream().as_raw_fd(), events: libc::POLLIN, revents: 0 };
        unsafe { libc::poll(&mut pfd, 1, 200) };
        while let Some(event) = conn.poll_for_event()? {
            let mut received = None;
            match event {
                // Someone copied: ask for their text (as UTF-8).
                Event::XfixesSelectionNotify(e) if e.owner != win && e.owner != x11rb::NONE => {
                    owned = None;
                    incr = None;
                    conn.convert_selection(win, atoms.clipboard, atoms.utf8, atoms.prop, e.selection_timestamp)?;
                }
                Event::SelectionNotify(e) if e.requestor == win => {
                    if e.property == x11rb::NONE {
                        // No UTF-8: try Latin-1 text (STRING) once.
                        if e.target == atoms.utf8 {
                            conn.convert_selection(win, atoms.clipboard, AtomEnum::STRING.into(), atoms.prop, e.time)?;
                        }
                    } else {
                        let reply = conn.get_property(true, win, atoms.prop, AtomEnum::ANY, 0, (MAX_TEXT / 4 + 1) as u32)?.reply()?;
                        if reply.type_ == atoms.incr {
                            incr = Some(Vec::new()); // pieces follow as property changes
                        } else if reply.bytes_after == 0 {
                            received = Some(decode(reply.value, e.target == atoms.utf8));
                        }
                    }
                }
                // The next piece of an INCR transfer; an empty one ends it.
                Event::PropertyNotify(e) if e.window == win && e.atom == atoms.prop && e.state == Property::NEW_VALUE => {
                    if let Some(buf) = incr.as_mut() {
                        let reply = conn.get_property(true, win, atoms.prop, AtomEnum::ANY, 0, (MAX_TEXT / 4 + 1) as u32)?.reply()?;
                        if reply.value.is_empty() {
                            received = incr.take().map(|b| decode(b, true));
                        } else if buf.len() + reply.value.len() > MAX_TEXT {
                            incr = None; // too big; let the transfer run out
                        } else {
                            buf.extend_from_slice(&reply.value);
                        }
                    }
                }
                // Someone wants the text we own.
                Event::SelectionRequest(e) => {
                    let property = if e.property == x11rb::NONE { e.target } else { e.property };
                    let ok = match &owned {
                        Some(text) if e.selection == atoms.clipboard => {
                            if e.target == atoms.targets {
                                let list = [atoms.targets, atoms.utf8, atoms.text, atoms.text_plain, AtomEnum::STRING.into()];
                                conn.change_property32(PropMode::REPLACE, e.requestor, property, AtomEnum::ATOM, &list)?;
                                true
                            } else if text.len() > max_property {
                                false
                            } else if [atoms.utf8, atoms.text, atoms.text_plain].contains(&e.target) {
                                conn.change_property8(PropMode::REPLACE, e.requestor, property, atoms.utf8, text.as_bytes())?;
                                true
                            } else if e.target == u32::from(AtomEnum::STRING) {
                                let latin1: Vec<u8> = text.chars().map(|c| if (c as u32) < 256 { c as u8 } else { b'?' }).collect();
                                conn.change_property8(PropMode::REPLACE, e.requestor, property, AtomEnum::STRING, &latin1)?;
                                true
                            } else {
                                false
                            }
                        }
                        _ => false,
                    };
                    let notify = SelectionNotifyEvent {
                        response_type: SELECTION_NOTIFY_EVENT,
                        sequence: 0,
                        time: e.time,
                        requestor: e.requestor,
                        selection: e.selection,
                        target: e.target,
                        property: if ok { property } else { x11rb::NONE },
                    };
                    conn.send_event(false, e.requestor, EventMask::NO_EVENT, notify)?;
                }
                Event::SelectionClear(e) if e.selection == atoms.clipboard => owned = None,
                _ => {}
            }
            if let Some(text) = received
                && !text.is_empty()
                && text.len() <= MAX_TEXT
                && synced.as_deref() != Some(text.as_str())
            {
                send(&text)?;
                synced = Some(text);
            }
        }
        conn.flush()?;
    }
    Ok(())
}

/// Clipboard bytes as text: UTF-8, or Latin-1 for the old STRING type.
fn decode(bytes: Vec<u8>, utf8: bool) -> String {
    if utf8 {
        String::from_utf8_lossy(&bytes).into_owned()
    } else {
        bytes.iter().map(|&b| b as char).collect()
    }
}
