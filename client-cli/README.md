# Nordkraft Cloud CLI

A powerful command-line interface for developers to interact with the Nordkraft Cloud platform.

## Overview

The Nordkraft CLI provides developers with a fast, scriptable interface to deploy and manage containers, configure domains, and monitor usage without leaving the terminal.

## Features

- **Container Management**: Deploy, stop, delete, and monitor containers
- **Domain Configuration**: Set up and manage custom domains
- **Resource Monitoring**: Track CPU, memory, and storage usage
- **WireGuard Authentication**: Secure API access using WireGuard keys
- **Scripting Support**: Easily integrate with shell scripts and CI/CD pipelines
- **Output Formats**: JSON, YAML, and table output for easy parsing and viewing

## Installation

### Via Package Manager

```bash
# macOS
brew install nordkraft-cli

# Linux
curl -sL https://install.nordkraft.io/cli | bash
```

### Manual Download

Download the latest release from: https://github.com/nordkraft/cli/releases

### From Source

```bash
git clone https://github.com/nordkraft/cli.git
cd cli
cargo build --release
```

## Quick Start

```bash
# Deploy a container
nordkraft container deploy --image nginx:latest --port 8080

# List your containers
nordkraft container list

# Create a domain for your container
nordkraft domain create myapp --container-id nordkraft-abc123

# View usage stats (not implemented)
nordkraft stats
```

## Authentication

The CLI uses your WireGuard connection for authentication:

```bash
# Make sure WireGuard is connected
wg show

# Then run any command
nordkraft auth login
```


## License
TBD

