//! H.264 encoders behind one interface.
//!
//! Shipped builds only use encoders whose H.264 patent licensing is covered by
//! someone else: Windows' Media Foundation encoders (the GPU's, else Windows'
//! own software encoder), and on Linux the GPU's encoder through its driver
//! (NVENC, VA-API). OpenH264 is only available in local builds with
//! `--features openh264`: Cisco's patent license covers Cisco's own binaries,
//! not copies compiled into this program.

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
}

/// Windows: `auto` tries the hardware encoder, then Windows' software encoder;
/// `hw` / `sw` force one. Linux: `auto` / `hw` tries NVENC (NVIDIA), then
/// VA-API (Intel, AMD); `nvenc` / `vaapi` force one.
/// `openh264` needs a build with that feature.
pub fn create(kind: &str, cap: &Capturer) -> Result<Box<dyn VideoEncoder>> {
    #[cfg(feature = "openh264")]
    if kind == "openh264" {
        return Ok(Box::new(openh264_encoder::SoftwareEncoder::new(cap.width, cap.height)?));
    }
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
            _ => {}
        }
    }
    #[cfg(target_os = "linux")]
    {
        use crate::platform::{nvenc::NvEncoder, vaapi::VaEncoder};
        match kind {
            "nvenc" => return Ok(Box::new(NvEncoder::new(cap)?)),
            "vaapi" => return Ok(Box::new(VaEncoder::new(cap)?)),
            "auto" | "hw" => {
                let nvenc = match NvEncoder::new(cap) {
                    Ok(e) => return Ok(Box::new(e)),
                    Err(e) => e,
                };
                return match VaEncoder::new(cap) {
                    Ok(e) => Ok(Box::new(e)),
                    Err(vaapi) => anyhow::bail!("no hardware H.264 encoder found. NVIDIA (NVENC): {nvenc:#}. Intel / AMD (VA-API): {vaapi:#}"),
                };
            }
            _ => {}
        }
    }
    anyhow::bail!("unknown --encoder {kind:?} (expected {}{})",
        if cfg!(windows) { "auto, hw, sw" } else { "auto, hw, nvenc, vaapi" },
        if cfg!(feature = "openh264") { ", openh264" } else { "" })
}

#[cfg(feature = "openh264")]
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
    }

    impl SoftwareEncoder {
        pub fn new(width: u32, height: u32) -> Result<Self> {
            let config = EncoderConfig::new()
                // ScreenContentRealTime had 300-700 ms spikes on large changes; camera mode doesn't.
                .usage_type(UsageType::CameraVideoRealTime)
                .rate_control_mode(RateControlMode::Bitrate)
                .bitrate(BitRate::from_bps(bitrate(width, height)))
                .max_frame_rate(FrameRate::from_hz(MAX_FPS as f32))
                .complexity(Complexity::Low)
                .intra_frame_period(IntraFramePeriod::from_num_frames(0))
                .skip_frames(true);
            let encoder =
                Encoder::with_api_config(OpenH264API::from_source(), config).context("creating OpenH264 encoder")?;
            let dims = (width as usize, height as usize);
            Ok(Self { encoder, yuv: YUVBuffer::new(dims.0, dims.1), bgra: Vec::new(), dims })
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
            "avc1.42E033"
        }
        fn name(&self) -> &str {
            "OpenH264 (software, local builds only)"
        }
    }
}
