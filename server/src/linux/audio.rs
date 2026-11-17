//! Captures what the default speakers are playing (the default sink's monitor,
//! through PulseAudio or PipeWire's PulseAudio server) and encodes it as Opus:
//! 48 kHz stereo, 20 ms packets.
//!
//! libpulse-simple is loaded at runtime, so the server still runs where it's
//! missing (sound just stays off). The stream is only open while the viewer
//! has sound on.

use anyhow::{Context, Result, bail};
use libloading::Library;
use std::ffi::{c_char, c_int, c_void};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const RATE: u32 = 48_000;
const FRAME: usize = 960; // 20 ms at 48 kHz
const BITRATE: i32 = 128_000;

const PA_STREAM_RECORD: c_int = 2;
const PA_SAMPLE_FLOAT32LE: c_int = 5;

#[repr(C)]
struct SampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}

#[repr(C)]
struct BufferAttr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}

type NewFn = unsafe extern "C" fn(
    *const c_char,
    *const c_char,
    c_int,
    *const c_char,
    *const c_char,
    *const SampleSpec,
    *const c_void,
    *const BufferAttr,
    *mut c_int,
) -> *mut c_void;
type ReadFn = unsafe extern "C" fn(*mut c_void, *mut c_void, usize, *mut c_int) -> c_int;
type FreeFn = unsafe extern "C" fn(*mut c_void);

/// Runs until `closed` is set. While `enabled` is set, records and calls
/// `send(timestamp_us, opus_packet)` for each packet. Retries after errors
/// (e.g. the sound server restarting).
pub fn run(
    enabled: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    start: Instant,
    mut send: impl FnMut(u64, &[u8]) -> std::io::Result<()>,
) {
    let lib = match unsafe { Library::new("libpulse-simple.so.0") } {
        Ok(lib) => lib,
        Err(e) => {
            eprintln!("desktop-server: audio unavailable: can't load libpulse-simple.so.0 ({e})");
            return;
        }
    };
    let mut reported = false;
    while !closed.load(Ordering::Relaxed) {
        if !enabled.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        match capture(&lib, &enabled, &closed, start, &mut send) {
            Ok(()) => reported = false,
            Err(e) => {
                if !std::mem::replace(&mut reported, true) {
                    eprintln!("desktop-server: audio: {e:#}; retrying");
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }
}

/// Records until sound is turned off or the viewer goes away.
fn capture(
    lib: &Library,
    enabled: &AtomicBool,
    closed: &AtomicBool,
    start: Instant,
    send: &mut impl FnMut(u64, &[u8]) -> std::io::Result<()>,
) -> Result<()> {
    unsafe {
        let new = *lib.get::<NewFn>(b"pa_simple_new\0")?;
        let read = *lib.get::<ReadFn>(b"pa_simple_read\0")?;
        let free = *lib.get::<FreeFn>(b"pa_simple_free\0")?;
        let spec = SampleSpec { format: PA_SAMPLE_FLOAT32LE, rate: RATE, channels: 2 };
        let frame_bytes = (FRAME * 2 * 4) as u32;
        // Small fragments: data arrives every 20 ms instead of in large batches.
        let attr = BufferAttr { maxlength: u32::MAX, tlength: u32::MAX, prebuf: u32::MAX, minreq: u32::MAX, fragsize: frame_bytes };
        let mut error = 0;
        let stream = new(
            std::ptr::null(),
            c"Remote Desktop View".as_ptr(),
            PA_STREAM_RECORD,
            c"@DEFAULT_MONITOR@".as_ptr(),
            c"Remote desktop sound".as_ptr(),
            &spec,
            std::ptr::null(),
            &attr,
            &mut error,
        );
        if stream.is_null() {
            bail!("connecting to the sound server failed (PulseAudio error {error})");
        }
        struct Stream(*mut c_void, FreeFn);
        impl Drop for Stream {
            fn drop(&mut self) {
                unsafe { (self.1)(self.0) };
            }
        }
        let stream = Stream(stream, free);
        eprintln!("desktop-server: audio: capturing the default output's monitor -> Opus 48000 Hz stereo");

        let mut encoder = opus::Encoder::new(RATE, opus::Channels::Stereo, opus::Application::LowDelay)?;
        encoder.set_bitrate(opus::Bitrate::Bits(BITRATE))?;
        let mut samples = vec![0f32; FRAME * 2];
        let mut packet = vec![0u8; 4000];
        while enabled.load(Ordering::Relaxed) && !closed.load(Ordering::Relaxed) {
            if read(stream.0, samples.as_mut_ptr() as *mut c_void, samples.len() * 4, &mut error) < 0 {
                bail!("reading sound failed (PulseAudio error {error})");
            }
            let len = encoder.encode_float(&samples, &mut packet).context("Opus")?;
            let ts = start.elapsed().as_micros() as u64;
            if send(ts, &packet[..len]).is_err() {
                return Ok(()); // the viewer went away
            }
        }
        Ok(())
    }
}
