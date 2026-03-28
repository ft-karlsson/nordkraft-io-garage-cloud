# WireGuard Setup

WireGuard is the authentication and transport layer for NordKraft Garage Cloud. Your VPN connection *is* your identity — no passwords, no tokens at runtime. The server resolves your IP → public key → account at the kernel level.

This guide covers setting up the server-side WireGuard interface on your node.

---

## Prerequisites

```bash
sudo apt install -y wireguard wireguard-tools
```

---

## Server configuration

### 1. Generate server keypair

```bash
# Generate keys (keep the private key secret)
wg genkey | sudo tee /etc/wireguard/server_private.key | wg pubkey | sudo tee /etc/wireguard/server_public.key
sudo chmod 600 /etc/wireguard/server_private.key

cat /etc/wireguard/server_public.key  # You'll need this for client configs
```

### 2. Create the interface config

```bash
sudo tee /etc/wireguard/wg0.conf > /dev/null <<EOF
[Interface]
Address = 172.20.0.1/24
ListenPort = 51820
PrivateKey = $(sudo cat /etc/wireguard/server_private.key)

# IP forwarding — required for container routing
PostUp = sysctl -w net.ipv4.ip_forward=1
PostUp = nft add table inet filter 2>/dev/null || true

# Peers are added dynamically by container-api via `wg set`
# Do NOT add peers manually here — they will be managed by the reconciler
EOF

sudo chmod 600 /etc/wireguard/wg0.conf
```

> **Important:** Peers are added dynamically at runtime via `wg set`. Do not define `[Peer]` sections in the static config — they will be dropped when `wg-quick` restarts and replaced by the WgReconciler.

### 3. Enable IP forwarding persistently

```bash
echo "net.ipv4.ip_forward=1" | sudo tee -a /etc/sysctl.conf
sudo sysctl -p
```

### 4. Bring up the interface

```bash
sudo wg-quick up wg0
sudo systemctl enable wg-quick@wg0
```

Verify:

```bash
sudo wg show wg0
# Should show interface with your public key and ListenPort 51820
```

---

## Port forwarding

You need to expose UDP port `51820` from your router/firewall to your node.

| Protocol | Port | Direction | Purpose |
|----------|------|-----------|---------|
| UDP | 51820 | Inbound | WireGuard client connections |

No other ports need to be exposed to the internet for basic operation. Container access is tunnelled through WireGuard.

If you run the HTTPS ingress (pfSense/HAProxy or Caddy), you will additionally need TCP 80 and 443 open — see [INGRESS_PFSENSE.md](INGRESS_PFSENSE.md).

---

## How authentication works

When a client connects, the kernel assigns them a deterministic IP based on their keypair (`172.20.0.<slot>/32`). `container-api` reads `wg show` output, builds an in-memory peer cache (IP → public key), and resolves every API request against it. No headers, no tokens — the TCP socket IP is the identity.

```
Client keypair → WireGuard handshake → kernel assigns 172.20.0.N/32
                                                ↓
API request arrives on 172.20.0.N → peer cache lookup → public key → DB → User
```

This means:
- A compromised token cannot impersonate a user without their WireGuard private key
- Revoking a user = removing their peer from WireGuard
- No session management, no token rotation

---

## Security considerations

**Keep `server_private.key` private.** If it leaks, an attacker can impersonate your server and intercept client traffic. Store it with `chmod 600` and do not commit it to version control.

**`AllowedIPs` is a routing table, not a firewall.** WireGuard enforces that packets from peer X can only arrive from peer X's assigned IP, but it does not prevent tenants from reaching each other's containers. That isolation is handled by nftables. Do not remove the nftables rules that `container-api` installs.

**Do not expose the API port (8001) to the internet.** It should only be reachable over the WireGuard interface (`172.20.0.0/24`). Bind it to the WireGuard address:

```bash
export BIND_ADDRESS=172.20.0.1
export BIND_PORT=8001
```

**WgReconciler runs every 5 minutes.** It compares active WireGuard peers against the database and re-adds any that are missing. This handles the known failure mode where `unattended-upgrades` triggers a `systemd-networkd` restart and drops dynamically added peers. If you disable the reconciler, peers will disappear after network restarts.

**Do not add a `DNS =` line to client configs on Ubuntu 24.04+** with `systemd-resolved`. It breaks general internet routing. The CLI removes this line automatically — if you write configs manually, leave DNS out.

---

## Troubleshooting

### Peer connects but API returns 401

The peer cache refresh is asynchronous. Wait a few seconds after connection and retry. If it persists, check:

```bash
sudo wg show wg0 peers   # Is the peer listed?
curl http://172.20.0.1:8001/api/status  # Is the API reachable over WireGuard?
```

### Peers disappear after reboot

Make sure you're **not** defining peers in `/etc/wireguard/wg0.conf`. If `wg-quick up wg0` runs on boot with a static config, it overwrites the dynamically-added peers. The WgReconciler will re-add them within 5 minutes, but you can also run `nordkraft auth login` to force re-registration.

### `wg-quick up wg0` fails

```bash
sudo journalctl -u wg-quick@wg0 -n 50
# Common causes: PrivateKey path wrong, port already in use, kernel module not loaded
sudo modprobe wireguard
```
