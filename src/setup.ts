import * as vscode from 'vscode';

/** Instructions for remotes where setup isn't automated yet. */
export function showManualSetup(): void {
  if (process.platform === 'darwin') {
    void vscode.window.showInformationMessage(
      'On macOS, turn on System Settings → General → Sharing → Screen Sharing, then under its (i) options enable ' +
      '"VNC viewers may control screen with password". macOS does not allow this to be switched on from a script. ' +
      'Then set "remoteDesktop.port" to 5900.',
      { modal: true },
    );
  } else {
    void vscode.window.showInformationMessage(
      'Linux setup is not automated yet. On an X11 desktop: install x11vnc, set a password with "x11vnc -storepasswd", ' +
      'then run "x11vnc -localhost -display :0 -rfbauth ~/.vnc/passwd -forever" and set "remoteDesktop.port" to 5900.',
      { modal: true },
    );
  }
}
