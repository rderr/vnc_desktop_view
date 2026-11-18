//! Which X display to capture.
//!
//! The extension runs inside VS Code's server, which Remote Tunnels or SSH
//! started without DISPLAY / XAUTHORITY. Borrow them from a process of the
//! same user that has them: one running in the user's desktop session.

use std::os::unix::fs::MetadataExt;

/// Sets DISPLAY (and XAUTHORITY, when known) if DISPLAY isn't set, and
/// XDG_RUNTIME_DIR if that isn't. Call it before starting any threads (it
/// changes the environment).
pub fn find_display() {
    let uid = unsafe { libc::getuid() };
    // The sound server's socket lives here; services started outside a login
    // session may not have it set.
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        let dir = format!("/run/user/{uid}");
        if std::path::Path::new(&dir).is_dir() {
            // SAFETY: called at startup, before any other threads exist.
            unsafe { std::env::set_var("XDG_RUNTIME_DIR", dir) };
        }
    }
    // libva logs every device it opens; errors are enough (we report our own).
    if std::env::var_os("LIBVA_MESSAGING_LEVEL").is_none() {
        // SAFETY: called at startup, before any other threads exist.
        unsafe { std::env::set_var("LIBVA_MESSAGING_LEVEL", "1") };
    }
    if std::env::var_os("DISPLAY").is_some() {
        return;
    }
    let Ok(procs) = std::fs::read_dir("/proc") else { return };
    let mut best: Option<(String, Option<String>)> = None;
    for entry in procs.flatten() {
        let path = entry.path();
        if !entry.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if std::fs::metadata(&path).map(|m| m.uid()).ok() != Some(uid) {
            continue;
        }
        let Ok(environ) = std::fs::read(path.join("environ")) else { continue };
        let mut display = None;
        let mut xauthority = None;
        for var in environ.split(|&b| b == 0) {
            let var = String::from_utf8_lossy(var);
            if let Some(v) = var.strip_prefix("DISPLAY=") {
                display = Some(v.to_string());
            } else if let Some(v) = var.strip_prefix("XAUTHORITY=") {
                xauthority = Some(v.to_string());
            }
        }
        if let Some(display) = display {
            let complete = xauthority.is_some();
            if best.is_none() || complete {
                best = Some((display, xauthority));
            }
            if complete {
                break;
            }
        }
    }
    match best {
        Some((display, xauthority)) => {
            eprintln!("desktop-server: using DISPLAY={display} from the desktop session");
            // SAFETY: called at startup, before any other threads exist.
            unsafe {
                std::env::set_var("DISPLAY", display);
                if let Some(x) = xauthority {
                    std::env::set_var("XAUTHORITY", x);
                }
            }
        }
        None => eprintln!("desktop-server: DISPLAY isn't set and no desktop session was found (is anyone logged in?)"),
    }
}
