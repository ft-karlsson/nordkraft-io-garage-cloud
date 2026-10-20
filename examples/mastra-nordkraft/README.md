# Mastra ↔ Nordkraft example

A Mastra sandbox provider that runs an agent's commands inside a Nordkraft
container over the controller's REST API. WireGuard is the auth: run this
from a machine that is a peer (`nordkraft connect`).

```bash
cp .env.example .env            # fill in MISTRAL_API_KEY
npm install
set -a; . ./.env; set +a
npm run smoke                   # provider only, no model
npm run agent                   # Mistral + sandbox, default task
npm run agent -- "count the words in /etc/passwd"
```

`src/nordkraft-sandbox.ts` is the whole provider (~150 lines): start/stop/
destroy, `executeCommand` → `POST /containers/<name>/exec`, `writeFiles` via
exec+stdin, `getPortUrl` → the container's WireGuard address.
