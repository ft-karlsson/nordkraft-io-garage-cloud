#!/bin/bash

set -e

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
DIM='\033[2m'
NC='\033[0m'

echo -e "${RED}🧹 Nordkraft Full Uninstall${NC}"
echo -e "${DIM}Removes CLI, WireGuard config, and all local state${NC}"
echo ""

# What we'll remove
echo "This will:"
echo "  • Disconnect WireGuard VPN (nordkraft interface)"
echo "  • Remove nordkraft binary from /usr/local/bin"
echo "  • Remove ~/.nordkraft/ (keys, config, aliases)"
echo "  • Remove WireGuard system config for nordkraft"
echo ""
echo -e "${YELLOW}⚠️  You will need a new invite token to set up again.${NC}"
echo ""

# Confirm unless --force
if [[ "$1" != "--force" ]]; then
    read -p "Continue? [y/N] " -n 1 -r < /dev/tty
    echo
    if [[ ! $REPLY =~ ^[Yy]$ ]]; then
        echo -e "${DIM}Cancelled.${NC}"
        exit 0
    fi
fi

echo ""

# 1. Bring WireGuard down
WG_INTERFACE="nordkraft"
if command -v wg-quick >/dev/null 2>&1; then
    # Check if the interface is active — works on both Linux and macOS
    # macOS uses utun* names so we can't check by interface name directly
    if sudo wg show "$WG_INTERFACE" >/dev/null 2>&1; then
        echo -e "${DIM}  Disconnecting WireGuard...${NC}"
        sudo wg-quick down "$WG_INTERFACE" 2>/dev/null || true
        echo -e "${GREEN}✔${NC} WireGuard disconnected"
    else
        echo -e "${DIM}  WireGuard not active${NC}"
    fi
else
    echo -e "${DIM}  wg-quick not found, skipping${NC}"
fi

# 2. Remove WireGuard system config (check both paths — macOS wg-quick uses either)
WG_DIRS="/etc/wireguard"
case "$(uname -s)" in
    Darwin*) WG_DIRS="/etc/wireguard /usr/local/etc/wireguard" ;;
esac

for WG_DIR in $WG_DIRS; do
    WG_CONF="${WG_DIR}/${WG_INTERFACE}.conf"
    if [ -f "$WG_CONF" ]; then
        echo -e "${DIM}  Removing ${WG_CONF}...${NC}"
        sudo rm -f "$WG_CONF"
        echo -e "${GREEN}✔${NC} Removed ${WG_CONF}"
    fi
done

# 3. Remove ~/.nordkraft/
NK_DIR="${HOME}/.nordkraft"
if [ -d "$NK_DIR" ]; then
    echo -e "${DIM}  Removing ${NK_DIR}/...${NC}"
    rm -rf "$NK_DIR"
    echo -e "${GREEN}✔${NC} Local config removed"
else
    echo -e "${DIM}  No ~/.nordkraft/ found${NC}"
fi

# 4. Remove binary
NK_BIN="/usr/local/bin/nordkraft"
if [ -f "$NK_BIN" ]; then
    echo -e "${DIM}  Removing ${NK_BIN}...${NC}"
    sudo rm -f "$NK_BIN"
    echo -e "${GREEN}✔${NC} Binary removed"
else
    echo -e "${DIM}  No binary found at ${NK_BIN}${NC}"
fi

echo ""
echo -e "${GREEN}✅ Uninstall complete.${NC}"
echo ""
echo -e "${CYAN}To reinstall:${NC}"
echo "  curl -fsSL https://install.nordkraft.io | sh -s NKINVITE-..."
echo ""
