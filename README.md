# Nordkraft I/O Garage Cloud

> ⚠️ **Still in active development.** MARK I ready: The platform runs in frendly user testing today and active development is ongoing. For a full stable production release wait for MARK II.

**Run containers on infrastructure you control — from anywhere, on any hardware.**

CLI-first. WireGuard private-by-default. Built on refurbished hardware in Denmark.

---

## What problem does this solve?

You have a side project — or a home lab — that deserves proper infrastructure without the overhead. Most container tooling assumes you're either all-in on Kubernetes or fine with `docker compose up` on a single machine.

Nordkraft.io's Garage Cloud sits in between: your containers get real IP addresses, WireGuard makes them securely accessible from anywhere, and the CLI keeps things simple. No YAML sprawl. No reverse proxy maze. No dashboard you need to babysit.

Every container runs in its own micro-VM via Kata Containers — a dedicated kernel, not just a namespace boundary. That gives you VM-level isolation without needing a separate hypervisor layer. One host, multiple tenants, real separation. The security of a full virtualization stack, with the developer experience of a modern deploy CLI.

It runs on hardware you already own. A Raspberry Pi and an old PC are enough. And if you'd rather skip the setup, [Nordkraft I/O cloud](https://cloud.nordkraft.io) runs the same stack on dedicated Danish hardware — so you can start hosted and self-host later, or the other way around.

**Who this is for:**
- Developers who care where their data lives
- Homelab builders who want real orchestration, not just Docker Compose
- Anyone tired of paying hyperscale prices for simple workloads
- People who want VM-level isolation without managing a hypervisor
- And if you'd rather not self-host: [Nordkraft.io Cloud](https://cloud.nordkraft.io) runs the same stack for you

---

## What you get

- **Deploy containers from your terminal** — the same workflow as any cloud provider
- **Access them from anywhere** via WireGuard VPN — no port forwarding, no dynamic DNS
- **VM-level isolation** with Kata Containers — workloads are genuinely isolated
- **Multi-machine support** — add nodes as you grow
- **IPv4 and IPv6 support** — works out of the box, globally routable when you need it
- **Optional HTTPS ingress** — real certificates, real domains, when you need them

---

## Quick start

> ⚠️ **Self-hosting install script is in progress.** The platform runs in production today — a streamlined self-hosting installer is coming in MARK II.

```bash
# Install the CLI (with invite token for Nordkraft I/O Garage Cloud.io Cloud)
curl -fsSL https://install.nordkraft.io | sh -s NKINVITE-your-token

# Deploy your first container
nordkraft deploy nginx:alpine --name my-app

# See it running — direct container IP via WireGuard, no proxy
nordkraft ps

# Optional: public HTTPS ingress
nordkraft ingress enable my-app --subdomain my-app
# → https://my-app.my.cloud
```

---

## How it works

```
Your laptop  →  WireGuard VPN  →  Controller  →  NATS  →  Agent nodes  →  Isolated containers
```

Your VPN connection *is* your authentication. No passwords, no tokens to manage at runtime. When you connect over WireGuard, the system resolves your IP to your public key to your account — cryptographically. All container operations flow through that encrypted tunnel.

Containers run with VM-level isolation via Kata Containers on top of containerd. Each deployment gets its own network, its own IP address, isolated from everything else.

### Architecture

The controller and agent nodes are separate hosts that communicate over NATS. In hybrid mode, the controller also runs workloads — useful for small setups or single-machine installs.

```
┌──────────────────────┐          ┌─────────────────────┐
│  HOST A: Controller  │          │  HOST B: Agent node  │
│                      │   NATS   │                      │
│  • WireGuard VPN     │◄────────►│  • Kata Containers   │
│  • Container API     │          │  • Your workloads    │
│  • PostgreSQL        │          │  • 172.21.x.x IPs    │
│  • NATS server       │          └──────────────────────┘
│                      │
│  In hybrid mode:     │          ┌──────────────────────┐
│  • Kata Containers   │   NATS   │  HOST C: Agent node  │
│  • Local workloads   │◄────────►│  • Kata Containers   │
│                      │          │  • More workloads    │
└──────────────────────┘          └──────────────────────┘
         ▲
         │ WireGuard (encrypted)
         │
    Your laptop / CI / anywhere
```

Add more agent nodes, add more capacity. The controller discovers nodes automatically via NATS.

---

## Minimum hardware

**To get started:**
- Any machine running Ubuntu — a Raspberry Pi 4 (2GB RAM) is enough
- A network connection

**To grow:**
- Add any Linux machine as a worker node
- Old laptops, mini PCs, refurbished office hardware — all fair game

---

## Current status

This is working software running in production at [cloud.nordkraft.io](https://cloud.nordkraft.io).

**Working today:**
- WireGuard authentication and VPN setup
- Container deployment and lifecycle management
- Network isolation per user and container
- Multi-node orchestration via NATS
- Kata Container VM isolation
- Persistent volumes
- HTTPS ingress with automatic certificates
- IPv4 and IPv6 support

---

## Technology

- **[WireGuard](https://wireguard.com)** — VPN and authentication layer
- **[Rust](https://rust-lang.org)** — API and CLI, memory safe by default
- **[Kata Containers](https://katacontainers.io)** — VM-level isolation
- **[NATS](https://nats.io)** — Lightweight messaging for multi-node coordination
- **[PostgreSQL](https://postgresql.org)** — State management on the controller

---

## Development setup

```bash
git clone https://github.com/nordkraft/nordkraft
cd nordkraft

# Start NATS
docker run -p 4222:4222 nats:latest

# Set environment
export DEV_MODE=true
export NATS_URL=nats://127.0.0.1:4222
export NORDKRAFT_MODE=hybrid
export NODE_ID=dev-node

# Run
cargo run -- serve
```

`DEV_MODE=true` bypasses WireGuard auth so you can develop without a VPN setup.

---

## Open source

The core — container API, CLI, and WireGuard auth — is licensed under [AGPL-3.0](https://www.gnu.org/licenses/agpl-3.0.html). You can use, modify, and self-host freely. If you offer it as a network service, your changes must be open sourced under the same license.

Infrastructure configuration and the hosted platform at [cloud.nordkraft.io](https://cloud.nordkraft.io) are not public.

**Found a bug?** Open an issue or PR.
**Have a question?** frederikkarlsson@me.com

---

## Security

Your WireGuard VPN connection is your identity. No passwords in config files, no API tokens to rotate. The cryptographic key on your machine is your credential, enforced at the kernel level.

Every container runs in a Kata micro-VM with its own kernel. Tenants are separated by nftables rules and isolated network namespaces. This isn't bolted on — it's how the system works.

**What's in place:**
- **Identity:** WireGuard public key authentication — no passwords, no tokens
- **Isolation:** Kata Containers — each container gets its own kernel and memory space
- **Network:** Per-tenant nftables rules + macvlan — tenants cannot see each other's traffic
- **Data:** Full-disk encryption at rest, per-node
- **API:** All queries parameterized (sqlx) — no injection surface
- **Access:** API only reachable through WireGuard tunnel — zero public attack surface

!! Use at own risk and always examine and take security necessary measures when self-hosting. !!

---

## Try Nordkraft.io Cloud

Don't want to manage your own hardware? [cloud.nordkraft.io](https://cloud.nordkraft.io) runs the same stack on refurbished hardware in Denmark, powered by renewable energy.

---

*Your compute. Your rules. Built in Denmark.*
