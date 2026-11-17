// Remembered VNC passwords, kept on the REMOTE machine (where the extension
// runs) rather than in VS Code's secret storage, which lives on whichever
// computer you connect *from*. That way every computer that can reach the
// remote with your login (e.g. your Remote Tunnel's GitHub account) can use
// them, and the password never leaves the remote machine.
//
//  - Windows: encrypted with DPAPI (CurrentUser), so only this Windows account
//    can decrypt it. Done via PowerShell, since Node has no DPAPI binding; the
//    data travels over stdin, never on a command line.
//  - macOS / Linux: a JSON file readable only by this account (mode 0600).

import * as fs from 'fs';
import * as path from 'path';
import { spawn } from 'child_process';

export interface StoredCredentials {
  username?: string;
  password: string;
}

type Entries = Record<string, StoredCredentials>;

// Extra DPAPI entropy so other programs using DPAPI can't trivially decrypt our blob by accident.
const ENTROPY_B64 = Buffer.from('vnc-desktop-view/passwords/v1').toString('base64');

export class PasswordStore {
  private readonly file: string;
  private queue: Promise<unknown> = Promise.resolve();
  /** Decrypted contents, valid while the file's modification time is unchanged (another window may write it). */
  private cache: { mtimeMs: number; entries: Entries } | undefined;

  constructor(private readonly dir: string) {
    this.file = path.join(dir, process.platform === 'win32' ? 'passwords.dat' : 'passwords.json');
  }

  static key(host: string, port: number): string {
    return `${host}:${port}`;
  }

  get(key: string): Promise<StoredCredentials | undefined> {
    return this.serialized(async () => (await this.load())[key]);
  }

  set(key: string, creds: StoredCredentials): Promise<void> {
    return this.serialized(async () => {
      const all = await this.load();
      all[key] = creds;
      await this.save(all);
    });
  }

  delete(key: string): Promise<void> {
    return this.serialized(async () => {
      const all = await this.load();
      if (!(key in all)) return;
      delete all[key];
      await this.save(all);
    });
  }

  /** Read-modify-write operations run one at a time. */
  private serialized<T>(op: () => Promise<T>): Promise<T> {
    const next = this.queue.then(op, op);
    this.queue = next.catch(() => undefined);
    return next;
  }

  private async load(): Promise<Entries> {
    if (!fs.existsSync(this.file)) return {};
    const mtimeMs = fs.statSync(this.file).mtimeMs;
    if (this.cache?.mtimeMs === mtimeMs) return { ...this.cache.entries };
    const raw = fs.readFileSync(this.file, 'utf8').trim();
    const json = !raw ? '{}' : process.platform === 'win32' ? await dpapi('unprotect', raw) : raw;
    const entries = JSON.parse(json) as Entries;
    this.cache = { mtimeMs, entries };
    return { ...entries };
  }

  private async save(all: Entries): Promise<void> {
    fs.mkdirSync(this.dir, { recursive: true });
    const json = JSON.stringify(all);
    const tmp = `${this.file}.tmp`;
    if (process.platform === 'win32') {
      fs.writeFileSync(tmp, await dpapi('protect', json));
    } else {
      fs.writeFileSync(tmp, json, { mode: 0o600 });
    }
    fs.renameSync(tmp, this.file);
    this.cache = { mtimeMs: fs.statSync(this.file).mtimeMs, entries: { ...all } };
  }
}

/** DPAPI (CurrentUser) via PowerShell. protect: text -> base64 blob; unprotect: base64 blob -> text. */
function dpapi(op: 'protect' | 'unprotect', input: string): Promise<string> {
  const script = [
    'Add-Type -AssemblyName System.Security',
    `$entropy = [Convert]::FromBase64String('${ENTROPY_B64}')`,
    '$in = [Console]::In.ReadToEnd()',
    op === 'protect'
      ? '$out = [Convert]::ToBase64String([Security.Cryptography.ProtectedData]::Protect([Text.Encoding]::UTF8.GetBytes($in), $entropy, "CurrentUser"))'
      : '$out = [Text.Encoding]::UTF8.GetString([Security.Cryptography.ProtectedData]::Unprotect([Convert]::FromBase64String($in.Trim()), $entropy, "CurrentUser"))',
    '[Console]::Out.Write($out)',
  ].join('; ');
  return new Promise((resolve, reject) => {
    const ps = spawn('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', script], { windowsHide: true });
    let out = '';
    let err = '';
    ps.stdout.on('data', (d) => (out += d));
    ps.stderr.on('data', (d) => (err += d));
    ps.on('error', reject);
    ps.on('close', (code) => {
      if (code === 0) resolve(out);
      else reject(new Error(`Could not ${op} stored passwords: ${err.trim() || `exit code ${code}`}`));
    });
    ps.stdin.end(input, 'utf8');
  });
}
