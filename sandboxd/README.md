# nordkraft-sandboxd

The daemon that is PID 1 of a Nordkraft sandbox container (`nordkraft deploy --sandbox`,
API `"sandbox": true`). The node bind-mounts this static binary read-only at
`/opt/nordkraft/sandboxd` and makes it the container's entrypoint; the image's command
(or the deploy's `command`) runs as its child, orphans are reaped, SIGTERM is forwarded.

It serves, on the container's own address (port 7777), authenticated with a per-container
bearer token the controller mints at deploy:

| Endpoint | Purpose |
|---|---|
| `GET /v1/health` | version, pid, main process pid, uptime |
| `POST /v1/exec` | buffered exec: `{cmd, cwd, env, user, stdin, timeout_s}` → `{exit_code, stdout, stderr, duration_ms, truncated}`; no `stdin` means `/dev/null` |
| `GET /v1/stream` | WebSocket, Kubernetes `v4.channel.k8s.io` framing (0 stdin, 1 stdout, 2 stderr, 3 status, 4 resize); `tty=1` gives a real pty; `stdin=false` detaches stdin |
| `PUT /v1/files?path=&mode=` | upload (raw body), parent dirs created, atomic rename |
| `GET /v1/files?path=` | download |

The controller relays user requests to these endpoints (`/containers/{id}/exec`, `/stream`,
`/files`), so clients never talk to the daemon directly. Why it exists: with Kata, the
runtime's own exec path drops output after stdin EOF and short-writes large stdin; being
the process's parent inside the micro-VM makes EOF a `close()`, a terminal an `openpty`,
and output something nothing in between can lose. Same model as Daytona's daemon and E2B's envd.

## Build

Static musl binaries for both node architectures, linked with the `rust-lld` that ships
with rustup (no musl-gcc or cross toolchain), see `.cargo/config.toml`:

```bash
cd sandboxd
cargo build --release --target x86_64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl
```

Install on a node as `/opt/nordkraft/sandboxd/sandboxd` (the agent's `SANDBOXD_PATH`).
Running sandboxes keep the copy they were started with; new deploys pick up the new one.

## Environment

| Variable | Meaning |
|---|---|
| `NORDKRAFT_SANDBOX_TOKEN` | bearer token (required; `NORDKRAFT_SANDBOX_INSECURE=1` to run without, dev only) |
| `NORDKRAFT_SANDBOX_PORT` | listen port, default 7777 |
| `NORDKRAFT_SANDBOX_BIND` | listen address, default 0.0.0.0 |
| `RUST_LOG` | log filter, default `info` |

## Try it by hand

```bash
nerdctl run -d --name sbx -v /opt/nordkraft/sandboxd/sandboxd:/opt/nordkraft/sandboxd:ro \
  --entrypoint /opt/nordkraft/sandboxd -e NORDKRAFT_SANDBOX_TOKEN=test alpine:3.21
IP=$(nerdctl inspect sbx --format '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}')
curl -H 'Authorization: Bearer test' http://$IP:7777/v1/health
curl -H 'Authorization: Bearer test' -H 'content-type: application/json' \
  http://$IP:7777/v1/exec -d '{"cmd":["sh","-c","cat; echo after"],"stdin":"hi\n"}'
```
