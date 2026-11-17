//! Windows screen capture with DXGI Desktop Duplication.
//!
//! The OS hands us a new frame only when something on screen changed, so an
//! idle desktop costs (almost) nothing. Frames are copied into a CPU-readable
//! staging texture and returned as tightly packed BGRA.

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

pub struct Capturer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    dup: IDXGIOutputDuplication,
    staging: ID3D11Texture2D,
    /// Size of the desktop (what DXGI gives us).
    pub src_width: u32,
    pub src_height: u32,
    /// Size of the frames we return: the desktop rounded down to even
    /// dimensions, since H.264 4:2:0 needs them.
    pub width: u32,
    pub height: u32,
}

pub enum Frame<'a> {
    /// Tightly packed BGRA, `width * height * 4` bytes.
    Image(&'a [u8]),
    /// Nothing changed on screen within the timeout.
    Unchanged,
}

impl Capturer {
    /// Captures the primary output of the default adapter.
    pub fn new() -> Result<Self> {
        unsafe {
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("D3D11CreateDevice")?;
            let device: ID3D11Device = device.context("no D3D11 device")?;
            let context = context.context("no D3D11 context")?;

            let adapter = device.cast::<IDXGIDevice>()?.GetAdapter()?;
            let output: IDXGIOutput1 = adapter.EnumOutputs(0).context("no display output")?.cast()?;
            let dup = output
                .DuplicateOutput(&device)
                .context("DuplicateOutput (is this running in the logged-in user's session?)")?;

            let desc = dup.GetDesc();
            let (src_width, src_height) = (desc.ModeDesc.Width, desc.ModeDesc.Height);
            let staging_desc = D3D11_TEXTURE2D_DESC {
                Width: src_width,
                Height: src_height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut staging = None;
            device.CreateTexture2D(&staging_desc, None, Some(&mut staging))?;

            Ok(Self {
                device,
                context,
                dup,
                staging: staging.context("no staging texture")?,
                src_width,
                src_height,
                width: src_width & !1,
                height: src_height & !1,
            })
        }
    }

    /// Waits up to `timeout_ms` for the screen to change. On a change, copies
    /// the desktop into `buf` (resized as needed) and returns it.
    pub fn next<'a>(&mut self, timeout_ms: u32, buf: &'a mut Vec<u8>) -> Result<Frame<'a>> {
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            if let Err(e) = self.dup.AcquireNextFrame(timeout_ms, &mut info, &mut resource) {
                if e.code() == DXGI_ERROR_WAIT_TIMEOUT {
                    return Ok(Frame::Unchanged);
                }
                // DXGI_ERROR_ACCESS_LOST: mode change, desktop switch (UAC, lock screen)…
                bail!("AcquireNextFrame: {e}");
            }
            // Only the mouse pointer moved: the image is unchanged.
            if info.LastPresentTime == 0 {
                self.dup.ReleaseFrame()?;
                return Ok(Frame::Unchanged);
            }
            let texture: ID3D11Texture2D = resource.context("no desktop resource")?.cast()?;
            self.context.CopyResource(&self.staging, &texture);
            self.dup.ReleaseFrame()?;

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let row_bytes = self.width as usize * 4;
            buf.resize(row_bytes * self.height as usize, 0);
            let src = mapped.pData as *const u8;
            for y in 0..self.height as usize {
                let row = std::slice::from_raw_parts(src.add(y * mapped.RowPitch as usize), row_bytes);
                buf[y * row_bytes..(y + 1) * row_bytes].copy_from_slice(row);
            }
            self.context.Unmap(&self.staging, 0);
            let _ = &self.device; // keep the device alive alongside the duplication
            Ok(Frame::Image(buf))
        }
    }
}
