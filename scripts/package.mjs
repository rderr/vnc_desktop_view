// Builds the extension's VSIX packages for the platform this runs on. VS Code
// installs the package matching the *remote* machine, where the extension runs.
//
//   On Windows:  vnc-desktop-view-<version>-win32-x64.vsix   includes bin/desktop-server.exe
//                vnc-desktop-view-<version>.vsix             every other platform (says it isn't supported yet)
//   On Linux:    vnc-desktop-view-<version>-linux-x64.vsix   includes bin/desktop-server
//   On macOS:    vnc-desktop-view-<version>-darwin-arm64.vsix (or -darwin-x64 on Intel Macs)
//
// Needs Rust. On Windows also the Visual Studio C++ build tools (they include
// CMake, which libopus needs); on Linux a C compiler and CMake.
//
//   node scripts/package.mjs            build this platform's package(s)
//   node scripts/package.mjs --publish  build, then publish them to the Marketplace (needs VSCE_PAT)
import { execFileSync } from 'child_process';
import * as fs from 'fs';
import * as path from 'path';

const pkg = JSON.parse(fs.readFileSync('package.json', 'utf8'));
const vsce = ['node_modules/@vscode/vsce/vsce'];
const windows = process.platform === 'win32';
const target = `${process.platform}-${process.arch}`;
const exe = windows ? 'desktop-server.exe' : 'desktop-server';
const outputs = { platform: `${pkg.name}-${pkg.version}-${target}.vsix`, universal: `${pkg.name}-${pkg.version}.vsix` };

function run(cmd, args, env = process.env) {
  console.log(`> ${[cmd, ...args].join(' ')}`);
  execFileSync(cmd, args, { stdio: 'inherit', env });
}

/** CMake for building libopus: from PATH, else (Windows) the copy inside Visual Studio, found with vswhere. */
function cmakeDir() {
  try {
    execFileSync('cmake', ['--version'], { stdio: 'ignore' });
    return undefined; // already on PATH
  } catch { /* look in Visual Studio */ }
  const vswhere = path.join(process.env['ProgramFiles(x86)'] ?? 'C:\\Program Files (x86)', 'Microsoft Visual Studio', 'Installer', 'vswhere.exe');
  const found = windows && fs.existsSync(vswhere)
    ? execFileSync(vswhere, ['-latest', '-products', '*', '-find', '**\\CMake\\bin\\cmake.exe'], { encoding: 'utf8' }).split(/\r?\n/)[0].trim()
    : '';
  if (!found) throw new Error('CMake not found: put cmake on PATH (on Windows, the Visual Studio C++ build tools include it).');
  return path.dirname(found);
}

if (!['win32-x64', 'linux-x64', 'darwin-arm64', 'darwin-x64'].includes(target)) {
  console.error(`No desktop server for ${target} yet: run this on Windows or Linux (x64), or macOS.`);
  process.exit(1);
}

// 1. The desktop server.
const dir = cmakeDir();
const env = dir ? { ...process.env, PATH: `${dir}${path.delimiter}${process.env.PATH}` } : process.env;
run('cargo', ['build', '--release', '--manifest-path', 'server/Cargo.toml'], env);
fs.rmSync('bin', { recursive: true, force: true });
fs.mkdirSync('bin');
fs.copyFileSync(path.join('server', 'target', 'release', exe), path.join('bin', exe));

// 2. The packages (vsce runs `npm run build` itself, via vscode:prepublish).
try {
  run(process.execPath, [...vsce, 'package', '--target', target, '--out', outputs.platform]);
} finally {
  fs.rmSync('bin', { recursive: true, force: true }); // never ship it in the universal package
}
const built = [outputs.platform];
if (windows) {
  // Built on Windows only, so it is published exactly once.
  run(process.execPath, [...vsce, 'package', '--out', outputs.universal]);
  built.push(outputs.universal);
}
console.log(`\nBuilt ${built.join(' and ')}`);

if (process.argv.includes('--publish')) {
  run(process.execPath, [...vsce, 'publish', '--packagePath', ...built]);
}
