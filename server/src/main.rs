//! desktop-server: captures the desktop and streams it as H.264 (Windows, and
//! Linux on X11; the platform code is in win/ and linux/).
//!
//! Modes:
//!   desktop-server [--encoder auto|hw|sw]                         stream on stdout (see PROTOCOL)
//!                  [--openh264 PATH]  Linux: Cisco's OpenH264 library, the software fallback
//!   desktop-server [--encoder ...] --out FILE [--seconds N]       write a raw Annex B .h264 file (testing)
//!
//! PROTOCOL (stdout, little-endian): each message is `u8 kind, u32 len, payload[len]`.
//!   kind 1 HELLO  payload: u32 width, u32 height, then UTF-8: codec (e.g. "avc1.42E033"), NUL,
//!                 short encoder label (e.g. "NVENC"), NUL, full encoder name.
//!                 Sent again whenever capture restarts (e.g. after a resolution change);
//!                 the viewer resets its decoder, and a keyframe follows.
//!   kind 2 VIDEO  payload: u8 flags (bit 0 = keyframe), u64 timestamp_us, Annex B data
//!   kind 3 AUDIO  payload: u64 timestamp_us, one Opus packet (48 kHz stereo, 20 ms)
//!   kind 4 CLIPBOARD payload: UTF-8 text (the remote clipboard changed)
//!   kind 5 NOTICE payload: u8 code, UTF-8 detail. 0 = clear, 1 = a secure desktop (lock
//!                 screen, UAC prompt) is showing and can't be captured, 2 = capture failing (detail),
//!                 3 = no usable H.264 encoder (detail); the software encoder (--openh264) would help
//! stdin: same framing.
//!   kind 1 KEYFRAME  (no payload) – send a keyframe next
//!   kind 2 POINTER   u16 x, u16 y (desktop pixels), u8 buttons (1 left, 2 right, 4 middle),
//!                    i16 wheel_y, i16 wheel_x (Windows wheel units, 120 = one notch, + = up/right)
//!   kind 3 KEY       u16 scan code (set 1, 0xE0xx = extended), u8 down
//!   kind 4 AUDIO     u8 enabled – start/stop sending audio (off until the viewer asks)
//!   kind 5 CLIPBOARD UTF-8 text – put it on the remote clipboard

mod convert;
mod encoder;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod win;

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(windows)]
use win as platform;

use anyhow::Result;
use platform::capture::{self, Capturer, Frame};
use platform::{audio, clipboard, input};
use encoder::{MAX_FPS, VideoEncoder};
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let encoder_kind = arg("--encoder").unwrap_or_else(|| "auto".into());
    if let Some(path) = arg("--openh264") {
        let _ = encoder::OPENH264_PATH.set(path.into());
    }
    #[cfg(target_os = "linux")]
    linux::find_display();

    if let Some(path) = arg("--out") {
        let seconds: u64 = arg("--seconds").map(|s| s.parse()).transpose()?.unwrap_or(5);
        let (mut capturer, mut encoder) = open(&encoder_kind)?;
        return record(&mut capturer, encoder.as_mut(), &path, Duration::from_secs(seconds));
    }
    stream(&encoder_kind)
}

/// Starts capturing the current desktop with an encoder sized to match.
fn open(encoder_kind: &str) -> Result<(Capturer, Box<dyn VideoEncoder>)> {
    let capturer = Capturer::new()?;
    let encoder = encoder::create(encoder_kind, &capturer)?;
    eprintln!(
        "desktop-server: capturing {}x{}, encoding {}x{} with {} ({})",
        capturer.src_width,
        capturer.src_height,
        capturer.width,
        capturer.height,
        encoder.name(),
        encoder.codec()
    );
    Ok((capturer, encoder))
}

fn record(capturer: &mut Capturer, encoder: &mut dyn VideoEncoder, path: &str, duration: Duration) -> Result<()> {
    let mut file = std::fs::File::create(path)?;
    let start = Instant::now();
    let (mut frames, mut bytes, mut encode_time, mut worst) = (0u32, 0usize, Duration::ZERO, Duration::ZERO);
    let mut have_image = false;
    while start.elapsed() < duration {
        // Wait (up to 1 s) for the first image even if the screen is idle.
        match capturer.next(if have_image { 100 } else { 1000 })? {
            Frame::Changed => have_image = true,
            Frame::Unchanged => continue,
            Frame::Lost(why) => anyhow::bail!("capture lost: {why}"),
        }
        let t = Instant::now();
        let (data, key) = encoder.encode(capturer, start.elapsed().as_micros() as u64)?;
        let took = t.elapsed();
        encode_time += took;
        worst = worst.max(took);
        if std::env::var_os("DS_DEBUG").is_some() {
            eprintln!("  frame {frames}: {:.1} ms, {} bytes{}", took.as_secs_f64() * 1000.0, data.len(), if key { " KEY" } else { "" });
        }
        file.write_all(&data)?;
        frames += 1;
        bytes += data.len();
    }
    let secs = start.elapsed().as_secs_f64();
    eprintln!(
        "desktop-server: {frames} frames in {secs:.1}s ({:.1} fps), {:.0} KB/s, encode avg {:.1} ms, worst {:.1} ms",
        frames as f64 / secs,
        bytes as f64 / secs / 1000.0,
        if frames > 0 { encode_time.as_secs_f64() * 1000.0 / frames as f64 } else { 0.0 },
        worst.as_secs_f64() * 1000.0
    );
    Ok(())
}

fn write_msg(out: &mut impl Write, kind: u8, parts: &[&[u8]]) -> std::io::Result<()> {
    let len: usize = parts.iter().map(|p| p.len()).sum();
    out.write_all(&[kind])?;
    out.write_all(&(len as u32).to_le_bytes())?;
    for p in parts {
        out.write_all(p)?;
    }
    out.flush()
}

/// Each message is written under its own stdout lock, so messages from the
/// audio and clipboard threads never land in the middle of a video message.
fn send(kind: u8, parts: &[&[u8]]) -> std::io::Result<()> {
    write_msg(&mut std::io::stdout().lock(), kind, parts)
}

const NOTICE_CLEAR: u8 = 0;
const NOTICE_SECURE_DESKTOP: u8 = 1;
const NOTICE_CAPTURE_FAILING: u8 = 2;
const NOTICE_NO_ENCODER: u8 = 3;

fn stream(encoder_kind: &str) -> Result<()> {
    let want_key = Arc::new(AtomicBool::new(false));
    let closed = Arc::new(AtomicBool::new(false));
    let audio_enabled = Arc::new(AtomicBool::new(false));
    // Desktop size for input scaling: width << 32 | height. Updated on every (re)start.
    let desktop_size = Arc::new(AtomicU64::new(0));
    let (clip_tx, clip_rx) = std::sync::mpsc::channel::<String>();
    let start = Instant::now();

    // stdin reader: keyframe requests, input, audio on/off, clipboard. EOF means the extension went away.
    {
        let (want_key, closed, audio_enabled) = (want_key.clone(), closed.clone(), audio_enabled.clone());
        let mut input = input::Input::new(desktop_size.clone());
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut header = [0u8; 5];
            while stdin.read_exact(&mut header).is_ok() {
                let len = u32::from_le_bytes(header[1..5].try_into().unwrap()) as usize;
                let mut payload = vec![0u8; len];
                if stdin.read_exact(&mut payload).is_err() {
                    break;
                }
                match header[0] {
                    1 => want_key.store(true, Ordering::Relaxed),
                    2 => input.pointer(&payload),
                    3 => input.key(&payload),
                    4 => audio_enabled.store(payload.first() == Some(&1), Ordering::Relaxed),
                    5 => {
                        if payload.len() <= clipboard::MAX_TEXT {
                            let _ = clip_tx.send(String::from_utf8_lossy(&payload).into_owned());
                        }
                    }
                    _ => {}
                }
            }
            closed.store(true, Ordering::Relaxed);
        });
    }
    {
        let (audio_enabled, closed) = (audio_enabled.clone(), closed.clone());
        std::thread::spawn(move || {
            audio::run(audio_enabled, closed, start, |ts, packet| send(3, &[&ts.to_le_bytes(), packet]));
        });
    }
    {
        let closed = closed.clone();
        std::thread::spawn(move || {
            clipboard::run(clip_rx, &closed, |text| send(4, &[text.as_bytes()]));
        });
    }

    let mut session: Option<(Capturer, Box<dyn VideoEncoder>)> = None;
    // The notice currently shown, so it's sent only when it changes.
    let mut notice = NOTICE_CLEAR;
    let min_interval = Duration::from_secs_f32(1.0 / MAX_FPS as f32);
    let mut last_sent = Instant::now() - min_interval;
    let mut have_image = false;
    let mut need_send = true;

    while !closed.load(Ordering::Relaxed) {
        // (Re)start capture: at launch, after a resolution change, after a lock screen…
        let Some((capturer, encoder)) = session.as_mut() else {
            match open(encoder_kind) {
                Ok((capturer, encoder)) => {
                    desktop_size.store(((capturer.src_width as u64) << 32) | capturer.src_height as u64, Ordering::Relaxed);
                    let dims = [capturer.width.to_le_bytes(), capturer.height.to_le_bytes()].concat();
                    send(1, &[&dims, encoder.codec().as_bytes(), b"\0", encoder.label().as_bytes(), b"\0", encoder.name().as_bytes()])?;
                    if notice != NOTICE_CLEAR {
                        send(5, &[&[NOTICE_CLEAR]])?;
                        notice = NOTICE_CLEAR;
                    }
                    session = Some((capturer, encoder));
                    // A new duplication delivers the current screen as its first frame.
                    have_image = false;
                    need_send = true;
                }
                Err(e) => {
                    let code = if capture::is_secure_desktop(&e) {
                        NOTICE_SECURE_DESKTOP
                    } else if e.downcast_ref::<encoder::NoEncoder>().is_some() {
                        NOTICE_NO_ENCODER
                    } else {
                        NOTICE_CAPTURE_FAILING
                    };
                    if code != notice {
                        eprintln!("desktop-server: capture unavailable ({e:#}); retrying");
                        send(5, &[&[code], format!("{e:#}").as_bytes()])?;
                        notice = code;
                    }
                    // A missing encoder won't appear by itself; don't keep probing for it.
                    std::thread::sleep(Duration::from_millis(if code == NOTICE_NO_ENCODER { 10_000 } else { 500 }));
                }
            }
            continue;
        };

        match capturer.next(100) {
            Ok(Frame::Changed) => {
                have_image = true;
                need_send = true;
            }
            Ok(Frame::Unchanged) => {}
            Ok(Frame::Lost(why)) => {
                eprintln!("desktop-server: capture lost ({why}); restarting capture");
                // A resolution change is announced more than once while the display
                // settles; give it a moment so capture restarts once, not twice.
                std::thread::sleep(Duration::from_millis(300));
                session = None;
                continue;
            }
            Err(e) => {
                eprintln!("desktop-server: capture failed ({e:#}); restarting capture");
                session = None;
                continue;
            }
        }
        if want_key.swap(false, Ordering::Relaxed) {
            encoder.force_keyframe();
            need_send = true;
        }
        if !need_send || !have_image {
            continue;
        }
        let since = last_sent.elapsed();
        if since < min_interval {
            std::thread::sleep(min_interval - since);
        }
        let ts = start.elapsed().as_micros() as u64;
        let (data, key) = match encoder.encode(capturer, ts) {
            Ok(r) => r,
            Err(e) => {
                // e.g. the GPU was reset; start over with a fresh capturer and encoder.
                eprintln!("desktop-server: encoding failed ({e:#}); restarting capture");
                session = None;
                continue;
            }
        };
        last_sent = Instant::now();
        need_send = false;
        if data.is_empty() {
            continue; // encoder skipped the frame (rate control)
        }
        send(2, &[&[key as u8], &ts.to_le_bytes(), &data])?;
    }
    Ok(())
}
