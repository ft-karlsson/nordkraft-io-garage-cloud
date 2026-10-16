# Self-hosting Nordkraft

One installer, three shapes. Ubuntu 22.04 / 24.04, x86_64 or aarch64.

| | `--dev` | `--role server` | `--role agent` |
|---|---|---|---|
| What it is | Local API for development and integration tests | Controller + worker on one machine (hybrid) | Extra worker joining a controller |
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

Or straight from your own backend:

```bash
curl -X POST http://127.0.0.1:8001/api/containers/deploy \
  -H 'content-type: application/json' \
  -d '{"image":"nginx:alpine","ports":[{"port":80,"protocol":"tcp"}],"cpu_limit":0.5,"memory_limit":"256m"}'

curl http://127.0.0.1:8001/api/containers
curl http://127.0.0.1:8001/api/containers/<name>/logs?lines=50
curl -X DELETE http://127.0.0.1:8001/api/containers/<name>
```

What `serve --dev` actually does (see `container-api/src/config.rs`,
`apply_dev_preset`): `DEV_MODE=true NATS_ENABLED=false DISABLE_KATA=true
NETWORK_MODE=bridge BIND_ADDRESS=127.0.0.1`, plus an auto-seeded dev user,
garage and node. Every value is only a default — anything set in
`/etc/nordkraft/config.env` wins.

Dev mode has **no authentication**. Keep it on 127.0.0.1 or a private
network. It is for development, not for serving anyone else's containers.

### On a Mac (OrbStack)

The server needs Linux with containerd, so on macOS it runs in a VM.
OrbStack forwards the VM's ports to `localhost` and mounts your home
directory inside the VM at the same path, so nothing else needs configuring.
(Kata is unavailable in any macOS VM — dev mode disables it anyway.)

```bash
brew install orbstack
orb create ubuntu:24.04 nordkraft

# install the dev node inside the VM — your checkout is visible at the same path
orb -m nordkraft sudo bash ~/deus/nordkraft-io-garage-cloud/scripts/install-node.sh --dev

# on the Mac: CLI with the `config` command, pointed at the VM
cd client-cli && cargo build --release && cp target/release/nordkraft /usr/local/bin/
nordkraft config set-api http://127.0.0.1:8001
nordkraft deploy nginx:alpine -p 80
curl localhost
```

Before a release exists (or to run your own build), build inside the VM
and hand the binary to the installer:

```bash
orb -m nordkraft bash -c 'curl -fsSL https://sh.rustup.rs | sh -s -- -y && \
  cd ~/deus/nordkraft-io-garage-cloud/container-api && \
  ~/.cargo/bin/cargo build --release'
orb -m nordkraft sudo bash ~/deus/nordkraft-io-garage-cloud/scripts/install-node.sh --dev \
  --binary ~/deus/nordkraft-io-garage-cloud/container-api/target/release/container-api
```

Day to day: `orb start nordkraft` brings the service up via systemd;
`orb -m nordkraft journalctl -u nordkraft -f` tails the logs.

### Running from source without the installer

```bash
createuser nordkraft -P   # password: nordkraft
createdb -O nordkraft nordkraft
cd container-api && cargo run -- serve --dev
```

## Server — production, one machine

```bash
sudo bash scripts/install-node.sh --role server [--kata]
```

Installs containerd, nerdctl, CNI plugins, Postgres, WireGuard, NATS and
writes `/etc/nordkraft/config.env` + `nordkraft.service`. The API binds to
the WireGuard address (`172.20.0.254:8001`) only. Create the first user and
invite as described in `SETUP.md`, then connect with the CLI.

## Agent — add a worker

```bash
sudo bash scripts/install-node.sh --role agent --controller 172.20.0.254 [--kata]
```

The script prints the agent's WireGuard public key. Add it as a peer on the
controller, register the node in the `nodes` table, and the agent joins over
NATS.

## Options

```
--role server|agent     default server
--dev                   dev preset (implies server)
--controller <ip>       agent only
--kata                  install Kata Containers (VM isolation)
--binary <path>         use a local build instead of a GitHub release
--version <tag>         pin a release
--bind <addr>           dev bind address (0.0.0.0 for LAN access)
--force                 rewrite config.env and WireGuard config
--no-systemd            write configs only (containers, CI)
```

The script is idempotent; re-running keeps existing keys, database password
and `config.env` unless `--force`.

## CLI endpoint

```
nordkraft config set-api http://host:8001    # persists to ~/.nordkraft/api_url
nordkraft config show
nordkraft config reset-api                   # back to WireGuard / connection.json
```

Precedence: `API_BASE_URL` env → `~/.nordkraft/api_url` → `connection.json`
→ `http://172.20.0.254:8001/api`.
