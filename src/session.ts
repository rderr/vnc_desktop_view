import * as vscode from 'vscode';
import * as crypto from 'crypto';
import { Relay } from './relay';
import { PasswordStore, type StoredCredentials } from './passwordStore';
import type { ToExtension, ToWebview } from './protocol';

export interface Target {
  host: string;
  port: number;
  label: string;
  /** Known password (application mode generates one per run); otherwise ask / use the remote password store. */
  password?: string;
  /** Whether the server can reach UAC prompts / lock screen (and so Ctrl+Alt+Del makes sense). */
  fullAccess: boolean;
  /** Which kind of server this is, shown as a badge in the toolbar. */
  mode: ConnectionMode;
}

export type ConnectionMode = 'app' | 'service' | 'custom' | 'test';

/**
 * One editor tab showing one remote desktop. Owns the webview panel and the
 * relay to the VNC server.
 */
export class DesktopSession {
  private readonly panel: vscode.WebviewPanel;
  private relay: Relay | undefined;
  /** A connection the viewer closed, still shutting down; a reconnect waits for it. */
  private closing: Promise<void> | undefined;
  private conn = -1;
  /** Credentials sent to the server but not yet confirmed; saved to the store once the login succeeds. */
  private pendingCredentials: StoredCredentials | undefined;
  /** The pending credentials came from 0.1.0's secret storage (on the computer we connect from) and should move to the store. */
  private migratingLegacySecret = false;
  private readonly disposables: vscode.Disposable[] = [];
  private onDisposeCallbacks: (() => void)[] = [];

  constructor(
    private readonly context: vscode.ExtensionContext,
    private readonly target: Target,
    private readonly passwords?: PasswordStore,
    private readonly log: (msg: string) => void = () => {},
  ) {
    const distUri = vscode.Uri.joinPath(context.extensionUri, 'dist');
    this.panel = vscode.window.createWebviewPanel(
      'vncDesktopView',
      tabTitle(target.label, target.mode),
      vscode.ViewColumn.Active,
      {
        enableScripts: true,
        // Keep the VNC session alive when the tab is in the background.
        retainContextWhenHidden: true,
        localResourceRoots: [distUri],
      },
    );
    this.panel.webview.html = this.html(distUri);
    this.panel.webview.onDidReceiveMessage((m: ToExtension) => this.onMessage(m), null, this.disposables);
    this.panel.onDidDispose(() => this.dispose(), null, this.disposables);
  }

  onDidDispose(cb: () => void): void {
    this.onDisposeCallbacks.push(cb);
  }

  /** Show (or with null, hide) a warning banner above the desktop. Re-sent if the viewer reloads. */
  setBanner(text: string | null, offerFullAccess = false): void {
    this.banner = { type: 'banner', text, offerFullAccess };
    void this.post(this.banner);
  }
  private banner: ToWebview | undefined;

  /** Post to the viewer. Exposed so the test server can report its frame rate. */
  post(msg: ToWebview): Thenable<boolean> {
    return this.panel.webview.postMessage(msg);
  }

  private storeKey(): string {
    return PasswordStore.key(this.target.host, this.target.port);
  }

  /** Where 0.1.0 kept passwords: VS Code secret storage, which lives on the computer you connect from. */
  private legacySecretKey(): string {
    return `vncPassword:${this.target.host}:${this.target.port}`;
  }

  private async rememberPendingCredentials(): Promise<void> {
    const creds = this.pendingCredentials;
    this.pendingCredentials = undefined;
    if (!creds || !this.passwords) return;
    try {
      await this.passwords.set(this.storeKey(), creds);
      if (this.migratingLegacySecret) await this.context.secrets.delete(this.legacySecretKey());
    } catch (e) {
      void vscode.window.showWarningMessage(`Could not save the VNC password on the remote machine: ${(e as Error).message}`);
    } finally {
      this.migratingLegacySecret = false;
    }
  }

  private async forgetCredentials(): Promise<void> {
    this.pendingCredentials = undefined;
    this.migratingLegacySecret = false;
    await this.passwords?.delete(this.storeKey()).catch(() => undefined);
    await this.context.secrets.delete(this.legacySecretKey());
  }

  private async onMessage(msg: ToExtension): Promise<void> {
    switch (msg.type) {
      case 'ready': {
        // Viewer asked for a (re)connection. Close any previous socket first and
        // wait until it's really closed: some servers (the TightVNC service) drop
        // a new connection that arrives while the old one is still being torn down.
        this.conn = msg.conn;
        const conn = msg.conn;
        const old = this.relay;
        this.relay = undefined;
        const closing = [this.closing, old?.disposeAndWait()].filter(Boolean);
        this.closing = undefined;
        if (closing.length) {
          await Promise.all(closing);
          await new Promise((r) => setTimeout(r, 200));
          if (this.conn !== conn) return; // an even newer reconnect superseded this one
        }
        const where = `${this.target.host}:${this.target.port}`;
        this.log(`connection ${conn}: connecting to ${where} (${this.target.mode})`);
        const cfg = vscode.workspace.getConfiguration('vncDesktopView');
        void this.post({
          type: 'init', scaleToFit: cfg.get('scaleToFit', true), label: this.target.label, fullAccess: this.target.fullAccess,
          mode: this.target.mode, server: `${this.target.host}:${this.target.port}`,
        });
        if (this.banner) void this.post(this.banner);
        this.relay = new Relay(this.target.host, this.target.port, {
          open: () => {
            this.log(`connection ${conn}: TCP connected`);
            void this.post({ type: 'open', conn });
          },
          send: (data) => this.post({ type: 'data', conn, data }),
          closed: (reason) => {
            this.log(`connection ${conn}: closed by server side: ${reason}`);
            void this.post({ type: 'closed', conn, reason });
          },
        });
        break;
      }
      case 'log':
        this.log(msg.text);
        break;
      case 'data':
        if (msg.conn === this.conn) this.relay?.write(toBytes(msg.data));
        break;
      case 'close':
        if (msg.conn === this.conn && this.relay) {
          this.log(`connection ${msg.conn}: closed by viewer`);
          this.closing = this.relay.disposeAndWait();
          this.relay = undefined;
        }
        break;
      case 'ack':
        if (msg.conn === this.conn) this.relay?.ack(msg.bytes);
        break;
      case 'stream':
        if (msg.conn === this.conn) this.relay?.stream(msg.width, msg.height);
        break;
      case 'needCredentials':
        await this.provideCredentials(msg.conn, msg.types);
        break;
      case 'connected':
        this.log(`connection ${msg.conn}: logged in${msg.desktopName ? ` to "${msg.desktopName}"` : ''}`);
        // Only remember a password once the server has accepted it.
        await this.rememberPendingCredentials();
        if (msg.desktopName) this.panel.title = tabTitle(msg.desktopName, this.target.mode);
        break;
      case 'authFailed':
        await this.forgetCredentials();
        void vscode.window.showErrorMessage(`VNC authentication failed: ${msg.reason || 'wrong password?'} Use Reconnect to try again.`);
        break;
      case 'clipboard':
        await vscode.env.clipboard.writeText(msg.text);
        vscode.window.setStatusBarMessage('$(clippy) Copied from remote desktop', 2000);
        break;
      case 'pasteRequest':
        void this.post({ type: 'paste', text: await vscode.env.clipboard.readText() });
        break;
      case 'installFullAccess':
        void vscode.commands.executeCommand('vncDesktopView.installService');
        break;
    }
  }

  private async provideCredentials(conn: number, types: string[]): Promise<void> {
    if (this.target.password !== undefined) {
      void this.post({ type: 'credentials', conn, password: this.target.password });
      return;
    }
    const stored = await this.passwords?.get(this.storeKey()).catch((e: Error) => {
      void vscode.window.showWarningMessage(`Could not read saved VNC passwords: ${e.message}`);
      return undefined;
    });
    if (stored) {
      void this.post({ type: 'credentials', conn, ...stored });
      return;
    }
    // Saved by 0.1.0 on this client? Use it, and move it to the remote once it works.
    const legacy = await this.context.secrets.get(this.legacySecretKey());
    if (legacy) {
      this.pendingCredentials = JSON.parse(legacy) as StoredCredentials;
      this.migratingLegacySecret = true;
      void this.post({ type: 'credentials', conn, ...this.pendingCredentials });
      return;
    }
    let username: string | undefined;
    if (types.includes('username')) {
      username = await vscode.window.showInputBox({
        title: `Remote desktop: ${this.target.label}`,
        prompt: 'Username for the remote desktop',
        ignoreFocusOut: true,
      });
      if (username === undefined) return;
    }
    const password = await vscode.window.showInputBox({
      title: `Remote desktop: ${this.target.label}`,
      prompt: 'VNC password (remembered on the remote machine after a successful login)',
      password: true,
      ignoreFocusOut: true,
    });
    if (password === undefined) return;
    this.pendingCredentials = { username, password };
    void this.post({ type: 'credentials', conn, username, password });
  }

  private html(distUri: vscode.Uri): string {
    const webview = this.panel.webview;
    const script = webview.asWebviewUri(vscode.Uri.joinPath(distUri, 'webview.js'));
    const nonce = crypto.randomBytes(16).toString('base64');
    const csp = [
      `default-src 'none'`,
      `img-src ${webview.cspSource} data: blob:`,
      `style-src ${webview.cspSource} 'unsafe-inline'`,
      `script-src 'nonce-${nonce}'`,
    ].join('; ');
    return /* html */ `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy" content="${csp}">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<style>
  html, body { height: 100%; margin: 0; padding: 0; overflow: hidden; }
  body { display: flex; flex-direction: column; background: var(--vscode-editor-background); color: var(--vscode-foreground);
         font-family: var(--vscode-font-family); font-size: var(--vscode-font-size); }
  #toolbar { display: flex; flex-wrap: wrap; gap: 6px; align-items: center; padding: 4px 8px;
             border-bottom: 1px solid var(--vscode-panel-border, transparent); flex: none; }
  button { background: var(--vscode-button-secondaryBackground); color: var(--vscode-button-secondaryForeground);
           border: none; padding: 3px 10px; border-radius: 2px; cursor: pointer; font: inherit; }
  button:hover { background: var(--vscode-button-secondaryHoverBackground); }
  button:disabled { opacity: 0.5; cursor: default; }
  button[aria-pressed="true"] { background: var(--vscode-button-background); color: var(--vscode-button-foreground); }
  #status { margin-left: 8px; }
  #mode { padding: 1px 8px; border-radius: 10px; font-size: 0.9em; white-space: nowrap; cursor: default;
          background: var(--vscode-badge-background); color: var(--vscode-badge-foreground); }
  #mode:empty { display: none; }
  #stats { margin-left: auto; opacity: 0.75; font-family: var(--vscode-editor-font-family); font-variant-numeric: tabular-nums; }
  #banner { display: flex; flex-wrap: wrap; gap: 8px; align-items: center; padding: 6px 10px; flex: none;
            background: var(--vscode-inputValidation-warningBackground, #5a4a00);
            border-bottom: 1px solid var(--vscode-inputValidation-warningBorder, #b89500); }
  #banner[hidden] { display: none; }
  #bannerText { flex: 1; min-width: 200px; }
  #bannerAction { background: var(--vscode-button-background); color: var(--vscode-button-foreground); }
  #bannerAction:hover { background: var(--vscode-button-hoverBackground); }
  #screen { flex: 1; min-height: 0; position: relative; }
  #message { position: absolute; inset: 0; display: flex; align-items: center; justify-content: center;
             padding: 24px; text-align: center; pointer-events: none; }
  #message[hidden] { display: none; }
</style>
</head>
<body>
  <div id="toolbar">
    <button id="fit" aria-pressed="true" title="Scale the desktop to fit this tab">Fit</button>
    <button id="cad" title="Send Ctrl+Alt+Del">Ctrl+Alt+Del</button>
    <button id="paste" title="Send your clipboard text to the remote clipboard">Paste to remote</button>
    <button id="reconnect">Reconnect</button>
    <span id="mode"></span>
    <span id="status">Starting…</span>
    <span id="stats"></span>
  </div>
  <div id="banner" role="status" hidden>
    <span id="bannerText"></span>
    <button id="bannerAction" hidden>Install Full-Access Mode…</button>
  </div>
  <div id="screen"><div id="message" hidden></div></div>
  <script type="module" nonce="${nonce}" src="${script}"></script>
</body>
</html>`;
  }

  dispose(): void {
    this.relay?.dispose();
    this.relay = undefined;
    while (this.disposables.length) this.disposables.pop()?.dispose();
    const cbs = this.onDisposeCallbacks;
    this.onDisposeCallbacks = [];
    cbs.forEach((cb) => cb());
  }
}

const MODE_SUFFIX: Record<ConnectionMode, string> = {
  app: ' (application mode)',
  service: ' (full access)',
  custom: '',
  test: '',
};

function tabTitle(name: string, mode: ConnectionMode): string {
  return `Desktop: ${name}${MODE_SUFFIX[mode]}`;
}

/** Webview messages may deliver binary data as a typed array, an ArrayBuffer, or (worst case) a plain object. */
function toBytes(data: unknown): Uint8Array {
  if (data instanceof Uint8Array) return data;
  if (data instanceof ArrayBuffer) return new Uint8Array(data);
  if (ArrayBuffer.isView(data)) return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
  return Uint8Array.from(Object.values(data as Record<string, number>));
}
