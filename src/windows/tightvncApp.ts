// Runs TightVNC Server in *application mode* on Windows: no installer, no
// service, no administrator rights. The extension downloads TightVNC once,
// unpacks it into its own storage folder, and runs tvnserver.exe only while a
// desktop tab is open — with a fresh random port and password every time.
//
// Limitation (Windows by design): an application-mode server runs as the user,
// so it can't see or control UAC prompts, the lock screen or the sign-in
// screen, and can't send Ctrl+Alt+Del. The optional full-access service
// (scripts/setup-windows.ps1) covers those.

import * as fs from 'fs';
import * as path from 'path';
import * as https from 'https';
import * as crypto from 'crypto';
import * as net from 'net';
import { execFile, spawn, type ChildProcess } from 'child_process';
import { ForegroundWatcher } from './foregroundWatcher';
// noVNC's DES implements VNC's bit ordering; Node's OpenSSL build has no DES.
// @ts-expect-error -- plain JS module without type declarations
import { DESECBCipher } from '../../node_modules/@novnc/novnc/core/crypto/des.js';

// Pinned download, verified by hash (same values winget publishes).
const TIGHTVNC_VERSION = '2.8.89';
const MSI_URL = `https://www.tightvnc.com/download/${TIGHTVNC_VERSION}/tightvnc-${TIGHTVNC_VERSION}-gpl-setup-64bit.msi`;
const MSI_SHA256 = 'dff8d7e40a81e0b86ba1d9d5cc0d4f22d08354c50c9cbdc0825bbb6ccfb1073c';

// Application mode always reads its settings from here.
const REG_KEY = 'HKCU\\Software\\TightVNC\\Server';
const OUR_MARKER = 'VSCodeRemoteDesktop'; // extra value so we can recognise our own settings

export interface AppServerLease {
  port: number;
  password: string;
}

export class TightVncAppServer {
  private child: ChildProcess | undefined;
  private lease: AppServerLease | undefined;
  private starting: Promise<AppServerLease> | undefined;
  private users = 0;
  /** Reports when Windows blocks input from this (normal-privilege) server. Runs only while the server does. */
  readonly inputWatcher: ForegroundWatcher;

  constructor(
    private readonly storageDir: string,
    watcherScript: string,
    private readonly log: (msg: string) => void,
  ) {
    this.inputWatcher = new ForegroundWatcher(watcherScript, log);
  }

  private get installDir(): string {
    return path.join(this.storageDir, `tightvnc-${TIGHTVNC_VERSION}`);
  }
  private get exe(): string {
    return path.join(this.installDir, 'PFiles', 'TightVNC', 'tvnserver.exe');
  }
  private get backupFile(): string {
    return path.join(this.storageDir, 'user-tightvnc-settings.reg');
  }

  isDownloaded(): boolean {
    return fs.existsSync(this.exe);
  }

  /** Start the server if needed and register one more user (desktop tab). */
  async acquire(): Promise<AppServerLease> {
    this.users++;
    try {
      if (this.lease && this.child && this.child.exitCode === null) return this.lease;
      this.starting ??= this.start().finally(() => (this.starting = undefined));
      return await this.starting;
    } catch (e) {
      this.users--;
      throw e;
    }
  }

  /** One user (tab) is done; stop the server when nobody is using it. */
  async release(): Promise<void> {
    this.users = Math.max(0, this.users - 1);
    if (this.users === 0) await this.stop();
  }

  private async start(): Promise<AppServerLease> {
    await this.ensureDownloaded();
    await this.stopStrayInstances();

    const port = await findFreePort();
    const password = randomPassword();
    await this.writeSettings(port, password);

    this.log(`starting ${this.exe} -run on 127.0.0.1:${port}`);
    const child = spawn(this.exe, ['-run'], { stdio: 'ignore', windowsHide: false });
    this.child = child;
    child.on('exit', (code) => {
      this.log(`tvnserver exited (${code})`);
      if (this.child === child) {
        this.child = undefined;
        this.lease = undefined;
        this.inputWatcher.stop();
        void this.restoreSettings();
      }
    });

    try {
      await waitForRfb(port, 15000);
    } catch (e) {
      await this.stop();
      throw new Error(`TightVNC did not start listening on port ${port}: ${(e as Error).message}`);
    }
    this.lease = { port, password };
    this.inputWatcher.start();
    return this.lease;
  }

  async stop(): Promise<void> {
    const child = this.child;
    this.child = undefined;
    this.lease = undefined;
    this.inputWatcher.stop();
    if (child && child.exitCode === null) {
      this.log('stopping tvnserver');
      await run(this.exe, ['-controlapp', '-shutdown'], 5000).catch(() => undefined);
      if (!(await waitForExit(child, 5000))) child.kill();
    }
    await this.restoreSettings();
  }

  // ---- download ----------------------------------------------------------

  private async ensureDownloaded(): Promise<void> {
    if (this.isDownloaded()) return;
    fs.mkdirSync(this.storageDir, { recursive: true });
    const tmp = fs.mkdtempSync(path.join(this.storageDir, 'download-'));
    const partial = `${this.installDir}.partial`;
    try {
      const msi = path.join(tmp, 'tightvnc.msi');
      this.log(`downloading ${MSI_URL}`);
      await download(MSI_URL, msi);
      const hash = crypto.createHash('sha256').update(fs.readFileSync(msi)).digest('hex');
      if (hash !== MSI_SHA256) throw new Error(`TightVNC download failed verification (sha256 ${hash}).`);

      // "Administrative install" just unpacks the files; it needs no admin rights.
      fs.rmSync(partial, { recursive: true, force: true });
      await run('msiexec.exe', ['/a', `"${msi}"`, '/qn', `TARGETDIR="${partial}"`], 120000, true);
      if (!fs.existsSync(path.join(partial, 'PFiles', 'TightVNC', 'tvnserver.exe'))) {
        throw new Error('Unpacking TightVNC failed (tvnserver.exe not found).');
      }
      fs.rmSync(path.join(partial, 'tightvnc.msi'), { force: true });
      fs.renameSync(partial, this.installDir);
      this.log(`unpacked to ${this.installDir}`);
    } finally {
      fs.rmSync(tmp, { recursive: true, force: true });
      fs.rmSync(partial, { recursive: true, force: true });
    }
  }

  // ---- other tvnserver instances -------------------------------------------

  private async stopStrayInstances(): Promise<void> {
    const procs = await listTvnServers();
    for (const p of procs) {
      if (samePath(p.path, this.exe)) {
        // Left over from an earlier VS Code session that ended abruptly.
        this.log(`stopping leftover tvnserver (pid ${p.pid})`);
        try { process.kill(p.pid); } catch { /* already gone */ }
      } else if (/\s-run\b/i.test(p.commandLine) && p.sessionId === currentSessionId(procs)) {
        // The user runs TightVNC in application mode themselves. It shares
        // our settings location, so don't interfere with it.
        throw new Error(
          'TightVNC is already running in application mode for your account. Close it from its tray icon, ' +
          'or set "remoteDesktop.server" to "custom" to connect to it instead.');
      }
    }
  }

  // ---- settings (HKCU) -----------------------------------------------------

  private async writeSettings(port: number, password: string): Promise<void> {
    const exists = await regOk(['query', REG_KEY]);
    if (exists) {
      const ours = await regOk(['query', REG_KEY, '/v', OUR_MARKER]);
      if (!ours && !fs.existsSync(this.backupFile)) {
        // The user has their own application-mode settings: keep a copy to put back afterwards.
        await reg(['export', REG_KEY, this.backupFile, '/y']);
        this.log(`backed up existing TightVNC settings to ${this.backupFile}`);
      }
      await reg(['delete', REG_KEY, '/f']);
    }
    const dwords: Record<string, number> = {
      RfbPort: port,
      LoopbackOnly: 1,            // only accept connections from this machine
      AllowLoopback: 1,           // ...and do accept those (off by default)
      AcceptRfbConnections: 1,
      AcceptHttpConnections: 0,   // no built-in Java web viewer
      UseVncAuthentication: 1,
      UseControlAuthentication: 0,
      EnableFileTransfers: 0,
      RemoveWallpaper: 0,
      AlwaysShared: 1,            // several tabs can share the desktop
      NeverShared: 0,
      DisconnectClients: 0,
      [OUR_MARKER]: 1,
    };
    for (const [name, value] of Object.entries(dwords)) {
      await reg(['add', REG_KEY, '/v', name, '/t', 'REG_DWORD', '/d', String(value), '/f']);
    }
    await reg(['add', REG_KEY, '/v', 'Password', '/t', 'REG_BINARY', '/d', obfuscateVncPassword(password).toString('hex'), '/f']);
  }

  private async restoreSettings(): Promise<void> {
    try {
      if (await regOk(['query', REG_KEY, '/v', OUR_MARKER])) await reg(['delete', REG_KEY, '/f']);
      if (fs.existsSync(this.backupFile)) {
        await reg(['import', this.backupFile]);
        fs.rmSync(this.backupFile, { force: true });
        this.log('restored the previous TightVNC settings');
      }
    } catch (e) {
      this.log(`could not restore TightVNC settings: ${(e as Error).message}`);
    }
  }
}

// ---- helpers ----------------------------------------------------------------

/** VNC stores passwords DES-encrypted with a fixed, well-known key (obfuscation, not secrecy). */
export function obfuscateVncPassword(password: string): Buffer {
  const plain = new Uint8Array(8);
  plain.set(Buffer.from(password, 'latin1').subarray(0, 8));
  const cipher = DESECBCipher.importKey(new Uint8Array([23, 82, 107, 6, 35, 78, 88, 7]));
  return Buffer.from(cipher.encrypt(null, plain));
}

function randomPassword(): string {
  const chars = 'abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789';
  return Array.from({ length: 8 }, () => chars[crypto.randomInt(chars.length)]).join(''); // VNC uses at most 8
}

function findFreePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.once('error', reject);
    srv.listen(0, '127.0.0.1', () => {
      const port = (srv.address() as net.AddressInfo).port;
      srv.close(() => resolve(port));
    });
  });
}

/** Resolves once something on the port answers with an RFB greeting. */
async function waitForRfb(port: number, timeoutMs: number): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  let lastError = 'timed out';
  while (Date.now() < deadline) {
    try {
      const greeting = await new Promise<string>((resolve, reject) => {
        const s = net.connect(port, '127.0.0.1');
        s.setTimeout(2000, () => { s.destroy(); reject(new Error('no greeting')); });
        s.once('data', (d) => { s.destroy(); resolve(d.toString('latin1')); });
        s.once('error', reject);
      });
      if (greeting.startsWith('RFB ')) return;
      lastError = `unexpected greeting ${JSON.stringify(greeting.slice(0, 12))}`;
    } catch (e) {
      lastError = (e as Error).message;
    }
    await new Promise((r) => setTimeout(r, 300));
  }
  throw new Error(lastError);
}

function waitForExit(child: ChildProcess, timeoutMs: number): Promise<boolean> {
  if (child.exitCode !== null) return Promise.resolve(true);
  return new Promise((resolve) => {
    const t = setTimeout(() => resolve(false), timeoutMs);
    child.once('exit', () => { clearTimeout(t); resolve(true); });
  });
}

function run(file: string, args: string[], timeout: number, verbatim = false): Promise<string> {
  return new Promise((resolve, reject) => {
    execFile(file, args, { timeout, windowsHide: true, windowsVerbatimArguments: verbatim }, (err, stdout, stderr) => {
      if (err) reject(new Error(`${path.basename(file)} failed: ${stderr || err.message}`.trim()));
      else resolve(stdout);
    });
  });
}

const reg = (args: string[]) => run('reg.exe', args, 15000);
const regOk = (args: string[]) => reg(args).then(() => true, () => false);

interface TvnProcess { pid: number; sessionId: number; path: string; commandLine: string }

async function listTvnServers(): Promise<TvnProcess[]> {
  const script =
    `@(Get-CimInstance Win32_Process -Filter "Name='tvnserver.exe' or ProcessId=${process.pid}" | ` +
    `Select-Object ProcessId, SessionId, ExecutablePath, CommandLine) | ConvertTo-Json -Compress`;
  const out = await run('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', script], 20000);
  const rows = out.trim() ? JSON.parse(out) : [];
  return (Array.isArray(rows) ? rows : [rows]).map((r: any) => ({
    pid: r.ProcessId, sessionId: r.SessionId, path: r.ExecutablePath ?? '', commandLine: r.CommandLine ?? '',
  }));
}

/** Our own (extension host) process is included in the listing to learn the current session. */
function currentSessionId(procs: TvnProcess[]): number | undefined {
  return procs.find((p) => p.pid === process.pid)?.sessionId;
}

function samePath(a: string, b: string): boolean {
  return path.resolve(a).toLowerCase() === path.resolve(b).toLowerCase();
}

function download(url: string, dest: string, redirects = 5): Promise<void> {
  return new Promise((resolve, reject) => {
    https.get(url, { headers: { 'User-Agent': 'vscode-remote-desktop' } }, (res) => {
      if (res.statusCode && res.statusCode >= 300 && res.statusCode < 400 && res.headers.location && redirects > 0) {
        res.resume();
        download(new URL(res.headers.location, url).toString(), dest, redirects - 1).then(resolve, reject);
        return;
      }
      if (res.statusCode !== 200) {
        res.resume();
        reject(new Error(`Download failed: HTTP ${res.statusCode}`));
        return;
      }
      const file = fs.createWriteStream(dest);
      res.pipe(file);
      file.on('finish', () => file.close(() => resolve()));
      file.on('error', reject);
    }).on('error', reject);
  });
}
