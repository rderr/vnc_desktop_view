import * as net from 'net';
import * as zlib from 'zlib';

/**
 * A tiny fake VNC (RFB 3.8) server for testing the viewer and relay without
 * installing a real VNC server. No authentication. Uses zlib encoding when the
 * client supports it (as real servers compress), raw otherwise.
 *
 *  - "light"  mode: static background, a bouncing square, and you can draw by
 *                   dragging the mouse. Sends only changed rectangles, like a
 *                   typical desktop.
 *  - "stress" mode: the whole screen changes every frame (worst case, like
 *                   full-screen video) to measure maximum throughput.
 *
 * Keys change the square's colour, so you can see that keyboard input works.
 * Can also be run standalone: `node dist/testServer.js [port] [light|stress]` (see testServerCli.ts).
 */
export type TestMode = 'light' | 'stress';

export interface TestServer {
  port: number;
  close(): void;
}

const WIDTH = 1280;
const HEIGHT = 720;
const BOX = 120;
const MAX_FPS = 60;

interface PixelFormat {
  bpp: number;
  bigEndian: boolean;
  rMax: number; gMax: number; bMax: number;
  rShift: number; gShift: number; bShift: number;
}

export function startTestServer(
  mode: TestMode,
  onFps: (fps: number) => void = () => {},
  port = 0,
): Promise<TestServer> {
  const server = net.createServer((sock) => handleClient(sock, mode, onFps));
  return new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(port, '127.0.0.1', () => {
      const addr = server.address() as net.AddressInfo;
      resolve({ port: addr.port, close: () => server.close() });
    });
  });
}

function handleClient(sock: net.Socket, mode: TestMode, onFps: (fps: number) => void): void {
  sock.setNoDelay(true);

  // Logical framebuffer as packed 0xRRGGBB.
  const fb = new Uint32Array(WIDTH * HEIGHT);
  for (let y = 0; y < HEIGHT; y++) {
    for (let x = 0; x < WIDTH; x++) {
      const r = Math.floor((x / WIDTH) * 60) + 20;
      const g = Math.floor((y / HEIGHT) * 60) + 30;
      fb[y * WIDTH + x] = (r << 16) | (g << 8) | 70;
    }
  }

  let pf: PixelFormat = {
    bpp: 32, bigEndian: false, rMax: 255, gMax: 255, bMax: 255, rShift: 16, gShift: 8, bShift: 0,
  };
  let buf = Buffer.alloc(0);
  let state: 'version' | 'security' | 'init' | 'normal' = 'version';
  let useZlib = false;
  // One zlib stream for the whole connection, as the RFB zlib encoding requires.
  const deflate = zlib.createDeflate({ level: 1 });
  let deflated: Buffer[] = [];
  deflate.on('data', (c: Buffer) => deflated.push(c));
  let sending = false;

  // Animation state
  let t = 0;
  let boxX = 100, boxY = 100, dx = 7, dy = 5;
  let boxColor = 0xe8a33c;
  let buttonDown = false;
  let dirty: Rect | null = { x: 0, y: 0, w: WIDTH, h: HEIGHT };
  let fullRequested = true;
  let updateRequested = false;
  let lastFrame = 0;
  let timer: NodeJS.Timeout | undefined;
  let framesThisSecond = 0;
  const fpsTimer = setInterval(() => { onFps(framesThisSecond); framesThisSecond = 0; }, 1000);

  sock.write('RFB 003.008\n');

  sock.on('data', (chunk) => {
    buf = Buffer.concat([buf, chunk]);
    try {
      while (processOne()) { /* keep parsing */ }
    } catch (err) {
      console.error('[test-server]', err);
      sock.destroy();
    }
  });
  sock.on('close', () => { clearInterval(fpsTimer); if (timer) clearTimeout(timer); deflate.close(); });
  sock.on('error', () => { /* client went away */ });

  /** Parses one message from `buf`. Returns false if more bytes are needed. */
  function processOne(): boolean {
    switch (state) {
      case 'version':
        if (buf.length < 12) return false;
        buf = buf.subarray(12);
        sock.write(Buffer.from([1, 1])); // one security type: None
        state = 'security';
        return true;
      case 'security':
        if (buf.length < 1) return false;
        buf = buf.subarray(1);
        sock.write(Buffer.from([0, 0, 0, 0])); // SecurityResult OK
        state = 'init';
        return true;
      case 'init': {
        if (buf.length < 1) return false;
        buf = buf.subarray(1);
        const name = Buffer.from(`Test desktop (${mode})`);
        const msg = Buffer.alloc(24 + name.length);
        msg.writeUInt16BE(WIDTH, 0);
        msg.writeUInt16BE(HEIGHT, 2);
        // pixel format: 32bpp, depth 24, little endian, true colour, 8 bits per channel
        msg.writeUInt8(32, 4); msg.writeUInt8(24, 5); msg.writeUInt8(0, 6); msg.writeUInt8(1, 7);
        msg.writeUInt16BE(255, 8); msg.writeUInt16BE(255, 10); msg.writeUInt16BE(255, 12);
        msg.writeUInt8(16, 14); msg.writeUInt8(8, 15); msg.writeUInt8(0, 16);
        msg.writeUInt32BE(name.length, 20);
        name.copy(msg, 24);
        sock.write(msg);
        state = 'normal';
        return true;
      }
      case 'normal':
        return clientMessage();
    }
  }

  function clientMessage(): boolean {
    if (buf.length < 1) return false;
    const type = buf[0];
    switch (type) {
      case 0: { // SetPixelFormat
        if (buf.length < 20) return false;
        pf = {
          bpp: buf[4], bigEndian: buf[6] !== 0,
          rMax: buf.readUInt16BE(8), gMax: buf.readUInt16BE(10), bMax: buf.readUInt16BE(12),
          rShift: buf[14], gShift: buf[15], bShift: buf[16],
        };
        if (pf.bpp !== 32) throw new Error(`test server only supports 32bpp, client asked for ${pf.bpp}`);
        buf = buf.subarray(20);
        return true;
      }
      case 2: { // SetEncodings
        if (buf.length < 4) return false;
        const n = buf.readUInt16BE(2);
        if (buf.length < 4 + 4 * n) return false;
        for (let i = 0; i < n; i++) if (buf.readInt32BE(4 + 4 * i) === 6) useZlib = true;
        buf = buf.subarray(4 + 4 * n);
        return true;
      }
      case 3: { // FramebufferUpdateRequest
        if (buf.length < 10) return false;
        if (buf[1] === 0) fullRequested = true;
        buf = buf.subarray(10);
        updateRequested = true;
        scheduleFrame();
        return true;
      }
      case 4: { // KeyEvent
        if (buf.length < 8) return false;
        if (buf[1] === 1) boxColor = (Math.random() * 0xffffff) | 0x404040;
        buf = buf.subarray(8);
        return true;
      }
      case 5: { // PointerEvent
        if (buf.length < 6) return false;
        buttonDown = (buf[1] & 1) !== 0;
        const x = buf.readUInt16BE(2), y = buf.readUInt16BE(4);
        buf = buf.subarray(6);
        if (buttonDown) paint(x, y);
        return true;
      }
      case 6: { // ClientCutText
        if (buf.length < 8) return false;
        const len = buf.readUInt32BE(4);
        if (buf.length < 8 + len) return false;
        console.log('[test-server] clipboard from client:', buf.subarray(8, 8 + len).toString('latin1'));
        buf = buf.subarray(8 + len);
        return true;
      }
      default:
        throw new Error(`unsupported client message type ${type}`);
    }
  }

  function paint(cx: number, cy: number): void {
    const r = 4;
    const x0 = Math.max(0, cx - r), y0 = Math.max(0, cy - r);
    const x1 = Math.min(WIDTH, cx + r), y1 = Math.min(HEIGHT, cy + r);
    for (let y = y0; y < y1; y++) for (let x = x0; x < x1; x++) fb[y * WIDTH + x] = 0xffffff;
    markDirty({ x: x0, y: y0, w: x1 - x0, h: y1 - y0 });
  }

  function markDirty(r: Rect): void {
    dirty = dirty ? union(dirty, r) : r;
  }

  function scheduleFrame(): void {
    if (timer) return;
    const wait = Math.max(0, 1000 / MAX_FPS - (Date.now() - lastFrame));
    timer = setTimeout(() => { timer = undefined; sendFrame(); }, wait);
  }

  function animate(): void {
    t++;
    if (mode === 'stress') {
      for (let y = 0; y < HEIGHT; y++) {
        for (let x = 0; x < WIDTH; x++) {
          fb[y * WIDTH + x] = (((x + t * 4) & 255) << 16) | (((y + t * 2) & 255) << 8) | (((x + y + t) >> 2) & 255);
        }
      }
      markDirty({ x: 0, y: 0, w: WIDTH, h: HEIGHT });
    }
    // Erase the old square, move it, draw the new one.
    const old = { x: boxX, y: boxY, w: BOX, h: BOX };
    if (mode === 'light') fillRect(old, 0x283246);
    boxX += dx; boxY += dy;
    if (boxX < 0 || boxX + BOX > WIDTH) { dx = -dx; boxX += 2 * dx; }
    if (boxY < 0 || boxY + BOX > HEIGHT) { dy = -dy; boxY += 2 * dy; }
    const now = { x: boxX, y: boxY, w: BOX, h: BOX };
    fillRect(now, boxColor);
    markDirty(union(old, now));
  }

  function fillRect(r: Rect, color: number): void {
    for (let y = r.y; y < r.y + r.h; y++) fb.fill(color, y * WIDTH + r.x, y * WIDTH + r.x + r.w);
  }

  function sendFrame(): void {
    if (!updateRequested || sock.destroyed || sending) return;
    animate();
    const rect = fullRequested ? { x: 0, y: 0, w: WIDTH, h: HEIGHT } : dirty!;
    updateRequested = false;
    fullRequested = false;
    dirty = null;
    lastFrame = Date.now();
    framesThisSecond++;

    const header = Buffer.alloc(16);
    header.writeUInt8(0, 0);            // FramebufferUpdate
    header.writeUInt16BE(1, 2);         // one rectangle
    header.writeUInt16BE(rect.x, 4);
    header.writeUInt16BE(rect.y, 6);
    header.writeUInt16BE(rect.w, 8);
    header.writeUInt16BE(rect.h, 10);
    const pixels = encode(rect);
    if (!useZlib) {
      header.writeInt32BE(0, 12);       // raw encoding
      sock.write(header);
      sock.write(pixels);
      return;
    }
    // zlib encoding: u32 length + bytes from the connection's deflate stream.
    // Compression is async, so hold further frames until this one is written.
    header.writeInt32BE(6, 12);
    sending = true;
    deflate.write(pixels);
    deflate.flush(zlib.constants.Z_SYNC_FLUSH, () => {
      sending = false;
      if (sock.destroyed) return;
      const body = Buffer.concat(deflated);
      deflated = [];
      const len = Buffer.alloc(4);
      len.writeUInt32BE(body.length, 0);
      sock.write(Buffer.concat([header, len, body]));
      if (updateRequested) scheduleFrame();
    });
  }

  function encode(r: Rect): Buffer {
    const out = Buffer.alloc(r.w * r.h * 4);
    const scale = (v: number, max: number) => (max === 255 ? v : Math.round((v * max) / 255));
    let o = 0;
    for (let y = r.y; y < r.y + r.h; y++) {
      for (let x = r.x; x < r.x + r.w; x++) {
        const c = fb[y * WIDTH + x];
        const v = ((scale((c >> 16) & 255, pf.rMax) << pf.rShift) |
                   (scale((c >> 8) & 255, pf.gMax) << pf.gShift) |
                   (scale(c & 255, pf.bMax) << pf.bShift)) >>> 0;
        if (pf.bigEndian) out.writeUInt32BE(v, o); else out.writeUInt32LE(v, o);
        o += 4;
      }
    }
    return out;
  }
}

interface Rect { x: number; y: number; w: number; h: number }

function union(a: Rect, b: Rect): Rect {
  const x = Math.max(0, Math.min(a.x, b.x)), y = Math.max(0, Math.min(a.y, b.y));
  const x2 = Math.min(WIDTH, Math.max(a.x + a.w, b.x + b.w)), y2 = Math.min(HEIGHT, Math.max(a.y + a.h, b.y + b.h));
  return { x, y, w: x2 - x, h: y2 - y };
}
