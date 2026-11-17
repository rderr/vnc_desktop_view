// Builds the extension's VSIX packages:
//   vnc-desktop-view-<version>-win32-x64.vsix  Windows remotes; includes bin/desktop-server.exe (video preview)
//   vnc-desktop-view-<version>.vsix            every other platform (VNC only)
// VS Code installs the package matching the *remote* machine, where the extension runs.
//
// Must run on Windows with Rust and the Visual Studio C++ build tools (the server is a
// Windows program, and libopus needs CMake, which the build tools include).
//
//   node scripts/package.mjs            build both packages
//   node scripts/package.mjs --publish  build, then publish both to the Marketplace (needs VSCE_PAT)
import { execFileSync } from 'child_process';
import * as fs from 'fs';
import * as path from 'path';

const pkg = JSON.parse(fs.readFileSync('package.json', 'utf8'));
const vsce = ['node_modules/@vscode/vsce/vsce'];
const exe = 'desktop-server.exe';
const outputs = { win: `${pkg.name}-${pkg.version}-win32-x64.vsix`, universal: `${pkg.name}-${pkg.version}.vsix` };

function run(cmd, args, env = process.env) {
  console.log(`> ${[cmd, ...args].join(' ')}`);
  execFileSync(cmd, args, { stdio: 'inherit', env });
}

/** CMake for building libopus: from PATH, else the copy inside Visual Studio (found with vswhere). */
function cmakeDir() {
  try {
    execFileSync('cmake', ['--version'], { stdio: 'ignore' });
    return undefined; // already on PATH
  } catch { /* look in Visual Studio */ }
  const vswhere = path.join(process.env['ProgramFiles(x86)'] ?? 'C:\\Program Files (x86)', 'Microsoft Visual Studio', 'Installer', 'vswhere.exe');
  const found = fs.existsSync(vswhere)
    ? execFileSync(vswhere, ['-latest', '-products', '*', '-find', '**\\CMake\\bin\\cmake.exe'], { encoding: 'utf8' }).split(/\r?\n/)[0].trim()
    : '';
  if (!found) throw new Error('CMake not found: install the Visual Studio C++ build tools (they include CMake) or put cmake on PATH.');
  return path.dirname(found);
}

if (process.platform !== 'win32') {
  console.error('The Windows package includes the desktop server, which is built with the MSVC toolchain: run this on Windows.');
  process.exit(1);
}

// 1. The desktop server.
const dir = cmakeDir();
const env = dir ? { ...process.env, PATH: `${dir};${process.env.PATH}` } : process.env;
run('cargo', ['build', '--release', '--manifest-path', 'server/Cargo.toml'], env);
fs.rmSync('bin', { recursive: true, force: true });
fs.mkdirSync('bin');
fs.copyFileSync(path.join('server', 'target', 'release', exe), path.join('bin', exe));

// 2. The packages (vsce runs `npm run build` itself, via vscode:prepublish).
try {
  run(process.execPath, [...vsce, 'package', '--target', 'win32-x64', '--out', outputs.win]);
} finally {
  fs.rmSync('bin', { recursive: true, force: true }); // never ship it in the universal package
}
run(process.execPath, [...vsce, 'package', '--out', outputs.universal]);
console.log(`\nBuilt ${outputs.win} and ${outputs.universal}`);

if (process.argv.includes('--publish')) {
  run(process.execPath, [...vsce, 'publish', '--packagePath', outputs.win, outputs.universal]);
}
