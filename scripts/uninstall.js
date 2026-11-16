// Runs via the "vscode:uninstall" hook after the extension is uninstalled
// (VS Code runs it once it has fully removed the extension, usually on the
// next start). Plain Node.js, no VS Code API, no administrator rights.
//
// Cleans up what application mode leaves in this VS Code installation's
// storage folder: the unpacked TightVNC files, a possibly still-running
// tvnserver.exe, and our TightVNC settings in HKCU (restoring the user's own
// settings if we had backed them up).
//
// The full-access service can't be removed here (that needs administrator
// rights); the README tells users to run "Remove Full-Access Mode" first.
'use strict';

const fs = require('fs');
const os = require('os');
const path = require('path');
const { execFileSync } = require('child_process');

const EXTENSION_ID = 'robderr.vnc-desktop-view'; // storage folders use the lower-cased id
const REG_KEY = 'HKCU\\Software\\TightVNC\\Server';
const OUR_MARKER = 'VncDesktopView';

function log(msg) {
  console.log(`[vnc-desktop-view uninstall] ${msg}`);
}

/** The globalStorage folder of the VS Code installation this copy of the extension belongs to. */
function storageDir() {
  const ext = __dirname.toLowerCase();
  const home = os.homedir();
  const appData = process.env.APPDATA || path.join(home, 'AppData', 'Roaming');
  // Remote servers (Remote-SSH, Remote Tunnels) keep extensions and data side by side.
  const server = ext.match(/^(.*[\\/]\.vscode-server(?:-insiders)?)[\\/]/);
  if (server) return path.join(server[1], 'data', 'User', 'globalStorage', EXTENSION_ID);
  // Local installs: ~/.vscode*/extensions  ->  %APPDATA%/<product>/User/globalStorage
  const products = [
    ['.vscode-insiders', 'Code - Insiders'],
    ['.vscode-oss', 'VSCodium'],
    ['.cursor', 'Cursor'],
    ['.vscode', 'Code'],
  ];
  for (const [dir, product] of products) {
    if (ext.includes(`${path.sep}${dir}${path.sep}`.toLowerCase()) || ext.includes(`/${dir}/`)) {
      return path.join(appData, product, 'User', 'globalStorage', EXTENSION_ID);
    }
  }
  return undefined;
}

function run(file, args) {
  try {
    return execFileSync(file, args, { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'], windowsHide: true });
  } catch {
    return null;
  }
}

function main() {
  if (process.platform !== 'win32') return;
  const dir = storageDir();
  if (!dir || !fs.existsSync(dir)) {
    log('nothing to clean up');
    return;
  }

  // Stop any tvnserver.exe running from our storage folder.
  const ps = `Get-CimInstance Win32_Process -Filter "Name='tvnserver.exe'" | ` +
    `Where-Object { $_.ExecutablePath -and $_.ExecutablePath.StartsWith('${dir.replace(/'/g, "''")}', 'OrdinalIgnoreCase') } | ` +
    `ForEach-Object { Stop-Process -Id $_.ProcessId -Force; $_.ProcessId }`;
  const stopped = run('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', ps]);
  if (stopped && stopped.trim()) log(`stopped tvnserver ${stopped.trim().split(/\s+/).join(', ')}`);

  // Remove our application-mode settings and restore the user's own, if we saved them.
  if (run('reg.exe', ['query', REG_KEY, '/v', OUR_MARKER]) !== null) {
    run('reg.exe', ['delete', REG_KEY, '/f']);
    log('removed TightVNC application-mode settings');
  }
  const backup = path.join(dir, 'user-tightvnc-settings.reg');
  if (fs.existsSync(backup) && run('reg.exe', ['import', backup]) !== null) {
    log('restored the previous TightVNC settings');
  }

  fs.rmSync(dir, { recursive: true, force: true });
  log(`deleted ${dir}`);
}

try {
  main();
} catch (e) {
  log(`cleanup failed: ${e && e.message}`);
}
