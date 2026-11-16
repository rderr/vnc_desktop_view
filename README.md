# Remote Desktop (in-editor) — prototype

Shows the remote machine's desktop in a VS Code tab. The desktop is streamed over
the connection VS Code already has (Remote-SSH, Remote Tunnels, WSL…), so no
ports are opened and nothing is installed on your own machine.

```
your machine: editor tab (noVNC)  <-- webview.postMessage -->  remote: extension host (relay)  <-- TCP 127.0.0.1:5900 -->  VNC server
```

## Layout

| File | Runs on | Purpose |
|---|---|---|
| `src/extension.ts` | remote | Commands |
| `src/session.ts` | remote | One tab: webview panel, password storage, clipboard |
| `src/relay.ts` | remote | TCP ⇄ webview message pipe, with batching and backpressure. Also requests updates from the server itself instead of waiting a round trip for each viewer request, limited by how much data the viewer has confirmed receiving |
| `src/webview/main.ts` | your machine | noVNC viewer + `PostMessageChannel` (a WebSocket look-alike) |
| `src/testServer.ts` | remote | Built-in fake VNC server for testing |
| `src/windows/tightvncApp.ts` | remote | Windows default: TightVNC in application mode (no admin) |
| `src/windows/service.ts`, `scripts/setup-windows.ps1` | remote | Windows optional: full-access TightVNC service |
| `src/setup.ts` | remote | Manual setup instructions for macOS / Linux |

## VNC server modes (`remoteDesktop.server`)

| Mode | Admin needed | UAC prompts, lock screen, Ctrl+Alt+Del | How it works |
|---|---|---|---|
| **app** (Windows default) | No | No | Downloads TightVNC once (hash-checked), unpacks it into the extension's storage folder, and runs `tvnserver.exe -run` only while a desktop tab is open, with a new random port and password each time. A user's own TightVNC application-mode settings (`HKCU\Software\TightVNC\Server`) are backed up and restored. |
| **service** | Once | Yes | **Install Full-Access Mode** installs TightVNC as a service, localhost-only, with a random password handed to the extension. Records itself in `%ProgramData%\vscode-remote-desktop\service.json`. **Remove Full-Access Mode** uninstalls it only if the extension installed it. |
| **custom** | — | Depends | Any VNC server at `remoteDesktop.host`/`port` (macOS Screen Sharing, x11vnc, …). |

`auto` (the default) picks *service* when it is installed, otherwise *app* on Windows and *custom* elsewhere.

Note: TightVNC briefly blocks all connections from 127.0.0.1 after a failed login, so a wrong saved password causes a short lockout.

The extension is a *workspace* extension (`extensionKind: ["workspace"]`), so when
you're connected to a remote it is installed and runs on the remote side.

## Try it

```sh
npm install
npm run build
```

Then press **F5** (Run Extension) and, in the new window:

1. **Remote Desktop: Open Test Desktop** → *Light* or *Stress*. Uses the built-in
   fake server, so nothing needs installing. Drag to draw; press any key to change
   the square's colour. The toolbar shows throughput and frame rate.
2. **Remote Desktop: Open Desktop** shows the real desktop. On Windows this uses
   application mode with no setup.
3. Optionally, **Remote Desktop: Install Full-Access Mode** for UAC prompts and the lock screen.

To test from another machine (for example a Mac connected to a Windows remote), package
it with `npx @vscode/vsce package` and install the `.vsix` into the remote window
(Extensions view → `…` → *Install from VSIX*).

## Tests

- `npm test` — headless RFB handshake and update test against the test server.
- `test/app-mode-test.ts` — Windows only: real download/start/login/stop of
  application mode, including the settings backup and restore.
- `test/browser-harness.ts` — loads the real viewer bundle in headless Edge/Chrome
  with a stand-in for VS Code's messaging, drives mouse and keyboard, and saves
  a screenshot.

## Known gaps

- Keyboard: VS Code may grab some shortcuts (e.g. ⌘W) before the remote sees them;
  no ⌘→Ctrl mapping for Mac users yet.
- Clipboard: remote→local is automatic; local→remote is the *Paste to remote* button.
- macOS and Linux server setup is instructions only, not automated.
- Uninstalling the extension doesn't yet remove the full-access service (use *Remove Full-Access Mode* first),
  or the unpacked TightVNC files in the extension's storage folder.
- Sharing the physical screen: VNC shows the remote's real display, unlike RDP.
