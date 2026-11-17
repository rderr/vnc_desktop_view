// Video-preview viewer: decodes the custom server's H.264 with WebCodecs and
// draws it on a canvas. Runs in the editor tab, on the user's own machine.

import type { VideoToExtension, VideoToWebview } from '../videoProtocol';
import { scanCode } from './scancodes';

declare function acquireVsCodeApi(): { postMessage(msg: unknown): void };
const vscode = acquireVsCodeApi();
const post = (msg: VideoToExtension) => vscode.postMessage(msg);

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const canvas = $<HTMLCanvasElement>('canvas');
const ctx = canvas.getContext('2d')!;
const statusEl = $('status');
const statsEl = $('stats');
const screenEl = $('screen');
const fitBtn = $<HTMLButtonElement>('fit');

let decoder: VideoDecoder | undefined;
let codec = '';
/** After an error we must wait for a keyframe before decoding again. */
let needKey = true;
const stats = { bytes: 0, frames: 0, decodeMs: 0 };
const submitted = new Map<number, number>(); // timestamp -> performance.now() at submit

function setStatus(text: string): void {
  statusEl.textContent = text;
}

function newDecoder(): void {
  decoder?.close();
  decoder = new VideoDecoder({
    output: (frame) => {
      if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
        canvas.width = frame.displayWidth;
        canvas.height = frame.displayHeight;
      }
      ctx.drawImage(frame, 0, 0);
      const t0 = submitted.get(frame.timestamp);
      if (t0 !== undefined) {
        stats.decodeMs = Math.max(stats.decodeMs, performance.now() - t0);
        submitted.delete(frame.timestamp);
      }
      frame.close();
      stats.frames++;
    },
    error: (e) => {
      post({ type: 'log', text: `video preview: decoder error: ${e.message}` });
      setStatus(`Decoder error: ${e.message}`);
      needKey = true;
      newDecoder();
      post({ type: 'keyframe' });
    },
  });
  decoder.configure({ codec, optimizeForLatency: true, hardwareAcceleration: 'no-preference' });
}

window.addEventListener('message', (event: MessageEvent<VideoToWebview>) => {
  const msg = event.data;
  switch (msg.type) {
    case 'hello':
      codec = msg.codec;
      needKey = true;
      newDecoder();
      setStatus(`Connected · ${msg.width}×${msg.height}`);
      break;
    case 'video': {
      const data = msg.data instanceof Uint8Array ? msg.data : new Uint8Array(msg.data as ArrayBufferLike);
      stats.bytes += data.byteLength;
      if (decoder && decoder.state === 'configured' && (!needKey || msg.key)) {
        needKey = false;
        submitted.set(msg.timestamp, performance.now());
        try {
          decoder.decode(new EncodedVideoChunk({ type: msg.key ? 'key' : 'delta', timestamp: msg.timestamp, data }));
        } catch (e) {
          post({ type: 'log', text: `video preview: decode() threw: ${(e as Error).message}` });
          needKey = true;
          post({ type: 'keyframe' });
        }
      }
      post({ type: 'ack', bytes: data.byteLength });
      break;
    }
    case 'status':
      setStatus(msg.text);
      break;
  }
});

// ---- Input -------------------------------------------------------------

/** Converts a mouse position to desktop pixels, allowing for Fit scaling (object-fit: contain). */
function toDesktop(e: MouseEvent): { x: number; y: number } {
  const r = canvas.getBoundingClientRect();
  const scale = Math.min(r.width / canvas.width, r.height / canvas.height) || 1;
  const offX = (r.width - canvas.width * scale) / 2, offY = (r.height - canvas.height * scale) / 2;
  const x = (e.clientX - r.left - offX) / scale, y = (e.clientY - r.top - offY) / scale;
  return { x: Math.min(Math.max(x, 0), canvas.width - 1), y: Math.min(Math.max(y, 0), canvas.height - 1) };
}

let buttons = 0;
function sendPointer(e: MouseEvent, wheelX = 0, wheelY = 0): void {
  if (!canvas.width) return;
  const { x, y } = toDesktop(e);
  post({ type: 'pointer', x, y, buttons, wheelX, wheelY });
}

canvas.addEventListener('pointerdown', (e) => {
  canvas.focus();
  canvas.setPointerCapture(e.pointerId); // keep getting moves/ups while dragging outside the tab
  buttons = e.buttons;
  sendPointer(e);
  e.preventDefault();
});
canvas.addEventListener('pointerup', (e) => { buttons = e.buttons; sendPointer(e); e.preventDefault(); });
canvas.addEventListener('pointermove', (e) => { buttons = e.buttons; sendPointer(e); });
canvas.addEventListener('contextmenu', (e) => e.preventDefault());

// Browsers report wheel movement in pixels, lines or pages; Windows wants units of 120 per notch
// (about 3 lines, ~100 px). Fractions accumulate so slow trackpad scrolling isn't lost.
let wheelAccX = 0, wheelAccY = 0;
canvas.addEventListener('wheel', (e) => {
  e.preventDefault();
  const unit = e.deltaMode === WheelEvent.DOM_DELTA_LINE ? 40 : e.deltaMode === WheelEvent.DOM_DELTA_PAGE ? 120 * 3 : 1.2;
  wheelAccY -= e.deltaY * unit;
  wheelAccX += e.deltaX * unit;
  const wy = Math.trunc(wheelAccY), wx = Math.trunc(wheelAccX);
  if (!wy && !wx) return;
  wheelAccY -= wy;
  wheelAccX -= wx;
  sendPointer(e, wx, wy);
}, { passive: false });

const pressed = new Set<number>();
function sendKey(e: KeyboardEvent, down: boolean): void {
  const scan = scanCode(e.code);
  e.preventDefault();
  e.stopPropagation();
  if (scan === undefined) {
    if (down) post({ type: 'log', text: `video preview: no scan code for key "${e.code}"` });
    return;
  }
  if (down) pressed.add(scan);
  else pressed.delete(scan);
  post({ type: 'key', scan, down });
}
canvas.addEventListener('keydown', (e) => sendKey(e, true));
canvas.addEventListener('keyup', (e) => sendKey(e, false));
// Losing focus mid-press would leave keys stuck down on the remote.
canvas.addEventListener('blur', () => {
  for (const scan of pressed) post({ type: 'key', scan, down: false });
  pressed.clear();
});

fitBtn.addEventListener('click', () => {
  const fit = screenEl.classList.toggle('fit');
  fitBtn.setAttribute('aria-pressed', String(fit));
});
$('restart').addEventListener('click', () => post({ type: 'ready' }));

setInterval(() => {
  const kb = stats.bytes / 1000;
  statsEl.textContent = `${stats.frames} fps · ${kb >= 1000 ? `${(kb / 1000).toFixed(1)} MB/s` : `${kb.toFixed(0)} KB/s`}` +
    (stats.frames ? ` · decode ≤${stats.decodeMs.toFixed(0)} ms` : '');
  stats.bytes = stats.frames = stats.decodeMs = 0;
}, 1000);

post({ type: 'ready' });
