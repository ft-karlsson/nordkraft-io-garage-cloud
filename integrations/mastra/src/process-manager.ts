/**
 * Nordkraft process manager.
 *
 * Every `spawn()` opens one WebSocket to `GET /containers/{id}/stream` and
 * runs the command under `sh -c`. The stream carries stdout/stderr frames
 * back, stdin frames forward, and a single status frame at the end; closing
 * the socket kills the process on the node, so `kill()` is just `close()`.
 */
import { ProcessHandle, SandboxProcessManager } from '@mastra/core/workspace';
import type { CommandResult, ProcessInfo, SpawnProcessOptions } from '@mastra/core/workspace';
import { frame, parseStatus, STATUS, STDERR, STDIN, STDOUT, toBytes, unframe } from './protocol.js';
import type { StatusFrame } from './protocol.js';
import type { NordkraftSandbox } from './sandbox.js';

/** The slice of the WebSocket API this module uses; satisfied by Node's global `WebSocket` and by the `ws` package. */
export interface WebSocketLike {
  binaryType: string;
  readyState: number;
  send(data: Uint8Array): void;
  close(code?: number, reason?: string): void;
  addEventListener(type: 'open', listener: () => void): void;
  addEventListener(type: 'message', listener: (event: { data: unknown }) => void): void;
  addEventListener(type: 'close', listener: (event: { code?: number; reason?: string }) => void): void;
  addEventListener(type: 'error', listener: (event: unknown) => void): void;
}

export type WebSocketConstructor = new (url: string) => WebSocketLike;

export interface NordkraftProcessManagerOptions {
  /** Environment merged into every spawn (the base class's `ProcessManagerOptions.env`). */
  env?: Record<string, string | undefined>;
  /** Default timeout in milliseconds for commands that don't specify one. */
  defaultTimeout?: number;
  /** WebSocket implementation; defaults to the global one (Node 22+). */
  WebSocket?: WebSocketConstructor;
}

/** Exit code reported when the process was killed by us (SIGKILL convention). */
const KILLED_EXIT_CODE = 137;
/** Exit code reported on timeout (GNU `timeout` convention, also what `nordkraft exec --timeout` uses). */
const TIMEOUT_EXIT_CODE = 124;

function describeWsError(event: unknown): string {
  if (event && typeof event === 'object') {
    const e = event as { message?: unknown; error?: { message?: unknown } };
    if (typeof e.message === 'string' && e.message) return e.message;
    if (e.error && typeof e.error.message === 'string') return e.error.message;
  }
  return 'WebSocket error';
}

export class NordkraftProcessHandle extends ProcessHandle {
  readonly pid: string;
  /** Resolves once the WebSocket handshake succeeded; rejects if it failed. */
  readonly opened: Promise<void>;

  private readonly _ws: WebSocketLike;
  private readonly _startedAt = Date.now();
  private readonly _timeout?: number;
  private readonly _stdoutDecoder = new TextDecoder();
  private readonly _stderrDecoder = new TextDecoder();
  private _exitCode: number | undefined;
  private _killed = false;
  private _settled = false;
  private _resolveDone!: () => void;
  private readonly _done: Promise<void>;
  private _waitPromise?: Promise<CommandResult>;

  constructor(pid: string, ws: WebSocketLike, options: SpawnProcessOptions = {}) {
    super(options);
    this.pid = pid;
    this._ws = ws;
    this._timeout = options.timeout;
    this._done = new Promise<void>(resolve => {
      this._resolveDone = resolve;
    });

    this.opened = new Promise<void>((resolve, reject) => {
      ws.addEventListener('open', () => resolve());
      ws.addEventListener('error', event => {
        reject(new Error(describeWsError(event)));
        // An error after open ends the session without a status frame.
        this.settle({ error: describeWsError(event) });
      });
    });
    // Nobody awaits `opened` on the kill path; keep an unobserved rejection from surfacing.
    this.opened.catch(() => {});

    ws.addEventListener('message', event => {
      const f = unframe(toBytes(event.data));
      if (!f) return;
      switch (f.channel) {
        case STDOUT:
          this.emitStdout(this._stdoutDecoder.decode(f.payload, { stream: true }));
          break;
        case STDERR:
          this.emitStderr(this._stderrDecoder.decode(f.payload, { stream: true }));
          break;
        case STATUS:
          this.settle(parseStatus(f.payload));
          break;
        default:
          // Resize/stdin never flow towards the client; ignore unknown channels.
          break;
      }
    });
    ws.addEventListener('close', () => {
      this.settle(this._killed ? {} : { error: 'stream closed before the status frame arrived' });
    });
  }

  get exitCode(): number | undefined {
    return this._exitCode;
  }

  /** Record the final outcome exactly once. */
  private settle(status: StatusFrame): void {
    if (this._settled) return;
    this._settled = true;
    // Flush any partial multi-byte sequence.
    const tailOut = this._stdoutDecoder.decode();
    if (tailOut) this.emitStdout(tailOut);
    const tailErr = this._stderrDecoder.decode();
    if (tailErr) this.emitStderr(tailErr);

    if (typeof status.exit_code === 'number') {
      this._exitCode = status.exit_code;
    } else if (this._killed) {
      this._exitCode = KILLED_EXIT_CODE;
    } else {
      this._exitCode = 1;
      if (status.error) this.emitStderr(`${status.error}\n`);
    }
    this._resolveDone();
  }

  async wait(): Promise<CommandResult> {
    if (!this._waitPromise) this._waitPromise = this.doWait();
    return this._waitPromise;
  }

  private async doWait(): Promise<CommandResult> {
    let timedOut = false;
    if (this._timeout && this._timeout > 0) {
      let timer: NodeJS.Timeout | undefined;
      const timeout = new Promise<'timeout'>(resolve => {
        timer = setTimeout(() => resolve('timeout'), this._timeout);
      });
      try {
        const winner = await Promise.race([this._done.then(() => 'done' as const), timeout]);
        if (winner === 'timeout') {
          timedOut = true;
          await this.kill();
          this._exitCode = TIMEOUT_EXIT_CODE;
        }
      } finally {
        clearTimeout(timer);
      }
    } else {
      await this._done;
    }
    const exitCode = this._exitCode ?? 1;
    return {
      success: exitCode === 0,
      exitCode,
      stdout: this.stdout,
      stderr: this.stderr,
      executionTimeMs: Date.now() - this._startedAt,
      timedOut,
      killed: this._killed,
      stdoutTruncated: this.stdoutTruncated,
      stderrTruncated: this.stderrTruncated,
      stdoutDroppedBytes: this.stdoutDroppedBytes,
      stderrDroppedBytes: this.stderrDroppedBytes,
    };
  }

  /** Closing the session makes the node kill the process — no orphans by design. */
  async kill(): Promise<boolean> {
    if (this._settled) return false;
    this._killed = true;
    try {
      this._ws.close(1000, 'killed by client');
    } catch {
      // Already closing or never opened; settle below regardless.
    }
    // The server acknowledges with a status frame or a close; don't hang forever if the link is gone.
    await Promise.race([this._done, new Promise<void>(resolve => setTimeout(resolve, 5_000))]);
    this.settle({});
    return true;
  }

  private _stdinOpen = true;

  /** @internal The process was spawned without stdin (`stdinMode: 'ignore'`). */
  markStdinClosed(): void {
    this._stdinOpen = false;
  }

  async sendStdin(data: string): Promise<void> {
    if (this._settled) throw new Error(`Process ${this.pid} has already exited with code ${this._exitCode}`);
    if (!this._stdinOpen) throw new Error(`Process ${this.pid} was spawned with stdinMode 'ignore'; there is no stdin to write to`);
    await this.opened;
    this._ws.send(frame(STDIN, data));
  }

  /** An empty stdin frame is EOF on the wire. */
  async closeStdin(): Promise<void> {
    if (this._settled || !this._stdinOpen) return;
    this._stdinOpen = false;
    await this.opened;
    this._ws.send(frame(STDIN));
  }
}

/**
 * Nordkraft implementation of SandboxProcessManager: one WebSocket session
 * per spawned process, tracked in memory.
 */
export class NordkraftProcessManager extends SandboxProcessManager<NordkraftSandbox> {
  private _counter = 0;
  private readonly _defaultTimeout?: number;
  private readonly _WebSocket?: WebSocketConstructor;

  constructor(options: NordkraftProcessManagerOptions = {}) {
    const { defaultTimeout, WebSocket, ...base } = options;
    super(base);
    this._defaultTimeout = defaultTimeout;
    this._WebSocket = WebSocket;
  }

  private get WebSocketImpl(): WebSocketConstructor {
    const impl = this._WebSocket ?? (globalThis as { WebSocket?: WebSocketConstructor }).WebSocket;
    if (!impl) {
      throw new Error('No WebSocket implementation available: run on Node 22+ or pass `WebSocket` (e.g. from the ws package)');
    }
    return impl;
  }

  async spawn(command: string, options: SpawnProcessOptions = {}): Promise<ProcessHandle> {
    const sandbox = this.sandbox;
    const env: Record<string, string> = {};
    for (const [k, v] of Object.entries(options.env ?? {})) if (v !== undefined) env[k] = v;
    // `ignore` (what executeCommand uses) attaches no stdin at all: the process
    // reads EOF from /dev/null and the node never has to close stdin through
    // containerd — which, on Kata agents before 3.28, drops pending output.
    const ignoreStdin = options.stdinMode === 'ignore';
    const spec = {
      cmd: ['sh', '-c', command],
      cwd: options.cwd ?? sandbox.workingDirectory,
      env,
      user: sandbox.user,
      tty: false,
      stdin: !ignoreStdin,
    };
    const effective: SpawnProcessOptions = { ...options, timeout: options.timeout ?? this._defaultTimeout };

    const open = async (): Promise<NordkraftProcessHandle> => {
      const url = sandbox.client.streamUrl(sandbox.containerName, spec);
      const ws = new this.WebSocketImpl(url);
      ws.binaryType = 'arraybuffer';
      const pid = `nk-${Date.now().toString(36)}-${++this._counter}`;
      const handle = new NordkraftProcessHandle(pid, ws, effective);
      await handle.opened;
      return handle;
    };

    // A handshake failure usually means the container was auto-stopped since we
    // last looked; revive it once and retry before giving up.
    const handle = await sandbox.retryOnDead(open);
    if (ignoreStdin) handle.markStdinClosed();
    this._tracked.set(handle.pid, handle);
    return handle;
  }

  async list(): Promise<ProcessInfo[]> {
    const result: ProcessInfo[] = [];
    for (const [pid, handle] of this._tracked) {
      result.push({ pid, command: handle.command, running: handle.exitCode === undefined, exitCode: handle.exitCode });
    }
    return result;
  }
}
