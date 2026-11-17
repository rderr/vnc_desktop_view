import * as net from 'net';

/**
 * Pipes bytes between a TCP connection to the VNC server and a message-based
 * channel (the webview). Runs in the extension host on the remote machine.
 *
 * Server -> viewer: socket chunks are coalesced per event-loop turn so a large
 * framebuffer update becomes a few big messages instead of hundreds of 64 KiB
 * ones, and the socket is paused while too much data is still in flight, so a
 * slow link to the viewer can't make memory grow without bound.
 */
export interface RelaySink {
  /** Deliver bytes to the viewer. Resolves once the message has been handed off. */
  send(data: Uint8Array): Thenable<unknown>;
  open(): void;
  closed(reason: string): void;
}

const HIGH_WATER = 8 * 1024 * 1024; // pause the socket above this many in-flight bytes
const LOW_WATER = 2 * 1024 * 1024;  // resume below this

/**
 * Update pre-requesting ("streaming").
 *
 * VNC is pull-based: the viewer asks for an update, waits for it, draws it,
 * then asks again — so over a tunnel with ~150 ms round trips you get ~6 fps
 * no matter how small the updates are. Instead, the relay (next to the server,
 * where round trips are ~0) keeps asking the server for incremental updates
 * itself. To avoid flooding the tunnel and building up lag, it only asks while
 * fewer than STREAM_WINDOW bytes are unacknowledged by the viewer.
 */
const STREAM_INTERVAL_MS = 1000 / 60;
const STREAM_WINDOW = 256 * 1024;

export class Relay {
  private socket: net.Socket;
  private pending: Buffer[] = [];
  private flushScheduled = false;
  private inFlight = 0;
  private paused = false;
  private done = false;
  private bytesToViewer = 0;
  private bytesAcked = 0;
  private streamTimer: NodeJS.Timeout | undefined;
  private updateRequest: Buffer | undefined;
  /** Resolves once the TCP socket is fully closed. */
  private readonly socketClosed: Promise<void>;

  constructor(host: string, port: number, private readonly sink: RelaySink) {
    this.socket = net.connect({ host, port });
    this.socketClosed = new Promise((resolve) => this.socket.once('close', () => resolve()));
    this.socket.setNoDelay(true);
    this.socket.on('connect', () => sink.open());
    this.socket.on('data', (chunk) => this.onData(chunk));
    this.socket.on('error', (err) => this.finish(describeError(err, host, port)));
    this.socket.on('close', () => this.finish('Connection closed by the VNC server.'));
  }

  /**
   * Close gracefully (FIN rather than an abrupt reset) and resolve once the
   * socket is closed, or after timeoutMs. Used on reconnect so the server has
   * finished with the old connection before the new one arrives.
   */
  async disposeAndWait(timeoutMs = 1000): Promise<void> {
    if (this.streamTimer) clearInterval(this.streamTimer);
    this.done = true;
    this.pending = [];
    this.socket.end();
    const timedOut = await Promise.race([
      this.socketClosed.then(() => false),
      new Promise<boolean>((r) => setTimeout(() => r(true), timeoutMs)),
    ]);
    if (timedOut) this.socket.destroy();
  }

  /** Bytes from the viewer to the VNC server. */
  write(data: Uint8Array): void {
    if (!this.done) this.socket.write(data);
  }

  /** The viewer reports how many bytes it has received (for the stream window). */
  ack(bytes: number): void {
    this.bytesAcked += bytes;
  }

  /**
   * Start (or update the size for) pre-requesting incremental updates.
   * Only called after the RFB handshake is complete, so injected requests
   * can't land in the middle of it. noVNC writes whole messages per send, so
   * injecting between viewer writes never splits a client message.
   */
  stream(width: number, height: number): void {
    const req = Buffer.alloc(10);
    req[0] = 3; // FramebufferUpdateRequest
    req[1] = 1; // incremental
    req.writeUInt16BE(width, 6);
    req.writeUInt16BE(height, 8);
    this.updateRequest = req;
    this.streamTimer ??= setInterval(() => {
      if (this.done || !this.updateRequest) return;
      if (this.bytesToViewer - this.bytesAcked < STREAM_WINDOW) this.socket.write(this.updateRequest);
    }, STREAM_INTERVAL_MS);
  }

  /** Close initiated by the viewer (or the panel going away). */
  dispose(): void {
    if (this.streamTimer) clearInterval(this.streamTimer);
    this.done = true;
    this.pending = [];
    this.socket.destroy();
  }

  private onData(chunk: Buffer): void {
    this.pending.push(chunk);
    if (!this.flushScheduled) {
      this.flushScheduled = true;
      setImmediate(() => this.flush());
    }
  }

  private flush(): void {
    this.flushScheduled = false;
    if (this.done || this.pending.length === 0) return;
    const joined = Buffer.concat(this.pending);
    this.pending = [];
    // Tight copy: Buffers may be views into a shared pool, and we don't want
    // to serialize the whole pool.
    const data = new Uint8Array(joined.byteLength);
    data.set(joined);

    this.inFlight += data.byteLength;
    this.bytesToViewer += data.byteLength;
    if (!this.paused && this.inFlight > HIGH_WATER) {
      this.paused = true;
      this.socket.pause();
    }
    Promise.resolve(this.sink.send(data)).then(
      () => this.released(data.byteLength),
      () => this.released(data.byteLength),
    );
  }

  private released(bytes: number): void {
    this.inFlight -= bytes;
    if (this.paused && this.inFlight < LOW_WATER && !this.done) {
      this.paused = false;
      this.socket.resume();
    }
  }

  private finish(reason: string): void {
    if (this.done) return;
    // Deliver anything still buffered before reporting the close.
    this.flush();
    this.done = true;
    if (this.streamTimer) clearInterval(this.streamTimer);
    this.socket.destroy();
    this.sink.closed(reason);
  }
}

function describeError(err: NodeJS.ErrnoException, host: string, port: number): string {
  if (err.code === 'ECONNREFUSED') {
    return `No VNC server is listening on ${host}:${port} on the remote machine. ` +
      'Start one there (see the README for macOS and Linux), or check the vncDesktopView.port setting.';
  }
  return `Connection error (${err.code ?? 'unknown'}): ${err.message}`;
}
