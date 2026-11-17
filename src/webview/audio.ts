// Plays the server's Opus audio: libopus (WebAssembly, from opus-decoder) ->
// Web Audio. VS Code's webview has no Opus in WebCodecs, so we bring our own.
// Each decoded 20 ms chunk is scheduled right after the previous one, with a
// small cushion so network jitter doesn't cause clicks, and a cap so delay
// can't build up.

import { OpusDecoder } from 'opus-decoder';

const RATE = 48000;
/** Cushion when (re)starting playback. */
const START_LATENCY = 0.06;
/** If playback falls this far behind real time, drop audio to catch up. */
const MAX_LATENCY = 0.25;

export class AudioPlayer {
  private ctx: AudioContext | undefined;
  private gain: GainNode | undefined;
  private decoder: OpusDecoder<48000> | undefined;
  private decoderReady = false;
  private nextTime = 0;
  private reportedError = false;

  constructor(private readonly log: (text: string) => void) {}

  get running(): boolean {
    return this.ctx?.state === 'running' && this.decoderReady;
  }

  /** Must be called from a user gesture (click / key), or the browser keeps audio suspended. */
  async start(): Promise<boolean> {
    try {
      if (!this.ctx) {
        this.ctx = new AudioContext({ sampleRate: RATE, latencyHint: 'interactive' });
        this.gain = this.ctx.createGain();
        this.gain.connect(this.ctx.destination);
      }
      if (this.ctx.state !== 'running') await this.ctx.resume();
      if (!this.decoder) {
        this.decoder = new OpusDecoder({ sampleRate: RATE, channels: 2 });
        await this.decoder.ready;
        this.decoderReady = true;
      }
      return this.running;
    } catch (e) {
      if (!this.reportedError) this.log(`video preview: could not start audio: ${(e as Error).message}`);
      this.reportedError = true;
      return false;
    }
  }

  stop(): void {
    void this.ctx?.suspend();
  }

  push(data: Uint8Array): void {
    if (!this.running) return;
    const out = this.decoder!.decodeFrame(data);
    if (out.errors.length && !this.reportedError) {
      this.reportedError = true;
      this.log(`video preview: audio decode error: ${out.errors[0].message}`);
    }
    if (out.samplesDecoded) this.play(out.channelData, out.samplesDecoded);
  }

  private play(channels: Float32Array[], frames: number): void {
    const ctx = this.ctx!;
    const now = ctx.currentTime;
    if (this.nextTime < now + 0.01) this.nextTime = now + START_LATENCY; // underrun or first chunk
    if (this.nextTime > now + MAX_LATENCY) return; // too far behind: drop to catch up
    const buffer = ctx.createBuffer(2, frames, RATE);
    for (let ch = 0; ch < 2; ch++) {
      buffer.copyToChannel(channels[Math.min(ch, channels.length - 1)].subarray(0, frames) as Float32Array<ArrayBuffer>, ch);
    }
    const src = ctx.createBufferSource();
    src.buffer = buffer;
    src.connect(this.gain!);
    src.start(this.nextTime);
    this.nextTime += buffer.duration;
  }
}
