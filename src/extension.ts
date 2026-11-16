import * as vscode from 'vscode';
import * as os from 'os';
import { DesktopSession } from './session';
import { startTestServer, type TestMode } from './testServer';
import { showManualSetup } from './setup';
import { TightVncAppServer } from './windows/tightvncApp';
import type { InputState } from './windows/foregroundWatcher';
import { importHandoffPassword, installService, readServiceInfo, removeService } from './windows/service';

type ServerMode = 'app' | 'service' | 'custom';

let appServer: TightVncAppServer | undefined;

export function activate(context: vscode.ExtensionContext): void {
  const log = vscode.window.createOutputChannel('VNC Desktop View');
  context.subscriptions.push(log);
  appServer = new TightVncAppServer(
    context.globalStorageUri.fsPath,
    vscode.Uri.joinPath(context.extensionUri, 'scripts', 'foreground-watch.ps1').fsPath,
    (m) => log.appendLine(m),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand('vncDesktopView.open', () => openDesktop(context, log)),

    vscode.commands.registerCommand('vncDesktopView.openTestServer', async () => {
      const pick = await vscode.window.showQuickPick(
        [
          { label: 'Light', description: 'Small changing areas, like normal desktop use', mode: 'light' as TestMode },
          { label: 'Stress', description: 'Whole screen changes every frame, like full-screen video', mode: 'stress' as TestMode },
        ],
        { title: 'Test desktop mode' },
      );
      if (!pick) return;
      let session: DesktopSession | undefined;
      const server = await startTestServer(pick.mode, (fps) => void session?.post({ type: 'serverStats', fps }));
      session = new DesktopSession(context, {
        host: '127.0.0.1', port: server.port, label: `Test (${pick.label})`, fullAccess: true, mode: 'test',
      });
      session.onDidDispose(() => server.close());
    }),

    vscode.commands.registerCommand('vncDesktopView.installService', () =>
      process.platform === 'win32' ? installService(context) : showManualSetup()),

    vscode.commands.registerCommand('vncDesktopView.removeService', () =>
      process.platform === 'win32' ? removeService(context) : undefined),

    vscode.commands.registerCommand('vncDesktopView.forgetPassword', async () => {
      const cfg = vscode.workspace.getConfiguration('vncDesktopView');
      await context.secrets.delete(`vncPassword:${cfg.get('host', '127.0.0.1')}:${cfg.get('port', 5900)}`);
      const svc = readServiceInfo();
      if (svc) await context.secrets.delete(`vncPassword:127.0.0.1:${svc.port}`);
      void vscode.window.showInformationMessage('Saved VNC passwords removed.');
    }),
  );
}

function resolveMode(): ServerMode {
  const setting = vscode.workspace.getConfiguration('vncDesktopView').get<string>('server', 'auto');
  if (setting === 'app' || setting === 'service' || setting === 'custom') return setting;
  if (process.platform !== 'win32') return 'custom';
  return readServiceInfo() ? 'service' : 'app';
}

async function openDesktop(context: vscode.ExtensionContext, log: vscode.OutputChannel): Promise<void> {
  const label = os.hostname();
  const mode = resolveMode();
  log.appendLine(`open desktop: mode=${mode} remote=${vscode.env.remoteName ?? 'none'}`);

  // In a plain local window the "remote" machine is this one. That's rarely what's
  // wanted (it shows your own screen), so explain, unless a server was chosen explicitly.
  const explicit = vscode.workspace.getConfiguration('vncDesktopView').get<string>('server', 'auto') !== 'auto';
  if (!vscode.env.remoteName && !explicit) {
    const choice = await vscode.window.showInformationMessage(
      'VNC Desktop View shows the desktop of the machine VS Code is connected to.',
      {
        modal: true,
        detail: 'This window is not connected to a remote. Connect with Remote Tunnels, Remote-SSH or WSL first, ' +
          'then run "Open Desktop" in that window. Opening it here would show this machine\'s own screen.',
      },
      'Open This Machine\'s Desktop',
    );
    if (!choice) return;
  }

  try {
    switch (mode) {
      case 'app': {
        if (process.platform !== 'win32') throw new Error('Application mode is only available on Windows remotes.');
        const server = appServer!;
        const lease = await vscode.window.withProgress(
          {
            location: vscode.ProgressLocation.Notification,
            title: server.isDownloaded() ? 'Starting remote desktop…' : 'Downloading TightVNC (one time, ~2.5 MB)…',
          },
          () => server.acquire(),
        );
        const session = new DesktopSession(context, {
          host: '127.0.0.1', port: lease.port, label, password: lease.password, fullAccess: false, mode: 'app',
        });
        // Explain when Windows silently drops our input (admin window focused, UAC prompt, lock screen).
        const unsubscribe = server.inputWatcher.subscribe((s) => {
          session.setBanner(inputBlockedMessage(s), s.blocked);
        });
        session.onDidDispose(() => {
          unsubscribe();
          void server.release();
        });
        return;
      }
      case 'service': {
        const info = readServiceInfo();
        if (!info) {
          const choice = await vscode.window.showWarningMessage(
            'Full-access mode is not installed on this machine.', 'Install Full Access…');
          if (choice) await installService(context);
          return;
        }
        await importHandoffPassword(context, info.port);
        new DesktopSession(context, { host: '127.0.0.1', port: info.port, label, fullAccess: true, mode: 'service' });
        return;
      }
      case 'custom': {
        const cfg = vscode.workspace.getConfiguration('vncDesktopView');
        new DesktopSession(context, {
          host: cfg.get('host', '127.0.0.1'), port: cfg.get('port', 5900), label, fullAccess: true, mode: 'custom',
        });
        return;
      }
    }
  } catch (e) {
    const message = (e as Error).message;
    log.appendLine(`error: ${message}`);
    void vscode.window.showErrorMessage(`VNC Desktop View: ${message}`);
  }
}

function inputBlockedMessage(s: InputState): string | null {
  if (!s.blocked) return null;
  if (s.reason === 'secure') {
    return 'A UAC prompt or the lock screen is showing on the remote machine. Application mode can\'t see or control ' +
      'it, so the picture may be frozen. Full-access mode can.';
  }
  const what = s.title ? `"${s.title}" (${s.process})` : s.process ? `${s.process}` : 'The focused window';
  return `${what} is running as administrator. While it has focus, Windows blocks remote mouse and keyboard input ` +
    'in application mode, so you can\'t click or type. Full-access mode can control administrator windows.';
}

export async function deactivate(): Promise<void> {
  await appServer?.stop();
}
