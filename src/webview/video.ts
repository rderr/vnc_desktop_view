// Video-preview viewer: decodes the custom server's H.264 with WebCodecs and
// draws it on a canvas. Runs in the editor tab, on the user's own machine.

import type { VideoToExtension, VideoToWebview } from '../videoProtocol';

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
