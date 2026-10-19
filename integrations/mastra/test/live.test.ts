/**
 * Live test against a real Nordkraft controller. Deploys one small sandbox on
 * the caller's account and removes it again.
 *
 *   NORDKRAFT_LIVE=1 NORDKRAFT_API_URL=http://172.20.0.254:8001/api npm run test:live
 */
import { Workspace } from '@mastra/core/workspace';
import { describe, expect, it } from 'vitest';
import { NordkraftSandbox } from '../src/index.js';

const live = process.env.NORDKRAFT_LIVE === '1';

describe.skipIf(!live)('live Nordkraft sandbox', () => {
  it('runs the full lifecycle with complete output', { timeout: 300_000 }, async () => {
    const sandbox = new NordkraftSandbox({
      id: `mastra-live-${Date.now().toString(36)}`,
      image: process.env.IMAGE ?? 'alpine:3.21',
      resources: { cpu: 0.5, memory: '256m' },
      autoStopInterval: 5,
      autoDeleteInterval: 10,
      labels: { purpose: 'live-test' },
    });
    const workspace = new Workspace({ sandbox });
    try {
      const started = await sandbox.start();
      expect(started?.outcome).toBe('created');

      // Output after the first chunk must not be lost (the Kata CloseIO regression).
      const multi = await sandbox.executeCommand!('for i in 1 2 3 4 5; do echo line $i; sleep 0.05; done; seq 1 2000 | tail -1');
      expect(multi.exitCode).toBe(0);
      expect(multi.stdout).toBe('line 1\nline 2\nline 3\nline 4\nline 5\n2000\n');

      const noStdin = await sandbox.executeCommand!('cat; echo after-eof', [], { timeout: 20_000 });
      expect(noStdin.stdout).toBe('after-eof\n');
      expect(noStdin.timedOut).toBeFalsy();

      const blob = Buffer.alloc(700 * 1024, 7);
      await sandbox.writeFiles([{ path: 'blob.bin', content: blob, mode: 0o600 }]);
      const check = await sandbox.executeCommand!('wc -c < /workspace/blob.bin; stat -c %a /workspace/blob.bin');
      expect(check.stdout).toBe(`${blob.length}\n600\n`);

      const cat = await sandbox.processes.spawn('cat', { stdinMode: 'pipe' });
      await cat.sendStdin('piped\n');
      await cat.closeStdin();
      expect((await cat.wait()).stdout).toBe('piped\n');

      const slow = await sandbox.executeCommand!('sleep 30', [], { timeout: 1_500 });
      expect(slow.exitCode).toBe(124);

      await sandbox._stop();
      expect(sandbox.status).toBe('stopped');
      expect(await sandbox.start()).toEqual({ outcome: 'connected' });
      expect((await sandbox.executeCommand!('test -f /workspace/blob.bin && echo kept')).stdout).toBe('kept\n');
    } finally {
      await workspace.destroy();
    }
  });
});
