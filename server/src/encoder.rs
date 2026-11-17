//! H.264 encoders behind one interface. Shipped builds use Media Foundation:
//! the GPU's hardware encoder, else Microsoft's software encoder that comes
//! with Windows (both covered by Windows' H.264 licensing). OpenH264 is only
//! available in local builds with `--features openh264`: Cisco's patent
//! license covers Cisco's own binaries, not copies compiled into this program.

use crate::capture::Capturer;
use anyhow::Result;

pub const MAX_FPS: u32 = 60;
pub const BITRATE: u32 = 8_000_000;

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

/// `auto` tries the hardware encoder, then Windows' software encoder.
/// `hw` / `sw` force one; `openh264` needs a build with that feature.
pub fn create(kind: &str, cap: &Capturer) -> Result<Box<dyn VideoEncoder>> {
    match kind {
        "hw" => Ok(Box::new(crate::mf::MfEncoder::new(cap, true)?)),
        "sw" => Ok(Box::new(crate::mf::MfEncoder::new(cap, false)?)),
        #[cfg(feature = "openh264")]
        "openh264" => Ok(Box::new(openh264_encoder::SoftwareEncoder::new(cap.width, cap.height)?)),
        "auto" => match crate::mf::MfEncoder::new(cap, true) {
            Ok(e) => Ok(Box::new(e)),
            Err(e) => {
                eprintln!("desktop-server: no hardware encoder ({e:#}); using Windows' software encoder");
                Ok(Box::new(crate::mf::MfEncoder::new(cap, false)?))
            }
        },
        other => anyhow::bail!("unknown --encoder {other:?} (expected auto, hw, sw{})",
            if cfg!(feature = "openh264") { ", openh264" } else { "" }),
    }
}

/// BGRA -> NV12 (Y plane, then interleaved U/V at half resolution), BT.601
/// limited range, on the CPU: the fallback when the GPU can't convert (see
/// gpu.rs). Rows are split across threads; ~2 ms for 1440x900.
pub struct Converter {
    w: usize,
    h: usize,
}

impl Converter {
    pub fn new(width: u32, height: u32) -> Self {
        Self { w: width as usize, h: height as usize }
    }

    pub fn nv12(&mut self, bgra: &[u8], out: &mut Vec<u8>) {
        let (w, h) = (self.w, self.h);
        out.resize(w * h * 3 / 2, 0);
        let (y_plane, uv_plane) = out.split_at_mut(w * h);
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(8);
        // Work in bands of row pairs, so each band owns whole chroma rows.
        let pairs = h / 2;
        let pairs_per_band = pairs.div_ceil(threads).max(1);
        std::thread::scope(|s| {
            let y_bands = y_plane.chunks_mut(pairs_per_band * 2 * w);
            let uv_bands = uv_plane.chunks_mut(pairs_per_band * w);
            for (band, (y_out, uv_out)) in y_bands.zip(uv_bands).enumerate() {
                let first_row = band * pairs_per_band * 2;
                s.spawn(move || convert_band(bgra, w, first_row, y_out, uv_out));
            }
        });
    }
}

fn convert_band(bgra: &[u8], w: usize, first_row: usize, y_out: &mut [u8], uv_out: &mut [u8]) {
    let luma = |b: i32, g: i32, r: i32| (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
    for pair in 0..uv_out.len() / w {
        let row0 = first_row + pair * 2;
        let (src0, src1) = (&bgra[row0 * w * 4..(row0 + 1) * w * 4], &bgra[(row0 + 1) * w * 4..(row0 + 2) * w * 4]);
        let (dst0, dst1) = y_out[pair * 2 * w..(pair * 2 + 2) * w].split_at_mut(w);
        let uv = &mut uv_out[pair * w..(pair + 1) * w];
        for x in (0..w).step_by(2) {
            let px = |row: &[u8], i: usize| (row[i * 4] as i32, row[i * 4 + 1] as i32, row[i * 4 + 2] as i32);
            let p = [px(src0, x), px(src0, x + 1), px(src1, x), px(src1, x + 1)];
            dst0[x] = luma(p[0].0, p[0].1, p[0].2);
            dst0[x + 1] = luma(p[1].0, p[1].1, p[1].2);
            dst1[x] = luma(p[2].0, p[2].1, p[2].2);
            dst1[x + 1] = luma(p[3].0, p[3].1, p[3].2);
            let (b, g, r) = p.iter().fold((0, 0, 0), |a, c| (a.0 + c.0, a.1 + c.1, a.2 + c.2));
            let (b, g, r) = (b / 4, g / 4, r / 4);
            uv[x] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
            uv[x + 1] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
        }
    }
}

#[cfg(feature = "openh264")]
mod openh264_encoder {
    use super::{BITRATE, MAX_FPS, VideoEncoder};
    use crate::capture::Capturer;
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
                .bitrate(BitRate::from_bps(BITRATE))
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
