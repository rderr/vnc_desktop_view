// Real-machine test of TightVNC application mode (Windows only, no admin):
// download + unpack, start, VNC password login, two users sharing one server,
// stop, and restoring a pre-existing HKCU settings key.
//
// Usage: node <bundled test> <storageDir>
import * as net from 'net';
import { execFileSync } from 'child_process';
import { TightVncAppServer } from '../src/windows/tightvncApp';
// @ts-expect-error -- plain JS module
import { DESECBCipher } from '../node_modules/@novnc/novnc/core/crypto/des.js';

const KEY = 'HKCU\\Software\\TightVNC\\Server';
const assert = (cond: unknown, msg: string) => {
  if (!cond) { console.error('FAIL:', msg); process.exit(1); }
  console.log('ok -', msg);
};
const regQuery = (args: string[]) => {
  try { return execFileSync('reg.exe', ['query', KEY, ...args], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] }); }
  catch { return null; }
};

/** RFB 3.8 handshake with VNC authentication; resolves true if the server accepts the password. */
function vncLogin(port: number, password: string): Promise<boolean> {
  return new Promise((resolve, reject) => {
    const s = net.connect(port, '127.0.0.1');
    let buf = Buffer.alloc(0);
    let step = 0;
    s.on('data', (d) => {
      buf = Buffer.concat([buf, d]);
      if (step === 0 && buf.length >= 12) { buf = buf.subarray(12); s.write('RFB 003.008\n'); step = 1; }
      if (step === 1 && buf.length >= 1 && buf.length >= 1 + buf[0]) {
        const types = [...buf.subarray(1, 1 + buf[0])];
        buf = buf.subarray(1 + buf[0]);
        if (!types.includes(2)) {
          s.destroy();
          reject(new Error(`no VNC auth offered; server says: ${buf.subarray(4).toString('latin1')}`));
          return;
        }
        s.write(Buffer.from([2])); step = 2;
      }
      if (step === 2 && buf.length >= 16) {
        const key = new Uint8Array(8); key.set(Buffer.from(password, 'latin1').subarray(0, 8));
        s.write(Buffer.from(DESECBCipher.importKey(key).encrypt(null, buf.subarray(0, 16))));
        buf = buf.subarray(16); step = 3;
      }
      if (step === 3 && buf.length >= 4) { s.destroy(); resolve(buf.readUInt32BE(0) === 0); }
    });
    s.on('error', reject);
  });
}

async function main(): Promise<void> {
  const storage = process.argv[2];
  const watcherScript = require('path').resolve(process.env.REPO ?? '.', 'scripts', 'foreground-watch.ps1');
  const server = new TightVncAppServer(storage, watcherScript, (m) => console.log('   [app]', m));
  const watcherStates: string[] = [];
  server.inputWatcher.subscribe((s) => watcherStates.push(JSON.stringify(s)));
  const watcherRunning = () => execFileSync('powershell.exe', ['-NoProfile', '-Command',
    // Exclude this checking process itself: its own command line mentions the script name.
    `@(Get-CimInstance Win32_Process -Filter "Name='powershell.exe'" | Where-Object { $_.ProcessId -ne $PID -and $_.CommandLine -like '*-File*foreground-watch.ps1*' }).Count`],
    { encoding: 'utf8' }).trim() !== '0';

  // Pretend the user already has their own application-mode settings.
  execFileSync('reg.exe', ['add', KEY, '/v', 'UserOwnSetting', '/t', 'REG_DWORD', '/d', '42', '/f'], { stdio: 'ignore' });

  const t0 = Date.now();
  const lease = await server.acquire();
  console.log(`   started in ${Date.now() - t0} ms on port ${lease.port}`);
  assert(regQuery(['/v', 'UserOwnSetting']) === null, 'our settings replaced the user\'s while running');
  assert(await vncLogin(lease.port, lease.password), 'VNC login with the generated password');
  await new Promise((r) => setTimeout(r, 4000));
  assert(watcherRunning(), 'foreground watcher runs alongside the server');
  console.log(`   watcher states so far: ${watcherStates.join(' ')}`);

  const lease2 = await server.acquire();
  assert(lease2.port === lease.port, 'second tab shares the same server');
  await server.release();
  assert(await vncLogin(lease.port, lease.password), 'still running while one tab remains');

  // Last, because TightVNC then briefly blocks further attempts from 127.0.0.1.
  assert(!(await vncLogin(lease.port, 'wrongpw1')), 'wrong password rejected');
  const blocked = await vncLogin(lease.port, lease.password).then(() => 'no', (e: Error) => e.message);
  console.log(`   right after a failure, correct password: ${blocked}`);

  await server.release();
  await new Promise((r) => setTimeout(r, 500));
  const stillListening = await new Promise<boolean>((resolve) => {
    const s = net.connect(lease.port, '127.0.0.1');
    s.on('connect', () => { s.destroy(); resolve(true); });
    s.on('error', () => resolve(false));
  });
  assert(!stillListening, 'server stopped after the last tab closed');
  assert(regQuery(['/v', 'UserOwnSetting'])?.includes('0x2a'), 'user\'s original settings restored');
  assert(regQuery(['/v', 'VSCodeRemoteDesktop']) === null, 'no leftover extension settings');
  await new Promise((r) => setTimeout(r, 1000));
  assert(!watcherRunning(), 'foreground watcher stopped with the server');

  execFileSync('reg.exe', ['delete', KEY, '/f'], { stdio: 'ignore' });
  console.log('all application-mode checks passed');
}

main().catch((e) => { console.error(e); process.exit(1); });
