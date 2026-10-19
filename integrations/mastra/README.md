# @nordkraft/mastra

Nordkraft Garage Cloud as a sandbox provider for [Mastra](https://mastra.ai) workspaces.
A drop-in alternative to `@mastra/daytona` / `@mastra/e2b` that runs the agent's
commands in a Kata micro-VM on hardware you control — self-hosted or on
[Nordkraft Cloud](https://cloud.nordkraft.io) — reached over your WireGuard tunnel.
Fully open source (AGPL-3.0), no API keys: the tunnel is the credential.

```ts
import { Agent } from '@mastra/core/agent';
import { Workspace } from '@mastra/core/workspace';
import { NordkraftSandbox } from '@nordkraft/mastra';

export const codingAgent = new Agent({
  id: 'coding-agent',
  name: 'Coding agent',
  instructions: 'Use the sandbox to complete coding tasks.',
  model: 'anthropic/claude-opus-5-5',
  workspace: new Workspace({
    sandbox: new NordkraftSandbox({
      id: 'coding-agent',           // reused on the next start
      image: 'python:3.12-alpine',  // anything with /bin/sh
      resources: { cpu: 1, memory: '1g' },
      autoStopInterval: 15,         // minutes idle → stopped (files kept, no compute billed)
      autoDeleteInterval: 24 * 60,  // minutes after stop → removed
    }),
  }),
});
```

The agent gets Mastra's `execute_command`, `get_process_output` and `kill_process`
tools, backed by `POST /containers/{id}/exec` and the `GET /containers/{id}/stream`
WebSocket (Kubernetes `v4.channel.k8s.io` framing). Nothing in this package talks
to the node directly; the controller relays every byte to the node that runs the
container.

## Requirements

- Node 22+ (uses the global `WebSocket`; pass your own `WebSocket` implementation on older runtimes)
- `@mastra/core` ≥ 1.67
- A Nordkraft controller with the exec/stream endpoints (CLI/server 0.3.43 or later) and a
  WireGuard connection to it (`nordkraft connect`), or a `--dev` node on localhost

## Install

```bash
npm install @nordkraft/mastra @mastra/core
```

Point it at your controller with `NORDKRAFT_API_URL` (default `http://172.20.0.254:8001/api`,
the Nordkraft Cloud address) or the `apiUrl` option.

## How the sandbox maps onto a container

| Mastra | Nordkraft |
|---|---|
| `sandbox.start()` | find the container labelled `mastra-sandbox-id=<id>` and start it if stopped, else `POST /containers/deploy` |
| `executeCommand(cmd)` | `sh -c cmd` over the stream, stdin detached (`stdin=false`) |
| `processes.spawn(cmd)` | same stream, stdin attached; `sendStdin` / `closeStdin` / `kill` map to stdin frames and closing the socket |
| `writeFiles([...])` | base64 in an environment variable, 32 KiB per buffered exec, decoded with `base64 -d` in the container (no stdin involved, see below) |
| `networking.getPortUrl(p)` | `http://<container-ip>:<p>` on your WireGuard network (not public) |
| `stop()` / `destroy()` | `POST /containers/{id}/stop` / `DELETE /containers/{id}?force=true` |
| `autoStopInterval`, `autoDeleteInterval`, `maxLifetimeMinutes`, `ephemeral` | the container's `lifecycle` spec, enforced by the node even when your process is gone |
| `sandboxId` | the container name (`app-…`); pass it back as `sandboxId` to reattach from another process |

Options not listed above (`command`, `env`, `labels`, `workingDirectory`, `user`, `timeout`,
`ports`, `persistence`, `volumePath`, `targetGarage`, `hardwarePreference`, `architecture`,
`restart`, `startTimeout`) are documented on `NordkraftSandboxOptions`.

The default image is `alpine:3.21` with `tail -f /dev/null` as the main process, so the
micro-VM stays up between commands. Use any image that ships `/bin/sh`.

## Running the examples

```bash
npm install
NORDKRAFT_API_URL=http://172.20.0.254:8001/api npm run example:workspace   # no LLM needed
ANTHROPIC_API_KEY=… npm run example:agent                                  # a real agent loop
```

`example:workspace` deploys a small sandbox on your account, runs commands through the
exact tool an agent would call, and removes it again (`KEEP=1` to leave it running).

## Tests

```bash
npm test          # unit tests against a fake controller that runs commands in a host `sh`
npm run test:live # against a real controller; deploys and removes one sandbox
```

## Notes on stdin and Kata

A process that is spawned without stdin (`executeCommand`, or `stdinMode: 'ignore'`) reads
EOF from `/dev/null` straight away and the node never has to close stdin through containerd.
That matters on Kata Containers: closing stdin there (observed on 3.25 and 3.32) cancels the
shim's pending stdout read and whatever the process prints afterwards is lost. With stdin
attached (`processes.spawn` default) `closeStdin()` does go through containerd, so on a Kata
node output printed *after* EOF may be truncated; output printed before it is fine. Large
stdin is fragile there too: when the process drains stdin slower than the node pushes it, the
Kata agent short-writes the 64 KiB pipe and the rest is dropped. That is why `writeFiles`
never uses stdin. Prefer `executeCommand`, `writeFiles` plus a file redirect, or a heredoc
for commands that read stdin to EOF and then print. runc nodes are not affected.

## License

AGPL-3.0-only, like the rest of Nordkraft Garage Cloud.
