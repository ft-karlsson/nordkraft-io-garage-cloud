/**
 * A real agent loop on a Nordkraft sandbox.
 *
 *   ANTHROPIC_API_KEY=… NORDKRAFT_API_URL=http://172.20.0.254:8001/api npm run example:agent
 *
 * Mastra's model router reads ANTHROPIC_API_KEY (or OPENAI_API_KEY with
 * MODEL=openai/…). The agent receives the workspace tools automatically.
 */
import { Agent } from '@mastra/core/agent';
import { Workspace } from '@mastra/core/workspace';
import { NordkraftSandbox } from '../src/index.js';

const sandbox = new NordkraftSandbox({
  id: 'mastra-agent-example',
  image: process.env.IMAGE ?? 'python:3.12-alpine',
  resources: { cpu: 1, memory: '512m' },
  autoStopInterval: 15,
  autoDeleteInterval: 60,
});

const agent = new Agent({
  id: 'nordkraft-coder',
  name: 'Nordkraft coder',
  instructions:
    'You work inside a Linux sandbox. Use the execute_command tool to inspect the environment and run code. ' +
    'Verify results by running them; report what you ran and what came back.',
  model: process.env.MODEL ?? 'anthropic/claude-opus-5-5',
  workspace: new Workspace({ sandbox }),
});

const task =
  process.argv.slice(2).join(' ') ||
  'Write a Python script /workspace/primes.py that prints the first 10 primes, run it, and tell me the output and which kernel the sandbox runs.';

try {
  const result = await agent.generate(task);
  console.log('\n--- agent ---\n' + result.text);
  console.log(`\nsandbox: ${sandbox.sandboxId} (${(await sandbox.getInfo()).metadata?.containerIp ?? 'no ip'})`);
} finally {
  if (process.env.KEEP !== '1') await sandbox._destroy();
}
