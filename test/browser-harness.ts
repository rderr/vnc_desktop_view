// End-to-end check of the viewer outside VS Code:
//   headless Edge/Chrome  ->  dist/webview.js (noVNC + PostMessageChannel)
//   -> stand-in for VS Code's webview messaging (a WebSocket)
//   -> Relay  ->  test VNC server.
// Then drives the mouse/keyboard through the DevTools protocol, reads the
// toolbar status, and saves a screenshot.
//
// Usage: node <bundled harness> <browser.exe> <outDir> [light|stress]
import * as http from 'http';
import * as fs from 'fs';
import * as path from 'path';
import { spawn } from 'child_process';
import { WebSocketServer } from 'ws';
import { Relay } from '../src/relay';
import { startTestServer, type TestMode } from '../src/testServer';

const [browserPath, outDir, modeArg] = process.argv.slice(2);
const mode = (modeArg ?? 'light') as TestMode;
const root = path.resolve(__dirname, '..', '..', 'remote-desktop');
const webviewJs = fs.readFileSync(process.env.WEBVIEW_JS ?? path.join(root, 'dist', 'webview.js'));

const page = `<!DOCTYPE html><html><head><meta charset="utf-8">
<style>html,body{height:100%;margin:0}body{display:flex;flex-direction:column;font:13px sans-serif}
#screen{flex:1;min-height:0;position:relative}#stats{margin-left:12px}</style>
<script>
  // Stand-in for VS Code's acquireVsCodeApi(): bridges postMessage over a WebSocket.
  const ws = new WebSocket('ws://' + location.host + '/bridge');
  ws.binaryType = 'arraybuffer';
  const queue = [];
  const enc = (m) => JSON.stringify(m, (k, v) => v instanceof Uint8Array ? { __b64: btoa(String.fromCharCode(...v)) } : v);
  ws.onopen = () => { queue.forEach((m) => ws.send(m)); queue.length = 0; };
  ws.onmessage = (e) => {
    const msg = JSON.parse(e.data, (k, v) => v && v.__b64 !== undefined ? Uint8Array.from(atob(v.__b64), (c) => c.charCodeAt(0)) : v);
    window.dispatchEvent(new MessageEvent('message', { data: msg }));
  };
  window.acquireVsCodeApi = () => ({ postMessage: (m) => { const s = enc(m); ws.readyState === 1 ? ws.send(s) : queue.push(s); } });
</script></head><body>
<div id="toolbar"><button id="fit">Fit</button><button id="cad">CAD</button><button id="paste">Paste</button>
<button id="reconnect">Reconnect</button><span id="status"></span><span id="stats"></span></div>
<div id="screen"><div id="message" hidden></div></div>
<script type="module" src="/webview.js"></script></body></html>`;

function encode(m: unknown): string {
  return JSON.stringify(m, (_k, v) => (v instanceof Uint8Array ? { __b64: Buffer.from(v).toString('base64') } : v));
}
function decode(s: string): any {
  return JSON.parse(s, (_k, v) => (v && v.__b64 !== undefined ? new Uint8Array(Buffer.from(v.__b64, 'base64')) : v));
}

async function main(): Promise<void> {
  let bridgeSend: (m: unknown) => void = () => {};
  const vnc = await startTestServer(mode, (fps) => bridgeSend({ type: 'serverStats', fps }));

  const httpServer = http.createServer((req, res) => {
    if (req.url === '/webview.js') { res.writeHead(200, { 'content-type': 'text/javascript' }); res.end(webviewJs); return; }
    res.writeHead(200, { 'content-type': 'text/html' }); res.end(page);
  });
  const wss = new WebSocketServer({ server: httpServer, path: '/bridge' });
  // Simulated tunnel: LATENCY_MS one-way delay in each direction.
  const latency = Number(process.env.LATENCY_MS ?? 0);
  const streaming = process.env.STREAM !== '0';
  const later = (fn: () => void) => (latency ? setTimeout(fn, latency) : fn());
  wss.on('connection', (ws) => {
    let relay: Relay | undefined;
    let conn = -1;
    bridgeSend = (m) => later(() => ws.send(encode(m)));
    ws.on('message', (raw) => later(() => {
      const msg = decode(raw.toString());
      switch (msg.type) {
        case 'ready': {
          relay?.dispose();
          conn = msg.conn;
          const c = conn;
          bridgeSend({ type: 'init', scaleToFit: true, label: 'harness' });
          relay = new Relay('127.0.0.1', vnc.port, {
            open: () => bridgeSend({ type: 'open', conn: c }),
            send: (data) => { bridgeSend({ type: 'data', conn: c, data }); return Promise.resolve(); },
            closed: (reason) => bridgeSend({ type: 'closed', conn: c, reason }),
          });
          break;
        }
        case 'data': if (msg.conn === conn) relay?.write(msg.data); break;
        case 'close': if (msg.conn === conn) relay?.dispose(); break;
        case 'ack': if (msg.conn === conn) relay?.ack(msg.bytes); break;
        case 'stream': if (msg.conn === conn && streaming) relay?.stream(msg.width, msg.height); break;
        default: console.log('[harness] viewer ->', msg.type, msg.desktopName ?? '');
      }
    }));
  });
  await new Promise<void>((r) => httpServer.listen(0, '127.0.0.1', () => r()));
  const url = `http://127.0.0.1:${(httpServer.address() as any).port}/`;

  const debugPort = 9300 + Math.floor(Math.random() * 500);
  const profile = path.join(outDir, 'browser-profile');
  const browser = spawn(browserPath, [
    '--headless=new', `--remote-debugging-port=${debugPort}`, `--user-data-dir=${profile}`,
    '--window-size=1300,820', '--no-first-run', url,
  ], { stdio: 'ignore' });

  try {
    const target = await poll(async () => {
      const list = await (await fetch(`http://127.0.0.1:${debugPort}/json`)).json() as any[];
      return list.find((t) => t.type === 'page' && t.url.startsWith(url));
    });
    const cdp = await connectCdp(target.webSocketDebuggerUrl);
    const evalText = async (expr: string) => (await cdp('Runtime.evaluate', { expression: expr, returnByValue: true })).result.value;

    await poll(async () => (await evalText(`document.getElementById('status').textContent`)) === 'Connected' || undefined, 10000);
    console.log('status:', await evalText(`document.getElementById('status').textContent`));

    // Draw a diagonal line by dragging, then press a key (changes the square's colour).
    const canvas = await evalText(`(() => { const r = document.querySelector('#screen canvas').getBoundingClientRect(); return [r.x, r.y, r.width, r.height]; })()`);
    console.log('canvas rect:', canvas.map((n: number) => Math.round(n)).join(', '));
    const [cx, cy, cw, ch] = canvas;
    await cdp('Input.dispatchMouseEvent', { type: 'mousePressed', x: cx + cw * 0.3, y: cy + ch * 0.3, button: 'left', clickCount: 1 });
    for (let i = 0; i <= 40; i++) {
      await cdp('Input.dispatchMouseEvent', { type: 'mouseMoved', x: cx + cw * (0.3 + i * 0.01), y: cy + ch * (0.3 + i * 0.01), button: 'left', buttons: 1 });
    }
    await cdp('Input.dispatchMouseEvent', { type: 'mouseReleased', x: cx + cw * 0.7, y: cy + ch * 0.7, button: 'left', clickCount: 1 });
    await cdp('Input.dispatchKeyEvent', { type: 'keyDown', key: 'a', code: 'KeyA', text: 'a', windowsVirtualKeyCode: 65 });
    await cdp('Input.dispatchKeyEvent', { type: 'keyUp', key: 'a', code: 'KeyA', windowsVirtualKeyCode: 65 });

    await new Promise((r) => setTimeout(r, 3500));
    console.log('stats:', await evalText(`document.getElementById('stats').textContent`));
    const shot = await cdp('Page.captureScreenshot', { format: 'png' });
    const file = path.join(outDir, `viewer-${mode}.png`);
    fs.writeFileSync(file, Buffer.from(shot.data, 'base64'));
    console.log('screenshot:', file);
  } finally {
    browser.kill();
    httpServer.close();
    vnc.close();
    setTimeout(() => process.exit(0), 300);
  }
}

async function poll<T>(fn: () => Promise<T | undefined>, timeoutMs = 10000): Promise<T> {
  const end = Date.now() + timeoutMs;
  for (;;) {
    try { const v = await fn(); if (v) return v; } catch { /* not ready yet */ }
    if (Date.now() > end) throw new Error('timed out');
    await new Promise((r) => setTimeout(r, 200));
  }
}

async function connectCdp(wsUrl: string) {
  const ws = new WebSocket(wsUrl);
  await new Promise((r, j) => { ws.onopen = r; ws.onerror = j; });
  let id = 0;
  const waiting = new Map<number, (v: any) => void>();
  ws.onmessage = (e) => {
    const m = JSON.parse(String(e.data));
    if (m.id && waiting.has(m.id)) { waiting.get(m.id)!(m.error ? Promise.reject(new Error(m.error.message)) : m.result); waiting.delete(m.id); }
  };
  return (method: string, params: object = {}) => new Promise<any>((resolve) => {
    const i = ++id; waiting.set(i, resolve); ws.send(JSON.stringify({ id: i, method, params }));
  });
}

main().catch((e) => { console.error(e); process.exit(1); });
