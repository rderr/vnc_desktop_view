// Runs scripts/foreground-watch.ps1 as one long-lived helper process and
// reports when remote input can't reach the foreground window in
// application mode (an administrator window has focus, or a UAC prompt /
// lock screen is showing).

import * as readline from 'readline';
import { spawn, type ChildProcess } from 'child_process';

export interface InputState {
  blocked: boolean;
  reason?: 'elevated' | 'secure';
  process?: string;
  title?: string;
}

export class ForegroundWatcher {
  private child: ChildProcess | undefined;
  private listeners = new Set<(s: InputState) => void>();
  private _state: InputState = { blocked: false };

  constructor(private readonly scriptPath: string, private readonly log: (msg: string) => void) {}

  get state(): InputState {
    return this._state;
  }

  /** Subscribe; the listener is called right away with the current state. Returns an unsubscribe function. */
  subscribe(listener: (s: InputState) => void): () => void {
    this.listeners.add(listener);
    listener(this._state);
    return () => this.listeners.delete(listener);
  }

  start(): void {
    if (this.child) return;
    const child = spawn('powershell.exe', [
      '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass',
      '-File', this.scriptPath, '-ParentPid', String(process.pid),
    ], { stdio: ['ignore', 'pipe', 'pipe'], windowsHide: true });
    this.child = child;

    readline.createInterface({ input: child.stdout! }).on('line', (line) => {
      try {
        const s = JSON.parse(line) as InputState & { error?: string };
        if (s.error) this.log(`foreground watcher: ${s.error}`);
        this.update({ blocked: s.blocked, reason: s.reason, process: s.process, title: s.title });
      } catch {
        this.log(`foreground watcher: unexpected output ${line}`);
      }
    });
    child.stderr!.on('data', (d) => this.log(`foreground watcher: ${String(d).trim()}`));
    child.on('exit', (code) => {
      if (this.child === child) {
        this.child = undefined;
        this.log(`foreground watcher exited (${code})`);
      }
    });
  }

  stop(): void {
    const child = this.child;
    this.child = undefined;
    child?.kill();
    this.update({ blocked: false });
  }

  private update(s: InputState): void {
    const changed = JSON.stringify(s) !== JSON.stringify(this._state);
    this._state = s;
    if (changed) for (const l of this.listeners) l(s);
  }
}
