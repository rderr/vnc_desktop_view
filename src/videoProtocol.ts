// Messages between the video-preview webview and the extension host
// (experimental custom server, see server/). Separate from protocol.ts, which
// is the VNC viewer's.

export type VideoToExtension =
  | { type: 'ready' }                       // viewer loaded (or wants a restart)
  | { type: 'ack'; bytes: number }          // viewer has decoded this many more bytes
  | { type: 'keyframe' }                    // decoder lost sync; ask for a keyframe
  // Desktop pixels; buttons as in PointerEvent.buttons; wheel in Windows units (120 = notch, + = up/right).
  | { type: 'pointer'; x: number; y: number; buttons: number; wheelX: number; wheelY: number }
  | { type: 'key'; scan: number; down: boolean }  // PC set-1 scan code, 0xE0xx = extended
  | { type: 'log'; text: string };

export type VideoToWebview =
  | { type: 'hello'; width: number; height: number; codec: string }
  | { type: 'video'; key: boolean; timestamp: number; data: Uint8Array }
  | { type: 'status'; text: string };
