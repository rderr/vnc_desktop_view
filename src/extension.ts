import * as vscode from 'vscode';
import * as os from 'os';
import { VideoSession, findServerBinary } from './videoSession';

export function activate(context: vscode.ExtensionContext): void {
  const log = vscode.window.createOutputChannel('Remote Desktop View');
  context.subscriptions.push(log);

  context.subscriptions.push(
    vscode.commands.registerCommand('vncDesktopView.open', () => openDesktop(context, log)),
  );
}

async function openDesktop(context: vscode.ExtensionContext, log: vscode.OutputChannel): Promise<void> {
  if (process.platform !== 'win32') {
    void vscode.window.showInformationMessage(
      'Remote Desktop View supports Windows remotes so far. macOS and Linux support is planned.');
    return;
  }
  // In a plain local window the "remote" machine is this one. That's rarely what's
  // wanted (it shows your own screen), so explain first.
  if (!vscode.env.remoteName) {
    const choice = await vscode.window.showInformationMessage(
      'Remote Desktop View shows the desktop of the machine VS Code is connected to.',
      {
        modal: true,
        detail: 'This window is not connected to a remote. Connect with Remote Tunnels, Remote-SSH or WSL first, ' +
          'then run "Open Desktop" in that window. Opening it here would show this machine\'s own screen.',
      },
      'Open This Machine\'s Desktop',
    );
    if (!choice) return;
  }
  const binary = findServerBinary(context.extensionPath);
  if (!binary) {
    // Installed packages ship bin/desktop-server.exe; a development checkout needs it built.
    void vscode.window.showErrorMessage(
      'The desktop server is missing from this installation. Reinstall the extension, or in a ' +
      'development checkout run "cargo build --release" in server/.');
    return;
  }
  log.appendLine(`open desktop: remote=${vscode.env.remoteName ?? 'none'}`);
  new VideoSession(context, binary, os.hostname(), (m) => log.appendLine(m));
}
