# Mastra ↔ Nordkraft example

A [Mastra](https://mastra.ai) sandbox provider that runs an agent's commands
inside a Nordkraft container. The model (here Mistral, in the cloud) decides
what to run; every command executes in a container on Nordkraft hardware and
only its output goes back to the model.

```
 your Mac                                   Mistral (cloud)
┌──────────────────────────────┐            ┌──────────────────────┐
│ Mastra agent (TypeScript)    │── HTTPS ──►│ inference only:      │
│  tool: execute_command       │            │ prompt + tool output │
│  NordkraftSandbox provider   │            │ never a shell        │
└──────────────┬───────────────┘            └──────────────────────┘
               │ REST  POST /containers/<id>/exec
               │ WireGuard tunnel — your key is your identity,
               │ no API key, no password, kernel-level crypto
               ▼
┌──────────────────────────────┐  Nordkraft controller (Raspberry Pi)
│ container-api :8001          │  WG source IP → user → tenant
│  owner check on every call   │  timeouts, lifecycle, placement
└──────────────┬───────────────┘
               │ WebSocket /api/internal/…  (private LAN/WG)
               │ trusted only from the controller's IP
               ▼
┌──────────────────────────────┐  Nordkraft node (OptiPlex)
│ container-api agent          │  containerd gRPC: Exec/Start/Wait
│  nftables per-tenant rules   │  macvlan: container gets a LAN IP
│  reaper: auto_stop/delete    │
└──────────────┬───────────────┘
               │ containerd + Kata shim
               ▼
┌──────────────────────────────┐  Kata Containers
│ microVM, own kernel          │  python:3.12-alpine
│   sh -c "<agent's command>"  │  stdout/stderr/exit → back up the chain
└──────────────────────────────┘
```

Every hop has its own boundary: TLS to the model (which only ever sees text),
WireGuard between you and the cloud (the peer key is the credential), the
controller mapping that key to a tenant and checking ownership, the node
trusting only the controller for internal calls and fencing tenants with
nftables, and finally a VM boundary around the container itself — the agent's
command never shares a kernel with the host or with other tenants. Idle
sandboxes stop and delete themselves via lifecycle labels.

`src/nordkraft-sandbox.ts` is the whole provider (~170 lines): find-or-create,
`executeCommand` → `POST /containers/<name>/exec`, `writeFiles` via
exec+stdin, lifecycle labels so an idle sandbox stops and cleans itself up.

Two ways to try it: against Nordkraft Cloud (A), or against a dev node running
locally on your Mac (B). The example is the same; only `NORDKRAFT_API` changes.

Prerequisites for both: Node 20+, and a Mistral API key from
<https://console.mistral.ai> (a plain API key, not a workspace key).

---

## A. Against Nordkraft Cloud

You need to be a WireGuard peer — that is the authentication, there is no API
key. If `nordkraft status` answers, you are.

```bash
nordkraft connect            # if not already up
nordkraft status             # must answer; shows your garage and nodes

cd examples/mastra-nordkraft
cp .env.example .env         # set MISTRAL_API_KEY; NORDKRAFT_API already points at the cloud
npm install
set -a; . ./.env; set +a

npm run smoke                # provider only, no model: create, exec, write, destroy
npm run agent                # Mistral + sandbox, default task (primes.py)
npm run agent -- "count the words in /etc/passwd and tell me the number"
```

What you should see from `npm run agent`:

```
task: Write a python script /work/primes.py that prints the first 10 prime numbers, run it, and report the output.
→ tool mastra_workspace_execute_command {"command":"mkdir -p /work"}
   ← "(no output)"
→ tool mastra_workspace_execute_command {"command":"cat > /work/primes.py << 'EOF' ..."}
   ← "(no output)"
→ tool mastra_workspace_execute_command {"command":"cd /work && python3 primes.py"}
   ← "2\n3\n5\n7\n11\n13\n17\n19\n23\n29\n"

answer:
2 3 5 7 11 13 17 19 23 29
```

The first run deploys the sandbox (`python:3.12-alpine`, a few seconds for the
image pull); later runs find it by its label and reuse it. Each command is a
round trip Mac → WireGuard → controller → node → Kata VM, typically under
100 ms.

```bash
nordkraft list --label mastra.sandbox=sandbox-1    # see the sandbox
nordkraft shell <name>                             # poke around in it
nordkraft rm <name>                                # or let auto_delete (2h) do it
```

---

## B. Against a local dev node on your Mac

The server needs Linux with containerd, so on macOS it runs in an OrbStack
VM. Dev mode has no WireGuard and no auth, uses bridge networking and plain
runc instead of Kata — the API is the same.
Full details: [`setup/SELF_HOSTING.md`](../../setup/SELF_HOSTING.md).

```bash
brew install orbstack
orb create ubuntu:24.04 nordkraft

# build container-api inside the VM and install it as a dev node (one command, a few minutes)
REPO=$(pwd)   # run from the repo root
orb -m nordkraft sudo bash $REPO/scripts/install-node.sh --dev --build-from $REPO

# the CLI on the Mac, pointed at the VM (OrbStack forwards :8001 to localhost)
cargo build --release -p nordkraft && cp target/release/nordkraft /usr/local/bin/
nordkraft config set-api http://127.0.0.1:8001
nordkraft status             # shows "DEV MODE · bridge · no auth"
```

Then the example, with the API pointed at the VM instead of the cloud:

```bash
cd examples/mastra-nordkraft
cp .env.example .env         # set MISTRAL_API_KEY
npm install
set -a; . ./.env; set +a
export NORDKRAFT_API=http://127.0.0.1:8001/api

npm run smoke
npm run agent
```

Same output as in A, just faster (no WireGuard hop) and without VM isolation.

Useful while developing:

```bash
bash scripts/smoke-dev.sh                           # the full node smoke suite (exec, PTY, wait, lifecycle, …)
orb -m nordkraft sudo journalctl -u nordkraft-node -f   # node log
orb -m nordkraft sudo bash $REPO/scripts/install-node.sh --dev --reset   # wipe and start over
```

After changing `container-api`, re-run the `install-node.sh --dev --build-from`
line; it rebuilds and restarts the node.

---

## Environment

| variable | default | |
|---|---|---|
| `NORDKRAFT_API` | `http://172.20.0.254:8001/api` | cloud over WireGuard; `http://127.0.0.1:8001/api` for the dev node |
| `NORDKRAFT_CONTAINER` | `sandbox-1` | logical sandbox id (label `mastra.sandbox=<id>`) |
| `MISTRAL_API_KEY` | — | required for `npm run agent` |
| `MODEL` | `mistral/mistral-medium-latest` | any model Mastra's router knows |
