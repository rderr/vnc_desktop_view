//! H.264 encoders behind one interface: Media Foundation hardware (preferred)
//! and OpenH264 software (fallback).

use anyhow::{Context, Result};
use openh264::OpenH264API;
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode, UsageType,
};
use openh264::formats::{BgraSliceU8, YUVBuffer, YUVSource};

pub const MAX_FPS: u32 = 60;
pub const BITRATE: u32 = 8_000_000;

pub trait VideoEncoder {
    /// Encodes one tightly packed BGRA frame. Returns Annex B data (may be empty
    /// if the encoder dropped the frame) and whether it is a keyframe.
    fn encode(&mut self, bgra: &[u8], timestamp_us: u64) -> Result<(Vec<u8>, bool)>;
    /// Make the next frame a keyframe.
    fn force_keyframe(&mut self);
    /// WebCodecs codec string for the stream.
    fn codec(&self) -> &str;
    fn name(&self) -> &str;
}

/// `auto` tries hardware first; `hw` / `sw` force one.
pub fn create(kind: &str, width: u32, height: u32) -> Result<Box<dyn VideoEncoder>> {
    if kind != "sw" {
        match crate::mf::MfEncoder::new(width, height) {
            Ok(e) => return Ok(Box::new(e)),
            Err(e) if kind == "hw" => return Err(e.context("hardware encoder")),
            Err(e) => eprintln!("desktop-server: no hardware encoder ({e:#}); using OpenH264"),
        }
    }
    Ok(Box::new(SoftwareEncoder::new(width, height)?))
}

/// BGRA -> planar YUV 4:2:0, reusing one buffer (SIMD-accelerated in openh264).
pub struct Converter {
    w: usize,
    h: usize,
    yuv: YUVBuffer,
}

impl Converter {
    pub fn new(width: u32, height: u32) -> Self {
        Self { w: width as usize, h: height as usize, yuv: YUVBuffer::new(width as usize, height as usize) }
    }
    pub fn i420(&mut self, bgra: &[u8]) -> &YUVBuffer {
        self.yuv.read_bgra8(BgraSliceU8::new(bgra, (self.w, self.h)));
        &self.yuv
    }
    /// NV12: the Y plane, then U and V interleaved.
    pub fn nv12(&mut self, bgra: &[u8], out: &mut Vec<u8>) {
        let (w, h) = (self.w, self.h);
        let yuv = self.i420(bgra);
        out.clear();
        out.extend_from_slice(&yuv.y()[..w * h]);
        let (u, v) = (&yuv.u()[..w * h / 4], &yuv.v()[..w * h / 4]);
        out.reserve(w * h / 2);
        for (a, b) in u.iter().zip(v) {
            out.push(*a);
            out.push(*b);
        }
    }
}

pub struct SoftwareEncoder {
    encoder: Encoder,
    conv: Converter,
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
            // Keyframes only on request (new viewer, lost data), not periodically.
            .intra_frame_period(IntraFramePeriod::from_num_frames(0))
            .skip_frames(true);
        let encoder =
            Encoder::with_api_config(OpenH264API::from_source(), config).context("creating OpenH264 encoder")?;
        Ok(Self { encoder, conv: Converter::new(width, height) })
    }
}

impl VideoEncoder for SoftwareEncoder {
    fn encode(&mut self, bgra: &[u8], _timestamp_us: u64) -> Result<(Vec<u8>, bool)> {
        let yuv = self.conv.i420(bgra);
        let bs = self.encoder.encode(yuv)?;
        let key = matches!(bs.frame_type(), FrameType::IDR | FrameType::I);
        Ok((bs.to_vec(), key))
    }
    fn force_keyframe(&mut self) {
        self.encoder.force_intra_frame();
    }
    fn codec(&self) -> &str {
        "avc1.42E033" // constrained baseline, level 5.1
    }
    fn name(&self) -> &str {
        "OpenH264 (software)"
    }
}
