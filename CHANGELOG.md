# Changelog

All notable changes to the Remote Desktop View extension (formerly VNC Desktop View) will be documented in this file.

## [Unreleased]

## [0.2.0]

The desktop is now streamed by the extension's own desktop server instead of VNC. The extension is renamed **Remote Desktop View**; its ID and settings prefix are unchanged.

### Added
- H.264 video, encoded on the remote GPU straight from video memory (Windows' software encoder when there's no hardware one) and decoded by VS Code: smoother and a fraction of VNC's bandwidth
- Sound from the remote machine (**Sound** button)
- Clipboard sync in both directions
- On a Mac, ⌘ acts as Ctrl on the remote (`vncDesktopView.commandKey`, or the **⌘ = Ctrl** button)
- Recovery from resolution changes, the lock screen, UAC prompts and server crashes without closing the tab
- Linux remotes: X11 desktops with an NVIDIA GPU (NVENC, up to 4K), with input, sound (PulseAudio / PipeWire) and clipboard sync
- Linux remotes with Intel or AMD GPUs through VA-API (experimental, untested on real hardware)

### Removed
- VNC: application mode, full-access mode, custom VNC servers, the test desktop, saved VNC passwords, and the `vncDesktopView.server`, `host`, `port` and `scaleToFit` settings
- macOS remotes (they relied on VNC); support with the new server is planned

## [0.1.1] - 2026-11-16

### Fixed
- The full-access password only worked on the computer that installed full-access mode: it was saved in VS Code's secret storage, which lives on the computer you connect *from*. Remembered passwords are now stored on the remote machine (encrypted with DPAPI on Windows), so every computer that connects to it can use them
- Passwords saved by 0.1.0 move to the remote machine automatically the next time you connect from the computer that has them
- **Reconnect** sometimes needed two clicks in full-access mode: the old connection is now closed gracefully and fully before the new one opens, and a connection that drops while starting up is retried automatically (up to 3 attempts; never after a failed login)

### Added
- Connection events (connect, login, disconnect and why) are logged in the **VNC Desktop View** output channel

## [0.1.0] - 2026-11-16

First public preview.

### Added
- Remote machine's desktop in an editor tab (noVNC), carried over the existing VS Code remote connection (Remote Tunnels, Remote-SSH, WSL) — no ports opened, nothing installed on your own machine
- Update streaming: the remote side requests screen updates itself, with flow control, so frame rate isn't limited by the connection's round-trip time
- Windows **application mode** (default): TightVNC is downloaded once (SHA-256 verified) and runs only while a desktop tab is open, without administrator rights, with a new random port and password each time
- Windows **full-access mode** (optional): TightVNC as a Windows service for UAC prompts, the lock screen and Ctrl+Alt+Del; localhost-only, random password handed to the extension
- Warning banner when Windows blocks remote input in application mode (administrator window focused, UAC prompt or lock screen showing)
- Mode badge and tab titles showing which mode a tab is connected in
- Remote clipboard copied to your clipboard; **Paste to remote** button
- Custom mode for any VNC server (macOS Screen Sharing, x11vnc, …)
- Built-in test desktop for checking speed and input
- Cleanup of application-mode files and settings when the extension is uninstalled
