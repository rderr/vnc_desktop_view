# Remote Desktop View

A Visual Studio Code extension that shows your remote Windows machine's desktop in an editor tab — with sound and a shared clipboard — and lets you control it, over the connection VS Code already has (Remote Tunnels, Remote-SSH or WSL).

No ports to open, no VPN, nothing to install on your own machine, no setup on the remote.

> **Preview.** Windows 10/11 remotes are supported. macOS and Linux remotes are planned.

![Remote Desktop View screenshot](resources/remotedesktop.png)

## Features

### The Desktop in an Editor Tab

Open the remote desktop next to your code, split it, or drag it into its own window. Mouse, keyboard and scroll wheel work as you'd expect; the view scales to fit the tab or shows 1:1.

### Uses Your Existing Connection

The screen travels through VS Code's own connection to the remote, so whatever already works for your code — Remote Tunnels across the internet, Remote-SSH, WSL — works for the desktop too. Nothing listens on the network: the extension talks to its desktop server through a pipe.

### Video, Not Screenshots

The remote desktop is streamed as H.264 video, encoded by the remote machine's GPU (Intel Quick Sync, NVIDIA NVENC, AMD — whatever Windows provides) straight from video memory, and decoded by VS Code on your machine. It only sends frames when something changes, and uses a fraction of the bandwidth of VNC: a few hundred KB/s for everyday use at up to 60 fps. Machines without a hardware encoder use the H.264 encoder built into Windows.

### Sound

Click **Sound** to hear the remote machine's audio (Opus, low latency).

### Clipboard Both Ways

Text copied on the remote desktop lands on your clipboard, and your clipboard is sent to the remote when you paste there or switch to the tab.

### Mac Keyboards

On a Mac, ⌘ acts as Ctrl on the remote by default, so ⌘C, ⌘V and ⌘Z do what you expect. Switch it to the Windows key with the **⌘ = Ctrl** button or the `vncDesktopView.commandKey` setting.

### Recovers by Itself

Resolution changes, the lock screen, UAC prompts and GPU resets are handled without closing the tab: the picture pauses with an explanation and comes back on its own.

## Requirements

- VS Code 1.90 or later on your machine, connected to the remote machine with **Remote Tunnels**, **Remote-SSH** or **WSL**
- A **Windows 10 or 11** remote, with someone signed in to its desktop

## Getting Started

1. Connect VS Code to the remote machine and install the extension there (VS Code offers to install it on the remote side)
2. Run **Remote Desktop View: Open Desktop** from the Command Palette

## Settings

| Setting | Default | Description |
|---------|---------|-------------|
| `vncDesktopView.commandKey` | `ctrl` | Mac only: what ⌘ sends to the remote desktop (`ctrl` or `windows`) |

## Security & Privacy

- **No telemetry.** The extension sends nothing anywhere. Screen, sound, keyboard, mouse and clipboard data only travel over your existing VS Code remote connection.
- **Nothing exposed.** The desktop server runs as your user, only while a desktop tab is open, and talks to the extension over its standard input and output. It opens no network ports.
- **No changes to the remote machine.** Nothing is installed or configured; the desktop server ships inside the extension.
- **Clipboard.** Text is synced only while a desktop tab is open, up to 1 MB.

## Known Issues

- **Lock screen and UAC prompts** can't be shown or controlled yet (Windows reserves them for system processes); the tab shows a notice until the desktop returns
- **Administrator windows** don't receive mouse and keyboard input while they have focus (Windows blocks input from ordinary programs into elevated ones)
- **Keyboard shortcuts** that VS Code handles itself (such as ⌘W / Ctrl+W) don't reach the remote
- **After a reboot**, the remote is only reachable once someone has logged in (VS Code's tunnel starts at login)

## Development

### Prerequisites

- Node.js 20+
- VS Code 1.90+
- For the desktop server (Windows): Rust, and the Visual Studio C++ build tools (which include CMake, used to build libopus)

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

On Windows: builds the desktop server and two packages, `…-win32-x64.vsix` (with the server, for Windows remotes) and a universal `.vsix` for other platforms. `npm run publish` builds and publishes both.

### Project Structure

```
src/
├── extension.ts              # Commands
├── videoSession.ts           # One desktop tab: runs the server, flow control, clipboard
├── videoProtocol.ts          # Messages between the tab and the extension
└── webview/
    ├── video.ts              # Viewer: WebCodecs H.264, input, toolbar
    ├── audio.ts              # Opus playback (WebAssembly decoder)
    └── scancodes.ts          # Keyboard codes -> Windows scan codes
server/src/                   # Desktop server (Rust, Windows)
├── main.rs                   # Capture loop and stdin/stdout protocol
├── capture.rs                # DXGI Desktop Duplication
├── gpu.rs                    # BGRA -> NV12 on the GPU
├── mf.rs                     # Media Foundation H.264 encoders
├── encoder.rs                # Encoder selection, CPU color conversion fallback
├── audio.rs                  # WASAPI loopback -> Opus
├── input.rs                  # Mouse and keyboard (SendInput)
└── clipboard.rs              # Clipboard sync
scripts/
├── package.mjs               # Builds the server and both packages
├── build-notices.mjs         # Generates ThirdPartyNotices.txt
└── server-notices.mjs        # Licenses of the server's Rust crates
```

## License

See [LICENSE](LICENSE) for details. Bundled third-party code is listed in [ThirdPartyNotices.txt](ThirdPartyNotices.txt).
