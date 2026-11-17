//! desktop-server: captures the desktop and streams it as H.264.
//!
//! Modes:
//!   desktop-server                         stream framed messages on stdout (see PROTOCOL)
//!   desktop-server --out FILE [--seconds N] write a raw Annex B .h264 file (for testing)
//!
//! PROTOCOL (stdout, little-endian): each message is `u8 kind, u32 len, payload[len]`.
//!   kind 1 HELLO  payload: u32 width, u32 height, codec string (UTF-8, e.g. "avc1.42E01F")
//!   kind 2 VIDEO  payload: u8 flags (bit 0 = keyframe), u64 timestamp_us, Annex B data
//! stdin: same framing.
//!   kind 1 KEYFRAME  (no payload) – send a keyframe next
//!   kind 2 POINTER   u16 x, u16 y (desktop pixels), u8 buttons (1 left, 2 right, 4 middle),
//!                    i16 wheel_y, i16 wheel_x (Windows wheel units, 120 = one notch, + = up/right)
//!   kind 3 KEY       u16 scan code (set 1, 0xE0xx = extended), u8 down

mod capture;
mod input;

use anyhow::{Context, Result};
use capture::{Capturer, Frame};
use openh264::OpenH264API;
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode, UsageType,
};
use openh264::formats::{BgraSliceU8, YUVBuffer};
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const MAX_FPS: f32 = 60.0;
/// Constrained baseline, level 5.1 (fine up to 4K). Must match what the encoder produces.
const CODEC: &str = "avc1.42E033";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();

    let mut capturer = Capturer::new()?;
    eprintln!(
        "desktop-server: capturing {}x{} (encoding {}x{})",
        capturer.src_width, capturer.src_height, capturer.width, capturer.height
    );
    let mut encoder = new_encoder()?;

    if let Some(path) = arg("--out") {
        let seconds: u64 = arg("--seconds").map(|s| s.parse()).transpose()?.unwrap_or(5);
        return record(&mut capturer, &mut encoder, &path, Duration::from_secs(seconds));
    }
    stream(&mut capturer, &mut encoder)
}

fn new_encoder() -> Result<Encoder> {
    let config = EncoderConfig::new()
        .usage_type(UsageType::CameraVideoRealTime)
        .rate_control_mode(RateControlMode::Bitrate)
        .bitrate(BitRate::from_bps(8_000_000))
        .max_frame_rate(FrameRate::from_hz(MAX_FPS))
        .complexity(Complexity::Low)
        // Keyframes only on request (new viewer, lost data), not periodically.
        .intra_frame_period(IntraFramePeriod::from_num_frames(0))
        .skip_frames(true);
    Encoder::with_api_config(OpenH264API::from_source(), config).context("creating H.264 encoder")
}

/// Encodes one captured BGRA frame; returns (annex B bytes, is keyframe).
fn encode(encoder: &mut Encoder, cap: &Capturer, bgra: &[u8], yuv: &mut Option<YUVBuffer>) -> Result<(Vec<u8>, bool)> {
    let (w, h) = (cap.width as usize, cap.height as usize);
    let src = BgraSliceU8::new(bgra, (w, h));
    match yuv {
        Some(buf) => buf.read_bgra8(src),
        None => *yuv = Some(YUVBuffer::from_bgra8_source(src)),
    }
    let bs = encoder.encode(yuv.as_ref().unwrap())?;
    let key = matches!(bs.frame_type(), FrameType::IDR | FrameType::I);
    Ok((bs.to_vec(), key))
}

fn record(capturer: &mut Capturer, encoder: &mut Encoder, path: &str, duration: Duration) -> Result<()> {
    let mut file = std::fs::File::create(path)?;
    let (mut bgra, mut yuv) = (Vec::new(), None);
    let start = Instant::now();
    let (mut frames, mut bytes, mut encode_time) = (0u32, 0usize, Duration::ZERO);
    // Always start with a frame, even if the screen is idle.
    let mut force = true;
    while start.elapsed() < duration {
        let frame = capturer.next(if force { 1000 } else { 100 }, &mut bgra)?;
        if let Frame::Unchanged = frame {
            if !force {
                continue;
            }
        }
        if let Frame::Image(_) = frame {
            force = false;
        } else {
            continue; // forced but still nothing yet: keep waiting
        }
        let t = Instant::now();
        let (data, key) = encode(encoder, capturer, &bgra, &mut yuv)?;
        encode_time += t.elapsed();
        if std::env::var_os("DS_DEBUG").is_some() {
            eprintln!("  frame {frames}: {:.1} ms, {} bytes{}", t.elapsed().as_secs_f64() * 1000.0, data.len(), if key { " KEY" } else { "" });
        }
        file.write_all(&data)?;
        frames += 1;
        bytes += data.len();
    }
    let secs = start.elapsed().as_secs_f64();
    eprintln!(
        "desktop-server: {frames} frames in {secs:.1}s ({:.1} fps), {:.0} KB/s, avg encode {:.1} ms",
        frames as f64 / secs,
        bytes as f64 / secs / 1000.0,
        if frames > 0 { encode_time.as_secs_f64() * 1000.0 / frames as f64 } else { 0.0 }
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

fn stream(capturer: &mut Capturer, encoder: &mut Encoder) -> Result<()> {
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
    write_msg(&mut out, 1, &[&hello_dims, CODEC.as_bytes()])?;

    let (mut bgra, mut yuv) = (Vec::new(), None);
    let start = Instant::now();
    let min_interval = Duration::from_secs_f32(1.0 / MAX_FPS);
    let mut last_sent = Instant::now() - min_interval;
    // The viewer needs a first picture even if nothing on screen changes.
    let mut have_image = false;
    let mut need_send = true;
    while !closed.load(Ordering::Relaxed) {
        match capturer.next(100, &mut bgra)? {
            Frame::Image(_) => {
                have_image = true;
                need_send = true;
            }
            Frame::Unchanged => {}
        }
        if want_key.swap(false, Ordering::Relaxed) {
            encoder.force_intra_frame();
            need_send = true;
        }
        if !need_send || !have_image {
            continue;
        }
        let since = last_sent.elapsed();
        if since < min_interval {
            std::thread::sleep(min_interval - since);
        }
        let (data, key) = encode(encoder, capturer, &bgra, &mut yuv)?;
        last_sent = Instant::now();
        need_send = false;
        if data.is_empty() {
            continue; // encoder skipped the frame (rate control)
        }
        let ts = (start.elapsed().as_micros() as u64).to_le_bytes();
        write_msg(&mut out, 2, &[&[key as u8], &ts, &data])?;
    }
    Ok(())
}
