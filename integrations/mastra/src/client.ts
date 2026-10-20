/**
 * Thin client for the Nordkraft container API.
 *
 * Authentication is the WireGuard tunnel: the controller resolves the
 * caller's source address to an account, so there is no token to send.
 * In `--dev` mode the API accepts any caller on localhost.
 */

export const DEFAULT_API_URL = 'http://172.20.0.254:8001/api';

export interface PortMapping {
  port: number;
  protocol: string;
  access_url: string;
  ipv6_url?: string | null;
}

/** One row of `GET /containers`. */
export interface ContainerInfo {
  container_id: string;
  name: string;
  image: string;
  /** `Up 3 hours`, `Exited (0) 2 minutes ago`, or a DB state like `deploying` / `failed`. */
  status: string;
  status_message?: string | null;
  created_at: string;
  ports: PortMapping[];
  container_ip?: string | null;
  ipv6_address?: string | null;
  ipv6_enabled?: boolean;
  /** User labels with the `user.` runtime prefix stripped. */
  labels: Record<string, string>;
}

/** `GET /containers/{id}` — the inspect view. */
export interface ContainerInspect {
  container_id: string;
  name: string;
  image: string;
  status: string;
  created_at: string;
  started_at?: string | null;
  finished_at?: string | null;
  exit_code?: number | null;
  container_ip?: string | null;
  ports: unknown[];
  node_id: string;
  runtime: string;
  cpu_limit?: number | null;
  memory_limit?: number | null;
  labels: Record<string, string>;
}

export interface LifecycleSpec {
  restart?: 'always' | 'unless-stopped' | 'on-failure' | 'never';
  auto_stop?: string;
  auto_delete?: string;
  max_lifetime?: string;
}

export interface DeployRequest {
  image: string;
  command?: string[];
  env_vars?: Record<string, string>;
  ports?: Array<{ port: number; protocol?: string }>;
  cpu_limit?: number;
  memory_limit?: string;
  volume_size?: string;
  enable_persistence?: boolean;
  volume_path?: string;
  enable_ipv6?: boolean;
  target_garage?: string;
  hardware_preference?: string;
  architecture?: string;
  lifecycle?: LifecycleSpec;
  labels?: Record<string, string>;
  /**
   * Run nordkraft-sandboxd as the container's entrypoint: exec, process
   * streams, PTYs and file transfer are then served from inside the micro-VM
   * instead of through the runtime's exec path. `command` (if any) runs as
   * the daemon's child; without one the sandbox idles.
   */
  sandbox?: boolean;
}

export interface DeployResponse {
  /** `deployed` (ran on the controller, synchronous) or `scheduled` (handed to a node, asynchronous). */
  status: string;
  container_name: string;
  container_ip?: string;
  node?: string;
  garage?: string;
  network_mode?: string;
  access_urls?: string[];
  [key: string]: unknown;
}

export interface ExecRequest {
  cmd: string[];
  cwd?: string;
  env?: Record<string, string>;
  user?: string;
  stdin?: string;
  timeout_s?: number;
}

export interface ExecResult {
  exit_code: number | null;
  stdout: string;
  stderr: string;
  duration_ms: number;
  truncated: boolean;
  error?: string;
}

export interface WaitResult {
  exit_code: number | null;
  timed_out: boolean;
}

export interface StreamSpec {
  cmd: string[];
  cwd?: string;
  env?: Record<string, string>;
  user?: string;
  tty?: boolean;
  /**
   * Attach stdin (server default: true). `false` gives the process EOF from
   * /dev/null right away and never closes stdin through containerd, which is
   * what you want for commands that will not read input.
   */
  stdin?: boolean;
  cols?: number;
  rows?: number;
}

export class NordkraftApiError extends Error {
  constructor(
    message: string,
    readonly httpStatus?: number,
    readonly body?: unknown,
  ) {
    super(message);
    this.name = 'NordkraftApiError';
  }

  get notFound(): boolean {
    return this.httpStatus === 404 || /not found|not running on this node/i.test(this.message);
  }
}

export interface NordkraftClientOptions {
  /** `http://host:8001`, `http://host:8001/api` — the `/api` suffix is added when missing. */
  apiUrl?: string;
  /** Override for tests. */
  fetch?: typeof fetch;
}

/**
 * Normalise an API base URL the same way the CLI does: no trailing slash,
 * `/api` appended when the path is empty.
 */
export function normaliseApiUrl(input: string): string {
  let url = input.trim();
  if (!/^[a-z]+:\/\//i.test(url)) url = `http://${url}`;
  const parsed = new URL(url);
  let path = parsed.pathname.replace(/\/+$/, '');
  if (path === '') path = '/api';
  parsed.pathname = path;
  parsed.search = '';
  parsed.hash = '';
  return parsed.toString().replace(/\/+$/, '');
}

export class NordkraftClient {
  readonly apiUrl: string;
  private readonly _fetch: typeof fetch;

  constructor(options: NordkraftClientOptions = {}) {
    this.apiUrl = normaliseApiUrl(options.apiUrl ?? process.env.NORDKRAFT_API_URL ?? DEFAULT_API_URL);
    this._fetch = options.fetch ?? globalThis.fetch.bind(globalThis);
  }

  /** `ws://` or `wss://` form of {@link apiUrl}. */
  get wsUrl(): string {
    return this.apiUrl.replace(/^http/i, 'ws');
  }

  async status(): Promise<Record<string, unknown>> {
    return this.request('GET', '/status');
  }

  /** Containers owned by the caller, optionally only those carrying every given label. */
  async list(labels?: Record<string, string>): Promise<ContainerInfo[]> {
    const qs = new URLSearchParams();
    for (const [k, v] of Object.entries(labels ?? {})) qs.append('label', `${k}=${v}`);
    const q = qs.toString();
    const body = await this.request<{ containers?: ContainerInfo[] }>('GET', `/containers${q ? `?${q}` : ''}`);
    return body.containers ?? [];
  }

  /** Inspect one container; `undefined` when it does not exist or is not ours. */
  async inspect(name: string): Promise<ContainerInspect | undefined> {
    try {
      return await this.request<ContainerInspect>('GET', `/containers/${encodeURIComponent(name)}`);
    } catch (error) {
      if (error instanceof NordkraftApiError && error.notFound) return undefined;
      throw error;
    }
  }

  async deploy(req: DeployRequest): Promise<DeployResponse> {
    return this.request('POST', '/containers/deploy', req);
  }

  async start(name: string): Promise<void> {
    await this.request('POST', `/containers/${encodeURIComponent(name)}/start`);
  }

  async stop(name: string): Promise<void> {
    await this.request('POST', `/containers/${encodeURIComponent(name)}/stop`);
  }

  async remove(name: string, force = true): Promise<void> {
    await this.request('DELETE', `/containers/${encodeURIComponent(name)}${force ? '?force=true' : ''}`);
  }

  /** Buffered, non-interactive exec. Output capped at 1 MiB by the server. */
  async exec(name: string, req: ExecRequest): Promise<ExecResult> {
    return this.request('POST', `/containers/${encodeURIComponent(name)}/exec`, req);
  }

  /** Long-poll until the container's main process exits. */
  async wait(name: string, timeoutS = 300): Promise<WaitResult> {
    return this.request('POST', `/containers/${encodeURIComponent(name)}/wait?timeout_s=${timeoutS}`);
  }

  /**
   * Upload a file into a sandbox (one deployed with `sandbox: true`). The
   * controller streams it to nordkraft-sandboxd inside the container, which
   * creates parent directories and writes atomically.
   */
  async putFile(name: string, path: string, content: Uint8Array, mode?: number): Promise<{ path: string; bytes: number }> {
    const qs = new URLSearchParams({ path });
    if (mode !== undefined) qs.set('mode', mode.toString(8));
    const url = `${this.apiUrl}/containers/${encodeURIComponent(name)}/files?${qs}`;
    const res = await this._fetch(url, {
      method: 'PUT',
      headers: { 'content-type': 'application/octet-stream' },
      body: content as unknown as BodyInit,
    });
    const text = await res.text();
    if (!res.ok) throw new NordkraftApiError(`PUT files ${path} → ${res.status}${text ? `: ${text}` : ''}`, res.status, text);
    return JSON.parse(text) as { path: string; bytes: number };
  }

  /** Download a file from a sandbox. */
  async getFile(name: string, path: string): Promise<Buffer> {
    const qs = new URLSearchParams({ path });
    const url = `${this.apiUrl}/containers/${encodeURIComponent(name)}/files?${qs}`;
    const res = await this._fetch(url);
    if (!res.ok) {
      const text = await res.text();
      throw new NordkraftApiError(`GET files ${path} → ${res.status}${text ? `: ${text}` : ''}`, res.status, text);
    }
    return Buffer.from(await res.arrayBuffer());
  }

  /** WebSocket URL for an exec/shell session. */
  streamUrl(name: string, spec: StreamSpec): string {
    const qs = new URLSearchParams();
    qs.set('tty', spec.tty ? 'true' : 'false');
    if (spec.stdin !== undefined) qs.set('stdin', spec.stdin ? 'true' : 'false');
    for (const c of spec.cmd) qs.append('cmd', c);
    if (spec.cwd) qs.set('cwd', spec.cwd);
    for (const [k, v] of Object.entries(spec.env ?? {})) qs.append('env', `${k}=${v}`);
    if (spec.user) qs.set('user', spec.user);
    if (spec.cols) qs.set('cols', String(spec.cols));
    if (spec.rows) qs.set('rows', String(spec.rows));
    return `${this.wsUrl}/containers/${encodeURIComponent(name)}/stream?${qs.toString()}`;
  }

  private async request<T = Record<string, unknown>>(method: string, path: string, body?: unknown): Promise<T> {
    const url = `${this.apiUrl}${path}`;
    let res: Response;
    try {
      res = await this._fetch(url, {
        method,
        headers: body !== undefined ? { 'content-type': 'application/json' } : undefined,
        body: body !== undefined ? JSON.stringify(body) : undefined,
      });
    } catch (error) {
      throw new NordkraftApiError(
        `Nordkraft API unreachable at ${this.apiUrl} (${error instanceof Error ? error.message : String(error)}). Is WireGuard connected?`,
      );
    }
    const text = await res.text();
    let parsed: unknown = undefined;
    if (text) {
      try {
        parsed = JSON.parse(text);
      } catch {
        parsed = text;
      }
    }
    if (!res.ok) {
      const detail = typeof parsed === 'string' ? parsed : (parsed as { error?: string })?.error ?? text;
      throw new NordkraftApiError(`${method} ${path} → ${res.status}${detail ? `: ${detail}` : ''}`, res.status, parsed);
    }
    // Most container routes answer 200 with {"error": "..."} on failure.
    if (parsed && typeof parsed === 'object' && 'error' in parsed && (parsed as { error?: unknown }).error) {
      throw new NordkraftApiError(`${method} ${path}: ${String((parsed as { error: unknown }).error)}`, res.status, parsed);
    }
    return parsed as T;
  }
}
