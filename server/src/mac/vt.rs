//! H.264 with VideoToolbox, the Mac's hardware video encoder.
//!
//! The capture's pixel buffers (NV12, on the GPU) go straight in. Low-latency
//! rate control (macOS 11.3+) with Constrained Baseline: one frame in, one
//! frame out, no reordering, like the Windows server. VideoToolbox produces
//! length-prefixed NAL units (AVCC) with SPS/PPS kept in the format description;
//! the viewer wants Annex B with SPS/PPS before keyframes, so we convert.

use super::capture::Capturer;
use crate::encoder::{MAX_FPS, VideoEncoder, bitrate};
use anyhow::{Result, anyhow, bail};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMFormatDescription, CMSampleBuffer, CMTime, CMVideoFormatDescriptionGetH264ParameterSetAtIndex, kCMSampleAttachmentKey_NotSync,
    kCMTimeInvalid, kCMVideoCodecType_H264,
};
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, VTSessionSetProperty, kVTCompressionPropertyKey_AllowFrameReordering,
    kVTCompressionPropertyKey_AverageBitRate, kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime, kVTEncodeFrameOptionKey_ForceKeyFrame,
    kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel, kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
};
use std::ffi::{c_char, c_int, c_void};
use std::ptr::NonNull;
use std::sync::Mutex;

/// What the output callback produced for the frame being encoded.
#[derive(Default)]
struct Output {
    frame: Option<(Vec<u8>, bool)>,
    error: Option<String>,
}

pub struct VtEncoder {
    session: CFRetained<VTCompressionSession>,
    /// Boxed so its address (handed to the callback) stays put.
    output: Box<Mutex<Output>>,
    force_key: bool,
    codec: String,
}

// SAFETY: the session is only used from the capture loop's thread; VideoToolbox
// itself is thread-safe.
unsafe impl Send for VtEncoder {}

impl VtEncoder {
    pub fn new(cap: &Capturer) -> Result<Self> {
        let (width, height) = (cap.width, cap.height);
        let output = Box::new(Mutex::new(Output::default()));
        unsafe {
            let true_value: &CFType = CFBoolean::new(true).as_ref();
            let spec = CFDictionary::<CFString, CFType>::from_slices(&[kVTVideoEncoderSpecification_EnableLowLatencyRateControl], &[true_value]);
            let mut session: *mut VTCompressionSession = std::ptr::null_mut();
            let status = VTCompressionSession::create(
                None,
                width as i32,
                height as i32,
                kCMVideoCodecType_H264,
                Some(spec.as_opaque()),
                None,
                None,
                Some(on_output),
                &*output as *const Mutex<Output> as *mut c_void,
                NonNull::from(&mut session),
            );
            if status != 0 || session.is_null() {
                bail!("creating the VideoToolbox H.264 encoder failed (OSStatus {status})");
            }
            let session = CFRetained::from_raw(NonNull::new_unchecked(session));
            let set = |key: &CFString, value: &CFType, what: &str| {
                let status = VTSessionSetProperty(session.as_ref(), key, Some(value));
                if status != 0 {
                    eprintln!("desktop-server: encoder setting {what} not accepted (OSStatus {status})");
                }
            };
            set(kVTCompressionPropertyKey_RealTime, true_value, "RealTime");
            set(kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel.as_ref(), "ProfileLevel");
            set(kVTCompressionPropertyKey_AllowFrameReordering, CFBoolean::new(false).as_ref(), "AllowFrameReordering");
            set(kVTCompressionPropertyKey_AverageBitRate, CFNumber::new_i32(bitrate(width, height) as i32).as_ref(), "AverageBitRate");
            set(kVTCompressionPropertyKey_ExpectedFrameRate, CFNumber::new_i32(MAX_FPS as i32).as_ref(), "ExpectedFrameRate");
            // Keyframes on request, plus one every 10 s as a safety net.
            set(kVTCompressionPropertyKey_MaxKeyFrameInterval, CFNumber::new_i32((MAX_FPS * 10) as i32).as_ref(), "MaxKeyFrameInterval");
            session.prepare_to_encode_frames();

            let mbs = width.div_ceil(16) * height.div_ceil(16);
            let codec = format!("avc1.42E0{:02X}", if mbs <= 8160 { 42 } else { 52 });
            Ok(Self { session, output, force_key: true, codec })
        }
    }
}

/// VideoToolbox's output callback: converts the frame to Annex B.
unsafe extern "C-unwind" fn on_output(
    refcon: *mut c_void,
    _frame_refcon: *mut c_void,
    status: i32,
    _flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    let output = unsafe { &*(refcon as *const Mutex<Output>) };
    let mut out = output.lock().unwrap();
    match unsafe { sample.as_ref() } {
        Some(sample) if status == 0 => match unsafe { annex_b(sample) } {
            Ok(frame) => out.frame = Some(frame),
            Err(e) => out.error = Some(format!("{e:#}")),
        },
        Some(_) | None if status != 0 => out.error = Some(format!("encoding failed (OSStatus {status})")),
        _ => {} // frame dropped
    }
}

/// The sample's NAL units as Annex B, with SPS/PPS first on keyframes; and whether it's a keyframe.
unsafe fn annex_b(sample: &CMSampleBuffer) -> Result<(Vec<u8>, bool)> {
    unsafe {
        // A keyframe has no "NotSync" attachment.
        let key = match sample.sample_attachments_array(false) {
            Some(a) if a.count() > 0 => {
                let dict = &*(a.value_at_index(0) as *const CFDictionary);
                dict.value(kCMSampleAttachmentKey_NotSync as *const CFString as *const c_void).is_null()
            }
            _ => true,
        };
        let mut out = Vec::new();
        let mut nal_length_size: c_int = 4;
        if key {
            let format = sample.format_description().ok_or_else(|| anyhow!("no format description"))?;
            let format: &CMFormatDescription = &format;
            let mut count = 0usize;
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format, 0, std::ptr::null_mut(), std::ptr::null_mut(), &mut count, &mut nal_length_size);
            for i in 0..count {
                let (mut ptr, mut size) = (std::ptr::null(), 0usize);
                if CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format, i, &mut ptr, &mut size, std::ptr::null_mut(), std::ptr::null_mut()) == 0 {
                    out.extend_from_slice(&[0, 0, 0, 1]);
                    out.extend_from_slice(std::slice::from_raw_parts(ptr, size));
                }
            }
        }
        let block = sample.data_buffer().ok_or_else(|| anyhow!("no data in the encoded frame"))?;
        let (mut total, mut ptr): (usize, *mut c_char) = (0, std::ptr::null_mut());
        let mut contiguous = 0usize;
        if block.data_pointer(0, &mut contiguous, &mut total, &mut ptr) != 0 || ptr.is_null() {
            bail!("reading the encoded frame failed");
        }
        let data: Vec<u8> = if contiguous == total {
            std::slice::from_raw_parts(ptr as *const u8, total).to_vec()
        } else {
            let mut copy = vec![0u8; total];
            if block.copy_data_bytes(0, total, NonNull::new_unchecked(copy.as_mut_ptr() as *mut c_void)) != 0 {
                bail!("copying the encoded frame failed");
            }
            copy
        };
        // Length-prefixed NAL units -> start codes.
        let n = nal_length_size.clamp(1, 4) as usize;
        let mut i = 0;
        while i + n <= data.len() {
            let len = data[i..i + n].iter().fold(0usize, |acc, &b| (acc << 8) | b as usize);
            i += n;
            if i + len > data.len() {
                bail!("malformed encoded frame");
            }
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&data[i..i + len]);
            i += len;
        }
        Ok((out, key))
    }
}

impl VideoEncoder for VtEncoder {
    fn encode(&mut self, frame: &Capturer, timestamp_us: u64) -> Result<(Vec<u8>, bool)> {
        let image = frame.pixel_buffer().ok_or_else(|| anyhow!("no frame captured yet"))?;
        unsafe {
            let force = std::mem::take(&mut self.force_key);
            let options = force.then(|| {
                let true_value: &CFType = CFBoolean::new(true).as_ref();
                CFDictionary::<CFString, CFType>::from_slices(&[kVTEncodeFrameOptionKey_ForceKeyFrame], &[true_value])
            });
            let status = self.session.encode_frame(
                &image,
                CMTime::new(timestamp_us as i64, 1_000_000),
                kCMTimeInvalid,
                options.as_ref().map(|d| d.as_opaque()),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            if status != 0 {
                bail!("VTCompressionSessionEncodeFrame failed (OSStatus {status})");
            }
            // Wait for this frame's output (the callback runs before this returns).
            let status = self.session.complete_frames(kCMTimeInvalid);
            if status != 0 {
                bail!("VTCompressionSessionCompleteFrames failed (OSStatus {status})");
            }
        }
        let mut out = self.output.lock().unwrap();
        if let Some(e) = out.error.take() {
            bail!("{e}");
        }
        Ok(out.frame.take().unwrap_or_default())
    }
    fn force_keyframe(&mut self) {
        self.force_key = true;
    }
    fn codec(&self) -> &str {
        &self.codec
    }
    fn name(&self) -> &str {
        "VideoToolbox (hardware)"
    }
    fn label(&self) -> &str {
        "VideoToolbox"
    }
}

impl Drop for VtEncoder {
    fn drop(&mut self) {
        unsafe { self.session.invalidate() };
    }
}
