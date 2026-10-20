import { Workspace, WORKSPACE_TOOLS, createWorkspaceTools } from '@mastra/core/workspace';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { WebSocket as WsWebSocket } from 'ws';
import { NordkraftSandbox, normaliseApiUrl, SANDBOX_ID_LABEL } from '../src/index.js';
import type { WebSocketConstructor } from '../src/index.js';
import { FakeNordkraft } from './fake-server.js';

// Node 22+ has a global WebSocket; fall back to `ws` where it is missing.
const WebSocketImpl = ((globalThis as { WebSocket?: WebSocketConstructor }).WebSocket ??
  (WsWebSocket as unknown)) as WebSocketConstructor;

let api: FakeNordkraft;

beforeEach(async () => {
  api = new FakeNordkraft();
  await api.start();
});

afterEach(async () => {
  await api.stop();
});

function make(overrides: ConstructorParameters<typeof NordkraftSandbox>[0] = {}) {
  return new NordkraftSandbox({
    id: 'test-sandbox',
    apiUrl: api.apiUrl,
    image: 'alpine:3.21',
    timeout: 10_000,
    startTimeout: 5_000,
    WebSocket: WebSocketImpl,
    ...overrides,
  });
}

describe('normaliseApiUrl', () => {
  it('appends /api and strips trailing slashes', () => {
    expect(normaliseApiUrl('http://172.20.0.254:8001')).toBe('http://172.20.0.254:8001/api');
    expect(normaliseApiUrl('http://172.20.0.254:8001/')).toBe('http://172.20.0.254:8001/api');
    expect(normaliseApiUrl('http://172.20.0.254:8001/api/')).toBe('http://172.20.0.254:8001/api');
    expect(normaliseApiUrl('localhost:8001')).toBe('http://localhost:8001/api');
  });
});

describe('lifecycle', () => {
  it('creates a labelled container on first start and reconnects on the next', async () => {
    const a = make({ labels: { team: 'x' }, resources: { cpu: 1, memory: '1g', disk: '2g' }, env: { FOO: 'bar' }, autoStopInterval: 30, autoDeleteInterval: 120, maxLifetimeMinutes: 1440 });
    const first = await a.start();
    expect(first).toEqual({ outcome: 'created' });
    expect(a.status).toBe('running');
    expect(a.sandboxId).toMatch(/^app-fake-/);

    const c = api.containers.get(a.sandboxId!)!;
    expect(c.labels).toEqual({ team: 'x', [SANDBOX_ID_LABEL]: 'test-sandbox' });
    expect(c.deployBody).toMatchObject({
      image: 'alpine:3.21',
      sandbox: true,
      cpu_limit: 1,
      memory_limit: '1g',
      volume_size: '2g',
      env_vars: { FOO: 'bar' },
      lifecycle: { restart: 'never', auto_stop: '30m', auto_delete: '120m', max_lifetime: '1440m' },
    });
    // Under the daemon an idle sandbox needs no keep-alive command.
    expect(c.deployBody.command).toBeUndefined();
    // The working directory is created through the buffered exec, not the stream.
    expect(api.execCalls.some(e => e.body.cmd[2]?.includes('mkdir -p /workspace'))).toBe(true);

    const b = make();
    expect(await b.start()).toEqual({ outcome: 'connected' });
    expect(b.sandboxId).toBe(a.sandboxId);
    expect(api.containers.size).toBe(1);
  });

  it('waits for a scheduled remote deploy to come up', async () => {
    api.deployDelayMs = 300;
    api.execFailuresBeforeReady = 1;
    const sb = make();
    const t0 = Date.now();
    await sb.start();
    expect(Date.now() - t0).toBeGreaterThanOrEqual(250);
    expect(api.containers.get(sb.sandboxId!)!.status).toMatch(/^Up/);
  });

  it('stop keeps the container, start resumes it, destroy removes it', async () => {
    // `_stop()` / `_destroy()` are the status-tracking wrappers Workspace uses;
    // the bare `stop()` / `destroy()` are the provider hooks underneath.
    const sb = make();
    await sb.start();
    const name = sb.sandboxId!;
    await sb._stop();
    expect(sb.status).toBe('stopped');
    expect(api.containers.get(name)!.status).toMatch(/^Exited/);

    expect(await sb.start()).toEqual({ outcome: 'connected' });
    expect(api.calls).toContain(`POST /containers/${name}/start`);
    expect(api.containers.get(name)!.status).toMatch(/^Up/);

    await sb._destroy();
    expect(sb.status).toBe('destroyed');
    expect(api.containers.has(name)).toBe(false);
  });

  it('attaches to an existing container by name with sandboxId', async () => {
    const a = make({ id: 'owner' });
    await a.start();
    const b = make({ id: 'other', sandboxId: a.sandboxId });
    expect(await b.start()).toEqual({ outcome: 'connected' });
    expect(b.sandboxId).toBe(a.sandboxId);
  });

  it('daemon: false deploys a plain container with a keep-alive command', async () => {
    const sb = make({ daemon: false });
    await sb.start();
    const body = api.containers.get(sb.sandboxId!)!.deployBody;
    expect(body.sandbox).toBeUndefined();
    expect(body.command).toEqual(['tail', '-f', '/dev/null']);
  });

  it('ephemeral maps to auto_delete 1s', async () => {
    const sb = make({ ephemeral: true });
    await sb.start();
    expect(api.containers.get(sb.sandboxId!)!.deployBody.lifecycle).toMatchObject({ auto_delete: '1s' });
  });
});

describe('commands over the stream', () => {
  it('keeps multi-line stdout and stderr apart and reports the exit code', async () => {
    const sb = make();
    const r = await sb.executeCommand!('for i in 1 2 3; do echo line $i; done; echo oops >&2; exit 3');
    expect(r.exitCode).toBe(3);
    expect(r.success).toBe(false);
    expect(r.stdout).toBe('line 1\nline 2\nline 3\n');
    expect(r.stderr).toBe('oops\n');
    expect(r.command).toContain('for i in 1 2 3');
  });

  it('executeCommand attaches no stdin: stdin=false on the wire and no EOF frame', async () => {
    const sb = make();
    const r = await sb.executeCommand!('cat; echo after');
    expect(r.exitCode).toBe(0);
    expect(r.stdout).toBe('after\n');
    const open = api.streams.at(-1)!;
    expect(open.query.get('stdin')).toBe('false');
    expect(open.query.get('tty')).toBe('false');
    expect(open.query.getAll('cmd')).toEqual(['sh', '-c', 'cat; echo after']);
    expect(open.sawEof).toBe(false);
  });

  it('passes cwd and env, with the sandbox env overlay underneath', async () => {
    const sb = make({ env: { FROM_SANDBOX: 'yes' } });
    const r = await sb.executeCommand!('pwd; echo $FROM_SANDBOX $PER_CALL', [], { cwd: '/tmp', env: { PER_CALL: 'also' } });
    const open = api.streams.at(-1)!;
    expect(open.query.get('cwd')).toBe('/tmp');
    expect(open.query.getAll('env')).toEqual(expect.arrayContaining(['FROM_SANDBOX=yes', 'PER_CALL=also']));
    expect(r.stdout).toContain('yes also');
  });

  it('shell-quotes args', async () => {
    const sb = make();
    const r = await sb.executeCommand!('printf', ['%s|', 'a b', "it's"]);
    expect(r.stdout).toBe("a b|it's|");
  });

  it('spawn pipes stdin and closes it with an EOF frame', async () => {
    const sb = make();
    const handle = await sb.processes.spawn('cat', { stdinMode: 'pipe' });
    await handle.sendStdin('hello ');
    await handle.sendStdin('world\n');
    await handle.closeStdin();
    const r = await handle.wait();
    expect(r.stdout).toBe('hello world\n');
    expect(r.exitCode).toBe(0);
    expect(api.streams.at(-1)!.sawEof).toBe(true);
    expect(api.streams.at(-1)!.query.get('stdin')).toBe('true');
  });

  it('streams output while running, lists the process, and kill() closes the session', async () => {
    const sb = make();
    const chunks: string[] = [];
    const handle = await sb.processes.spawn('i=0; while :; do i=$((i+1)); echo tick $i; sleep 0.05; done', {
      onStdout: d => chunks.push(d),
    });
    await new Promise(r => setTimeout(r, 300));
    expect(chunks.join('')).toContain('tick 2');
    const list = await sb.processes.list();
    expect(list).toEqual([expect.objectContaining({ pid: handle.pid, running: true })]);

    expect(await handle.kill()).toBe(true);
    const r = await handle.wait();
    expect(r.killed).toBe(true);
    expect(r.exitCode).toBe(137);
    expect(await handle.kill()).toBe(false);
    expect((await sb.processes.list())[0]).toMatchObject({ running: false, exitCode: 137 });
  });

  it('timeout kills the process and reports 124', async () => {
    const sb = make();
    const t0 = Date.now();
    const r = await sb.executeCommand!('sleep 30', [], { timeout: 300 });
    expect(Date.now() - t0).toBeLessThan(5_000);
    expect(r.timedOut).toBe(true);
    expect(r.exitCode).toBe(124);
    expect(r.success).toBe(false);
  });

  it('abort signal kills the process', async () => {
    const sb = make();
    const ac = new AbortController();
    const handle = await sb.processes.spawn('sleep 30', { abortSignal: ac.signal });
    setTimeout(() => ac.abort(), 100);
    const r = await handle.wait();
    expect(r.killed).toBe(true);
  });

  it('revives an auto-stopped container before running', async () => {
    const sb = make();
    await sb.start();
    // Simulate the node's auto_stop reaper.
    api.containers.get(sb.sandboxId!)!.status = 'Exited (0) 5 minutes ago';
    const r = await sb.executeCommand!('echo back');
    expect(r.stdout).toBe('back\n');
    expect(api.calls).toContain(`POST /containers/${sb.sandboxId}/start`);
  });

  it('a vanished container is reported, and the next start creates a new one', async () => {
    const sb = make();
    await sb.start();
    const old = sb.sandboxId!;
    api.containers.delete(old);
    await expect(sb.executeCommand!('true')).rejects.toThrow(/gone/);
    await sb.start();
    expect(sb.sandboxId).not.toBe(old);
  });
});

describe('writeFiles', () => {
  it('uploads through the sandbox file endpoint when the daemon is on', async () => {
    const sb = make();
    await sb.start();
    api.execCalls.length = 0;
    const big = Buffer.alloc(3 * 1024 * 1024, 9);
    await sb.writeFiles([
      { path: 'nested/dir/big.bin', content: big, mode: 0o640 },
      { path: '/abs/run.sh', content: '#!/bin/sh\necho hi\n' },
    ]);
    const stored = api.files.get(`${sb.sandboxId}:/workspace/nested/dir/big.bin`)!;
    expect(stored.bytes.equals(big)).toBe(true);
    expect(stored.mode).toBe('640');
    expect(api.files.get(`${sb.sandboxId}:/abs/run.sh`)!.bytes.toString()).toBe('#!/bin/sh\necho hi\n');
    expect(api.execCalls).toHaveLength(0);
  });

  it('falls back to base64 through env in 32 KiB pieces with mkdir, append and chmod — never stdin', async () => {
    const sb = make({ daemon: false });
    await sb.start();
    api.execCalls.length = 0;
    const big = Buffer.alloc(60 * 1024, 1); // 80 KiB of base64 → three pieces
    await sb.writeFiles([
      { path: 'nested/dir/big.bin', content: big },
      { path: '/abs/run.sh', content: '#!/bin/sh\necho hi\n', mode: 0o755 },
      { path: 'empty.txt', content: '' },
    ]);
    const scripts = api.execCalls.map(e => e.body.cmd[2]!);
    expect(scripts[0]).toBe(`mkdir -p "$(dirname /workspace/nested/dir/big.bin)" && printf %s "$NK_B64" | base64 -d > /workspace/nested/dir/big.bin`);
    expect(scripts[1]).toBe(`printf %s "$NK_B64" | base64 -d >> /workspace/nested/dir/big.bin`);
    expect(scripts[2]).toBe(`printf %s "$NK_B64" | base64 -d >> /workspace/nested/dir/big.bin`);
    const pieces = api.execCalls.slice(0, 3).map(e => e.body.env!.NK_B64!);
    expect(pieces.every(p => p.length <= 32 * 1024)).toBe(true);
    expect(pieces.join('')).toBe(big.toString('base64'));
    expect(api.execCalls.every(e => e.body.stdin === undefined)).toBe(true);
    expect(scripts[3]).toBe(`mkdir -p "$(dirname /abs/run.sh)" && printf %s "$NK_B64" | base64 -d > /abs/run.sh`);
    expect(scripts[4]).toBe(`chmod 755 /abs/run.sh`);
    expect(scripts[5]).toBe(`mkdir -p "$(dirname /workspace/empty.txt)" && : > /workspace/empty.txt`);
  });

  it('rejects invalid modes before touching the API', async () => {
    const sb = make();
    await expect(sb.writeFiles([{ path: 'x', content: 'y', mode: 0o1777 }])).rejects.toThrow();
  });
});

describe('info, networking and clone', () => {
  it('exposes container details and private port URLs', async () => {
    const sb = make({ ports: [8080], resources: { cpu: 2, memory: '512m' } });
    await sb.start();
    const info = await sb.getInfo();
    expect(info.resources).toEqual({ cpuCores: 2, memoryMB: 512 });
    expect(info.metadata).toMatchObject({ containerName: sb.sandboxId, image: 'alpine:3.21' });
    expect(await sb.networking.getPortUrl(8080)).toBe('http://127.0.0.1:48080');
    expect(await sb.networking.getPortUrl(9000)).toMatch(/^http:\/\/172\.21\.1\.\d+:9000$/);
    expect(sb.getInstructions()).toContain('Kata micro-VM');
  });

  it('clone inherits config with overrides and no I/O', () => {
    const sb = make({ labels: { a: 'b' } });
    const c = sb.clone({ id: 'clone-1', env: { X: '1' }, idleTimeoutMinutes: 5, workingDirectory: '/w' });
    expect(c.id).toBe('clone-1');
    expect(c.image).toBe('alpine:3.21');
    expect(c.workingDirectory).toBe('/w');
    expect(c.status).toBe('pending');
    expect(api.calls).toHaveLength(0);
  });
});

describe('through Mastra Workspace', () => {
  it('serves the execute_command tool an agent would call', async () => {
    const sandbox = make();
    const workspace = new Workspace({ sandbox });
    await workspace.init();
    const tools = await createWorkspaceTools(workspace);
    expect(Object.keys(tools)).toEqual(
      expect.arrayContaining([WORKSPACE_TOOLS.SANDBOX.EXECUTE_COMMAND, WORKSPACE_TOOLS.SANDBOX.GET_PROCESS_OUTPUT, WORKSPACE_TOOLS.SANDBOX.KILL_PROCESS]),
    );
    const out = await tools[WORKSPACE_TOOLS.SANDBOX.EXECUTE_COMMAND].execute({ command: 'echo tool-ok; echo warn >&2' }, { workspace });
    expect(String(out)).toContain('tool-ok');
    await workspace.destroy();
    expect(api.containers.size).toBe(0);
  });
});
