// Messages exchanged between the webview (viewer, on the user's machine) and
// the extension host (relay, on the remote machine) via webview.postMessage.
//
// Every message about a connection carries `conn`, an id the webview picks for
// each connection attempt, so data from a stale socket (after a reconnect) is
// never fed into a new session.

export type ToExtension =
  | { type: 'ready'; conn: number }                         // viewer wants a fresh TCP connection
  | { type: 'data'; conn: number; data: Uint8Array }        // client -> VNC server bytes
  | { type: 'close'; conn: number }                         // viewer closed the connection
  | { type: 'ack'; conn: number; bytes: number }            // viewer has received this many more bytes
  | { type: 'stream'; conn: number; width: number; height: number } // start/refresh update pre-requesting
  | { type: 'connected'; conn: number; desktopName: string } // RFB handshake + auth succeeded
  | { type: 'needCredentials'; conn: number; types: string[] }
  | { type: 'authFailed'; conn: number; reason: string }
  | { type: 'clipboard'; text: string }                     // remote clipboard changed
  | { type: 'pasteRequest' }                                // user clicked "Paste to remote"
  | { type: 'installFullAccess' }                           // banner button
  | { type: 'log'; text: string };                          // diagnostic line for the output channel

export type ToWebview =
  | { type: 'init'; scaleToFit: boolean; label: string; fullAccess: boolean;
      mode: 'app' | 'service' | 'custom' | 'test'; server: string }
  | { type: 'open'; conn: number }
  | { type: 'data'; conn: number; data: Uint8Array }        // VNC server -> client bytes
  | { type: 'closed'; conn: number; reason: string }
  | { type: 'credentials'; conn: number; username?: string; password: string }
  | { type: 'paste'; text: string }
  | { type: 'serverStats'; fps: number }                     // only from the built-in test server
  | { type: 'banner'; text: string | null; offerFullAccess: boolean }; // warning above the desktop (null hides it)
