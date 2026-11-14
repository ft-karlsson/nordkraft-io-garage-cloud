use clap::{Args, Parser, Subcommand};
use colored::*;
use console::Term;
use dialoguer::{theme::ColorfulTheme, Confirm, Input};
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

mod cli;
mod core;
mod spinner;
mod stream;
mod tui;
mod volume;

use crate::core::aliases::*;
use crate::core::api::*;
use crate::core::config::*;
use crate::core::registry::*;
use crate::core::spec::*;
use cli::containers::*;
use cli::deploy::*;
use cli::specs::*;

// ============= ALIAS MANAGEMENT =============

// ============= CLI STRUCTURE =============

#[derive(Parser)]
#[command(name = "nordkraft")]
#[command(author = "Nordkraft.io")]
#[command(version = env!("CARGO_PKG_VERSION"))]
#[command(about = "🚀 Nordkraft Garage Cloud CLI - Secure Container Hosting")]
#[command(long_about = None)]
#[command(disable_help_subcommand = true)]
struct Cli {
    /// Output as JSON (for scripting)
    #[arg(long, global = true)]
    json: bool,

    /// Non-interactive: skip prompts and accept defaults (implied when
    /// stdin is not a terminal or --json is given)
    #[arg(short = 'y', long, global = true)]
    yes: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Authentication commands
    Auth {
        #[command(subcommand)]
        command: AuthCommands,
    },
    /// Your account: status, deletion (confirmed by an emailed code), restore
    Account {
        #[command(subcommand)]
        command: AccountCommands,
    },
    /// Container management
    #[command(alias = "c")]
    Container {
        #[command(subcommand)]
        command: ContainerCommands,
    },
    /// HTTPS ingress with auto-TLS (*.example.com)
    Ingress {
        #[command(subcommand)]
        command: IngressCommands,
    },
    /// Use your own domain (e.g. customer1.net) with a container
    Domain {
        #[command(subcommand)]
        command: DomainCommands,
    },
    /// IPv6 direct access (global addresses)
    Ipv6 {
        #[command(subcommand)]
        command: Ipv6Commands,
    },
    /// Extra WireGuard peers (a phone, a colleague, CI) with access to your containers only
    Peer {
        #[command(subcommand)]
        command: PeerCommands,
    },
    /// Network information
    Network {
        #[command(subcommand)]
        command: NetworkCommands,
    },
    /// Cluster node status
    Nodes,
    /// System status
    Status,
    /// CLI configuration (API endpoint — e.g. point at a local dev server)
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },
    /// Show help with examples
    Help,
    /// Private container registry
    Registry {
        #[command(subcommand)]
        command: RegistryCommands,
    },
    Ui,
    // ===== ALIAS MANAGEMENT =====
    /// Manage container aliases (short names)
    Alias {
        #[command(subcommand)]
        command: AliasCommands,
    },

    // ===== SHORTCUTS =====
    /// Deploy a container (shortcut for 'container deploy')
    Deploy(Box<DeployArgs>),
    /// Push a local image to your private registry
    Push {
        /// Image to push (e.g., myapp:v1)
        image: String,
    },
    /// List containers (shortcut for 'container list')
    #[command(alias = "ls")]
    List {
        /// Only containers with this label (KEY=VALUE, repeatable)
        #[arg(long = "label", short = 'l', value_name = "KEY=VALUE")]
        label: Vec<String>,
    },
    /// Wait for a container's main process to exit; exit with its code
    Wait {
        /// Container name or alias
        container: String,
        /// Give up after this many seconds (exit 124)
        #[arg(long, default_value = "300")]
        timeout: u64,
    },
    /// Interactive shell in a container (default /bin/sh)
    ///
    ///   nordkraft shell web
    ///   nordkraft shell web -- bash -l
    #[command(alias = "sh")]
    Shell {
        /// Container name, alias, or omit for interactive selection
        container: Option<String>,
        /// Command to run instead of /bin/sh (after --)
        #[arg(last = true)]
        cmd: Vec<String>,
    },
    /// Run a command in a container and exit with its status
    ///
    ///   nordkraft exec web -- ls -la /data
    ///   echo hi | nordkraft exec web --stdin -- cat
    Exec {
        /// Container name or alias
        container: String,
        /// Working directory inside the container
        #[arg(long)]
        cwd: Option<String>,
        /// Environment variable KEY=VALUE (repeatable)
        #[arg(long = "env", short = 'e', value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// Run as uid[:gid]
        #[arg(long)]
        user: Option<String>,
        /// Seconds before the command is killed (exit 124)
        #[arg(long, default_value = "120")]
        timeout: u64,
        /// Pass our stdin to the command
        #[arg(long)]
        stdin: bool,
        /// Command and arguments (after --)
        #[arg(last = true, required = true)]
        cmd: Vec<String>,
    },
    /// Get container logs (shortcut for 'container logs')
    Logs {
        /// Container name, alias, or omit for interactive selection
        container: Option<String>,
        /// Number of lines
        #[arg(short = 'n', long, default_value = "100")]
        lines: usize,
    },
    /// Download a full backup of a container's volume (tar.zst) to this machine
    ///
    ///   nordkraft backup campfire
    ///   nordkraft backup campfire -o campfire.tar.zst
    Backup {
        /// Container name, alias, or omit for interactive selection
        container: Option<String>,
        /// Output file (default: <name>-<timestamp>.tar.zst; "-" for stdout)
        #[arg(short, long, value_name = "FILE")]
        output: Option<String>,
    },
    /// Replace a container's volume with a backup file made by `nordkraft backup`
    ///
    ///   nordkraft restore campfire campfire-20261026T1840.tar.zst
    Restore {
        /// Container name or alias
        container: Option<String>,
        /// Backup file (tar.zst)
        file: String,
        /// Do not ask for confirmation
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Stop a container (shortcut for 'container stop')
    Stop {
        /// Container name, alias, or omit for interactive selection
        container: Option<String>,
    },
    /// Start a container (shortcut for 'container start')
    Start {
        /// Container name, alias, or omit for interactive selection
        container: Option<String>,
        /// Return only when the container is running (exec works)
        #[arg(long)]
        wait: bool,
    },
    /// Remove a container (shortcut for 'container rm')
    #[command(alias = "rm")]
    Remove {
        /// Container name, alias, or omit for interactive selection
        container: Option<String>,
        /// Kill immediately instead of a graceful stop first
        #[arg(short, long)]
        force: bool,
    },
    /// Update nordkraft CLI to the latest version
    Update {
        /// Check for updates without installing
        #[arg(long)]
        check: bool,
    },
    /// First-time setup: claim invite token, configure WireGuard, connect
    Setup {
        /// Invite token from signup (NKINVITE-...)
        token: String,
    },
    /// Connect to NordKraft via WireGuard
    Connect,
    /// Disconnect WireGuard
    Disconnect,
    /// Full reset: disconnect VPN, remove all local config (for testing)
    Reset {
        /// Skip confirmation prompt
        #[arg(long)]
        force: bool,
    },

    // ===== DECLARATIVE DEPLOYMENTS =====
    /// Compare .nk spec against live container
    Diff {
        /// Container name, alias, or omit for interactive selection
        container: Option<String>,
    },
    /// Apply .nk spec changes to running container
    Upgrade {
        /// Container name, alias, or omit for interactive selection
        container: Option<String>,
        /// Skip confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },
    /// Generate .nk spec from a running container
    Init {
        /// Container name, alias, or omit for interactive selection
        container: Option<String>,
        /// Rebuild from the config stored at deploy time instead of from the running container
        #[arg(long = "from-server")]
        from_server: bool,
    },
    /// Open .nk spec in $EDITOR
    Edit {
        /// Container name, alias, or omit for interactive selection
        container: Option<String>,
    },
    /// List all saved deployment specs
    Specs,
    /// Set a value in a .nk spec (e.g. `nordkraft spec-set web resources.cpu 2`)
    ///
    /// Use `+value` to append to an array, `-value` to remove from one.
    /// Examples:
    ///   nordkraft spec-set web deployment.image nginx:1.27
    ///   nordkraft spec-set web network.ports +8080:80
    ///   nordkraft spec-set web network.ports -8080:80
    SpecSet {
        /// Container name or alias
        container: String,
        /// Dotted key path (e.g. `resources.cpu`, `deployment.image`)
        key: String,
        /// New value (prefix with + to append / - to remove from arrays)
        #[arg(allow_hyphen_values = true)]
        value: String,
        /// Apply to running container immediately (runs `upgrade --yes`)
        #[arg(long)]
        apply: bool,
    },
    /// Remove a field from a .nk spec
    SpecUnset {
        /// Container name or alias
        container: String,
        /// Dotted key path (e.g. `network.ipv6`)
        key: String,
    },
    /// Delete a .nk spec file (does NOT destroy the running container)
    SpecDelete {
        /// Container name or alias
        container: String,
        /// Skip confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },
    /// Show plan usage vs. limits
    Usage,
    /// Watts and CO₂: your containers' share of the node's measured power
    Energy {
        /// Container name or alias; omit for the account overview
        container: Option<String>,
        /// Window: 1h, 24h, 7d, 30d
        #[arg(long, default_value = "24h")]
        since: String,
    },
    /// Show deploy lifecycle events
    Events {
        /// Filter by container name or alias
        container: Option<String>,
        /// Number of events to show
        #[arg(short = 'n', long, default_value = "20")]
        limit: usize,
    },
}

// ============= AUTH COMMANDS =============

#[derive(Subcommand)]
enum AuthCommands {
    /// Login and verify VPN connection
    Login,
    /// Show authentication status
    Status,
    /// Show detailed user info
    Whoami,
}

// ============= ACCOUNT COMMANDS =============

#[derive(Subcommand)]
enum AccountCommands {
    /// Show your account and any pending deletion
    Status,
    /// Delete your account. A code is emailed to you; once confirmed, everything
    /// is removed after a grace period. `nordkraft account restore` cancels.
    Delete {
        /// Skip the confirmation prompt (the emailed code is still required)
        #[arg(short, long)]
        yes: bool,
        /// No grace period: delete right after the code is confirmed
        #[arg(long)]
        now: bool,
        /// Code from the email (non-interactive; together with --verification-id)
        #[arg(long)]
        code: Option<String>,
        /// Verification id printed by an earlier `account delete`
        #[arg(long = "verification-id")]
        verification_id: Option<String>,
    },
    /// Cancel a pending deletion
    #[command(alias = "cancel")]
    Restore,
}

// ============= CONTAINER COMMANDS =============

#[derive(Subcommand)]
enum ContainerCommands {
    /// List your containers
    #[command(alias = "ls")]
    List {
        /// Only containers with this label (KEY=VALUE, repeatable)
        #[arg(long = "label", short = 'l', value_name = "KEY=VALUE")]
        label: Vec<String>,
    },
    /// Deploy a new container
    Deploy(Box<DeployArgs>),
    /// Start a stopped container
    Start {
        /// Container name, alias, or omit for interactive selection
        container: Option<String>,
        /// Return only when the container is running (exec works)
        #[arg(long)]
        wait: bool,
    },
    /// Stop a running container
    Stop {
        /// Container name, alias, or omit for interactive selection
        container: Option<String>,
    },
    /// Restart a container
    Restart {
        /// Container name, alias, or omit for interactive selection
        container: Option<String>,
    },
    /// Remove a container
    #[command(alias = "rm")]
    Remove {
        /// Container name, alias, or omit for interactive selection
        container: Option<String>,
        /// Kill immediately instead of a graceful stop first
        #[arg(short, long)]
        force: bool,
    },
    /// Get container logs
    Logs {
        /// Container name, alias, or omit for interactive selection
        container: Option<String>,
        /// Number of lines to show
        #[arg(short = 'n', long, default_value = "100")]
        lines: usize,
        /// Follow log output (not yet implemented)
        #[arg(short, long)]
        follow: bool,
    },
    /// Show container details
    Inspect {
        /// Container name, alias, or omit for interactive selection
        container: Option<String>,
    },
    /// Watts and CO₂ for a container (its share of the node's measured power)
    Energy {
        /// Container name, alias, or omit for interactive selection
        container: Option<String>,
        /// Window: 1h, 24h, 7d, 30d
        #[arg(long, default_value = "24h")]
        since: String,
    },
}

#[derive(Args, Clone)]
struct DeployArgs {
    /// Container image (e.g., nginx:alpine, ghcr.io/user/app:latest)
    /// Not required when using --from
    #[arg(default_value = "")]
    image: String,

    /// Deploy from a .nk spec file (overrides all other flags unless explicitly set)
    /// Accepts a path or a deployment name (e.g., my-campfire or ./my-campfire.nk)
    #[arg(long, value_name = "SPEC")]
    from: Option<String>,

    /// Port(s) to expose (can specify multiple: -p 80 -p 443)
    #[arg(short, long, value_name = "PORT")]
    port: Vec<u16>,

    /// Environment variables (KEY=VALUE, can specify multiple)
    #[arg(short, long, value_name = "KEY=VALUE")]
    env: Vec<String>,

    /// Load environment from file
    #[arg(long, value_name = "FILE")]
    env_file: Option<String>,

    /// CPU limit (e.g., 0.5, 1, 2)
    #[arg(long, default_value = "0.5")]
    cpu: f32,

    /// Memory limit (e.g., 256m, 512m, 1g)
    #[arg(long, default_value = "512m")]
    memory: String,

    /// Enable persistent storage (REQUIRES --volume-path)
    #[arg(long)]
    persistence: bool,

    /// Path inside container to mount persistent storage (e.g., /data, /rails/storage, /var/lib/mysql)
    /// REQUIRED when --persistence is set
    #[arg(long, value_name = "PATH")]
    volume_path: Option<String>,

    /// Volume size allocation (e.g., 1g, 512m, 5g) — default 1g
    #[arg(long, default_value = "1g")]
    volume_size: String,

    /// Allocate global IPv6 address
    #[arg(long)]
    ipv6: bool,

    /// Target garage (e.g., ry, aarhus)
    #[arg(long)]
    garage: Option<String>,

    /// Hardware preference (optiplex, raspi, mac-mini)
    #[arg(long)]
    hardware: Option<String>,

    /// Container name (auto-generated if not specified)
    #[arg(long)]
    name: Option<String>,

    /// Set a short alias for this container (skips interactive alias prompt)
    #[arg(long, short = 'a')]
    alias: Option<String>,

    /// Command to run (overrides image default)
    #[arg(long, value_name = "CMD")]
    command: Option<String>,

    /// Restart policy: always, unless-stopped, on-failure, never
    #[arg(long, value_name = "POLICY")]
    restart: Option<String>,

    /// Stop after this long with no exec/shell activity (e.g. 15m; 0 = never)
    #[arg(long, value_name = "DURATION")]
    auto_stop: Option<String>,

    /// Remove this long after the container stopped (e.g. 1h; 0 = never)
    #[arg(long, value_name = "DURATION")]
    auto_delete: Option<String>,

    /// Stop this long after creation, whatever happens (e.g. 24h; 0 = never)
    #[arg(long, value_name = "DURATION")]
    max_lifetime: Option<String>,

    /// Label KEY=VALUE (repeatable); filter with `nordkraft list --label`
    #[arg(long = "label", short = 'l', value_name = "KEY=VALUE")]
    label: Vec<String>,

    /// Restore this backup file (from `nordkraft backup`) into the new container's volume
    #[arg(long, value_name = "FILE")]
    volume_from: Option<String>,
}

// ============= INGRESS COMMANDS =============

#[derive(Subcommand)]
enum IngressCommands {
    /// Enable HTTPS ingress for a container
    Enable {
        /// Container name or ID
        container: String,
        /// Subdomain (e.g., 'myapp' → myapp.my.cloud)
        #[arg(short, long)]
        subdomain: String,
        /// Target port in container (default: 80)
        #[arg(short, long, default_value = "80")]
        port: u16,
        /// Mode: http, https, tcp (default: https)
        #[arg(short, long, default_value = "https")]
        mode: String,
    },
    /// Disable ingress for a container
    Disable {
        /// Container name or ID
        container: String,
    },
    /// Show ingress status for a container
    Status {
        /// Container name or ID
        container: String,
    },
    /// List all your ingress routes
    #[command(alias = "ls")]
    List,
}

// ============= DOMAIN COMMANDS =============

#[derive(Subcommand)]
enum DomainCommands {
    /// Register your own domain for a container (returns DNS instructions)
    Add {
        /// Your domain or hostname (e.g. customer1.net or www.customer1.net)
        domain: String,
        /// Container name, ID, or alias
        #[arg(short, long)]
        container: String,
        /// Container port to forward decrypted HTTPS traffic to (default: 80)
        #[arg(short, long, default_value = "80")]
        port: u16,
    },
    /// Show verification/TLS/routing progress for a domain
    Status {
        /// The domain to inspect
        domain: String,
    },
    /// Ask the platform to re-check DNS records right away
    Verify {
        /// The domain to verify
        domain: String,
    },
    /// List all your custom domains
    #[command(alias = "ls")]
    List,
    /// Remove a custom domain (tears down routing + certificate)
    Remove {
        /// The domain to remove
        domain: String,
        /// Skip confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },
}

// ============= PEER COMMANDS =============

#[derive(Subcommand)]
enum PeerCommands {
    /// Create a peer: generates a keypair here, uploads only the public key,
    /// writes a WireGuard config you hand to the device
    ///
    ///   nordkraft peer add anna-laptop --all
    ///   nordkraft peer add ci --container web --ports 80,443 --container db
    ///   nordkraft peer add phone --all --qr
    ///   nordkraft peer add anna --all --pubkey 'K7f…='   (she made her own key)
    ///   nordkraft peer add gpu-box --all --callable --callable-ports 11434
    ///       (your containers may call the peer on tcp 11434, e.g. an inference API)
    Add {
        /// Peer name: a-z, 0-9 and '-', e.g. anna-laptop
        name: String,
        /// Reach every container you own (now and later)
        #[arg(long)]
        all: bool,
        /// Reach only these containers (repeatable; name, ID or alias)
        #[arg(short, long = "container")]
        containers: Vec<String>,
        /// Restrict the given containers to these TCP ports, e.g. 80,443
        #[arg(long)]
        ports: Option<String>,
        /// Let your containers open connections TO this peer (default: replies only).
        /// Needs --callable-ports: a callable peer is never open on every port.
        #[arg(long, requires = "callable_ports")]
        callable: bool,
        /// With --callable: the TCP ports your containers may call on the peer, e.g. 11434
        #[arg(long, requires = "callable")]
        callable_ports: Option<String>,
        /// Use this public key instead of generating a keypair here
        #[arg(long)]
        pubkey: Option<String>,
        /// Where to write the WireGuard config (default ~/.nordkraft/peers/<name>.conf)
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// Also print the config as a QR code to scan with the WireGuard app
        #[arg(long)]
        qr: bool,
    },
    /// Let your containers call this peer (or stop that again)
    ///
    ///   nordkraft peer callable gpu-box --ports 11434
    ///   nordkraft peer callable gpu-box --off
    Callable {
        /// Peer name
        name: String,
        /// The TCP ports your containers may call on the peer, e.g. 11434 (required unless --off)
        #[arg(long, conflicts_with = "off", required_unless_present = "off")]
        ports: Option<String>,
        /// Back to the default: containers may only reply, never call
        #[arg(long)]
        off: bool,
    },
    /// List your peers with address, scope and last handshake
    #[command(alias = "ls")]
    List,
    /// Show one peer: scope, targets with their current IPs, connection state
    Show {
        /// Peer name
        name: String,
    },
    /// Grant access to more containers
    Allow {
        /// Peer name
        name: String,
        /// Container(s) to add (repeatable; name, ID or alias)
        #[arg(short, long = "container", required = true)]
        containers: Vec<String>,
        /// Restrict the added containers to these TCP ports, e.g. 80,443
        #[arg(long)]
        ports: Option<String>,
    },
    /// Take access to containers away
    Revoke {
        /// Peer name
        name: String,
        /// Container(s) to remove (repeatable)
        #[arg(short, long = "container", required = true)]
        containers: Vec<String>,
    },
    /// Replace the scope: every container, or exactly these
    Set {
        /// Peer name
        name: String,
        /// Reach every container you own
        #[arg(long, conflicts_with = "only")]
        all: bool,
        /// Reach only these containers, comma-separated
        #[arg(long, value_delimiter = ',')]
        only: Vec<String>,
    },
    /// Cut the peer off without deleting it
    Disable {
        /// Peer name
        name: String,
    },
    /// Re-enable a disabled peer
    Enable {
        /// Peer name
        name: String,
    },
    /// New keypair, same address (writes a new config)
    Rotate {
        /// Peer name
        name: String,
        /// Use this public key instead of generating a keypair here
        #[arg(long)]
        pubkey: Option<String>,
        /// Where to write the WireGuard config (default ~/.nordkraft/peers/<name>.conf)
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// Also print the config as a QR code to scan with the WireGuard app
        #[arg(long)]
        qr: bool,
    },
    /// Delete a peer
    #[command(alias = "rm")]
    Remove {
        /// Peer name
        name: String,
        /// Skip confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },
}

// ============= IPV6 COMMANDS =============

#[derive(Subcommand)]
enum Ipv6Commands {
    /// Open firewall for IPv6 access from internet
    Open {
        /// Container name or ID
        container: String,
    },
    /// Close firewall (block internet access)
    Close {
        /// Container name or ID
        container: String,
    },
    /// Show IPv6 status for a container
    Status {
        /// Container name or ID
        container: String,
    },
    /// List all IPv6 allocations
    #[command(alias = "ls")]
    List,
    /// Update exposed ports for IPv6
    Ports {
        /// Container name or ID
        container: String,
        /// Ports to expose (e.g., 80 443 8080)
        #[arg(required = true)]
        ports: Vec<u16>,
    },
}

// ============= NETWORK COMMANDS =============

#[derive(Subcommand)]
enum NetworkCommands {
    /// Show your network allocation info
    Info,
}

// ============= ALIAS COMMANDS =============

#[derive(Subcommand)]
enum AliasCommands {
    /// Set an alias for a container
    Set {
        /// Short alias name (e.g., 'myapp')
        alias: String,
        /// Full container name (e.g., 'app-fab3a39f-...')
        container: String,
    },
    /// Remove an alias
    #[command(alias = "rm")]
    Remove {
        /// Alias to remove
        alias: String,
    },
    /// List all aliases
    #[command(alias = "ls")]
    List,
}

// ============= REGISTRY COMMANDS =============

#[derive(Subcommand)]
enum RegistryCommands {
    /// Initialize your private container registry
    Init,
    /// Show registry status and stored images
    Status,
    /// List images in the registry
    #[command(alias = "ls")]
    List,
    /// Push a local image to the registry
    Push {
        /// Image to push (e.g., myapp:v1)
        image: String,
    },
    /// Destroy registry and all stored images
    Destroy {
        /// Skip confirmation
        #[arg(long)]
        force: bool,
    },
}

// ============= DEPLOYMENT SPEC (.nk TOML) =============

#[derive(Subcommand)]
enum ConfigCommands {
    /// Set the API endpoint, e.g. http://localhost:8001 (no WireGuard needed)
    SetApi {
        /// Base URL of a Nordkraft API ("/api" is appended if missing)
        url: String,
    },
    /// Forget the API endpoint override (back to WireGuard / connection.json)
    ResetApi,
    /// Show the effective API endpoint and where it comes from
    Show,
}

/// True when prompts must be skipped: --yes, --json, or no terminal on stdin.
static NON_INTERACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn non_interactive() -> bool {
    NON_INTERACTIVE.load(std::sync::atomic::Ordering::Relaxed)
}

// ============= API TYPES =============

async fn handle_config(
    command: ConfigCommands,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        ConfigCommands::SetApi { url } => {
            let url = normalise_api_url(&url);
            let dir = get_config_dir();
            fs::create_dir_all(&dir)?;
            fs::write(api_url_override_path(), format!("{}\n", url))?;

            // Quick reachability check — not fatal, just informative
            let reachable = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(3))
                .build()?
                .get(format!("{}/status", url))
                .send()
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false);

            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"api_url": url, "reachable": reachable})
                );
            } else {
                println!("{} API endpoint set to {}", "✓".green(), url.cyan());
                if reachable {
                    println!("  {} server responded on {}/status", "✓".green(), url);
                } else {
                    println!(
                        "  {} no response from {}/status yet (is the server running?)",
                        "!".yellow(),
                        url
                    );
                }
                println!("  Remove with: {}", "nordkraft config reset-api".dimmed());
            }
            Ok(())
        }
        ConfigCommands::ResetApi => {
            let path = api_url_override_path();
            if path.exists() {
                fs::remove_file(&path)?;
            }
            let (url, source) = resolve_api_base_url();
            if json_output {
                println!("{}", serde_json::json!({"api_url": url, "source": source}));
            } else {
                println!("{} API endpoint override removed", "✓".green());
                println!("  Now using {} ({})", url.cyan(), source.dimmed());
            }
            Ok(())
        }
        ConfigCommands::Show => {
            let (url, source) = resolve_api_base_url();
            if json_output {
                println!("{}", serde_json::json!({"api_url": url, "source": source}));
            } else {
                println!("API endpoint: {}", url.cyan());
                println!("Source:       {}", source.dimmed());
            }
            Ok(())
        }
    }
}

// ============= MAIN =============

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let json_output = cli.json;
    {
        use std::io::IsTerminal;
        let quiet = cli.yes || cli.json || !std::io::stdin().is_terminal();
        NON_INTERACTIVE.store(quiet, std::sync::atomic::Ordering::Relaxed);
    }

    let result = match cli.command {
        Commands::Auth { command } => handle_auth(command, json_output).await,
        Commands::Account { command } => handle_account(command, json_output).await,
        Commands::Container { command } => handle_container(command, json_output).await,
        Commands::Ingress { command } => handle_ingress(command, json_output).await,
        Commands::Domain { command } => handle_domain(command, json_output).await,
        Commands::Ipv6 { command } => handle_ipv6(command, json_output).await,
        Commands::Peer { command } => handle_peer(command, json_output).await,
        Commands::Network { command } => handle_network(command, json_output).await,
        Commands::Nodes => handle_nodes(json_output).await,
        Commands::Status => handle_status(json_output).await,
        Commands::Help => {
            show_help();
            Ok(())
        }
        Commands::Alias { command } => handle_alias(command, json_output).await,
        Commands::Update { check } => handle_update(check, json_output).await,
        Commands::Config { command } => handle_config(command, json_output).await,
        Commands::Registry { command } => handle_registry(command, json_output).await,
        // UI console
        Commands::Ui => crate::tui::run_tui().await,

        // WireGuard management
        Commands::Setup { token } => handle_setup(token, json_output).await,
        Commands::Connect => handle_connect(json_output).await,
        Commands::Disconnect => handle_disconnect(json_output).await,
        Commands::Reset { force } => handle_reset(force, json_output).await,

        // Declarative deployments
        Commands::Diff { container } => handle_diff_interactive(container, json_output).await,
        Commands::Upgrade { container, yes } => {
            handle_upgrade_interactive(container, yes, json_output).await
        }
        Commands::Init {
            container,
            from_server,
        } => handle_init_interactive(container, from_server, json_output).await,
        Commands::Edit { container } => handle_edit_interactive(container, json_output).await,
        Commands::Specs => handle_specs_list(json_output).await,
        Commands::SpecSet {
            container,
            key,
            value,
            apply,
        } => handle_spec_set(container, key, value, apply, json_output).await,
        Commands::SpecUnset { container, key } => {
            handle_spec_unset(container, key, json_output).await
        }
        Commands::SpecDelete { container, yes } => {
            handle_spec_delete(container, yes, json_output).await
        }
        Commands::Usage => handle_usage(json_output).await,
        Commands::Energy { container, since } => {
            handle_energy(container, &since, json_output).await
        }
        Commands::Events { container, limit } => handle_events(container, limit, json_output).await,

        // Shortcuts with interactive selection
        Commands::Deploy(args) => handle_deploy(*args, json_output).await,
        Commands::Push { image } => handle_registry_push(&image, json_output).await,
        Commands::List { label } => handle_container_list(&label, json_output).await,
        Commands::Backup { container, output } => {
            volume::handle_backup(container, output, json_output).await
        }
        Commands::Restore {
            container,
            file,
            yes,
        } => volume::handle_restore(container, file, yes, json_output).await,
        Commands::Wait { container, timeout } => handle_wait(container, timeout, json_output).await,
        Commands::Logs { container, lines } => {
            handle_logs_interactive(container, lines, json_output).await
        }
        Commands::Stop { container } => handle_stop_interactive(container, json_output).await,
        Commands::Start { container, wait } => {
            handle_start_interactive(container, wait, json_output).await
        }
        Commands::Shell { container, cmd } => handle_shell(container, cmd, json_output).await,
        Commands::Exec {
            container,
            cwd,
            env,
            user,
            timeout,
            stdin,
            cmd,
        } => handle_exec(container, cmd, cwd, env, user, timeout, stdin, json_output).await,
        Commands::Remove { container, force } => {
            handle_remove_interactive(container, force, json_output).await
        }
    };

    if let Err(e) = result {
        if json_output {
            println!("{}", serde_json::json!({"error": e.to_string()}));
        } else {
            eprintln!("{} {}", "Error:".red().bold(), e);
        }
        std::process::exit(1);
    }
}

// ============= AUTH HANDLERS =============

async fn handle_auth(
    command: AuthCommands,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = create_client()?;

    match command {
        AuthCommands::Login | AuthCommands::Whoami => {
            if !json_output {
                println!("{}", "🔐 Verifying Secure connection...".cyan());
            }

            let url = format!("{}/auth/verify", *API_BASE_URL);
            let response = client.get(&url).send().await.map_err(|e| {
                if e.is_timeout() {
                    "Timeout - check WireGuard connection".to_string()
                } else if e.is_connect() {
                    "Cannot connect to API. Is WireGuard connected?".to_string()
                } else {
                    format!("Network error: {}", e)
                }
            })?;

            if !response.status().is_success() {
                return Err(format!("API error: HTTP {}", response.status()).into());
            }

            let auth: AuthResponse = response.json().await?;

            if json_output {
                println!("{}", serde_json::to_string_pretty(&auth)?);
                return Ok(());
            }

            if auth.authenticated {
                if let Some(user) = auth.user {
                    println!(
                        "{}",
                        "✅ Connected to Nordkraft Garage Cloud!".green().bold()
                    );
                    println!();
                    println!("{}", "👤 User Info:".yellow().bold());
                    println!("   {} {}", "Name:".cyan(), user.full_name);
                    println!("   {} {}", "Email:".cyan(), user.email);
                    println!("   {} {}", "Plan:".cyan(), user.plan_id);
                    println!("   {} {}/32", "VPN IP:".cyan(), user.wireguard_ip);
                    if let Some(garage) = user.primary_garage_id {
                        println!("   {} {}", "Garage:".cyan(), garage);
                    }
                    if let Some(slot) = user.user_slot {
                        println!("   {} #{}", "Slot:".cyan(), slot);
                    }
                    println!(
                        "   {} {}",
                        "Status:".cyan(),
                        if user.account_status == "active" {
                            user.account_status.green()
                        } else {
                            user.account_status.yellow()
                        }
                    );
                    println!();
                    println!("{}", "🎯 Quick commands:".yellow().bold());
                    println!("   nordkraft deploy nginx:alpine     Deploy a container");
                    println!("   nordkraft list                    List containers");
                    println!("   nordkraft logs <name>             View logs");
                }
            } else {
                println!("{}", "✗ Authentication failed".red().bold());
                return Err("Not authenticated".into());
            }
        }
        AuthCommands::Status => {
            let url = format!("{}/auth/verify", *API_BASE_URL);
            let response = client.get(&url).send().await;

            match response {
                Ok(resp) if resp.status().is_success() => {
                    let auth: AuthResponse = resp.json().await?;
                    if json_output {
                        println!("{}", serde_json::json!({"connected": auth.authenticated}));
                    } else if auth.authenticated {
                        println!("{} Connected", "✔".green().bold());
                    } else {
                        println!("{} Not authenticated", "✗".red().bold());
                    }
                }
                _ => {
                    if json_output {
                        println!("{}", serde_json::json!({"connected": false}));
                    } else {
                        println!("{} Not connected", "✗".red().bold());
                    }
                }
            }
        }
    }
    Ok(())
}

// ============= CONTAINER HANDLERS =============

async fn handle_container(
    command: ContainerCommands,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        ContainerCommands::List { label } => handle_container_list(&label, json_output).await,
        ContainerCommands::Deploy(args) => handle_deploy(*args, json_output).await,
        ContainerCommands::Start { container, wait } => {
            let container = get_container_or_select(container, json_output).await?;
            handle_container_start(&container, wait, json_output).await
        }
        ContainerCommands::Stop { container } => {
            let container = get_container_or_select(container, json_output).await?;
            handle_container_stop(&container, json_output).await
        }
        ContainerCommands::Restart { container } => {
            let container = get_container_or_select(container, json_output).await?;
            handle_container_stop(&container, json_output).await?;
            handle_container_start(&container, false, json_output).await
        }
        ContainerCommands::Remove { container, force } => {
            let container = get_container_or_select(container, json_output).await?;
            handle_container_remove_opts(&container, force, json_output).await
        }
        ContainerCommands::Logs {
            container,
            lines,
            follow: _,
        } => {
            let container = get_container_or_select(container, json_output).await?;
            handle_container_logs(&container, lines, json_output).await
        }
        ContainerCommands::Inspect { container } => {
            let container = get_container_or_select(container, json_output).await?;
            handle_container_inspect(&container, json_output).await
        }
        ContainerCommands::Energy { container, since } => {
            let container = get_container_or_select(container, json_output).await?;
            handle_energy(Some(container), &since, json_output).await
        }
    }
}

async fn handle_shell(
    container: Option<String>,
    cmd: Vec<String>,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let name = get_container_or_select(container, json_output).await?;
    let code = stream::shell(&name, &cmd).await?;
    std::process::exit(code);
}

#[allow(clippy::too_many_arguments)]
async fn handle_exec(
    container: String,
    cmd: Vec<String>,
    cwd: Option<String>,
    env: Vec<String>,
    user: Option<String>,
    timeout: u64,
    stdin: bool,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let name = resolve_alias(&container);
    let stdin_data = if stdin {
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut std::io::stdin(), &mut buf)?;
        Some(buf)
    } else {
        None
    };
    let code = stream::exec(
        &name,
        &cmd,
        cwd.as_deref(),
        &env,
        user.as_deref(),
        timeout,
        stdin_data,
        json_output,
    )
    .await?;
    std::process::exit(code);
}

async fn handle_wait(
    container: String,
    timeout: u64,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let name = resolve_alias(&container);
    let code = stream::wait(&name, timeout, json_output).await?;
    std::process::exit(code);
}

// Interactive wrapper functions for shortcuts
async fn handle_logs_interactive(
    container: Option<String>,
    lines: usize,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let container = get_container_or_select(container, json_output).await?;
    handle_container_logs(&container, lines, json_output).await
}

async fn handle_stop_interactive(
    container: Option<String>,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let container = get_container_or_select(container, json_output).await?;
    handle_container_stop(&container, json_output).await
}

async fn handle_start_interactive(
    container: Option<String>,
    wait: bool,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let container = get_container_or_select(container, json_output).await?;
    handle_container_start(&container, wait, json_output).await
}

async fn handle_remove_interactive(
    container: Option<String>,
    force: bool,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let container = get_container_or_select(container, json_output).await?;
    handle_container_remove_opts(&container, force, json_output).await
}

// ============= DECLARATIVE DEPLOYMENT HANDLERS =============

// ============= PLAN USAGE =============

async fn handle_usage(json_output: bool) -> Result<(), Box<dyn std::error::Error>> {
    let client = create_client()?;
    let url = format!("{}/usage", *API_BASE_URL);
    let response = client.get(&url).send().await?;

    if !response.status().is_success() {
        let error: ApiError = response.json().await?;
        return Err(format!("Failed to fetch usage: {}", error.error).into());
    }

    let data: serde_json::Value = response.json().await?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&data)?);
        return Ok(());
    }

    let plan_name = data["plan"]["name"].as_str().unwrap_or("Unknown");
    let cpu_used = data["usage"]["cpu"].as_f64().unwrap_or(0.0);
    let cpu_max = data["plan"]["limits"]["cpu"].as_f64().unwrap_or(1.0);
    let mem_used = data["usage"]["memory_mb"].as_i64().unwrap_or(0);
    let mem_max = data["plan"]["limits"]["memory_mb"].as_i64().unwrap_or(512);
    let containers = data["usage"]["containers"].as_i64().unwrap_or(0);
    let cpu_ratio = data["ratios"]["cpu"].as_f64().unwrap_or(0.0);
    let mem_ratio = data["ratios"]["memory"].as_f64().unwrap_or(0.0);

    println!(
        "{}",
        format!("📊 Plan: {}  (allocated resources)", plan_name)
            .cyan()
            .bold()
    );
    println!();

    // CPU bar
    let cpu_bar = render_bar(cpu_ratio, 30);
    let cpu_color = if cpu_ratio < 0.6 {
        "green"
    } else if cpu_ratio < 0.85 {
        "yellow"
    } else {
        "red"
    };
    print!("   CPU      ");
    print_colored_bar(&cpu_bar, cpu_color);
    println!("  {:.1}/{:.1} vCPU", cpu_used, cpu_max);

    // Memory bar
    let mem_bar = render_bar(mem_ratio, 30);
    let mem_color = if mem_ratio < 0.6 {
        "green"
    } else if mem_ratio < 0.85 {
        "yellow"
    } else {
        "red"
    };
    print!("   Memory   ");
    print_colored_bar(&mem_bar, mem_color);
    println!("  {}MB/{}MB", mem_used, mem_max);

    // Disk bar
    let disk_used = data["usage"]["disk_mb"].as_i64().unwrap_or(0);
    let disk_max = data["plan"]["limits"]["storage_mb"]
        .as_i64()
        .unwrap_or(102400);
    let disk_ratio = data["ratios"]["disk"].as_f64().unwrap_or(0.0);
    let disk_bar = render_bar(disk_ratio, 30);
    let disk_color = if disk_ratio < 0.6 {
        "green"
    } else if disk_ratio < 0.85 {
        "yellow"
    } else {
        "red"
    };
    print!("   Disk     ");
    print_colored_bar(&disk_bar, disk_color);
    println!(
        "  {:.1}/{:.0}GB",
        disk_used as f64 / 1024.0,
        disk_max as f64 / 1024.0
    );

    println!();
    println!("   {} {} active", "Containers:".dimmed(), containers);

    Ok(())
}

// ============= ENERGY =============

fn fmt_wh(wh: f64) -> String {
    if wh >= 1000.0 {
        format!("{:.2} kWh", wh / 1000.0)
    } else {
        format!("{wh:.0} Wh")
    }
}

fn fmt_co2(g: Option<f64>) -> String {
    match g {
        Some(g) if g >= 1000.0 => format!("{:.2} kg CO₂", g / 1000.0),
        Some(g) => format!("{g:.0} g CO₂"),
        None => "CO₂ n/a".to_string(),
    }
}

fn fmt_watts(w: Option<f64>) -> String {
    match w {
        Some(w) => format!("{w:.1} W"),
        None => "no reading".to_string(),
    }
}

/// `nordkraft energy [name] [--since 24h]`: the account overview, or one
/// container with its hourly series.
async fn handle_energy(
    container: Option<String>,
    since: &str,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = create_client()?;
    let container = container.map(|c| resolve_alias(&c));
    let url = match &container {
        Some(name) => format!(
            "{}/containers/{}/energy?since={}",
            *API_BASE_URL, name, since
        ),
        None => format!("{}/energy?since={}", *API_BASE_URL, since),
    };
    let response = client.get(&url).send().await?;
    if !response.status().is_success() {
        let error: ApiError = response.json().await?;
        return Err(format!("Failed to fetch energy: {}", error.error).into());
    }
    let data: serde_json::Value = response.json().await?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&data)?);
        return Ok(());
    }
    if let Some(err) = data["error"].as_str() {
        println!("{}", err.yellow());
        if let Some(hint) = data["hint"].as_str() {
            println!("   {}", hint.dimmed());
        }
        return Ok(());
    }

    let grid = &data["grid"];
    let grid_line = match grid["g_per_kwh"].as_f64() {
        Some(g) => {
            let tag = if grid["forecast"].as_bool().unwrap_or(false) {
                " (forecast)"
            } else {
                ""
            };
            format!(
                "{:.0} g CO₂/kWh in {}{}",
                g,
                grid["price_area"].as_str().unwrap_or("DK1"),
                tag
            )
        }
        None => "grid intensity unavailable".to_string(),
    };

    let total = &data["total"];
    let now = &data["now"];
    match &container {
        Some(name) => {
            let c = &data["container"];
            println!(
                "{}",
                format!("⚡ {} on {}", name, c["node_id"].as_str().unwrap_or("?"))
                    .cyan()
                    .bold()
            );
            println!();
            println!(
                "   {} {}",
                "Now:".dimmed(),
                fmt_watts(now["watts"].as_f64()).yellow()
            );
            println!(
                "   {} {}  ·  {}",
                format!("Last {since}:").dimmed(),
                fmt_wh(total["wh"].as_f64().unwrap_or(0.0)),
                fmt_co2(total["co2_g"].as_f64())
            );
            println!("   {} {}", "Grid now:".dimmed(), grid_line);
            if let Some(hours) = data["hourly"].as_array().filter(|h| !h.is_empty()) {
                println!();
                println!("   {}", "Per hour (UTC)".dimmed());
                let max_w = hours
                    .iter()
                    .filter_map(|h| h["watts_avg"].as_f64())
                    .fold(0.0_f64, f64::max)
                    .max(0.1);
                for h in hours
                    .iter()
                    .rev()
                    .take(24)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                {
                    let w = h["watts_avg"].as_f64().unwrap_or(0.0);
                    let hour = h["hour"].as_str().unwrap_or("");
                    let hh = hour.get(11..16).unwrap_or(hour);
                    let (filled, empty) = render_bar(w / max_w, 20);
                    println!(
                        "   {}  {}{}  {:>6.1} W  {:>8}  {}",
                        hh.dimmed(),
                        "█".repeat(filled).yellow(),
                        "░".repeat(empty).dimmed(),
                        w,
                        fmt_wh(h["wh"].as_f64().unwrap_or(0.0)),
                        fmt_co2(h["co2_g"].as_f64()).dimmed()
                    );
                }
            }
        }
        None => {
            println!("{}", "⚡ Energy".cyan().bold());
            println!();
            println!(
                "   {} {}",
                "Now:".dimmed(),
                fmt_watts(now["watts"].as_f64()).yellow()
            );
            println!(
                "   {} {}  ·  {}",
                format!("Last {since}:").dimmed(),
                fmt_wh(total["wh"].as_f64().unwrap_or(0.0)),
                fmt_co2(total["co2_g"].as_f64())
            );
            println!("   {} {}", "Grid now:".dimmed(), grid_line);
            if let Some(list) = data["containers"].as_array().filter(|l| !l.is_empty()) {
                println!();
                for c in list {
                    println!(
                        "   {:<24} {:>9}  {:>10}  {}",
                        c["name"].as_str().unwrap_or("?").white().bold(),
                        fmt_watts(c["watts_now"].as_f64()),
                        fmt_wh(c["wh"].as_f64().unwrap_or(0.0)),
                        fmt_co2(c["co2_g"].as_f64()).dimmed()
                    );
                }
            } else {
                println!();
                println!(
                    "   {}",
                    "No readings yet: numbers appear once the node's meter and agent both report."
                        .dimmed()
                );
            }
        }
    }
    let unpriced = total["unpriced_samples"].as_i64().unwrap_or(0);
    if unpriced > 0 {
        println!();
        println!(
            "   {}",
            format!("{unpriced} interval(s) had no grid value (Energinet unreachable); their CO₂ is left out.")
                .dimmed()
        );
    }
    if let Some(text) = data["method"]["text"].as_str() {
        println!();
        println!("   {}", text.dimmed());
    }
    Ok(())
}

// ============= EVENTS HANDLER =============

async fn handle_events(
    container: Option<String>,
    limit: usize,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = create_client()?;

    // Resolve alias if provided
    let container = container.map(|c| resolve_alias(&c));

    let mut url = format!("{}/events?limit={}", *API_BASE_URL, limit);
    if let Some(ref name) = container {
        url = format!("{}&container={}", url, name);
    }

    let response = client.get(&url).send().await?;

    if !response.status().is_success() {
        let error: ApiError = response.json().await?;
        return Err(format!("Failed to fetch events: {}", error.error).into());
    }

    let data: serde_json::Value = response.json().await?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&data)?);
        return Ok(());
    }

    let events = data["events"].as_array();

    if events.is_none() || events.unwrap().is_empty() {
        println!("{}", "No deploy events found.".dimmed());
        if container.is_some() {
            println!("   Try without filter: {}", "nordkraft events".cyan());
        }
        return Ok(());
    }

    let events = events.unwrap();

    if let Some(ref name) = container {
        println!(
            "{}",
            format!("📋 Deploy events for: {}", name).cyan().bold()
        );
    } else {
        println!("{}", "📋 Deploy events".cyan().bold());
    }
    println!();

    // Events come newest-first from API, reverse for chronological display
    for event in events.iter().rev() {
        let phase = event["phase"].as_str().unwrap_or("?");
        let message = event["message"].as_str().unwrap_or("");
        let success = event["success"].as_bool().unwrap_or(true);
        let timestamp = event["created_at"].as_str().unwrap_or("");
        let container_name = event["container_name"].as_str().unwrap_or("");

        // Format timestamp to just time if today
        let time_str = if timestamp.len() > 19 {
            &timestamp[11..19]
        } else {
            timestamp
        };

        let phase_icon = match phase {
            "network" => "🌐",
            "pulling" => "⬇️ ",
            "pulled" => "📦",
            "created" => "🔧",
            "starting" => "🚀",
            "running" => "✅",
            "upgrading" => "🔄",
            "failed" => "❌",
            _ => "  ",
        };

        let phase_colored = if success {
            match phase {
                "running" => phase.green().bold().to_string(),
                "pulling" | "upgrading" => phase.cyan().to_string(),
                _ => phase.white().to_string(),
            }
        } else {
            phase.red().bold().to_string()
        };

        // Show container name only in unfiltered view
        if container.is_none() {
            println!(
                "   {} {} {} {} {}",
                time_str.dimmed(),
                phase_icon,
                phase_colored,
                container_name.white().bold(),
                message.dimmed()
            );
        } else {
            println!(
                "   {} {} {} {}",
                time_str.dimmed(),
                phase_icon,
                phase_colored,
                message.dimmed()
            );
        }
    }

    println!();

    Ok(())
}

fn render_bar(ratio: f64, width: usize) -> (usize, usize) {
    let ratio = ratio.clamp(0.0, 1.0);
    let filled = (ratio * width as f64).round() as usize;
    let empty = width - filled;
    (filled, empty)
}

fn print_colored_bar((filled, empty): &(usize, usize), color: &str) {
    let filled_str = "█".repeat(*filled);
    let empty_str = "░".repeat(*empty);
    match color {
        "green" => print!("{}{}", filled_str.green(), empty_str.dimmed()),
        "yellow" => print!("{}{}", filled_str.yellow(), empty_str.dimmed()),
        "red" => print!("{}{}", filled_str.red(), empty_str.dimmed()),
        _ => print!("{}{}", filled_str, empty_str.dimmed()),
    }
}

// ============= INGRESS HANDLERS =============

async fn handle_ingress(
    command: IngressCommands,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = create_client()?;

    match command {
        IngressCommands::Enable {
            container,
            subdomain,
            port,
            mode,
        } => {
            let container = resolve_alias(&container);
            if !json_output {
                println!(
                    "{}",
                    format!("🌐 Enabling ingress for {}...", container).cyan()
                );
            }

            let request = IngressEnableRequest {
                subdomain: subdomain.clone(),
                mode,
                target_port: port,
            };

            let url = format!("{}/ingress/{}/enable", *API_BASE_URL, container);
            let response = client.post(&url).json(&request).send().await?;

            if !response.status().is_success() {
                let error: ApiError = response.json().await?;
                return Err(error.error.into());
            }

            let result: IngressEnableResponse = response.json().await?;

            if json_output {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("{}", "✅ Ingress enabled!".green().bold());
                if let Some(url) = result.url {
                    println!();
                    println!("   {} {}", "URL:".cyan(), url.white().bold());
                }
                if result.ssl.unwrap_or(false) {
                    println!("   {} ✓ Auto-TLS enabled", "SSL:".cyan());
                }
            }
        }
        IngressCommands::Disable { container } => {
            let container = resolve_alias(&container);
            if !json_output {
                println!(
                    "{}",
                    format!("🔒 Disabling ingress for {}...", container).cyan()
                );
            }

            let url = format!("{}/ingress/{}/disable", *API_BASE_URL, container);
            let response = client.delete(&url).send().await?;

            if !response.status().is_success() {
                let text = response.text().await?;
                if let Ok(error) = serde_json::from_str::<ApiError>(&text) {
                    return Err(error.error.into());
                }
                return Err(format!("Failed to disable ingress: {}", text).into());
            }

            if json_output {
                // Try to parse JSON, fallback to simple success message
                let text = response.text().await?;
                if let Ok(result) = serde_json::from_str::<ApiResponse>(&text) {
                    println!("{}", serde_json::to_string_pretty(&result)?);
                } else {
                    println!("{}", serde_json::json!({"status": "disabled"}));
                }
            } else {
                // Consume body but don't parse
                let _ = response.text().await;
                println!("{} Ingress disabled", "✅".green().bold());
            }
        }
        IngressCommands::Status { container } => {
            let container = resolve_alias(&container);
            let url = format!("{}/ingress/{}/status", *API_BASE_URL, container);
            let response = client.get(&url).send().await?;

            if !response.status().is_success() {
                let error: ApiError = response.json().await?;
                return Err(error.error.into());
            }

            let result: IngressStatusResponse = response.json().await?;

            if json_output {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("{}", format!("🌐 Ingress status for {}:", container).cyan());
                println!();
                println!(
                    "   {} {}",
                    "Enabled:".cyan(),
                    if result.enabled {
                        "Yes".green()
                    } else {
                        "No".yellow()
                    }
                );
                if let Some(url) = result.url {
                    println!("   {} {}", "URL:".cyan(), url);
                }
                if let Some(port) = result.target_port {
                    println!("   {} {}", "Target port:".cyan(), port);
                }
                if let Some(ssl) = result.ssl {
                    println!(
                        "   {} {}",
                        "SSL:".cyan(),
                        if ssl { "✓ Enabled" } else { "✗ Disabled" }
                    );
                }
            }
        }
        IngressCommands::List => {
            let url = format!("{}/ingress/list", *API_BASE_URL);
            let response = client.get(&url).send().await?;

            if !response.status().is_success() {
                let error: ApiError = response.json().await?;
                return Err(error.error.into());
            }

            let result: IngressListResponse = response.json().await?;

            if json_output {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else if result.routes.is_empty() {
                println!("{}", "No ingress routes configured.".yellow());
                println!("   Enable one: nordkraft ingress enable <container> --subdomain myapp");
            } else {
                println!(
                    "{}",
                    format!("🌐 {} ingress route(s):", result.routes.len())
                        .green()
                        .bold()
                );
                println!();
                for route in &result.routes {
                    let status = if route.is_active {
                        "active".green()
                    } else {
                        "inactive".yellow()
                    };
                    let mode = route.mode.as_deref().unwrap_or("https");
                    println!(
                        "   {} → {}",
                        route.subdomain.cyan().bold(),
                        route.url.white()
                    );
                    println!(
                        "     Container: {} | Port: {} | Mode: {} | Status: {}",
                        route.container_id.dimmed(),
                        route.target_port,
                        mode,
                        status
                    );
                    if let Some(ip) = &route.target_ip {
                        println!("     Target IP: {}", ip.dimmed());
                    }
                    println!();
                }
            }
        }
    }

    Ok(())
}

// ============= DOMAIN HANDLERS =============

// ============= PEER HANDLERS =============

const PEERS_DIR: &str = "peers";

fn parse_port_list(ports: &Option<String>) -> Result<Option<Vec<u16>>, Box<dyn std::error::Error>> {
    let Some(p) = ports else { return Ok(None) };
    let mut out = Vec::new();
    for part in p.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let n: u16 = part
            .parse()
            .map_err(|_| format!("invalid port {part:?} (expected e.g. --ports 80,443)"))?;
        if n == 0 {
            return Err("port 0 is not valid".into());
        }
        out.push(n);
    }
    Ok(if out.is_empty() { None } else { Some(out) })
}

fn peer_targets_json(containers: &[String], ports: &Option<Vec<u16>>) -> Vec<serde_json::Value> {
    containers
        .iter()
        .map(|c| serde_json::json!({ "container": resolve_alias(c), "ports": ports }))
        .collect()
}

/// Render the device's WireGuard config from the API's `client` block.
fn render_peer_config(peer: &serde_json::Value, private_key: Option<&str>) -> String {
    let client = &peer["client"];
    let s = |k: &str| {
        client
            .get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let allowed: Vec<String> = client["allowed_ips"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let server_key = client
        .get("server_public_key")
        .and_then(|v| v.as_str())
        .unwrap_or("<SERVER PUBLIC KEY: ask the operator>");
    let private_key =
        private_key.unwrap_or("<the private key that belongs to the public key you supplied>");
    let name = peer["name"].as_str().unwrap_or("peer");
    let scope = peer["scope"].as_str().unwrap_or("all");
    let mut reach = String::new();
    if scope == "all" {
        reach.push_str(&format!("#   every container in {}\n", allowed.join(", ")));
    } else if let Some(t) = peer["targets"].as_array() {
        for t in t {
            let c = t["container"].as_str().unwrap_or("?");
            let ip = t["ip"]
                .as_str()
                .unwrap_or("(no running container with that name)");
            let ports = t["ports"]
                .as_array()
                .map(|a| {
                    format!(
                        "  tcp {}",
                        a.iter()
                            .filter_map(|v| v.as_u64())
                            .map(|v| v.to_string())
                            .collect::<Vec<_>>()
                            .join(",")
                    )
                })
                .unwrap_or_default();
            reach.push_str(&format!("#   {c}  {ip}{ports}\n"));
        }
    }
    format!(
        "[Interface]\nPrivateKey = {private_key}\nAddress = {}\n\n[Peer]\nPublicKey = {server_key}\nEndpoint = {}\nAllowedIPs = {}\nPersistentKeepalive = 25\n\n# Nordkraft peer \"{name}\". Reachable from this device (enforced by Nordkraft, not by this file):\n{reach}# Callable by your containers: {callable}\n",
        s("address"),
        s("server_endpoint"),
        allowed.join(", "),
        callable = callable_label(peer),
    )
}

fn write_peer_config(
    name: &str,
    out: &Option<PathBuf>,
    config: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let path = match out {
        Some(p) => p.clone(),
        None => {
            let dir = get_config_dir().join(PEERS_DIR);
            std::fs::create_dir_all(&dir)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
            }
            dir.join(format!("{name}.conf"))
        }
    };
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)?;
        f.write_all(config.as_bytes())?;
    }
    #[cfg(not(unix))]
    std::fs::write(&path, config)?;
    Ok(path)
}

/// Draw the config as a QR code for the WireGuard app. Two modules per
/// character row (half blocks), and explicit black-on-white colours so the
/// code has the right polarity whatever the terminal theme is.
fn print_qr(config: &str) {
    let code = match qrcode::QrCode::new(config.as_bytes()) {
        Ok(c) => c,
        Err(e) => {
            println!(
                "   {}",
                format!("QR code not possible ({e}); import the file instead.").yellow()
            );
            return;
        }
    };
    let width = code.width();
    let modules = code.to_colors();
    let dark = |x: isize, y: isize| -> bool {
        if x < 0 || y < 0 || x >= width as isize || y >= width as isize {
            return false; // quiet zone
        }
        modules[y as usize * width + x as usize] == qrcode::Color::Dark
    };
    let quiet: isize = 2;
    let mut y = -quiet;
    while y < width as isize + quiet {
        let mut line = String::new();
        for x in -quiet..width as isize + quiet {
            line.push(match (dark(x, y), dark(x, y + 1)) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        println!("{}", line.black().on_white());
        y += 2;
    }
    println!();
    println!(
        "   {}",
        "Scan with the WireGuard app: Add a tunnel → Create from QR code".dimmed()
    );
}

fn handshake_label(peer: &serde_json::Value) -> String {
    match peer["live"]["latest_handshake"].as_str() {
        Some(h) if !h.is_empty() => h.to_string(),
        _ => "never".to_string(),
    }
}

fn print_peer(peer: &serde_json::Value) {
    let name = peer["name"].as_str().unwrap_or("?");
    let status = peer["status"].as_str().unwrap_or("?");
    println!("{}", format!("🔗 {name}").cyan().bold());
    println!();
    println!(
        "   {} {}",
        "Address:".cyan(),
        peer["vpn_ip"].as_str().unwrap_or("?")
    );
    println!(
        "   {} {}",
        "Status:".cyan(),
        if status == "active" {
            status.green()
        } else {
            status.yellow()
        }
    );
    println!("   {} {}", "Handshake:".cyan(), handshake_label(peer));
    if let Some(ep) = peer["live"]["endpoint"].as_str() {
        println!("   {} {}", "From:".cyan(), ep);
    }
    if let (Some(rx), Some(tx)) = (
        peer["live"]["transfer_rx"].as_str(),
        peer["live"]["transfer_tx"].as_str(),
    ) {
        println!("   {} {} received, {} sent", "Traffic:".cyan(), rx, tx);
    }
    println!(
        "   {} {}",
        "Key:".cyan(),
        peer["public_key"].as_str().unwrap_or("?").dimmed()
    );
    println!();
    if peer["scope"].as_str() == Some("all") {
        let allowed = peer["client"]["allowed_ips"][0].as_str().unwrap_or("?");
        println!("   {} every container ({})", "Reaches:".cyan(), allowed);
    } else {
        println!("   {}", "Reaches:".cyan());
        if let Some(t) = peer["targets"].as_array() {
            for t in t {
                let c = t["container"].as_str().unwrap_or("?");
                let ports = t["ports"]
                    .as_array()
                    .map(|a| {
                        format!(
                            " tcp {}",
                            a.iter()
                                .filter_map(|v| v.as_u64())
                                .map(|v| v.to_string())
                                .collect::<Vec<_>>()
                                .join(",")
                        )
                    })
                    .unwrap_or_default();
                match t["ip"].as_str() {
                    Some(ip) => println!("     {c:<24} {ip}{ports}"),
                    None => println!(
                        "     {c:<24} {}",
                        "no running container with that name (dangling)".yellow()
                    ),
                }
            }
        }
    }
    println!("   {} {}", "Callable:".cyan(), callable_label(peer));
    if let Some(errs) = peer["reconcile_errors"].as_array() {
        println!();
        for e in errs {
            println!("   {} {}", "⚠️".yellow(), e.as_str().unwrap_or(""));
        }
    }
}

/// "no (replies only)" / "yes, every port" / "yes, tcp 11434" — may the
/// owner's containers open connections to this peer?
fn callable_label(peer: &serde_json::Value) -> String {
    if peer["callable"].as_bool() != Some(true) {
        return "no — your containers can only answer this peer, not call it".to_string();
    }
    match peer["callable_ports"].as_array() {
        Some(a) if !a.is_empty() => format!(
            "yes — your containers may call it on tcp {}",
            a.iter()
                .filter_map(|v| v.as_u64())
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ),
        _ => "yes — your containers may call it on every port".to_string(),
    }
}

/// Create or rotate: keypair handling and config output shared by both.
async fn peer_issue_config(
    name: &str,
    pubkey: Option<String>,
    out: &Option<PathBuf>,
    qr: bool,
    json_output: bool,
    send: impl FnOnce(String) -> reqwest::RequestBuilder,
) -> Result<(), Box<dyn std::error::Error>> {
    let (private_key, public_key) = match pubkey {
        Some(k) => (None, k.trim().to_string()),
        None => {
            if !json_output {
                println!("{}", "  Generating WireGuard keypair locally...".dimmed());
            }
            let (private, public) = generate_wireguard_keypair().map_err(|e| {
                format!("{e}\n   Either install wireguard-tools here, or let the device create its own key and pass it with --pubkey.")
            })?;
            (Some(private), public)
        }
    };

    let response = send(public_key).send().await?;
    let result: serde_json::Value = response.json().await?;
    if let Some(error) = result.get("error").and_then(|v| v.as_str()) {
        return Err(error.to_string().into());
    }

    let config = render_peer_config(&result, private_key.as_deref());
    let path = write_peer_config(name, out, &config)?;

    if json_output {
        let mut v = result.clone();
        v["config_path"] = serde_json::json!(path.to_string_lossy());
        v["config"] = serde_json::json!(config);
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }

    println!("{}", "✅ Peer ready".green().bold());
    println!();
    print_peer(&result);
    println!();
    println!(
        "   {} {}",
        "Config written:".cyan(),
        path.display().to_string().white().bold()
    );
    if private_key.is_none() {
        println!(
            "   {}",
            "You supplied the public key, so the PrivateKey line is a placeholder: fill it in on the device.".yellow()
        );
    }
    if result["client"]["server_public_key"].is_null() {
        println!(
            "   {}",
            "This garage has no server public key on record; fill in PublicKey under [Peer] by hand.".yellow()
        );
    }
    println!();
    println!("{}", config.dimmed());
    if qr {
        println!();
        print_qr(&config);
    }
    println!(
        "   Import the file in the WireGuard app on the device, or: {}",
        format!("sudo wg-quick up {}", path.display())
            .white()
            .bold()
    );
    Ok(())
}

async fn handle_peer(
    command: PeerCommands,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = create_client()?;

    match command {
        PeerCommands::Add {
            name,
            all,
            containers,
            ports,
            callable,
            callable_ports,
            pubkey,
            out,
            qr,
        } => {
            if !all && containers.is_empty() {
                return Err(
                    "say what the peer may reach: --all, or one or more --container <name>".into(),
                );
            }
            if all && !containers.is_empty() {
                return Err("--all and --container exclude each other".into());
            }
            let ports = parse_port_list(&ports)?;
            let callable_ports = parse_port_list(&callable_ports)?;
            let targets = peer_targets_json(&containers, &ports);
            let scope = if all { "all" } else { "containers" };
            let url = format!("{}/peers", *API_BASE_URL);
            let name_for_body = name.clone();
            peer_issue_config(&name, pubkey, &out, qr, json_output, |public_key| {
                client.post(&url).json(&serde_json::json!({
                    "name": name_for_body,
                    "public_key": public_key,
                    "scope": scope,
                    "targets": targets,
                    "callable": callable,
                    "callable_ports": callable_ports,
                }))
            })
            .await
        }

        PeerCommands::Callable { name, ports, off } => {
            let body = if off {
                serde_json::json!({ "callable": false })
            } else {
                serde_json::json!({
                    "callable": true,
                    "callable_ports": parse_port_list(&ports)?.unwrap_or_default(),
                })
            };
            peer_patch(&client, &name, body, json_output).await
        }

        PeerCommands::List => {
            let url = format!("{}/peers", *API_BASE_URL);
            let result: serde_json::Value = client.get(&url).send().await?.json().await?;
            if let Some(error) = result.get("error").and_then(|v| v.as_str()) {
                return Err(error.to_string().into());
            }
            if json_output {
                println!("{}", serde_json::to_string_pretty(&result)?);
                return Ok(());
            }
            let peers = result["peers"].as_array().cloned().unwrap_or_default();
            if peers.is_empty() {
                println!(
                    "No peers yet. Create one with: {}",
                    "nordkraft peer add <name> --all".white().bold()
                );
                return Ok(());
            }
            println!(
                "{:<20} {:<16} {:<10} {:<9} {:<22} {}",
                "NAME".bold(),
                "ADDRESS".bold(),
                "SCOPE".bold(),
                "STATUS".bold(),
                "HANDSHAKE".bold(),
                "REACHES".bold()
            );
            for p in &peers {
                let reaches = if p["scope"].as_str() == Some("all") {
                    "every container".to_string()
                } else {
                    p["targets"]
                        .as_array()
                        .map(|t| {
                            t.iter()
                                .filter_map(|x| x["container"].as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default()
                };
                let reaches = if p["callable"].as_bool() == Some(true) {
                    format!("{reaches} · callable")
                } else {
                    reaches
                };
                let status = p["status"].as_str().unwrap_or("?");
                println!(
                    "{:<20} {:<16} {:<10} {:<9} {:<22} {}",
                    p["name"].as_str().unwrap_or("?"),
                    p["vpn_ip"].as_str().unwrap_or("?"),
                    p["scope"].as_str().unwrap_or("?"),
                    if status == "active" {
                        status.green()
                    } else {
                        status.yellow()
                    },
                    handshake_label(p),
                    reaches
                );
            }
            Ok(())
        }

        PeerCommands::Show { name } => {
            let url = format!("{}/peers/{}", *API_BASE_URL, name);
            let result: serde_json::Value = client.get(&url).send().await?.json().await?;
            if let Some(error) = result.get("error").and_then(|v| v.as_str()) {
                return Err(error.to_string().into());
            }
            if json_output {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                print_peer(&result);
            }
            Ok(())
        }

        PeerCommands::Allow {
            name,
            containers,
            ports,
        } => {
            let ports = parse_port_list(&ports)?;
            let body = serde_json::json!({ "add_targets": peer_targets_json(&containers, &ports) });
            peer_patch(&client, &name, body, json_output).await
        }

        PeerCommands::Revoke { name, containers } => {
            let names: Vec<String> = containers.iter().map(|c| resolve_alias(c)).collect();
            let body = serde_json::json!({ "remove_targets": names });
            peer_patch(&client, &name, body, json_output).await
        }

        PeerCommands::Set { name, all, only } => {
            if !all && only.is_empty() {
                return Err("use --all, or --only web,db".into());
            }
            let body = if all {
                serde_json::json!({ "scope": "all" })
            } else {
                serde_json::json!({ "targets": peer_targets_json(&only, &None) })
            };
            peer_patch(&client, &name, body, json_output).await
        }

        PeerCommands::Disable { name } => {
            peer_set_status(&client, &name, "disable", json_output).await
        }
        PeerCommands::Enable { name } => {
            peer_set_status(&client, &name, "enable", json_output).await
        }

        PeerCommands::Rotate {
            name,
            pubkey,
            out,
            qr,
        } => {
            let url = format!("{}/peers/{}/rotate", *API_BASE_URL, name);
            peer_issue_config(&name, pubkey, &out, qr, json_output, |public_key| {
                client
                    .post(&url)
                    .json(&serde_json::json!({ "public_key": public_key }))
            })
            .await
        }

        PeerCommands::Remove { name, yes } => {
            if !yes && !json_output && !non_interactive() {
                let confirmed = Confirm::with_theme(&ColorfulTheme::default())
                    .with_prompt(format!(
                        "Remove peer {name}? The device loses access immediately."
                    ))
                    .default(false)
                    .interact()?;
                if !confirmed {
                    println!("Cancelled.");
                    return Ok(());
                }
            }
            let url = format!("{}/peers/{}", *API_BASE_URL, name);
            let result: serde_json::Value = client.delete(&url).send().await?.json().await?;
            if let Some(error) = result.get("error").and_then(|v| v.as_str()) {
                return Err(error.to_string().into());
            }
            if json_output {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("{}", format!("🗑️ Peer {name} removed").green());
                let local = get_config_dir()
                    .join(PEERS_DIR)
                    .join(format!("{name}.conf"));
                if local.exists() {
                    let _ = std::fs::remove_file(&local);
                    println!(
                        "   {} {}",
                        "Deleted local config".dimmed(),
                        local.display().to_string().dimmed()
                    );
                }
            }
            Ok(())
        }
    }
}

async fn peer_patch(
    client: &Client,
    name: &str,
    body: serde_json::Value,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/peers/{}", *API_BASE_URL, name);
    let result: serde_json::Value = client.patch(&url).json(&body).send().await?.json().await?;
    if let Some(error) = result.get("error").and_then(|v| v.as_str()) {
        return Err(error.to_string().into());
    }
    if json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("{}", "✅ Updated".green().bold());
        println!();
        print_peer(&result);
    }
    Ok(())
}

async fn peer_set_status(
    client: &Client,
    name: &str,
    verb: &str,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/peers/{}/{}", *API_BASE_URL, name, verb);
    let result: serde_json::Value = client.post(&url).send().await?.json().await?;
    if let Some(error) = result.get("error").and_then(|v| v.as_str()) {
        return Err(error.to_string().into());
    }
    if json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("{}", format!("✅ Peer {name} {verb}d").green().bold());
        println!();
        print_peer(&result);
    }
    Ok(())
}

/// Pretty-print the DNS instructions block returned by the API.
fn print_dns_instructions(instructions: &serde_json::Value) {
    let get = |key: &str| {
        instructions
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };

    println!();
    println!("   {}", "Add these records at your DNS provider:".bold());
    println!();
    println!(
        "   {}  {}",
        "TXT".cyan().bold(),
        get("txt_record_name").white()
    );
    println!("        value: {}", get("txt_record_value").yellow());
    println!();
    println!(
        "   {}    {}",
        "A".cyan().bold(),
        get("a_record_name").white()
    );
    println!("        value: {}", get("a_record_value").yellow());
    if !get("cname_alternative").is_empty() {
        println!("        ({})", get("cname_alternative").dimmed());
    }
    println!();
    if !get("note").is_empty() {
        println!("   {}", get("note").dimmed());
    }
}

fn domain_status_color(status: &str) -> colored::ColoredString {
    match status {
        "active" => status.green().bold(),
        "degraded" | "error" => status.red().bold(),
        "disabled" => status.dimmed(),
        _ => status.yellow(),
    }
}

async fn handle_domain(
    command: DomainCommands,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = create_client()?;

    match command {
        DomainCommands::Add {
            domain,
            container,
            port,
        } => {
            let container = resolve_alias(&container);
            if !json_output {
                println!(
                    "{}",
                    format!("🌍 Registering {} for {}...", domain, container).cyan()
                );
            }

            let url = format!("{}/domains/add", *API_BASE_URL);
            let response = client
                .post(&url)
                .json(&serde_json::json!({
                    "domain": domain,
                    "container_id": container,
                    "target_port": port,
                }))
                .send()
                .await?;

            let result: serde_json::Value = response.json().await?;
            if let Some(error) = result.get("error").and_then(|v| v.as_str()) {
                return Err(error.to_string().into());
            }

            if json_output {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                let normalized = result
                    .get("domain")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&domain);
                println!("{}", "✅ Domain registered!".green().bold());
                if let Some(instructions) = result.get("dns_instructions") {
                    print_dns_instructions(instructions);
                }
                println!();
                println!(
                    "   Track progress: {}",
                    format!("nordkraft domain status {}", normalized)
                        .white()
                        .bold()
                );
            }
        }

        DomainCommands::Status { domain } => {
            let url = format!("{}/domains/{}/status", *API_BASE_URL, domain);
            let response = client.get(&url).send().await?;
            let result: serde_json::Value = response.json().await?;

            if let Some(error) = result.get("error").and_then(|v| v.as_str()) {
                return Err(error.to_string().into());
            }

            if json_output {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                let status = result.get("status").and_then(|v| v.as_str()).unwrap_or("?");
                println!("{}", format!("🌍 {}", domain).cyan().bold());
                println!();
                println!("   {} {}", "Status:".cyan(), domain_status_color(status));
                if let Some(explanation) = result.get("explanation").and_then(|v| v.as_str()) {
                    println!("   {}", explanation.dimmed());
                }
                if status == "active" {
                    if let Some(url) = result.get("url").and_then(|v| v.as_str()) {
                        println!();
                        println!("   {} {}", "URL:".cyan(), url.white().bold());
                    }
                    if let Some(expires) = result.get("cert_expires_at").and_then(|v| v.as_str()) {
                        println!("   {} valid until {} (auto-renews)", "TLS:".cyan(), expires);
                    }
                }
                if let Some(last_error) = result.get("last_error").and_then(|v| v.as_str()) {
                    println!();
                    println!("   {} {}", "Last check:".cyan(), last_error.yellow());
                }
                if status == "pending_dns" {
                    if let Some(instructions) = result.get("dns_instructions") {
                        print_dns_instructions(instructions);
                        println!();
                        println!(
                            "   Re-check now: {}",
                            format!("nordkraft domain verify {}", domain).white()
                        );
                    }
                }
            }
        }

        DomainCommands::Verify { domain } => {
            if !json_output {
                println!("{}", format!("🔍 Checking DNS for {}...", domain).cyan());
            }

            let url = format!("{}/domains/{}/verify", *API_BASE_URL, domain);
            let response = client.post(&url).send().await?;
            let result: serde_json::Value = response.json().await?;

            if let Some(error) = result.get("error").and_then(|v| v.as_str()) {
                return Err(error.to_string().into());
            }

            if json_output {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                let txt_ok = result
                    .get("txt_record_found")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let a_ok = result
                    .get("a_record_points_here")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let mark = |ok: bool| if ok { "✓".green() } else { "✗".red() };
                println!();
                println!("   {} TXT verification record", mark(txt_ok));
                println!("   {} A/CNAME points at the platform", mark(a_ok));
                if let Some(detail) = result.get("detail").and_then(|v| v.as_str()) {
                    println!();
                    println!("   {}", detail.dimmed());
                }
                println!();
                if txt_ok && a_ok {
                    println!(
                        "{}",
                        "✅ Records look good — activation continues automatically (TLS issuance takes a few minutes)."
                            .green()
                    );
                } else {
                    println!(
                        "{}",
                        "DNS changes can take a while to propagate — try again in a few minutes."
                            .yellow()
                    );
                }
            }
        }

        DomainCommands::List => {
            let url = format!("{}/domains/list", *API_BASE_URL);
            let response = client.get(&url).send().await?;
            let result: serde_json::Value = response.json().await?;

            if let Some(error) = result.get("error").and_then(|v| v.as_str()) {
                return Err(error.to_string().into());
            }

            if json_output {
                println!("{}", serde_json::to_string_pretty(&result)?);
                return Ok(());
            }

            let empty = vec![];
            let domains = result
                .get("domains")
                .and_then(|v| v.as_array())
                .unwrap_or(&empty);

            if domains.is_empty() {
                println!("{}", "No custom domains configured.".yellow());
                println!("   Add one: nordkraft domain add customer1.net --container <name>");
            } else {
                println!(
                    "{}",
                    format!("🌍 {} custom domain(s):", domains.len())
                        .green()
                        .bold()
                );
                println!();
                for d in domains {
                    let name = d.get("domain").and_then(|v| v.as_str()).unwrap_or("?");
                    let status = d.get("status").and_then(|v| v.as_str()).unwrap_or("?");
                    let container = d
                        .get("container_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    let port = d.get("target_port").and_then(|v| v.as_i64()).unwrap_or(80);
                    println!(
                        "   {} [{}]",
                        name.cyan().bold(),
                        domain_status_color(status)
                    );
                    println!("     Container: {} | Port: {}", container.dimmed(), port);
                    if let Some(last_error) = d.get("last_error").and_then(|v| v.as_str()) {
                        if status != "active" {
                            println!("     {}", last_error.yellow());
                        }
                    }
                    println!();
                }
            }
        }

        DomainCommands::Remove { domain, yes } => {
            if !yes && !json_output {
                let confirmed = Confirm::with_theme(&ColorfulTheme::default())
                    .with_prompt(format!(
                        "Remove {} ? Routing and its TLS certificate will be torn down.",
                        domain
                    ))
                    .default(false)
                    .interact()?;
                if !confirmed {
                    println!("Cancelled.");
                    return Ok(());
                }
            }

            if !json_output {
                println!("{}", format!("🗑️ Removing {}...", domain).cyan());
            }

            let url = format!("{}/domains/{}", *API_BASE_URL, domain);
            let response = client.delete(&url).send().await?;
            let result: serde_json::Value = response.json().await?;

            if let Some(error) = result.get("error").and_then(|v| v.as_str()) {
                return Err(error.to_string().into());
            }

            if json_output {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("{} Domain removed", "✅".green().bold());
                if let Some(warnings) = result.get("warnings").and_then(|v| v.as_array()) {
                    for w in warnings {
                        if let Some(w) = w.as_str() {
                            println!("   {} {}", "warning:".yellow(), w);
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

// ============= IPV6 HANDLERS =============

async fn handle_ipv6(
    command: Ipv6Commands,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = create_client()?;

    match command {
        Ipv6Commands::Open { container } => {
            let container = resolve_alias(&container);
            if !json_output {
                println!(
                    "{}",
                    format!("🌍 Opening IPv6 firewall for {}...", container).cyan()
                );
            }

            let url = format!("{}/ipv6/{}/open", *API_BASE_URL, container);
            let response = client.post(&url).send().await?;

            if !response.status().is_success() {
                let error: ApiError = response.json().await?;
                return Err(error.error.into());
            }

            let result: Ipv6OpenResponse = response.json().await?;

            if json_output {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("{}", "✅ IPv6 firewall opened!".green().bold());
                println!();
                if let Some(ipv6) = &result.ipv6_address {
                    println!("   {} {}", "Address:".cyan(), ipv6.white().bold());
                }
                if !result.access_urls.is_empty() {
                    println!("   {} {}", "Access:".cyan(), result.access_urls[0]);
                }
                println!();
                println!(
                    "   {} Container is now accessible from the internet via IPv6",
                    "🌍".yellow()
                );
            }
        }
        Ipv6Commands::Close { container } => {
            let container = resolve_alias(&container);
            if !json_output {
                println!(
                    "{}",
                    format!("🔒 Closing IPv6 firewall for {}...", container).cyan()
                );
            }

            let url = format!("{}/ipv6/{}/close", *API_BASE_URL, container);
            let response = client.post(&url).send().await?;

            if !response.status().is_success() {
                let error: ApiError = response.json().await?;
                return Err(error.error.into());
            }

            if json_output {
                let result: ApiResponse = response.json().await?;
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("{} IPv6 firewall closed", "✅".green().bold());
            }
        }
        Ipv6Commands::Status { container } => {
            let container = resolve_alias(&container);
            let url = format!("{}/ipv6/{}/status", *API_BASE_URL, container);
            let response = client.get(&url).send().await?;

            if !response.status().is_success() {
                let error: ApiError = response.json().await?;
                return Err(error.error.into());
            }

            let result: Ipv6StatusResponse = response.json().await?;

            if json_output {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("{}", "🌍 IPv6 Status:".cyan().bold());
                println!();
                if let Some(ipv6) = &result.ipv6_address {
                    println!("   {} {}", "Address:".cyan(), ipv6);
                }
                if let Some(status) = &result.firewall_status {
                    println!(
                        "   {} {}",
                        "Firewall:".cyan(),
                        if status == "open" {
                            status.green()
                        } else {
                            status.yellow()
                        }
                    );
                }
                if !result.exposed_ports.is_empty() {
                    println!("   {} {:?}", "Ports:".cyan(), result.exposed_ports);
                }
                if let Some(urls) = &result.access_urls {
                    if !urls.is_empty() {
                        println!("   {} {}", "Access:".cyan(), urls[0]);
                    }
                }
            }
        }
        Ipv6Commands::List => {
            let url = format!("{}/ipv6/list", *API_BASE_URL);
            let response = client.get(&url).send().await?;

            if !response.status().is_success() {
                let error: ApiError = response.json().await?;
                return Err(error.error.into());
            }

            let result: Ipv6ListResponse = response.json().await?;

            if json_output {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else if result.allocations.is_empty() {
                println!("{}", "No IPv6 allocations.".yellow());
                println!("   Deploy with IPv6: nordkraft deploy nginx:alpine --ipv6");
            } else {
                println!(
                    "{}",
                    format!("🌍 {} IPv6 allocation(s):", result.allocations.len())
                        .green()
                        .bold()
                );
                println!();
                for alloc in &result.allocations {
                    let fw_status = if alloc.firewall_status == "open" {
                        "🟢 open".green()
                    } else {
                        "🔴 closed".red()
                    };
                    println!(
                        "   {} {}",
                        alloc.container_name.cyan().bold(),
                        alloc.ipv6_address.white()
                    );
                    println!(
                        "     Firewall: {} | Ports: {:?}",
                        fw_status, alloc.exposed_ports
                    );
                    if !alloc.access_urls.is_empty() {
                        println!("     URL: {}", alloc.access_urls[0]);
                    }
                    println!();
                }
            }
        }
        Ipv6Commands::Ports { container, ports } => {
            let container = resolve_alias(&container);
            if !json_output {
                println!(
                    "{}",
                    format!("🔧 Updating IPv6 ports for {}...", container).cyan()
                );
            }

            let request = Ipv6PortsRequest {
                ports: ports.clone(),
            };
            let url = format!("{}/ipv6/{}/ports", *API_BASE_URL, container);
            let response = client.post(&url).json(&request).send().await?;

            if !response.status().is_success() {
                let error: ApiError = response.json().await?;
                return Err(error.error.into());
            }

            if json_output {
                let result: ApiResponse = response.json().await?;
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("{} Ports updated: {:?}", "✅".green().bold(), ports);
            }
        }
    }

    Ok(())
}

// ============= NETWORK HANDLERS =============

async fn handle_network(
    command: NetworkCommands,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = create_client()?;

    match command {
        NetworkCommands::Info => {
            let url = format!("{}/network/info", *API_BASE_URL);
            let response = client.get(&url).send().await?;

            if !response.status().is_success() {
                let error: ApiError = response.json().await?;
                return Err(error.error.into());
            }

            let result: NetworkInfoResponse = response.json().await?;

            if json_output {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("{}", "🌐 Network Info:".cyan().bold());
                println!();
                if let Some(garage) = &result.garage {
                    println!("   {} {}", "Garage:".cyan(), garage);
                }
                if let Some(subnet) = &result.container_subnet {
                    println!("   {} {}", "Container subnet:".cyan(), subnet);
                }
                if let Some(ip) = &result.user_ip {
                    println!("   {} {}", "Your VPN IP:".cyan(), ip);
                }
            }
        }
    }

    Ok(())
}

// ============= OTHER HANDLERS =============

async fn handle_nodes(json_output: bool) -> Result<(), Box<dyn std::error::Error>> {
    let client = create_client()?;
    let url = format!("{}/nodes", *API_BASE_URL);
    let response = client.get(&url).send().await?;

    if !response.status().is_success() {
        let error: ApiError = response.json().await?;
        return Err(error.error.into());
    }

    let result: NodesResponse = response.json().await?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!(
            "{}",
            format!("🖥️  {} node(s):", result.nodes.len()).cyan().bold()
        );
        println!();
        for node in &result.nodes {
            let status_colored = if node.status == "online" {
                "●".green()
            } else {
                "●".red()
            };
            println!(
                "   {} {} ({})",
                status_colored,
                node.id.white().bold(),
                node.status
            );
        }
    }

    Ok(())
}

async fn handle_status(json_output: bool) -> Result<(), Box<dyn std::error::Error>> {
    let client = create_client()?;
    let url = format!("{}/status", *API_BASE_URL);
    let response = client.get(&url).send().await?;

    if !response.status().is_success() {
        if json_output {
            println!("{}", serde_json::json!({"status": "offline"}));
        } else {
            println!("{} API offline", "●".red());
        }
        return Ok(());
    }

    let result: StatusResponse = response.json().await?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!(
            "{} API {} ({})",
            "●".green(),
            result.status.green(),
            result.timestamp.unwrap_or_default().dimmed()
        );
        let (url, source) = resolve_api_base_url();
        println!("  {} {} ({})", "Endpoint:".dimmed(), url, source.dimmed());
        if let (Some(v), Some(n)) = (&result.version, &result.node_id) {
            println!("  {} nordkraft-node {} on {}", "Server:".dimmed(), v, n);
        }
        if result.dev == Some(true) {
            println!(
                "  {} {} · {} · {}",
                "⚠ DEV MODE".yellow().bold(),
                result.mode.as_deref().unwrap_or("hybrid"),
                result.network_mode.as_deref().unwrap_or("bridge"),
                result.auth.as_deref().unwrap_or("no auth").yellow()
            );
        } else if let Some(m) = &result.mode {
            println!(
                "  {} {} · {} · {}",
                "Mode:".dimmed(),
                m,
                result.network_mode.as_deref().unwrap_or("macvlan"),
                result.auth.as_deref().unwrap_or("wireguard peer")
            );
        }
    }

    Ok(())
}

// ============= UPDATE HANDLER =============

const GITHUB_REPO: &str = "ft-karlsson/nordkraft-io-garage-cloud";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    assets: Vec<GithubAsset>,
    html_url: String,
}

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

async fn handle_update(
    check_only: bool,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if !json_output {
        println!("{}", "🔄 Checking for updates...".cyan());
    }

    // Get latest release from GitHub
    let client = reqwest::Client::builder()
        .user_agent("nordkraft-cli")
        .build()?;

    let url = format!(
        "https://api.github.com/repos/{}/releases/latest",
        GITHUB_REPO
    );
    let response = client.get(&url).send().await?;

    if !response.status().is_success() {
        return Err("Failed to check for updates. GitHub API unavailable.".into());
    }

    let release: GithubRelease = response.json().await?;
    let latest_version = release.tag_name.trim_start_matches('v');

    if json_output {
        println!(
            "{}",
            serde_json::json!({
                "current_version": CURRENT_VERSION,
                "latest_version": latest_version,
                "update_available": latest_version != CURRENT_VERSION,
                "release_url": release.html_url
            })
        );
        return Ok(());
    }

    // Compare versions
    if latest_version == CURRENT_VERSION {
        println!(
            "{} You're on the latest version ({})",
            "✅".green(),
            CURRENT_VERSION.green()
        );
        return Ok(());
    }

    println!("   Current version: {}", CURRENT_VERSION.yellow());
    println!("   Latest version:  {}", latest_version.green());
    println!();

    if check_only {
        println!("   Run {} to update", "nordkraft update".cyan());
        return Ok(());
    }

    // Determine platform
    let platform = get_platform()?;
    let asset_name = format!("nordkraft-{}.tar.gz", platform);

    let asset = release
        .assets
        .iter()
        .find(|a| a.name == asset_name)
        .ok_or_else(|| format!("No release found for platform: {}", platform))?;

    println!("{}", format!("📦 Downloading {}...", asset_name).cyan());

    // Download to temp file
    let download_response = client.get(&asset.browser_download_url).send().await?;
    if !download_response.status().is_success() {
        return Err("Failed to download update".into());
    }

    let bytes = download_response.bytes().await?;

    // Create temp directory
    let temp_dir = std::env::temp_dir().join("nordkraft-update");
    std::fs::create_dir_all(&temp_dir)?;

    let tarball_path = temp_dir.join(&asset_name);
    std::fs::write(&tarball_path, &bytes)?;

    println!("{}", "📂 Extracting...".cyan());

    // Extract tarball
    let output = std::process::Command::new("tar")
        .args([
            "-xzf",
            tarball_path.to_str().unwrap(),
            "-C",
            temp_dir.to_str().unwrap(),
        ])
        .output()?;

    if !output.status.success() {
        return Err("Failed to extract update".into());
    }

    let new_binary = temp_dir.join("nordkraft");

    // Get current executable path
    let current_exe = std::env::current_exe()?;

    println!(
        "{}",
        format!("🔧 Installing to {}...", current_exe.display()).cyan()
    );

    // On Unix, we need to handle the replacement carefully
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        // Make new binary executable
        let mut perms = std::fs::metadata(&new_binary)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&new_binary, perms)?;

        // Try to replace directly first, fall back to sudo if needed
        if std::fs::rename(&new_binary, &current_exe).is_err() {
            println!("{}", "   Need sudo permission...".yellow());

            let status = std::process::Command::new("sudo")
                .args([
                    "mv",
                    new_binary.to_str().unwrap(),
                    current_exe.to_str().unwrap(),
                ])
                .status()?;

            if !status.success() {
                return Err("Failed to install update. Try running with sudo.".into());
            }
        }
    }

    #[cfg(windows)]
    {
        // On Windows, rename current exe and move new one
        let backup_path = current_exe.with_extension("old");
        std::fs::rename(&current_exe, &backup_path)?;
        std::fs::rename(&new_binary, &current_exe)?;
        let _ = std::fs::remove_file(&backup_path);
    }

    // Cleanup
    let _ = std::fs::remove_dir_all(&temp_dir);

    println!();
    println!(
        "{} Updated to version {}!",
        "🎉".green(),
        latest_version.green()
    );
    println!("   Run {} to verify", "nordkraft --version".cyan());

    Ok(())
}

fn get_platform() -> Result<String, Box<dyn std::error::Error>> {
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        return Err("Unsupported operating system".into());
    };

    let arch = if cfg!(target_arch = "x86_64") {
        "amd64"
    } else if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        return Err("Unsupported architecture".into());
    };

    Ok(format!("{}-{}", os, arch))
}

// ============= ALIAS HANDLERS =============

async fn handle_alias(
    command: AliasCommands,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        AliasCommands::Set { alias, container } => {
            set_alias(&alias, &container)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"status": "ok", "alias": alias, "container": container})
                );
            } else {
                println!(
                    "{} Alias '{}' → '{}'",
                    "✅".green().bold(),
                    alias.cyan(),
                    container
                );
            }
        }
        AliasCommands::Remove { alias } => {
            remove_alias(&alias)?;
            if json_output {
                println!("{}", serde_json::json!({"status": "ok", "removed": alias}));
            } else {
                println!("{} Removed alias '{}'", "✅".green().bold(), alias);
            }
        }
        AliasCommands::List => {
            let aliases = load_aliases();
            if json_output {
                println!("{}", serde_json::to_string_pretty(&aliases)?);
            } else if aliases.is_empty() {
                println!("{}", "No aliases configured.".yellow());
                println!("   Set one: nordkraft alias set myapp app-abc123...");
            } else {
                println!("{}", "📝 Container aliases:".cyan().bold());
                println!();
                for (alias, container) in &aliases {
                    println!("   {} → {}", alias.cyan().bold(), container.dimmed());
                }
            }
        }
    }
    Ok(())
}

// ============= INTERACTIVE HELPERS =============

// ============= REGISTRY HANDLERS =============

async fn handle_registry(
    command: RegistryCommands,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        RegistryCommands::Init => handle_registry_init(json_output).await,
        RegistryCommands::Status => handle_registry_status(json_output).await,
        RegistryCommands::List => handle_registry_list(json_output).await,
        RegistryCommands::Push { image } => handle_registry_push(&image, json_output).await,
        RegistryCommands::Destroy { force } => handle_registry_destroy(force, json_output).await,
    }
}

async fn handle_registry_init(json_output: bool) -> Result<(), Box<dyn std::error::Error>> {
    // Check if already initialized
    if let Some(config) = load_registry_config() {
        if !json_output {
            println!("{}", "📦 Registry already initialized!".yellow());
            println!("   {} {}", "Address:".cyan(), config.address);
            println!("   {} {}", "Container:".cyan(), config.container_name);
            println!();
            println!(
                "   {} Run '{}' to check status",
                "💡".yellow(),
                "nordkraft registry status".cyan()
            );
        } else {
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
        return Ok(());
    }

    if !json_output {
        println!("{}", "📦 Initializing private registry...".cyan());
    }

    // Credentials: the registry enforces Basic auth, and the platform presents these
    // only when *you* deploy from this registry. Nobody else gets them.
    let password = generate_registry_password()?;
    let mut env_vars = HashMap::new();
    env_vars.insert(
        "REGISTRY_USERNAME".to_string(),
        REGISTRY_USERNAME.to_string(),
    );
    env_vars.insert("REGISTRY_PASSWORD".to_string(), password.clone());

    // Deploy the registry container via the API
    let client = create_client()?;
    let request = DeployRequest {
        image: REGISTRY_IMAGE.to_string(),
        ports: vec![PortSpec {
            port: 5001,
            protocol: "tcp".to_string(),
        }],
        command: None,
        env_vars,
        cpu_limit: 0.5,
        memory_limit: "256m".to_string(),
        enable_persistence: true,
        volume_path: Some("/data".to_string()),
        volume_size: Some("1g".to_string()),
        enable_ipv6: false,
        target_garage: None,
        hardware_preference: None,
        lifecycle: LifecycleSpec::default(),
        labels: HashMap::new(),
        wait: false,
        wait_timeout_s: None,
    };

    let url = format!("{}/containers/deploy", *API_BASE_URL);
    let response = client.post(&url).json(&request).send().await?;

    if !response.status().is_success() {
        let error: ApiError = response.json().await?;
        return Err(format!("Failed to deploy registry: {}", error.error).into());
    }

    let result: DeployResponse = response.json().await?;

    let container_name = result
        .container_name
        .ok_or("No container name in response")?;
    let container_ip = result.container_ip.ok_or("No container IP in response")?;

    let registry_address = format!("{}:5001", container_ip);

    // Save registry config
    let config = RegistryConfig {
        address: registry_address.clone(),
        container_name: container_name.clone(),
        container_alias: Some("registry".to_string()),
        username: Some(REGISTRY_USERNAME.to_string()),
        password: Some(password),
    };
    save_registry_config(&config)?;

    // Set alias
    let _ = set_alias("registry", &container_name);

    if !json_output {
        println!("{}", "✅ Private registry ready!".green().bold());
        println!();
        println!("   {} {}", "Address:".cyan(), registry_address);
        println!("   {} {}", "Container:".cyan(), container_name);
        println!("   {} registry", "Alias:".cyan());
        println!(
            "   {} basic auth, credentials kept in ~/.nordkraft/registry.json",
            "Auth:".cyan()
        );
        if let Some(garage) = &result.garage {
            println!("   {} {}", "Garage:".cyan(), garage);
        }
        println!();
        println!("{}", "🎯 Next steps:".yellow().bold());
        println!("   nordkraft push myapp:v1               Push an image");
        println!("   nordkraft deploy myapp:v1 --port 80   Deploy from registry");
        println!("   nordkraft registry list               List stored images");
    } else {
        println!("{}", serde_json::to_string_pretty(&config)?);
    }

    Ok(())
}

async fn handle_registry_push(
    image: &str,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = load_registry_config()
        .ok_or("Registry not initialized. Run 'nordkraft registry init' first.")?;

    let registry_tag = format!("{}/{}", config.address, image);

    if !json_output {
        println!(
            "{}",
            format!("📤 Pushing {} to private registry...", image).cyan()
        );
    }

    // Tag the image for the private registry
    let tag_output = std::process::Command::new("podman")
        .args(["tag", image, &registry_tag])
        .output()
        .map_err(|e| format!("Failed to run podman tag: {}. Is podman installed?", e))?;

    if !tag_output.status.success() {
        let stderr = String::from_utf8_lossy(&tag_output.stderr);
        return Err(format!("Failed to tag image: {}", stderr).into());
    }

    // Push to registry (TLS disabled — WireGuard handles encryption)
    let mut push_args = vec![
        "push".to_string(),
        registry_tag.clone(),
        "--tls-verify=false".to_string(),
    ];
    if let (Some(user), Some(pass)) = (&config.username, &config.password) {
        push_args.push("--creds".to_string());
        push_args.push(format!("{user}:{pass}"));
    }
    let push_output = std::process::Command::new("podman")
        .args(&push_args)
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|e| format!("Failed to run podman push: {}", e))?;

    if !push_output.success() {
        return Err("Push failed".into());
    }

    // Clean up the temporary tag
    let _ = std::process::Command::new("podman")
        .args(["rmi", &registry_tag])
        .output();

    if !json_output {
        println!("{}", format!("✅ Pushed {}", image).green().bold());
        println!();
        println!("   {} Deploy it:", "🎯".yellow());
        println!("     nordkraft deploy registry://{} --port <PORT>", image);
    } else {
        println!(
            "{}",
            serde_json::json!({
                "status": "pushed",
                "image": image,
                "registry": config.address,
                "registry_tag": registry_tag,
            })
        );
    }

    Ok(())
}

async fn handle_registry_status(json_output: bool) -> Result<(), Box<dyn std::error::Error>> {
    let config = load_registry_config()
        .ok_or("Registry not initialized. Run 'nordkraft registry init' first.")?;

    // Check if the registry container is running
    let client = create_client()?;

    let reachable = match registry_get(&client, &config, "/v2/").send().await {
        Ok(resp) => resp.status().is_success(),
        Err(_) => false,
    };

    if json_output {
        println!(
            "{}",
            serde_json::json!({
                "address": config.address,
                "container": config.container_name,
                "reachable": reachable,
            })
        );
        return Ok(());
    }

    println!("{}", "📦 Private Registry".cyan().bold());
    println!();
    println!("   {} {}", "Address:".cyan(), config.address);
    println!("   {} {}", "Container:".cyan(), config.container_name);
    println!(
        "   {} {}",
        "Status:".cyan(),
        if reachable {
            "online".green().bold()
        } else {
            "offline".red().bold()
        }
    );

    // List images if reachable
    if reachable {
        if let Ok(catalogs) = list_registry_repos(&client, &config).await {
            if catalogs.is_empty() {
                println!("   {} (no images)", "Images:".cyan());
            } else {
                println!("   {} {}", "Images:".cyan(), catalogs.len());
                for repo in &catalogs {
                    println!("     {} {}", "•".dimmed(), repo);
                }
            }
        }
    }
    println!();

    Ok(())
}

async fn handle_registry_list(json_output: bool) -> Result<(), Box<dyn std::error::Error>> {
    let config = load_registry_config()
        .ok_or("Registry not initialized. Run 'nordkraft registry init' first.")?;

    let client = create_client()?;
    let repos = list_registry_repos(&client, &config).await?;

    if json_output {
        let mut all_images = Vec::new();
        for repo in &repos {
            if let Ok(tags) = list_registry_tags(&client, &config, repo).await {
                for tag in tags {
                    all_images.push(serde_json::json!({
                        "name": repo,
                        "tag": tag,
                        "full": format!("{}:{}", repo, tag),
                    }));
                }
            }
        }
        println!("{}", serde_json::to_string_pretty(&all_images)?);
        return Ok(());
    }

    if repos.is_empty() {
        println!("{}", "No images in registry.".yellow());
        println!("   Push one: nordkraft push myapp:v1");
        return Ok(());
    }

    println!("{}", "📦 Registry images:".green().bold());
    println!();
    for repo in &repos {
        if let Ok(tags) = list_registry_tags(&client, &config, repo).await {
            for tag in &tags {
                println!("   {} {}:{}", "•".dimmed(), repo.white().bold(), tag.cyan());
            }
        } else {
            println!("   {} {} {}", "•".dimmed(), repo, "(no tags)".dimmed());
        }
    }
    println!();

    Ok(())
}

async fn handle_registry_destroy(
    force: bool,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = load_registry_config().ok_or("Registry not initialized. Nothing to destroy.")?;

    if !force && !json_output {
        println!(
            "{}",
            "⚠️  This will destroy your private registry and all stored images.".red()
        );
        let confirmed = Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt("Are you sure?")
            .default(false)
            .interact()?;

        if !confirmed {
            println!("{}", "Cancelled.".dimmed());
            return Ok(());
        }
    }

    if !json_output {
        println!("{}", "🗑️  Destroying registry...".cyan());
    }

    // Remove the container
    let _ = handle_container_remove(&config.container_name, json_output).await;

    // Clean up alias and config
    let _ = remove_alias("registry");
    remove_registry_config();

    if !json_output {
        println!("{}", "✅ Registry destroyed.".green().bold());
        println!(
            "   Run '{}' to set up again.",
            "nordkraft registry init".cyan()
        );
    }

    Ok(())
}

/// List all repositories in the registry (OCI _catalog endpoint)
async fn list_registry_repos(
    client: &Client,
    config: &RegistryConfig,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    // The OCI spec doesn't mandate _catalog, but our registry implements the common
    // Docker _catalog extension.
    match registry_get(client, config, "/v2/_catalog").send().await {
        Ok(resp) if resp.status().is_success() => {
            let body: serde_json::Value = resp.json().await?;
            if let Some(repos) = body["repositories"].as_array() {
                Ok(repos
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect())
            } else {
                Ok(vec![])
            }
        }
        _ => {
            // _catalog not supported — return empty (user can still push/pull by name)
            Ok(vec![])
        }
    }
}

/// List tags for a repository
async fn list_registry_tags(
    client: &Client,
    config: &RegistryConfig,
    repo: &str,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let resp = registry_get(client, config, &format!("/v2/{}/tags/list", repo))
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err("Failed to list tags".into());
    }

    let body: serde_json::Value = resp.json().await?;
    if let Some(tags) = body["tags"].as_array() {
        Ok(tags
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect())
    } else {
        Ok(vec![])
    }
}

// ============= WIREGUARD HELPERS =============

/// Generate WireGuard keypair using system `wg` command (same approach as signup API)
fn generate_wireguard_keypair() -> Result<(String, String), Box<dyn std::error::Error>> {
    use std::process::Command;

    let private_output = Command::new("wg")
        .args(["genkey"])
        .output()
        .map_err(|e| format!("Failed to run wg genkey: {e}. Is wireguard-tools installed?"))?;

    if !private_output.status.success() {
        return Err("wg genkey failed".into());
    }

    let private_key = String::from_utf8(private_output.stdout)?.trim().to_string();

    let public_output = Command::new("sh")
        .args(["-c", &format!("echo '{private_key}' | wg pubkey")])
        .output()
        .map_err(|e| format!("Failed to run wg pubkey: {e}"))?;

    if !public_output.status.success() {
        return Err("wg pubkey failed".into());
    }

    let public_key = String::from_utf8(public_output.stdout)?.trim().to_string();

    Ok((private_key, public_key))
}

/// Write WireGuard config file (same format as signup API's generate_wireguard_config_with_container_access)
fn write_wg_config(
    private_key: &str,
    client_ip: &str,
    server_public_key: &str,
    server_endpoint: &str,
    allowed_ips: &[String],
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let dir = get_config_dir();
    std::fs::create_dir_all(&dir)?;

    // Secure the directory itself (wg-quick may check parent permissions)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }

    let path = dir.join(WG_CONFIG_FILE);

    let allowed_ips_str = allowed_ips.join(", ");

    let config = format!(
        r#"[Interface]
PrivateKey = {private_key}
Address = {client_ip}/32

[Peer]
PublicKey = {server_public_key}
Endpoint = {server_endpoint}
AllowedIPs = {allowed_ips_str}
PersistentKeepalive = 25"#
    );

    // Write with restrictive permissions from the start — no race window
    // where the file exists world-readable before chmod
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(config.as_bytes())?;
    }

    #[cfg(not(unix))]
    {
        std::fs::write(&path, config)?;
    }

    Ok(path)
}

/// Build a `sudo wg-quick <action> <interface>` command.
///
/// On macOS wg-quick(8) is `#!/usr/bin/env bash` and refuses bash < 4, while the
/// system bash at /bin/bash is 3.2 and will stay there (Apple froze it at the
/// last GPLv2 release). Homebrew's bash therefore has to win the PATH lookup that
/// happens *inside* the script, and sudo may drop the Homebrew prefix from PATH
/// altogether (secure_path), which would hide wg-quick itself. Pin PATH for the
/// child process so neither lookup depends on how the user's shell is ordered.
fn wg_quick_command(action: &str) -> std::process::Command {
    use std::process::Command;

    let mut cmd = Command::new("sudo");

    if cfg!(target_os = "macos") {
        let brew_bin = Command::new("brew")
            .arg("--prefix")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| format!("{}/bin", String::from_utf8_lossy(&o.stdout).trim()))
            .unwrap_or_else(|| "/opt/homebrew/bin".to_string());
        cmd.args([
            "env",
            &format!("PATH={brew_bin}:/usr/local/bin:/usr/bin:/bin"),
        ]);
    }

    cmd.args(["wg-quick", action, WG_INTERFACE]);
    cmd
}

/// Turn wg-quick's raw stderr into something the user can act on.
fn explain_wg_quick_error(stderr: &str) -> String {
    let stderr = stderr.trim();

    if stderr.contains("bash 4+ required") || stderr.contains("Version mismatch") {
        return format!(
            "{stderr}\n   → wg-quick requires bash 4+, but macOS ships bash 3.2.\n     Install a newer one with: brew install bash"
        );
    }

    if stderr.contains("command not found") {
        return format!(
            "{stderr}\n   → wg-quick was not found.\n     Install it with: brew install wireguard-tools"
        );
    }

    stderr.to_string()
}

/// Bring WireGuard up using wg-quick
/// Copies config to /etc/wireguard/nordkraft.conf (root-owned) to avoid
/// permission issues with uutils/Rust coreutils stat + AppArmor on newer Ubuntu.
fn wg_up(config_path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    use std::process::Command;

    // Migration: strip DNS line from existing configs — it causes systemd-resolved
    // on newer Ubuntu (25.x) to route all DNS through the tunnel, killing internet.
    if let Ok(contents) = std::fs::read_to_string(config_path) {
        if contents.contains("DNS =") || contents.contains("DNS=") {
            let cleaned: String = contents
                .lines()
                .filter(|line| {
                    let trimmed = line.trim();
                    !trimmed.starts_with("DNS =") && !trimmed.starts_with("DNS=")
                })
                .collect::<Vec<_>>()
                .join("\n");
            // Write back cleaned config (best-effort, non-fatal if it fails)
            let _ = std::fs::write(config_path, &cleaned);
        }
    }

    // macOS (Homebrew wg-quick) uses /usr/local/etc/wireguard/
    // Linux uses /etc/wireguard/
    let wg_dir = if cfg!(target_os = "macos") {
        "/usr/local/etc/wireguard"
    } else {
        "/etc/wireguard"
    };
    let system_conf = format!("{wg_dir}/{WG_INTERFACE}.conf");

    // Ensure the WireGuard config directory exists (macOS doesn't create it)
    let mkdir_output = Command::new("sudo")
        .args(["mkdir", "-p", wg_dir])
        .output()
        .map_err(|e| format!("Failed to create {wg_dir}: {e}"))?;

    if !mkdir_output.status.success() {
        let stderr = String::from_utf8_lossy(&mkdir_output.stderr);
        return Err(format!("Failed to create {wg_dir}: {stderr}").into());
    }

    // Copy config to wireguard dir as root (owned by root, mode 600)
    let cp_output = Command::new("sudo")
        .args(["cp", &config_path.to_string_lossy(), &system_conf])
        .output()
        .map_err(|e| format!("Failed to copy WireGuard config: {e}"))?;

    if !cp_output.status.success() {
        let stderr = String::from_utf8_lossy(&cp_output.stderr);
        return Err(format!("Failed to copy config to {system_conf}: {stderr}").into());
    }

    // Ensure strict permissions (skip chown root:root on macOS — use root:wheel)
    let _ = Command::new("sudo")
        .args(["chmod", "600", &system_conf])
        .output();
    if cfg!(target_os = "macos") {
        let _ = Command::new("sudo")
            .args(["chown", "root:wheel", &system_conf])
            .output();
    } else {
        let _ = Command::new("sudo")
            .args(["chown", "root:root", &system_conf])
            .output();
    }

    // Use interface name instead of file path — wg-quick looks up /etc/wireguard/<name>.conf
    let output = wg_quick_command("up")
        .output()
        .map_err(|e| format!("Failed to run wg-quick up: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("already exists") {
            return Ok(());
        }
        return Err(format!("wg-quick up failed: {}", explain_wg_quick_error(&stderr)).into());
    }

    Ok(())
}

/// Bring WireGuard down
fn wg_down(_config_path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    use std::process::Command;

    let output = wg_quick_command("down")
        .output()
        .map_err(|e| format!("Failed to run wg-quick down: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Already down is not an error
        if stderr.contains("is not a WireGuard interface") {
            return Ok(());
        }
        return Err(format!("wg-quick down failed: {}", explain_wg_quick_error(&stderr)).into());
    }

    // Clean up system config (macOS: wg-quick checks both paths)
    let wg_dirs: Vec<&str> = if cfg!(target_os = "macos") {
        vec!["/etc/wireguard", "/usr/local/etc/wireguard"]
    } else {
        vec!["/etc/wireguard"]
    };
    for wg_dir in wg_dirs {
        let system_conf = format!("{wg_dir}/{WG_INTERFACE}.conf");
        let _ = Command::new("sudo")
            .args(["rm", "-f", &system_conf])
            .output();
    }

    Ok(())
}

// ============= SETUP / CONNECT / DISCONNECT HANDLERS =============

async fn handle_setup(token: String, json_output: bool) -> Result<(), Box<dyn std::error::Error>> {
    if !token.starts_with("NKINVITE-") {
        return Err("Invalid token format. Expected NKINVITE-...".into());
    }

    if !json_output {
        println!("{}", "🔧 NordKraft Setup".cyan().bold());
        println!();
    }

    // 1. Generate WireGuard keypair locally
    if !json_output {
        println!("{}", "  Generating WireGuard keypair...".dimmed());
    }
    let (private_key, public_key) = generate_wireguard_keypair()?;

    // 2. Claim token via public API (no WireGuard needed yet)
    if !json_output {
        println!("{}", "  Claiming invite token...".dimmed());
    }
    let client = Client::builder().timeout(Duration::from_secs(15)).build()?;

    let claim_req = ClaimRequest {
        token: token.clone(),
        wireguard_public_key: public_key,
    };

    let response = client
        .post(format!("{}/claim", *PUBLIC_API_URL))
        .json(&claim_req)
        .send()
        .await
        .map_err(|e| format!("Failed to reach signup API: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("Claim failed (HTTP {status}): {body}").into());
    }

    let claim_resp: ClaimApiResponse = response.json().await?;

    if !claim_resp.success {
        return Err(format!(
            "Claim failed: {}",
            claim_resp
                .error
                .unwrap_or_else(|| "Unknown error".to_string())
        )
        .into());
    }

    let data = claim_resp.data.ok_or("Claim response missing data")?;

    // 3. Write WireGuard config
    if !json_output {
        println!("{}", "  Writing WireGuard config...".dimmed());
    }
    let wg_path = write_wg_config(
        &private_key,
        &data.wireguard_ip,
        &data.server_public_key,
        &data.server_endpoint,
        &data.allowed_ips,
    )?;

    // 4. Save connection config
    let conn_config = ConnectionConfig {
        user_id: data.user_id,
        full_name: data.full_name.clone(),
        email: data.email.clone(),
        plan_id: data.plan_id,
        assigned_garage: data.assigned_garage,
        wireguard_ip: data.wireguard_ip.clone(),
        server_public_key: data.server_public_key,
        server_endpoint: data.server_endpoint,
        allowed_ips: data.allowed_ips,
        api_endpoint: None,
        public_api_url: None,
    };
    save_connection_config(&conn_config)?;

    // 5. Bring WireGuard up
    if !json_output {
        println!("{}", "  Starting WireGuard...".dimmed());
    }
    wg_up(&wg_path)?;

    // Wait for provisioning to complete — the controller needs a few seconds
    // to register the WireGuard peer and populate routing
    if !json_output {
        let spinner_frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let messages = [
            "Establishing secure tunnel...",
            "Registering your identity...",
            "Sprinkling fairy dust...",
            "Waking up the garage...",
            "Almost there...",
        ];
        let term = Term::stdout();
        let total_ms = 5000u64;
        let step_ms = 100u64;
        let steps = total_ms / step_ms;
        for i in 0..steps {
            let frame = spinner_frames[i as usize % spinner_frames.len()];
            let msg = messages[(i as usize * messages.len()) / steps as usize];
            let _ = term.clear_line();
            print!("\r  {} {}", frame.cyan(), msg.dimmed());
            let _ = std::io::Write::flush(&mut std::io::stdout());
            tokio::time::sleep(Duration::from_millis(step_ms)).await;
        }
        let _ = term.clear_line();
        print!("\r");
    }

    // 6. Verify connection (with retry — provisioning may need an extra moment)
    if !json_output {
        println!("{}", "  Verifying connection...".dimmed());
    }
    let verify_client = create_client()?;
    let verify_url = format!("{}/auth/verify", *API_BASE_URL);

    // Retry verify up to 3 times (provisioning can take a moment)
    let mut verified = false;
    for attempt in 0..3 {
        match verify_client.get(&verify_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                verified = true;
                break;
            }
            _ => {
                if attempt < 2 {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }

    if verified {
        if !json_output {
            println!();
            println!("{}", "✅ Connected to NordKraft!".green().bold());
            println!();
            println!("   {} {}", "Name:".cyan(), data.full_name);
            println!("   {} {}", "Email:".cyan(), data.email);
            println!("   {} {}/32", "VPN IP:".cyan(), data.wireguard_ip);
            println!();
            println!("{}", "🎯 Try it:".yellow().bold());
            println!("   nordkraft deploy nginx:alpine");
            println!("   nordkraft list");
            println!();
            println!("{}", "🔌 Connection:".yellow().bold());
            println!("   nordkraft disconnect    Disconnect VPN");
            println!("   nordkraft connect       Reconnect VPN");
        } else {
            println!(
                "{}",
                serde_json::json!({
                    "status": "connected",
                    "wireguard_ip": data.wireguard_ip,
                })
            );
        }
    } else if !json_output {
        println!();
        println!("{}", "✅ WireGuard is up!".green().bold());
        println!();
        println!("   {} {}", "Name:".cyan(), data.full_name);
        println!("   {} {}", "Email:".cyan(), data.email);
        println!("   {} {}/32", "VPN IP:".cyan(), data.wireguard_ip);
        println!();
        println!(
            "{}",
            "⚠️  API not reachable yet — provisioning may still be in progress.".yellow()
        );
        println!("{}", "🎯 Try in a moment:".yellow().bold());
        println!("   nordkraft auth status");
        println!("   nordkraft deploy nginx:alpine");
        println!();
        println!("{}", "🔌 Connection:".yellow().bold());
        println!("   nordkraft disconnect    Disconnect VPN");
        println!("   nordkraft connect       Reconnect VPN");
    } else {
        println!(
            "{}",
            serde_json::json!({
                "status": "connected",
                "wireguard_ip": data.wireguard_ip,
                "api_verified": false,
            })
        );
    }

    Ok(())
}

async fn handle_connect(json_output: bool) -> Result<(), Box<dyn std::error::Error>> {
    let wg_path = get_wg_config_path();
    if !wg_path.exists() {
        return Err("No WireGuard config found. Run 'nordkraft setup <token>' first.".into());
    }

    if !json_output {
        println!("{}", "🔌 Connecting to NordKraft...".cyan());
    }

    wg_up(&wg_path)?;

    if !json_output {
        if let Some(config) = load_connection_config() {
            println!(
                "{} {}/32",
                "✅ Connected:".green().bold(),
                config.wireguard_ip
            );
        } else {
            println!("{}", "✅ WireGuard up".green().bold());
        }
    } else {
        println!("{}", serde_json::json!({"status": "connected"}));
    }

    Ok(())
}

async fn handle_disconnect(json_output: bool) -> Result<(), Box<dyn std::error::Error>> {
    let wg_path = get_wg_config_path();
    if !wg_path.exists() {
        return Err("No WireGuard config found. Nothing to disconnect.".into());
    }

    if !json_output {
        println!("{}", "🔌 Disconnecting from NordKraft...".cyan());
    }

    wg_down(&wg_path)?;

    if !json_output {
        println!("{}", "✅ Disconnected".green().bold());
    } else {
        println!("{}", serde_json::json!({"status": "disconnected"}));
    }

    Ok(())
}

// ============= ACCOUNT HANDLERS =============

fn fmt_date(value: &serde_json::Value) -> String {
    value
        .as_str()
        .and_then(|s| s.parse::<chrono::DateTime<chrono::Utc>>().ok())
        .map(|t| t.format("%-d %B %Y").to_string())
        .unwrap_or_else(|| value.to_string())
}

async fn handle_account(
    command: AccountCommands,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = create_client()?;
    let base = &*API_BASE_URL;
    match command {
        AccountCommands::Status => {
            let resp = client
                .get(format!("{base}/account"))
                .send()
                .await
                .map_err(unreachable_api)?;
            if !resp.status().is_success() {
                return Err(api_error_text(resp).await.into());
            }
            let v: serde_json::Value = resp.json().await?;
            if json_output {
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            println!("{}", "👤 Account".cyan().bold());
            println!(
                "   {} {}",
                "Name:".cyan(),
                v["full_name"].as_str().unwrap_or("-")
            );
            println!(
                "   {} {}",
                "Email:".cyan(),
                v["email"].as_str().unwrap_or("-")
            );
            println!(
                "   {} {}",
                "Plan:".cyan(),
                v["plan_id"].as_str().unwrap_or("-")
            );
            println!(
                "   {} {}",
                "Status:".cyan(),
                v["account_status"].as_str().unwrap_or("-")
            );
            if v["deletion"].is_object() {
                println!();
                println!(
                    "{}",
                    format!(
                        "⚠️  Deletion scheduled for {} (requested {})",
                        fmt_date(&v["deletion"]["scheduled_for"]),
                        fmt_date(&v["deletion"]["requested_at"])
                    )
                    .yellow()
                );
                println!("   Changed your mind?  nordkraft account restore");
            }
        }
        AccountCommands::Delete {
            yes,
            now,
            code,
            verification_id,
        } => {
            handle_account_delete(&client, yes, now, code, verification_id, json_output).await?;
        }
        AccountCommands::Restore => {
            let resp = client
                .post(format!("{base}/account/deletion/cancel"))
                .send()
                .await
                .map_err(unreachable_api)?;
            if resp.status().as_u16() == 404 {
                if json_output {
                    println!("{}", serde_json::json!({"status": "nothing_pending"}));
                } else {
                    println!("{}", "No pending deletion. Nothing to restore.".dimmed());
                }
                return Ok(());
            }
            if !resp.status().is_success() {
                return Err(api_error_text(resp).await.into());
            }
            if json_output {
                println!("{}", serde_json::json!({"status": "cancelled"}));
            } else {
                println!(
                    "{}",
                    "✅ Deletion cancelled. Everything stays as it is."
                        .green()
                        .bold()
                );
                println!("   Good to still have you around.");
            }
        }
    }
    Ok(())
}

async fn handle_account_delete(
    client: &Client,
    yes: bool,
    now: bool,
    code: Option<String>,
    verification_id: Option<String>,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = &*API_BASE_URL;
    let non_interactive = NON_INTERACTIVE.load(std::sync::atomic::Ordering::Relaxed);

    // Step 1: request a code (unless the caller already has one).
    let (verification_id, email) = match (verification_id, &code) {
        (Some(id), Some(_)) => (id, None),
        (Some(_), None) | (None, Some(_)) => {
            return Err("--verification-id and --code go together".into());
        }
        (None, None) => {
            if !json_output {
                println!("{}", "🗑️  Delete your Garage Cloud account".red().bold());
                println!();
                if now {
                    println!("This will, {}:", "right after you confirm".bold());
                } else {
                    println!("This will, after a 14-day grace period:");
                }
                println!("   • remove every container, volume and volume backup");
                println!("   • remove ingress subdomains, custom domains and IPv6 rules");
                println!("   • remove your WireGuard access and extra peers");
                println!("   • delete your name, address and email from the database");
                println!();
                if now {
                    println!(
                        "There is no way back once the code is confirmed. Without --now you get"
                    );
                    println!("14 days to change your mind or fetch what you want to keep.");
                } else {
                    println!("Until then everything keeps working, so you can back up what you want to keep,");
                    println!("and `nordkraft account restore` cancels the whole thing.");
                    println!(
                        "In a hurry? `nordkraft account delete --now` skips the grace period."
                    );
                }
                println!();
                println!(
                    "To make sure it is really you, a 6-digit code is sent to your email first."
                );
                println!();
            }
            if !yes && !non_interactive {
                let confirmed = Confirm::with_theme(&ColorfulTheme::default())
                    .with_prompt("Send me the code")
                    .default(false)
                    .interact()?;
                if !confirmed {
                    println!("{}", "Cancelled. Nothing changed.".dimmed());
                    return Ok(());
                }
            }
            let resp = client
                .post(format!("{base}/account/deletion/request"))
                .send()
                .await
                .map_err(unreachable_api)?;
            match resp.status().as_u16() {
                202 => {}
                409 => {
                    let v: serde_json::Value = resp.json().await.unwrap_or_default();
                    let when = fmt_date(&v["deletion"]["scheduled_for"]);
                    if json_output {
                        println!(
                            "{}",
                            serde_json::json!({"status": "already_scheduled", "deletion": v["deletion"]})
                        );
                    } else {
                        println!(
                            "{}",
                            format!("Deletion is already scheduled for {when}.").yellow()
                        );
                        println!("   nordkraft account restore   cancels it");
                    }
                    return Ok(());
                }
                _ => return Err(api_error_text(resp).await.into()),
            }
            let v: serde_json::Value = resp.json().await?;
            let id = v["verification_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let email = v["email"].as_str().unwrap_or_default().to_string();
            if id.is_empty() {
                return Err("API did not return a verification id".into());
            }
            (id, Some(email))
        }
    };

    // Step 2: the code.
    let mut pending_code = code;
    if pending_code.is_none() && (json_output || non_interactive) {
        if json_output {
            println!(
                "{}",
                serde_json::json!({
                    "status": "code_sent",
                    "verification_id": verification_id,
                    "email": email,
                    "next": "nordkraft account delete --verification-id <id> --code <code>",
                })
            );
        } else {
            println!("A code was sent to {}.", email.unwrap_or_default());
            println!(
                "Finish with:  nordkraft account delete --verification-id {verification_id} --code <code>"
            );
        }
        return Ok(());
    }
    if !json_output && pending_code.is_none() {
        println!();
        println!(
            "{}",
            format!(
                "📧 I sent a 6-digit code to {}.",
                email.clone().unwrap_or_default()
            )
            .yellow()
        );
        println!(
            "   {}",
            "It is valid for 30 minutes. Press Enter without a code to abort.".dimmed()
        );
        println!();
    }

    loop {
        let input = match pending_code.take() {
            Some(c) => c.trim().to_string(),
            None => Input::<String>::with_theme(&ColorfulTheme::default())
                .with_prompt("Code")
                .allow_empty(true)
                .interact_text()?
                .trim()
                .to_string(),
        };
        if input.is_empty() {
            println!(
                "{}",
                "Aborted. Your account is untouched; the code simply expires.".dimmed()
            );
            return Ok(());
        }
        let resp = client
            .post(format!("{base}/account/deletion/confirm"))
            .json(&serde_json::json!({
                "verification_id": verification_id,
                "code": input,
                "immediate": now,
            }))
            .send()
            .await
            .map_err(unreachable_api)?;
        let status = resp.status();
        if status.is_success() {
            let v: serde_json::Value = resp.json().await?;
            if json_output {
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            println!();
            if v["immediate"].as_bool().unwrap_or(false) {
                println!(
                    "{}",
                    "✅ Confirmed. Your account is being deleted now."
                        .green()
                        .bold()
                );
                println!();
                println!(
                    "The tunnel closes in a moment, and a last email arrives when it is done."
                );
                println!("Then clean up this machine with:");
                println!("   nordkraft reset                   Removes local keys and config");
            } else {
                let when = fmt_date(&v["deletion"]["scheduled_for"]);
                println!(
                    "{}",
                    format!("✅ Confirmed. Your account will be deleted on {when}.")
                        .green()
                        .bold()
                );
                println!();
                println!("Until then everything works as before. To keep something:");
                println!("   nordkraft backup <container>      Save a volume locally");
                println!("Changed your mind?");
                println!("   nordkraft account restore         Cancels the deletion");
                println!("After that date:");
                println!("   nordkraft reset                   Removes local keys and config");
            }
            println!();
            println!("Thank you for trying Garage Cloud. All the best, and maybe see you again some day. 😃");
            return Ok(());
        }
        let msg = api_error_text(resp).await;
        match status.as_u16() {
            400 if !(json_output || non_interactive) => {
                println!("   {}", format!("✗ {msg}").red());
                continue;
            }
            _ => return Err(msg.into()),
        }
    }
}

async fn handle_reset(force: bool, json_output: bool) -> Result<(), Box<dyn std::error::Error>> {
    if !json_output {
        println!("{}", "🧹 NordKraft Reset".red().bold());
        println!();
        println!("This will:");
        println!("   • Disconnect WireGuard VPN");
        println!("   • Remove WireGuard system config");
        println!("   • Delete ~/.nordkraft/ (keys, config, aliases)");
        println!();
        println!(
            "{}",
            "⚠️  You will need a new invite token to set up again.".yellow()
        );
        println!();
    }

    if !force {
        let confirmed = Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt("Are you sure? This cannot be undone.")
            .default(false)
            .interact()?;

        if !confirmed {
            println!("{}", "Cancelled.".dimmed());
            return Ok(());
        }
    }

    // 1. Bring WireGuard down
    if !json_output {
        println!("{}", "  Disconnecting WireGuard...".dimmed());
    }
    let wg_path = get_wg_config_path();
    if wg_path.exists() {
        let _ = wg_down(&wg_path);
    }

    // 2. Remove system WireGuard config (macOS: check both paths)
    let wg_dirs: Vec<&str> = if cfg!(target_os = "macos") {
        vec!["/etc/wireguard", "/usr/local/etc/wireguard"]
    } else {
        vec!["/etc/wireguard"]
    };
    if !json_output {
        println!("{}", "  Removing system WireGuard config...".dimmed());
    }
    for wg_dir in wg_dirs {
        let system_conf = format!("{wg_dir}/{WG_INTERFACE}.conf");
        let _ = std::process::Command::new("sudo")
            .args(["rm", "-f", &system_conf])
            .output();
    }

    // 3. Remove ~/.nordkraft/
    let config_dir = get_config_dir();
    if config_dir.exists() {
        if !json_output {
            println!(
                "{}",
                format!("  Removing {}...", config_dir.display()).dimmed()
            );
        }
        std::fs::remove_dir_all(&config_dir)?;
    }

    if !json_output {
        println!();
        println!("{}", "✅ Reset complete.".green().bold());
        println!();
        println!("{}", "To set up again:".dimmed());
        println!("   1. Get a new invite token from cloud.nordkraft.io");
        println!("   2. Run: nordkraft setup NKINVITE-...");
    } else {
        println!("{}", serde_json::json!({"status": "reset_complete"}));
    }

    Ok(())
}

fn show_help() {
    println!("{}", "🚀 Nordkraft Garage Cloud CLI".cyan().bold());
    println!("{}", "   Secure Container Hosting from Denmark".dimmed());
    println!();

    println!("{}", "QUICK COMMANDS:".yellow().bold());
    println!("   nordkraft deploy <image>              Deploy a container");
    println!("   nordkraft list                        List your containers");
    println!("   nordkraft logs <name>                 View container logs");
    println!("   nordkraft stop <name>                 Stop a container");
    println!("   nordkraft rm <name>                   Remove a container");
    println!();

    println!("{}", "AUTHENTICATION:".yellow().bold());
    println!("   nordkraft auth login                  Verify VPN connection");
    println!("   nordkraft auth status                 Quick connection check");
    println!();

    println!("{}", "CONTAINERS:".yellow().bold());
    println!("   nordkraft container list              List containers");
    println!("   nordkraft container deploy <image>    Deploy with options");
    println!("   nordkraft container start <name>      Start stopped container");
    println!("   nordkraft container stop <name>       Stop container");
    println!("   nordkraft container rm <name>         Remove container");
    println!("   nordkraft container logs <name>       View logs");
    println!("   nordkraft container inspect <name>    Show details");
    println!();

    println!("{}", "INGRESS (HTTPS):".yellow().bold());
    println!(
        "   nordkraft ingress enable <name> -s myapp    Enable HTTPS at myapp.nordkraft.cloud"
    );
    println!("   nordkraft ingress disable <name>            Disable ingress");
    println!("   nordkraft ingress status <name>             Show ingress status");
    println!("   nordkraft ingress list                      List all routes");
    println!();

    println!("{}", "CUSTOM DOMAINS (your own domain):".yellow().bold());
    println!(
        "   nordkraft domain add customer1.net -c <name>  Use your own domain for a container"
    );
    println!("   nordkraft domain status customer1.net         Show verification/TLS progress");
    println!("   nordkraft domain verify customer1.net         Re-check DNS records now");
    println!("   nordkraft domain list                         List your custom domains");
    println!("   nordkraft domain remove customer1.net         Remove domain + certificate");
    println!();

    println!("{}", "IPV6 DIRECT ACCESS:".yellow().bold());
    println!("   nordkraft ipv6 open <name>            Open firewall for internet access");
    println!("   nordkraft ipv6 close <name>           Close firewall");
    println!("   nordkraft ipv6 status <name>          Show IPv6 status");
    println!("   nordkraft ipv6 list                   List all IPv6 allocations");
    println!();

    println!("{}", "EXAMPLES:".yellow().bold());
    println!("   {} Deploy nginx with IPv6:", "→".dimmed());
    println!("     nordkraft deploy nginx:alpine --ipv6");
    println!();
    println!("   {} Deploy with environment variables:", "→".dimmed());
    println!("     nordkraft deploy myapp:latest -e DB_HOST=db.example.com -e DEBUG=true");
    println!();
    println!("   {} Deploy with env file:", "→".dimmed());
    println!("     nordkraft deploy myapp:latest --env-file .env");
    println!();
    println!("   {} Deploy from .nk spec:", "→".dimmed());
    println!("     nordkraft deploy --from my-campfire");
    println!("     nordkraft deploy --from ./custom-spec.nk");
    println!();
    println!("   {} Enable HTTPS ingress:", "→".dimmed());
    println!("     nordkraft ingress enable app-abc123 --subdomain myapp");
    println!("     # Access at https://myapp.nordkraft.cloud");
    println!();
    println!("   {} Open IPv6 to internet:", "→".dimmed());
    println!("     nordkraft ipv6 open app-abc123");
    println!("     # Access at http://[2a05:f6c3:...]/");
    println!();

    println!("{}", "ALIASES:".yellow().bold());
    println!("   nordkraft alias set <name> <container>  Create short alias");
    println!("   nordkraft alias list                    List all aliases");
    println!("   nordkraft alias rm <name>               Remove alias");
    println!();

    println!("{}", "DEPLOYMENT SPECS (.nk):".yellow().bold());
    println!("   nordkraft specs                         List saved deployment specs");
    println!("   nordkraft init <name>                   Generate .nk from running container");
    println!("   nordkraft diff <name>                   Compare .nk spec vs live container");
    println!("   nordkraft edit <name>                   Edit .nk in $EDITOR");
    println!("   nordkraft upgrade <name>                Apply .nk changes to container");
    println!();

    println!("{}", "PRIVATE REGISTRY:".yellow().bold());
    println!("   nordkraft registry init                 Set up private image registry");
    println!("   nordkraft push myapp:v1                 Push image to registry");
    println!("   nordkraft registry list                 List stored images");
    println!("   nordkraft registry status               Show registry status");
    println!("   nordkraft registry destroy              Remove registry");
    println!();

    println!("{}", "UPDATES:".yellow().bold());
    println!("   nordkraft update                        Update to latest version");
    println!("   nordkraft update --check                Check for updates");
    println!();

    println!("{}", "CONNECTION:".yellow().bold());
    println!("   nordkraft setup <token>                 First-time setup with invite token");
    println!("   nordkraft connect                       Connect WireGuard VPN");
    println!("   nordkraft disconnect                    Disconnect WireGuard VPN");
    println!("   nordkraft reset                         Full cleanup (remove all local config)");
    println!();

    println!("{}", "ACCOUNT:".yellow().bold());
    println!("   nordkraft account status                Show account and pending deletion");
    println!(
        "   nordkraft account delete                Delete account (code by email, 14-day grace)"
    );
    println!("   nordkraft account delete --now          Same, without the grace period");
    println!("   nordkraft account restore               Cancel a pending deletion");
    println!();

    println!("{}", "JSON OUTPUT:".yellow().bold());
    println!("   nordkraft --json list                 Output as JSON (for scripting)");
    println!();

    println!("{}", "MORE INFO:".dimmed());
    println!("   https://docs.nordkraft.io");
}
