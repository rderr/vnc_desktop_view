# VNC Desktop View

A Visual Studio Code extension that shows your remote machine's desktop in an editor tab — and lets you control it — over the connection VS Code already has (Remote Tunnels, Remote-SSH or WSL).

No ports to open, no VPN, nothing to install on your own machine. On a Windows remote it works with no setup at all.

> **Preview.** Windows remotes are fully supported; macOS and Linux remotes work with a VNC server you set up yourself.

![VNC Desktop View screenshot](resources/screenshot.png)

## Features

### The Desktop in an Editor Tab

Open the remote desktop next to your code, split it, or drag it into its own window. Mouse, keyboard and scaling work as you'd expect; the view scales to fit the tab or shows 1:1.

### Uses Your Existing Connection

The screen travels through VS Code's own connection to the remote, so whatever already works for your code — Remote Tunnels across the internet, Remote-SSH, WSL — works for the desktop too. The VNC server only accepts connections from the remote machine itself; nothing is exposed to the network.

### Zero Setup on Windows

In **application mode** (the default on Windows), the extension downloads TightVNC once, then runs it only while a desktop tab is open — as an ordinary program, without administrator rights, with a new random port and password each time. Close the tab and it's gone.

### Optional Full Access

Application mode can't see or control UAC prompts, the lock screen or administrator windows (Windows doesn't allow that for ordinary programs). **Full-access mode** installs TightVNC as a Windows service, once, so all of those work too, including Ctrl+Alt+Del.

### Smooth Over Slow Links

VNC normally waits a full round trip for every screen update, which makes it sluggish over tunnels. The extension requests updates from right next to the server and keeps a bounded amount of data in flight, giving around 45 fps for everyday desktop use over an internet tunnel instead of single digits.

### Tells You When Input Is Blocked

If an administrator window, a UAC prompt or the lock screen is in front in application mode, a banner explains why clicks and keys aren't getting through, and offers full-access mode.

### Clipboard

Text copied on the remote desktop lands on your clipboard automatically. **Paste to remote** sends your clipboard the other way.

## Requirements

- VS Code 1.90 or later, connected to the remote machine with **Remote Tunnels**, **Remote-SSH** or **WSL**
- **Windows remote** (Windows 10/11): internet access to `tightvnc.com` the first time; administrator rights only for full-access mode
- **macOS or Linux remote**: a VNC server you start yourself (see [Getting Started](#getting-started))

## Getting Started

### Windows remote

1. Connect VS Code to the remote machine and install the extension there (VS Code offers to install it on the remote side)
2. Run **VNC Desktop View: Open Desktop** from the Command Palette
3. The first time, TightVNC is downloaded (~2.5 MB); then the desktop opens in a tab

For UAC prompts, the lock screen and administrator windows, run **VNC Desktop View: Install Full-Access Mode**. This needs administrator rights once: either approve the UAC prompt on the remote machine's own screen, or run VS Code's terminal as an administrator — for example over SSH as an admin user, which needs no UAC prompt.

### macOS remote

Turn on **System Settings → General → Sharing → Screen Sharing**, and under its ⓘ options enable *VNC viewers may control screen with password*. Then run **Open Desktop**; you'll be asked for that password once.

### Linux remote (X11)

Start a VNC server that only listens on localhost, for example:

```bash
x11vnc -storepasswd
x11vnc -localhost -display :0 -rfbauth ~/.vnc/passwd -forever
```

Then run **Open Desktop**.

## Modes

The `vncDesktopView.server` setting chooses the VNC server. The toolbar badge and tab title show which one a tab uses.

| Mode | Admin rights | UAC prompts, lock screen, admin windows | What it is |
|---|---|---|---|
| **Application mode** (`app`) | No | No | TightVNC run as an ordinary program while a tab is open (Windows) |
| **Full access** (`service`) | Once | Yes | TightVNC installed as a Windows service, localhost-only |
| **Custom** (`custom`) | — | Depends on the server | Any VNC server at `vncDesktopView.host` / `vncDesktopView.port` |

`auto` (the default) uses full access when it's installed, otherwise application mode on Windows and custom elsewhere.

## Commands

| Command | Description |
|---------|-------------|
| **Open Desktop** | Show the remote desktop in a new tab |
| **Install Full-Access Mode** | Install the TightVNC service on a Windows remote (UAC prompts, lock screen) |
| **Remove Full-Access Mode** | Remove it again (uninstalls TightVNC only if the extension installed it) |
| **Open Test Desktop** | Built-in fake desktop for checking speed and input |
| **Forget Saved VNC Password** | Remove the stored password for custom / full-access servers |

## Settings

| Setting | Default | Description |
|---------|---------|-------------|
| `vncDesktopView.server` | `auto` | Which VNC server to use (see [Modes](#modes)) |
| `vncDesktopView.host` | `127.0.0.1` | Custom mode: server address, as seen from the remote machine |
| `vncDesktopView.port` | `5900` | Custom mode: server port |
| `vncDesktopView.scaleToFit` | `true` | Scale the desktop to fit the tab |

## Security & Privacy

- **No telemetry.** The extension sends nothing anywhere. Screen, keyboard and mouse data only travel over your existing VS Code remote connection.
- **Localhost only.** Every VNC server the extension sets up accepts connections only from the remote machine itself, and full-access mode disables TightVNC's inbound firewall rule. (If TightVNC was already installed and you choose to keep its settings, they're left as they were.)
- **Downloads.** On Windows, the extension downloads the official TightVNC 2.8.89 installer from `https://www.tightvnc.com` and checks its SHA-256 before using it. In application mode it is only unpacked (not installed) into the extension's storage folder.
- **Changes to the remote machine.**
  - Application mode writes TightVNC settings to `HKCU\Software\TightVNC\Server` while a tab is open, and puts back any settings you had there afterwards.
  - Full-access mode installs the TightVNC service and writes `%ProgramData%\vnc-desktop-view\service.json`.
- **Passwords.** Application mode uses a new random password for every session and never stores it. Full-access and custom passwords are kept in VS Code's secret storage. VNC passwords are limited to 8 characters, which is why the servers only accept local connections.
- **Brute-force lockout.** After a failed login TightVNC briefly refuses all connections from the same machine, so a wrong saved password causes a short lockout.

## Known Issues

- **Mac keyboards:** ⌘ is sent as the Windows key rather than Ctrl, and VS Code handles some shortcuts (such as ⌘W) before the desktop sees them
- **Clipboard:** local → remote needs the **Paste to remote** button
- **macOS and Linux remotes:** server setup isn't automated yet
- **After a reboot**, the remote is only reachable once someone has logged in (VS Code's tunnel starts at login)

## Uninstalling

Uninstalling the extension removes the application-mode files and settings automatically. **Remove full-access mode first** (run **Remove Full-Access Mode**), since the extension can't remove a Windows service without administrator rights.

## Development

### Prerequisites

- Node.js 20+
- VS Code 1.90+

### Build

```bash
npm install
npm run build
```

### Test

```bash
npm test
```

On Windows, `test/app-mode-test.ts` exercises application mode for real (download, start, login, settings restore) and `test/browser-harness.ts` drives the viewer in headless Edge/Chrome.

### Watch

```bash
npm run watch
```

### Run

Press **F5** in VS Code to launch the Extension Development Host. Over Remote Tunnels the debugger may fail to attach; use **Run Without Debugging** (Ctrl+F5) instead.

### Package

```bash
npm run package
```

### Project Structure

```
src/
├── extension.ts              # Commands, mode selection
├── session.ts                # One desktop tab: webview panel, credentials, clipboard
├── relay.ts                  # TCP <-> webview message pipe, flow control, update streaming
├── protocol.ts               # Messages between the tab and the extension
├── testServer.ts             # Built-in fake VNC server
├── setup.ts                  # macOS / Linux setup instructions
├── webview/
│   └── main.ts               # Viewer: noVNC, toolbar, banner
└── windows/
    ├── tightvncApp.ts        # Application mode
    ├── service.ts            # Full-access mode
    └── foregroundWatcher.ts  # Detects when Windows blocks remote input
scripts/
├── setup-windows.ps1         # Full-access install / removal (runs elevated)
├── foreground-watch.ps1      # Helper for foregroundWatcher.ts
├── uninstall.js              # Cleanup when the extension is uninstalled
└── build-notices.mjs         # Generates ThirdPartyNotices.txt
test/
├── relay-test.mjs            # RFB protocol test against the test server
├── app-mode-test.ts          # Application mode on a real Windows machine
└── browser-harness.ts        # Viewer end-to-end in a headless browser
```

## License

See [LICENSE](LICENSE) for details. Bundled third-party code (noVNC, pako) is listed in [ThirdPartyNotices.txt](ThirdPartyNotices.txt).
