//! Linux on X11: MIT-SHM screen grabs driven by the DAMAGE extension, H.264 on
//! NVIDIA GPUs with NVENC, PulseAudio monitor capture, XTEST input and the
//! X11 CLIPBOARD selection.

pub mod audio;
pub mod capture;
pub mod clipboard;
mod display;
pub mod input;
pub mod nvenc;
mod nvenc_sys;

pub use display::find_display;
