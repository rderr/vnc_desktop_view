# Changelog

All notable changes to the VNC Desktop View extension will be documented in this file.

## [Unreleased]

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
