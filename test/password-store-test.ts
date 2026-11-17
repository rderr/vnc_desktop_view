// Real-machine test of the remote password store: round trip, persistence
// across instances, deletion, and (on Windows) that the file on disk is
// DPAPI-encrypted rather than readable text.
//
// Usage: node <bundled test> <scratchDir>
import * as fs from 'fs';
import * as path from 'path';
import { PasswordStore } from '../src/passwordStore';

const assert = (cond: unknown, msg: string) => {
  if (!cond) { console.error('FAIL:', msg); process.exit(1); }
  console.log('ok -', msg);
};

async function main(): Promise<void> {
  const dir = path.join(process.argv[2], 'store');
  fs.rmSync(dir, { recursive: true, force: true });
  const secret = 'Tq7xK2pa';

  const a = new PasswordStore(dir);
  assert(await a.get('127.0.0.1:5900') === undefined, 'empty store returns nothing');

  // Concurrent writes must not lose entries (they're serialized).
  await Promise.all([
    a.set('127.0.0.1:5900', { password: secret }),
    a.set('127.0.0.1:5901', { username: 'me', password: 'other123' }),
  ]);

  const file = fs.readdirSync(dir).find((f) => f.startsWith('passwords.'))!;
  const raw = fs.readFileSync(path.join(dir, file), 'utf8');
  assert(!raw.includes(secret) && !raw.includes('5900'), `file on disk (${file}) doesn't contain the password or keys in readable form`);

  const b = new PasswordStore(dir); // a fresh instance, as after a VS Code restart
  assert((await b.get('127.0.0.1:5900'))?.password === secret, 'password survives a new store instance');
  assert((await b.get('127.0.0.1:5901'))?.username === 'me', 'concurrent second entry kept, with username');

  const t0 = Date.now();
  await b.get('127.0.0.1:5900');
  assert(Date.now() - t0 < 100, `repeat read served from cache (${Date.now() - t0} ms)`);

  // Another window (another store instance) changes the file: the cache must notice.
  await new Promise((r) => setTimeout(r, 50));
  await a.set('127.0.0.1:5900', { password: 'changed1' });
  assert((await b.get('127.0.0.1:5900'))?.password === 'changed1', 'change made by another instance is picked up');

  await b.delete('127.0.0.1:5900');
  assert(await b.get('127.0.0.1:5900') === undefined, 'deleted entry is gone');
  assert((await b.get('127.0.0.1:5901'))?.password === 'other123', 'other entry untouched');

  fs.rmSync(dir, { recursive: true, force: true });
  console.log('all password-store checks passed');
}

main().catch((e) => { console.error(e); process.exit(1); });
