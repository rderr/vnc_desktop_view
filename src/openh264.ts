// Cisco's OpenH264 binary: the software H.264 encoder for Linux remotes without
// a usable GPU encoder.
//
// Cisco's patent license covers its own binary only if it's downloaded
// separately to the user's machine (here: the remote, straight from Cisco), the
// user can enable and disable it, and "OpenH264 Video Codec provided by Cisco
// Systems, Inc." is shown where they do; see ThirdPartyNotices.txt. So it's
// never bundled: the user is asked first, and the `vncDesktopView.openH264`
// setting turns it on or off.

import * as crypto from 'crypto';
import * as fs from 'fs';
import * as https from 'https';
import * as path from 'path';
import { spawn } from 'child_process';
import * as vscode from 'vscode';

export const CISCO_NOTICE = 'OpenH264 Video Codec provided by Cisco Systems, Inc.';

/** The release the desktop server is built for (its bindings match this version). */
const RELEASE = {
  file: 'libopenh264-2.6.0-linux64.8.so',
  url: 'https://ciscobinary.openh264.org/libopenh264-2.6.0-linux64.8.so.bz2',
  downloadSha256: '27ab53323c110b76214c1c72222f459d17febbcd1e252136cadc292b0308d75b',
  sha256: '2f0cde7c6a6abcf5cae76942894ea42897fa677bce4ed6c91a24dd1b041d5f04',
};

/** ask: offer it when there's no GPU encoder; on: use it when there's none; always: use it even when there is; off: never. */
export type OpenH264Setting = 'ask' | 'on' | 'always' | 'off';

export function openH264Setting(): OpenH264Setting {
  const v = vscode.workspace.getConfiguration('vncDesktopView').get<string>('openH264', 'ask');
  return v === 'on' || v === 'always' || v === 'off' ? v : 'ask';
}

export function setOpenH264Setting(value: OpenH264Setting): Thenable<void> {
  return vscode.workspace.getConfiguration('vncDesktopView').update('openH264', value, vscode.ConfigurationTarget.Global);
}

/** Only Linux x64 has no built-in software fallback (Windows has its own encoder). */
export function openH264Applies(): boolean {
  return process.platform === 'linux' && process.arch === 'x64';
}

/** Where the library is (or will be) kept on this machine. */
export function openH264Path(storageDir: string): string {
  return path.join(storageDir, 'openh264', RELEASE.file);
}

/** Downloads the library from Cisco (once), checking both the download and the result against pinned hashes. */
export async function downloadOpenH264(storageDir: string, log: (m: string) => void): Promise<string> {
  const target = openH264Path(storageDir);
  if (fs.existsSync(target)) return target;
  log(`openh264: downloading ${RELEASE.url}`);
  const compressed = await get(RELEASE.url);
  if (sha256(compressed) !== RELEASE.downloadSha256) {
    throw new Error('the OpenH264 download from Cisco failed verification (unexpected SHA-256)');
  }
  const library = await bunzip2(compressed);
  if (sha256(library) !== RELEASE.sha256) {
    throw new Error('the unpacked OpenH264 library failed verification (unexpected SHA-256)');
  }
  fs.mkdirSync(path.dirname(target), { recursive: true });
  const tmp = `${target}.${process.pid}.tmp`;
  fs.writeFileSync(tmp, library, { mode: 0o644 });
  fs.renameSync(tmp, target);
  log(`openh264: saved ${target}`);
  return target;
}

function sha256(data: Buffer): string {
  return crypto.createHash('sha256').update(data).digest('hex');
}

function get(url: string, redirects = 5): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    const req = https.get(url, { timeout: 30_000 }, (res) => {
      const status = res.statusCode ?? 0;
      if (status >= 300 && status < 400 && res.headers.location && redirects > 0) {
        res.resume();
        resolve(get(new URL(res.headers.location, url).toString(), redirects - 1));
        return;
      }
      if (status !== 200) {
        res.resume();
        reject(new Error(`downloading OpenH264 from Cisco failed (HTTP ${status})`));
        return;
      }
      const chunks: Buffer[] = [];
      res.on('data', (c: Buffer) => chunks.push(c));
      res.on('end', () => resolve(Buffer.concat(chunks)));
      res.on('error', reject);
    });
    req.on('timeout', () => req.destroy(new Error('downloading OpenH264 from Cisco timed out')));
    req.on('error', reject);
  });
}

/** Cisco ships the library bzip2-compressed; Node can't unpack that, the system's bzip2 can. */
function bunzip2(data: Buffer): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    const proc = spawn('bzip2', ['-dc'], { stdio: ['pipe', 'pipe', 'pipe'] });
    const out: Buffer[] = [];
    let err = '';
    proc.stdout.on('data', (c: Buffer) => out.push(c));
    proc.stderr.on('data', (c: Buffer) => { err += c.toString(); });
    proc.on('error', (e) => reject(new Error(`unpacking OpenH264 needs the bzip2 program (${e.message})`)));
    proc.on('exit', (code) => code === 0 ? resolve(Buffer.concat(out)) : reject(new Error(`bzip2 failed: ${err.trim() || code}`)));
    proc.stdin.on('error', () => undefined); // reported through 'exit'
    proc.stdin.end(data);
  });
}
