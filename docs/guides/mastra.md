# Mastra: AI-agenter i en Nordkraft-sandbox

[Mastra](https://mastra.ai) er et open source TypeScript-framework til AI-agenter. En
agent får et *workspace* med en *sandbox*, hvor den kan køre kommandoer, installere
pakker og skrive filer. Mastra leverer selv providers til Daytona, E2B, Docker og
andre; `@nordkraft/mastra` er den samme slags provider for Nordkraft.

Forskellen: sandboxen er en Kata micro-VM på hardware du selv bestemmer over, nået
gennem din WireGuard-forbindelse. Ingen API-nøgler, ingen tredjepart der ser
agentens kommandoer. Alt er open source.

## Installation

```bash
npm install @nordkraft/mastra @mastra/core
```

Pakken ligger i repoet under `integrations/mastra`. Den kræver Node 22 eller nyere
og en controller med `exec`/`stream`-endpoints (version 0.3.43+). Sæt
`NORDKRAFT_API_URL` hvis du ikke bruger Nordkraft Cloud-adressen
(`http://172.20.0.254:8001/api`); mod en lokal `--dev`-node er det
`http://127.0.0.1:8001/api`.

## En agent med sandbox

```ts
import { Agent } from '@mastra/core/agent';
import { Workspace } from '@mastra/core/workspace';
import { NordkraftSandbox } from '@nordkraft/mastra';

export const coder = new Agent({
  id: 'coder',
  name: 'Coder',
  instructions: 'Brug sandboxen til at løse opgaven. Kør koden og rapportér resultatet.',
  model: 'anthropic/claude-opus-5-5',
  workspace: new Workspace({
    sandbox: new NordkraftSandbox({
      id: 'coder',                  // findes igen ved næste start
      image: 'python:3.12-alpine',  // alt med /bin/sh virker
      resources: { cpu: 1, memory: '1g' },
      autoStopInterval: 15,         // minutter uden aktivitet → stop (filer bevares)
      autoDeleteInterval: 24 * 60,  // minutter efter stop → slettes
    }),
  }),
});

await coder.generate('Skriv et script der printer de første 10 primtal, og kør det.');
```

Agenten får Mastras værktøjer `execute_command`, `get_process_output` og
`kill_process`. Bag dem ligger `POST /containers/{id}/exec` og WebSocket-strømmen
på `/containers/{id}/stream` — det samme som `nordkraft exec` og `nordkraft shell`
bruger.

## Hvad der sker på platformen

| Mastra | Nordkraft |
|---|---|
| `start()` | finder containeren med label `mastra-sandbox-id=<id>`, starter den hvis den er stoppet, ellers `deploy` |
| `executeCommand()` | `sh -c …` over strømmen, uden stdin |
| `processes.spawn()` | samme strøm med stdin; `kill()` lukker strømmen, og noden dræber processen |
| `writeFiles()` | `base64 -d > fil` via den bufferede exec |
| `stop()` / `destroy()` | `stop` / `rm --force` |
| `autoStopInterval`, `autoDeleteInterval`, `maxLifetimeMinutes` | containerens [livscyklus](sandboxes.md#livscyklus), som noden selv håndhæver |

Sandboxen er en almindelig container: `nordkraft list --label mastra-sandbox-id=coder`
viser den, `nordkraft shell` kommer ind i den, og den tæller på din plan som enhver
anden container. Standardimage er `alpine:3.21` med `tail -f /dev/null` som
hovedproces, så VM'en lever mellem kommandoerne.

## Én sandbox pr. bruger eller tråd

Mastra kan oprette sandboxen pr. request. `clone()` giver en ny sandbox med samme
opsætning:

```ts
const template = new NordkraftSandbox({ image: 'node:22-alpine', autoStopInterval: 10 });

const workspace = new Workspace({
  sandbox: async ({ requestContext }) => {
    const userId = requestContext.get('user-id') as string;
    const sandbox = template.clone({ id: `user-${userId}` });
    await sandbox.start();
    return sandbox;
  },
  sandboxCacheKey: ({ requestContext }) => requestContext.get('user-id') as string,
});
```

## Prøv det uden LLM

`integrations/mastra/examples/workspace.ts` kører hele livscyklussen mod din
controller — deploy, kommandoer, baggrundsproces, filer, timeout, oprydning — med
de præcise værktøjer en agent ville kalde:

```bash
cd integrations/mastra && npm install
NORDKRAFT_API_URL=http://172.20.0.254:8001/api npm run example:workspace
```

## Begrænsninger

- Output over 1 MiB pr. kommando klippes (det gælder også `nordkraft exec`).
- Port-URL'er (`networking.getPortUrl`) peger på container-IP'en på dit WireGuard-net,
  ikke en offentlig adresse. Brug [ingress](domains.md) hvis noget skal være offentligt.
- På Kata-noder kan output skrevet *efter* at stdin lukkes gå tabt for processer startet
  *med* stdin (`processes.spawn` + `closeStdin()`). `executeCommand` er ikke berørt.
  Se [kendte problemer](../troubleshooting/known-issues.md).
