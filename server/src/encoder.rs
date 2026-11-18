//! H.264 encoders behind one interface.
//!
//! Shipped builds only use encoders whose H.264 patent licensing is covered by
//! someone else: Windows' Media Foundation encoders (the GPU's, else Windows'
//! own software encoder); on Linux the GPU's encoder through its driver (NVENC,
//! VA-API), else Cisco's own OpenH264 binary, which the extension downloads
//! from Cisco when the user agrees. Cisco's patent license covers that binary,
//! not copies of OpenH264 compiled into this program, so compiling it in
//! (`--features openh264`) is for local testing only.

use crate::platform::capture::Capturer;
use anyhow::Result;

pub const MAX_FPS: u32 = 60;

/// Target bitrate: 8 Mbit/s up to about 1440x900, more for larger screens, at
/// most 20 Mbit/s. (Desktop content is mostly static, so the average is far lower.)
pub fn bitrate(width: u32, height: u32) -> u32 {
    let pixels = width as f64 * height as f64;
    (pixels * 6.2).clamp(8_000_000.0, 20_000_000.0) as u32
}

pub trait VideoEncoder {
    /// Encodes the capturer's current image. Returns Annex B data (may be empty
    /// if the encoder dropped the frame) and whether it is a keyframe.
    fn encode(&mut self, frame: &Capturer, timestamp_us: u64) -> Result<(Vec<u8>, bool)>;
    /// Make the next frame a keyframe.
    fn force_keyframe(&mut self);
    /// WebCodecs codec string for the stream.
    fn codec(&self) -> &str;
    fn name(&self) -> &str;
    /// Short form of `name` for the viewer's toolbar, e.g. "NVENC" or "OpenH264 (software)".
    fn label(&self) -> &str;
}

/// Where Cisco's OpenH264 library was downloaded (`--openh264 PATH`), if the
/// user enabled it: the software fallback on Linux.
pub static OPENH264_PATH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// No encoder could be used. The viewer may offer to set up the software
/// encoder (Cisco's OpenH264) when it sees this.
#[derive(Debug)]
pub struct NoEncoder(pub String);

impl std::fmt::Display for NoEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NoEncoder {}

/// Windows: `auto` tries the hardware encoder, then Windows' software encoder;
/// `hw` / `sw` force one. Linux: `auto` tries NVENC (NVIDIA), then VA-API
/// (Intel, AMD), then Cisco's OpenH264 if `--openh264` gave its path;
/// `hw` / `nvenc` / `vaapi` / `openh264` restrict the choice.
/// In local builds with the `openh264` feature, `openh264` uses the compiled-in copy.
pub fn create(kind: &str, cap: &Capturer) -> Result<Box<dyn VideoEncoder>> {
    #[cfg(windows)]
    {
        use crate::platform::mf::MfEncoder;
        match kind {
            "hw" => return Ok(Box::new(MfEncoder::new(cap, true)?)),
            "sw" => return Ok(Box::new(MfEncoder::new(cap, false)?)),
            "auto" => {
                return match MfEncoder::new(cap, true) {
                    Ok(e) => Ok(Box::new(e)),
                    Err(e) => {
                        eprintln!("desktop-server: no hardware encoder ({e:#}); using Windows' software encoder");
                        Ok(Box::new(MfEncoder::new(cap, false)?))
                    }
                };
            }
            #[cfg(feature = "openh264")]
            "openh264" => return Ok(Box::new(openh264_encoder::SoftwareEncoder::compiled_in(cap.width, cap.height)?)),
            _ => {}
        }
    }
    #[cfg(target_os = "linux")]
    {
        use crate::platform::{nvenc::NvEncoder, vaapi::VaEncoder};
        use openh264_encoder::SoftwareEncoder;
        let cisco = || match OPENH264_PATH.get() {
            Some(path) => SoftwareEncoder::cisco(path, cap.width, cap.height),
            None => anyhow::bail!("OpenH264 isn't enabled (it needs --openh264 PATH)"),
        };
        match kind {
            "nvenc" => return Ok(Box::new(NvEncoder::new(cap)?)),
            "vaapi" => return Ok(Box::new(VaEncoder::new(cap)?)),
            #[cfg(feature = "openh264")]
            "openh264" if OPENH264_PATH.get().is_none() => {
                return Ok(Box::new(SoftwareEncoder::compiled_in(cap.width, cap.height)?));
            }
            "openh264" => return Ok(Box::new(cisco()?)),
            "auto" | "hw" => {
                let nvenc = match NvEncoder::new(cap) {
                    Ok(e) => return Ok(Box::new(e)),
                    Err(e) => e,
                };
                let vaapi = match VaEncoder::new(cap) {
                    Ok(e) => return Ok(Box::new(e)),
                    Err(e) => e,
                };
                let hardware = format!("NVIDIA (NVENC): {nvenc:#}. Intel / AMD (VA-API): {vaapi:#}");
                if kind == "auto" && OPENH264_PATH.get().is_some() {
                    eprintln!("desktop-server: no hardware H.264 encoder ({hardware}); using OpenH264");
                    return Ok(Box::new(cisco()?));
                }
                return Err(NoEncoder(format!("no hardware H.264 encoder found. {hardware}")).into());
            }
            _ => {}
        }
    }
    #[cfg(target_os = "macos")]
    if kind == "auto" || kind == "hw" {
        return Ok(Box::new(crate::platform::vt::VtEncoder::new(cap)?));
    }
    anyhow::bail!("unknown --encoder {kind:?} (expected {})",
        if cfg!(windows) { "auto, hw, sw" } else if cfg!(target_os = "macos") { "auto, hw" } else { "auto, hw, nvenc, vaapi, openh264" })
}

/// OpenH264: on Linux, Cisco's binary that the extension downloaded from Cisco
/// (with the user's consent); in local test builds, optionally compiled in.
#[cfg(any(feature = "openh264", target_os = "linux"))]
mod openh264_encoder {
    use super::{MAX_FPS, VideoEncoder, bitrate};
    use crate::platform::capture::Capturer;
    use anyhow::{Context, Result};
    use openh264::OpenH264API;
    use openh264::encoder::{
        BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode, UsageType,
    };
    use openh264::formats::{BgraSliceU8, YUVBuffer};

    pub struct SoftwareEncoder {
        encoder: Encoder,
        yuv: YUVBuffer,
        bgra: Vec<u8>,
        dims: (usize, usize),
        codec: String,
        name: &'static str,
    }

    impl SoftwareEncoder {
        /// Cisco's OpenH264 library at `path`; only accepted if its SHA-256 is that
        /// of a known Cisco release.
        #[cfg(target_os = "linux")]
        pub fn cisco(path: &std::path::Path, width: u32, height: u32) -> Result<Self> {
            let api = OpenH264API::from_blob_path(path).with_context(|| format!("loading {}", path.display()))?;
            Self::new(api, width, height, "OpenH264 (software; OpenH264 Video Codec provided by Cisco Systems, Inc.)")
        }

        #[cfg(feature = "openh264")]
        pub fn compiled_in(width: u32, height: u32) -> Result<Self> {
            Self::new(OpenH264API::from_source(), width, height, "OpenH264 (software, compiled in: local builds only)")
        }

        fn new(api: OpenH264API, width: u32, height: u32, name: &'static str) -> Result<Self> {
            let config = EncoderConfig::new()
                // ScreenContentRealTime had 300-700 ms spikes on large changes; camera mode doesn't.
                .usage_type(UsageType::CameraVideoRealTime)
                .rate_control_mode(RateControlMode::Bitrate)
                .bitrate(BitRate::from_bps(bitrate(width, height)))
                .max_frame_rate(FrameRate::from_hz(MAX_FPS as f32))
                .complexity(Complexity::Low)
                .intra_frame_period(IntraFramePeriod::from_num_frames(0))
                .skip_frames(true);
            let encoder = Encoder::with_api_config(api, config).context("creating OpenH264 encoder")?;
            let dims = (width as usize, height as usize);
            // Baseline; level 4.2 up to 1920x1088, else 5.2.
            let mbs = width.div_ceil(16) * height.div_ceil(16);
            let codec = format!("avc1.42E0{:02X}", if mbs <= 8160 { 42 } else { 52 });
            Ok(Self { encoder, yuv: YUVBuffer::new(dims.0, dims.1), bgra: Vec::new(), dims, codec, name })
        }
    }

    impl VideoEncoder for SoftwareEncoder {
        fn encode(&mut self, frame: &Capturer, _timestamp_us: u64) -> Result<(Vec<u8>, bool)> {
            frame.read_bgra(&mut self.bgra)?;
            self.yuv.read_bgra8(BgraSliceU8::new(&self.bgra, self.dims));
            let bs = self.encoder.encode(&self.yuv)?;
            let key = matches!(bs.frame_type(), FrameType::IDR | FrameType::I);
            Ok((bs.to_vec(), key))
        }
        fn force_keyframe(&mut self) {
            self.encoder.force_intra_frame();
        }
        fn codec(&self) -> &str {
            &self.codec
        }
        fn name(&self) -> &str {
            self.name
        }
        fn label(&self) -> &str {
            "OpenH264 (software)"
        }
    }
}
