#!/bin/bash
set -euo pipefail

# =============================================================================
# NordKraft Server Install Script
# Ubuntu 22.04 / 24.04 only
# Usage: curl -fsSL https://install.nordkraft.io/server | sudo bash
# =============================================================================

NORDKRAFT_VERSION="${NORDKRAFT_VERSION:-latest}"
NORDKRAFT_USER="nordkraft"
NORDKRAFT_DIR="/etc/nordkraft"
NORDKRAFT_DATA="/var/lib/nordkraft"
NORDKRAFT_LOG="/var/log/nordkraft"
DB_NAME="nordkraft"
DB_USER="garage_user"
WG_INTERFACE="wg0"
WG_PORT="51820"
API_PORT="8001"
PEER_RESOLVER_PORT="3001"
VPN_SUBNET="172.20.0.0/16"
VPN_SERVER_IP="172.20.0.254"
CONTAINER_SUBNET_BASE="172.21.1.0/24"
GITHUB_REPO="ft-karlsson/nordkraft-io-garage-cloud"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

log()     { echo -e "${GREEN}[nordkraft]${NC} $1"; }
warn()    { echo -e "${YELLOW}[warning]${NC} $1"; }
error()   { echo -e "${RED}[error]${NC} $1"; exit 1; }
section() { echo -e "\n${BLUE}━━━ $1 ━━━${NC}"; }

# =============================================================================
# Preflight checks
# =============================================================================
section "Preflight"

[[ $EUID -ne 0 ]] && error "Run as root: sudo bash install.sh"

# Ubuntu only
if ! grep -qi ubuntu /etc/os-release; then
  error "NordKraft requires Ubuntu (22.04 or 24.04)"
fi

UBUNTU_VERSION=$(grep VERSION_ID /etc/os-release | cut -d'"' -f2)
log "Ubuntu ${UBUNTU_VERSION} detected"

# Architecture
ARCH=$(uname -m)
case "$ARCH" in
  x86_64)   BINARY_ARCH="x86_64" ;;
  aarch64)  BINARY_ARCH="aarch64" ;;
  *)        error "Unsupported architecture: $ARCH" ;;
esac
log "Architecture: $ARCH"

# Get server's public IP
SERVER_IP=$(curl -fsSL --max-time 5 https://api.ipify.org 2>/dev/null || hostname -I | awk '{print $1}')
log "Server IP: ${SERVER_IP}"

NODE_ID="${HOSTNAME}"
log "Node ID: ${NODE_ID}"

# The one question we ask
echo ""
echo -e "${BLUE}What do you want to call your garage cloud?${NC}"
echo -e "  (press enter for '${HOSTNAME} Garage')"
read -r -p "  > " GARAGE_NAME
GARAGE_NAME="${GARAGE_NAME:-$(echo "${HOSTNAME} Garage")}"
log "Your garage: ${GARAGE_NAME}"
echo ""

# =============================================================================
# Phase 1 — System dependencies
# =============================================================================
section "Phase 1: Installing dependencies"

apt-get update -qq
apt-get install -y -qq \
  wireguard \
  wireguard-tools \
  postgresql \
  postgresql-client \
  nftables \
  curl \
  jq \
  uuid-runtime

log "Dependencies installed"

# NATS server
if ! command -v nats-server &>/dev/null; then
  log "Installing NATS server..."
  NATS_VERSION=$(curl -fsSL https://api.github.com/repos/nats-io/nats-server/releases/latest | jq -r '.tag_name')
  NATS_ARCH=$([ "$ARCH" = "x86_64" ] && echo "amd64" || echo "arm64")
  curl -fsSL "https://github.com/nats-io/nats-server/releases/download/${NATS_VERSION}/nats-server-${NATS_VERSION}-linux-${NATS_ARCH}.tar.gz" \
    | tar -xz --strip-components=1 -C /usr/local/bin/ "nats-server-${NATS_VERSION}-linux-${NATS_ARCH}/nats-server"
  log "NATS ${NATS_VERSION} installed"
fi

# =============================================================================
# Phase 2 — Download NordKraft binaries
# =============================================================================
section "Phase 2: Downloading NordKraft"

if [ "$NORDKRAFT_VERSION" = "latest" ]; then
  NORDKRAFT_VERSION=$(curl -fsSL "https://api.github.com/repos/${GITHUB_REPO}/releases/latest" | jq -r '.tag_name')
fi
log "Version: ${NORDKRAFT_VERSION}"

BINARY_URL="https://github.com/${GITHUB_REPO}/releases/download/${NORDKRAFT_VERSION}/nordkraft-${BINARY_ARCH}-linux"
CHECKSUM_URL="${BINARY_URL}.sha256"

log "Downloading binary..."
curl -fsSL "$BINARY_URL" -o /tmp/nordkraft-new
curl -fsSL "$CHECKSUM_URL" -o /tmp/nordkraft-new.sha256

# Verify checksum
EXPECTED=$(cat /tmp/nordkraft-new.sha256 | awk '{print $1}')
ACTUAL=$(sha256sum /tmp/nordkraft-new | awk '{print $1}')
[ "$EXPECTED" = "$ACTUAL" ] || error "Checksum mismatch! Expected $EXPECTED, got $ACTUAL"
log "Checksum verified"

install -m 755 /tmp/nordkraft-new /usr/local/bin/nordkraft
rm -f /tmp/nordkraft-new /tmp/nordkraft-new.sha256
log "Binary installed → /usr/local/bin/nordkraft"

# =============================================================================
# Phase 3 — Create directories and user
# =============================================================================
section "Phase 3: Setup directories"

id -u "$NORDKRAFT_USER" &>/dev/null || useradd --system --no-create-home --shell /bin/false "$NORDKRAFT_USER"

mkdir -p "$NORDKRAFT_DIR" "$NORDKRAFT_DATA" "$NORDKRAFT_LOG"
chmod 750 "$NORDKRAFT_DIR"

# =============================================================================
# Phase 4 — WireGuard
# =============================================================================
section "Phase 4: WireGuard"

WG_PRIVATE_KEY=$(wg genkey)
WG_PUBLIC_KEY=$(echo "$WG_PRIVATE_KEY" | wg pubkey)

# Store keys
echo "$WG_PRIVATE_KEY" > "${NORDKRAFT_DIR}/server.key"
echo "$WG_PUBLIC_KEY"  > "${NORDKRAFT_DIR}/server.pub"
chmod 600 "${NORDKRAFT_DIR}/server.key"

cat > "/etc/wireguard/${WG_INTERFACE}.conf" <<EOF
[Interface]
PrivateKey = ${WG_PRIVATE_KEY}
Address = ${VPN_SERVER_IP}/16
ListenPort = ${WG_PORT}
PostUp   = nft add table ip nordkraft; nft add chain ip nordkraft forward { type filter hook forward priority 0 \; }
PostDown = nft delete table ip nordkraft
EOF

chmod 600 "/etc/wireguard/${WG_INTERFACE}.conf"
systemctl enable --now "wg-quick@${WG_INTERFACE}"
log "WireGuard interface ${WG_INTERFACE} up — server IP: ${VPN_SERVER_IP}"

# =============================================================================
# Phase 5 — PostgreSQL
# =============================================================================
section "Phase 5: PostgreSQL"

# Generate a random DB password
DB_PASSWORD=$(openssl rand -base64 32 | tr -dc 'a-zA-Z0-9' | head -c 32)

systemctl enable --now postgresql

# Create user and database
sudo -u postgres psql -v ON_ERROR_STOP=1 <<SQL
DO \$\$ BEGIN
  IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = '${DB_USER}') THEN
    CREATE USER ${DB_USER} WITH PASSWORD '${DB_PASSWORD}';
  END IF;
END \$\$;
CREATE DATABASE ${DB_NAME} OWNER ${DB_USER};
GRANT ALL PRIVILEGES ON DATABASE ${DB_NAME} TO ${DB_USER};
SQL

log "Database '${DB_NAME}' created"

# Run schema — fetch from GitHub release assets
SCHEMA_URL="https://github.com/${GITHUB_REPO}/releases/download/${NORDKRAFT_VERSION}/schema.sql"
curl -fsSL "$SCHEMA_URL" -o /tmp/nordkraft-schema.sql

# Strip \restrict / \unrestrict lines (pg_dump artifacts)
grep -v '\\restrict\|\\unrestrict' /tmp/nordkraft-schema.sql > /tmp/nordkraft-schema-clean.sql

# Fix owner references: schema uses garage_user, must exist before schema runs
sudo -u postgres psql -d "$DB_NAME" -v ON_ERROR_STOP=1 -f /tmp/nordkraft-schema-clean.sql
rm -f /tmp/nordkraft-schema.sql /tmp/nordkraft-schema-clean.sql

log "Schema applied"

# Seed: default plan
GARAGE_ID="garage-${NODE_ID}"

sudo -u postgres psql -d "$DB_NAME" -v ON_ERROR_STOP=1 <<SQL

-- Default plan
INSERT INTO plans (id, name, display_name, cpu, memory, storage, price, description, is_active)
VALUES ('my-garage-cloud', 'my-garage-cloud', 'My Garage Cloud', 'unlimited', 'unlimited', 'unlimited', '0', 'Your hardware, your rules.', true)
ON CONFLICT (id) DO NOTHING;

-- This garage/datacenter
INSERT INTO garages (garage_id, name, location, country, vpn_endpoint, wireguard_public_key, container_subnet_base, status)
VALUES ('${GARAGE_ID}', '${GARAGE_NAME}', 'local', 'DK', '${SERVER_IP}:${WG_PORT}', '${WG_PUBLIC_KEY}', '${CONTAINER_SUBNET_BASE}', 'active')
ON CONFLICT (garage_id) DO NOTHING;

-- This node (hybrid mode — controller + agent)
INSERT INTO nodes (node_id, name, location, ip_range, internal_ip, garage_id, hardware_type, architecture, status, network_interface)
VALUES ('${NODE_ID}', '${NODE_ID}', 'local', '${CONTAINER_SUBNET_BASE}', '${VPN_SERVER_IP}', '${GARAGE_ID}', 'hybrid', '${ARCH}', 'active', 'eth0')
ON CONFLICT (node_id) DO NOTHING;

-- TCP ingress port pool (10000-10999)
INSERT INTO ingress_port_pool (port)
SELECT generate_series(10000, 10999)
ON CONFLICT DO NOTHING;

SQL

log "Database seeded"

# =============================================================================
# Phase 6 — NATS
# =============================================================================
section "Phase 6: NATS"

mkdir -p /etc/nats
cat > /etc/nats/nats.conf <<EOF
port: 4222
http_port: 8222

jetstream {
  store_dir: /var/lib/nats
  max_memory_store: 256MB
  max_file_store: 1GB
}

log_file: /var/log/nordkraft/nats.log
logtime: true
EOF

mkdir -p /var/lib/nats

cat > /etc/systemd/system/nats-server.service <<EOF
[Unit]
Description=NATS Server
After=network.target

[Service]
ExecStart=/usr/local/bin/nats-server -c /etc/nats/nats.conf
Restart=always
RestartSec=5
User=root
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable --now nats-server
log "NATS running on :4222"

# =============================================================================
# Phase 7 — NordKraft config + systemd
# =============================================================================
section "Phase 7: NordKraft service"

DATABASE_URL="postgresql://${DB_USER}:${DB_PASSWORD}@localhost/${DB_NAME}"

cat > "${NORDKRAFT_DIR}/config.env" <<EOF
NORDKRAFT_MODE=hybrid
NODE_ID=${NODE_ID}
GARAGE_ID=${GARAGE_ID}

# Network
BIND_ADDRESS=${VPN_SERVER_IP}
BIND_PORT=${API_PORT}

# Database
DATABASE_URL=${DATABASE_URL}

# NATS
NATS_ENABLED=true
NATS_URL=nats://127.0.0.1:4222

# Auth (peer resolver — maps WireGuard IPs to public keys)
PEER_RESOLVER_HOST=127.0.0.1
PEER_RESOLVER_PORT=${PEER_RESOLVER_PORT}
DEV_MODE=false

# WireGuard
WG_INTERFACE=${WG_INTERFACE}
WG_CONFIG=/etc/wireguard/${WG_INTERFACE}.conf
EOF

chmod 640 "${NORDKRAFT_DIR}/config.env"

cat > /etc/systemd/system/nordkraft.service <<EOF
[Unit]
Description=NordKraft Container Orchestrator
After=network.target postgresql.service nats-server.service wg-quick@${WG_INTERFACE}.service
Wants=postgresql.service nats-server.service

[Service]
EnvironmentFile=${NORDKRAFT_DIR}/config.env
ExecStart=/usr/local/bin/nordkraft serve
Restart=always
RestartSec=5
User=root
StandardOutput=append:${NORDKRAFT_LOG}/nordkraft.log
StandardError=append:${NORDKRAFT_LOG}/nordkraft.log

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable --now nordkraft
log "NordKraft service running"

# =============================================================================
# Phase 8 — Generate first invite token
# =============================================================================
section "Phase 8: First admin invite"

# Generate NKINVITE token directly in DB
# The API will swap this for a real WireGuard key on /api/claim
INVITE_TOKEN="NKINVITE-$(uuidgen)"

# Allocate a WireGuard IP for first user (172.20.1.1)
FIRST_WG_IP="172.20.1.1"

sudo -u postgres psql -d "$DB_NAME" -v ON_ERROR_STOP=1 <<SQL
INSERT INTO users (
  id, email, full_name, address,
  wireguard_public_key, wireguard_ip, client_ip,
  node_id, plan_id, account_status, primary_garage_id
) VALUES (
  '$(uuidgen)',
  'admin@localhost',
  'Admin',
  'local',
  '${INVITE_TOKEN}',
  '${FIRST_WG_IP}',
  '0.0.0.0',
  '${NODE_ID}',
  'test',
  'pending',
  '${GARAGE_ID}'
);
SQL

log "Invite token created"

# =============================================================================
# Phase 9 — Install update script
# =============================================================================
section "Phase 9: Update script"

cat > /usr/local/bin/nordkraft-update <<'UPDATEEOF'
#!/bin/bash
set -euo pipefail

GITHUB_REPO="nordkraft/nordkraft"
ARCH=$(uname -m)
case "$ARCH" in
  x86_64)   BINARY_ARCH="x86_64" ;;
  aarch64)  BINARY_ARCH="aarch64" ;;
  *)        echo "Unsupported architecture: $ARCH"; exit 1 ;;
esac

[[ $EUID -ne 0 ]] && echo "Run as root: sudo nordkraft-update" && exit 1

CURRENT=$(/usr/local/bin/nordkraft --version 2>/dev/null | awk '{print $2}' || echo "unknown")
LATEST=$(curl -fsSL "https://api.github.com/repos/${GITHUB_REPO}/releases/latest" | grep tag_name | cut -d'"' -f4)

if [ "$CURRENT" = "$LATEST" ]; then
  echo "Already up to date: ${CURRENT}"
  exit 0
fi

echo "Updating NordKraft: ${CURRENT} → ${LATEST}"

BINARY_URL="https://github.com/${GITHUB_REPO}/releases/download/${LATEST}/nordkraft-${BINARY_ARCH}-linux"
CHECKSUM_URL="${BINARY_URL}.sha256"

curl -fsSL "$BINARY_URL" -o /tmp/nordkraft-new
curl -fsSL "$CHECKSUM_URL" -o /tmp/nordkraft-new.sha256

EXPECTED=$(awk '{print $1}' /tmp/nordkraft-new.sha256)
ACTUAL=$(sha256sum /tmp/nordkraft-new | awk '{print $1}')
[ "$EXPECTED" = "$ACTUAL" ] || { echo "Checksum mismatch!"; exit 1; }

echo "Stopping NordKraft service..."
systemctl stop nordkraft

# Keep backup one version back
cp /usr/local/bin/nordkraft /usr/local/bin/nordkraft.bak
install -m 755 /tmp/nordkraft-new /usr/local/bin/nordkraft
rm -f /tmp/nordkraft-new /tmp/nordkraft-new.sha256

echo "Starting NordKraft service..."
systemctl start nordkraft

echo "✓ NordKraft updated to ${LATEST}"
echo "  Containers were not affected."
UPDATEEOF

chmod +x /usr/local/bin/nordkraft-update
log "Update script installed → /usr/local/bin/nordkraft-update"

# =============================================================================
# Done — print summary
# =============================================================================
section "Setup Complete"

SUMMARY_FILE="${NORDKRAFT_DIR}/setup-summary.txt"

cat > "$SUMMARY_FILE" <<EOF
═══════════════════════════════════════════════════
  NordKraft Setup Summary
  $(date)
═══════════════════════════════════════════════════

Server
  Hostname:          ${NODE_ID}
  Public IP:         ${SERVER_IP}
  WireGuard IP:      ${VPN_SERVER_IP}
  WireGuard pubkey:  ${WG_PUBLIC_KEY}
  WireGuard port:    ${WG_PORT}
  API endpoint:      http://${VPN_SERVER_IP}:${API_PORT}

Database
  URL:               ${DATABASE_URL}

Services
  nordkraft:         systemctl status nordkraft
  nats-server:       systemctl status nats-server
  wg-quick@wg0:      systemctl status wg-quick@wg0
  postgresql:        systemctl status postgresql

Logs
  tail -f ${NORDKRAFT_LOG}/nordkraft.log

═══════════════════════════════════════════════════
  First User Setup
═══════════════════════════════════════════════════

On your LOCAL machine, run:

  curl -fsSL https://install.nordkraft.io | sh -s ${INVITE_TOKEN}

This will:
  1. Generate your WireGuard keypair (private key never leaves your machine)
  2. Claim the invite token and register your public key
  3. Configure WireGuard to connect to this server
  4. You're in — nordkraft ps, nordkraft deploy, etc. 

═══════════════════════════════════════════════════
  Updating NordKraft
═══════════════════════════════════════════════════

  sudo nordkraft-update

Containers keep running during updates.

═══════════════════════════════════════════════════
EOF

cat "$SUMMARY_FILE"
echo ""
log "Summary saved → ${SUMMARY_FILE}"
