import * as vscode from 'vscode';
import * as crypto from 'crypto';
import * as fs from 'fs';
import { spawn, type ChildProcessWithoutNullStreams } from 'child_process';
import type { VideoToExtension, VideoToWebview } from './videoProtocol';

/** Pause reading the server's output while the viewer has this many bytes not yet decoded. */
const MAX_UNACKED = 1024 * 1024;

/**
 * Experimental: an editor tab showing the desktop through the custom Rust
 * server (server/), which streams H.264 on stdout. View only for now.
 */
export class VideoSession {
  private readonly panel: vscode.WebviewPanel;
  private proc: ChildProcessWithoutNullStreams | undefined;
  private buffered: Buffer = Buffer.alloc(0);
  private sent = 0;
  private acked = 0;
  private paused = false;

  constructor(
    context: vscode.ExtensionContext,
    private readonly binary: string,
    label: string,
    private readonly log: (msg: string) => void,
  ) {
    const distUri = vscode.Uri.joinPath(context.extensionUri, 'dist');
    this.panel = vscode.window.createWebviewPanel('vncDesktopViewVideo', `Desktop: ${label} (video preview)`,
      vscode.ViewColumn.Active, { enableScripts: true, retainContextWhenHidden: true, localResourceRoots: [distUri] });
    this.panel.webview.html = this.html(distUri);
    this.panel.webview.onDidReceiveMessage((m: VideoToExtension) => this.onMessage(m));
    this.panel.onDidDispose(() => this.stop());
  }

  private post(msg: VideoToWebview): Thenable<boolean> {
    return this.panel.webview.postMessage(msg);
  }

  private onMessage(msg: VideoToExtension): void {
    switch (msg.type) {
      case 'ready':
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
        this.proc?.stdin.write(Buffer.from([1, 0, 0, 0, 0]));
        break;
      case 'log':
        this.log(msg.text);
        break;
    }
  }

  private start(): void {
    this.stop();
    this.buffered = Buffer.alloc(0);
    this.sent = this.acked = 0;
    this.paused = false;
    this.log(`video preview: starting ${this.binary}`);
    void this.post({ type: 'status', text: 'Starting server…' });
    const proc = spawn(this.binary, [], { windowsHide: true });
    this.proc = proc;
    proc.stdout.on('data', (chunk: Buffer) => { if (this.proc === proc) this.onData(chunk); });
    proc.stderr.on('data', (chunk: Buffer) => {
      for (const line of chunk.toString().split(/\r?\n/)) if (line.trim()) this.log(`server: ${line}`);
    });
    proc.on('error', (e) => this.log(`video preview: ${e.message}`));
    proc.on('exit', (code) => {
      this.log(`video preview: server exited (${code})`);
      if (this.proc === proc) {
        this.proc = undefined;
        void this.post({ type: 'status', text: `Server stopped (exit code ${code}). Click Restart.` });
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
      }
    }
    if (!this.paused && this.sent - this.acked > MAX_UNACKED) {
      this.paused = true;
      this.proc?.stdout.pause(); // the server then blocks on write and sends fewer frames
    }
  }

  private stop(): void {
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
    const csp = [`default-src 'none'`, `style-src ${webview.cspSource} 'unsafe-inline'`, `script-src 'nonce-${nonce}'`].join('; ');
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
  #screen { flex: 1; min-height: 0; overflow: auto; display: flex; }
  canvas { margin: auto; display: block; }
  #screen.fit canvas { width: 100%; height: 100%; object-fit: contain; }
</style>
</head>
<body>
  <div id="toolbar">
    <button id="fit" aria-pressed="true">Fit</button>
    <button id="restart">Restart</button>
    <span id="badge" title="Experimental custom server (H.264 over VS Code's connection). View only.">Video preview · view only</span>
    <span id="status">Starting…</span>
    <span id="stats"></span>
  </div>
  <div id="screen" class="fit"><canvas id="canvas"></canvas></div>
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
