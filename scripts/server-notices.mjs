// Collects the licenses of everything compiled into the desktop server
// (server/, Rust) from Cargo's dependency graph: runtime dependencies only,
// for the Windows target. Used by build-notices.mjs; run directly to see a summary.
import { execFileSync } from 'child_process';
import * as fs from 'fs';
import * as path from 'path';
import { fileURLToPath } from 'url';

const LICENSE_FILE = /^(licen[cs]e|copying|notice)/i;

/** Returns [{ name, version, license, texts: [{ file, text }] }] sorted by name. */
export function serverDependencies(target = 'x86_64-pc-windows-msvc') {
  const meta = JSON.parse(execFileSync('cargo', [
    'metadata', '--format-version', '1', '--manifest-path', 'server/Cargo.toml', '--filter-platform', target,
  ], { encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 }));
  const packages = new Map(meta.packages.map((p) => [p.id, p]));
  const nodes = new Map(meta.resolve.nodes.map((n) => [n.id, n]));
  const seen = new Set();
  const stack = [meta.resolve.root];
  while (stack.length) {
    const id = stack.pop();
    if (seen.has(id)) continue;
    seen.add(id);
    for (const dep of nodes.get(id).deps) {
      // Normal (runtime) dependencies only: not build scripts' or dev dependencies.
      if (dep.dep_kinds.some((k) => k.kind === null)) stack.push(dep.pkg);
    }
  }
  seen.delete(meta.resolve.root);
  return [...seen].map((id) => {
    const p = packages.get(id);
    const dir = path.dirname(p.manifest_path);
    const texts = fs.readdirSync(dir)
      .filter((f) => LICENSE_FILE.test(f) && fs.statSync(path.join(dir, f)).isFile())
      .sort()
      .map((file) => ({ file, text: fs.readFileSync(path.join(dir, file), 'utf8').trim() }));
    return { name: p.name, version: p.version, license: p.license ?? 'see license file', repository: p.repository, texts };
  }).sort((a, b) => a.name.localeCompare(b.name));
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  for (const d of serverDependencies()) {
    console.log(`${d.name.padEnd(28)} ${d.version.padEnd(10)} ${d.license.padEnd(32)} ${d.texts.map((t) => t.file).join(', ') || '(no license file)'}`);
  }
}
