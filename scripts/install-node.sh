#!/bin/bash
set -euo pipefail

# =============================================================================
# Nordkraft node installer — Ubuntu 22.04 / 24.04, x86_64 or aarch64
#
# A Nordkraft node is one binary (nordkraft-node) run in one of three modes:
#   hybrid      controller + agent on the same machine (default, the single-box setup)
#   controller  API, scheduler, database — no workloads
#   agent       workloads only, joins a controller over NATS
#
#   sudo bash install-node.sh                         hybrid node
#   sudo bash install-node.sh --mode agent --controller 10.0.0.200
#   sudo bash install-node.sh --dev                   local dev: hybrid, no WireGuard, no NATS,
#                                                     bridge networking, API on 127.0.0.1:8001
#
# Options
#   --mode hybrid|controller|agent   (default: hybrid)
#   --dev                   dev preset (implies --mode hybrid, skips WireGuard/NATS/Kata)
#   --controller <ip>       (agent only) controller's internal IP
#   --kata                  install Kata Containers for VM isolation (not dev)
#   --binary <path>         use a locally built binary instead of downloading a release
#   --build-from <repo>     build container-api from a checkout on this machine (installs rustup + gcc)
#   --version <tag>         release tag to download (default: latest)
#   --bind <addr>           API bind address for --dev (default 127.0.0.1; use 0.0.0.0 for LAN)
#   --no-systemd            write configs but do not manage services (containers, CI)
#   --reset                 (dev) wipe the dev database and all managed containers, then restart
#
# Idempotent: safe to re-run. Existing config.env is kept unless --force.
# =============================================================================

MODE="hybrid"
DEV=false
KATA=false
FORCE=false
CONTROLLER_IP=""
LOCAL_BINARY=""
BUILD_FROM=""
NORDKRAFT_VERSION="${NORDKRAFT_VERSION:-latest}"
DEV_BIND=""
USE_SYSTEMD=true
RESET=false

while [[ $# -gt 0 ]]; do
  case "$1" in
    --mode)        MODE="$2"; shift 2 ;;
    --role)        MODE="$2"; [[ "$MODE" == "server" ]] && MODE="hybrid"; shift 2 ;;  # legacy
    --dev)         DEV=true; shift ;;
    --kata)        KATA=true; shift ;;
    --force)       FORCE=true; shift ;;
    --controller)  CONTROLLER_IP="$2"; shift 2 ;;
    --binary)      LOCAL_BINARY="$2"; shift 2 ;;
    --build-from)  BUILD_FROM="$2"; shift 2 ;;
    --version)     NORDKRAFT_VERSION="$2"; shift 2 ;;
    --bind)        DEV_BIND="$2"; shift 2 ;;
    --no-systemd)  USE_SYSTEMD=false; shift ;;
    --reset)       RESET=true; shift ;;
    -h|--help)     sed -n '4,25p' "$0"; exit 0 ;;
    *) echo "Unknown option: $1"; exit 1 ;;
  esac
done

case "$MODE" in hybrid|controller|agent) ;; *) echo "--mode must be hybrid, controller or agent"; exit 1 ;; esac
if $DEV; then MODE="hybrid"; fi
if [[ "$MODE" == "agent" && -z "$CONTROLLER_IP" ]]; then
  echo "--mode agent requires --controller <ip>"; exit 1
fi
# "server" below = anything that runs the controller (hybrid or controller)
IS_SERVER=$([[ "$MODE" != "agent" ]] && echo true || echo false)

NORDKRAFT_DIR="/etc/nordkraft"
NORDKRAFT_DATA="/var/lib/nordkraft"
NORDKRAFT_LOG="/var/log/nordkraft"
DB_NAME="nordkraft"
DB_USER="nordkraft"
WG_INTERFACE="wg0"
WG_PORT="51820"
API_PORT="8001"
VPN_SERVER_IP="172.20.0.254"
GITHUB_REPO="ft-karlsson/nordkraft-io-garage-cloud"
NERDCTL_VERSION="2.0.3"
CNI_VERSION="1.6.2"
KATA_VERSION="3.25.0"

GREEN='\033[0;32m'; YELLOW='\033[1;33m'; RED='\033[0;31m'; BLUE='\033[0;34m'; NC='\033[0m'
log()     { echo -e "${GREEN}[nordkraft]${NC} $1"; }
warn()    { echo -e "${YELLOW}[warning]${NC} $1"; }
error()   { echo -e "${RED}[error]${NC} $1"; exit 1; }
section() { echo -e "\n${BLUE}━━━ $1 ━━━${NC}"; }

# =============================================================================
section "Preflight"
# =============================================================================
[[ $EUID -eq 0 ]] || error "Run as root: sudo bash install-node.sh ..."
grep -qi ubuntu /etc/os-release || error "Nordkraft nodes run on Ubuntu 22.04 / 24.04"
if $USE_SYSTEMD && ! systemctl is-system-running --quiet 2>/dev/null \
   && [[ "$(systemctl is-system-running 2>/dev/null)" != "degraded" ]]; then
  warn "systemd is not running here — continuing with --no-systemd"
  USE_SYSTEMD=false
fi
# svc <enable-now|restart|daemon-reload> <unit>
svc() {
  if $USE_SYSTEMD; then
    case "$1" in
      enable-now)    systemctl enable --now "$2" ;;
      restart)       systemctl restart "$2" ;;
      daemon-reload) systemctl daemon-reload ;;
    esac
  fi
}

ARCH=$(uname -m)
case "$ARCH" in
  x86_64)  BINARY_ARCH="x86_64";  GO_ARCH="amd64" ;;
  aarch64) BINARY_ARCH="aarch64"; GO_ARCH="arm64" ;;
  *) error "Unsupported architecture: $ARCH" ;;
esac
NODE_ID="${NODE_ID:-$(hostname)}"
if $DEV; then NODE_ID="dev-node"; fi

# Dev bind address: 127.0.0.1 on a real machine; 0.0.0.0 inside a VM such as
# OrbStack, where published ports (iptables DNAT, not sockets) are only
# reachable from the host by the VM's own address.
IN_ORBSTACK=false
if grep -qi orbstack /proc/version 2>/dev/null || [[ -d /opt/orbstack-guest ]]; then IN_ORBSTACK=true; fi
if [[ -z "$DEV_BIND" ]]; then
  if $IN_ORBSTACK; then DEV_BIND="0.0.0.0"; else DEV_BIND="127.0.0.1"; fi
fi
log "Mode: ${MODE}$($DEV && echo ' (dev)')   Arch: ${ARCH}   Node: ${NODE_ID}$($IN_ORBSTACK && echo '   (OrbStack VM)')"

if $RESET; then
  $DEV || error "--reset only works with --dev"
  [[ -f "${NORDKRAFT_DIR}/config.env" && -x /usr/local/bin/nordkraft-node ]] || error "--reset: no dev node installed here yet"
  section "Dev reset"
  if $USE_SYSTEMD; then systemctl stop nordkraft-node || true; fi
  ( set -a; . "${NORDKRAFT_DIR}/config.env"; set +a; /usr/local/bin/nordkraft-node --dev --reset )
  if $USE_SYSTEMD; then systemctl start nordkraft-node; sleep 2; fi
  log "Dev node reset — empty database, no containers"
  exit 0
fi

# =============================================================================
section "Phase 1: Packages"
# =============================================================================
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
PKGS=(curl jq uuid-runtime ca-certificates containerd iptables file zstd)
if $IS_SERVER; then PKGS+=(postgresql postgresql-client); fi
if ! $DEV; then PKGS+=(wireguard wireguard-tools nftables); fi
apt-get install -y -qq "${PKGS[@]}"
log "Packages installed"

# --- nerdctl ----------------------------------------------------------------
if ! command -v nerdctl >/dev/null; then
  log "Installing nerdctl ${NERDCTL_VERSION}..."
  curl -fsSL "https://github.com/containerd/nerdctl/releases/download/v${NERDCTL_VERSION}/nerdctl-${NERDCTL_VERSION}-linux-${GO_ARCH}.tar.gz" \
    | tar -xz -C /usr/local/bin/ nerdctl
fi
log "nerdctl $(nerdctl --version | awk '{print $3}')"

# --- CNI plugins (bridge, macvlan, host-local, portmap, firewall) ------------
if [[ ! -x /opt/cni/bin/bridge ]]; then
  log "Installing CNI plugins ${CNI_VERSION}..."
  mkdir -p /opt/cni/bin
  curl -fsSL "https://github.com/containernetworking/plugins/releases/download/v${CNI_VERSION}/cni-plugins-linux-${GO_ARCH}-v${CNI_VERSION}.tgz" \
    | tar -xz -C /opt/cni/bin/
fi
log "CNI plugins ready"

# --- containerd config -------------------------------------------------------
mkdir -p /etc/containerd
if [[ ! -f /etc/containerd/config.toml ]]; then
  containerd config default > /etc/containerd/config.toml
fi
svc enable-now containerd
log "containerd configured"

# --- Kata (optional, production isolation) -----------------------------------
if $KATA && ! $DEV; then
  if [[ ! -x /opt/kata/bin/kata-runtime ]]; then
    log "Installing Kata Containers ${KATA_VERSION}..."
    apt-get install -y -qq zstd
    curl -fsSL -o /tmp/kata-static.tar.zst \
      "https://github.com/kata-containers/kata-containers/releases/download/${KATA_VERSION}/kata-static-${KATA_VERSION}-${GO_ARCH}.tar.zst"
    tar -I zstd -xf /tmp/kata-static.tar.zst -C /
    rm -f /tmp/kata-static.tar.zst
    ln -sf /opt/kata/bin/kata-runtime /usr/local/bin/
    ln -sf /opt/kata/bin/containerd-shim-kata-v2 /usr/local/bin/
    printf 'vhost\nvhost_net\nvhost_vsock\n' > /etc/modules-load.d/kata.conf
    modprobe vhost vhost_net vhost_vsock || warn "vhost modules not loaded (no KVM?)"
  fi
  if ! grep -q 'runtimes.kata' /etc/containerd/config.toml; then
    cat >> /etc/containerd/config.toml <<'EOF'

[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.kata]
  runtime_type = "io.containerd.kata.v2"
  [plugins."io.containerd.grpc.v1.cri".containerd.runtimes.kata.options]
    ConfigPath = "/opt/kata/share/defaults/kata-containers/configuration.toml"
EOF
    svc restart containerd
  fi
  log "Kata Containers ready"
fi

# =============================================================================
section "Phase 2: Nordkraft binary"
# =============================================================================
if [[ -n "$BUILD_FROM" ]]; then
  [[ -f "$BUILD_FROM/container-api/Cargo.toml" ]] || error "--build-from: $BUILD_FROM/container-api/Cargo.toml not found (inside OrbStack your Mac home is at /Users/<you>, not ~)"
  log "Building container-api from $BUILD_FROM"
  # protobuf-compiler: containerd-client generates its gRPC bindings at build time
  apt-get install -y -qq build-essential pkg-config protobuf-compiler >/dev/null
  # The repo's .cargo/config.toml names cross-compiler binaries even for native
  # builds; point those names at the system toolchain.
  for t in gcc g++ ar; do ln -sf "/usr/bin/$t" "/usr/local/bin/${ARCH}-unknown-linux-gnu-$t"; done
  BUILD_USER="${SUDO_USER:-root}"
  BUILD_HOME=$(getent passwd "$BUILD_USER" | cut -d: -f6)
  if [[ ! -x "$BUILD_HOME/.cargo/bin/cargo" ]]; then
    log "Installing rustup for $BUILD_USER..."
    sudo -u "$BUILD_USER" -H bash -c 'curl -fsSL https://sh.rustup.rs | sh -s -- -y -q --profile minimal' >/dev/null
  fi
  # Separate target dir: the checkout may be shared with a macOS build.
  TARGET_DIR="/var/tmp/nordkraft-build"
  mkdir -p "$TARGET_DIR" && chown "$BUILD_USER" "$TARGET_DIR"
  sudo -u "$BUILD_USER" -H bash -c "cd '$BUILD_FROM/container-api' && CARGO_TARGET_DIR='$TARGET_DIR' '$BUILD_HOME/.cargo/bin/cargo' build --release 2>&1 | tail -3"
  LOCAL_BINARY="$TARGET_DIR/release/container-api"
  [[ -x "$LOCAL_BINARY" ]] || error "build failed — see output above"
  file "$LOCAL_BINARY" | grep -q "ELF" || error "$LOCAL_BINARY is not a Linux binary"
fi
if [[ -n "$LOCAL_BINARY" ]] && ! file "$LOCAL_BINARY" | grep -q "ELF.*${ARCH/x86_64/x86-64}"; then
  error "--binary $LOCAL_BINARY is not a Linux $ARCH executable: $(file -b "$LOCAL_BINARY" | cut -c1-60)"
fi
# =============================================================================
if [[ -n "$LOCAL_BINARY" ]]; then
  install -m 755 "$LOCAL_BINARY" /usr/local/bin/nordkraft-node
  log "Installed local binary → /usr/local/bin/nordkraft-node"
elif [[ -x /usr/local/bin/nordkraft-node && "$NORDKRAFT_VERSION" == "latest" && ! $FORCE ]]; then
  log "Binary already present ($(/usr/local/bin/nordkraft-node --version 2>/dev/null || echo unknown)) — keep"
else
  if [[ "$NORDKRAFT_VERSION" == "latest" ]]; then
    NORDKRAFT_VERSION=$(curl -fsSL "https://api.github.com/repos/${GITHUB_REPO}/releases/latest" | jq -r '.tag_name')
  fi
  BINARY_URL="https://github.com/${GITHUB_REPO}/releases/download/${NORDKRAFT_VERSION}/nordkraft-linux-${BINARY_ARCH}"
  log "Downloading ${NORDKRAFT_VERSION}..."
  curl -fsSL "$BINARY_URL" -o /tmp/nordkraft-new
  curl -fsSL "${BINARY_URL}.sha256" -o /tmp/nordkraft-new.sha256
  EXPECTED=$(awk '{print $1}' /tmp/nordkraft-new.sha256)
  ACTUAL=$(sha256sum /tmp/nordkraft-new | awk '{print $1}')
  [[ "$EXPECTED" == "$ACTUAL" ]] || error "Checksum mismatch"
  install -m 755 /tmp/nordkraft-new /usr/local/bin/nordkraft-node
  rm -f /tmp/nordkraft-new /tmp/nordkraft-new.sha256
  log "Installed ${NORDKRAFT_VERSION} → /usr/local/bin/nordkraft-node"
fi

mkdir -p "$NORDKRAFT_DIR" "$NORDKRAFT_DATA" "$NORDKRAFT_LOG"
chmod 750 "$NORDKRAFT_DIR"

# =============================================================================
# Phase 3: PostgreSQL (controller / hybrid only)
# =============================================================================
DATABASE_URL=""
if $IS_SERVER; then
  section "Phase 3: PostgreSQL"
  svc enable-now postgresql
  if $DEV; then
    DB_PASSWORD="nordkraft"
  elif [[ -f "${NORDKRAFT_DIR}/config.env" ]] && grep -q '^DATABASE_URL=' "${NORDKRAFT_DIR}/config.env"; then
    DB_PASSWORD=$(grep '^DATABASE_URL=' "${NORDKRAFT_DIR}/config.env" | sed -E 's#.*://[^:]+:([^@]+)@.*#\1#')
  else
    DB_PASSWORD=$(openssl rand -base64 32 | tr -dc 'a-zA-Z0-9' | head -c 32)
  fi

  sudo -u postgres psql -v ON_ERROR_STOP=1 <<SQL
DO \$\$ BEGIN
  IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = '${DB_USER}') THEN
    CREATE USER ${DB_USER} WITH PASSWORD '${DB_PASSWORD}';
  ELSE
    ALTER USER ${DB_USER} WITH PASSWORD '${DB_PASSWORD}';
  END IF;
END \$\$;
SELECT 'CREATE DATABASE ${DB_NAME} OWNER ${DB_USER}'
WHERE NOT EXISTS (SELECT FROM pg_database WHERE datname = '${DB_NAME}')\gexec
GRANT ALL PRIVILEGES ON DATABASE ${DB_NAME} TO ${DB_USER};
SQL
  DATABASE_URL="postgres://${DB_USER}:${DB_PASSWORD}@localhost/${DB_NAME}"
  log "Database '${DB_NAME}' ready (schema is applied by the server on first start)"
fi

# =============================================================================
# Phase 4: WireGuard + NATS (production only)
# =============================================================================
WG_PUBLIC_KEY=""
if ! $DEV; then
  section "Phase 4: WireGuard"
  if [[ ! -f "${NORDKRAFT_DIR}/server.key" ]]; then
    WG_PRIVATE_KEY=$(wg genkey)
    echo "$WG_PRIVATE_KEY" > "${NORDKRAFT_DIR}/server.key"
    chmod 600 "${NORDKRAFT_DIR}/server.key"
  fi
  WG_PRIVATE_KEY=$(cat "${NORDKRAFT_DIR}/server.key")
  WG_PUBLIC_KEY=$(echo "$WG_PRIVATE_KEY" | wg pubkey)
  echo "$WG_PUBLIC_KEY" > "${NORDKRAFT_DIR}/server.pub"

  if $IS_SERVER; then
    if [[ ! -f "/etc/wireguard/${WG_INTERFACE}.conf" || $FORCE == true ]]; then
      cat > "/etc/wireguard/${WG_INTERFACE}.conf" <<EOF
[Interface]
PrivateKey = ${WG_PRIVATE_KEY}
Address = ${VPN_SERVER_IP}/16
ListenPort = ${WG_PORT}
PostUp   = nft add table inet filter; nft add chain inet filter forward { type filter hook forward priority 0 \; policy drop \; }
EOF
      chmod 600 "/etc/wireguard/${WG_INTERFACE}.conf"
    fi
    svc enable-now "wg-quick@${WG_INTERFACE}"
    log "WireGuard ${WG_INTERFACE} up on ${VPN_SERVER_IP}:${WG_PORT}"

    section "Phase 4b: NATS"
    if ! command -v nats-server >/dev/null; then
      NATS_VERSION=$(curl -fsSL https://api.github.com/repos/nats-io/nats-server/releases/latest | jq -r '.tag_name')
      curl -fsSL "https://github.com/nats-io/nats-server/releases/download/${NATS_VERSION}/nats-server-${NATS_VERSION}-linux-${GO_ARCH}.tar.gz" \
        | tar -xz --strip-components=1 -C /usr/local/bin/ "nats-server-${NATS_VERSION}-linux-${GO_ARCH}/nats-server"
    fi
    mkdir -p /etc/nats /var/lib/nats
    [[ -f /etc/nats/nats.conf ]] || cat > /etc/nats/nats.conf <<EOF
listen: 0.0.0.0:4222
http_port: 8222
jetstream { store_dir: /var/lib/nats, max_memory_store: 256MB, max_file_store: 1GB }
EOF
    cat > /etc/systemd/system/nats-server.service <<EOF
[Unit]
Description=NATS Server
After=network.target
[Service]
ExecStart=/usr/local/bin/nats-server -c /etc/nats/nats.conf
Restart=always
RestartSec=5
LimitNOFILE=65536
[Install]
WantedBy=multi-user.target
EOF
    svc daemon-reload
    svc enable-now nats-server
    log "NATS on :4222"
  else
    log "Agent WireGuard key: ${WG_PUBLIC_KEY}"
    log "Add this agent as a peer on the controller, then reach NATS at ${CONTROLLER_IP}:4222"
  fi
fi

# =============================================================================
section "Phase 5: Config + service"
# =============================================================================
if [[ -f "${NORDKRAFT_DIR}/config.env" && $FORCE == false ]]; then
  warn "Keeping existing ${NORDKRAFT_DIR}/config.env (use --force to rewrite)"
else
  if $DEV; then
    cat > "${NORDKRAFT_DIR}/config.env" <<EOF
# Nordkraft DEV node — generated by install-node.sh --dev
# Everything that can reach ${DEV_BIND}:${API_PORT} is trusted as the dev user.
NODE_ID=${NODE_ID}
BIND_ADDRESS=${DEV_BIND}
BIND_PORT=${API_PORT}
DATABASE_URL=${DATABASE_URL}
BRIDGE_PUBLISH_ADDR=${DEV_BIND}
$($IN_ORBSTACK && echo "PUBLIC_HOST=$(hostname).orb.local" || echo "# PUBLIC_HOST=<hostname or IP to print in URLs>")
RUST_LOG=info
EOF
    EXEC="/usr/local/bin/nordkraft-node --dev"
  elif $IS_SERVER; then
    cat > "${NORDKRAFT_DIR}/config.env" <<EOF
# Nordkraft node (${MODE}) — generated by install-node.sh
NORDKRAFT_MODE=${MODE}
NODE_ID=${NODE_ID}
BIND_ADDRESS=${VPN_SERVER_IP}
BIND_PORT=${API_PORT}
DATABASE_URL=${DATABASE_URL}
NATS_ENABLED=true
NATS_URL=nats://127.0.0.1:4222
WG_INTERFACE=${WG_INTERFACE}
# VPN traffic no generated nft rule accepts: drop (default) or audit (log only). See setup/CHILD_PEERS.md
VPN_DENY_MODE=drop
VPN_NETWORK=172.20.0.0/16
CONTAINER_NETWORK=172.21.0.0/16
NETWORK_MODE=macvlan
MACVLAN_INTERFACE=$(ip -o route get 1.1.1.1 | awk '{print $5}')
CONTROLLER_IP=${VPN_SERVER_IP}
ADMIN_API_KEY=$(openssl rand -hex 32)
ADMIN_ALLOWED_IPS=127.0.0.1,${VPN_SERVER_IP}
USE_KATA=$($KATA && echo true || echo false)
INGRESS_ENABLED=false
QUOTA_ENFORCED=false
RUST_LOG=info
EOF
    EXEC="/usr/local/bin/nordkraft-node"
  else
    cat > "${NORDKRAFT_DIR}/config.env" <<EOF
# Nordkraft AGENT (worker) — generated by install-node.sh
NORDKRAFT_MODE=agent
NODE_ID=${NODE_ID}
BIND_ADDRESS=0.0.0.0
BIND_PORT=${API_PORT}
NATS_ENABLED=true
NATS_URL=nats://${CONTROLLER_IP}:4222
CONTROLLER_IP=${CONTROLLER_IP}
CONTROLLER_INTERNAL_IP=${CONTROLLER_IP}
VPN_NETWORK=172.20.0.0/16
CONTAINER_NETWORK=172.21.0.0/16
NETWORK_MODE=macvlan
MACVLAN_INTERFACE=$(ip -o route get "$CONTROLLER_IP" | awk '{print $5}')
AGENT_INTERFACE=$(ip -o route get "$CONTROLLER_IP" | awk '{print $5}')
ADMIN_API_KEY=unused-on-agent
USE_KATA=$($KATA && echo true || echo false)
RUST_LOG=info
EOF
    EXEC="/usr/local/bin/nordkraft-node"
  fi
  chmod 640 "${NORDKRAFT_DIR}/config.env"
  log "Wrote ${NORDKRAFT_DIR}/config.env"
fi
: "${EXEC:=/usr/local/bin/nordkraft-node$($DEV && echo " --dev")}"

cat > /etc/systemd/system/nordkraft-node.service <<EOF
[Unit]
Description=Nordkraft node (${MODE}$($DEV && echo ', dev'))
After=network-online.target containerd.service$($IS_SERVER && echo ' postgresql.service')
Wants=containerd.service

[Service]
EnvironmentFile=${NORDKRAFT_DIR}/config.env
ExecStart=${EXEC}
Restart=always
RestartSec=3
User=root

[Install]
WantedBy=multi-user.target
EOF
if $USE_SYSTEMD; then
  systemctl daemon-reload
  systemctl enable nordkraft-node >/dev/null
  systemctl restart nordkraft-node
  sleep 2
  systemctl is-active --quiet nordkraft-node && log "nordkraft-node.service running" || warn "nordkraft-node.service not running — journalctl -u nordkraft-node -n 50"
else
  log "Unit written to /etc/systemd/system/nordkraft-node.service (not started: --no-systemd)"
  log "Start by hand:  set -a; . ${NORDKRAFT_DIR}/config.env; set +a; ${EXEC}"
fi

# =============================================================================
section "Done"
# =============================================================================
if $DEV; then
  cat <<EOF

  Dev node ready.

    API:         http://127.0.0.1:${API_PORT}/api
    Try it:      curl http://127.0.0.1:${API_PORT}/api/status
    CLI:         nordkraft config set-api http://127.0.0.1:${API_PORT}
                 nordkraft deploy nginx:alpine -p 80
    Reach it:    curl http://$($IN_ORBSTACK && echo "$(hostname).orb.local" || echo "127.0.0.1")
    Logs:        journalctl -u nordkraft-node -f
    Start over:  sudo bash install-node.sh --dev --reset

  No WireGuard, no auth: anything that can reach the port is the dev user.
  Keep it on 127.0.0.1 or a private network.

EOF
elif $IS_SERVER; then
  cat <<EOF

  Node ready (${MODE}).

    WireGuard:   ${VPN_SERVER_IP}:${WG_PORT}   pubkey ${WG_PUBLIC_KEY}
    API:         http://${VPN_SERVER_IP}:${API_PORT}/api   (WireGuard only)
    Config:      ${NORDKRAFT_DIR}/config.env
    Logs:        journalctl -u nordkraft-node -f

  Next: create the first user/invite (see setup/SETUP.md) and connect with
  the CLI: curl -fsSL https://install.nordkraft.io | sh -s NKINVITE-...

EOF
else
  cat <<EOF

  Agent ready — joins controller ${CONTROLLER_IP} over NATS.

    Config:      ${NORDKRAFT_DIR}/config.env
    Logs:        journalctl -u nordkraft-node -f

  On the controller, register this node in the 'nodes' table and add its
  WireGuard key (${WG_PUBLIC_KEY}) as a peer.

EOF
fi
