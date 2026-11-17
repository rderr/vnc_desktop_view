//! BGRA -> NV12 on the GPU with the D3D11 video processor (the fixed-function
//! scaler/color converter that video players use), so the desktop image goes
//! from capture to encoder without leaving video memory.
//!
//! Output is BT.601 limited range, the same as the CPU converter in
//! encoder.rs and what viewers assume for H.264 without color info.

use super::capture::Capturer;
use anyhow::{Context, Result};
use std::mem::ManuallyDrop;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;

pub struct Nv12Converter {
    video: ID3D11VideoContext,
    processor: ID3D11VideoProcessor,
    input: ID3D11VideoProcessorInputView,
    /// Output textures, used in turn so the encoder can still be reading one
    /// while the next frame is converted into another.
    outputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
}

impl Nv12Converter {
    /// Converts `cap.texture` into `ring` NV12 textures of `cap.width` x `cap.height`.
    pub fn new(cap: &Capturer, ring: usize) -> Result<Self> {
        anyhow::ensure!(cap.video_support, "the D3D11 device has no video support");
        unsafe {
            let video_device: ID3D11VideoDevice = cap.device.cast().context("ID3D11VideoDevice")?;
            let video: ID3D11VideoContext = cap.context.cast().context("ID3D11VideoContext")?;
            let rate = DXGI_RATIONAL { Numerator: 60, Denominator: 1 };
            let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: cap.src_width,
                InputHeight: cap.src_height,
                OutputFrameRate: rate,
                OutputWidth: cap.width,
                OutputHeight: cap.height,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            };
            let enumerator = video_device.CreateVideoProcessorEnumerator(&content).context("CreateVideoProcessorEnumerator")?;
            let processor = video_device.CreateVideoProcessor(&enumerator, 0).context("CreateVideoProcessor")?;

            let mut input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                ..Default::default()
            };
            input_desc.Anonymous.Texture2D = D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 };
            let mut input = None;
            video_device
                .CreateVideoProcessorInputView(&cap.texture, &enumerator, &input_desc, Some(&mut input))
                .context("CreateVideoProcessorInputView")?;

            let mut outputs = Vec::new();
            for _ in 0..ring.max(1) {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: cap.width,
                    Height: cap.height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_NV12,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut texture = None;
                cap.device.CreateTexture2D(&desc, None, Some(&mut texture)).context("creating NV12 texture")?;
                let texture = texture.context("no NV12 texture")?;
                let mut out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                    ..Default::default()
                };
                out_desc.Anonymous.Texture2D = D3D11_TEX2D_VPOV { MipSlice: 0 };
                let mut view = None;
                video_device
                    .CreateVideoProcessorOutputView(&texture, &enumerator, &out_desc, Some(&mut view))
                    .context("CreateVideoProcessorOutputView")?;
                outputs.push((texture, view.context("no output view")?));
            }

            // Plain conversion: no scaling (the even-sized crop aside), no
            // driver "enhancements", sRGB full range in, BT.601 limited range out.
            let crop = RECT { left: 0, top: 0, right: cap.width as i32, bottom: cap.height as i32 };
            video.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            video.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            video.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&crop));
            video.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&crop));
            video.VideoProcessorSetOutputTargetRect(&processor, true, Some(&crop));
            if let Ok(video1) = video.cast::<ID3D11VideoContext1>() {
                video1.VideoProcessorSetStreamColorSpace1(&processor, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
                video1.VideoProcessorSetOutputColorSpace1(&processor, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601);
            } else {
                // Bitfield: Usage:1, RGB_Range:1 (0 = full), YCbCr_Matrix:1 (0 = BT.601),
                // YCbCr_xvYCC:1, Nominal_Range:2 (1 = 16-235).
                video.VideoProcessorSetStreamColorSpace(&processor, 0, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 });
                video.VideoProcessorSetOutputColorSpace(&processor, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 1 << 4 });
            }

            Ok(Self {
                video,
                processor,
                input: input.context("no input view")?,
                outputs,
                next: 0,
            })
        }
    }

    /// Converts the capturer's current image; returns the NV12 texture holding it.
    pub fn convert(&mut self) -> Result<ID3D11Texture2D> {
        let (texture, view) = &self.outputs[self.next];
        self.next = (self.next + 1) % self.outputs.len();
        unsafe {
            let mut streams = [D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(Some(self.input.clone())),
                ..Default::default()
            }];
            let result = self.video.VideoProcessorBlt(&self.processor, view, 0, &streams);
            ManuallyDrop::drop(&mut streams[0].pInputSurface);
            result.context("VideoProcessorBlt")?;
        }
        Ok(texture.clone())
    }
}
