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
| `sandbox.start()` | find the container labelled `mastra-sandbox-id=<id>` and start it if stopped, else `POST /containers/deploy` with `sandbox: true` |
| `executeCommand(cmd)` | `sh -c cmd` over the stream to nordkraft-sandboxd, stdin detached (`stdin=false`) |
| `processes.spawn(cmd)` | same stream, stdin attached; `sendStdin` / `closeStdin` / `kill` map to stdin frames and closing the socket |
| `writeFiles([...])` | `PUT /containers/{id}/files` — streamed to the daemon, written atomically with the requested mode |
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

## How commands reach the sandbox

By default the provider deploys with `sandbox: true`, which makes **nordkraft-sandboxd**
the container's entrypoint: a small static daemon (PID 1) that runs your `command` as a
child and serves exec, process streams, PTYs and file transfer from inside the micro-VM on
the container's own address, authenticated with a per-container token. The controller
relays to it, so nothing a sandbox does goes through the container runtime's exec path.
That is the same model Daytona and E2B use, and it is what makes stdin, output after EOF
and large uploads reliable on Kata Containers.

`daemon: false` deploys a plain container instead and falls back to the runtime's exec
path (`tail -f /dev/null` keeps it alive, `writeFiles` ships base64 through environment
variables). On Kata that path can lose output printed after stdin EOF and truncate stdin
larger than one pipe buffer; `executeCommand` is safe either way. Use it only on nodes
without the daemon installed.

## License

AGPL-3.0-only, like the rest of Nordkraft Garage Cloud.
