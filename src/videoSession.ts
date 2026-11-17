import * as vscode from 'vscode';
import * as crypto from 'crypto';
import * as fs from 'fs';
import { spawn, type ChildProcessWithoutNullStreams } from 'child_process';
import type { CommandKey, VideoToExtension, VideoToWebview } from './videoProtocol';

/** Pause reading the server's output while the viewer has this many bytes not yet decoded. */
const MAX_UNACKED = 1024 * 1024;
/** Larger clipboard text is not synced (the server has the same limit). */
const MAX_CLIPBOARD = 1024 * 1024;
/** Restart a server that exits unexpectedly at most this many times a minute. */
const MAX_AUTO_RESTARTS = 3;

/**
 * Experimental: an editor tab showing the desktop through the custom Rust
 * server (server/), which streams H.264 and Opus on stdout and takes input and
 * clipboard text on stdin.
 */
export class VideoSession {
  private readonly panel: vscode.WebviewPanel;
  private proc: ChildProcessWithoutNullStreams | undefined;
  private buffered: Buffer = Buffer.alloc(0);
  private sent = 0;
  private acked = 0;
  private paused = false;
  /** The text both clipboards last agreed on, so a sync only happens when one side changed. */
  private clipboardText: string | undefined;
  /**
   * Keystrokes wait behind a pending clipboard sync (reading the local clipboard
   * is a round trip to the user's machine), so a paste sees the new text. Mouse
   * input never waits.
   */
  private keyQueue: Promise<void> = Promise.resolve();
  private readonly disposables: vscode.Disposable[] = [];
  /** Times of recent unexpected server exits, to stop auto-restarting a server that keeps crashing. */
  private crashes: number[] = [];
  private restartTimer: NodeJS.Timeout | undefined;

  constructor(
    context: vscode.ExtensionContext,
    private readonly binary: string,
    private readonly label: string,
    private readonly log: (msg: string) => void,
  ) {
    const distUri = vscode.Uri.joinPath(context.extensionUri, 'dist');
    this.panel = vscode.window.createWebviewPanel('vncDesktopViewVideo', `Desktop: ${label} (video preview)`,
      vscode.ViewColumn.Active, { enableScripts: true, retainContextWhenHidden: true, localResourceRoots: [distUri] });
    this.panel.webview.html = this.html(distUri);
    this.panel.webview.onDidReceiveMessage((m: VideoToExtension) => this.onMessage(m));
    this.panel.onDidDispose(() => {
      this.stop();
      this.disposables.forEach((d) => d.dispose());
    });
    this.disposables.push(vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration('vncDesktopView.commandKey')) this.postConfig();
    }));
  }

  private postConfig(): void {
    const commandKey = vscode.workspace.getConfiguration('vncDesktopView').get<CommandKey>('commandKey', 'ctrl');
    void this.post({ type: 'config', commandKey });
  }

  /** Local clipboard -> remote, if it changed since the clipboards last agreed. */
  private async syncClipboard(): Promise<void> {
    try {
      const text = await vscode.env.clipboard.readText();
      if (!text || text === this.clipboardText || Buffer.byteLength(text) > MAX_CLIPBOARD) return;
      this.clipboardText = text;
      this.send(5, Buffer.from(text, 'utf8'));
    } catch (e) {
      this.log(`video preview: reading the clipboard failed: ${(e as Error).message}`);
    }
  }

  private post(msg: VideoToWebview): Thenable<boolean> {
    return this.panel.webview.postMessage(msg);
  }

  private onMessage(msg: VideoToExtension): void {
    switch (msg.type) {
      case 'ready':
        this.crashes = []; // a manual (re)start gets a fresh set of automatic retries
        this.postConfig();
        this.start();
        break;
      case 'ack':
        this.acked += msg.bytes;
        if (this.paused && this.sent - this.acked < MAX_UNACKED / 2) {
          this.paused = false;
          this.proc?.stdout.resume();
        }
        break;
      case 'keyframe':
        this.send(1, Buffer.alloc(0));
        break;
      case 'pointer': {
        const p = Buffer.alloc(9);
        p.writeUInt16LE(Math.max(0, Math.round(msg.x)), 0);
        p.writeUInt16LE(Math.max(0, Math.round(msg.y)), 2);
        p.writeUInt8(msg.buttons & 7, 4);
        p.writeInt16LE(clamp16(msg.wheelY), 5);
        p.writeInt16LE(clamp16(msg.wheelX), 7);
        this.send(2, p);
        break;
      }
      case 'key': {
        const p = Buffer.alloc(3);
        p.writeUInt16LE(msg.scan, 0);
        p.writeUInt8(msg.down ? 1 : 0, 2);
        this.keyQueue = this.keyQueue.then(() => this.send(3, p));
        break;
      }
      case 'audio':
        this.send(4, Buffer.from([msg.enabled ? 1 : 0]));
        break;
      case 'syncClipboard':
        this.keyQueue = this.keyQueue.then(() => this.syncClipboard());
        break;
      case 'setCommandKey':
        void vscode.workspace.getConfiguration('vncDesktopView')
          .update('commandKey', msg.value, vscode.ConfigurationTarget.Global);
        break;
      case 'log':
        this.log(msg.text);
        break;
    }
  }

  /** Writes one message to the server's stdin: u8 kind, u32 LE length, payload. */
  private send(kind: number, payload: Buffer): void {
    const proc = this.proc;
    if (!proc || !proc.stdin.writable) return;
    const header = Buffer.alloc(5);
    header[0] = kind;
    header.writeUInt32LE(payload.length, 1);
    proc.stdin.write(payload.length ? Buffer.concat([header, payload]) : header);
  }

  private start(): void {
    clearTimeout(this.restartTimer);
    this.stop();
    this.buffered = Buffer.alloc(0);
    this.sent = this.acked = 0;
    this.paused = false;
    this.log(`video preview: starting ${this.binary}`);
    void this.post({ type: 'status', text: 'Starting server…' });
    void this.post({ type: 'notice', text: null });
    const proc = spawn(this.binary, [], { windowsHide: true });
    this.proc = proc;
    proc.stdout.on('data', (chunk: Buffer) => { if (this.proc === proc) this.onData(chunk); });
    proc.stderr.on('data', (chunk: Buffer) => {
      for (const line of chunk.toString().split(/\r?\n/)) if (line.trim()) this.log(`server: ${line}`);
    });
    proc.on('error', (e) => this.log(`video preview: ${e.message}`));
    proc.on('exit', (code) => {
      this.log(`video preview: server exited (${code})`);
      if (this.proc !== proc) return; // we stopped it on purpose
      this.proc = undefined;
      // Unexpected exit: restart automatically, unless it keeps happening.
      const now = Date.now();
      this.crashes = this.crashes.filter((t) => now - t < 60_000);
      this.crashes.push(now);
      if (this.crashes.length <= MAX_AUTO_RESTARTS) {
        void this.post({ type: 'status', text: 'Server stopped unexpectedly; restarting…' });
        this.restartTimer = setTimeout(() => this.start(), 1000);
      } else {
        void this.post({ type: 'status', text: `Server keeps stopping (exit code ${code}). Click Restart to try again.` });
      }
    });
    proc.stdin.on('error', () => undefined); // server gone; 'exit' reports it
  }

  /** Splits the server's output into messages: u8 kind, u32 LE length, payload. */
  private onData(chunk: Buffer): void {
    this.buffered = this.buffered.length ? Buffer.concat([this.buffered, chunk]) : chunk;
    while (this.buffered.length >= 5) {
      const len = this.buffered.readUInt32LE(1);
      if (this.buffered.length < 5 + len) break;
      const kind = this.buffered[0];
      const payload = this.buffered.subarray(5, 5 + len);
      this.buffered = this.buffered.subarray(5 + len);
      if (kind === 1) {
        const width = payload.readUInt32LE(0), height = payload.readUInt32LE(4);
        const codec = payload.subarray(8).toString('utf8');
        this.log(`video preview: ${width}x${height} ${codec}`);
        void this.post({ type: 'hello', width, height, codec });
      } else if (kind === 2) {
        const key = (payload[0] & 1) === 1;
        const timestamp = Number(payload.readBigUInt64LE(1));
        // Tight copy so only this frame (not Node's whole pooled buffer) is serialized.
        const data = new Uint8Array(payload.subarray(9));
        this.sent += data.byteLength;
        void this.post({ type: 'video', key, timestamp, data });
      } else if (kind === 5) {
        // Capture can't run right now (secure desktop, or failing); the server keeps retrying.
        const code = payload[0];
        const detail = payload.subarray(1).toString('utf8');
        if (code !== 0) this.log(`video preview: capture unavailable (${code}): ${detail}`);
        void this.post({ type: 'notice', text: noticeText(code, this.label, detail) });
      } else if (kind === 3) {
        // Audio is small (~16 KB/s) and not counted against the video flow-control window.
        const timestamp = Number(payload.readBigUInt64LE(0));
        void this.post({ type: 'audio', timestamp, data: new Uint8Array(payload.subarray(8)) });
      } else if (kind === 4) {
        // The remote clipboard changed: put it on the local one.
        const text = payload.toString('utf8');
        this.clipboardText = text;
        void vscode.env.clipboard.writeText(text);
        vscode.window.setStatusBarMessage('$(clippy) Copied from remote desktop', 2000);
      }
    }
    if (!this.paused && this.sent - this.acked > MAX_UNACKED) {
      this.paused = true;
      this.proc?.stdout.pause(); // the server then blocks on write and sends fewer frames
    }
  }

  private stop(): void {
    clearTimeout(this.restartTimer);
    const proc = this.proc;
    this.proc = undefined;
    if (proc) {
      proc.stdin.end();
      setTimeout(() => proc.kill(), 1000);
    }
  }

  private html(distUri: vscode.Uri): string {
    const webview = this.panel.webview;
    const script = webview.asWebviewUri(vscode.Uri.joinPath(distUri, 'video.js'));
    const nonce = crypto.randomBytes(16).toString('base64');
    const csp = [`default-src 'none'`, `style-src ${webview.cspSource} 'unsafe-inline'`,
      // wasm-unsafe-eval: the Opus audio decoder is WebAssembly.
      `script-src 'nonce-${nonce}' 'wasm-unsafe-eval'`].join('; ');
    return /* html */ `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy" content="${csp}">
<style>
  html, body { height: 100%; margin: 0; overflow: hidden; }
  body { display: flex; flex-direction: column; background: var(--vscode-editor-background); color: var(--vscode-foreground);
         font-family: var(--vscode-font-family); font-size: var(--vscode-font-size); }
  #toolbar { display: flex; gap: 6px; align-items: center; padding: 4px 8px; flex: none;
             border-bottom: 1px solid var(--vscode-panel-border, transparent); }
  button { background: var(--vscode-button-secondaryBackground); color: var(--vscode-button-secondaryForeground);
           border: none; padding: 3px 10px; border-radius: 2px; cursor: pointer; font: inherit; }
  button:hover { background: var(--vscode-button-secondaryHoverBackground); }
  button[aria-pressed="true"] { background: var(--vscode-button-background); color: var(--vscode-button-foreground); }
  #badge { padding: 1px 8px; border-radius: 10px; font-size: 0.9em; background: var(--vscode-badge-background); color: var(--vscode-badge-foreground); }
  #stats { margin-left: auto; opacity: 0.75; font-family: var(--vscode-editor-font-family); font-variant-numeric: tabular-nums; }
  #notice { padding: 6px 10px; flex: none; background: var(--vscode-inputValidation-warningBackground, #5a4a00);
            border-bottom: 1px solid var(--vscode-inputValidation-warningBorder, #b89500); }
  #notice[hidden] { display: none; }
  #screen { flex: 1; min-height: 0; overflow: auto; display: flex; }
  canvas { margin: auto; display: block; outline: none; }
  #screen.fit canvas { width: 100%; height: 100%; object-fit: contain; }
</style>
</head>
<body>
  <div id="toolbar">
    <button id="fit" aria-pressed="true">Fit</button>
    <button id="restart">Restart</button>
    <button id="sound" aria-pressed="false" title="Play the remote machine's sound here">Sound off</button>
    <button id="cmdkey" hidden title="What the ⌘ key sends to the remote desktop (setting: vncDesktopView.commandKey)">⌘ = Ctrl</button>
    <span id="badge" title="Experimental custom server (H.264 over VS Code's connection).">Video preview</span>
    <span id="status">Starting…</span>
    <span id="stats"></span>
  </div>
  <div id="notice" role="status" hidden></div>
  <div id="screen" class="fit"><canvas id="canvas" tabindex="0"></canvas></div>
  <script nonce="${nonce}" src="${script}"></script>
</body>
</html>`;
  }
}

/** The server binary, if it has been built (dev only for now: `cargo build --release` in server/). */
export function findServerBinary(extensionPath: string): string | undefined {
  const exe = process.platform === 'win32' ? 'desktop-server.exe' : 'desktop-server';
  for (const p of [`${extensionPath}/bin/${exe}`, `${extensionPath}/server/target/release/${exe}`]) {
    if (fs.existsSync(p)) return p;
  }
  return undefined;
}

function clamp16(v: number): number {
  return Math.max(-32768, Math.min(32767, Math.round(v)));
}

/** Banner text for a server NOTICE (null hides the banner). */
function noticeText(code: number, label: string, detail: string): string | null {
  switch (code) {
    case 0:
      return null;
    case 1:
      return `${label} is showing the lock screen, a UAC prompt or another secure screen, which the video preview ` +
        `can't show or control. The picture comes back by itself when the desktop returns ` +
        `(to unlock remotely, use the full-access VNC tab: "Open Desktop").`;
    default:
      return `Can't capture the screen of ${label} right now; retrying… (${detail})`;
  }
}
