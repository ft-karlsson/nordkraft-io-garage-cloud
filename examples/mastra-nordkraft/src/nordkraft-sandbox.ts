// NordkraftSandbox — a Mastra sandbox provider backed by a Nordkraft container.
//
// Mastra's agent tools (execute_command etc.) call executeCommand(); we turn
// that into POST /containers/<name>/exec on the controller. Auth is the
// WireGuard source address, so there is no token anywhere in this file.
//
// Kept to the minimum Mastra needs: lifecycle + executeCommand + writeFiles
// (via exec + stdin) + networking.getPortUrl (container IP on the WG net).

import { MastraSandbox, type MastraSandboxOptions } from '@mastra/core/workspace';
import type {
  CommandResult,
  ExecuteCommandOptions,
  SandboxInfo,
  SandboxFileInput,
  SandboxNetworking,
} from '@mastra/core/workspace';
import type { ProviderStatus } from '@mastra/core/workspace';

export interface NordkraftSandboxOptions extends MastraSandboxOptions {
  /** Controller API base, e.g. http://172.20.0.254:8001/api */
  apiBase: string;
  /** Logical sandbox id. The container carries it as the label
   *  `mastra.sandbox=<id>`; the API's own container name is app-<uuid>. */
  container: string;
  /** Image to deploy when the container does not exist yet. */
  image?: string;
  /** Labels so the provider can find its own containers again. */
  labels?: Record<string, string>;
  /** Default exec timeout in ms (Nordkraft caps at 3600 s). */
  defaultTimeoutMs?: number;
}

interface ExecResponse {
  exit_code: number | null;
  stdout: string;
  stderr: string;
  duration_ms: number;
  truncated: boolean;
  error?: string;
}

export class NordkraftSandbox extends MastraSandbox<string> {
  readonly id: string;
  readonly name: string;
  readonly provider = 'nordkraft';
  status: ProviderStatus = 'pending';
  readonly networking: SandboxNetworking;

  private readonly apiBase: string;
  private readonly sandboxKey: string;
  /** Real container name (app-<uuid>) once found or created. */
  private container?: string;
  private readonly image?: string;
  private readonly labels: Record<string, string>;
  private readonly defaultTimeoutMs: number;
  private containerIp?: string;
  private createdAt = new Date();

  constructor(opts: NordkraftSandboxOptions) {
    super({ name: `nordkraft:${opts.container}`, ...opts });
    this.id = opts.container;
    this.name = `nordkraft:${opts.container}`;
    this.apiBase = opts.apiBase.replace(/\/$/, '');
    this.sandboxKey = opts.container;
    this.image = opts.image;
    this.labels = opts.labels ?? {};
    this.defaultTimeoutMs = opts.defaultTimeoutMs ?? 120_000;
    this.networking = {
      getPortUrl: async (port: number) =>
        this.containerIp ? `http://${this.containerIp}:${port}` : null,
    };
  }

  // ---- lifecycle: find → connect, else create -----------------------------

  protected async find(): Promise<string | undefined> {
    const q = `label=${encodeURIComponent(`mastra.sandbox=${this.sandboxKey}`)}`;
    const res = await fetch(`${this.apiBase}/containers?${q}`);
    if (!res.ok) throw new Error(`nordkraft list: HTTP ${res.status}`);
    const body = (await res.json()) as { containers?: Array<{ name: string; status: string; container_ip?: string }> };
    const hit = body.containers?.[0];
    if (!hit) return undefined;
    this.containerIp = hit.container_ip;
    if (!/^Up/.test(hit.status)) {
      const r = await fetch(`${this.apiBase}/containers/${hit.name}/start`, { method: 'POST' });
      if (!r.ok) throw new Error(`nordkraft start: HTTP ${r.status}`);
    }
    return hit.name;
  }

  protected async connect(handle: string): Promise<void> {
    this.container = handle;
    this.status = 'running';
    await this.ensureWorkingDirectory();
  }

  /** A cwd that does not exist makes every exec fail before it starts;
   *  images rarely ship the directory an agent wants, so create it. */
  private async ensureWorkingDirectory(): Promise<void> {
    const dir = this.workingDirectory;
    if (!dir || !this.container) return;
    await fetch(`${this.apiBase}/containers/${this.container}/exec`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ cmd: ['mkdir', '-p', dir], timeout_s: 10 }),
    });
  }

  protected async create(): Promise<void> {
    if (!this.image) throw new Error(`sandbox ${this.sandboxKey} not found and no image given`);
    const res = await fetch(`${this.apiBase}/containers/deploy`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        image: this.image,
        name: this.container,
        command: ['sleep', 'infinity'],
        // Sandbox contract: deploy returns when exec works (needs node ≥ 0.3.44).
        wait: true,
        wait_timeout_s: 180,
        lifecycle: { restart: 'never', auto_stop: '30m', auto_delete: '2h' },
        labels: { 'mastra.provider': 'nordkraft', 'mastra.sandbox': this.sandboxKey, ...this.labels },
      }),
    });
    const body = (await res.json()) as { error?: string; container_ip?: string; container_name?: string };
    if (!res.ok || body.error || !body.container_name) throw new Error(`nordkraft deploy: ${body.error ?? res.status}`);
    this.container = body.container_name;
    this.containerIp = body.container_ip;
    this.status = 'running';
    await this.ensureWorkingDirectory();
  }

  private get name_(): string {
    if (!this.container) throw new Error('sandbox not started');
    return this.container;
  }

  async _stop(): Promise<void> {
    await fetch(`${this.apiBase}/containers/${this.name_}/stop`, { method: 'POST' });
    this.status = 'stopped';
  }

  async _destroy(): Promise<void> {
    await fetch(`${this.apiBase}/containers/${this.name_}?force=true`, { method: 'DELETE' });
    this.status = 'destroyed';
  }

  // ---- the one call that matters --------------------------------------------

  async executeCommand(command: string, args: string[] = [], options: ExecuteCommandOptions = {}): Promise<CommandResult> {
    await this.ensureRunning();
    const timeoutMs = options.timeout ?? this.defaultTimeoutMs;
    const env = Object.fromEntries(
      Object.entries({ ...this.getEnv(), ...(options.env ?? {}) }).filter((kv): kv is [string, string] => typeof kv[1] === 'string'),
    );
    // Mastra's execute_command tool hands the whole shell line as `command`
    // with no args (its LocalSandbox runs it with shell: true), so pipes,
    // heredocs and && must work. With explicit args it is argv.
    const cmd = args.length === 0 ? ['sh', '-c', command] : [command, ...args];
    const res = await fetch(`${this.apiBase}/containers/${this.name_}/exec`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        cmd,
        cwd: options.cwd ?? this.workingDirectory,
        env,
        timeout_s: Math.max(1, Math.ceil(timeoutMs / 1000)),
      }),
      signal: options.abortSignal,
    });
    if (!res.ok) throw new Error(`nordkraft exec: HTTP ${res.status} ${await res.text()}`);
    const r = (await res.json()) as ExecResponse;

    options.onStdout?.(r.stdout);
    if (r.stderr) options.onStderr?.(r.stderr);

    const timedOut = r.exit_code === null && /timeout/i.test(r.error ?? '');
    const exitCode = r.exit_code ?? (timedOut ? 124 : 1);
    return {
      command,
      args,
      exitCode,
      success: exitCode === 0,
      stdout: r.stdout,
      stderr: r.error && !timedOut ? `${r.stderr}${r.error}\n` : r.stderr,
      executionTimeMs: r.duration_ms,
      timedOut,
      killed: timedOut,
      stdoutTruncated: r.truncated,
    };
  }

  /** Files via exec + stdin: enough for scripts and small inputs. */
  async writeFiles(files: SandboxFileInput[]): Promise<void> {
    for (const f of files) {
      const content = typeof f.content === 'string' ? f.content : f.content.toString('utf8');
      const dir = f.path.includes('/') ? f.path.slice(0, f.path.lastIndexOf('/')) : '.';
      const res = await fetch(`${this.apiBase}/containers/${this.name_}/exec`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({
          cmd: ['sh', '-c', `mkdir -p "$1" && cat > "$2"${f.mode ? ` && chmod ${f.mode.toString(8)} "$2"` : ''}`, 'sh', dir, f.path],
          stdin: content,
          timeout_s: 30,
        }),
      });
      const r = (await res.json()) as ExecResponse;
      if (r.exit_code !== 0) throw new Error(`writeFiles ${f.path}: ${r.stderr || r.error}`);
    }
  }

  getInfo(): SandboxInfo {
    return {
      id: this.id,
      name: this.name,
      provider: this.provider,
      status: this.status,
      createdAt: this.createdAt,
      metadata: { sandbox: this.sandboxKey, container: this.container, ip: this.containerIp, apiBase: this.apiBase },
    };
  }
}
