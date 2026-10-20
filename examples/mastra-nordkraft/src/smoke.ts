// Step 1 — no model involved. Proves the provider ↔ Nordkraft path alone.
//   npm run smoke
import { NordkraftSandbox } from './nordkraft-sandbox.js';

const sandbox = new NordkraftSandbox({
  apiBase: process.env.NORDKRAFT_API ?? 'http://172.20.0.254:8001/api',
  container: process.env.NORDKRAFT_CONTAINER ?? 'sandbox-1',
  image: 'docker.io/library/python:3.12-alpine',
});

const t0 = Date.now();
const start = await sandbox.start();
console.log('start:', start, `${Date.now() - t0} ms`);

const r1 = await sandbox.executeCommand('echo hello from $(hostname); python3 --version'); // shell line, as Mastra sends it
console.log('exec 1:', r1.exitCode, r1.stdout.trim(), `${r1.executionTimeMs} ms`);

await sandbox.writeFiles([{ path: '/work/hello.py', content: 'import sys; print("args:", sys.argv[1:]); sys.exit(3)\n' }]);
const r2 = await sandbox.executeCommand('python3', ['/work/hello.py', 'a', 'b'], { cwd: '/work' });
console.log('exec 2:', r2.exitCode, r2.stdout.trim(), r2.success ? 'success' : 'non-zero as expected');

const r3 = await sandbox.executeCommand('sleep', ['10'], { timeout: 2000 });
console.log('exec 3 (timeout):', r3.exitCode, r3.timedOut ? 'timed out' : '??');

console.log('port url:', await sandbox.networking.getPortUrl(8000));
console.log('info:', sandbox.getInfo().metadata);
