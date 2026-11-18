//! H.264 with NVENC, the encoder in NVIDIA GPUs, through the NVIDIA driver's
//! libnvidia-encode (loaded at runtime, so the server also starts on machines
//! without NVIDIA's driver and falls back or reports why).
//!
//! The screen goes in as BGRX straight from the capture's shared memory; NVENC
//! converts it to YUV on the GPU. Encoding is synchronous: each frame's
//! bitstream is ready when nvEncLockBitstream returns.

use super::capture::Capturer;
use super::nvenc_sys::*;
use crate::encoder::{MAX_FPS, VideoEncoder, bitrate};
use anyhow::{Context, Result, anyhow, bail};
use libloading::Library;
use std::ffi::{CStr, c_char, c_int, c_uint, c_void};

const fn struct_ver(v: u32) -> u32 {
    NVENCAPI_VERSION | (v << 16) | (0x7 << 28)
}
const FUNCTION_LIST_VER: u32 = struct_ver(2);
const OPEN_SESSION_VER: u32 = struct_ver(1);
const PRESET_CONFIG_VER: u32 = struct_ver(4) | (1 << 31);
const CONFIG_VER: u32 = struct_ver(8) | (1 << 31);
const INITIALIZE_PARAMS_VER: u32 = struct_ver(6) | (1 << 31);
const CREATE_INPUT_BUFFER_VER: u32 = struct_ver(1);
const CREATE_BITSTREAM_BUFFER_VER: u32 = struct_ver(1);
const LOCK_INPUT_BUFFER_VER: u32 = struct_ver(1);
const PIC_PARAMS_VER: u32 = struct_ver(6) | (1 << 31);
const LOCK_BITSTREAM_VER: u32 = struct_ver(1) | (1 << 31);

const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> GUID {
    GUID { Data1: d1, Data2: d2, Data3: d3, Data4: d4 }
}
const CODEC_H264: GUID = guid(0x6bc82762, 0x4e63, 0x4ca4, [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf]);
const PROFILE_HIGH: GUID = guid(0xe7cbc309, 0x4f7a, 0x4b89, [0xaf, 0x2a, 0xd5, 0x37, 0xc9, 0x2b, 0xe3, 0x10]);
/// P3: fast, still good for text. (P1 fastest .. P7 best.)
const PRESET_P3: GUID = guid(0x36850110, 0x3a07, 0x441f, [0x94, 0xd5, 0x36, 0x70, 0x63, 0x1f, 0x91, 0xf6]);
/// 5.2: up to 4096x2304 at 60 fps.
const H264_LEVEL_5_2: u32 = 52;

/// The CUDA driver API, just enough to give NVENC a context.
struct Cuda {
    _lib: Library,
    ctx: *mut c_void,
    destroy: unsafe extern "C" fn(*mut c_void) -> c_int,
    device_name: String,
}

impl Cuda {
    fn new() -> Result<Self> {
        unsafe {
            let lib = Library::new("libcuda.so.1").context("loading libcuda.so.1 (is NVIDIA's driver installed?)")?;
            let init = *lib.get::<unsafe extern "C" fn(c_uint) -> c_int>(b"cuInit\0")?;
            let device_get = *lib.get::<unsafe extern "C" fn(*mut c_int, c_int) -> c_int>(b"cuDeviceGet\0")?;
            let device_name = *lib.get::<unsafe extern "C" fn(*mut c_char, c_int, c_int) -> c_int>(b"cuDeviceGetName\0")?;
            let ctx_create = *lib.get::<unsafe extern "C" fn(*mut *mut c_void, c_uint, c_int) -> c_int>(b"cuCtxCreate_v2\0")?;
            let destroy = *lib.get::<unsafe extern "C" fn(*mut c_void) -> c_int>(b"cuCtxDestroy_v2\0")?;
            let check = |what: &str, r: c_int| if r == 0 { Ok(()) } else { Err(anyhow!("{what} failed (CUDA error {r})")) };
            check("cuInit", init(0))?;
            let mut dev = 0;
            check("cuDeviceGet", device_get(&mut dev, 0))?;
            let mut name = [0 as c_char; 128];
            let name = if device_name(name.as_mut_ptr(), name.len() as c_int, dev) == 0 {
                CStr::from_ptr(name.as_ptr()).to_string_lossy().into_owned()
            } else {
                "NVIDIA GPU".into()
            };
            let mut ctx = std::ptr::null_mut();
            check("cuCtxCreate", ctx_create(&mut ctx, 0, dev))?;
            Ok(Self { _lib: lib, ctx, destroy, device_name: name })
        }
    }
}

impl Drop for Cuda {
    fn drop(&mut self) {
        unsafe { (self.destroy)(self.ctx) };
    }
}

pub struct NvEncoder {
    api: NV_ENCODE_API_FUNCTION_LIST,
    encoder: *mut c_void,
    input: NV_ENC_INPUT_PTR,
    output: NV_ENC_OUTPUT_PTR,
    width: u32,
    height: u32,
    frame_idx: u32,
    force_key: bool,
    name: String,
    // Dropped after the encoder (fields drop in order).
    _lib: Library,
    _cuda: Cuda,
}

/// Calls an NVENC API function, turning a failure status into an error.
macro_rules! nv {
    ($self_api:expr, $enc:expr, $f:ident ( $($arg:expr),* )) => {{
        let f = $self_api.$f.ok_or_else(|| anyhow!(concat!(stringify!($f), " missing")))?;
        let status = f($($arg),*);
        if status != _NVENCSTATUS::NV_ENC_SUCCESS {
            Err(anyhow!("{} failed: {}", stringify!($f), last_error(&$self_api, $enc, status)))
        } else {
            Ok(())
        }
    }};
}

unsafe fn last_error(api: &NV_ENCODE_API_FUNCTION_LIST, encoder: *mut c_void, status: NVENCSTATUS) -> String {
    let detail = match api.nvEncGetLastErrorString {
        Some(f) if !encoder.is_null() => {
            let s = unsafe { f(encoder) };
            if s.is_null() { String::new() } else { unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned() }
        }
        _ => String::new(),
    };
    format!("NVENCSTATUS {status}{}", if detail.is_empty() { String::new() } else { format!(" ({detail})") })
}

impl NvEncoder {
    pub fn new(cap: &Capturer) -> Result<Self> {
        let (width, height) = (cap.width, cap.height);
        if width > 4096 || height > 4096 {
            bail!("{width}x{height} is larger than NVENC's H.264 limit of 4096x4096");
        }
        unsafe {
            let cuda = Cuda::new()?;
            let lib = Library::new("libnvidia-encode.so.1").context("loading libnvidia-encode.so.1 (NVIDIA's encoder library)")?;
            let max_version = *lib.get::<unsafe extern "C" fn(*mut u32) -> NVENCSTATUS>(b"NvEncodeAPIGetMaxSupportedVersion\0")?;
            let create_instance =
                *lib.get::<unsafe extern "C" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS>(b"NvEncodeAPICreateInstance\0")?;
            let mut driver_version = 0;
            max_version(&mut driver_version);
            let needed = (NVENCAPI_MAJOR_VERSION << 4) | NVENCAPI_MINOR_VERSION;
            if driver_version < needed {
                bail!(
                    "NVIDIA's driver supports NVENC API {}.{}; {}.{} (driver 530 or newer) is needed",
                    driver_version >> 4,
                    driver_version & 0xf,
                    NVENCAPI_MAJOR_VERSION,
                    NVENCAPI_MINOR_VERSION
                );
            }
            let mut api: NV_ENCODE_API_FUNCTION_LIST = std::mem::zeroed();
            api.version = FUNCTION_LIST_VER;
            let status = create_instance(&mut api);
            if status != _NVENCSTATUS::NV_ENC_SUCCESS {
                bail!("NvEncodeAPICreateInstance failed: NVENCSTATUS {status}");
            }

            let mut open: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS = std::mem::zeroed();
            open.version = OPEN_SESSION_VER;
            open.deviceType = _NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_CUDA;
            open.device = cuda.ctx;
            open.apiVersion = NVENCAPI_VERSION;
            let mut encoder = std::ptr::null_mut();
            nv!(api, std::ptr::null_mut(), nvEncOpenEncodeSessionEx(&mut open, &mut encoder))?;

            // Partly built encoder: from here on, Drop cleans up.
            let mut this = Self {
                api,
                encoder,
                input: std::ptr::null_mut(),
                output: std::ptr::null_mut(),
                width,
                height,
                frame_idx: 0,
                force_key: true,
                name: format!("NVENC on {} (hardware)", cuda.device_name),
                _lib: lib,
                _cuda: cuda,
            };
            this.init()?;
            Ok(this)
        }
    }

    unsafe fn init(&mut self) -> Result<()> {
        let (api, enc) = (self.api, self.encoder);
        unsafe {
            let tuning = NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
            let mut preset: NV_ENC_PRESET_CONFIG = std::mem::zeroed();
            preset.version = PRESET_CONFIG_VER;
            preset.presetCfg.version = CONFIG_VER;
            nv!(api, enc, nvEncGetEncodePresetConfigEx(enc, CODEC_H264, PRESET_P3, tuning, &mut preset))?;

            let mut config = preset.presetCfg;
            config.version = CONFIG_VER;
            config.profileGUID = PROFILE_HIGH;
            // Keyframes on request, plus one every 10 s as a safety net; no B-frames.
            config.gopLength = MAX_FPS * 10;
            config.frameIntervalP = 1;
            let rate = bitrate(self.width, self.height);
            config.rcParams.rateControlMode = _NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
            config.rcParams.averageBitRate = rate;
            config.rcParams.maxBitRate = rate;
            let h264 = &mut config.encodeCodecConfig.h264Config;
            h264.idrPeriod = config.gopLength;
            h264.level = H264_LEVEL_5_2;
            h264.set_repeatSPSPPS(1);
            h264.maxNumRefFrames = 1;
            // Say how the colors were converted (NVENC converts RGB with the
            // matrix given here), so the viewer converts back the same way.
            let vui = &mut h264.h264VUIParameters;
            vui.videoSignalTypePresentFlag = 1;
            vui.videoFormat = _NV_ENC_VUI_VIDEO_FORMAT::NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
            vui.videoFullRangeFlag = 0;
            vui.colourDescriptionPresentFlag = 1;
            vui.colourPrimaries = _NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_BT709;
            vui.transferCharacteristics = _NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_SRGB;
            vui.colourMatrix = _NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT709;
            // Declare that frames are never reordered. Without it a High profile
            // decoder must assume they may be, and holds back several frames before
            // showing one: on an idle screen the picture would never appear.
            vui.bitstreamRestrictionFlag = 1;

            let mut init: NV_ENC_INITIALIZE_PARAMS = std::mem::zeroed();
            init.version = INITIALIZE_PARAMS_VER;
            init.encodeGUID = CODEC_H264;
            init.presetGUID = PRESET_P3;
            init.tuningInfo = tuning;
            init.encodeWidth = self.width;
            init.encodeHeight = self.height;
            init.darWidth = self.width;
            init.darHeight = self.height;
            init.maxEncodeWidth = self.width;
            init.maxEncodeHeight = self.height;
            init.frameRateNum = MAX_FPS;
            init.frameRateDen = 1;
            init.enablePTD = 1;
            init.encodeConfig = &mut config;
            nv!(api, enc, nvEncInitializeEncoder(enc, &mut init))?;

            let mut input: NV_ENC_CREATE_INPUT_BUFFER = std::mem::zeroed();
            input.version = CREATE_INPUT_BUFFER_VER;
            input.width = self.width;
            input.height = self.height;
            input.bufferFmt = _NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB;
            nv!(api, enc, nvEncCreateInputBuffer(enc, &mut input))?;
            self.input = input.inputBuffer;

            let mut output: NV_ENC_CREATE_BITSTREAM_BUFFER = std::mem::zeroed();
            output.version = CREATE_BITSTREAM_BUFFER_VER;
            nv!(api, enc, nvEncCreateBitstreamBuffer(enc, &mut output))?;
            self.output = output.bitstreamBuffer;
        }
        Ok(())
    }
}

impl VideoEncoder for NvEncoder {
    fn encode(&mut self, frame: &Capturer, timestamp_us: u64) -> Result<(Vec<u8>, bool)> {
        let (api, enc) = (self.api, self.encoder);
        unsafe {
            // Copy the screen into NVENC's input buffer. (BGRX in memory is NVENC's "ARGB".)
            let mut lock: NV_ENC_LOCK_INPUT_BUFFER = std::mem::zeroed();
            lock.version = LOCK_INPUT_BUFFER_VER;
            lock.inputBuffer = self.input;
            nv!(api, enc, nvEncLockInputBuffer(enc, &mut lock))?;
            let row = self.width as usize * 4;
            let (src, stride, pitch) = (frame.pixels(), frame.stride(), lock.pitch as usize);
            let dst = lock.bufferDataPtr as *mut u8;
            for y in 0..self.height as usize {
                std::ptr::copy_nonoverlapping(src[y * stride..].as_ptr(), dst.add(y * pitch), row);
            }
            nv!(api, enc, nvEncUnlockInputBuffer(enc, self.input))?;

            let mut pic: NV_ENC_PIC_PARAMS = std::mem::zeroed();
            pic.version = PIC_PARAMS_VER;
            pic.inputWidth = self.width;
            pic.inputHeight = self.height;
            pic.inputPitch = lock.pitch;
            pic.inputBuffer = self.input;
            pic.outputBitstream = self.output;
            pic.bufferFmt = _NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB;
            pic.pictureStruct = _NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME;
            pic.inputTimeStamp = timestamp_us;
            pic.frameIdx = self.frame_idx;
            self.frame_idx = self.frame_idx.wrapping_add(1);
            if std::mem::take(&mut self.force_key) {
                pic.encodePicFlags = (_NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR | _NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_OUTPUT_SPSPPS) as u32;
            }
            nv!(api, enc, nvEncEncodePicture(enc, &mut pic))?;

            let mut out: NV_ENC_LOCK_BITSTREAM = std::mem::zeroed();
            out.version = LOCK_BITSTREAM_VER;
            out.outputBitstream = self.output;
            nv!(api, enc, nvEncLockBitstream(enc, &mut out))?;
            let data = std::slice::from_raw_parts(out.bitstreamBufferPtr as *const u8, out.bitstreamSizeInBytes as usize).to_vec();
            let key = out.pictureType == _NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR || out.pictureType == _NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_I;
            nv!(api, enc, nvEncUnlockBitstream(enc, self.output))?;
            Ok((data, key))
        }
    }
    fn force_keyframe(&mut self) {
        self.force_key = true;
    }
    fn codec(&self) -> &str {
        "avc1.640034" // High profile, level 5.2
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn label(&self) -> &str {
        "NVENC"
    }
}

impl Drop for NvEncoder {
    fn drop(&mut self) {
        unsafe {
            if let Some(f) = self.api.nvEncDestroyInputBuffer
                && !self.input.is_null()
            {
                f(self.encoder, self.input);
            }
            if let Some(f) = self.api.nvEncDestroyBitstreamBuffer
                && !self.output.is_null()
            {
                f(self.encoder, self.output);
            }
            if let Some(f) = self.api.nvEncDestroyEncoder {
                f(self.encoder);
            }
        }
    }
}
