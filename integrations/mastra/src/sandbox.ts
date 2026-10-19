/**
 * Nordkraft Sandbox Provider for Mastra workspaces.
 *
 * A sandbox is one Nordkraft container: a Kata micro-VM with its own kernel,
 * reached through the controller over WireGuard. `start()` finds the
 * container carrying this sandbox's id label (or deploys one), `stop()` stops
 * it (files kept, no compute billed), `destroy()` removes it. Commands run
 * over the exec/shell stream; lifecycle timers (auto-stop, auto-delete,
 * max-lifetime) are enforced by the node itself, so they hold even when the
 * orchestrator process is gone.
 */
import { MastraSandbox, SandboxNotReadyError, validateSandboxFileMode } from '@mastra/core/workspace';
import type {
  MastraSandboxOptions,
  ProviderStatus,
  SandboxCloneOptions,
  SandboxFileInput,
  SandboxInfo,
  SandboxNetworking,
} from '@mastra/core/workspace';
import { NordkraftApiError, NordkraftClient } from './client.js';
import type { ContainerInfo, DeployRequest, ExecResult, LifecycleSpec } from './client.js';
import { minutesToDuration, sizeToMiB } from './duration.js';
import { NordkraftProcessManager } from './process-manager.js';
import type { WebSocketConstructor } from './process-manager.js';

/** Label that ties a container to a logical Mastra sandbox id. */
export const SANDBOX_ID_LABEL = 'mastra-sandbox-id';

export const DEFAULT_IMAGE = 'alpine:3.21';
/** Keeps the micro-VM alive so exec sessions have something to attach to. */
export const DEFAULT_COMMAND = ['tail', '-f', '/dev/null'];
export const DEFAULT_WORKING_DIRECTORY = '/workspace';

export interface NordkraftResources {
  /** vCPUs (fractions allowed, e.g. 0.5). Platform default 0.5. */
  cpu?: number;
  /** Memory limit, e.g. `"512m"`, `"2g"`. Platform default 512m. */
  memory?: string;
  /** Disk allocation counted against the plan, e.g. `"1g"`. Platform default 1g. */
  disk?: string;
}

export interface NordkraftSandboxOptions extends Omit<MastraSandboxOptions, 'processes'> {
  /** Logical sandbox id. A container labelled with it is reused on later starts. */
  id?: string;
  /** Controller API, e.g. `http://172.20.0.254:8001/api`. Falls back to `NORDKRAFT_API_URL`, then the Nordkraft Cloud address. */
  apiUrl?: string;
  /** Attach to an existing container by name (`app-…`) or alias instead of looking it up by label. */
  sandboxId?: string;
  /** OCI image. Needs `/bin/sh`. @default "alpine:3.21" */
  image?: string;
  /** Main process; the sandbox lives as long as it does. @default ["tail","-f","/dev/null"] */
  command?: string[];
  resources?: NordkraftResources;
  /**
   * Environment baked into the container at creation and also applied as the
   * Mastra runtime overlay, so `nordkraft shell` sees the same variables.
   */
  env?: Record<string, string>;
  /** Free-form labels; `mastra-sandbox-id` is always added. */
  labels?: Record<string, string>;
  /** Created on start; commands run here unless they pass `cwd`. @default "/workspace" */
  workingDirectory?: string;
  /** User to exec as (`"1000:1000"`). Default: the image's user. */
  user?: string;
  /** Default command timeout in milliseconds. @default 300_000 */
  timeout?: number;
  /** Ports to publish. Only matters on bridge-networked (dev) nodes; on macvlan every port is reachable on the container IP. */
  ports?: number[];
  /** Keep a persistent volume at `volumePath` across upgrades. */
  persistence?: boolean;
  volumePath?: string;
  enableIpv6?: boolean;
  /** Pin to a garage / hardware class / CPU architecture. */
  targetGarage?: string;
  hardwarePreference?: string;
  architecture?: string;
  /** Stop after this many minutes without an exec/shell session (0 = never). @default 15 */
  autoStopInterval?: number;
  /** Delete this many minutes after the container stopped (undefined = never, 0 = at the next reaper tick). */
  autoDeleteInterval?: number;
  /** Stop this many minutes after creation whatever happens (undefined = never). */
  maxLifetimeMinutes?: number;
  /** Shorthand for `autoDeleteInterval: 0`: the container is removed as soon as it stops. */
  ephemeral?: boolean;
  /** Restart policy for the main process. @default "never" */
  restart?: LifecycleSpec['restart'];
  /** How long to wait for a freshly deployed container to accept exec (image pull included). @default 180_000 */
  startTimeout?: number;
  /** Test hooks. */
  fetch?: typeof fetch;
  WebSocket?: WebSocketConstructor;
}

const RUNNING = /^(up|running)\b/i;
const DEAD = /^(failed|deleted|removing)\b/i;

function isRunning(status: string | undefined): boolean {
  return !!status && RUNNING.test(status);
}

function sleep(ms: number): Promise<void> {
  return new Promise(resolve => setTimeout(resolve, ms));
}

function shellQuote(arg: string): string {
  return /^[A-Za-z0-9_./=:@-]+$/.test(arg) ? arg : `'${arg.replace(/'/g, `'\\''`)}'`;
}

/**
 * Nordkraft sandbox provider for Mastra workspaces.
 *
 * @example
 * ```typescript
 * import { Workspace } from '@mastra/core/workspace';
 * import { NordkraftSandbox } from '@nordkraft/mastra';
 *
 * const workspace = new Workspace({
 *   sandbox: new NordkraftSandbox({ id: 'demo', image: 'python:3.12-alpine' }),
 * });
 * await workspace.init();
 * const result = await workspace.sandbox!.executeCommand!('python3 -c "print(40+2)"');
 * ```
 */
export class NordkraftSandbox extends MastraSandbox<ContainerInfo> {
  readonly id: string;
  readonly name = 'NordkraftSandbox';
  readonly provider = 'nordkraft';
  status: ProviderStatus = 'pending';
  declare readonly processes: NordkraftProcessManager;
  readonly networking: SandboxNetworking;
  readonly client: NordkraftClient;

  readonly image: string;
  readonly user?: string;
  readonly timeout: number;
  private readonly command: string[];
  private readonly resources?: NordkraftResources;
  private readonly creationEnv: Record<string, string>;
  private readonly labels: Record<string, string>;
  private readonly ports: number[];
  private readonly persistence: boolean;
  private readonly volumePath?: string;
  private readonly enableIpv6: boolean;
  private readonly targetGarage?: string;
  private readonly hardwarePreference?: string;
  private readonly architecture?: string;
  private readonly lifecycle: LifecycleSpec;
  private readonly startTimeout: number;
  private readonly attachName?: string;
  private readonly _constructorOptions: NordkraftSandboxOptions;

  private _container?: ContainerInfo;
  /** Name of the container we last held, so a stopped sandbox is found again by name, not by a label scan. */
  private _lastName?: string;
  private _createdAt?: Date;
  private _reviving?: Promise<void>;

  constructor(options: NordkraftSandboxOptions = {}) {
    const { fetch: fetchImpl, WebSocket, ...rest } = options;
    const timeout = options.timeout ?? 300_000;
    super({
      name: 'NordkraftSandbox',
      onStart: options.onStart,
      onStop: options.onStop,
      onDestroy: options.onDestroy,
      env: options.env,
      workingDirectory: options.workingDirectory ?? DEFAULT_WORKING_DIRECTORY,
      processes: new NordkraftProcessManager({ defaultTimeout: timeout, WebSocket }),
    });
    this._constructorOptions = rest;
    this.id = options.id ?? `nordkraft-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`;
    this.client = new NordkraftClient({ apiUrl: options.apiUrl, fetch: fetchImpl });
    this.attachName = options.sandboxId;
    this.image = options.image ?? DEFAULT_IMAGE;
    this.command = options.command ?? DEFAULT_COMMAND;
    this.user = options.user;
    this.timeout = timeout;
    this.resources = options.resources;
    this.creationEnv = { ...options.env };
    this.labels = { ...options.labels };
    this.ports = options.ports ?? [];
    this.persistence = options.persistence ?? false;
    this.volumePath = options.volumePath;
    this.enableIpv6 = options.enableIpv6 ?? false;
    this.targetGarage = options.targetGarage;
    this.hardwarePreference = options.hardwarePreference;
    this.architecture = options.architecture;
    this.startTimeout = options.startTimeout ?? 180_000;

    const autoDelete = options.ephemeral ? 0 : options.autoDeleteInterval;
    this.lifecycle = {
      restart: options.restart ?? 'never',
      auto_stop: minutesToDuration(options.autoStopInterval ?? 15),
      ...(autoDelete !== undefined && { auto_delete: autoDelete === 0 ? '1s' : minutesToDuration(autoDelete) }),
      ...(options.maxLifetimeMinutes !== undefined && { max_lifetime: minutesToDuration(options.maxLifetimeMinutes) }),
    };

    this.networking = {
      getPortUrl: async (port: number) => {
        const c = this._container ?? (await this.lookup().catch(() => undefined));
        if (!c) return null;
        const mapped = c.ports?.find(p => p.port === port);
        if (mapped?.access_url) return /^https?:\/\//.test(mapped.access_url) ? mapped.access_url : `http://${mapped.access_url}`;
        if (c.container_ip && c.container_ip !== 'pending') return `http://${c.container_ip}:${port}`;
        return null;
      },
    };
  }

  // ---------------------------------------------------------------------------
  // Identity
  // ---------------------------------------------------------------------------

  /** The container name (`app-…`) — the provider-side id to reattach with via `sandboxId`. */
  get sandboxId(): string | undefined {
    return this._container?.name ?? this._lastName;
  }

  /** Name of the attached container. @throws SandboxNotReadyError before start */
  get containerName(): string {
    if (!this._container) throw new SandboxNotReadyError(this.id);
    return this._container.name;
  }

  /** The last container listing we saw for this sandbox. */
  get container(): ContainerInfo | undefined {
    return this._container;
  }

  clone(options: SandboxCloneOptions = {}): NordkraftSandbox {
    const { id: _id, sandboxId: _sid, ...base } = this._constructorOptions;
    return new NordkraftSandbox({
      ...base,
      fetch: this._constructorOptions.fetch,
      ...(options.id !== undefined && { id: options.id }),
      ...(options.sandboxId !== undefined && { sandboxId: options.sandboxId }),
      ...(options.env !== undefined && { env: options.env }),
      ...(options.workingDirectory !== undefined && { workingDirectory: options.workingDirectory }),
      ...(options.idleTimeoutMinutes !== undefined && { autoStopInterval: options.idleTimeoutMinutes }),
    });
  }

  // ---------------------------------------------------------------------------
  // Acquisition: find / connect / create (the base class orchestrates start)
  // ---------------------------------------------------------------------------

  /** Look the container up by remembered name, attach name, or sandbox-id label. */
  private async lookup(): Promise<ContainerInfo | undefined> {
    const wanted = this._lastName ?? this.attachName;
    if (wanted) {
      const all = await this.client.list();
      const hit = all.find(c => c.name === wanted || c.container_id === wanted);
      if (hit) return hit;
      if (this.attachName && !this._lastName) {
        throw new NordkraftApiError(`sandbox container '${this.attachName}' not found (or not yours)`, 404);
      }
      return undefined;
    }
    const tagged = await this.client.list({ [SANDBOX_ID_LABEL]: this.id });
    const alive = tagged.filter(c => !DEAD.test(c.status));
    // Prefer a running one; otherwise the newest.
    alive.sort((a, b) => Number(isRunning(b.status)) - Number(isRunning(a.status)) || b.created_at.localeCompare(a.created_at));
    return alive[0];
  }

  protected async find(): Promise<ContainerInfo | undefined> {
    if (this._container) return this._container;
    return this.lookup();
  }

  protected async connect(existing: ContainerInfo): Promise<void> {
    this._container = existing;
    this._lastName = existing.name;
    this._createdAt = existing.created_at ? new Date(existing.created_at) : new Date();
    if (!isRunning(existing.status)) {
      this.logger.debug(`[Nordkraft] starting stopped container ${existing.name} for sandbox ${this.id}`);
      await this.client.start(existing.name);
      // A start on a remote node is asynchronous and a Kata micro-VM takes a
      // few seconds to boot. Exec'ing into a sandbox that is still booting is
      // refused by the runtime, so wait for the node to report it Up first.
      await this.waitUntilRunning(existing.name);
    }
    await this.waitExecReady(existing.name);
    await this.refresh();
    await this.prepareWorkingDirectory();
  }

  /** Poll the listing until the node reports the container running. */
  private async waitUntilRunning(name: string): Promise<void> {
    const deadline = Date.now() + this.startTimeout;
    let last = 'unknown';
    while (Date.now() < deadline) {
      const listed = (await this.client.list().catch(() => [] as ContainerInfo[])).find(c => c.name === name);
      if (listed) {
        last = listed.status;
        if (isRunning(listed.status)) {
          this._container = listed;
          return;
        }
        if (DEAD.test(listed.status)) {
          throw new NordkraftApiError(`container ${name} ${listed.status}${listed.status_message ? `: ${listed.status_message}` : ''}`);
        }
      }
      await sleep(1_000);
    }
    throw new NordkraftApiError(`container ${name} did not start within ${Math.round(this.startTimeout / 1000)}s (last status: ${last})`);
  }

  protected async create(): Promise<void> {
    const req: DeployRequest = {
      image: this.image,
      command: this.command,
      ...(Object.keys(this.creationEnv).length > 0 && { env_vars: this.creationEnv }),
      ...(this.ports.length > 0 && { ports: this.ports.map(port => ({ port, protocol: 'tcp' })) }),
      ...(this.resources?.cpu !== undefined && { cpu_limit: this.resources.cpu }),
      ...(this.resources?.memory !== undefined && { memory_limit: this.resources.memory }),
      ...(this.resources?.disk !== undefined && { volume_size: this.resources.disk }),
      enable_persistence: this.persistence,
      ...(this.volumePath && { volume_path: this.volumePath }),
      enable_ipv6: this.enableIpv6,
      ...(this.targetGarage && { target_garage: this.targetGarage }),
      ...(this.hardwarePreference && { hardware_preference: this.hardwarePreference }),
      ...(this.architecture && { architecture: this.architecture }),
      lifecycle: this.lifecycle,
      labels: { ...this.labels, [SANDBOX_ID_LABEL]: this.id },
    };
    this.logger.debug(`[Nordkraft] deploying ${this.image} for sandbox ${this.id}`);
    const res = await this.client.deploy(req);
    const name = res.container_name;
    if (!name) throw new NordkraftApiError(`deploy returned no container name: ${JSON.stringify(res)}`);
    this._lastName = name;
    this._createdAt = new Date();
    this._container = {
      container_id: '',
      name,
      image: this.image,
      status: res.status === 'deployed' ? 'Up' : 'deploying',
      created_at: this._createdAt.toISOString(),
      ports: [],
      container_ip: typeof res.container_ip === 'string' ? res.container_ip : null,
      labels: req.labels ?? {},
    };
    this.logger.debug(`[Nordkraft] ${res.status}: ${name} on ${String(res.node ?? 'controller')}`);
    await this.waitExecReady(name);
    await this.refresh();
    await this.prepareWorkingDirectory();
  }

  /**
   * Poll the exec endpoint until the container accepts commands. Covers the
   * asynchronous remote deploy (image pull on the node), a container that was
   * just started, and the controller's placement lookup catching up.
   */
  private async waitExecReady(name: string): Promise<void> {
    const deadline = Date.now() + this.startTimeout;
    let lastError = 'not ready';
    let tick = 0;
    while (Date.now() < deadline) {
      try {
        const r = await this.client.exec(name, { cmd: ['true'], timeout_s: 10 });
        if (r.exit_code === 0) return;
        lastError = r.error ?? `exit ${r.exit_code}`;
      } catch (error) {
        lastError = error instanceof Error ? error.message : String(error);
      }
      if (++tick % 5 === 0) {
        // Every few seconds, check whether the deploy failed outright so we don't wait out the whole timeout.
        const listed = (await this.client.list().catch(() => [] as ContainerInfo[])).find(c => c.name === name);
        if (listed && DEAD.test(listed.status)) {
          throw new NordkraftApiError(`container ${name} ${listed.status}${listed.status_message ? `: ${listed.status_message}` : ''}`);
        }
      }
      await sleep(1_000);
    }
    throw new NordkraftApiError(`container ${name} did not accept exec within ${Math.round(this.startTimeout / 1000)}s (${lastError})`);
  }

  /** Re-read the listing so ports and IP are current. */
  private async refresh(): Promise<void> {
    const name = this._container?.name ?? this._lastName;
    if (!name) return;
    const listed = (await this.client.list().catch(() => [] as ContainerInfo[])).find(c => c.name === name);
    if (listed) this._container = listed;
  }

  /**
   * Runs inside start(), so it must talk to the client directly: `this.exec()`
   * would call `ensureRunning()` and wait for the very start that is in progress.
   */
  private async prepareWorkingDirectory(): Promise<void> {
    const dir = this.workingDirectory;
    if (!dir || !this._container) return;
    const r = await this.client.exec(this._container.name, {
      cmd: ['sh', '-c', `mkdir -p ${shellQuote(dir)}`],
      user: this.user,
      timeout_s: 30,
    });
    if (r.exit_code !== 0) {
      this.logger.warn(`[Nordkraft] could not create working directory ${dir}: ${r.stderr || r.error}`);
    }
  }

  /**
   * Run `fn`; if it fails and the container turns out to be stopped (auto-stop
   * fired, node rebooted), start it again and retry once.
   */
  async retryOnDead<T>(fn: () => Promise<T>): Promise<T> {
    try {
      return await fn();
    } catch (error) {
      const name = this._container?.name;
      if (!name) throw error;
      const listed = (await this.client.list().catch(() => [] as ContainerInfo[])).find(c => c.name === name);
      if (!listed) {
        this._container = undefined;
        this.status = 'stopped';
        throw new NordkraftApiError(
          `sandbox container ${name} is gone (${error instanceof Error ? error.message : String(error)}); start() will create a new one`,
        );
      }
      if (isRunning(listed.status)) throw error;
      this.logger.debug(`[Nordkraft] container ${name} is ${listed.status}; reviving`);
      await this.revive(listed);
      return fn();
    }
  }

  private revive(listed: ContainerInfo): Promise<void> {
    if (!this._reviving) {
      this._reviving = (async () => {
        await this.client.start(listed.name);
        await this.waitUntilRunning(listed.name);
        await this.waitExecReady(listed.name);
        await this.refresh();
      })().finally(() => {
        this._reviving = undefined;
      });
    }
    return this._reviving;
  }

  // ---------------------------------------------------------------------------
  // Lifecycle
  // ---------------------------------------------------------------------------

  /**
   * Stop the container. Files stay; no compute is billed while stopped.
   *
   * On a remote node the stop is asynchronous (`stop_initiated`), so wait
   * until the node reports the container down: a `start()` issued while the
   * stop is still in flight races it and can leave the micro-VM in a state
   * where exec is refused.
   */
  async stop(): Promise<void> {
    const c = this._container ?? (await this.lookup().catch(() => undefined));
    if (c && isRunning(c.status)) {
      try {
        await this.client.stop(c.name);
      } catch (error) {
        if (!(error instanceof NordkraftApiError && error.notFound)) throw error;
      }
      await this.waitUntilStopped(c.name);
    }
    if (c) this._lastName = c.name;
    this._container = undefined;
  }

  private async waitUntilStopped(name: string): Promise<void> {
    const deadline = Date.now() + 60_000;
    while (Date.now() < deadline) {
      const listed = (await this.client.list().catch(() => undefined))?.find(c => c.name === name);
      if (!listed || !isRunning(listed.status)) return;
      await sleep(1_000);
    }
    this.logger.warn(`[Nordkraft] container ${name} still reported running 60s after stop`);
  }

  /** Remove the container and its volume. */
  async destroy(): Promise<void> {
    const c = this._container ?? (await this.lookup().catch(() => undefined));
    if (c) {
      try {
        await this.client.remove(c.name, true);
      } catch (error) {
        if (!(error instanceof NordkraftApiError && error.notFound)) throw error;
      }
    }
    this._container = undefined;
    this._lastName = undefined;
    this.mounts?.clear();
  }

  async isReady(): Promise<boolean> {
    return this.status === 'running' && this._container !== undefined;
  }

  async getInfo(): Promise<SandboxInfo> {
    const c = this._container;
    const memoryMB = sizeToMiB(this.resources?.memory);
    const diskMB = sizeToMiB(this.resources?.disk);
    return {
      id: this.id,
      name: this.name,
      provider: this.provider,
      status: this.status,
      createdAt: this._createdAt ?? new Date(),
      resources: {
        ...(this.resources?.cpu !== undefined && { cpuCores: this.resources.cpu }),
        ...(memoryMB !== undefined && { memoryMB }),
        ...(diskMB !== undefined && { diskMB }),
      },
      metadata: {
        apiUrl: this.client.apiUrl,
        image: this.image,
        lifecycle: this.lifecycle,
        ...(c && {
          containerName: c.name,
          containerStatus: c.status,
          containerIp: c.container_ip ?? undefined,
          ports: c.ports,
        }),
      },
    };
  }

  getInstructions(): string {
    const parts = [
      `Nordkraft sandbox: an isolated Kata micro-VM container (image ${this.image}) on Nordkraft Garage Cloud, reached over WireGuard.`,
      'Commands run through /bin/sh; background processes are supported.',
    ];
    if (this.workingDirectory) parts.push(`Default working directory: ${this.workingDirectory}.`);
    parts.push(`Command timeout: ${Math.ceil(this.timeout / 1000)}s.`);
    if (this.user) parts.push(`Running as user ${this.user}.`);
    if (this.lifecycle.auto_stop && this.lifecycle.auto_stop !== '0') {
      parts.push(`The sandbox auto-stops after ${this.lifecycle.auto_stop} without activity; files survive a stop.`);
    }
    if (this._container?.container_ip && this._container.container_ip !== 'pending') {
      parts.push(`Container IP ${this._container.container_ip} (WireGuard network only; not public).`);
    }
    return parts.join(' ');
  }

  // ---------------------------------------------------------------------------
  // Convenience: buffered exec and file upload
  // ---------------------------------------------------------------------------

  /**
   * Buffered, non-interactive exec through `POST /containers/{id}/exec`.
   * Unlike `executeCommand`, takes an argv array and runs no shell unless you ask for one.
   */
  async exec(
    cmd: string[],
    options: { cwd?: string; env?: Record<string, string>; stdin?: string; timeoutS?: number; user?: string } = {},
  ): Promise<ExecResult> {
    await this.ensureRunning();
    return this.retryOnDead(() =>
      this.client.exec(this.containerName, {
        cmd,
        cwd: options.cwd ?? this.workingDirectory,
        env: options.env,
        stdin: options.stdin,
        user: options.user ?? this.user,
        timeout_s: options.timeoutS ?? Math.max(1, Math.ceil(this.timeout / 1000)),
      }),
    );
  }

  /**
   * Write files into the container.
   *
   * Content travels base64-encoded in an environment variable, 32 KiB per
   * exec, and is decoded inside the container — deliberately not through
   * stdin. On Kata the agent short-writes a full 64 KiB stdin pipe and the
   * shim then drops the rest, so anything larger than one pipe buffer may
   * arrive truncated when the reader is slower than the sender (a file on
   * virtiofs is). Environment variables have no such path. 32 KiB keeps the
   * controller→node relay URL (where env travels as query parameters) well
   * under the ~48 KiB the node accepts, and far under the kernel's 128 KiB
   * single-argument limit.
   */
  async writeFiles(files: SandboxFileInput[]): Promise<void> {
    for (const f of files) if (f.mode !== undefined) validateSandboxFileMode(f.mode);
    await this.ensureRunning();
    const CHUNK = 32 * 1024;
    for (const file of files) {
      const bytes = Buffer.isBuffer(file.content) ? file.content : Buffer.from(file.content);
      const b64 = bytes.toString('base64');
      const path = file.path.startsWith('/') || !this.workingDirectory ? file.path : `${this.workingDirectory}/${file.path}`;
      const q = shellQuote(path);
      const steps: Array<{ script: string; env?: Record<string, string> }> = [];
      if (b64.length === 0) {
        steps.push({ script: `mkdir -p "$(dirname ${q})" && : > ${q}` });
      }
      for (let i = 0; i < b64.length; i += CHUNK) {
        const redirect = i === 0 ? '>' : '>>';
        steps.push({
          script: `${i === 0 ? `mkdir -p "$(dirname ${q})" && ` : ''}printf %s "$NK_B64" | base64 -d ${redirect} ${q}`,
          env: { NK_B64: b64.slice(i, i + CHUNK) },
        });
      }
      if (file.mode !== undefined) {
        steps.push({ script: `chmod ${file.mode.toString(8)} ${q}` });
      }
      for (const step of steps) {
        const r = await this.exec(['sh', '-c', step.script], { env: step.env, timeoutS: 60 });
        if (r.exit_code !== 0) {
          throw new NordkraftApiError(`writeFiles ${file.path}: ${r.stderr.trim() || r.error || `exit ${r.exit_code}`}`);
        }
      }
    }
  }
}
