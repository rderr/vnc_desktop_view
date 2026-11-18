//! macOS screen capture with ScreenCaptureKit.
//!
//! The main display is captured at its full pixel resolution, as NV12 (the
//! encoder's input format, converted by the system on the GPU). ScreenCaptureKit
//! calls us on a dispatch queue with each frame; frames whose status is
//! "complete" carry new content, "idle" ones mean nothing changed. The latest
//! frame is kept (as an IOSurface-backed pixel buffer) for the encoder.
//!
//! Needs the Screen Recording permission (System Settings > Privacy & Security).

use anyhow::{Context, Result, anyhow, bail};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString};
use objc2_core_graphics::{CGDisplayCopyDisplayMode, CGDisplayMode, CGMainDisplayID};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::CVPixelBuffer;
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamDelegate, SCStreamFrameInfoStatus,
    SCFrameStatus, SCStreamOutput, SCStreamOutputType,
};
use std::ffi::c_void;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange ('420v'): NV12, limited range.
const PIXEL_FORMAT_NV12: u32 = u32::from_be_bytes(*b"420v");

/// A frame handed between ScreenCaptureKit's queue and the capture loop.
struct PixelBuffer(CFRetained<CVPixelBuffer>);
// SAFETY: CVPixelBuffer is a reference-counted, thread-safe CoreFoundation object.
unsafe impl Send for PixelBuffer {}

#[derive(Default)]
struct State {
    latest: Option<PixelBuffer>,
    /// Counts frames with new content.
    seq: u64,
    stopped: Option<String>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

struct Ivars {
    shared: Arc<Shared>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; we don't implement Drop.
    #[unsafe(super(NSObject))]
    #[name = "DesktopServerStreamOutput"]
    #[ivars = Ivars]
    struct Output;

    unsafe impl NSObjectProtocol for Output {}

    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn stream_did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            if kind != SCStreamOutputType::Screen || unsafe { frame_status(sample) } != Some(SCFrameStatus::Complete) {
                return; // idle (unchanged), blank, suspended… frames carry no new image
            }
            let Some(image) = (unsafe { sample.image_buffer() }) else { return };
            let shared = &self.ivars().shared;
            let mut state = shared.state.lock().unwrap();
            state.latest = Some(PixelBuffer(image));
            state.seq += 1;
            shared.changed.notify_all();
        }
    }

    unsafe impl SCStreamDelegate for Output {
        #[unsafe(method(stream:didStopWithError:))]
        unsafe fn stream_did_stop(&self, _stream: &SCStream, error: &NSError) {
            let shared = &self.ivars().shared;
            shared.state.lock().unwrap().stopped = Some(error.localizedDescription().to_string());
            shared.changed.notify_all();
        }
    }
);

impl Output {
    fn new(shared: Arc<Shared>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(Ivars { shared });
        unsafe { msg_send![super(this), init] }
    }
}

/// The frame's SCFrameStatus, from its sample attachments.
unsafe fn frame_status(sample: &CMSampleBuffer) -> Option<SCFrameStatus> {
    unsafe {
        let attachments = sample.sample_attachments_array(false)?;
        if attachments.count() < 1 {
            return None;
        }
        let dict = attachments.value_at_index(0) as *const CFDictionary;
        let key: &CFString = (SCStreamFrameInfoStatus as &objc2_foundation::NSString).as_ref();
        let value = (*dict).value(key as *const CFString as *const c_void) as *const CFNumber;
        Some(SCFrameStatus((*value.as_ref()?).as_i64()? as isize))
    }
}

pub struct Capturer {
    stream: Retained<SCStream>,
    _output: Retained<Output>,
    _queue: DispatchRetained<DispatchQueue>,
    shared: Arc<Shared>,
    /// Last frame `next` reported.
    seen: u64,
    pub src_width: u32,
    pub src_height: u32,
    /// Size of the encoded frames (even, as H.264 4:2:0 needs).
    pub width: u32,
    pub height: u32,
}

pub enum Frame {
    /// The screen changed; `pixel_buffer()` holds the new image.
    Changed,
    /// Capture stopped (display change, permission revoked…); create a new Capturer.
    Lost(String),
    /// Nothing changed on screen within the timeout.
    Unchanged,
}

/// Runs an API that reports through a completion block, and waits for it.
fn wait_for<T: Send + 'static>(start: impl FnOnce(mpsc::Sender<T>)) -> Result<T> {
    let (tx, rx) = mpsc::channel();
    start(tx);
    rx.recv_timeout(Duration::from_secs(10)).map_err(|_| anyhow!("ScreenCaptureKit didn't answer"))
}

struct Sendable<T>(T);
// SAFETY: only used to carry ScreenCaptureKit results (thread-safe objects) out of a completion block.
unsafe impl<T> Send for Sendable<T> {}

impl Capturer {
    pub fn new() -> Result<Self> {
        unsafe {
            let content = wait_for(|tx| {
                let block = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
                    let result = match (Retained::retain(content), error.as_ref()) {
                        (Some(c), _) => Ok(Sendable(c)),
                        (None, Some(e)) => Err(e.localizedDescription().to_string()),
                        (None, None) => Err("no shareable content".into()),
                    };
                    let _ = tx.send(result);
                });
                SCShareableContent::getShareableContentWithCompletionHandler(&block);
            })?
            .map_err(|e| anyhow!("{e} (does this program have the Screen Recording permission?)"))?
            .0;

            let main_id = CGMainDisplayID();
            let displays = content.displays();
            let display: Retained<SCDisplay> = displays
                .iter()
                .find(|d| d.displayID() == main_id)
                .or_else(|| displays.firstObject())
                .context("no display to capture")?;
            // Pixel size (Retina displays have more pixels than points).
            let mode: Option<CFRetained<CGDisplayMode>> = CGDisplayCopyDisplayMode(display.displayID());
            let (src_width, src_height) = match &mode {
                Some(m) => (CGDisplayMode::pixel_width(Some(m)) as u32, CGDisplayMode::pixel_height(Some(m)) as u32),
                None => (display.width() as u32, display.height() as u32),
            };
            let (width, height) = fit_h264(src_width, src_height);

            let filter = SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), &display, &NSArray::new());
            let config = SCStreamConfiguration::new();
            config.setWidth(width as usize);
            config.setHeight(height as usize);
            config.setPixelFormat(PIXEL_FORMAT_NV12);
            config.setMinimumFrameInterval(CMTime::new(1, crate::encoder::MAX_FPS as i32));
            config.setShowsCursor(false); // the viewer shows its own cursor
            config.setQueueDepth(5);

            let shared = Arc::new(Shared::default());
            let output = Output::new(shared.clone());
            let stream = SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                Some(ProtocolObject::from_ref(&*output)),
            );
            let queue = DispatchQueue::new("desktop-server.capture", None);
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(ProtocolObject::from_ref(&*output), SCStreamOutputType::Screen, Some(&queue))
                .map_err(|e| anyhow!("adding the stream output failed: {}", e.localizedDescription()))?;
            wait_for(|tx| {
                let block = RcBlock::new(move |error: *mut NSError| {
                    let _ = tx.send(error.as_ref().map(|e| e.localizedDescription().to_string()));
                });
                stream.startCaptureWithCompletionHandler(Some(&block));
            })?
            .map_or(Ok(()), |e| Err(anyhow!("starting screen capture failed: {e}")))?;

            Ok(Self { stream, _output: output, _queue: queue, shared, seen: 0, src_width, src_height, width, height })
        }
    }

    /// Waits up to `timeout_ms` for the screen to change.
    pub fn next(&mut self, timeout_ms: u32) -> Result<Frame> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
        let mut state = self.shared.state.lock().unwrap();
        loop {
            if let Some(why) = state.stopped.take() {
                return Ok(Frame::Lost(why));
            }
            if state.seq != self.seen {
                self.seen = state.seq;
                return Ok(Frame::Changed);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(Frame::Unchanged);
            }
            state = self.shared.changed.wait_timeout(state, left).unwrap().0;
        }
    }

    /// The latest image (NV12, `width` x `height`).
    pub fn pixel_buffer(&self) -> Option<CFRetained<CVPixelBuffer>> {
        self.shared.state.lock().unwrap().latest.as_ref().map(|p| p.0.clone())
    }

    #[allow(dead_code)] // only the OpenH264 test encoder would use it
    pub fn read_bgra(&self, _buf: &mut Vec<u8>) -> Result<()> {
        bail!("BGRA frames aren't available on macOS (capture is NV12)")
    }
}

impl Drop for Capturer {
    fn drop(&mut self) {
        unsafe { self.stream.stopCaptureWithCompletionHandler(None) };
    }
}

/// The capture size: the display's pixel size, scaled down (ScreenCaptureKit
/// scales on the GPU) if needed to fit H.264 level 5.2 at 60 fps, which common
/// decoders handle: 2073600 macroblocks per second, so 34560 per frame (e.g.
/// 3840x2304; a 4K display fits as is). Scaled Retina modes can be larger (a
/// 1920x1243-point mode renders at 3840x2486). Even sizes.
fn fit_h264(width: u32, height: u32) -> (u32, u32) {
    const MAX_MBS: f64 = 2_073_600.0 / crate::encoder::MAX_FPS as f64;
    let mbs = (width.div_ceil(16) * height.div_ceil(16)) as f64;
    let scale = (MAX_MBS / mbs).sqrt().min(4096.0 / width as f64).min(4096.0 / height as f64).min(1.0);
    // Round down to even; shrink a little more if rounding up the macroblocks still overshoots.
    let mut s = scale;
    loop {
        let (w, h) = (((width as f64 * s) as u32) & !1, ((height as f64 * s) as u32) & !1);
        if (w.div_ceil(16) * h.div_ceil(16)) as f64 <= MAX_MBS {
            return (w, h);
        }
        s *= 0.995;
    }
}

/// macOS shows the lock screen through the same capture (as a blank or the lock screen).
pub fn is_secure_desktop(_err: &anyhow::Error) -> bool {
    false
}
