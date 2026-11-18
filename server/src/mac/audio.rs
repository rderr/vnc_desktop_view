//! Captures what the Mac is playing (ScreenCaptureKit's system audio capture,
//! macOS 13+) and encodes it as Opus: 48 kHz stereo, 20 ms packets.
//!
//! ScreenCaptureKit only captures audio as part of a stream with a picture, so
//! this runs its own stream with a tiny, once-a-second picture that is
//! ignored. The stream only runs while the viewer has sound on. Needs the
//! "Screen & System Audio Recording" permission, like the screen capture.

use super::capture::{Sendable, main_display, wait_for};
use anyhow::{Result, anyhow, bail};
use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_core_audio_types::{AudioBuffer, AudioBufferList};
use objc2_core_media::{CMAudioFormatDescriptionGetStreamBasicDescription, CMBlockBuffer, CMSampleBuffer, CMTime};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{SCContentFilter, SCStream, SCStreamConfiguration, SCStreamDelegate, SCStreamOutput, SCStreamOutputType};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const RATE: u32 = 48_000;
const FRAME: usize = 960; // 20 ms at 48 kHz
const BITRATE: i32 = 128_000;
/// Drop audio that piles up beyond this (the encoder fell behind): ~0.5 s of stereo.
const MAX_PENDING: usize = RATE as usize;

#[derive(Default)]
struct Pending {
    /// Interleaved stereo samples not yet encoded.
    samples: Vec<f32>,
    stopped: Option<String>,
    reported_format: bool,
}

#[derive(Default)]
struct Shared {
    pending: Mutex<Pending>,
    ready: Condvar,
}

struct Ivars {
    shared: Arc<Shared>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; we don't implement Drop.
    #[unsafe(super(NSObject))]
    #[name = "DesktopServerAudioOutput"]
    #[ivars = Ivars]
    struct Output;

    unsafe impl NSObjectProtocol for Output {}

    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn stream_did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            if kind != SCStreamOutputType::Audio {
                return; // the stand-in picture
            }
            let shared = &self.ivars().shared;
            let mut pending = shared.pending.lock().unwrap();
            if let Err(e) = unsafe { append_stereo(sample, &mut pending.samples) } {
                if !std::mem::replace(&mut pending.reported_format, true) {
                    eprintln!("desktop-server: audio: {e:#}");
                }
                return;
            }
            let excess = pending.samples.len().saturating_sub(MAX_PENDING);
            pending.samples.drain(..excess);
            shared.ready.notify_all();
        }
    }

    unsafe impl SCStreamDelegate for Output {
        #[unsafe(method(stream:didStopWithError:))]
        unsafe fn stream_did_stop(&self, _stream: &SCStream, error: &NSError) {
            let shared = &self.ivars().shared;
            shared.pending.lock().unwrap().stopped = Some(error.localizedDescription().to_string());
            shared.ready.notify_all();
        }
    }
);

impl Output {
    fn new(shared: Arc<Shared>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(Ivars { shared });
        unsafe { msg_send![super(this), init] }
    }
}

/// Appends the buffer's audio as interleaved stereo f32 (it arrives as 32-bit
/// float, usually one buffer per channel).
unsafe fn append_stereo(sample: &CMSampleBuffer, out: &mut Vec<f32>) -> Result<()> {
    unsafe {
        let format = sample.format_description().ok_or_else(|| anyhow!("no audio format"))?;
        let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(&format);
        let asbd = asbd.as_ref().ok_or_else(|| anyhow!("not an audio format"))?;
        const FLOAT: u32 = 1; // kAudioFormatFlagIsFloat
        if asbd.mFormatFlags & FLOAT == 0 || asbd.mBitsPerChannel != 32 {
            bail!("unexpected audio format ({} bits, flags {:#x})", asbd.mBitsPerChannel, asbd.mFormatFlags);
        }
        if std::env::var_os("DS_DEBUG").is_some() {
            eprintln!(
                "    audio buffer: {} samples, ready {}, {} Hz, {} ch, {} bits, flags {:#x}, {} bytes/frame",
                sample.num_samples(),
                sample.data_is_ready(),
                asbd.mSampleRate,
                asbd.mChannelsPerFrame,
                asbd.mBitsPerChannel,
                asbd.mFormatFlags,
                asbd.mBytesPerFrame
            );
        }
        if asbd.mSampleRate as u32 != RATE {
            bail!("unexpected sample rate {}", asbd.mSampleRate);
        }
        // Ask how big the AudioBufferList must be, then get it (16-byte aligned storage).
        const ASSURE_16_BYTE_ALIGNMENT: u32 = 1; // kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment
        let mut needed = 0usize;
        sample.audio_buffer_list_with_retained_block_buffer(
            &mut needed,
            std::ptr::null_mut(),
            0,
            None,
            None,
            ASSURE_16_BYTE_ALIGNMENT,
            std::ptr::null_mut(),
        );
        if std::env::var_os("DS_DEBUG").is_some() {
            eprintln!("    audio buffer list needs {needed} bytes");
        }
        if needed == 0 {
            bail!("no audio buffer list");
        }
        let mut storage = vec![0u128; needed.div_ceil(16)];
        let list = storage.as_mut_ptr() as *mut AudioBufferList;
        let mut block: *mut CMBlockBuffer = std::ptr::null_mut();
        // The size must be exactly what was asked for.
        let status = sample.audio_buffer_list_with_retained_block_buffer(
            std::ptr::null_mut(),
            list,
            needed,
            None,
            None,
            ASSURE_16_BYTE_ALIGNMENT,
            &mut block,
        );
        if status != 0 {
            bail!("reading audio failed (OSStatus {status})");
        }
        // The retained block buffer keeps the data alive until we've copied it.
        let _block = (!block.is_null()).then(|| objc2_core_foundation::CFRetained::from_raw(std::ptr::NonNull::new_unchecked(block)));
        let count = (*list).mNumberBuffers as usize;
        let buffers = std::slice::from_raw_parts((*list).mBuffers.as_ptr() as *const AudioBuffer, count);
        let floats = |b: &AudioBuffer| std::slice::from_raw_parts(b.mData as *const f32, b.mDataByteSize as usize / 4);
        match buffers {
            // One buffer per channel (non-interleaved).
            [left, right, ..] => {
                for (l, r) in floats(left).iter().zip(floats(right)) {
                    out.extend_from_slice(&[*l, *r]);
                }
            }
            // One interleaved buffer: stereo, or mono to duplicate.
            [only] if only.mNumberChannels >= 2 => {
                let channels = only.mNumberChannels as usize;
                for frame in floats(only).chunks_exact(channels) {
                    out.extend_from_slice(&frame[..2]);
                }
            }
            [only] => {
                for s in floats(only) {
                    out.extend_from_slice(&[*s, *s]);
                }
            }
            [] => {}
        }
        Ok(())
    }
}

/// An audio-only capture, running until dropped.
struct Capture {
    stream: Retained<SCStream>,
    _output: Retained<Output>,
    _queue: dispatch2::DispatchRetained<DispatchQueue>,
}

impl Capture {
    fn start(shared: Arc<Shared>) -> Result<Self> {
        unsafe {
            let display = main_display()?;
            let filter = SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), &display, &NSArray::new());
            let config = SCStreamConfiguration::new();
            config.setCapturesAudio(true);
            config.setSampleRate(RATE as isize);
            config.setChannelCount(2);
            config.setExcludesCurrentProcessAudio(true);
            // The picture ScreenCaptureKit insists on: tiny and once a second.
            config.setWidth(2);
            config.setHeight(2);
            config.setMinimumFrameInterval(CMTime::new(1, 1));
            config.setQueueDepth(3);

            let output = Output::new(shared);
            let stream = SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &config, Some(ProtocolObject::from_ref(&*output)));
            let queue = DispatchQueue::new("desktop-server.audio", None);
            for kind in [SCStreamOutputType::Audio, SCStreamOutputType::Screen] {
                stream
                    .addStreamOutput_type_sampleHandlerQueue_error(ProtocolObject::from_ref(&*output), kind, Some(&queue))
                    .map_err(|e| anyhow!("adding the audio output failed: {}", e.localizedDescription()))?;
            }
            wait_for(|tx| {
                let block = RcBlock::new(move |error: *mut NSError| {
                    let _ = tx.send(Sendable(error.as_ref().map(|e| e.localizedDescription().to_string())));
                });
                stream.startCaptureWithCompletionHandler(Some(&block));
            })?
            .0
            .map_or(Ok(()), |e| Err(anyhow!("starting audio capture failed: {e}")))?;
            Ok(Self { stream, _output: output, _queue: queue })
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        unsafe { self.stream.stopCaptureWithCompletionHandler(None) };
    }
}

/// Runs until `closed` is set. While `enabled` is set, captures and calls
/// `send(timestamp_us, opus_packet)` for each packet. Retries after errors.
pub fn run(
    enabled: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    start: Instant,
    mut send: impl FnMut(u64, &[u8]) -> std::io::Result<()>,
) {
    let mut reported = false;
    while !closed.load(Ordering::Relaxed) {
        if !enabled.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        match capture(&enabled, &closed, start, &mut send) {
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

/// Captures until sound is turned off or the viewer goes away.
fn capture(
    enabled: &AtomicBool,
    closed: &AtomicBool,
    start: Instant,
    send: &mut impl FnMut(u64, &[u8]) -> std::io::Result<()>,
) -> Result<()> {
    let shared = Arc::new(Shared::default());
    let _capture = Capture::start(shared.clone())?;
    eprintln!("desktop-server: audio: capturing system audio -> Opus 48000 Hz stereo");
    let mut encoder = opus::Encoder::new(RATE, opus::Channels::Stereo, opus::Application::LowDelay)?;
    encoder.set_bitrate(opus::Bitrate::Bits(BITRATE))?;
    let mut packet = vec![0u8; 4000];
    let mut frame = vec![0f32; FRAME * 2];
    while enabled.load(Ordering::Relaxed) && !closed.load(Ordering::Relaxed) {
        {
            let mut pending = shared.pending.lock().unwrap();
            if let Some(why) = pending.stopped.take() {
                bail!("audio capture stopped: {why}");
            }
            if pending.samples.len() < FRAME * 2 {
                pending = shared.ready.wait_timeout(pending, Duration::from_millis(100)).unwrap().0;
                if pending.samples.len() < FRAME * 2 {
                    continue;
                }
            }
            frame.copy_from_slice(&pending.samples[..FRAME * 2]);
            pending.samples.drain(..FRAME * 2);
        }
        let len = encoder.encode_float(&frame, &mut packet)?;
        if send(start.elapsed().as_micros() as u64, &packet[..len]).is_err() {
            return Ok(()); // the viewer went away
        }
    }
    Ok(())
}
