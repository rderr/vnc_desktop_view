//! desktop-server: captures the desktop and streams it as H.264.
//!
//! Modes:
//!   desktop-server [--encoder auto|hw|sw]                         stream on stdout (see PROTOCOL)
//!   desktop-server [--encoder ...] --out FILE [--seconds N]       write a raw Annex B .h264 file (testing)
//!
//! PROTOCOL (stdout, little-endian): each message is `u8 kind, u32 len, payload[len]`.
//!   kind 1 HELLO  payload: u32 width, u32 height, codec string (UTF-8, e.g. "avc1.42E033")
//!   kind 2 VIDEO  payload: u8 flags (bit 0 = keyframe), u64 timestamp_us, Annex B data
//! stdin: same framing.
//!   kind 1 KEYFRAME  (no payload) – send a keyframe next
//!   kind 2 POINTER   u16 x, u16 y (desktop pixels), u8 buttons (1 left, 2 right, 4 middle),
//!                    i16 wheel_y, i16 wheel_x (Windows wheel units, 120 = one notch, + = up/right)
//!   kind 3 KEY       u16 scan code (set 1, 0xE0xx = extended), u8 down

mod capture;
mod encoder;
mod input;
mod mf;

use anyhow::Result;
use capture::{Capturer, Frame};
use encoder::{MAX_FPS, VideoEncoder};
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();

    let mut capturer = Capturer::new()?;
    let mut encoder = encoder::create(&arg("--encoder").unwrap_or_else(|| "auto".into()), capturer.width, capturer.height)?;
    eprintln!(
        "desktop-server: capturing {}x{}, encoding {}x{} with {} ({})",
        capturer.src_width,
        capturer.src_height,
        capturer.width,
        capturer.height,
        encoder.name(),
        encoder.codec()
    );

    if let Some(path) = arg("--out") {
        let seconds: u64 = arg("--seconds").map(|s| s.parse()).transpose()?.unwrap_or(5);
        return record(&mut capturer, encoder.as_mut(), &path, Duration::from_secs(seconds));
    }
    stream(&mut capturer, encoder.as_mut())
}

fn record(capturer: &mut Capturer, encoder: &mut dyn VideoEncoder, path: &str, duration: Duration) -> Result<()> {
    let mut file = std::fs::File::create(path)?;
    let mut bgra = Vec::new();
    let start = Instant::now();
    let (mut frames, mut bytes, mut encode_time, mut worst) = (0u32, 0usize, Duration::ZERO, Duration::ZERO);
    let mut have_image = false;
    while start.elapsed() < duration {
        // Wait (up to 1 s) for the first image even if the screen is idle.
        match capturer.next(if have_image { 100 } else { 1000 }, &mut bgra)? {
            Frame::Changed => have_image = true,
            Frame::Unchanged => continue,
        }
        let t = Instant::now();
        let (data, key) = encoder.encode(&bgra, start.elapsed().as_micros() as u64)?;
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

fn stream(capturer: &mut Capturer, encoder: &mut dyn VideoEncoder) -> Result<()> {
    // stdin reader: keyframe requests and input. EOF means the extension went away.
    let want_key = Arc::new(AtomicBool::new(false));
    let closed = Arc::new(AtomicBool::new(false));
    {
        let (want_key, closed) = (want_key.clone(), closed.clone());
        let mut input = input::Input::new(capturer.src_width, capturer.src_height);
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
                    _ => {}
                }
            }
            closed.store(true, Ordering::Relaxed);
        });
    }

    let mut out = std::io::stdout().lock();
    let hello_dims = [capturer.width.to_le_bytes(), capturer.height.to_le_bytes()].concat();
    write_msg(&mut out, 1, &[&hello_dims, encoder.codec().as_bytes()])?;

    let mut bgra = Vec::new();
    let start = Instant::now();
    let min_interval = Duration::from_secs_f32(1.0 / MAX_FPS as f32);
    let mut last_sent = Instant::now() - min_interval;
    // The viewer needs a first picture even if nothing on screen changes.
    let mut have_image = false;
    let mut need_send = true;
    while !closed.load(Ordering::Relaxed) {
        if let Frame::Changed = capturer.next(100, &mut bgra)? {
            have_image = true;
            need_send = true;
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
        let (data, key) = encoder.encode(&bgra, ts)?;
        last_sent = Instant::now();
        need_send = false;
        if data.is_empty() {
            continue; // encoder skipped the frame (rate control)
        }
        write_msg(&mut out, 2, &[&[key as u8], &ts.to_le_bytes(), &data])?;
    }
    Ok(())
}
