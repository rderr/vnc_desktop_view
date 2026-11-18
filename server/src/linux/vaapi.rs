//! H.264 with VA-API: the video encoder in Intel GPUs (driver: iHD or i965)
//! and AMD GPUs (Mesa's radeonsi), through libva (loaded at runtime).
//!
//! Frames are converted to NV12 on the CPU and uploaded into a VA surface.
//! The stream uses Constrained Baseline when the driver offers it (like the
//! Windows server: no frame reordering, so viewers show every frame at once),
//! else Main, else High, with one reference frame and no B-frames. The driver
//! writes the SPS/PPS headers itself (Intel's and Mesa's drivers do when no
//! packed headers are given), which we check on the first keyframe.
//!
//! Untested on real hardware so far; modeled on libva-utils' h264encode and
//! FFmpeg's vaapi_encode_h264.

use super::capture::Capturer;
use super::va_sys::*;
use crate::convert::Converter;
use crate::encoder::{MAX_FPS, VideoEncoder, bitrate};
use anyhow::{Context, Result, anyhow, bail};
use libloading::Library;
use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

type Dpy = VADisplay;

/// Set when encoding failed after a successful setup (e.g. a driver that
/// doesn't write sequence headers), so capture restarts report that instead
/// of failing the same way over and over.
static FAILED: std::sync::OnceLock<String> = std::sync::OnceLock::new();
type Buf = VABufferID;

/// The libva functions we use.
struct Va {
    get_display_drm: unsafe extern "C" fn(c_int) -> Dpy,
    initialize: unsafe extern "C" fn(Dpy, *mut c_int, *mut c_int) -> VAStatus,
    terminate: unsafe extern "C" fn(Dpy) -> VAStatus,
    query_vendor_string: unsafe extern "C" fn(Dpy) -> *const c_char,
    error_str: unsafe extern "C" fn(VAStatus) -> *const c_char,
    max_num_entrypoints: unsafe extern "C" fn(Dpy) -> c_int,
    query_config_entrypoints: unsafe extern "C" fn(Dpy, VAProfile::Type, *mut VAEntrypoint::Type, *mut c_int) -> VAStatus,
    get_config_attributes: unsafe extern "C" fn(Dpy, VAProfile::Type, VAEntrypoint::Type, *mut VAConfigAttrib, c_int) -> VAStatus,
    create_config: unsafe extern "C" fn(Dpy, VAProfile::Type, VAEntrypoint::Type, *mut VAConfigAttrib, c_int, *mut VAConfigID) -> VAStatus,
    destroy_config: unsafe extern "C" fn(Dpy, VAConfigID) -> VAStatus,
    create_surfaces: unsafe extern "C" fn(Dpy, c_uint, c_uint, c_uint, *mut VASurfaceID, c_uint, *mut c_void, c_uint) -> VAStatus,
    destroy_surfaces: unsafe extern "C" fn(Dpy, *mut VASurfaceID, c_int) -> VAStatus,
    create_context: unsafe extern "C" fn(Dpy, VAConfigID, c_int, c_int, c_int, *mut VASurfaceID, c_int, *mut VAContextID) -> VAStatus,
    destroy_context: unsafe extern "C" fn(Dpy, VAContextID) -> VAStatus,
    create_buffer: unsafe extern "C" fn(Dpy, VAContextID, VABufferType::Type, c_uint, c_uint, *mut c_void, *mut Buf) -> VAStatus,
    destroy_buffer: unsafe extern "C" fn(Dpy, Buf) -> VAStatus,
    map_buffer: unsafe extern "C" fn(Dpy, Buf, *mut *mut c_void) -> VAStatus,
    unmap_buffer: unsafe extern "C" fn(Dpy, Buf) -> VAStatus,
    begin_picture: unsafe extern "C" fn(Dpy, VAContextID, VASurfaceID) -> VAStatus,
    render_picture: unsafe extern "C" fn(Dpy, VAContextID, *mut Buf, c_int) -> VAStatus,
    end_picture: unsafe extern "C" fn(Dpy, VAContextID) -> VAStatus,
    sync_surface: unsafe extern "C" fn(Dpy, VASurfaceID) -> VAStatus,
    derive_image: unsafe extern "C" fn(Dpy, VASurfaceID, *mut VAImage) -> VAStatus,
    create_image: unsafe extern "C" fn(Dpy, *mut VAImageFormat, c_int, c_int, *mut VAImage) -> VAStatus,
    put_image: unsafe extern "C" fn(Dpy, VASurfaceID, VAImageID, c_int, c_int, c_uint, c_uint, c_int, c_int, c_uint, c_uint) -> VAStatus,
    destroy_image: unsafe extern "C" fn(Dpy, VAImageID) -> VAStatus,
    _libs: (Library, Library),
}

impl Va {
    fn load() -> Result<Self> {
        unsafe {
            let va = Library::new("libva.so.2").context("loading libva.so.2 (is VA-API installed?)")?;
            let drm = Library::new("libva-drm.so.2").context("loading libva-drm.so.2")?;
            macro_rules! f {
                ($lib:expr, $name:literal) => {
                    *$lib.get(concat!($name, "\0").as_bytes()).context(concat!("libva lacks ", $name))?
                };
            }
            Ok(Self {
                get_display_drm: f!(drm, "vaGetDisplayDRM"),
                initialize: f!(va, "vaInitialize"),
                terminate: f!(va, "vaTerminate"),
                query_vendor_string: f!(va, "vaQueryVendorString"),
                error_str: f!(va, "vaErrorStr"),
                max_num_entrypoints: f!(va, "vaMaxNumEntrypoints"),
                query_config_entrypoints: f!(va, "vaQueryConfigEntrypoints"),
                get_config_attributes: f!(va, "vaGetConfigAttributes"),
                create_config: f!(va, "vaCreateConfig"),
                destroy_config: f!(va, "vaDestroyConfig"),
                create_surfaces: f!(va, "vaCreateSurfaces"),
                destroy_surfaces: f!(va, "vaDestroySurfaces"),
                create_context: f!(va, "vaCreateContext"),
                destroy_context: f!(va, "vaDestroyContext"),
                create_buffer: f!(va, "vaCreateBuffer"),
                destroy_buffer: f!(va, "vaDestroyBuffer"),
                map_buffer: f!(va, "vaMapBuffer"),
                unmap_buffer: f!(va, "vaUnmapBuffer"),
                begin_picture: f!(va, "vaBeginPicture"),
                render_picture: f!(va, "vaRenderPicture"),
                end_picture: f!(va, "vaEndPicture"),
                sync_surface: f!(va, "vaSyncSurface"),
                derive_image: f!(va, "vaDeriveImage"),
                create_image: f!(va, "vaCreateImage"),
                put_image: f!(va, "vaPutImage"),
                destroy_image: f!(va, "vaDestroyImage"),
                _libs: (va, drm),
            })
        }
    }

    fn check(&self, what: &str, status: VAStatus) -> Result<()> {
        if status == VA_STATUS_SUCCESS as VAStatus {
            return Ok(());
        }
        let msg = unsafe { CStr::from_ptr((self.error_str)(status)) }.to_string_lossy();
        Err(anyhow!("{what} failed: {msg} ({status})"))
    }
}

/// A GPU that can encode H.264, opened and initialized.
struct Device {
    fd: OwnedFd,
    dpy: Dpy,
    vendor: String,
    profile: VAProfile::Type,
    entrypoint: VAEntrypoint::Type,
    rate_control: u32,
}

/// Opens the first render node whose driver can encode H.264 frames.
fn open_device(va: &Va) -> Result<Device> {
    let mut reasons = Vec::new();
    for n in 128..192 {
        let path = format!("/dev/dri/renderD{n}");
        if !std::path::Path::new(&path).exists() {
            continue;
        }
        match unsafe { try_device(va, &path) } {
            Ok(d) => return Ok(d),
            Err(e) => reasons.push(format!("{path}: {e:#}")),
        }
    }
    if reasons.is_empty() {
        bail!("no GPU render device in /dev/dri");
    }
    bail!("no GPU with an H.264 encoder ({})", reasons.join("; "))
}

unsafe fn try_device(va: &Va, path: &str) -> Result<Device> {
    unsafe {
        let cpath = std::ffi::CString::new(path)?;
        let raw = libc::open(cpath.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
        if raw < 0 {
            bail!("{}", std::io::Error::last_os_error());
        }
        let fd = OwnedFd::from_raw_fd(raw);
        let dpy = (va.get_display_drm)(fd.as_raw_fd());
        if dpy.is_null() {
            bail!("vaGetDisplayDRM failed");
        }
        let (mut major, mut minor) = (0, 0);
        // libva prints its version to stderr on initialize; that's fine for our log.
        if let Err(e) = va.check("vaInitialize", (va.initialize)(dpy, &mut major, &mut minor)) {
            return Err(e);
        }
        let vendor = CStr::from_ptr((va.query_vendor_string)(dpy)).to_string_lossy().into_owned();
        let chosen = (|| -> Result<(VAProfile::Type, VAEntrypoint::Type, u32)> {
            let mut entrypoints = vec![0; (va.max_num_entrypoints)(dpy).max(1) as usize];
            for profile in [VAProfile::VAProfileH264ConstrainedBaseline, VAProfile::VAProfileH264Main, VAProfile::VAProfileH264High] {
                let mut count = 0;
                if (va.query_config_entrypoints)(dpy, profile, entrypoints.as_mut_ptr(), &mut count) != 0 {
                    continue;
                }
                let available = &entrypoints[..count.max(0) as usize];
                // Full encoder first; some Intel GPUs only have the low-power one.
                let Some(&entrypoint) = [VAEntrypoint::VAEntrypointEncSlice, VAEntrypoint::VAEntrypointEncSliceLP]
                    .iter()
                    .find(|e| available.contains(e))
                else {
                    continue;
                };
                let mut attrs = [
                    VAConfigAttrib { type_: VAConfigAttribType::VAConfigAttribRTFormat, value: 0 },
                    VAConfigAttrib { type_: VAConfigAttribType::VAConfigAttribRateControl, value: 0 },
                ];
                va.check("vaGetConfigAttributes", (va.get_config_attributes)(dpy, profile, entrypoint, attrs.as_mut_ptr(), 2))?;
                if attrs[0].value == VA_ATTRIB_NOT_SUPPORTED || attrs[0].value & VA_RT_FORMAT_YUV420 == 0 {
                    continue;
                }
                let rc = attrs[1].value;
                let rate_control = if rc == VA_ATTRIB_NOT_SUPPORTED {
                    0
                } else if rc & VA_RC_CBR != 0 {
                    VA_RC_CBR
                } else if rc & VA_RC_VBR != 0 {
                    VA_RC_VBR
                } else {
                    0
                };
                return Ok((profile, entrypoint, rate_control));
            }
            bail!("{vendor}: no H.264 encoding")
        })();
        match chosen {
            Ok((profile, entrypoint, rate_control)) => Ok(Device { fd, dpy, vendor, profile, entrypoint, rate_control }),
            Err(e) => {
                (va.terminate)(dpy);
                Err(e)
            }
        }
    }
}

pub struct VaEncoder {
    va: Va,
    dev: Device,
    config: VAConfigID,
    context: VAContextID,
    /// The frame to encode, and two reconstructed frames used in turn: one
    /// written by the current frame, the other the previous frame (its reference).
    input: VASurfaceID,
    recon: [VASurfaceID; 2],
    next_recon: usize,
    coded: Buf,
    /// For drivers that can't map surfaces directly (vaDeriveImage): an image
    /// we fill and copy into the surface.
    upload: Option<VAImage>,
    width: u32,
    height: u32,
    /// Size in macroblocks (16x16).
    mb_width: u32,
    mb_height: u32,
    conv: Converter,
    nv12: Vec<u8>,
    /// Frames since the last keyframe (H.264 frame_num), and the keyframe count (idr_pic_id).
    frame_num: u32,
    idr_count: u16,
    force_key: bool,
    checked_headers: bool,
    level: u8,
    codec: String,
    name: String,
}

impl VaEncoder {
    pub fn new(cap: &Capturer) -> Result<Self> {
        if let Some(why) = FAILED.get() {
            bail!("{why}");
        }
        let (width, height) = (cap.width, cap.height);
        let va = Va::load()?;
        let dev = open_device(&va)?;
        let (mb_width, mb_height) = (width.div_ceil(16), height.div_ceil(16));
        let mbs = mb_width * mb_height;
        // 4.2 covers 1920x1088 at 60 fps; 5.2 up to 4096x2304.
        let level: u8 = if mbs <= 8160 { 42 } else { 52 };
        let (profile_hex, profile_name) = match dev.profile {
            VAProfile::VAProfileH264ConstrainedBaseline => ("42C0", "Constrained Baseline"),
            VAProfile::VAProfileH264Main => ("4D40", "Main"),
            _ => ("6400", "High"),
        };
        let mut this = Self {
            config: VA_INVALID_ID,
            context: VA_INVALID_ID,
            input: VA_INVALID_SURFACE,
            recon: [VA_INVALID_SURFACE; 2],
            next_recon: 0,
            coded: VA_INVALID_ID,
            upload: None,
            width,
            height,
            mb_width,
            mb_height,
            conv: Converter::new(width, height),
            nv12: Vec::new(),
            frame_num: 0,
            idr_count: 0,
            force_key: true,
            checked_headers: false,
            level,
            codec: format!("avc1.{profile_hex}{level:02X}"),
            name: format!(
                "VA-API on {} ({}, {profile_name}{})",
                dev.vendor,
                if dev.entrypoint == VAEntrypoint::VAEntrypointEncSliceLP { "hardware, low-power" } else { "hardware" },
                if dev.rate_control == 0 { ", constant quality" } else { "" }
            ),
            va,
            dev,
        };
        unsafe { this.init()? };
        Ok(this)
    }

    unsafe fn init(&mut self) -> Result<()> {
        let (va, dpy) = (&self.va, self.dev.dpy);
        unsafe {
            let mut attrs = vec![VAConfigAttrib { type_: VAConfigAttribType::VAConfigAttribRTFormat, value: VA_RT_FORMAT_YUV420 }];
            if self.dev.rate_control != 0 {
                attrs.push(VAConfigAttrib { type_: VAConfigAttribType::VAConfigAttribRateControl, value: self.dev.rate_control });
            }
            va.check(
                "vaCreateConfig",
                (va.create_config)(dpy, self.dev.profile, self.dev.entrypoint, attrs.as_mut_ptr(), attrs.len() as c_int, &mut self.config),
            )?;
            let (w, h) = (self.mb_width * 16, self.mb_height * 16);
            let mut surfaces = [VA_INVALID_SURFACE; 3];
            va.check("vaCreateSurfaces", (va.create_surfaces)(dpy, VA_RT_FORMAT_YUV420, w, h, surfaces.as_mut_ptr(), 3, std::ptr::null_mut(), 0))?;
            self.input = surfaces[0];
            self.recon = [surfaces[1], surfaces[2]];
            va.check(
                "vaCreateContext",
                (va.create_context)(dpy, self.config, w as c_int, h as c_int, VA_PROGRESSIVE as c_int, surfaces.as_mut_ptr(), 3, &mut self.context),
            )?;
            let coded_size = (w * h * 3 / 2).max(1 << 20);
            va.check(
                "vaCreateBuffer (coded)",
                (va.create_buffer)(dpy, self.context, VABufferType::VAEncCodedBufferType, coded_size, 1, std::ptr::null_mut(), &mut self.coded),
            )?;
        }
        Ok(())
    }

    /// Copies the NV12 frame into the input surface.
    unsafe fn upload(&mut self) -> Result<()> {
        let (va, dpy) = (&self.va, self.dev.dpy);
        let (w, h) = (self.width as usize, self.height as usize);
        unsafe {
            // Map the surface itself if the driver allows; else fill an image and copy it in.
            let mut image: VAImage = std::mem::zeroed();
            let mut derived = self.upload.is_none() && (va.derive_image)(dpy, self.input, &mut image) == 0;
            if derived && image.format.fourcc != VA_FOURCC_NV12 {
                (va.destroy_image)(dpy, image.image_id); // mapped, but not as NV12
                derived = false;
            }
            if !derived {
                // From now on always upload through our own image.
                if self.upload.is_none() {
                    let mut format: VAImageFormat = std::mem::zeroed();
                    format.fourcc = VA_FOURCC_NV12;
                    format.byte_order = 1; // VA_LSB_FIRST
                    format.bits_per_pixel = 12;
                    let mut img: VAImage = std::mem::zeroed();
                    let (aw, ah) = ((self.mb_width * 16) as c_int, (self.mb_height * 16) as c_int);
                    va.check("vaCreateImage", (va.create_image)(dpy, &mut format, aw, ah, &mut img))?;
                    self.upload = Some(img);
                }
                image = self.upload.unwrap();
            }
            let mut ptr = std::ptr::null_mut();
            va.check("vaMapBuffer (image)", (va.map_buffer)(dpy, image.buf, &mut ptr))?;
            let base = ptr as *mut u8;
            for y in 0..h {
                std::ptr::copy_nonoverlapping(self.nv12[y * w..].as_ptr(), base.add(image.offsets[0] as usize + y * image.pitches[0] as usize), w);
            }
            let uv = &self.nv12[w * h..];
            for y in 0..h / 2 {
                std::ptr::copy_nonoverlapping(uv[y * w..].as_ptr(), base.add(image.offsets[1] as usize + y * image.pitches[1] as usize), w);
            }
            va.check("vaUnmapBuffer (image)", (va.unmap_buffer)(dpy, image.buf))?;
            if derived {
                (va.destroy_image)(dpy, image.image_id);
            } else {
                let (aw, ah) = (self.mb_width * 16, self.mb_height * 16);
                va.check("vaPutImage", (va.put_image)(dpy, self.input, image.image_id, 0, 0, aw, ah, 0, 0, aw, ah))?;
            }
        }
        Ok(())
    }

    /// Creates a parameter buffer holding `value`.
    unsafe fn param<T>(&self, kind: VABufferType::Type, value: &mut T) -> Result<Buf> {
        let mut id = VA_INVALID_ID;
        let status = unsafe {
            (self.va.create_buffer)(self.dev.dpy, self.context, kind, size_of::<T>() as c_uint, 1, value as *mut T as *mut c_void, &mut id)
        };
        self.va.check("vaCreateBuffer", status)?;
        Ok(id)
    }

    /// Creates a misc parameter buffer: the type, then `value`.
    unsafe fn misc<T>(&self, kind: VAEncMiscParameterType::Type, value: &T) -> Result<Buf> {
        let (va, dpy) = (&self.va, self.dev.dpy);
        let header = size_of::<VAEncMiscParameterBuffer>();
        let mut id = VA_INVALID_ID;
        unsafe {
            va.check(
                "vaCreateBuffer (misc)",
                (va.create_buffer)(dpy, self.context, VABufferType::VAEncMiscParameterBufferType, (header + size_of::<T>()) as c_uint, 1, std::ptr::null_mut(), &mut id),
            )?;
            let mut ptr = std::ptr::null_mut();
            va.check("vaMapBuffer (misc)", (va.map_buffer)(dpy, id, &mut ptr))?;
            std::ptr::write_bytes(ptr as *mut u8, 0, header + size_of::<T>());
            (*(ptr as *mut VAEncMiscParameterBuffer)).type_ = kind;
            std::ptr::copy_nonoverlapping(value as *const T as *const u8, (ptr as *mut u8).add(header), size_of::<T>());
            va.check("vaUnmapBuffer (misc)", (va.unmap_buffer)(dpy, id))?;
        }
        Ok(id)
    }

    fn picture(surface: VASurfaceID, frame_num: u32, reference: bool) -> VAPictureH264 {
        VAPictureH264 {
            picture_id: surface,
            frame_idx: frame_num,
            flags: if reference { VA_PICTURE_H264_SHORT_TERM_REFERENCE } else { 0 },
            // pic_order_cnt_type 2: the order count is twice frame_num.
            TopFieldOrderCnt: (frame_num * 2) as i32,
            BottomFieldOrderCnt: (frame_num * 2) as i32,
            va_reserved: Default::default(),
        }
    }

    fn invalid_picture() -> VAPictureH264 {
        VAPictureH264 { picture_id: VA_INVALID_SURFACE, flags: VA_PICTURE_H264_INVALID, ..unsafe { std::mem::zeroed() } }
    }

    /// Builds this frame's parameter buffers.
    unsafe fn buffers(&mut self, idr: bool) -> Result<Vec<Buf>> {
        let gop = MAX_FPS * 10;
        let rate = bitrate(self.width, self.height);
        let cabac = self.dev.profile != VAProfile::VAProfileH264ConstrainedBaseline;
        let high = self.dev.profile == VAProfile::VAProfileH264High;
        let current = self.recon[self.next_recon];
        let reference = self.recon[1 - self.next_recon];
        let mut buffers = Vec::new();
        unsafe {
            if idr {
                let mut seq: VAEncSequenceParameterBufferH264 = std::mem::zeroed();
                seq.level_idc = self.level;
                seq.intra_period = gop;
                seq.intra_idr_period = gop;
                seq.ip_period = 1;
                seq.bits_per_second = rate;
                seq.max_num_ref_frames = 1;
                seq.picture_width_in_mbs = self.mb_width as u16;
                seq.picture_height_in_mbs = self.mb_height as u16;
                let f = &mut seq.seq_fields.bits;
                f.set_chroma_format_idc(1);
                f.set_frame_mbs_only_flag(1);
                f.set_direct_8x8_inference_flag(1);
                f.set_log2_max_frame_num_minus4(12); // frame_num up to 65535: never wraps within a GOP
                f.set_pic_order_cnt_type(2);
                let (crop_right, crop_bottom) = (self.mb_width * 16 - self.width, self.mb_height * 16 - self.height);
                if crop_right != 0 || crop_bottom != 0 {
                    seq.frame_cropping_flag = 1;
                    seq.frame_crop_right_offset = crop_right / 2; // in chroma samples (4:2:0)
                    seq.frame_crop_bottom_offset = crop_bottom / 2;
                }
                seq.vui_parameters_present_flag = 1;
                let v = &mut seq.vui_fields.bits;
                v.set_timing_info_present_flag(1);
                v.set_bitstream_restriction_flag(1); // no reordering: show each frame at once
                v.set_log2_max_mv_length_horizontal(15);
                v.set_log2_max_mv_length_vertical(15);
                v.set_motion_vectors_over_pic_boundaries_flag(1);
                seq.num_units_in_tick = 1;
                seq.time_scale = MAX_FPS * 2;
                buffers.push(self.param(VABufferType::VAEncSequenceParameterBufferType, &mut seq)?);

                if self.dev.rate_control != 0 {
                    let mut rc: VAEncMiscParameterRateControl = std::mem::zeroed();
                    rc.bits_per_second = rate;
                    rc.target_percentage = 100;
                    rc.window_size = 1000;
                    rc.initial_qp = 26;
                    buffers.push(self.misc(VAEncMiscParameterType::VAEncMiscParameterTypeRateControl, &rc)?);
                    let hrd = VAEncMiscParameterHRD { initial_buffer_fullness: rate / 4, buffer_size: rate / 2, va_reserved: Default::default() };
                    buffers.push(self.misc(VAEncMiscParameterType::VAEncMiscParameterTypeHRD, &hrd)?);
                }
                let mut fr: VAEncMiscParameterFrameRate = std::mem::zeroed();
                fr.framerate = MAX_FPS;
                buffers.push(self.misc(VAEncMiscParameterType::VAEncMiscParameterTypeFrameRate, &fr)?);
            }

            let mut pic: VAEncPictureParameterBufferH264 = std::mem::zeroed();
            pic.CurrPic = Self::picture(current, self.frame_num, false);
            for r in pic.ReferenceFrames.iter_mut() {
                *r = Self::invalid_picture();
            }
            if !idr {
                pic.ReferenceFrames[0] = Self::picture(reference, self.frame_num - 1, true);
            }
            pic.coded_buf = self.coded;
            pic.frame_num = self.frame_num as u16;
            pic.pic_init_qp = 26;
            let p = &mut pic.pic_fields.bits;
            p.set_idr_pic_flag(idr as u32);
            p.set_reference_pic_flag(1);
            p.set_entropy_coding_mode_flag(cabac as u32);
            p.set_transform_8x8_mode_flag(high as u32);
            p.set_deblocking_filter_control_present_flag(1);
            buffers.push(self.param(VABufferType::VAEncPictureParameterBufferType, &mut pic)?);

            let mut slice: VAEncSliceParameterBufferH264 = std::mem::zeroed();
            slice.num_macroblocks = self.mb_width * self.mb_height;
            slice.macroblock_info = VA_INVALID_ID;
            slice.slice_type = if idr { 2 } else { 0 }; // I or P
            slice.idr_pic_id = self.idr_count;
            for r in slice.RefPicList0.iter_mut().chain(slice.RefPicList1.iter_mut()) {
                *r = Self::invalid_picture();
            }
            if !idr {
                slice.RefPicList0[0] = Self::picture(reference, self.frame_num - 1, true);
            }
            buffers.push(self.param(VABufferType::VAEncSliceParameterBufferType, &mut slice)?);
        }
        Ok(buffers)
    }
}

impl VideoEncoder for VaEncoder {
    fn encode(&mut self, frame: &Capturer, timestamp_us: u64) -> Result<(Vec<u8>, bool)> {
        self.encode_frame(frame, timestamp_us).inspect_err(|e| {
            let _ = FAILED.set(format!("VA-API encoding failed on {}: {e:#}", self.dev.vendor));
        })
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
    fn label(&self) -> &str {
        "VA-API"
    }
}

impl VaEncoder {
    fn encode_frame(&mut self, frame: &Capturer, _timestamp_us: u64) -> Result<(Vec<u8>, bool)> {
        let mut nv12 = std::mem::take(&mut self.nv12);
        self.conv.nv12_strided(frame.pixels(), frame.stride(), &mut nv12);
        self.nv12 = nv12;
        unsafe {
            self.upload()?;
            let idr = std::mem::take(&mut self.force_key) || self.frame_num >= MAX_FPS * 10;
            if idr {
                self.frame_num = 0;
                self.idr_count = self.idr_count.wrapping_add(1);
            }
            let mut buffers = self.buffers(idr)?;
            let (va, dpy, ctx) = (&self.va, self.dev.dpy, self.context);
            let result = (|| -> Result<Vec<u8>> {
                va.check("vaBeginPicture", (va.begin_picture)(dpy, ctx, self.input))?;
                va.check("vaRenderPicture", (va.render_picture)(dpy, ctx, buffers.as_mut_ptr(), buffers.len() as c_int))?;
                va.check("vaEndPicture", (va.end_picture)(dpy, ctx))?;
                va.check("vaSyncSurface", (va.sync_surface)(dpy, self.input))?;
                let mut segment = std::ptr::null_mut();
                va.check("vaMapBuffer (coded)", (va.map_buffer)(dpy, self.coded, &mut segment))?;
                let mut data = Vec::new();
                while !segment.is_null() {
                    let s = &*(segment as *const VACodedBufferSegment);
                    data.extend_from_slice(std::slice::from_raw_parts(s.buf as *const u8, s.size as usize));
                    segment = s.next;
                }
                va.check("vaUnmapBuffer (coded)", (va.unmap_buffer)(dpy, self.coded))?;
                Ok(data)
            })();
            for b in buffers {
                (va.destroy_buffer)(dpy, b);
            }
            let data = result?;
            self.frame_num += 1;
            self.next_recon = 1 - self.next_recon;
            if idr && !self.checked_headers {
                self.checked_headers = true;
                if !has_sps(&data) {
                    bail!("the VA-API driver ({}) doesn't write H.264 sequence headers; it isn't supported yet", self.dev.vendor);
                }
            }
            Ok((data, idr))
        }
    }
}

impl Drop for VaEncoder {
    fn drop(&mut self) {
        let (va, dpy) = (&self.va, self.dev.dpy);
        unsafe {
            if let Some(img) = self.upload {
                (va.destroy_image)(dpy, img.image_id);
            }
            if self.coded != VA_INVALID_ID {
                (va.destroy_buffer)(dpy, self.coded);
            }
            if self.context != VA_INVALID_ID {
                (va.destroy_context)(dpy, self.context);
            }
            let mut surfaces = [self.input, self.recon[0], self.recon[1]];
            if surfaces[0] != VA_INVALID_SURFACE {
                (va.destroy_surfaces)(dpy, surfaces.as_mut_ptr(), 3);
            }
            if self.config != VA_INVALID_ID {
                (va.destroy_config)(dpy, self.config);
            }
            (va.terminate)(dpy);
        }
        let _ = &self.dev.fd; // closed after vaTerminate (field drop)
    }
}

/// Whether Annex B data contains a sequence parameter set (NAL type 7).
fn has_sps(data: &[u8]) -> bool {
    data.windows(4).any(|w| w[0] == 0 && w[1] == 0 && w[2] == 1 && w[3] & 0x1f == 7)
}
