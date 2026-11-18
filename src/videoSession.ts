import * as vscode from 'vscode';
import * as crypto from 'crypto';
import * as fs from 'fs';
import { spawn, type ChildProcessWithoutNullStreams } from 'child_process';
import type { CommandKey, VideoToExtension, VideoToWebview } from './videoProtocol';
import { CISCO_NOTICE, downloadOpenH264, openH264Applies, openH264Path, openH264Setting, setOpenH264Setting } from './openh264';

/** Pause reading the server's output while the viewer has this many bytes not yet decoded. */
const MAX_UNACKED = 1024 * 1024;
/** Larger clipboard text is not synced (the server has the same limit). */
const MAX_CLIPBOARD = 1024 * 1024;
/** Restart a server that exits unexpectedly at most this many times a minute. */
const MAX_AUTO_RESTARTS = 3;

/**
 * An editor tab showing the remote desktop through the desktop server
 * (server/, Rust), which streams H.264 and Opus on stdout and takes input and
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
  private readonly storageDir: string;
  /** Asked about the OpenH264 software encoder in this tab already. */
  private askedOpenH264 = false;
  /** Why setting up OpenH264 failed, if it did (shown with the "no encoder" notice). */
  private openH264Error: string | undefined;

  constructor(
    context: vscode.ExtensionContext,
    private readonly binary: string,
    private readonly label: string,
    private readonly log: (msg: string) => void,
    /**
     * The desktop is the one VS Code itself is shown on (a local window): mouse and
     * keyboard input would land back in this tab and repeat endlessly, so none is sent.
     */
    private readonly viewOnly = false,
  ) {
    this.storageDir = context.globalStorageUri.fsPath;
    const distUri = vscode.Uri.joinPath(context.extensionUri, 'dist');
    this.panel = vscode.window.createWebviewPanel('vncDesktopView', `Desktop: ${label}`,
      vscode.ViewColumn.Active, { enableScripts: true, retainContextWhenHidden: true, localResourceRoots: [distUri] });
    this.panel.webview.html = this.html(distUri);
    this.panel.webview.onDidReceiveMessage((m: VideoToExtension) => this.onMessage(m));
    this.panel.onDidDispose(() => {
      this.stop();
      this.disposables.forEach((d) => d.dispose());
    });
    this.disposables.push(vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration('vncDesktopView.commandKey')) this.postConfig();
      // Turning the software encoder on or off takes effect right away.
      if (e.affectsConfiguration('vncDesktopView.openH264') && openH264Applies()) {
        this.openH264Error = undefined;
        this.start();
      }
    }));
  }

  private postConfig(): void {
    const commandKey = vscode.workspace.getConfiguration('vncDesktopView').get<CommandKey>('commandKey', 'ctrl');
    void this.post({ type: 'config', commandKey, remoteMac: process.platform === 'darwin' });
  }

  /** Local clipboard -> remote, if it changed since the clipboards last agreed. */
  private async syncClipboard(): Promise<void> {
    try {
      const text = await vscode.env.clipboard.readText();
      if (!text || text === this.clipboardText || Buffer.byteLength(text) > MAX_CLIPBOARD) return;
      this.clipboardText = text;
      this.send(5, Buffer.from(text, 'utf8'));
    } catch (e) {
      this.log(`desktop: reading the clipboard failed: ${(e as Error).message}`);
    }
  }

  private post(msg: VideoToWebview): Thenable<boolean> {
    return this.panel.webview.postMessage(msg);
  }

  private onMessage(msg: VideoToExtension): void {
    switch (msg.type) {
      case 'ready':
        this.crashes = []; // a manual (re)start gets a fresh set of automatic retries
        this.openH264Error = undefined; // and another try at setting up OpenH264
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
        if (this.viewOnly) break;
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
        if (this.viewOnly) break;
        const p = Buffer.alloc(3);
        p.writeUInt16LE(msg.scan, 0);
        p.writeUInt8(msg.down ? 1 : 0, 2);
        this.keyQueue = this.keyQueue.then(() => this.send(3, p));
        break;
      }
      case 'audio':
        if (this.viewOnly) break; // this computer's own sound would echo through the tab
        this.send(4, Buffer.from([msg.enabled ? 1 : 0]));
        break;
      case 'syncClipboard':
        if (this.viewOnly) break;
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
    this.log(`desktop: starting ${this.binary}`);
    void this.post({ type: 'status', text: 'Starting server…' });
    void this.post({
      type: 'notice',
      text: this.viewOnly
        ? 'View only: this tab shows the screen of the computer you are using, so mouse, keyboard and sound are off (they would ' +
          'loop back through this tab). Connect VS Code to a remote machine to control its desktop.'
        : null,
    });
    if (process.platform !== 'win32') {
      // Packages built elsewhere can lose the executable bit.
      try { fs.accessSync(this.binary, fs.constants.X_OK); } catch { try { fs.chmodSync(this.binary, 0o755); } catch { /* spawn reports it */ } }
    }
    const args: string[] = [];
    const openH264 = openH264Applies() ? openH264Setting() : 'off';
    if ((openH264 === 'on' || openH264 === 'always') && !this.openH264Error) {
      const lib = openH264Path(this.storageDir);
      if (!fs.existsSync(lib)) {
        void this.fetchOpenH264(); // starts the server when done
        return;
      }
      args.push('--openh264', lib);
      // 'always': software even when there is a GPU encoder.
      if (openH264 === 'always') args.push('--encoder', 'openh264');
    } else if (openH264 === 'always' && this.openH264Error) {
      void this.post({ type: 'notice', text: `OpenH264 couldn't be set up, so the GPU encoder is used: ${this.openH264Error}` });
    }
    const proc = spawn(this.binary, args, { windowsHide: true });
    this.proc = proc;
    proc.stdout.on('data', (chunk: Buffer) => { if (this.proc === proc) this.onData(chunk); });
    proc.stderr.on('data', (chunk: Buffer) => {
      for (const line of chunk.toString().split(/\r?\n/)) if (line.trim()) this.log(`server: ${line}`);
    });
    proc.on('error', (e) => this.log(`desktop: ${e.message}`));
    proc.on('exit', (code) => {
      this.log(`desktop: server exited (${code})`);
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

  /** Downloads Cisco's OpenH264 (the user turned it on), then starts the server. */
  private async fetchOpenH264(): Promise<void> {
    void this.post({ type: 'status', text: 'Downloading OpenH264 from Cisco…' });
    try {
      await downloadOpenH264(this.storageDir, this.log);
    } catch (e) {
      this.openH264Error = (e as Error).message;
      this.log(`openh264: ${this.openH264Error}`);
    }
    this.start();
  }

  /** No hardware encoder: offer the software encoder, once per tab, unless the user already decided. */
  private async offerOpenH264(): Promise<void> {
    if (!openH264Applies() || this.askedOpenH264 || openH264Setting() !== 'ask') return;
    this.askedOpenH264 = true;
    const use = 'Download and Use';
    const choice = await vscode.window.showInformationMessage(
      `${this.label} has no hardware video encoder that Remote Desktop View can use. Use the OpenH264 software encoder instead?`,
      {
        modal: true,
        detail: `OpenH264 is downloaded once from Cisco to ${this.label} (about 0.6 MB). It uses more CPU than a GPU encoder.\n\n` +
          `${CISCO_NOTICE}\n\nYou can change this later with the "vncDesktopView.openH264" setting.`,
      },
      use, "Don't Use",
    );
    // The setting change restarts the server (see the constructor).
    if (choice === use) await setOpenH264Setting('on');
    else if (choice === "Don't Use") await setOpenH264Setting('off');
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
        // codec, NUL, short encoder label, NUL, full encoder name
        const [codec, encoder = '', encoderName = ''] = payload.subarray(8).toString('utf8').split('\0');
        this.log(`desktop: ${width}x${height} ${codec}, ${encoderName || encoder}`);
        void this.post({ type: 'hello', width, height, codec, encoder, encoderName });
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
        if (code !== 0) this.log(`desktop: capture unavailable (${code}): ${detail}`);
        void this.post({ type: 'notice', text: noticeText(code, this.label, detail, this.openH264Error) });
        if (code === NOTICE_NO_ENCODER) void this.offerOpenH264();
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

/** The server binary: bin/ in installed packages, server/target/release in a development checkout. */
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

const NOTICE_NO_ENCODER = 3;

/** Banner text for a server NOTICE (null hides the banner). */
function noticeText(code: number, label: string, detail: string, openH264Error?: string): string | null {
  switch (code) {
    case 0:
      return null;
    case 1:
      return `${label} is showing the lock screen, a UAC prompt or another secure screen, which can't be shown ` +
        `or controlled from here yet. The picture comes back by itself when the desktop returns.`;
    case NOTICE_NO_ENCODER: {
      let text = `${label} has no video encoder that Remote Desktop View can use (${detail}).`;
      if (openH264Applies()) {
        if (openH264Error) text += ` Setting up the OpenH264 software encoder failed: ${openH264Error}`;
        else if (openH264Setting() === 'off') text += ' The OpenH264 software encoder is turned off; turn it on with the "vncDesktopView.openH264" setting.';
      }
      return text;
    }
    default:
      return `Can't capture the screen of ${label} right now; retrying… (${detail})`;
  }
}
