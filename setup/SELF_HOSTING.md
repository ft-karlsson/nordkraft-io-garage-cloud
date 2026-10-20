# Self-hosting Nordkraft

Two binaries, three node modes:

- **`nordkraft`** — the CLI (client-cli). Lives on your laptop.
- **`nordkraft-node`** — the server (container-api). Lives on a machine and
  runs in one mode: `hybrid` (controller + agent, the single-box default),
  `controller` (API, scheduler, database, no workloads) or `agent`
  (workloads only, joins a controller over NATS).

One installer. Ubuntu 22.04 / 24.04, x86_64 or aarch64.

| | `--dev` | `--mode hybrid` (default) | `--mode agent` |
|---|---|---|---|
| What it is | Local API for development and integration tests | Controller + worker on one machine | Extra worker joining a controller |
| WireGuard | no — anyone who can reach the port is the dev user | yes, API is WireGuard-only | yes |
| NATS | no | yes | client → controller |
| Networking | bridge, ports published on the host | macvlan, containers get LAN IPs | macvlan |
| Kata isolation | no | `--kata` | `--kata` |
| Postgres | local, `nordkraft:nordkraft` | local, random password | none |

The server applies the database schema itself on first start (it is embedded
in the binary), so there are no SQL files to run.

## Dev node — try the API in five minutes

```bash
git clone https://github.com/ft-karlsson/nordkraft-io-garage-cloud
cd nordkraft-io-garage-cloud
sudo bash scripts/install-node.sh --dev            # downloads the latest release
# or, with a locally built binary:
sudo bash scripts/install-node.sh --dev --binary container-api/target/release/container-api
```

Then, from the same machine (or `--bind 0.0.0.0` for a LAN):

```bash
nordkraft config set-api http://127.0.0.1:8001
nordkraft deploy nginx:alpine -p 80
curl http://127.0.0.1/            # published port
nordkraft list
nordkraft logs <name>
nordkraft rm <name>
```

Useful in dev:

```bash
nordkraft status                      # shows "DEV MODE · bridge · no auth" so you know which node you are on
nordkraft -y deploy nginx:alpine -p 80   # no prompts (also implied when stdin is not a terminal, or with --json)
nordkraft rm --force <name>           # kill now instead of a graceful stop
sudo bash scripts/install-node.sh --dev --reset   # wipe the dev database and all containers, start over
```

Several containers can ask for the same port: the first gets it, the rest get
a free port from 30000 upward, and `deploy` prints the URL. `PUBLIC_HOST` in
`/etc/nordkraft/config.env` is the hostname used in those URLs (the installer
sets `<machine>.orb.local` inside OrbStack).

Or straight from your own backend:

```bash
curl -X POST http://127.0.0.1:8001/api/containers/deploy \
  -H 'content-type: application/json' \
  -d '{"image":"nginx:alpine","ports":[{"port":80,"protocol":"tcp"}],"cpu_limit":0.5,"memory_limit":"256m"}'

curl http://127.0.0.1:8001/api/containers
curl http://127.0.0.1:8001/api/containers/<name>/logs?lines=50
curl -X DELETE http://127.0.0.1:8001/api/containers/<name>
```

What `nordkraft-node --dev` actually does (see `container-api/src/config.rs`,
`apply_dev_preset`): `DEV_MODE=true NATS_ENABLED=false DISABLE_KATA=true
NETWORK_MODE=bridge BIND_ADDRESS=127.0.0.1`, plus an auto-seeded dev user,
garage and node. Every value is only a default — anything set in
`/etc/nordkraft/config.env` wins.

Dev mode has **no authentication**. Keep it on 127.0.0.1 or a private
network. It is for development, not for serving anyone else's containers.

### On a Mac (OrbStack)

The server needs Linux with containerd, so on macOS it runs in a VM.
OrbStack forwards the VM's ports to `localhost` and mounts your Mac home
inside the VM at the same path (`/Users/<you>`, not `~`). Kata is
unavailable in any macOS VM; dev mode disables it anyway.

```bash
brew install orbstack
orb create ubuntu:24.04 nordkraft

# build inside the VM and install the dev node, one command
orb -m nordkraft sudo bash /Users/<you>/deus/nordkraft-io-garage-cloud/scripts/install-node.sh \
  --dev --build-from /Users/<you>/deus/nordkraft-io-garage-cloud

# on the Mac: CLI with the `config` command, pointed at the VM
cd ~/deus/nordkraft-io-garage-cloud/client-cli && cargo build --release && cp target/release/nordkraft /usr/local/bin/
nordkraft config set-api http://127.0.0.1:8001
nordkraft deploy nginx:alpine -p 80
curl http://nordkraft.orb.local
```

Inside OrbStack, published container ports are iptables rules, not sockets,
so OrbStack does not forward them to the Mac's `localhost`. The installer
detects OrbStack, publishes on `0.0.0.0`, and the VM is reached by its name,
`<machine>.orb.local`. The API on `127.0.0.1:8001` is a real socket and is
forwarded as usual.

`--build-from` installs gcc and rustup in the VM, builds `container-api`
into `/var/tmp/nordkraft-build` (so it never collides with a macOS build in
the same checkout) and installs the result. Once a release is tagged,
`install-node.sh --dev` alone downloads it instead.

Day to day: `orb start nordkraft` brings the service up via systemd;
`orb -m nordkraft journalctl -u nordkraft-node -f` tails the logs.

### Running from source without the installer

```bash
createuser nordkraft -P   # password: nordkraft
createdb -O nordkraft nordkraft
cd container-api && cargo run -- --dev
```

## Hybrid — production, one machine

```bash
sudo bash scripts/install-node.sh [--kata]          # --mode hybrid is the default
```

Installs containerd, nerdctl, CNI plugins, Postgres, WireGuard, NATS and
writes `/etc/nordkraft/config.env` + `nordkraft-node.service`. The API binds to
the WireGuard address (`172.20.0.254:8001`) only. Create the first user and
invite as described in `SETUP.md`, then connect with the CLI.

## Agent — add a worker

```bash
sudo bash scripts/install-node.sh --mode agent --controller 172.20.0.254 [--kata]
```

The script prints the agent's WireGuard public key. Add it as a peer on the
controller, register the node in the `nodes` table, and the agent joins over
NATS.

Shell, exec and wait do not go over NATS: the controller opens a connection to
the agent's own API at `<nodes.internal_ip>:8001` (`/api/internal/…`) and
relays the stream. So the agent must listen on that address — the installer
writes `BIND_ADDRESS=0.0.0.0` — and the firewall must accept TCP 8001 from the
controller's address. The agent accepts `/api/internal/` only from
`CONTROLLER_INTERNAL_IP`; everything else gets 403. Older hand-written agent
configs with `BIND_ADDRESS=127.0.0.1` (or another port) need
`BIND_ADDRESS=<LAN or WireGuard IP>` and `BIND_PORT=8001`, then a restart; a
`502 agent did not answer` from the CLI is the symptom.

## Options

```
--mode hybrid|controller|agent   default hybrid
--dev                   dev preset (implies hybrid)
--controller <ip>       agent only
--kata                  install Kata Containers (VM isolation)
--binary <path>         use a local build instead of a GitHub release
--build-from <repo>     build container-api from a checkout (installs rustup + gcc)
--version <tag>         pin a release
--bind <addr>           dev bind address (0.0.0.0 for LAN access)
--force                 rewrite config.env and WireGuard config
--no-systemd            write configs only (containers, CI)
--reset                 (dev) wipe the dev database and all managed containers
```

The script is idempotent; re-running keeps existing keys, database password
and `config.env` unless `--force`.

## Sandboxes: nordkraft-sandboxd on the node

`nordkraft deploy --sandbox` (API `"sandbox": true`) needs the sandbox daemon on every
node that runs containers. It is a single static binary the agent bind-mounts into each
sandbox as its entrypoint. `install-node.sh` installs it next to the node binary:

- from a release: the `sandboxd-linux-<arch>` asset (with checksum) of the same version;
- `--build-from <repo>`: built from the checkout as well (static musl, needs no extra toolchain);
- `--sandboxd <path>`: a binary you built yourself, for use with `--binary`.

Manual install is just:

```bash
cd sandboxd
cargo build --release --target x86_64-unknown-linux-musl     # or aarch64-unknown-linux-musl
sudo install -D -m 0755 target/x86_64-unknown-linux-musl/release/sandboxd /opt/nordkraft/sandboxd/sandboxd
```

The agent looks for it at `SANDBOXD_PATH` (default `/opt/nordkraft/sandboxd/sandboxd`) and
refuses a `--sandbox` deploy with a clear error when it is missing. Ordinary containers never
use it. Details in [`sandboxd/README.md`](../sandboxd/README.md).

## CLI endpoint

```
nordkraft config set-api http://host:8001    # persists to ~/.nordkraft/api_url
nordkraft config show
nordkraft config reset-api                   # back to WireGuard / connection.json
```

Precedence: `API_BASE_URL` env → `~/.nordkraft/api_url` → `connection.json`
→ `http://172.20.0.254:8001/api`.
