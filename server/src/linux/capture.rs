//! X11 screen capture: the root window is copied into shared memory (MIT-SHM)
//! when the DAMAGE extension reports that something on screen changed, so an
//! idle desktop costs (almost) nothing. RandR tells us about resolution changes.

use anyhow::{Context, Result, bail};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{ImageFormat, Window};
use x11rb::rust_connection::RustConnection;

pub struct Capturer {
    conn: RustConnection,
    root: Window,
    seg: shm::Seg,
    damage: damage::Damage,
    /// The shared memory the X server writes the screen into: BGRX, `src_width * 4` bytes per row.
    mem: *mut u8,
    len: usize,
    /// Grab even if no damage was reported (the first frame).
    pending: bool,
    /// Size of the screen.
    pub src_width: u32,
    pub src_height: u32,
    /// Size of the encoded frames: the screen rounded down to even
    /// dimensions, since H.264 4:2:0 needs them.
    pub width: u32,
    pub height: u32,
}

pub enum Frame {
    /// The screen changed; `pixels()` holds the new image.
    Changed,
    /// The screen's size changed; create a new Capturer.
    Lost(String),
    /// Nothing changed on screen within the timeout.
    Unchanged,
}

impl Capturer {
    pub fn new() -> Result<Self> {
        let (conn, screen_num) = RustConnection::connect(None).context("connecting to the X server")?;
        let screen = &conn.setup().roots[screen_num];
        let (root, src_width, src_height) = (screen.root, screen.width_in_pixels as u32, screen.height_in_pixels as u32);
        let depth = screen.root_depth;
        let bpp = conn.setup().pixmap_formats.iter().find(|f| f.depth == depth).map(|f| f.bits_per_pixel);
        if !(depth == 24 || depth == 32) || bpp != Some(32) {
            bail!("unsupported screen format: depth {depth}, {bpp:?} bits per pixel (need 24/32-bit color)");
        }
        for (ext, name) in [(shm::X11_EXTENSION_NAME, "MIT-SHM"), (damage::X11_EXTENSION_NAME, "DAMAGE"), (randr::X11_EXTENSION_NAME, "RANDR")] {
            if conn.extension_information(ext)?.is_none() {
                bail!("the X server lacks the {name} extension");
            }
        }
        let v = conn.shm_query_version()?.reply()?;
        if (v.major_version, v.minor_version) < (1, 2) {
            bail!("MIT-SHM {}.{} is too old (need 1.2)", v.major_version, v.minor_version);
        }
        conn.damage_query_version(1, 1)?.reply()?;
        conn.randr_query_version(1, 2)?.reply()?;

        // Shared memory as a memfd, handed to the X server.
        let len = (src_width * src_height * 4) as usize;
        let (mem, fd) = unsafe {
            let fd = libc::memfd_create(c"desktop-server".as_ptr(), libc::MFD_CLOEXEC);
            if fd < 0 {
                bail!("memfd_create: {}", std::io::Error::last_os_error());
            }
            let fd = OwnedFd::from_raw_fd(fd);
            if libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) != 0 {
                bail!("ftruncate: {}", std::io::Error::last_os_error());
            }
            let mem = libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd.as_raw_fd(), 0);
            if mem == libc::MAP_FAILED {
                bail!("mmap: {}", std::io::Error::last_os_error());
            }
            (mem as *mut u8, fd)
        };
        let seg = conn.generate_id()?;
        conn.shm_attach_fd(seg, fd, false)?.check().context("MIT-SHM attach")?;

        let damage = conn.generate_id()?;
        conn.damage_create(damage, root, damage::ReportLevel::NON_EMPTY)?.check().context("DAMAGE create")?;
        conn.randr_select_input(root, randr::NotifyMask::SCREEN_CHANGE)?.check()?;

        Ok(Self {
            conn,
            root,
            seg,
            damage,
            mem,
            len,
            pending: true,
            src_width,
            src_height,
            width: src_width & !1,
            height: src_height & !1,
        })
    }

    /// Waits up to `timeout_ms` for the screen to change. On a change, the new
    /// image is in `pixels()`.
    pub fn next(&mut self, timeout_ms: u32) -> Result<Frame> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
        loop {
            while let Some(event) = self.conn.poll_for_event()? {
                match event {
                    Event::DamageNotify(_) => self.pending = true,
                    Event::RandrScreenChangeNotify(e) => {
                        if (e.width as u32, e.height as u32) != (self.src_width, self.src_height) {
                            return Ok(Frame::Lost(format!("screen is now {}x{}", e.width, e.height)));
                        }
                    }
                    _ => {}
                }
            }
            if self.pending {
                break;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(Frame::Unchanged);
            }
            let mut pfd = libc::pollfd { fd: self.conn.stream().as_raw_fd(), events: libc::POLLIN, revents: 0 };
            unsafe { libc::poll(&mut pfd, 1, left.as_millis().max(1) as i32) };
        }
        self.pending = false;
        // Clear the damage first, so changes made during the grab trigger another one.
        self.conn.damage_subtract(self.damage, x11rb::NONE, x11rb::NONE)?;
        let t = Instant::now();
        self.conn
            .shm_get_image(self.root, 0, 0, self.src_width as u16, self.src_height as u16, !0, ImageFormat::Z_PIXMAP.into(), self.seg, 0)?
            .reply()
            .context("MIT-SHM GetImage")?;
        if std::env::var_os("DS_DEBUG").is_some() {
            eprintln!("    x11: grab {:.1} ms", t.elapsed().as_secs_f64() * 1e3);
        }
        Ok(Frame::Changed)
    }

    /// The latest image: BGRX, `stride()` bytes per row, `src_height` rows.
    pub fn pixels(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.mem, self.len) }
    }

    pub fn stride(&self) -> usize {
        self.src_width as usize * 4
    }

    /// Copies the latest image as tightly packed BGRA, `width * height * 4` bytes.
    #[allow(dead_code)] // used by the OpenH264 test encoder
    pub fn read_bgra(&self, buf: &mut Vec<u8>) -> Result<()> {
        let row = self.width as usize * 4;
        buf.resize(row * self.height as usize, 0);
        for (y, out) in buf.chunks_exact_mut(row).enumerate() {
            out.copy_from_slice(&self.pixels()[y * self.stride()..][..row]);
        }
        Ok(())
    }
}

impl Drop for Capturer {
    fn drop(&mut self) {
        let _ = self.conn.damage_destroy(self.damage);
        let _ = self.conn.shm_detach(self.seg);
        let _ = self.conn.flush();
        unsafe { libc::munmap(self.mem as *mut libc::c_void, self.len) };
    }
}

/// X11 has no secure desktop: the lock screen is an ordinary window.
pub fn is_secure_desktop(_err: &anyhow::Error) -> bool {
    false
}
