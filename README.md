# Remote Desktop View

A Visual Studio Code extension that shows your remote Windows, Linux or Mac desktop in an editor tab — with sound and a shared clipboard — and lets you control it, over the connection VS Code already has (Remote Tunnels, Remote-SSH or WSL).

No ports to open, no VPN, nothing to install on your own machine, no setup on the remote.

> **Preview.** Windows 10/11 remotes are supported, and Linux remotes running X11 with an NVIDIA GPU, or (experimental) an Intel or AMD GPU (see [Requirements](#requirements)). macOS remotes show the desktop and take mouse and keyboard input; sound and clipboard sync on macOS are in progress.

![Remote Desktop View screenshot](resources/remotedesktop.png)

## Features

### The Desktop in an Editor Tab

Open the remote desktop next to your code, split it, or drag it into its own window. Mouse, keyboard and scroll wheel work as you'd expect; the view scales to fit the tab or shows 1:1.

### Uses Your Existing Connection

The screen travels through VS Code's own connection to the remote, so whatever already works for your code — Remote Tunnels across the internet, Remote-SSH, WSL — works for the desktop too. Nothing listens on the network: the extension talks to its desktop server through a pipe.

### Video, Not Screenshots

The remote desktop is streamed as H.264 video, encoded by the remote machine's GPU and decoded by VS Code on your machine. It only sends frames when something changes, and uses a fraction of the bandwidth of VNC: a few hundred KB/s for everyday use at up to 60 fps.

- **Windows:** the GPU's encoder (Intel Quick Sync, NVIDIA NVENC, AMD — whatever Windows provides), fed straight from video memory. Machines without a hardware encoder use the H.264 encoder built into Windows.
- **macOS:** VideoToolbox, the Mac's hardware encoder, fed straight from the screen capture's GPU buffers.
- **Linux:** NVIDIA's NVENC, or VA-API on Intel and AMD GPUs (experimental), at up to 4K. Machines without either can use the OpenH264 software encoder, which the extension offers to download from Cisco.

### Sound

Click **Sound** to hear the remote machine's audio (Opus, low latency).

### Clipboard Both Ways

Text copied on the remote desktop lands on your clipboard, and your clipboard is sent to the remote when you paste there or switch to the tab.

### Mac Keyboards

On a Mac, ⌘ acts as Ctrl on a Windows or Linux remote by default, so ⌘C, ⌘V and ⌘Z do what you expect. Switch it to the Windows key with the **⌘ = Ctrl** button or the `vncDesktopView.commandKey` setting. On a Mac remote, ⌘ is simply ⌘.

### Recovers by Itself

Resolution changes, the Windows lock screen, UAC prompts and GPU resets are handled without closing the tab: the picture pauses with an explanation and comes back on its own.

## Requirements

- VS Code 1.90 or later on your machine, connected to the remote machine with **Remote Tunnels**, **Remote-SSH** or **WSL**
- A remote machine with someone signed in to its desktop:

### Windows

- Windows 10 or 11 (x64)

### Linux

| | Requirement |
|---|---|
| System | x64, glibc 2.34 or newer (Ubuntu 22.04+, Debian 12+, Linux Mint 21+, Fedora 35+, RHEL / Rocky 9+) |
| Desktop session | **X11 (Xorg).** Any desktop environment: Cinnamon, Xfce, MATE, KDE Plasma on X11, GNOME on Xorg, … Wayland sessions aren't supported yet; on GNOME or KDE, pick the X11 / Xorg session on the login screen. |
| GPU | **NVIDIA** GPU with NVIDIA's driver, version 530 or newer (NVENC), or *(experimental, not yet tested on real hardware)* an **Intel or AMD** GPU whose VA-API driver can encode H.264: Intel's `intel-media-va-driver` (iHD) or `i965-va-driver`, or Mesa's `radeonsi` for AMD. Some distributions (e.g. Fedora, openSUSE) build Mesa without H.264 encoding; there you need their "freeworld" Mesa VA drivers. The open-source nouveau driver for NVIDIA isn't supported. Without a usable GPU encoder, the **OpenH264** software encoder can be used instead (see below). |
| Screen size | At most 4096 × 4096 pixels — all monitors together, since X11 treats them as one screen |
| Sound (optional) | PulseAudio, or PipeWire with its PulseAudio server (most desktops). Without it, everything but sound works. |

Nothing needs to be installed on Linux: the desktop server only depends on the C library, and uses the GPU driver and the sound server that are already there. It finds the logged-in desktop session itself. With both an NVIDIA and an Intel / AMD GPU, NVENC is used.

#### Software encoder (OpenH264)

On a Linux machine without a usable GPU encoder (for example a virtual machine), the tab asks whether to use **OpenH264** instead: Cisco's H.264 encoder, which the extension downloads once from Cisco to the remote machine (about 0.6 MB, checked against a pinned SHA-256; unpacking it needs the `bzip2` program). It works at any resolution but uses much more CPU than a GPU: a 4K screen runs at around 20 fps on a 4-core desktop CPU. Turn it on or off at any time with the `vncDesktopView.openH264` setting; `always` uses it even on machines with a GPU encoder (to compare, or if a GPU driver misbehaves).

*OpenH264 Video Codec provided by Cisco Systems, Inc.*

### macOS

- macOS 12.3 or later (Apple Silicon or Intel)
- Two permissions, granted once in **System Settings → Privacy & Security** on the Mac:
  - **Screen & System Audio Recording**, to see the screen
  - **Accessibility**, to control it

  Which program needs them depends on how VS Code connects. With **Remote Tunnels**, macOS asks for VS Code's `code` command when the desktop is first opened. With **Remote-SSH**, add `/usr/libexec/sshd-keygen-wrapper` to both lists yourself (click **+**, press **⌘⇧G** and type the path). Someone has to be at the Mac to do this.
- macOS 15 may now and then ask on the Mac's screen whether to keep allowing screen recording.
- Large screens are scaled down to fit H.264 at 60 fps (e.g. a 3840×2486 Retina mode is sent as 3670×2376); a 4K display is sent as is.

## Getting Started

1. Connect VS Code to the remote machine and install the extension there (VS Code offers to install it on the remote side)
2. Run **Remote Desktop View: Open Desktop** from the Command Palette

## Settings

| Setting | Default | Description |
|---------|---------|-------------|
| `vncDesktopView.commandKey` | `ctrl` | Mac only: what ⌘ sends to the remote desktop (`ctrl`, or `windows` = the Windows / Super key) |
| `vncDesktopView.openH264` | `ask` | Linux remotes without a usable GPU encoder: use Cisco's OpenH264 software encoder (`ask`, `on`, `off`; `always` uses it even when there is a GPU encoder). *OpenH264 Video Codec provided by Cisco Systems, Inc.* |

## Security & Privacy

- **No telemetry.** The extension sends nothing anywhere. Screen, sound, keyboard, mouse and clipboard data only travel over your existing VS Code remote connection.
- **Nothing exposed.** The desktop server runs as your user, only while a desktop tab is open, and talks to the extension over its standard input and output. It opens no network ports.
- **No changes to the remote machine.** Nothing is installed or configured; the desktop server ships inside the extension.
- **Downloads.** Only if you choose the OpenH264 software encoder: its library is downloaded once from Cisco (`ciscobinary.openh264.org`), checked against a pinned SHA-256, and kept in the extension's storage folder on the remote machine. Nothing else is ever downloaded.
- **Clipboard.** Text is synced only while a desktop tab is open, up to 1 MB.
- **Sound.** On Linux the remote's sound is only recorded while **Sound** is on.

## Known Issues

- **Windows: lock screen and UAC prompts** can't be shown or controlled yet (Windows reserves them for system processes); the tab shows a notice until the desktop returns
- **Windows: administrator windows** don't receive mouse and keyboard input while they have focus (Windows blocks input from ordinary programs into elevated ones)
- **Linux: Wayland and screens over 4096 pixels** aren't supported yet (see [Requirements](#linux))
- **Linux: Intel / AMD GPUs (VA-API)** are experimental and haven't been tested on real hardware yet. If the tab shows an encoder error, please [report it](https://github.com/rderr/vnc_desktop_view/issues) with the **Remote Desktop View** output
- **Keyboard shortcuts** that VS Code handles itself (such as ⌘W / Ctrl+W) don't reach the remote
- **Viewing the computer you're using:** in a plain local VS Code window the tab is view-only, because mouse and keyboard input would land back in the tab and repeat endlessly. Over Remote-SSH to your own machine the extension can't tell, so don't type or click in that tab.
- **After a reboot**, the remote is only reachable once someone has logged in (VS Code's tunnel starts at login)

## Development

### Prerequisites

- Node.js 20+
- VS Code 1.90+
- For the desktop server: Rust, and CMake (used to build libopus). On Windows, the Visual Studio C++ build tools (which include CMake); on Linux, a C compiler and CMake.

### Build

```bash
npm install
npm run build                                       # extension and viewer
cargo build --release --manifest-path server/Cargo.toml   # desktop server
```

### Run

Press **F5** in VS Code to launch the Extension Development Host. Over Remote Tunnels the debugger may fail to attach; use **Run Without Debugging** (Ctrl+F5) instead. A development checkout runs the server from `server/target/release`.

### Package

```bash
npm run package
```

Builds the desktop server and the package for the platform it runs on:

- On Windows: `…-win32-x64.vsix`, plus the universal `.vsix` for platforms without a server
- On Linux: `…-linux-x64.vsix`

`npm run publish` builds and publishes them; run it on both systems for a release.

### Project Structure

```
src/
├── extension.ts              # Commands
├── videoSession.ts           # One desktop tab: runs the server, flow control, clipboard
├── videoProtocol.ts          # Messages between the tab and the extension
├── openh264.ts               # Downloads Cisco's OpenH264 (Linux software encoder)
└── webview/
    ├── video.ts              # Viewer: WebCodecs H.264, input, toolbar
    ├── audio.ts              # Opus playback (WebAssembly decoder)
    └── scancodes.ts          # Keyboard codes -> Windows scan codes
server/src/                   # Desktop server (Rust)
├── main.rs                   # Capture loop and stdin/stdout protocol
├── encoder.rs                # Encoder interface and selection
├── convert.rs                # CPU color conversion (BGRA -> NV12)
├── win/                      # Windows
│   ├── capture.rs            # DXGI Desktop Duplication
│   ├── gpu.rs                # BGRA -> NV12 on the GPU
│   ├── mf.rs                 # Media Foundation H.264 encoders
│   ├── audio.rs              # WASAPI loopback -> Opus
│   ├── input.rs              # Mouse and keyboard (SendInput)
│   └── clipboard.rs          # Clipboard sync
├── mac/                      # macOS
│   ├── capture.rs            # ScreenCaptureKit
│   ├── vt.rs                 # VideoToolbox H.264
│   └── input.rs              # Mouse and keyboard (Core Graphics events)
└── linux/                    # Linux (X11)
    ├── display.rs            # Finds the logged-in desktop session
    ├── capture.rs            # MIT-SHM screen grabs, driven by DAMAGE
    ├── nvenc.rs              # NVIDIA NVENC H.264 (nvenc_sys.rs: API bindings)
    ├── vaapi.rs              # Intel / AMD VA-API H.264 (va_sys.rs: API bindings)
    ├── audio.rs              # PulseAudio monitor -> Opus
    ├── input.rs              # Mouse and keyboard (XTEST)
    └── clipboard.rs          # CLIPBOARD selection sync
scripts/
├── package.mjs               # Builds the server and this platform's package(s)
├── build-notices.mjs         # Generates ThirdPartyNotices.txt
├── server-notices.mjs        # Licenses of the server's Rust crates
└── openh264-binary-license.txt # Cisco's OpenH264 binary license (copied into the notices)
```

## License

See [LICENSE](LICENSE) for details. Bundled third-party code is listed in [ThirdPartyNotices.txt](ThirdPartyNotices.txt).
