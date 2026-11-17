//! Windows screen capture with DXGI Desktop Duplication.
//!
//! The OS hands us a new frame only when something on screen changed, so an
//! idle desktop costs (almost) nothing. Each new frame is copied, on the GPU,
//! into our own texture (`texture`), which keeps the latest image after the
//! duplication's frame is released. Encoders take it from there: the GPU path
//! never brings it to system memory; `read_bgra` does, for encoders that need it.

use anyhow::{Context, Result};
use std::cell::OnceCell;
use windows::Win32::Foundation::{HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

pub struct Capturer {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    dup: IDXGIOutputDuplication,
    /// The latest desktop image (BGRA, `src_width` x `src_height`), on the GPU.
    pub texture: ID3D11Texture2D,
    /// CPU-readable copy for `read_bgra`, created on first use.
    staging: OnceCell<ID3D11Texture2D>,
    /// The device supports the D3D11 video API (GPU color conversion).
    pub video_support: bool,
    /// The GPU the desktop is on, so the encoder can be picked from the same one.
    pub adapter_luid: LUID,
    /// Size of the desktop (what DXGI gives us).
    pub src_width: u32,
    pub src_height: u32,
    /// Size of the encoded frames: the desktop rounded down to even
    /// dimensions, since H.264 4:2:0 needs them.
    pub width: u32,
    pub height: u32,
}

pub enum Frame {
    /// The screen changed; `Capturer::texture` holds the new image.
    Changed,
    /// Capture stopped working (resolution change, lock screen, UAC prompt…);
    /// create a new Capturer.
    Lost(String),
    /// Nothing changed on screen within the timeout.
    Unchanged,
}

impl Capturer {
    /// Captures the primary output of the default adapter.
    pub fn new() -> Result<Self> {
        unsafe {
            // Video support enables GPU color conversion; fall back without it
            // (e.g. Microsoft Basic Render Driver), and convert on the CPU instead.
            let (device, context, video_support) = match create_device(D3D11_CREATE_DEVICE_VIDEO_SUPPORT) {
                Ok((d, c)) => (d, c, true),
                Err(_) => {
                    let (d, c) = create_device(D3D11_CREATE_DEVICE_FLAG(0))?;
                    (d, c, false)
                }
            };
            // Media Foundation encoders use the device from their own threads.
            if let Ok(mt) = device.cast::<ID3D11Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }

            let adapter = device.cast::<IDXGIDevice>()?.GetAdapter()?;
            let adapter_luid = adapter.GetDesc()?.AdapterLuid;
            let output: IDXGIOutput1 = adapter.EnumOutputs(0).context("no display output")?.cast()?;
            let dup = output
                .DuplicateOutput(&device)
                .context("DuplicateOutput (is this running in the logged-in user's session?)")?;

            let desc = dup.GetDesc();
            let (src_width, src_height) = (desc.ModeDesc.Width, desc.ModeDesc.Height);
            let texture_desc = D3D11_TEXTURE2D_DESC {
                Width: src_width,
                Height: src_height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut texture = None;
            device.CreateTexture2D(&texture_desc, None, Some(&mut texture))?;

            Ok(Self {
                device,
                context,
                dup,
                texture: texture.context("no frame texture")?,
                staging: OnceCell::new(),
                video_support,
                adapter_luid,
                src_width,
                src_height,
                width: src_width & !1,
                height: src_height & !1,
            })
        }
    }

    /// Waits up to `timeout_ms` for the screen to change. On a change, the new
    /// image is in `texture`.
    pub fn next(&mut self, timeout_ms: u32) -> Result<Frame> {
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            if let Err(e) = self.dup.AcquireNextFrame(timeout_ms, &mut info, &mut resource) {
                if e.code() == DXGI_ERROR_WAIT_TIMEOUT {
                    return Ok(Frame::Unchanged);
                }
                // DXGI_ERROR_ACCESS_LOST: resolution change, or a switch to a secure
                // desktop (lock screen, UAC prompt). The caller re-creates the capturer.
                return Ok(Frame::Lost(e.to_string()));
            }
            // Only the mouse pointer moved: the image is unchanged.
            if info.LastPresentTime == 0 {
                self.dup.ReleaseFrame()?;
                return Ok(Frame::Unchanged);
            }
            let desktop: ID3D11Texture2D = resource.context("no desktop resource")?.cast()?;
            self.context.CopyResource(&self.texture, &desktop);
            self.dup.ReleaseFrame()?;
            Ok(Frame::Changed)
        }
    }

    /// Copies the latest image to system memory as tightly packed BGRA,
    /// `width * height * 4` bytes.
    pub fn read_bgra(&self, buf: &mut Vec<u8>) -> Result<()> {
        unsafe {
            if self.staging.get().is_none() {
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                self.texture.GetDesc(&mut desc);
                desc.Usage = D3D11_USAGE_STAGING;
                desc.BindFlags = 0;
                desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
                let mut staging = None;
                self.device.CreateTexture2D(&desc, None, Some(&mut staging))?;
                let _ = self.staging.set(staging.context("no staging texture")?);
            }
            let staging = self.staging.get().unwrap();
            self.context.CopyResource(staging, &self.texture);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let row_bytes = self.width as usize * 4;
            buf.resize(row_bytes * self.height as usize, 0);
            let src = mapped.pData as *const u8;
            for y in 0..self.height as usize {
                let row = std::slice::from_raw_parts(src.add(y * mapped.RowPitch as usize), row_bytes);
                buf[y * row_bytes..(y + 1) * row_bytes].copy_from_slice(row);
            }
            self.context.Unmap(staging, 0);
            Ok(())
        }
    }
}

unsafe fn create_device(extra: D3D11_CREATE_DEVICE_FLAG) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    unsafe {
        let mut device = None;
        let mut context = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | extra,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .context("D3D11CreateDevice")?;
        Ok((device.context("no D3D11 device")?, context.context("no D3D11 context")?))
    }
}

/// Whether a `Capturer::new` error means a secure desktop (lock screen, UAC
/// prompt, Ctrl+Alt+Del screen) is showing, which a program running as the
/// user isn't allowed to capture.
pub fn is_secure_desktop(err: &anyhow::Error) -> bool {
    err.downcast_ref::<windows::core::Error>()
        .is_some_and(|e| e.code() == windows::Win32::Foundation::E_ACCESSDENIED)
}
