//! Hardware H.264 encoding with a Media Foundation transform (Intel Quick Sync,
//! NVENC, AMD VCE — whatever the GPU driver registers).
//!
//! Hardware MFTs are asynchronous: they announce through events when they
//! want input (METransformNeedInput) and when output is ready
//! (METransformHaveOutput). We drive that synchronously, one frame at a time,
//! with low-latency mode on so each input produces its output right away.

use crate::encoder::{BITRATE, Converter, MAX_FPS, VideoEncoder};
use anyhow::{Context, Result, bail};
use std::mem::ManuallyDrop;
use windows::Win32::Foundation::VARIANT_TRUE;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::*;
use windows::core::{Interface, PWSTR};

pub struct MfEncoder {
    mft: IMFTransform,
    events: IMFMediaEventGenerator,
    codec_api: Option<ICodecAPI>,
    /// METransformNeedInput events not yet answered with ProcessInput.
    need_input: u32,
    conv: Converter,
    nv12: Vec<u8>,
    force_key: bool,
    /// Last SPS/PPS seen, prepended to keyframes that come without them, so a
    /// viewer that (re)starts its decoder on any keyframe can decode it.
    param_sets: Vec<u8>,
    name: String,
    codec: String,
}

impl MfEncoder {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_FULL).context("MFStartup")?;
            let (activate, name) = find_hardware_encoder()?;
            let mft: IMFTransform = activate.ActivateObject().context("activating encoder")?;
            mft.GetAttributes()?.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).context("unlocking async MFT")?;

            let codec_api = mft.cast::<ICodecAPI>().ok();
            if let Some(api) = &codec_api {
                // Best effort: not every encoder supports every property.
                report("CODECAPI_AVLowLatencyMode", api.SetValue(&CODECAPI_AVLowLatencyMode, &variant_bool(true)));
                report("CODECAPI_AVEncCommonRateControlMode", api.SetValue(&CODECAPI_AVEncCommonRateControlMode, &variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32)));
                report("CODECAPI_AVEncCommonMeanBitRate", api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &variant_u32(BITRATE)));
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
                out.SetUINT32(&MF_MT_AVG_BITRATE, BITRATE)?;
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

            let events: IMFMediaEventGenerator = mft.cast().context("encoder is not asynchronous")?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

            Ok(Self {
                mft,
                events,
                codec_api,
                need_input: 0,
                conv: Converter::new(width, height),
                nv12: Vec::new(),
                force_key: true,
                param_sets: Vec::new(),
                name: format!("{name} (hardware)"),
                codec,
            })
        }
    }

    /// Blocks until the encoder's next event and returns its type.
    fn next_event(&mut self) -> Result<MF_EVENT_TYPE> {
        unsafe {
            let ev = self.events.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0))?;
            let t = MF_EVENT_TYPE(ev.GetType()? as i32);
            if t == METransformNeedInput {
                self.need_input += 1;
            }
            Ok(t)
        }
    }

    fn pull_output(&mut self) -> Result<Option<(Vec<u8>, bool)>> {
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
                    return Ok(None);
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
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
            Ok(Some((data, key)))
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
    fn encode(&mut self, bgra: &[u8], timestamp_us: u64) -> Result<(Vec<u8>, bool)> {
        let t0 = std::time::Instant::now();
        let mut nv12 = std::mem::take(&mut self.nv12);
        self.conv.nv12(bgra, &mut nv12);
        let t1 = std::time::Instant::now();
        unsafe {
            if std::mem::take(&mut self.force_key) {
                if let Some(api) = &self.codec_api {
                    let _ = api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &variant_u32(1));
                }
            }
            while self.need_input == 0 {
                if self.next_event()? == METransformHaveOutput {
                    let _ = self.pull_output()?; // stray output from an earlier frame; drop it
                }
            }
            let buffer = MFCreateMemoryBuffer(nv12.len() as u32)?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(nv12.as_ptr(), ptr, nv12.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(nv12.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(timestamp_us as i64 * 10)?;
            sample.SetSampleDuration(10_000_000 / MAX_FPS as i64)?;
            self.mft.ProcessInput(0, &sample, 0).context("ProcessInput")?;
            self.need_input -= 1;
        }
        self.nv12 = nv12;
        let t2 = std::time::Instant::now();
        loop {
            if self.next_event()? == METransformHaveOutput {
                if let Some((data, key)) = self.pull_output()? {
                    debug_timing(t1 - t0, t2 - t1, t2.elapsed());
                    return Ok((self.fix_param_sets(data, key), key));
                }
            }
        }
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

impl Drop for MfEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        }
    }
}

unsafe fn find_hardware_encoder() -> Result<(IMFActivate, String)> {
    unsafe {
        let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
        let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
        let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut list,
            &mut count,
        )
        .context("MFTEnumEx")?;
        let items = std::slice::from_raw_parts_mut(list, count as usize);
        let first = items.first_mut().and_then(|a| a.take());
        for a in items.iter_mut() {
            drop(a.take());
        }
        CoTaskMemFree(Some(list as _));
        let activate = first.context("no hardware H.264 encoder found")?;

        let mut name = PWSTR::null();
        let mut len = 0;
        let friendly = if activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut name, &mut len).is_ok() {
            let s = name.to_string().unwrap_or_default();
            CoTaskMemFree(Some(name.0 as _));
            s
        } else {
            "hardware encoder".to_string()
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
fn debug_timing(convert: std::time::Duration, submit: std::time::Duration, wait: std::time::Duration) {
    if std::env::var_os("DS_DEBUG").is_some() {
        eprintln!(
            "    hw: convert {:.1} ms, submit {:.1} ms, wait for output {:.1} ms",
            convert.as_secs_f64() * 1e3,
            submit.as_secs_f64() * 1e3,
            wait.as_secs_f64() * 1e3
        );
    }
}
