// Runs inside the editor tab (on the user's own machine). Hosts noVNC and
// feeds it bytes that travel over VS Code's webview messaging instead of a
// real WebSocket.

import RFB from '@novnc/novnc';
import type { ToExtension, ToWebview } from '../protocol';

declare function acquireVsCodeApi(): { postMessage(msg: unknown): void };
const vscode = acquireVsCodeApi();
const post = (msg: ToExtension) => vscode.postMessage(msg);

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const screenEl = $<HTMLDivElement>('screen');
const messageEl = $<HTMLDivElement>('message');
const statusEl = $<HTMLSpanElement>('status');
const statsEl = $<HTMLSpanElement>('stats');
const fitBtn = $<HTMLButtonElement>('fit');

/**
 * Looks enough like a WebSocket for noVNC's Websock.attach(): it needs
 * send/close/binaryType/protocol/readyState and the four on* handlers.
 */
class PostMessageChannel {
  binaryType = 'arraybuffer';
  protocol = '';
  readyState: 'connecting' | 'open' | 'closing' | 'closed' = 'connecting';
  onopen: ((e: Event) => void) | null = null;
  onmessage: ((e: { data: ArrayBuffer }) => void) | null = null;
  onclose: ((e: { code: number; reason: string; wasClean: boolean }) => void) | null = null;
  onerror: ((e: Event) => void) | null = null;

  constructor(readonly conn: number) {}

  send = (data: Uint8Array | ArrayBuffer): void => {
    if (this.readyState !== 'open') return;
    // noVNC reuses its send buffer and hands us a subarray of it, so copy.
    const bytes = data instanceof ArrayBuffer ? new Uint8Array(data.slice(0)) : data.slice();
    stats.up += bytes.byteLength;
    post({ type: 'data', conn: this.conn, data: bytes });
  };

  close = (): void => {
    if (this.readyState === 'closed') return;
    this.readyState = 'closed';
    post({ type: 'close', conn: this.conn });
    this.onclose?.({ code: 1000, reason: '', wasClean: true });
  };

  // Called when messages arrive from the extension.
  remoteOpened(): void {
    this.readyState = 'open';
    this.onopen?.(new Event('open'));
  }
  remoteData(data: Uint8Array | ArrayBuffer): void {
    if (this.readyState !== 'open') return;
    const buf = data instanceof ArrayBuffer
      ? data
      : data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength) as ArrayBuffer;
    stats.down += buf.byteLength;
    stats.msgs++;
    this.onmessage?.({ data: buf });
    // Lets the relay keep a bounded amount of data in flight (see relay.ts).
    post({ type: 'ack', conn: this.conn, bytes: buf.byteLength });
  }
  remoteClosed(reason: string): void {
    if (this.readyState === 'closed') return;
    this.readyState = 'closed';
    this.onclose?.({ code: 1006, reason, wasClean: false });
  }
}

const stats = { down: 0, up: 0, msgs: 0, serverFps: undefined as number | undefined };
let channel: PostMessageChannel | undefined;
let rfb: any;
let nextConn = 1;
let scaleToFit = true;
let initialized = false;
let lastDisconnectReason = '';

function setStatus(text: string): void {
  statusEl.textContent = text;
}
function showMessage(text: string | null): void {
  messageEl.hidden = !text;
  messageEl.textContent = text ?? '';
}

const MODES: Record<string, { label: string; detail: string }> = {
  app: {
    label: 'Application mode',
    detail: 'TightVNC runs as an ordinary program while this tab is open (no admin rights). ' +
      'It can\'t show or control UAC prompts, the lock screen or administrator windows, and can\'t send Ctrl+Alt+Del.',
  },
  service: {
    label: 'Full access',
    detail: 'TightVNC runs as a Windows service, so UAC prompts, the lock screen, administrator windows ' +
      'and Ctrl+Alt+Del all work.',
  },
  custom: {
    label: 'Custom server',
    detail: 'Connected to the VNC server set in vncDesktopView.host / vncDesktopView.port.',
  },
  test: {
    label: 'Test server',
    detail: 'The extension\'s built-in fake VNC server, for checking speed and input.',
  },
};

function showMode(mode: string | undefined, server: string | undefined): void {
  const el = document.getElementById('mode');
  const info = mode ? MODES[mode] : undefined;
  if (!el || !info) return;
  el.textContent = info.label;
  el.title = `${info.detail}\n\nServer (on the remote machine): ${server ?? 'unknown'}`;
}

/** Ask the relay to pre-request updates for the current framebuffer size. */
let streamedSize = '';
function requestStreaming(): void {
  const w: number = rfb?._fbWidth ?? 0, h: number = rfb?._fbHeight ?? 0;
  if (!channel || channel.readyState !== 'open' || !w || !h || `${w}x${h}` === streamedSize) return;
  streamedSize = `${w}x${h}`;
  post({ type: 'stream', conn: channel.conn, width: w, height: h });
}

// A connection that drops before it's established (or within a few seconds of it)
// is retried automatically. Some servers — e.g. the TightVNC service — can drop a
// new connection that arrives while they're still tearing down the previous one.
const MAX_ATTEMPTS = 3;
const RETRY_DELAY_MS = [0, 700, 1500];
const EARLY_DROP_MS = 3000;
let retryTimer: ReturnType<typeof setTimeout> | undefined;

function connect(attempt = 1): void {
  clearTimeout(retryTimer);
  streamedSize = '';
  // Detach the old connection BEFORE closing it, so its (possibly synchronous)
  // disconnect event is recognised as stale and doesn't trigger a retry.
  const old = rfb;
  rfb = undefined;
  old?.disconnect();

  lastDisconnectReason = '';
  stats.serverFps = undefined;
  const conn = nextConn++;
  channel = new PostMessageChannel(conn);
  setStatus(attempt === 1 ? 'Connecting…' : `Reconnecting… (attempt ${attempt} of ${MAX_ATTEMPTS})`);
  showMessage(null);

  const thisRfb = new RFB(screenEl, channel, { shared: true });
  rfb = thisRfb;
  const isCurrent = () => rfb === thisRfb;
  let connectedAt = 0;
  let authFailed = false;

  thisRfb.scaleViewport = scaleToFit;
  thisRfb.clipViewport = !scaleToFit;
  thisRfb.resizeSession = false;
  thisRfb.focusOnClick = true;

  thisRfb.addEventListener('connect', () => {
    if (!isCurrent()) return;
    connectedAt = Date.now();
    setStatus('Connected');
    showMessage(null);
    post({ type: 'connected', conn, desktopName: thisRfb._fbName ?? '' });
    requestStreaming();
    thisRfb.focus();
  });
  thisRfb.addEventListener('desktopname', (e: CustomEvent<{ name: string }>) => {
    if (isCurrent()) post({ type: 'connected', conn, desktopName: e.detail.name });
  });
  thisRfb.addEventListener('disconnect', (e: CustomEvent<{ clean: boolean }>) => {
    if (!isCurrent()) return; // a connection we replaced on purpose
    const why = lastDisconnectReason || (e.detail.clean ? 'Disconnected.' : 'Connection lost.');
    const droppedEarly = !connectedAt || Date.now() - connectedAt < EARLY_DROP_MS;
    post({ type: 'log', text: `connection ${conn} closed (attempt ${attempt}, ${connectedAt ? 'after connecting' : 'before connecting'}): ${why}` });
    // Never retry after a failed login: TightVNC locks out the machine briefly after one.
    if (droppedEarly && !authFailed && attempt < MAX_ATTEMPTS) {
      setStatus(`Reconnecting… (attempt ${attempt + 1} of ${MAX_ATTEMPTS})`);
      retryTimer = setTimeout(() => { if (isCurrent()) connect(attempt + 1); }, RETRY_DELAY_MS[attempt]);
      return;
    }
    setStatus('Disconnected');
    showMessage(why);
  });
  thisRfb.addEventListener('credentialsrequired', (e: CustomEvent<{ types: string[] }>) => {
    if (!isCurrent()) return;
    setStatus('Waiting for password…');
    post({ type: 'needCredentials', conn, types: e.detail.types });
  });
  thisRfb.addEventListener('securityfailure', (e: CustomEvent<{ status: number; reason?: string }>) => {
    if (!isCurrent()) return;
    authFailed = true;
    lastDisconnectReason = `Authentication failed${e.detail.reason ? `: ${e.detail.reason}` : ''}.`;
    post({ type: 'authFailed', conn, reason: e.detail.reason ?? '' });
  });
  thisRfb.addEventListener('clipboard', (e: CustomEvent<{ text: string }>) => {
    if (!isCurrent()) return;
    post({ type: 'clipboard', text: e.detail.text });
  });

  post({ type: 'ready', conn });
}

window.addEventListener('message', (event: MessageEvent<ToWebview>) => {
  const msg = event.data;
  switch (msg.type) {
    case 'init':
      // Sent on every (re)connect; only apply the setting the first time so
      // the user's Fit toggle survives a reconnect.
      showMode(msg.mode, msg.server);
      if (initialized) break;
      initialized = true;
      if (!msg.fullAccess) {
        const cad = $<HTMLButtonElement>('cad');
        cad.disabled = true;
        cad.title = 'Not available in application mode. Run "VNC Desktop View: Install Full-Access Mode" for Ctrl+Alt+Del, UAC prompts and the lock screen.';
      }
      scaleToFit = msg.scaleToFit;
      fitBtn.setAttribute('aria-pressed', String(scaleToFit));
      if (rfb) {
        rfb.scaleViewport = scaleToFit;
        rfb.clipViewport = !scaleToFit;
      }
      break;
    case 'open':
      if (msg.conn === channel?.conn) channel.remoteOpened();
      break;
    case 'data':
      if (msg.conn === channel?.conn) channel.remoteData(msg.data);
      break;
    case 'closed':
      if (msg.conn === channel?.conn) {
        lastDisconnectReason ||= msg.reason;
        channel.remoteClosed(msg.reason);
      }
      break;
    case 'credentials':
      if (msg.conn === channel?.conn && rfb) {
        setStatus('Authenticating…');
        rfb.sendCredentials({ username: msg.username, password: msg.password });
      }
      break;
    case 'paste':
      if (rfb && msg.text) {
        rfb.clipboardPasteFrom(msg.text);
        setStatus('Clipboard sent — paste on the remote with Ctrl+V');
        setTimeout(() => setStatus('Connected'), 2500);
      }
      break;
    case 'serverStats':
      stats.serverFps = msg.fps;
      break;
    case 'banner': {
      const banner = document.getElementById('banner');
      if (!banner) break;
      banner.hidden = !msg.text;
      $('bannerText').textContent = msg.text ?? '';
      $('bannerAction').hidden = !msg.offerFullAccess;
      break;
    }
  }
});

fitBtn.addEventListener('click', () => {
  scaleToFit = !scaleToFit;
  fitBtn.setAttribute('aria-pressed', String(scaleToFit));
  if (rfb) {
    rfb.scaleViewport = scaleToFit;
    rfb.clipViewport = !scaleToFit;
  }
});
$('cad').addEventListener('click', () => rfb?.sendCtrlAltDel());
$('paste').addEventListener('click', () => post({ type: 'pasteRequest' }));
$('reconnect').addEventListener('click', () => connect());
document.getElementById('bannerAction')?.addEventListener('click', () => post({ type: 'installFullAccess' }));

// Throughput readout, updated once a second — the main thing this prototype measures.
setInterval(() => {
  const fmt = (b: number) =>
    b >= 1e6 ? `${(b / 1e6).toFixed(1)} MB/s` : b >= 1e3 ? `${(b / 1e3).toFixed(0)} KB/s` : `${b} B/s`;
  const fps = stats.serverFps !== undefined ? ` · ${stats.serverFps} fps` : '';
  statsEl.textContent = channel?.readyState === 'open'
    ? `↓ ${fmt(stats.down)} (${stats.msgs} msg/s) · ↑ ${fmt(stats.up)}${fps}`
    : '';
  stats.down = stats.up = stats.msgs = 0;
  requestStreaming(); // picks up remote resolution changes
}, 1000);

connect();
