// Optional "full access" mode on Windows: TightVNC installed as a Windows
// service (needs administrator rights once). Unlike application mode it can
// show UAC prompts, the lock screen and the sign-in screen.
//
// scripts/setup-windows.ps1 does the work. It leaves:
//  - %ProgramData%\vnc-desktop-view\service.json  { installedBy, port }
//  - a one-time password hand-off file in the extension's storage folder
//    (readable only by the user), which we move into the PasswordStore on
//    this machine, so every computer that connects here can use it.

import * as vscode from 'vscode';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import { PasswordStore } from '../passwordStore';

export interface ServiceInfo {
  /** 'extension' = we installed TightVNC; 'user' = theirs, settings kept; 'user-takeover' = theirs, reconfigured by us. */
  installedBy: 'extension' | 'user' | 'user-takeover';
  port: number;
}

const markerFile = () => path.join(process.env.ProgramData ?? 'C:\\ProgramData', 'vnc-desktop-view', 'service.json');
const handoffDir = (context: vscode.ExtensionContext) => path.join(context.globalStorageUri.fsPath, 'service-handoff');
export const secretKeyFor = (host: string, port: number) => `vncPassword:${host}:${port}`;

export function readServiceInfo(): ServiceInfo | undefined {
  try {
    const info = JSON.parse(fs.readFileSync(markerFile(), 'utf8').replace(/^\uFEFF/, '')) as ServiceInfo;
    return typeof info.port === 'number' ? info : undefined;
  } catch {
    return undefined;
  }
}

/** Move a password left by the setup script into the password store on this machine. */
export async function importHandoffPassword(
  context: vscode.ExtensionContext, store: PasswordStore, port: number,
): Promise<void> {
  const file = path.join(handoffDir(context), 'password.txt');
  if (!fs.existsSync(file)) return;
  const password = fs.readFileSync(file, 'utf8').replace(/^\uFEFF/, '').trim();
  if (password) {
    await store.set(PasswordStore.key('127.0.0.1', port), { password });
    // A new password replaces any copy 0.1.0 kept on the computer we connect from.
    await context.secrets.delete(secretKeyFor('127.0.0.1', port));
  }
  fs.rmSync(file, { force: true });
}

function runScript(context: vscode.ExtensionContext, name: string, extraArgs: string[]): void {
  const script = vscode.Uri.joinPath(context.extensionUri, 'scripts', 'setup-windows.ps1').fsPath;
  const user = `${process.env.USERDOMAIN ?? os.hostname()}\\${os.userInfo().username}`;
  fs.mkdirSync(handoffDir(context), { recursive: true });
  const term = vscode.window.createTerminal({
    name,
    shellPath: 'powershell.exe',
    shellArgs: ['-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', script,
      '-HandoffDir', handoffDir(context), '-ForUser', user, ...extraArgs],
  });
  term.show();
}

export async function installService(context: vscode.ExtensionContext): Promise<void> {
  const ok = await vscode.window.showInformationMessage(
    'Install full-access mode?',
    {
      modal: true,
      detail:
        'This installs TightVNC as a Windows service so the desktop tab can also show UAC prompts, the lock screen ' +
        'and the sign-in screen, and send Ctrl+Alt+Del.\n\n' +
        'It accepts connections only from this machine itself, uses a random password that the extension stores ' +
        'for you, and needs administrator rights once. Unless this session is already elevated (for example SSH as an ' +
        'administrator), a UAC prompt appears on the remote machine\'s own screen.',
    },
    'Install',
  );
  if (ok === 'Install') runScript(context, 'VNC Desktop View: install full access', []);
}

export async function removeService(context: vscode.ExtensionContext, store: PasswordStore): Promise<void> {
  const info = readServiceInfo();
  if (!info) {
    void vscode.window.showInformationMessage('Full-access mode is not installed.');
    return;
  }
  const detail = info.installedBy === 'extension'
    ? 'This uninstalls the TightVNC service that the extension installed. Needs administrator rights.'
    : 'TightVNC was installed before the extension set it up, so it will be left installed; ' +
      'the extension just stops using it. Needs administrator rights.';
  const ok = await vscode.window.showWarningMessage('Remove full-access mode?', { modal: true, detail }, 'Remove');
  if (ok === 'Remove') {
    await store.delete(PasswordStore.key('127.0.0.1', info.port)).catch(() => undefined);
    await context.secrets.delete(secretKeyFor('127.0.0.1', info.port));
    runScript(context, 'VNC Desktop View: remove full access', ['-Uninstall']);
  }
}
