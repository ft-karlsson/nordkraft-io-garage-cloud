/**
 * Drive a Nordkraft sandbox through Mastra's Workspace without an LLM.
 *
 *   NORDKRAFT_API_URL=http://172.20.0.254:8001/api npm run example:workspace
 *
 * Deploys a small Alpine sandbox on your Nordkraft account, runs commands
 * through the same tools an agent would use, and removes it again.
 * Set KEEP=1 to leave the sandbox running (reattach with the printed id).
 */
import { Workspace, WORKSPACE_TOOLS, createWorkspaceTools } from '@mastra/core/workspace';
import { NordkraftSandbox } from '../src/index.js';

const t0 = Date.now();
const log = (msg: string) => console.log(`[${String(Date.now() - t0).padStart(6)} ms] ${msg}`);

const sandbox = new NordkraftSandbox({
  id: process.env.SANDBOX_ID ?? 'mastra-example',
  image: process.env.IMAGE ?? 'alpine:3.21',
  resources: { cpu: 0.5, memory: '256m' },
  autoStopInterval: 15,
  autoDeleteInterval: 60,
  labels: { purpose: 'mastra-example' },
  env: { GREETING: 'hej fra Nordkraft' },
});

const workspace = new Workspace({ sandbox });

try {
  log('workspace.init() → find or deploy the container');
  await workspace.init();
  log(`sandbox ready: ${sandbox.sandboxId} (${sandbox.container?.status ?? '?'}, ip ${sandbox.container?.container_ip ?? '-'})`);

  const run = async (cmd: string) => {
    const r = await sandbox.executeCommand!(cmd);
    log(`$ ${cmd}\n    exit=${r.exitCode} ${r.executionTimeMs} ms\n    stdout: ${JSON.stringify(r.stdout)}\n    stderr: ${JSON.stringify(r.stderr)}`);
    return r;
  };

  await run('uname -a; cat /etc/os-release | head -2; pwd; echo "$GREETING"');
  await run('echo out; echo err >&2; exit 3');

  log('writeFiles → /workspace/hello.py + a binary blob');
  const blob = Buffer.alloc(3000, 7);
  await sandbox.writeFiles([
    { path: 'hello.py', content: 'print("hello from a Nordkraft micro-VM")\n', mode: 0o644 },
    { path: '/workspace/blob.bin', content: blob },
  ]);
  await run('ls -la /workspace; wc -c /workspace/blob.bin; md5sum /workspace/blob.bin');

  log('background process with streaming output + kill');
  const handle = await sandbox.processes.spawn('i=0; while :; do i=$((i+1)); echo tick $i; sleep 0.5; done', {
    onStdout: d => process.stdout.write(`    › ${d}`),
  });
  await new Promise(r => setTimeout(r, 2200));
  const list = await sandbox.processes.list();
  log(`processes.list(): ${JSON.stringify(list)}`);
  const killed = await handle.kill();
  const result = await handle.wait();
  log(`killed=${killed} exit=${result.exitCode} killed=${result.killed}`);

  log('stdin round-trip');
  const cat = await sandbox.processes.spawn('cat', { stdinMode: 'pipe' });
  await cat.sendStdin('piped through the websocket\n');
  await cat.closeStdin();
  const catResult = await cat.wait();
  log(`cat → ${JSON.stringify(catResult.stdout)} exit=${catResult.exitCode}`);

  log('timeout → 124');
  const slow = await sandbox.executeCommand!('sleep 30', [], { timeout: 1500 });
  log(`sleep 30 with 1.5s timeout → exit=${slow.exitCode} timedOut=${slow.timedOut}`);

  log('the agent-facing tool: mastra_workspace_execute_command');
  const tools = await createWorkspaceTools(workspace);
  const exec = tools[WORKSPACE_TOOLS.SANDBOX.EXECUTE_COMMAND];
  const toolResult = await exec.execute({ command: 'echo "$(hostname): $(nproc) cpu, $(free -m | awk \'/Mem/{print $2}\') MB"' }, { workspace });
  log(`tool result: ${JSON.stringify(toolResult)}`);

  log(`getInfo: ${JSON.stringify(await sandbox.getInfo())}`);
  log(`instructions: ${sandbox.getInstructions()}`);
  log(`port 8080 url: ${await sandbox.networking.getPortUrl(8080)}`);
} finally {
  if (process.env.KEEP === '1') {
    log(`KEEP=1: leaving ${sandbox.sandboxId} running (auto-stop ${15} min, auto-delete 60 min after stop)`);
  } else {
    log('destroy()');
    await workspace.destroy();
    log('done');
  }
}
