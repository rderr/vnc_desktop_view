//! H.264 encoding with a Media Foundation transform: the GPU's hardware
//! encoder (Intel Quick Sync, NVENC, AMD VCE — whatever the driver registers)
//! or, as a fallback, Microsoft's software H.264 encoder that ships with Windows.
//!
//! Hardware MFTs are asynchronous: they announce through events when they
//! want input (METransformNeedInput) and when output is ready
//! (METransformHaveOutput). The software MFT is synchronous: ProcessInput,
//! then ProcessOutput until it needs more input. Either way we drive it one
//! frame at a time, with low-latency mode on so each input produces its output
//! right away.
//!
//! Frames reach the encoder in the cheapest way available (see `Input`):
//! hardware encoders read a GPU-converted NV12 texture directly; the software
//! encoder gets them copied to system memory and converted on the CPU.

use super::capture::Capturer;
use crate::convert::Converter;
use crate::encoder::{MAX_FPS, VideoEncoder, bitrate};
use super::gpu::Nv12Converter;
use anyhow::{Context, Result, bail};
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::Foundation::{LUID, VARIANT_TRUE};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::*;
use windows::core::{Interface, PWSTR};

/// How frames get from the capturer to the encoder.
enum Input {
    /// Converted to NV12 on the GPU and handed over as a texture: the image
    /// never leaves video memory. Hardware encoders that accept a D3D11 device.
    Gpu(Nv12Converter),
    /// Copied to system memory as BGRA and converted on the CPU: the software
    /// encoder, and hardware encoders that can't take textures. (Converting on
    /// the GPU and copying NV12 back measured no faster: the copy is dominated
    /// by waiting for the GPU either way.)
    Cpu(Converter),
}

impl Input {
    fn describe(&self) -> &'static str {
        match self {
            Input::Gpu(_) => "GPU input",
            Input::Cpu(_) => "CPU conversion",
        }
    }
}

/// Set when encoding from GPU textures failed, so restarts use the CPU path
/// instead of failing the same way again.
static GPU_INPUT_FAILED: AtomicBool = AtomicBool::new(false);

pub struct MfEncoder {
    mft: IMFTransform,
    /// Present for asynchronous (hardware) encoders only.
    events: Option<IMFMediaEventGenerator>,
    codec_api: Option<ICodecAPI>,
    /// METransformNeedInput events not yet answered with ProcessInput.
    need_input: u32,
    input: Input,
    /// Shares the capturer's D3D11 device with the encoder (`Input::Gpu`).
    _device_manager: Option<IMFDXGIDeviceManager>,
    bgra: Vec<u8>,
    nv12: Vec<u8>,
    force_key: bool,
    /// Last SPS/PPS seen, prepended to keyframes that come without them, so a
    /// viewer that (re)starts its decoder on any keyframe can decode it.
    param_sets: Vec<u8>,
    name: String,
    codec: String,
}

impl MfEncoder {
    pub fn new(cap: &Capturer, hardware: bool) -> Result<Self> {
        let (width, height) = (cap.width, cap.height);
        unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_FULL).context("MFStartup")?;
            let (activate, name) = find_encoder(hardware, cap.adapter_luid)?;
            let mft: IMFTransform = activate.ActivateObject().context("activating encoder")?;
            let attributes = mft.GetAttributes()?;
            if hardware {
                attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).context("unlocking async MFT")?;
            }

            // Hardware encoders that take a D3D11 device get frames as textures. Must be
            // set up before the media types. DS_INPUT=cpu forces the CPU path, for comparison.
            let gpu_wanted = hardware
                && std::env::var("DS_INPUT").as_deref() != Ok("cpu")
                && !GPU_INPUT_FAILED.load(Ordering::Relaxed)
                && attributes.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) != 0;
            let mut device_manager = None;
            let mut input = Input::Cpu(Converter::new(width, height));
            if gpu_wanted {
                // A ring of 2: the encoder may still read one texture while the next is converted.
                match Nv12Converter::new(cap, 2).and_then(|conv| Ok((conv, share_device(&mft, cap)?))) {
                    Ok((conv, manager)) => {
                        input = Input::Gpu(conv);
                        device_manager = Some(manager);
                    }
                    Err(e) => eprintln!("desktop-server: can't pass frames to the encoder on the GPU ({e:#}); copying them to memory"),
                }
            }

            let codec_api = mft.cast::<ICodecAPI>().ok();
            if let Some(api) = &codec_api {
                // Best effort: not every encoder supports every property.
                report("CODECAPI_AVLowLatencyMode", api.SetValue(&CODECAPI_AVLowLatencyMode, &variant_bool(true)));
                report("CODECAPI_AVEncCommonRateControlMode", api.SetValue(&CODECAPI_AVEncCommonRateControlMode, &variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32)));
                report("CODECAPI_AVEncCommonMeanBitRate", api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &variant_u32(bitrate(width, height))));
                report("CODECAPI_AVEncMPVDefaultBPictureCount", api.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &variant_u32(0)));
                // Keyframes on request, plus one every 10 s as a safety net.
                report("CODECAPI_AVEncMPVGOPSize", api.SetValue(&CODECAPI_AVEncMPVGOPSize, &variant_u32(MAX_FPS * 10)));
            }

            // Encoders want the output type set before the input type.
            let mut codec = String::new();
            let mut last_err = None;
            for (profile, codec_str) in [(eAVEncH264VProfile_Base.0, "avc1.42E033"), (eAVEncH264VProfile_Main.0, "avc1.4D4033")] {
                let out = MFCreateMediaType()?;
                out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
                out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate(width, height))?;
                out.SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)?;
                out.SetUINT64(&MF_MT_FRAME_RATE, ((MAX_FPS as u64) << 32) | 1)?;
                out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
                out.SetUINT32(&MF_MT_MPEG2_PROFILE, profile as u32)?;
                match mft.SetOutputType(0, &out, 0) {
                    Ok(()) => {
                        codec = codec_str.to_string();
                        break;
                    }
                    Err(e) => last_err = Some(e),
                }
            }
            if codec.is_empty() {
                bail!("SetOutputType: {}", last_err.map(|e| e.to_string()).unwrap_or_default());
            }

            let inp = MFCreateMediaType()?;
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            inp.SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)?;
            inp.SetUINT64(&MF_MT_FRAME_RATE, ((MAX_FPS as u64) << 32) | 1)?;
            inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            mft.SetInputType(0, &inp, 0).context("SetInputType (NV12)")?;

            let events = if hardware { Some(mft.cast::<IMFMediaEventGenerator>().context("encoder is not asynchronous")?) } else { None };
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

            Ok(Self {
                mft,
                events,
                codec_api,
                need_input: 0,
                name: format!("{name} ({}, {})", if hardware { "hardware" } else { "software" }, input.describe()),
                input,
                _device_manager: device_manager,
                bgra: Vec::new(),
                nv12: Vec::new(),
                force_key: true,
                param_sets: Vec::new(),
                codec,
            })
        }
    }

    /// Blocks until the encoder's next event and returns its type.
    fn next_event(&mut self) -> Result<MF_EVENT_TYPE> {
        unsafe {
            let ev = self.events.as_ref().context("not an asynchronous encoder")?.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0))?;
            let t = MF_EVENT_TYPE(ev.GetType()? as i32);
            if t == METransformNeedInput {
                self.need_input += 1;
            }
            Ok(t)
        }
    }

    fn pull_output(&mut self) -> Result<Pulled> {
        unsafe {
            let info = self.mft.GetOutputStreamInfo(0)?;
            let sample = if info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0 {
                None
            } else {
                let s = MFCreateSample()?;
                s.AddBuffer(&MFCreateMemoryBuffer(info.cbSize.max(1 << 20))?)?;
                Some(s)
            };
            let mut out = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0;
            let result = self.mft.ProcessOutput(0, &mut out, &mut status);
            let sample = ManuallyDrop::take(&mut out[0].pSample);
            drop(ManuallyDrop::take(&mut out[0].pEvents));
            match result {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // The encoder wants its output type renegotiated (some do this on the first frame).
                    let t = self.mft.GetOutputAvailableType(0, 0)?;
                    self.mft.SetOutputType(0, &t, 0)?;
                    return Ok(Pulled::Retry);
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(Pulled::NeedInput),
                Err(e) => return Err(e).context("ProcessOutput"),
            }
            let sample = sample.context("encoder returned no sample")?;
            let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            let buf = sample.ConvertToContiguousBuffer()?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            buf.Lock(&mut ptr, None, Some(&mut len))?;
            let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            buf.Unlock()?;
            Ok(Pulled::Frame(data, key))
        }
    }

    /// The capturer's current image as an NV12 media buffer, by way of `self.input`.
    unsafe fn input_buffer(&mut self, frame: &Capturer) -> Result<IMFMediaBuffer> {
        unsafe {
            match &mut self.input {
                Input::Gpu(conv) => {
                    let texture = conv.convert()?;
                    let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &texture, 0, false)
                        .context("MFCreateDXGISurfaceBuffer")?;
                    // Some encoders insist on a current length, even for a texture.
                    if let Ok(b2) = buffer.cast::<IMF2DBuffer>() {
                        buffer.SetCurrentLength(b2.GetContiguousLength()?)?;
                    }
                    return Ok(buffer);
                }
                Input::Cpu(conv) => {
                    frame.read_bgra(&mut self.bgra)?;
                    conv.nv12(&self.bgra, &mut self.nv12);
                }
            }
            let buffer = MFCreateMemoryBuffer(self.nv12.len() as u32)?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(self.nv12.as_ptr(), ptr, self.nv12.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(self.nv12.len() as u32)?;
            Ok(buffer)
        }
    }

    /// Remembers SPS/PPS, and prepends them to keyframes that lack them.
    fn fix_param_sets(&mut self, data: Vec<u8>, key: bool) -> Vec<u8> {
        let nals = split_nals(&data);
        let has_sps = nals.iter().any(|n| nal_type(n) == 7);
        if has_sps {
            self.param_sets.clear();
            for n in nals.iter().filter(|n| matches!(nal_type(n), 7 | 8)) {
                self.param_sets.extend_from_slice(&[0, 0, 0, 1]);
                self.param_sets.extend_from_slice(n);
            }
            return data;
        }
        if key && !self.param_sets.is_empty() {
            let mut out = self.param_sets.clone();
            out.extend_from_slice(&data);
            return out;
        }
        data
    }
}

impl VideoEncoder for MfEncoder {
    fn encode(&mut self, frame: &Capturer, timestamp_us: u64) -> Result<(Vec<u8>, bool)> {
        let result = self.encode_frame(frame, timestamp_us);
        if result.is_err() && matches!(self.input, Input::Gpu(_)) && !GPU_INPUT_FAILED.swap(true, Ordering::Relaxed) {
            eprintln!("desktop-server: encoding from GPU textures failed; capture restarts will copy frames to memory");
        }
        result
    }
    fn force_keyframe(&mut self) {
        self.force_key = true;
    }
    fn codec(&self) -> &str {
        &self.codec
    }
    fn name(&self) -> &str {
        &self.name
    }
}

impl MfEncoder {
    fn encode_frame(&mut self, frame: &Capturer, timestamp_us: u64) -> Result<(Vec<u8>, bool)> {
        let t0 = std::time::Instant::now();
        let buffer = unsafe { self.input_buffer(frame)? };
        let t1 = std::time::Instant::now();
        let asynchronous = self.events.is_some();
        unsafe {
            if std::mem::take(&mut self.force_key) {
                if let Some(api) = &self.codec_api {
                    let _ = api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &variant_u32(1));
                }
            }
            while asynchronous && self.need_input == 0 {
                if self.next_event()? == METransformHaveOutput {
                    let _ = self.pull_output()?; // stray output from an earlier frame; drop it
                }
            }
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(timestamp_us as i64 * 10)?;
            sample.SetSampleDuration(10_000_000 / MAX_FPS as i64)?;
            self.mft.ProcessInput(0, &sample, 0).context("ProcessInput")?;
            if asynchronous {
                self.need_input -= 1;
            }
        }
        let t2 = std::time::Instant::now();
        if asynchronous {
            loop {
                if self.next_event()? == METransformHaveOutput {
                    match self.pull_output()? {
                        Pulled::Frame(data, key) => {
                            debug_timing(t1 - t0, t2 - t1, t2.elapsed());
                            return Ok((self.fix_param_sets(data, key), key));
                        }
                        Pulled::NeedInput | Pulled::Retry => {}
                    }
                }
            }
        }
        // Synchronous: collect whatever this input produced (one frame in low-latency mode).
        let (mut out, mut key) = (Vec::new(), false);
        for _ in 0..8 {
            match self.pull_output()? {
                Pulled::Frame(data, k) => {
                    key |= k;
                    out.extend_from_slice(&data);
                }
                Pulled::NeedInput => break,
                Pulled::Retry => {}
            }
        }
        debug_timing(t1 - t0, t2 - t1, t2.elapsed());
        if out.is_empty() {
            return Ok((out, false)); // still buffering; nothing to send for this frame
        }
        Ok((self.fix_param_sets(out, key), key))
    }
}

impl Drop for MfEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        }
    }
}

/// Lets the encoder use the capturer's D3D11 device, so it can read frames
/// from textures on it.
unsafe fn share_device(mft: &IMFTransform, cap: &Capturer) -> Result<IMFDXGIDeviceManager> {
    unsafe {
        let mut token = 0;
        let mut manager = None;
        MFCreateDXGIDeviceManager(&mut token, &mut manager).context("MFCreateDXGIDeviceManager")?;
        let manager = manager.context("no device manager")?;
        manager.ResetDevice(&cap.device, token).context("ResetDevice")?;
        mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
            .context("MFT_MESSAGE_SET_D3D_MANAGER")?;
        Ok(manager)
    }
}

unsafe fn find_encoder(hardware: bool, adapter: LUID) -> Result<(IMFActivate, String)> {
    unsafe {
        let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
        let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
        let flags = if hardware { MFT_ENUM_FLAG_HARDWARE } else { MFT_ENUM_FLAG_SYNCMFT } | MFT_ENUM_FLAG_SORTANDFILTER;
        let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        // Hardware: the encoder on the GPU that has the desktop (it matters with
        // more than one GPU, e.g. integrated + discrete). Fall back to any.
        let on_adapter = hardware
            && (|| -> windows::core::Result<()> {
                let mut attrs = None;
                MFCreateAttributes(&mut attrs, 1)?;
                let attrs = attrs.unwrap();
                let luid = std::slice::from_raw_parts(&adapter as *const LUID as *const u8, size_of::<LUID>());
                attrs.SetBlob(&MFT_ENUM_ADAPTER_LUID, luid)?;
                MFTEnum2(MFT_CATEGORY_VIDEO_ENCODER, flags, Some(&input), Some(&output), &attrs, &mut list, &mut count)
            })()
            .is_ok()
            && count > 0;
        if !on_adapter {
            if !list.is_null() {
                CoTaskMemFree(Some(list as _));
                list = std::ptr::null_mut();
            }
            MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, flags, Some(&input), Some(&output), &mut list, &mut count)
                .context("MFTEnumEx")?;
        }
        let items = std::slice::from_raw_parts_mut(list, count as usize);
        let first = items.first_mut().and_then(|a| a.take());
        for a in items.iter_mut() {
            drop(a.take());
        }
        CoTaskMemFree(Some(list as _));
        let activate = first.context(if hardware { "no hardware H.264 encoder found" } else { "no software H.264 encoder found" })?;

        let mut name = PWSTR::null();
        let mut len = 0;
        let friendly = if activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut name, &mut len).is_ok() {
            let s = name.to_string().unwrap_or_default();
            CoTaskMemFree(Some(name.0 as _));
            s
        } else {
            "H.264 encoder".to_string()
        };
        Ok((activate, friendly))
    }
}

fn variant_u32(v: u32) -> VARIANT {
    let mut var = VARIANT::default();
    unsafe {
        (*var.Anonymous.Anonymous).vt = VT_UI4;
        (*var.Anonymous.Anonymous).Anonymous.ulVal = v;
    }
    var
}

fn variant_bool(v: bool) -> VARIANT {
    let mut var = VARIANT::default();
    unsafe {
        (*var.Anonymous.Anonymous).vt = VT_BOOL;
        (*var.Anonymous.Anonymous).Anonymous.boolVal = if v { VARIANT_TRUE } else { Default::default() };
    }
    var
}

/// Splits Annex B data into NAL units (without start codes).
fn split_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(k, &s)| {
            let mut end = starts.get(k + 1).map(|&n| n - 3).unwrap_or(data.len());
            while end > s && data[end - 1] == 0 {
                end -= 1; // trailing zero of a 4-byte start code
            }
            &data[s..end]
        })
        .collect()
}

fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map(|b| b & 0x1f).unwrap_or(0)
}

fn report(what: &str, r: windows::core::Result<()>) {
    if let Err(e) = r {
        eprintln!("desktop-server: encoder setting {what} not accepted: {e}");
    }
}

/// Per-frame timing breakdown when DS_DEBUG is set.
fn debug_timing(input: std::time::Duration, submit: std::time::Duration, wait: std::time::Duration) {
    if std::env::var_os("DS_DEBUG").is_some() {
        eprintln!(
            "    mf: prepare input {:.1} ms, submit {:.1} ms, wait for output {:.1} ms",
            input.as_secs_f64() * 1e3,
            submit.as_secs_f64() * 1e3,
            wait.as_secs_f64() * 1e3
        );
    }
}

enum Pulled {
    Frame(Vec<u8>, bool),
    /// The encoder has nothing (more) for us until it gets another frame.
    NeedInput,
    /// The output type was renegotiated; ask again.
    Retry,
}
