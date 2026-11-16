// Headless check of the server side of the pipeline: runs the test VNC server
// and drives an RFB 3.8 handshake and two framebuffer updates against it,
// the same bytes noVNC would exchange through the relay.
import { createRequire } from 'module';
import * as net from 'net';
const require = createRequire(import.meta.url);
const { startTestServer } = require('../dist/testServer.js');

const server = await startTestServer('light');
const sock = net.connect(server.port, '127.0.0.1');
let buf = Buffer.alloc(0);
const waitFor = (n) => new Promise((resolve) => {
  const check = () => {
    if (buf.length >= n) { const out = buf.subarray(0, n); buf = buf.subarray(n); sock.off('data', on); resolve(out); }
  };
  const on = (c) => { buf = Buffer.concat([buf, c]); check(); };
  sock.on('data', on); check();
});
const assert = (cond, msg) => { if (!cond) { console.error('FAIL:', msg); process.exit(1); } console.log('ok -', msg); };

assert((await waitFor(12)).toString() === 'RFB 003.008\n', 'version greeting');
sock.write('RFB 003.008\n');
const sec = await waitFor(2);
assert(sec[0] === 1 && sec[1] === 1, 'offers security type None');
sock.write(Buffer.from([1]));
assert((await waitFor(4)).readUInt32BE(0) === 0, 'security result OK');
sock.write(Buffer.from([1])); // ClientInit, shared
const init = await waitFor(24);
const w = init.readUInt16BE(0), h = init.readUInt16BE(2);
await waitFor(init.readUInt32BE(20));
assert(w === 1280 && h === 720, `ServerInit ${w}x${h}`);

// Ask for noVNC's pixel format (RGBX little-endian: red shift 0, blue shift 16).
const spf = Buffer.alloc(20);
spf[0] = 0; spf[4] = 32; spf[5] = 24; spf[6] = 0; spf[7] = 1;
spf.writeUInt16BE(255, 8); spf.writeUInt16BE(255, 10); spf.writeUInt16BE(255, 12);
spf[14] = 0; spf[15] = 8; spf[16] = 16;
sock.write(spf);

const request = (incremental) => {
  const m = Buffer.alloc(10); m[0] = 3; m[1] = incremental ? 1 : 0;
  m.writeUInt16BE(w, 6); m.writeUInt16BE(h, 8); sock.write(m);
};
const readUpdate = async () => {
  const hdr = await waitFor(16);
  const rw = hdr.readUInt16BE(8), rh = hdr.readUInt16BE(10);
  await waitFor(rw * rh * 4);
  return { type: hdr[0], rects: hdr.readUInt16BE(2), rw, rh, enc: hdr.readInt32BE(12) };
};

request(false);
const full = await readUpdate();
assert(full.type === 0 && full.enc === 0 && full.rw === w && full.rh === h, 'full-screen update (raw)');
request(true);
const inc = await readUpdate();
assert(inc.rw < w && inc.rh < h, `incremental update is a small rect (${inc.rw}x${inc.rh})`);

sock.destroy();
server.close();
console.log('all relay/test-server checks passed');
