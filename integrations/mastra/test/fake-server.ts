/**
 * A fake Nordkraft controller for unit tests.
 *
 * Speaks the subset of the REST API the provider uses and serves the
 * exec/shell WebSocket with the real framing: commands run through the host
 * `sh`, so stdout/stderr ordering, stdin, exit codes and kill-on-close are
 * exercised for real — only the container is pretend.
 */
import { spawn } from 'node:child_process';
import { createServer } from 'node:http';
import type { IncomingMessage, Server, ServerResponse } from 'node:http';
import { AddressInfo } from 'node:net';
import { WebSocketServer } from 'ws';
import type { WebSocket } from 'ws';
import { STATUS, STDERR, STDIN, STDOUT, frame } from '../src/protocol.js';

export interface FakeContainer {
  name: string;
  image: string;
  status: string;
  labels: Record<string, string>;
  container_ip: string;
  created_at: string;
  ports: Array<{ port: number; protocol: string; access_url: string }>;
  deployBody: Record<string, unknown>;
}

export interface ExecCall {
  name: string;
  body: { cmd: string[]; cwd?: string; env?: Record<string, string>; stdin?: string; timeout_s?: number; user?: string };
}

export interface StreamOpen {
  name: string;
  query: URLSearchParams;
  /** true once the client sent an empty stdin frame */
  sawEof: boolean;
}

export class FakeNordkraft {
  readonly containers = new Map<string, FakeContainer>();
  readonly execCalls: ExecCall[] = [];
  /** Files uploaded through PUT /containers/:name/files, keyed by "name:path". */
  readonly files = new Map<string, { bytes: Buffer; mode?: string }>();
  readonly streams: StreamOpen[] = [];
  readonly calls: string[] = [];
  /** Deploy answers `scheduled` and the container only turns Up after this many ms. */
  deployDelayMs = 0;
  /** Status to report for the first N exec probes (simulates a node still starting). */
  execFailuresBeforeReady = 0;
  private server!: Server;
  private wss!: WebSocketServer;
  private counter = 0;
  apiUrl = '';

  async start(): Promise<void> {
    this.server = createServer((req, res) => void this.handle(req, res).catch(e => this.fail(res, 500, String(e))));
    this.wss = new WebSocketServer({ noServer: true });
    this.server.on('upgrade', (req, socket, head) => {
      const url = new URL(req.url ?? '/', 'http://x');
      const m = /^\/api\/containers\/([^/]+)\/stream$/.exec(url.pathname);
      const c = m && this.containers.get(decodeURIComponent(m[1]!));
      if (!c) {
        socket.write('HTTP/1.1 404 Not Found\r\nContent-Length: 19\r\n\r\ncontainer not found');
        socket.destroy();
        return;
      }
      if (!/^Up/.test(c.status)) {
        socket.write('HTTP/1.1 404 Not Found\r\nContent-Length: 34\r\n\r\ncontainer not running on this node');
        socket.destroy();
        return;
      }
      this.wss.handleUpgrade(req, socket, head, ws => this.stream(ws, c, url.searchParams));
    });
    await new Promise<void>(resolve => this.server.listen(0, '127.0.0.1', resolve));
    const { port } = this.server.address() as AddressInfo;
    this.apiUrl = `http://127.0.0.1:${port}/api`;
  }

  async stop(): Promise<void> {
    for (const ws of this.wss.clients) ws.terminate();
    await new Promise<void>(resolve => this.wss.close(() => resolve()));
    await new Promise<void>(resolve => this.server.close(() => resolve()));
  }

  private fail(res: ServerResponse, code: number, msg: string) {
    res.writeHead(code, { 'content-type': 'text/plain' });
    res.end(msg);
  }

  private json(res: ServerResponse, body: unknown, code = 200) {
    res.writeHead(code, { 'content-type': 'application/json' });
    res.end(JSON.stringify(body));
  }

  private async body(req: IncomingMessage): Promise<Record<string, unknown>> {
    const chunks: Buffer[] = [];
    for await (const c of req) chunks.push(c as Buffer);
    const text = Buffer.concat(chunks).toString('utf8');
    return text ? (JSON.parse(text) as Record<string, unknown>) : {};
  }

  private async handle(req: IncomingMessage, res: ServerResponse): Promise<void> {
    const url = new URL(req.url ?? '/', 'http://x');
    const path = url.pathname.replace(/^\/api/, '');
    this.calls.push(`${req.method} ${path}${url.search}`);

    if (req.method === 'GET' && path === '/status') return this.json(res, { status: 'online', dev: true, node_id: 'fake' });

    if (req.method === 'GET' && path === '/containers') {
      const wanted = url.searchParams.getAll('label').map(kv => kv.split('=') as [string, string]);
      const list = [...this.containers.values()].filter(c => wanted.every(([k, v]) => c.labels[k] === v));
      return this.json(res, { containers: list.map(c => this.view(c)), source: 'local' });
    }

    if (req.method === 'POST' && path === '/containers/deploy') {
      const body = await this.body(req);
      if (typeof body.image !== 'string') return this.json(res, { error: 'image required' });
      const name = `app-fake-${++this.counter}`;
      const c: FakeContainer = {
        name,
        image: body.image,
        status: this.deployDelayMs > 0 ? 'deploying' : 'Up 1 second',
        labels: (body.labels as Record<string, string>) ?? {},
        container_ip: `172.21.1.${10 + this.counter}`,
        created_at: new Date().toISOString(),
        ports: ((body.ports as Array<{ port: number }>) ?? []).map(p => ({
          port: p.port,
          protocol: 'tcp',
          access_url: `http://127.0.0.1:${40000 + p.port}`,
        })),
        deployBody: body,
      };
      this.containers.set(name, c);
      if (this.deployDelayMs > 0) setTimeout(() => (c.status = 'Up 1 second'), this.deployDelayMs);
      return this.json(res, {
        status: this.deployDelayMs > 0 ? 'scheduled' : 'deployed',
        container_name: name,
        container_ip: c.container_ip,
        node: 'fake',
      });
    }

    const m = /^\/containers\/([^/]+)(?:\/(start|stop|exec|wait|files))?$/.exec(path);
    if (!m) return this.fail(res, 404, 'no route');
    const name = decodeURIComponent(m[1]!);
    const c = this.containers.get(name);
    const action = m[2];

    if (action === 'files') {
      if (!c) return this.fail(res, 404, 'container not found');
      if (!c.deployBody.sandbox) return this.fail(res, 400, 'file transfer needs a sandbox (deploy with "sandbox": true)');
      const filePath = url.searchParams.get('path') ?? '';
      if (req.method === 'PUT') {
        const chunks: Buffer[] = [];
        for await (const ch of req) chunks.push(ch as Buffer);
        const bytes = Buffer.concat(chunks);
        this.files.set(`${name}:${filePath}`, { bytes, mode: url.searchParams.get('mode') ?? undefined });
        return this.json(res, { path: filePath, bytes: bytes.length });
      }
      if (req.method === 'GET') {
        const f = this.files.get(`${name}:${filePath}`);
        if (!f) return this.fail(res, 404, 'no such file');
        res.writeHead(200, { 'content-type': 'application/octet-stream' });
        res.end(f.bytes);
        return;
      }
    }

    if (req.method === 'DELETE' && !action) {
      if (!c) return this.json(res, { error: `Container '${name}' not found` });
      this.containers.delete(name);
      return this.json(res, { status: 'deleted' });
    }
    if (!c) {
      // The real API answers 200 + error for lifecycle routes and 404 for exec/wait.
      if (action === 'exec' || action === 'wait') return this.fail(res, 404, 'container not found');
      return this.json(res, { error: `Container '${name}' not found` });
    }
    if (req.method === 'GET' && !action) return this.json(res, { ...this.view(c), node_id: 'fake', runtime: 'fake' });
    if (req.method === 'POST' && action === 'stop') {
      c.status = 'Exited (0) 1 second ago';
      return this.json(res, { status: 'stopped' });
    }
    if (req.method === 'POST' && action === 'start') {
      c.status = 'Up 1 second';
      return this.json(res, { status: 'started' });
    }
    if (req.method === 'POST' && action === 'exec') {
      const body = (await this.body(req)) as ExecCall['body'];
      this.execCalls.push({ name, body });
      if (!/^Up/.test(c.status)) return this.fail(res, 404, 'container not running on this node');
      if (this.execFailuresBeforeReady > 0) {
        this.execFailuresBeforeReady--;
        return this.json(res, { exit_code: null, stdout: '', stderr: '', duration_ms: 1, truncated: false, error: 'containerd exec: not found' });
      }
      return this.json(res, { exit_code: 0, stdout: '', stderr: '', duration_ms: 1, truncated: false });
    }
    return this.fail(res, 404, 'no route');
  }

  private view(c: FakeContainer) {
    return {
      container_id: c.name.slice(-12),
      name: c.name,
      image: c.image,
      status: c.status,
      created_at: c.created_at,
      ports: c.ports,
      container_ip: c.container_ip,
      labels: c.labels,
    };
  }

  /** One exec session over the k8s channel protocol, backed by a host `sh`. */
  private stream(ws: WebSocket, c: FakeContainer, query: URLSearchParams): void {
    const open: StreamOpen = { name: c.name, query, sawEof: false };
    this.streams.push(open);
    const cmd = query.getAll('cmd');
    const env: Record<string, string> = { PATH: process.env.PATH ?? '/usr/bin:/bin' };
    for (const kv of query.getAll('env')) {
      const i = kv.indexOf('=');
      env[kv.slice(0, i)] = kv.slice(i + 1);
    }
    const noStdin = query.get('stdin') === 'false';
    const argv = noStdin ? ['sh', '-c', 'exec "$0" "$@" </dev/null', ...cmd] : cmd;
    const child = spawn(argv[0]!, argv.slice(1), { env, cwd: process.cwd(), stdio: ['pipe', 'pipe', 'pipe'] });
    child.stdout.on('data', (d: Buffer) => ws.send(frame(STDOUT, d)));
    child.stderr.on('data', (d: Buffer) => ws.send(frame(STDERR, d)));
    let done = false;
    child.on('close', (code, signal) => {
      done = true;
      const status = code === null ? { error: `killed by ${signal}` } : { exit_code: code };
      try {
        ws.send(frame(STATUS, JSON.stringify(status)));
        ws.close(1000);
      } catch {
        /* client gone */
      }
    });
    ws.on('message', (data: Buffer | ArrayBuffer | Buffer[]) => {
      const bytes = Buffer.isBuffer(data) ? data : Array.isArray(data) ? Buffer.concat(data) : Buffer.from(data);
      if (bytes.length === 0) return;
      if (bytes[0] === STDIN) {
        if (bytes.length === 1) {
          open.sawEof = true;
          child.stdin.end();
        } else {
          child.stdin.write(bytes.subarray(1));
        }
      }
    });
    ws.on('close', () => {
      if (!done) child.kill('SIGKILL');
    });
  }
}
