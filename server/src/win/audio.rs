//! Captures what the default speakers are playing (WASAPI loopback) and
//! encodes it as Opus: 48 kHz stereo, 20 ms packets.

use anyhow::{Context, Result, bail};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::*;

const RATE: u32 = 48_000;
const FRAME: usize = 960; // 20 ms at 48 kHz
const BITRATE: i32 = 128_000;

#[derive(Clone, Copy, PartialEq)]
enum SampleFormat {
    F32,
    I16,
    I32,
}

/// Runs until `closed` is set. `send(timestamp_us, opus_packet)` is called for
/// each packet while `enabled` is set. Recovers from device changes (e.g. the
/// default output switching to headphones) by starting over.
pub fn run(
    enabled: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    start: Instant,
    mut send: impl FnMut(u64, &[u8]) -> std::io::Result<()>,
) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    while !closed.load(Ordering::Relaxed) {
        if let Err(e) = capture(&enabled, &closed, start, &mut send) {
            eprintln!("desktop-server: audio: {e:#}; retrying");
            std::thread::sleep(Duration::from_secs(2));
        }
    }
}

fn capture(
    enabled: &AtomicBool,
    closed: &AtomicBool,
    start: Instant,
    send: &mut impl FnMut(u64, &[u8]) -> std::io::Result<()>,
) -> Result<()> {
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).context("no audio output device")?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;

        let fmt_ptr = client.GetMixFormat()?;
        let fmt = std::ptr::read_unaligned(fmt_ptr);
        let (channels, in_rate, bits) = (fmt.nChannels as usize, fmt.nSamplesPerSec, fmt.wBitsPerSample);
        let tag = fmt.wFormatTag as u32;
        let sample_format = if tag == WAVE_FORMAT_EXTENSIBLE {
            let ext = std::ptr::read_unaligned(fmt_ptr as *const WAVEFORMATEXTENSIBLE);
            let sub = ext.SubFormat;
            if sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT && bits == 32 {
                SampleFormat::F32
            } else if sub == KSDATAFORMAT_SUBTYPE_PCM && bits == 16 {
                SampleFormat::I16
            } else if sub == KSDATAFORMAT_SUBTYPE_PCM && bits == 32 {
                SampleFormat::I32
            } else {
                bail!("unsupported mix format ({bits}-bit)");
            }
        } else if tag == WAVE_FORMAT_IEEE_FLOAT && bits == 32 {
            SampleFormat::F32
        } else if tag == 1 && bits == 16 {
            SampleFormat::I16
        } else {
            bail!("unsupported mix format (tag {tag}, {bits}-bit)");
        };

        // 100 ms buffer; loopback captures the device's mix as-is.
        let init = client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, 1_000_000, 0, fmt_ptr, None);
        CoTaskMemFree(Some(fmt_ptr as _));
        init.context("IAudioClient::Initialize (loopback)")?;
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        eprintln!("desktop-server: audio: capturing {in_rate} Hz, {channels} ch -> Opus {RATE} Hz stereo");

        let mut encoder = opus::Encoder::new(RATE, opus::Channels::Stereo, opus::Application::LowDelay)?;
        encoder.set_bitrate(opus::Bitrate::Bits(BITRATE))?;
        let mut resampler = Resampler::new(in_rate);
        let mut pending: Vec<f32> = Vec::with_capacity(FRAME * 4);
        let mut stereo: Vec<f32> = Vec::new();
        let mut packet = vec![0u8; 4000];

        while !closed.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(5));
            loop {
                let frames_available = capture.GetNextPacketSize()?;
                if frames_available == 0 {
                    break;
                }
                let mut data = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                stereo.clear();
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                for f in 0..frames as usize {
                    let (l, r) = if silent {
                        (0.0, 0.0)
                    } else {
                        let get = |c: usize| -> f32 {
                            let i = f * channels + c.min(channels - 1);
                            match sample_format {
                                SampleFormat::F32 => *(data as *const f32).add(i),
                                SampleFormat::I16 => *(data as *const i16).add(i) as f32 / 32768.0,
                                SampleFormat::I32 => *(data as *const i32).add(i) as f32 / 2147483648.0,
                            }
                        };
                        (get(0), get(1))
                    };
                    stereo.push(l);
                    stereo.push(r);
                }
                capture.ReleaseBuffer(frames)?;
                resampler.push(&stereo, &mut pending);
            }
            while pending.len() >= FRAME * 2 {
                let len = encoder.encode_float(&pending[..FRAME * 2], &mut packet)?;
                pending.drain(..FRAME * 2);
                if enabled.load(Ordering::Relaxed) {
                    send(start.elapsed().as_micros() as u64, &packet[..len])?;
                }
            }
        }
        let _ = client.Stop();
        Ok(())
    }
}

/// Linear-interpolation resampler for interleaved stereo, to 48 kHz.
struct Resampler {
    step: f64,
    pos: f64,
    last: [f32; 2],
}

impl Resampler {
    fn new(in_rate: u32) -> Self {
        Self { step: in_rate as f64 / RATE as f64, pos: 0.0, last: [0.0; 2] }
    }

    fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.step == 1.0 {
            out.extend_from_slice(input);
            return;
        }
        let n = input.len() / 2;
        // Sample i of the virtual stream [last, input...] at index i.
        let at = |i: usize, c: usize| if i == 0 { self.last[c] } else { input[(i - 1) * 2 + c] };
        while self.pos + 1.0 <= n as f64 {
            let i = self.pos.floor() as usize;
            let frac = (self.pos - i as f64) as f32;
            for c in 0..2 {
                out.push(at(i, c) * (1.0 - frac) + at(i + 1, c) * frac);
            }
            self.pos += self.step;
        }
        self.pos -= n as f64;
        if n > 0 {
            self.last = [input[(n - 1) * 2], input[(n - 1) * 2 + 1]];
        }
    }
}
