#!/bin/bash

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
NC='\033[0m' # No Color

# Configuration
GITHUB_REPO="ft-karlsson/nordkraft-io-garage-cloud"
INSTALL_DIR="/usr/local/bin"
BINARY_NAME="nordkraft"
BASE_URL="https://github.com/${GITHUB_REPO}/releases/latest/download"
INVITE_TOKEN=""

# Print with escape sequences interpreted.
# NOT `echo -e`: the documented one-liner pipes this script into `sh`, which
# ignores the shebang above. macOS /bin/sh is bash in POSIX mode, where echo
# has no -e flag and prints it literally. printf behaves the same everywhere.
say() {
    printf '%b\n' "$1"
}

# Banner
say "${CYAN}🚀 Nordkraft CLI Installer${NC}"
say "${CYAN}Zero-Trust Container Cloud${NC}"
echo ""

# Function to print colored output
print_status() {
    say "${GREEN}✓${NC} $1"
}

print_warning() {
    say "${YELLOW}⚠${NC} $1"
}

print_error() {
    say "${RED}✗${NC} $1" >&2
}

print_info() {
    say "${BLUE}ℹ${NC} $1"
}

# Function to detect OS and architecture
detect_platform() {
    local os=""
    local arch=""
    
    case "$(uname -s)" in
        Darwin*) os="darwin" ;;
        Linux*) os="linux" ;;
        *) print_error "Unsupported OS: $(uname -s)"; exit 1 ;;
    esac
    
    case "$(uname -m)" in
        x86_64) arch="amd64" ;;
        arm64|aarch64) arch="arm64" ;;
        *) print_error "Unsupported architecture: $(uname -m)"; exit 1 ;;
    esac
    
    echo "${os}-${arch}"
}

# Function to check if command exists
command_exists() {
    command -v "$1" >/dev/null 2>&1
}

# Function to check dependencies
check_dependencies() {
    print_info "Checking dependencies..."
    
    if ! command_exists curl; then
        print_error "curl is required. Please install it first."
        exit 1
    fi
    
    if ! command_exists tar; then
        print_error "tar is required. Please install it first."
        exit 1
    fi
    
    print_status "Dependencies OK"
}

# Function to install WireGuard tools
install_wireguard_tools() {
    if command_exists wg; then
        print_status "WireGuard tools already installed"
        return 0
    fi

    print_info "Installing WireGuard tools..."

    case "$(uname -s)" in
        Darwin*)
            if command_exists brew; then
                brew install wireguard-tools 2>/dev/null
            else
                print_error "Homebrew required on macOS. Install from https://brew.sh"
                exit 1
            fi
            ;;
        Linux*)
            if command_exists apt-get; then
                sudo apt-get update -qq && sudo apt-get install -y -qq wireguard-tools
            elif command_exists dnf; then
                sudo dnf install -y wireguard-tools
            elif command_exists pacman; then
                sudo pacman -S --noconfirm wireguard-tools
            else
                print_error "Could not detect package manager. Install wireguard-tools manually."
                exit 1
            fi
            ;;
    esac

    if command_exists wg; then
        print_status "WireGuard tools installed"
    else
        print_error "WireGuard tools installation failed"
        exit 1
    fi
}

# Function to download and install
install_nordkraft() {
    local platform=$(detect_platform)
    local download_url="${BASE_URL}/nordkraft-${platform}.tar.gz"
    local temp_dir=$(mktemp -d)
    local temp_file="${temp_dir}/nordkraft.tar.gz"
    
    print_info "Detected platform: ${platform}"
    print_info "Downloading from: ${download_url}"
    
    # Download
    if ! curl -fsSL "${download_url}" -o "${temp_file}"; then
        print_error "Failed to download. Check https://github.com/${GITHUB_REPO}/releases"
        exit 1
    fi
    
    print_status "Downloaded ($(du -h "${temp_file}" | cut -f1))"
    
    # Extract
    if ! tar -xzf "${temp_file}" -C "${temp_dir}"; then
        print_error "Failed to extract archive"
        exit 1
    fi
    
    print_status "Extracted"
    
    # Find binary
    local binary_path="${temp_dir}/${BINARY_NAME}"
    if [ ! -f "$binary_path" ]; then
        binary_path=$(find "${temp_dir}" -name "nordkraft*" -type f | head -1)
    fi
    
    if [ ! -f "$binary_path" ]; then
        print_error "Could not find nordkraft binary in archive"
        exit 1
    fi
    
    chmod +x "$binary_path"
    
    # Install.
    # A fresh Apple Silicon Mac has no /usr/local/bin at all (Homebrew lives in
    # /opt/homebrew), so the directory must be created before anything is copied
    # into it — otherwise cp fails with ENOENT on the destination.
    if [ -d "${INSTALL_DIR}" ] && [ -w "${INSTALL_DIR}" ]; then
        install -m 755 "$binary_path" "${INSTALL_DIR}/${BINARY_NAME}"
    else
        print_warning "Need sudo to install to ${INSTALL_DIR}"
        if ! command_exists sudo; then
            print_error "sudo not available and ${INSTALL_DIR} is not writable"
            exit 1
        fi
        if ! sudo mkdir -p "${INSTALL_DIR}"; then
            print_error "Could not create ${INSTALL_DIR}"
            exit 1
        fi
        if ! sudo install -m 755 "$binary_path" "${INSTALL_DIR}/${BINARY_NAME}"; then
            print_error "Could not install to ${INSTALL_DIR}/${BINARY_NAME}"
            exit 1
        fi
    fi
    
    # Cleanup
    rm -rf "${temp_dir}"
    
    print_status "Installed to ${INSTALL_DIR}/${BINARY_NAME}"
}

# Function to verify installation
verify_installation() {
    if command_exists nordkraft; then
        local version=$(nordkraft --version 2>/dev/null || echo "unknown")
        print_status "Verified: ${version}"
        return 0
    else
        print_error "Verification failed - nordkraft not found in PATH"
        print_info "The binary is at ${INSTALL_DIR}/${BINARY_NAME} — add it to your PATH:"
        print_info "  echo 'export PATH=\"${INSTALL_DIR}:\$PATH\"' >> ~/.zshrc && source ~/.zshrc"
        return 1
    fi
}

# Function to show next steps
show_next_steps() {
    echo ""
    say "${GREEN}🎉 Installation Complete!${NC}"
    echo ""

    if [ -n "${INVITE_TOKEN}" ]; then
        say "${CYAN}Running setup with your invite token...${NC}"
        echo ""
        nordkraft setup "${INVITE_TOKEN}"
    else
        say "${CYAN}Next step:${NC}"
        echo "  Run the setup command from your signup page:"
        echo ""
        say "   ${YELLOW}nordkraft setup NKINVITE-xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx${NC}"
        echo ""
        echo "  This will configure WireGuard and connect you automatically."
        echo ""
    fi
}

# Main
main() {
    print_info "Starting installation..."

    # Parse args: token is NKINVITE-..., --force is a flag
    local force=false
    for arg in "$@"; do
        case "$arg" in
            --force) force=true ;;
            NKINVITE-*) INVITE_TOKEN="$arg" ;;
        esac
    done
    
    # Check if already installed
    if command_exists nordkraft; then
        local current_version=$(nordkraft --version 2>/dev/null || echo "unknown")
        print_status "Nordkraft already installed (${current_version})"
        
        if [ "$force" = true ]; then
            print_warning "Force reinstall..."
        elif [ -n "${INVITE_TOKEN}" ]; then
            # Already installed but got a token — skip install, run setup
            install_wireguard_tools
            echo ""
            say "${CYAN}Running setup with your invite token...${NC}"
            echo ""
            nordkraft setup "${INVITE_TOKEN}"
            exit 0
        else
            echo ""
            say "${CYAN}To update to latest version:${NC}"
            say "   ${YELLOW}nordkraft update${NC}"
            echo ""
            exit 0
        fi
    fi
    
    check_dependencies
    install_nordkraft
    install_wireguard_tools
    
    if verify_installation; then
        show_next_steps
    else
        exit 1
    fi
}

trap 'say "\n${RED}Cancelled${NC}"; exit 1' INT

main "$@"
