// Step 2 — the real thing: a Mastra agent, Mistral as the brain, a Nordkraft
// container as the hands. Mastra exposes execute_command etc. to the model;
// every tool call becomes POST /exec on the controller.
//   MISTRAL_API_KEY=… npm run agent
import { Agent } from '@mastra/core/agent';
import { Workspace } from '@mastra/core/workspace';
import { NordkraftSandbox } from './nordkraft-sandbox.js';

if (!process.env.MISTRAL_API_KEY) throw new Error('MISTRAL_API_KEY is not set');

const sandbox = new NordkraftSandbox({
  apiBase: process.env.NORDKRAFT_API ?? 'http://172.20.0.254:8001/api',
  container: process.env.NORDKRAFT_CONTAINER ?? 'sandbox-1',
  image: 'docker.io/library/python:3.12-alpine',
  workingDirectory: '/work',
});

const agent = new Agent({
  id: 'nordkraft-sandbox-agent',
  name: 'Nordkraft sandbox agent',
  model: process.env.MODEL ?? 'mistral/mistral-medium-latest',
  instructions: [
    'You have a Linux sandbox (Alpine, python3) reachable through your tools. Your working directory is /work (create it with mkdir -p if a command says it is missing).',
    'Do the task by running commands in it. Keep commands short and check their output.',
    'When done, answer with the result only.',
  ].join(' '),
  workspace: new Workspace({ sandbox }),
});

const task =
  process.argv.slice(2).join(' ') ||
  'Write a python script /work/primes.py that prints the first 10 prime numbers, run it, and report the output.';

console.log('task:', task);
const res = await agent.generate(task, { maxSteps: 12 });

for (const step of res.steps ?? []) {
  for (const call of step.toolCalls ?? []) {
    const p = call.payload as { toolName?: string; args?: unknown };
    console.log('→ tool', p.toolName, JSON.stringify(p.args ?? {}).slice(0, 160));
  }
  for (const result of step.toolResults ?? []) {
    const p = result.payload as { result?: unknown };
    console.log('   ←', JSON.stringify(p.result ?? result).slice(0, 300));
  }
}
console.log('\nanswer:\n' + res.text);
