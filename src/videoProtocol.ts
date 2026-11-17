// Messages between the video-preview webview and the extension host
// (experimental custom server, see server/). Separate from protocol.ts, which
// is the VNC viewer's.

export type VideoToExtension =
  | { type: 'ready' }                       // viewer loaded (or wants a restart)
  | { type: 'ack'; bytes: number }          // viewer has decoded this many more bytes
  | { type: 'keyframe' }                    // decoder lost sync; ask for a keyframe
  | { type: 'log'; text: string };

export type VideoToWebview =
  | { type: 'hello'; width: number; height: number; codec: string }
  | { type: 'video'; key: boolean; timestamp: number; data: Uint8Array }
  | { type: 'status'; text: string };
