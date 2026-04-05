use clap::{Args, Parser, Subcommand};
use colored::*;
use console::Term;
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Select};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;

mod tui;

// ============= ALIAS MANAGEMENT =============

fn get_config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(CONFIG_DIR)
}

fn load_aliases() -> HashMap<String, String> {
    let path = get_config_dir().join(ALIASES_FILE);
    if path.exists() {
        if let Ok(contents) = std::fs::read_to_string(&path) {
            if let Ok(aliases) = serde_json::from_str(&contents) {
                return aliases;
            }
        }
    }
    HashMap::new()
}

fn save_aliases(aliases: &HashMap<String, String>) -> Result<(), Box<dyn std::error::Error>> {
    let dir = get_config_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(ALIASES_FILE);
    std::fs::write(path, serde_json::to_string_pretty(aliases)?)?;
    Ok(())
}

fn resolve_alias(name: &str) -> String {
    let aliases = load_aliases();
    aliases
        .get(name)
        .cloned()
        .unwrap_or_else(|| name.to_string())
}

fn set_alias(alias: &str, container_name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut aliases = load_aliases();
    aliases.insert(alias.to_string(), container_name.to_string());
    save_aliases(&aliases)?;
    Ok(())
}

fn remove_alias(alias: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut aliases = load_aliases();
    aliases.remove(alias);
    save_aliases(&aliases)?;
    Ok(())
}

// ============= CLI STRUCTURE =============

#[derive(Parser)]
#[command(name = "nordkraft")]
#[command(author = "Nordkraft.io")]
#[command(version = env!("CARGO_PKG_VERSION"))]
#[command(about = "🚀 Nordkraft Garage Cloud CLI - Secure Container Hosting")]
#[command(long_about = None)]
struct Cli {
    /// Output as JSON (for scripting)
    #[arg(long, global = true)]
    json: bool,

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
    /// IPv6 direct access (global addresses)
    Ipv6 {
        #[command(subcommand)]
        command: Ipv6Commands,
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
    Deploy(DeployArgs),
    /// Push a local image to your private registry
    Push {
        /// Image to push (e.g., myapp:v1)
        image: String,
    },
    /// List containers (shortcut for 'container list')
    #[command(alias = "ls")]
    List,
    /// Get container logs (shortcut for 'container logs')
    Logs {
        /// Container name, alias, or omit for interactive selection
        container: Option<String>,
        /// Number of lines
        #[arg(short = 'n', long, default_value = "100")]
        lines: usize,
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
    },
    /// Remove a container (shortcut for 'container rm')
    #[command(alias = "rm")]
    Remove {
        /// Container name, alias, or omit for interactive selection
        container: Option<String>,
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
    },
    /// Open .nk spec in $EDITOR
    Edit {
        /// Container name, alias, or omit for interactive selection
        container: Option<String>,
    },
    /// List all saved deployment specs
    Specs,
    /// Show plan usage vs. limits
    Usage,
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

// ============= CONTAINER COMMANDS =============

#[derive(Subcommand)]
enum ContainerCommands {
    /// List your containers
    #[command(alias = "ls")]
    List,
    /// Deploy a new container
    Deploy(Box<DeployArgs>),
    /// Start a stopped container
    Start {
        /// Container name, alias, or omit for interactive selection
        container: Option<String>,
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

const REGISTRY_CONFIG_FILE: &str = "registry.json";
const REGISTRY_IMAGE: &str = "ghcr.io/ft-karlsson/oci-registry:0.1.0";

#[derive(Debug, Deserialize, Serialize)]
struct RegistryConfig {
    address: String,
    container_name: String,
    container_alias: Option<String>,
}

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

fn load_registry_config() -> Option<RegistryConfig> {
    let path = get_config_dir().join(REGISTRY_CONFIG_FILE);
    if path.exists() {
        if let Ok(contents) = std::fs::read_to_string(&path) {
            if let Ok(config) = serde_json::from_str(&contents) {
                return Some(config);
            }
        }
    }
    None
}

fn save_registry_config(config: &RegistryConfig) -> Result<(), Box<dyn std::error::Error>> {
    let dir = get_config_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(REGISTRY_CONFIG_FILE);
    std::fs::write(path, serde_json::to_string_pretty(config)?)?;
    Ok(())
}

fn remove_registry_config() {
    let path = get_config_dir().join(REGISTRY_CONFIG_FILE);
    let _ = std::fs::remove_file(path);
}

// ============= DEPLOYMENT SPEC (.nk TOML) =============

#[derive(Debug, Serialize, Deserialize, Clone)]
struct DeploymentSpec {
    deployment: DeploymentMeta,
    resources: ResourceSpec,
    network: NetworkSpec,
    #[serde(default, skip_serializing_if = "StorageSpec::is_empty")]
    storage: StorageSpec,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    env: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "PlacementSpec::is_empty")]
    placement: PlacementSpec,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct DeploymentMeta {
    name: String,
    image: String,
    #[serde(default)]
    revision: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    created: String,
    updated: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ResourceSpec {
    cpu: f32,
    memory: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct NetworkSpec {
    #[serde(default)]
    ports: Vec<u16>,
    #[serde(default)]
    ipv6: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
struct StorageSpec {
    #[serde(default)]
    persistence: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    volume_path: Option<String>,
    #[serde(default = "default_volume_size")]
    volume_size: String,
}

fn default_volume_size() -> String {
    "1g".to_string()
}

impl StorageSpec {
    fn is_empty(&self) -> bool {
        !self.persistence && self.volume_path.is_none()
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
struct PlacementSpec {
    #[serde(skip_serializing_if = "Option::is_none")]
    garage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hardware: Option<String>,
}

impl PlacementSpec {
    fn is_empty(&self) -> bool {
        self.garage.is_none() && self.hardware.is_none()
    }
}

#[derive(Debug)]
struct SpecDiff {
    field: String,
    spec_value: String,
    live_value: String,
    kind: DiffKind,
}

#[derive(Debug, PartialEq)]
enum DiffKind {
    Changed,
    Added,   // in spec but not live
    Removed, // in live but not spec
    Same,
}

fn get_deployments_dir() -> PathBuf {
    get_config_dir().join(DEPLOYMENTS_DIR)
}

fn nk_path(name: &str) -> PathBuf {
    get_deployments_dir().join(format!("{}.nk", name))
}

fn save_deployment_spec(spec: &DeploymentSpec) -> Result<(), Box<dyn std::error::Error>> {
    let dir = get_deployments_dir();
    std::fs::create_dir_all(&dir)?;
    let path = nk_path(&spec.deployment.name);
    let toml_str = toml::to_string_pretty(spec)?;
    let header = format!(
        "# NordKraft deployment spec — auto-generated\n# Edit and run: nordkraft upgrade {}\n\n",
        spec.deployment.name
    );
    std::fs::write(&path, format!("{}{}", header, toml_str))?;
    Ok(())
}

fn load_deployment_spec(name: &str) -> Option<DeploymentSpec> {
    let path = nk_path(name);
    if !path.exists() {
        return None;
    }
    let contents = std::fs::read_to_string(&path).ok()?;
    toml::from_str(&contents).ok()
}

fn list_deployment_specs() -> Vec<String> {
    let dir = get_deployments_dir();
    if !dir.exists() {
        return vec![];
    }
    std::fs::read_dir(&dir)
        .ok()
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    name.strip_suffix(".nk").map(|s| s.to_string())
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Resolve a --from argument to a DeploymentSpec.
/// Accepts:
///   - Bare name: "my-campfire" → ~/.nordkraft/deployments/my-campfire.nk
///   - Path with extension: "./my-campfire.nk" or "/abs/path/spec.nk"
///   - Name with extension: "my-campfire.nk" → tries as path first, then deployments dir
fn resolve_and_load_spec(from: &str) -> Result<DeploymentSpec, Box<dyn std::error::Error>> {
    let path = PathBuf::from(from);

    // 1. If it's an existing file path, use it directly
    if path.exists() {
        let contents = std::fs::read_to_string(&path)?;
        let spec: DeploymentSpec =
            toml::from_str(&contents).map_err(|e| format!("Invalid .nk spec '{}': {}", from, e))?;
        return Ok(spec);
    }

    // 2. Try as a name in the deployments dir
    let bare_name = from.strip_suffix(".nk").unwrap_or(from);
    if let Some(spec) = load_deployment_spec(bare_name) {
        return Ok(spec);
    }

    // 3. Nothing found
    Err(format!(
        "Spec '{}' not found. Tried:\n  • {}\n  • {}\n\nAvailable specs: {}",
        from,
        path.display(),
        nk_path(bare_name).display(),
        {
            let specs = list_deployment_specs();
            if specs.is_empty() {
                "(none)".to_string()
            } else {
                specs.join(", ")
            }
        }
    )
    .into())
}

fn spec_from_deploy_args(
    args: &DeployArgs,
    env_vars: &HashMap<String, String>,
    container_name: &str,
    garage: Option<&str>,
) -> DeploymentSpec {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    DeploymentSpec {
        deployment: DeploymentMeta {
            name: container_name.to_string(),
            image: args.image.clone(),
            revision: 1,
            command: args.command.clone(),
            created: now.clone(),
            updated: now,
        },
        resources: ResourceSpec {
            cpu: args.cpu,
            memory: args.memory.clone(),
        },
        network: NetworkSpec {
            ports: args.port.clone(),
            ipv6: args.ipv6,
        },
        storage: StorageSpec {
            persistence: args.persistence,
            volume_path: args.volume_path.clone(),
            volume_size: args.volume_size.clone(),
        },
        env: env_vars.clone(),
        placement: PlacementSpec {
            garage: garage
                .map(|s| s.to_string())
                .or_else(|| args.garage.clone()),
            hardware: args.hardware.clone(),
        },
    }
}

fn spec_from_inspect(c: &ContainerInspectResponse) -> DeploymentSpec {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    // Parse ports from inspect JSON
    let ports: Vec<u16> = c
        .ports
        .iter()
        .filter_map(|p| p["port"].as_u64().map(|v| v as u16))
        .collect();

    // Parse env vars (skip internal NK_ vars, HOME, PATH)
    let env: HashMap<String, String> = c
        .env_vars
        .iter()
        .filter(|e| !e.starts_with("NK_") && !e.starts_with("HOME=") && !e.starts_with("PATH="))
        .filter_map(|e| {
            e.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect();

    // Parse memory from bytes to human string
    let memory = c
        .memory_limit
        .map(|bytes| {
            if bytes >= 1024 * 1024 * 1024 {
                format!("{}g", bytes / 1024 / 1024 / 1024)
            } else {
                format!("{}m", bytes / 1024 / 1024)
            }
        })
        .unwrap_or_else(|| "512m".to_string());

    // Detect volume_path from real mounts
    let volume_path: Option<String> = c
        .volume_mounts
        .iter()
        .find(|m| !m.starts_with("tmpfs:"))
        .and_then(|m| {
            // Format is usually "host_path:container_path" or just container_path
            m.split(':')
                .nth(1)
                .or(Some(m.as_str()))
                .map(|s| s.to_string())
        });

    let persistence = !c
        .volume_mounts
        .iter()
        .filter(|m| !m.starts_with("tmpfs:"))
        .collect::<Vec<_>>()
        .is_empty();

    DeploymentSpec {
        deployment: DeploymentMeta {
            name: c.name.clone(),
            image: normalize_image(&c.image),
            revision: 0, // init from running = revision 0
            command: if c.command.is_empty() {
                None
            } else {
                Some(c.command.join(" "))
            },
            created: c.created_at.clone(),
            updated: now,
        },
        resources: ResourceSpec {
            cpu: c.cpu_limit.map(|v| v as f32).unwrap_or(0.5),
            memory,
        },
        network: NetworkSpec {
            ports,
            ipv6: c.ipv6_enabled,
        },
        storage: StorageSpec {
            persistence,
            volume_path,
            volume_size: "1g".to_string(), // default — not available from inspect
        },
        env,
        placement: PlacementSpec {
            garage: None, // not available from inspect
            hardware: None,
        },
    }
}

/// Normalize Docker image names for comparison.
/// containerd resolves short names: nginx:alpine → docker.io/library/nginx:alpine
/// This strips the prefix so both sides compare equal.
fn normalize_image(image: &str) -> String {
    let s = image
        .strip_prefix("docker.io/library/")
        .or_else(|| image.strip_prefix("docker.io/"))
        .unwrap_or(image);
    s.to_string()
}

/// Resolve `registry://name:tag` to the user's private registry address.
/// Returns the resolved image string, or the original if not a registry:// reference.
fn resolve_registry_image(image: &str) -> Result<String, Box<dyn std::error::Error>> {
    if !image.starts_with("registry://") {
        return Ok(image.to_string());
    }
    let short_name = image.strip_prefix("registry://").unwrap();
    let reg_config = load_registry_config()
        .ok_or("Registry not initialized. Run 'nordkraft registry init' first.")?;
    Ok(format!("{}/{}", reg_config.address, short_name))
}

fn compute_diff(spec: &DeploymentSpec, live: &DeploymentSpec) -> Vec<SpecDiff> {
    let mut diffs = Vec::new();

    // Image (normalize docker.io/library/ prefix)
    let spec_image = normalize_image(&spec.deployment.image);
    let live_image = normalize_image(&live.deployment.image);
    diffs.push(SpecDiff {
        field: "image".to_string(),
        spec_value: spec.deployment.image.clone(),
        live_value: live.deployment.image.clone(),
        kind: if spec_image == live_image {
            DiffKind::Same
        } else {
            DiffKind::Changed
        },
    });

    // CPU
    let spec_cpu = format!("{}", spec.resources.cpu);
    let live_cpu = format!("{}", live.resources.cpu);
    diffs.push(SpecDiff {
        field: "cpu".to_string(),
        spec_value: spec_cpu.clone(),
        live_value: live_cpu.clone(),
        kind: if spec_cpu == live_cpu {
            DiffKind::Same
        } else {
            DiffKind::Changed
        },
    });

    // Memory
    diffs.push(SpecDiff {
        field: "memory".to_string(),
        spec_value: spec.resources.memory.clone(),
        live_value: live.resources.memory.clone(),
        kind: if spec.resources.memory == live.resources.memory {
            DiffKind::Same
        } else {
            DiffKind::Changed
        },
    });

    // Ports
    let spec_ports = format!("{:?}", spec.network.ports);
    let live_ports = format!("{:?}", live.network.ports);
    diffs.push(SpecDiff {
        field: "ports".to_string(),
        spec_value: spec_ports.clone(),
        live_value: live_ports.clone(),
        kind: if spec_ports == live_ports {
            DiffKind::Same
        } else {
            DiffKind::Changed
        },
    });

    // IPv6
    if spec.network.ipv6 != live.network.ipv6 {
        diffs.push(SpecDiff {
            field: "ipv6".to_string(),
            spec_value: spec.network.ipv6.to_string(),
            live_value: live.network.ipv6.to_string(),
            kind: DiffKind::Changed,
        });
    }

    // Command
    if spec.deployment.command != live.deployment.command {
        diffs.push(SpecDiff {
            field: "command".to_string(),
            spec_value: spec.deployment.command.clone().unwrap_or_default(),
            live_value: live.deployment.command.clone().unwrap_or_default(),
            kind: DiffKind::Changed,
        });
    }

    // Env vars — compare both directions
    let mut all_keys: Vec<String> = spec.env.keys().chain(live.env.keys()).cloned().collect();
    all_keys.sort();
    all_keys.dedup();

    for key in &all_keys {
        let in_spec = spec.env.get(key);
        let in_live = live.env.get(key);
        match (in_spec, in_live) {
            (Some(sv), Some(lv)) if sv == lv => {} // same, skip
            (Some(sv), Some(lv)) => {
                diffs.push(SpecDiff {
                    field: format!("env.{}", key),
                    spec_value: sv.clone(),
                    live_value: lv.clone(),
                    kind: DiffKind::Changed,
                });
            }
            (Some(sv), None) => {
                diffs.push(SpecDiff {
                    field: format!("env.{}", key),
                    spec_value: sv.clone(),
                    live_value: String::new(),
                    kind: DiffKind::Added,
                });
            }
            (None, Some(lv)) => {
                diffs.push(SpecDiff {
                    field: format!("env.{}", key),
                    spec_value: String::new(),
                    live_value: lv.clone(),
                    kind: DiffKind::Removed,
                });
            }
            (None, None) => {}
        }
    }

    diffs
}

fn display_diff(diffs: &[SpecDiff], name: &str) {
    let changes: Vec<&SpecDiff> = diffs.iter().filter(|d| d.kind != DiffKind::Same).collect();

    if changes.is_empty() {
        println!(
            "{}",
            format!("✅ {} — spec matches live container", name)
                .green()
                .bold()
        );
        return;
    }

    println!(
        "{}",
        format!("🔍 Diff: {}.nk ↔ live container", name)
            .cyan()
            .bold()
    );
    println!();

    for d in diffs {
        let label = format!("   {:<14}", d.field);
        match d.kind {
            DiffKind::Same => {
                println!(
                    "{}{}   {}",
                    label.dimmed(),
                    d.spec_value.dimmed(),
                    "(unchanged)".dimmed()
                );
            }
            DiffKind::Changed => {
                println!(
                    "{}{}  →  {}   {}",
                    label.white(),
                    d.live_value.red(),
                    d.spec_value.green(),
                    "⚠ changed".yellow()
                );
            }
            DiffKind::Added => {
                println!(
                    "{}{}   {}",
                    label.white(),
                    format!("+ {}", d.spec_value).green(),
                    "(in spec, not live)".dimmed()
                );
            }
            DiffKind::Removed => {
                println!(
                    "{}{}   {}",
                    label.white(),
                    format!("- {}", d.live_value).red(),
                    "(in live, not spec)".dimmed()
                );
            }
        }
    }

    let changed = changes
        .iter()
        .filter(|d| d.kind == DiffKind::Changed)
        .count();
    let added = changes.iter().filter(|d| d.kind == DiffKind::Added).count();
    let removed = changes
        .iter()
        .filter(|d| d.kind == DiffKind::Removed)
        .count();
    println!();

    let mut parts = Vec::new();
    if changed > 0 {
        parts.push(format!("{} changed", changed));
    }
    if added > 0 {
        parts.push(format!("{} added", added));
    }
    if removed > 0 {
        parts.push(format!("{} removed", removed));
    }
    println!(
        "   {}. Run '{}' to apply.",
        parts.join(", "),
        format!("nordkraft upgrade {}", name).cyan()
    );
}

// ============= API TYPES =============

const CONFIG_DIR: &str = ".nordkraft";
const ALIASES_FILE: &str = "aliases.json";
const DEPLOYMENTS_DIR: &str = "deployments";
const WG_CONFIG_FILE: &str = "wg.conf";
const WG_INTERFACE: &str = "nordkraft";
const CONNECTION_FILE: &str = "connection.json";

pub(crate) static API_BASE_URL: LazyLock<String> = LazyLock::new(|| {
    // 1. Env var override (dev/testing)
    if let Ok(url) = std::env::var("API_BASE_URL") {
        return url;
    }

    // 2. From connection config if set
    if let Some(config) = load_connection_config() {
        if let Some(endpoint) = config.api_endpoint {
            if !endpoint.is_empty() {
                return endpoint;
            }
        }
    }

    // 3. Default — matches open source controller default
    "http://172.20.0.254:8001/api".to_string()
});

static PUBLIC_API_URL: LazyLock<String> = LazyLock::new(|| {
    // 1. Env var override
    if let Ok(url) = std::env::var("PUBLIC_API_URL") {
        return url;
    }

    // 2. From connection config if set (for re-setup / self-hosted)
    if let Some(config) = load_connection_config() {
        if let Some(url) = config.public_api_url {
            if !url.is_empty() {
                return url;
            }
        }
    }

    // 3. Default — NordKraft.io cloud
    "https://cloud.nordkraft.io/api".to_string()
});

// Persisted connection info (written by setup, read by connect/disconnect)
#[derive(Debug, Deserialize, Serialize)]
struct ConnectionConfig {
    user_id: String,
    full_name: String,
    email: String,
    plan_id: String,
    assigned_garage: String,
    wireguard_ip: String,
    server_public_key: String,
    server_endpoint: String,
    allowed_ips: Vec<String>,
    /// Controller API endpoint via WireGuard. Default: http://172.20.0.254:8001/api
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api_endpoint: Option<String>,
    /// Public signup API endpoint. Default: https://cloud.nordkraft.io/api
    #[serde(default, skip_serializing_if = "Option::is_none")]
    public_api_url: Option<String>,
}

// Claim API types
#[derive(Debug, Serialize)]
struct ClaimRequest {
    token: String,
    wireguard_public_key: String,
}

#[derive(Debug, Deserialize)]
struct ClaimApiResponse {
    success: bool,
    data: Option<ClaimData>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ClaimData {
    wireguard_ip: String,
    server_public_key: String,
    server_endpoint: String,
    allowed_ips: Vec<String>,
    user_id: String,
    full_name: String,
    email: String,
    plan_id: String,
    assigned_garage: String,
}

// Auth
#[derive(Debug, Deserialize, Serialize)]
struct AuthResponse {
    authenticated: bool,
    user: Option<AuthUser>,
}

#[derive(Debug, Deserialize, Serialize)]
struct AuthUser {
    id: String,
    full_name: String,
    email: String,
    wireguard_ip: String,
    wireguard_public_key: String,
    plan_id: String,
    account_status: String,
    #[serde(default)]
    allowed_actions: Vec<String>,
    primary_garage_id: Option<String>,
    user_slot: Option<u32>,
}

// Containers
#[derive(Debug, Deserialize, Serialize)]
struct ContainerInfo {
    container_id: String,
    name: String,
    image: String,
    status: String,
    #[serde(default)]
    status_message: Option<String>,
    pod_id: Option<String>,
    created_at: String,
    #[serde(default)]
    ports: Vec<PortInfo>,
    container_ip: Option<String>,
    ipv6_address: Option<String>,
    #[serde(default)]
    ipv6_enabled: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(untagged)]
enum PortInfo {
    Simple(String),
    Detailed {
        port: u16,
        protocol: Option<String>,
        access_url: Option<String>,
        ipv6_url: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
struct ContainerListResponse {
    containers: Vec<ContainerInfo>,
    // #[serde(default)]
    // source: Option<String>,
}

#[derive(Debug, Serialize)]
struct DeployRequest {
    image: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    ports: Vec<PortSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<Vec<String>>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    env_vars: HashMap<String, String>,
    cpu_limit: f32,
    memory_limit: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    enable_persistence: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    volume_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    volume_size: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    enable_ipv6: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_garage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hardware_preference: Option<String>,
}

#[derive(Debug, Serialize)]
struct PortSpec {
    port: u16,
    protocol: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct DeployResponse {
    status: String,
    container_name: Option<String>,
    container_ip: Option<String>,
    ipv6_address: Option<String>,
    #[serde(default)]
    ipv6_enabled: bool,
    garage: Option<String>,
    node: Option<String>,
    message: Option<String>,
    #[serde(default)]
    ipv6_urls: Vec<String>,
}

// Upgrade
#[derive(Debug, Serialize)]
struct CliUpgradeRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ports: Option<Vec<PortSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    env_vars: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cpu_limit: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    memory_limit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    volume_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    volume_size: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct UpgradeResponse {
    #[serde(default)]
    status: Option<String>,
    container_name: Option<String>,
    container_ip: Option<String>,
    node: Option<String>,
    image: Option<String>,
    revision_old: Option<u32>,
    revision: Option<u32>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

// Ingress
#[derive(Debug, Serialize)]
struct IngressEnableRequest {
    subdomain: String,
    mode: String,
    target_port: u16,
}

#[derive(Debug, Deserialize, Serialize)]
struct IngressEnableResponse {
    status: String,
    url: Option<String>,
    subdomain: Option<String>,
    domain: Option<String>,
    ssl: Option<bool>,
    certificate: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct IngressStatusResponse {
    enabled: bool,
    subdomain: Option<String>,
    url: Option<String>,
    target_port: Option<u16>,
    ssl: Option<bool>,
    certificate_status: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct IngressListResponse {
    routes: Vec<IngressRoute>,
    count: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize)]
struct IngressRoute {
    container_id: String,
    subdomain: String,
    url: String,
    target_port: u16,
    #[serde(default)]
    is_active: bool,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    target_ip: Option<String>,
    #[serde(default)]
    firewall_open: Option<bool>,
    #[serde(default)]
    public_port: Option<u16>,
    #[serde(default)]
    created_at: Option<String>,
}

// IPv6
#[derive(Debug, Deserialize, Serialize)]
struct Ipv6OpenResponse {
    status: String,
    ipv6_address: Option<String>,
    #[serde(default)]
    ports: Vec<u16>,
    rule_id: Option<String>,
    #[serde(default)]
    access_urls: Vec<String>,
    message: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Ipv6StatusResponse {
    container_name: Option<String>,
    ipv6_address: Option<String>,
    #[serde(default)]
    exposed_ports: Vec<u16>,
    firewall_status: Option<String>,
    rule_id: Option<String>,
    pfsense_configured: Option<bool>,
    #[serde(default)]
    access_urls: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Ipv6ListResponse {
    allocations: Vec<Ipv6Allocation>,
    count: Option<u32>,
    pfsense_configured: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Ipv6Allocation {
    container_name: String,
    ipv6_address: String,
    #[serde(default)]
    exposed_ports: Vec<u16>,
    firewall_status: String,
    rule_id: Option<String>,
    #[serde(default)]
    access_urls: Vec<String>,
}

#[derive(Debug, Serialize)]
struct Ipv6PortsRequest {
    ports: Vec<u16>,
}

// Network
#[derive(Debug, Deserialize, Serialize)]
struct NetworkInfoResponse {
    garage: Option<String>,
    container_subnet: Option<String>,
    user_ip: Option<String>,
}

// Nodes
#[derive(Debug, Deserialize, Serialize)]
struct NodesResponse {
    nodes: Vec<NodeInfo>,
}

#[derive(Debug, Deserialize, Serialize)]
struct NodeInfo {
    id: String,
    address: Option<String>,
    port: Option<u16>,
    status: String,
    last_heartbeat: Option<String>,
}

// Status
#[derive(Debug, Deserialize, Serialize)]
struct StatusResponse {
    status: String,
    timestamp: Option<String>,
}

// Logs
#[derive(Debug, Deserialize)]
struct LogsResponse {
    logs: String,
    #[serde(default)]
    source: Option<String>,
}

// Inspect - mirrors ContainerInspectData from server
#[derive(Debug, Deserialize, Serialize)]
struct ContainerInspectResponse {
    #[serde(default)]
    container_id: Option<String>,
    name: String,
    image: String,
    image_digest: Option<String>,
    status: String,
    created_at: String,
    started_at: Option<String>,
    finished_at: Option<String>,
    exit_code: Option<i64>,
    restart_count: Option<i64>,
    container_ip: Option<String>,
    ipv6_address: Option<String>,
    #[serde(default)]
    ipv6_enabled: bool,
    #[serde(default)]
    ports: Vec<serde_json::Value>,
    #[serde(default)]
    env_vars: Vec<String>,
    #[serde(default)]
    command: Vec<String>,
    hostname: Option<String>,
    node_id: String,
    runtime: String,
    cpu_limit: Option<f64>,
    memory_limit: Option<i64>,
    #[serde(default)]
    persistence_enabled: bool,
    #[serde(default)]
    volume_mounts: Vec<String>,
    #[serde(default)]
    labels: std::collections::HashMap<String, String>,
}

// Generic
#[derive(Debug, Deserialize, Serialize)]
struct ApiResponse {
    status: Option<String>,
    error: Option<String>,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    error: String,
    #[serde(default)]
    plan: Option<String>,
    #[serde(default)]
    plan_id: Option<String>,
    #[serde(default)]
    usage: Option<serde_json::Value>,
}

// ============= MAIN =============

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let json_output = cli.json;

    let result = match cli.command {
        Commands::Auth { command } => handle_auth(command, json_output).await,
        Commands::Container { command } => handle_container(command, json_output).await,
        Commands::Ingress { command } => handle_ingress(command, json_output).await,
        Commands::Ipv6 { command } => handle_ipv6(command, json_output).await,
        Commands::Network { command } => handle_network(command, json_output).await,
        Commands::Nodes => handle_nodes(json_output).await,
        Commands::Status => handle_status(json_output).await,
        Commands::Help => {
            show_help();
            Ok(())
        }
        Commands::Alias { command } => handle_alias(command, json_output).await,
        Commands::Update { check } => handle_update(check, json_output).await,
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
        Commands::Init { container } => handle_init_interactive(container, json_output).await,
        Commands::Edit { container } => handle_edit_interactive(container, json_output).await,
        Commands::Specs => handle_specs_list(json_output).await,
        Commands::Usage => handle_usage(json_output).await,
        Commands::Events { container, limit } => handle_events(container, limit, json_output).await,

        // Shortcuts with interactive selection
        Commands::Deploy(args) => handle_deploy(args, json_output).await,
        Commands::Push { image } => handle_registry_push(&image, json_output).await,
        Commands::List => handle_container_list(json_output).await,
        Commands::Logs { container, lines } => {
            handle_logs_interactive(container, lines, json_output).await
        }
        Commands::Stop { container } => handle_stop_interactive(container, json_output).await,
        Commands::Start { container } => handle_start_interactive(container, json_output).await,
        Commands::Remove { container } => handle_remove_interactive(container, json_output).await,
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

fn create_client() -> Result<Client, Box<dyn std::error::Error>> {
    Ok(Client::builder().timeout(Duration::from_secs(30)).build()?)
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
        ContainerCommands::List => handle_container_list(json_output).await,
        ContainerCommands::Deploy(args) => handle_deploy(*args, json_output).await,
        ContainerCommands::Start { container } => {
            let container = get_container_or_select(container, json_output).await?;
            handle_container_start(&container, json_output).await
        }
        ContainerCommands::Stop { container } => {
            let container = get_container_or_select(container, json_output).await?;
            handle_container_stop(&container, json_output).await
        }
        ContainerCommands::Restart { container } => {
            let container = get_container_or_select(container, json_output).await?;
            handle_container_stop(&container, json_output).await?;
            handle_container_start(&container, json_output).await
        }
        ContainerCommands::Remove { container } => {
            let container = get_container_or_select(container, json_output).await?;
            handle_container_remove(&container, json_output).await
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
    }
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
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let container = get_container_or_select(container, json_output).await?;
    handle_container_start(&container, json_output).await
}

async fn handle_remove_interactive(
    container: Option<String>,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let container = get_container_or_select(container, json_output).await?;
    handle_container_remove(&container, json_output).await
}

async fn handle_container_list(json_output: bool) -> Result<(), Box<dyn std::error::Error>> {
    let client = create_client()?;

    if !json_output {
        println!("{}", "📦 Fetching containers...".cyan());
    }

    let url = format!("{}/containers", *API_BASE_URL);
    let response = client.get(&url).send().await?;

    if !response.status().is_success() {
        let error: ApiError = response.json().await?;
        return Err(error.error.into());
    }

    let data: ContainerListResponse = response.json().await?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&data.containers)?);
        return Ok(());
    }

    if data.containers.is_empty() {
        println!("{}", "No containers found.".yellow());
        println!("   Deploy one: nordkraft deploy nginx:alpine");
        return Ok(());
    }

    // Load aliases for display
    let aliases = load_aliases();
    let reverse_aliases: std::collections::HashMap<&String, &String> = aliases
        .iter()
        .map(|(alias, container_name)| (container_name, alias))
        .collect();

    println!(
        "{}",
        format!("🐳 {} container(s):", data.containers.len())
            .green()
            .bold()
    );
    println!();

    for c in &data.containers {
        let status_lower = c.status.to_lowercase();
        let (status_colored, failure_reason) =
            if status_lower.contains("up") || status_lower == "running" {
                ("Up".green().bold(), None)
            } else if status_lower == "deploying" {
                ("Deploying".yellow().bold(), None)
            } else if status_lower.starts_with("failed") {
                let reason = if status_lower.starts_with("failed: ") {
                    Some(c.status[8..].to_string())
                } else {
                    c.status_message.clone()
                };
                ("Failed".red().bold(), reason)
            } else if status_lower.contains("exited") || status_lower.contains("stopped") {
                (c.status.red(), None)
            } else {
                (c.status.yellow(), None)
            };

        // Show alias as primary name if available
        if let Some(alias) = reverse_aliases.get(&c.name) {
            println!(
                "  {} {} {}",
                "NAME:".cyan().bold(),
                alias.white().bold(),
                format!("({})", c.name).dimmed()
            );
        } else {
            println!("  {} {}", "NAME:".cyan().bold(), c.name.white().bold());
        }
        println!("    {} {}", "Image:".dimmed(), c.image);
        println!("    {} {}", "Status:".dimmed(), status_colored);

        // Show failure reason
        if let Some(reason) = failure_reason {
            let truncated: String = reason.chars().take(120).collect();
            println!("    {} {}", "Reason:".dimmed(), truncated.red());
            println!(
                "    {} {}",
                "💡".dimmed(),
                "Run 'nordkraft events' for full details".dimmed()
            );
        }

        if let Some(ip) = &c.container_ip {
            println!("    {} {}", "IPv4:".dimmed(), ip);
        }
        if let Some(ipv6) = &c.ipv6_address {
            println!("    {} {}", "IPv6:".dimmed(), ipv6.cyan());
        }
        if !c.ports.is_empty() {
            let ports_str: Vec<String> = c
                .ports
                .iter()
                .map(|p| match p {
                    PortInfo::Simple(s) => s.clone(),
                    PortInfo::Detailed { port, protocol, .. } => {
                        format!("{}/{}", port, protocol.as_deref().unwrap_or("tcp"))
                    }
                })
                .collect();
            println!("    {} {}", "Ports:".dimmed(), ports_str.join(", "));
        }
        println!("    {} {}", "Created:".dimmed(), c.created_at.dimmed());
        println!();
    }

    Ok(())
}

async fn handle_deploy(
    args: DeployArgs,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = create_client()?;

    // If --from is set, load .nk spec and build args from it.
    // CLI flags (--name, --alias) are preserved alongside the spec.
    let mut args = if let Some(from) = &args.from {
        let spec = resolve_and_load_spec(from)?;

        if !json_output {
            println!(
                "{}",
                format!("📄 Loading spec: {}", spec.deployment.name).cyan()
            );
        }

        // Build DeployArgs from spec, preserving --name and --alias from CLI
        DeployArgs {
            image: spec.deployment.image.clone(),
            from: args.from.clone(),
            port: spec.network.ports.clone(),
            env: spec
                .env
                .iter()
                .map(|(k, v)| format!("{}={}", k, v))
                .collect(),
            env_file: None,
            cpu: spec.resources.cpu,
            memory: spec.resources.memory.clone(),
            persistence: spec.storage.persistence,
            volume_path: spec.storage.volume_path.clone(),
            volume_size: spec.storage.volume_size.clone(),
            ipv6: spec.network.ipv6,
            garage: spec.placement.garage.clone(),
            hardware: spec.placement.hardware.clone(),
            // --name: use CLI value if provided, otherwise spec name
            name: args.name.or(Some(spec.deployment.name.clone())),
            alias: args.alias.clone(),
            command: spec.deployment.command.clone(),
        }
    } else if args.image.is_empty() && !json_output {
        // If no image provided and not JSON mode, offer interactive
        prompt_deploy_interactive()?
    } else if args.image.is_empty() {
        return Err(
            "Image required. Use: nordkraft deploy <image> or nordkraft deploy --from <spec.nk>"
                .into(),
        );
    } else {
        args
    };

    // Explicit private registry references: "registry://myapp:v1"
    // Resolves to the user's private registry address.
    if args.image.starts_with("registry://") {
        args.image = resolve_registry_image(&args.image)?;
        if !json_output {
            println!("   {} Using private registry: {}", "→".dimmed(), args.image);
        }
    }

    // Parse environment variables
    let mut env_vars = HashMap::new();

    // Load from file first
    if let Some(env_file) = &args.env_file {
        let contents = std::fs::read_to_string(env_file)
            .map_err(|e| format!("Cannot read env file '{}': {}", env_file, e))?;
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                env_vars.insert(key.trim().to_string(), value.trim().to_string());
            }
        }
    }

    // Override with command line args
    for env in &args.env {
        if let Some((key, value)) = env.split_once('=') {
            env_vars.insert(key.to_string(), value.to_string());
        } else {
            return Err(format!("Invalid env format '{}'. Use KEY=VALUE", env).into());
        }
    }

    // Build ports — prompt interactively if no --port flag given
    let ports: Vec<PortSpec> = if !args.port.is_empty() {
        // Explicit --port flags from user
        args.port
            .iter()
            .map(|p| PortSpec {
                port: *p,
                protocol: "tcp".to_string(),
            })
            .collect()
    } else if json_output {
        // Non-interactive mode: no default port, let server decide
        vec![]
    } else {
        // Suggest a sensible default based on image name
        let suggested: Option<u16> = {
            let img = args.image.to_lowercase();
            if img.contains("nginx")
                || img.contains("apache")
                || img.contains("caddy")
                || img.contains("traefik")
            {
                Some(80)
            } else if img.contains("node") || img.contains("express") || img.contains("next") {
                Some(3000)
            } else if img.contains("rails") || img.contains("campfire") || img.contains("ruby") {
                Some(3000)
            } else if img.contains("django")
                || img.contains("flask")
                || img.contains("fastapi")
                || img.contains("python")
            {
                Some(8000)
            } else if img.contains("spring") || img.contains("java") || img.contains("tomcat") {
                Some(8080)
            } else if img.contains("go") || img.contains("golang") {
                Some(8080)
            } else if img.contains("postgres")
                || img.contains("mysql")
                || img.contains("mongo")
                || img.contains("redis")
            {
                None // DB images — no HTTP port
            } else {
                Some(80) // generic fallback
            }
        };

        println!();
        match suggested {
            None => {
                // DB or no-port image — ask explicitly
                let expose = Confirm::with_theme(&ColorfulTheme::default())
                    .with_prompt("Does this container expose a port?")
                    .default(false)
                    .interact()
                    .unwrap_or(false);

                if expose {
                    let port_input: String = Input::with_theme(&ColorfulTheme::default())
                        .with_prompt("Port")
                        .interact_text()
                        .unwrap_or_default();
                    port_input
                        .trim()
                        .parse::<u16>()
                        .map(|p| {
                            vec![PortSpec {
                                port: p,
                                protocol: "tcp".to_string(),
                            }]
                        })
                        .unwrap_or_default()
                } else {
                    vec![]
                }
            }
            Some(default_port) => {
                let port_str: String = Input::with_theme(&ColorfulTheme::default())
                    .with_prompt("Port to expose")
                    .default(default_port.to_string())
                    .show_default(true)
                    .interact_text()
                    .unwrap_or(default_port.to_string());

                if port_str.trim() == "none" || port_str.trim().is_empty() {
                    vec![]
                } else {
                    port_str
                        .trim()
                        .parse::<u16>()
                        .map(|p| {
                            vec![PortSpec {
                                port: p,
                                protocol: "tcp".to_string(),
                            }]
                        })
                        .unwrap_or_else(|_| {
                            vec![PortSpec {
                                port: default_port,
                                protocol: "tcp".to_string(),
                            }]
                        })
                }
            }
        }
    };

    // Validate: --persistence requires --volume-path
    if args.persistence && args.volume_path.is_none() {
        if json_output {
            return Err("--persistence requires --volume-path (e.g., --volume-path /data or --volume-path /rails/storage)".into());
        }
        eprintln!(
            "{}",
            "❌ Error: --persistence requires --volume-path"
                .red()
                .bold()
        );
        eprintln!();
        eprintln!("   You must specify where inside the container to mount persistent storage.");
        eprintln!();
        eprintln!("   {}", "Common paths by image:".yellow());
        eprintln!("     Rails apps:     --volume-path /rails/storage");
        eprintln!("     PostgreSQL:     --volume-path /var/lib/postgresql/data");
        eprintln!("     MySQL:          --volume-path /var/lib/mysql");
        eprintln!("     Redis:          --volume-path /data");
        eprintln!("     Generic apps:   --volume-path /data");
        eprintln!();
        eprintln!("   {}", "Example:".yellow());
        eprintln!("     nordkraft deploy ghcr.io/basecamp/once-campfire:latest \\");
        eprintln!("       --persistence --volume-path /rails/storage");
        eprintln!();
        return Err("Missing required --volume-path".into());
    }

    // Parse command — shell-style parsing that respects quoted strings.
    // split_whitespace() breaks on spaces inside quotes, mangling e.g.:
    //   node -e "require('http').createServer(...).listen(3000)"
    let command = args.command.clone().map(|c| {
        let mut tokens: Vec<String> = Vec::new();
        let mut current = String::new();
        let mut in_quotes = false;
        let mut quote_char = ' ';
        let mut chars = c.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '"' | '\'' if !in_quotes => {
                    in_quotes = true;
                    quote_char = ch;
                }
                ch if in_quotes && ch == quote_char => {
                    in_quotes = false;
                }
                '\\' if in_quotes => {
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                }
                ' ' | '\t' if !in_quotes => {
                    if !current.is_empty() {
                        tokens.push(current.clone());
                        current.clear();
                    }
                }
                ch => current.push(ch),
            }
        }
        if !current.is_empty() {
            tokens.push(current);
        }
        tokens
    });

    let request = DeployRequest {
        image: args.image.clone(),
        ports,
        command,
        env_vars: env_vars.clone(),
        cpu_limit: args.cpu,
        memory_limit: args.memory.clone(),
        enable_persistence: args.persistence,
        volume_path: args.volume_path.clone(),
        volume_size: if args.persistence {
            Some(args.volume_size.clone())
        } else {
            None
        },
        enable_ipv6: args.ipv6,
        target_garage: args.garage.clone(),
        hardware_preference: args.hardware.clone(),
    };

    if !json_output {
        println!("{}", format!("🚀 Deploying {}...", args.image).cyan());
        if args.ipv6 {
            println!("   {} IPv6 enabled", "→".dimmed());
        }
        if args.persistence {
            println!(
                "   {} Persistent storage at {}",
                "→".dimmed(),
                args.volume_path.as_ref().unwrap().cyan()
            );
        }
    }

    let url = format!("{}/containers/deploy", *API_BASE_URL);
    let response = client.post(&url).json(&request).send().await?;

    if !response.status().is_success() {
        let error: ApiError = response.json().await?;

        if error.plan.is_some() || error.usage.is_some() {
            print_quota_error(&error);
            return Err("Quota exceeded".into());
        }

        return Err(format!("Deploy failed: {}", error.error).into());
    }

    // API returns 200 for both success and quota errors — check for error field
    let body: serde_json::Value = response.json().await?;

    if let Some(err_msg) = body.get("error").and_then(|v| v.as_str()) {
        if body.get("plan").is_some() || body.get("usage").is_some() {
            eprintln!("{}", "❌ Deploy blocked — plan quota exceeded".red().bold());
            eprintln!();
            if let Some(plan) = body.get("plan").and_then(|v| v.as_str()) {
                eprintln!("   {} {}", "Plan:".cyan(), plan.white().bold());
            }
            if let Some(usage) = body.get("usage") {
                for key in &["cpu", "memory", "disk"] {
                    if let Some(val) = usage.get(key).and_then(|v| v.as_str()) {
                        eprintln!("   {} {}", format!("{}:", key.to_uppercase()).cyan(), val);
                    }
                }
                if let Some(count) = usage.get("containers").and_then(|v| v.as_i64()) {
                    eprintln!("   {} {}", "Containers:".cyan(), count);
                }
            }
            eprintln!();
            eprintln!("   {}", err_msg.yellow());
            return Err("Quota exceeded".into());
        }
        return Err(format!("Deploy failed: {}", err_msg).into());
    }

    let result: DeployResponse = serde_json::from_value(body)?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }

    println!("{}", "✅ Container deployed!".green().bold());
    println!();
    if let Some(name) = &result.container_name {
        println!("   {} {}", "Name:".cyan(), name.white().bold());

        // If --alias was explicitly provided, set it directly (no prompt)
        if let Some(alias_value) = &args.alias {
            if !alias_value.is_empty() {
                if let Err(e) = set_alias(alias_value, name) {
                    eprintln!("   {} Failed to set alias: {}", "⚠".yellow(), e);
                } else {
                    println!(
                        "   {} Alias '{}' → '{}'",
                        "✅".green(),
                        alias_value.cyan(),
                        name
                    );
                }
            }
        }
        // If --name was provided, user already chose a name — skip alias prompt
        else if args.name.is_some() {
            // No alias prompt — user explicitly named this container
        }
        // Otherwise, offer interactive alias prompt
        else {
            let set_alias_prompt = Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt("Set a short alias for this container?")
                .default(true)
                .interact();

            if let Ok(true) = set_alias_prompt {
                let alias: String = Input::with_theme(&ColorfulTheme::default())
                    .with_prompt("Alias")
                    .interact_text()
                    .unwrap_or_default();

                if !alias.is_empty() {
                    if let Err(e) = set_alias(&alias, name) {
                        eprintln!("   {} Failed to set alias: {}", "⚠".yellow(), e);
                    } else {
                        println!("   {} Alias '{}' → '{}'", "✅".green(), alias.cyan(), name);
                    }
                }
            }
        }
    }
    if let Some(garage) = &result.garage {
        println!("   {} {}", "Garage:".cyan(), garage);
    }
    if let Some(node) = &result.node {
        println!("   {} {}", "Node:".cyan(), node);
    }
    if let Some(ip) = &result.container_ip {
        println!("   {} {}", "IPv4:".cyan(), ip);
    }
    if let Some(ipv6) = &result.ipv6_address {
        println!("   {} {}", "IPv6:".cyan(), ipv6.cyan().bold());
        println!();
        println!(
            "   {} Open firewall: nordkraft ipv6 open {}",
            "💡".yellow(),
            result.container_name.as_deref().unwrap_or("<name>")
        );
    }
    println!();
    println!(
        "   {} nordkraft logs {}",
        "View logs:".dimmed(),
        result.container_name.as_deref().unwrap_or("<name>")
    );

    // Save .nk deployment spec
    if let Some(name) = &result.container_name {
        let spec = spec_from_deploy_args(&args, &env_vars, name, result.garage.as_deref());
        match save_deployment_spec(&spec) {
            Ok(_) if !json_output => {
                println!();
                println!(
                    "   {} Saved deployment spec → {}",
                    "💾".dimmed(),
                    format!("~/.nordkraft/deployments/{}.nk", name).dimmed()
                );
            }
            Err(e) if !json_output => {
                eprintln!("   {} Could not save .nk spec: {}", "⚠".yellow(), e);
            }
            _ => {}
        }
    }

    // Prompt to inspect — verify runtime, ports, volumes are correct
    if let Some(name) = &result.container_name {
        println!();
        let inspect_prompt = Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt("Inspect container now?")
            .default(false)
            .interact();

        if let Ok(true) = inspect_prompt {
            tokio::time::sleep(tokio::time::Duration::from_millis(800)).await;
            let _ = handle_container_inspect(name, json_output).await;
        }
    }

    Ok(())
}

async fn handle_container_start(
    container: &str,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = create_client()?;
    let container = resolve_alias(container);

    if !json_output {
        println!("{}", format!("▶️  Starting {}...", container).cyan());
    }

    let url = format!("{}/containers/{}/start", *API_BASE_URL, container);
    let response = client.post(&url).send().await?;

    if !response.status().is_success() {
        let error: ApiError = response.json().await?;
        return Err(error.error.into());
    }

    let result: ApiResponse = response.json().await?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("{} Container started", "✅".green().bold());
    }

    Ok(())
}

async fn handle_container_stop(
    container: &str,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = create_client()?;
    let container = resolve_alias(container);

    if !json_output {
        println!("{}", format!("⏸️  Stopping {}...", container).cyan());
    }

    let url = format!("{}/containers/{}/stop", *API_BASE_URL, container);
    let response = client.post(&url).send().await?;

    if !response.status().is_success() {
        let error: ApiError = response.json().await?;
        return Err(error.error.into());
    }

    let result: ApiResponse = response.json().await?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("{} Container stopped", "✅".green().bold());
    }

    Ok(())
}

async fn handle_container_remove(
    container: &str,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = create_client()?;
    let container = resolve_alias(container);

    if !json_output {
        println!("{}", format!("🗑️  Removing {}...", container).cyan());
    }

    let url = format!("{}/containers/{}", *API_BASE_URL, container);
    let response = client.delete(&url).send().await?;

    if !response.status().is_success() {
        let error: ApiError = response.json().await?;
        return Err(error.error.into());
    }

    let result: ApiResponse = response.json().await?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("{} Container removed", "✅".green().bold());
    }

    Ok(())
}

async fn handle_container_logs(
    container: &str,
    lines: usize,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = create_client()?;

    // Resolve alias to full name
    let container = resolve_alias(container);

    let url = format!(
        "{}/containers/{}/logs?lines={}",
        *API_BASE_URL, container, lines
    );
    let response = client.get(&url).send().await?;

    if !response.status().is_success() {
        let error: ApiError = response.json().await?;
        return Err(error.error.into());
    }

    // Try to parse logs response — if it fails, the container may not be ready yet
    let body = response.text().await?;
    let logs_result: Result<LogsResponse, _> = serde_json::from_str(&body);

    match logs_result {
        Ok(logs) => {
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "container": container,
                        "logs": logs.logs,
                        "source": logs.source
                    }))?
                );
            } else {
                if let Some(source) = logs.source {
                    println!(
                        "{}",
                        format!("📜 Logs from {} [{}]:", container, source)
                            .cyan()
                            .dimmed()
                    );
                }
                println!("{}", logs.logs);
            }
        }
        Err(_) => {
            // Could not parse logs — check container status to give a useful message
            let status = fetch_container_status(&client, &container).await;
            match status.as_deref() {
                Some("running") => {
                    // Running but logs not ready yet (race condition right after deploy)
                    if json_output {
                        println!(
                            "{}",
                            serde_json::json!({"container": container, "status": "running", "logs": null, "message": "Container is running but logs are not available yet. Try again in a moment."})
                        );
                    } else {
                        println!(
                            "{}",
                            "⏳ Container is running but logs aren't ready yet.".yellow()
                        );
                        println!("{}", "   Try again in a moment:".dimmed());
                        println!("   nordkraft logs {}", container);
                    }
                }
                Some(s) if s == "created" || s == "starting" => {
                    if json_output {
                        println!(
                            "{}",
                            serde_json::json!({"container": container, "status": s, "message": "Container is still starting up."})
                        );
                    } else {
                        println!(
                            "{}",
                            "⏳ Container is still starting up — no logs yet.".yellow()
                        );
                        println!("{}", "   Give it a few seconds:".dimmed());
                        println!("   nordkraft logs {}", container);
                    }
                }
                Some(s) => {
                    if json_output {
                        println!(
                            "{}",
                            serde_json::json!({"container": container, "status": s, "message": "Container is not running."})
                        );
                    } else {
                        println!(
                            "{}",
                            format!("⚠️  Container status is '{}' — logs unavailable.", s).yellow()
                        );
                    }
                }
                None => {
                    // Could not fetch status either — surface raw body for debugging
                    if json_output {
                        println!(
                            "{}",
                            serde_json::json!({"container": container, "error": "Could not parse logs response", "raw": body})
                        );
                    } else {
                        println!(
                            "{}",
                            "⚠️  Could not retrieve logs — container may still be starting."
                                .yellow()
                        );
                        println!("{}", "   Try again in a moment:".dimmed());
                        println!("   nordkraft logs {}", container);
                    }
                }
            }
        }
    }

    Ok(())
}

/// Fetch just the status field for a container — used as fallback when logs parse fails.
async fn fetch_container_status(client: &Client, container: &str) -> Option<String> {
    let url = format!("{}/containers", *API_BASE_URL);
    let response = client.get(&url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let data: ContainerListResponse = response.json().await.ok()?;
    data.containers
        .into_iter()
        .find(|c| c.name == container || c.container_id == container)
        .map(|c| c.status)
}

async fn handle_container_inspect(
    container: &str,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let container = resolve_alias(container);
    let client = create_client()?;

    let url = format!("{}/containers/{}", *API_BASE_URL, container);
    let response = client.get(&url).send().await?;

    if !response.status().is_success() {
        let error: ApiError = response.json().await?;
        return Err(error.error.into());
    }

    let body = response.text().await?;

    // Try to parse as rich inspect data
    if let Ok(c) = serde_json::from_str::<ContainerInspectResponse>(&body) {
        if json_output {
            println!("{}", serde_json::to_string_pretty(&c)?);
            return Ok(());
        }

        // Status color
        let status_colored = match c.status.as_str() {
            "running" => c.status.green().bold(),
            "stopped" | "exited" => c.status.red(),
            _ => c.status.yellow(),
        };

        // Uptime from started_at — parse ISO8601, correct epoch calculation
        let uptime = c.started_at.as_ref().and_then(|s| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_secs() as i64;
            let s = s.trim_end_matches('Z');
            let parts: Vec<&str> = s.splitn(2, 'T').collect();
            if parts.len() != 2 {
                return None;
            }
            let date: Vec<u32> = parts[0].split('-').filter_map(|x| x.parse().ok()).collect();
            let time_str = parts[1].split('.').next().unwrap_or(parts[1]);
            let time: Vec<u32> = time_str.split(':').filter_map(|x| x.parse().ok()).collect();
            if date.len() < 3 || time.len() < 3 {
                return None;
            }

            let year = date[0] as i64;
            let month = date[1] as i64;
            let day = date[2] as i64;

            // Days per month (non-leap year — close enough for uptime display)
            let days_per_month: [i64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

            // Days from epoch to start of year
            let y = year - 1970;
            let leap_days = (y / 4) - (y / 100) + (y / 400);
            let mut days = y * 365 + leap_days;

            // Add days for each completed month this year
            for m in 0..(month - 1) as usize {
                days += days_per_month[m];
            }
            days += day - 1;

            let epoch_secs =
                days * 86400 + time[0] as i64 * 3600 + time[1] as i64 * 60 + time[2] as i64;

            let elapsed = now - epoch_secs;
            if elapsed < 0 {
                return None;
            }
            if elapsed < 60 {
                Some(format!("{}s", elapsed))
            } else if elapsed < 3600 {
                Some(format!("{}m {}s", elapsed / 60, elapsed % 60))
            } else if elapsed < 86400 {
                Some(format!("{}h {}m", elapsed / 3600, (elapsed % 3600) / 60))
            } else {
                Some(format!(
                    "{}d {}h",
                    elapsed / 86400,
                    (elapsed % 86400) / 3600
                ))
            }
        });

        // Pull interesting OCI/image labels
        let restart_policy = c
            .labels
            .get("containerd.io/restart.policy")
            .map(String::as_str);
        let oci_version = c.labels.get("org.opencontainers.image.version");
        let oci_source = c.labels.get("org.opencontainers.image.source");
        let oci_revision = c.labels.get("org.opencontainers.image.revision");
        let tenant_id = c.labels.get("tenant_id");

        println!("{}", format!("🔍 {}", c.name).cyan().bold());
        println!();

        // ── Identity ──────────────────────────────────────────────
        println!(
            "   {} {}",
            "ID:".cyan(),
            c.container_id.as_deref().unwrap_or("n/a").dimmed()
        );
        println!("   {} {}", "Image:".cyan(), c.image);
        if let Some(ver) = oci_version {
            println!("   {} {}", "Version:".cyan(), ver.dimmed());
        }
        println!("   {} {}", "Status:".cyan(), status_colored);
        if let Some(up) = &uptime {
            println!("   {} {}", "Uptime:".cyan(), up.green());
        }
        println!("   {} {}", "Node:".cyan(), c.node_id);
        println!("   {} {}", "Runtime:".cyan(), c.runtime);
        if let Some(policy) = restart_policy {
            println!("   {} {}", "Restart:".cyan(), policy.dimmed());
        }
        if let Some(tid) = tenant_id {
            println!("   {} {}", "Tenant:".cyan(), tid.dimmed());
        }
        println!();

        // ── Network ───────────────────────────────────────────────
        println!("   {}", "Network:".cyan().bold());
        if let Some(ip) = &c.container_ip {
            println!("     {} {}", "IPv4:".cyan(), ip.white().bold());
        }
        if let Some(ipv6) = &c.ipv6_address {
            println!("     {} {}", "IPv6:".cyan(), ipv6.white().bold());
        }
        if let Some(h) = &c.hostname {
            println!("     {} {}", "Hostname:".cyan(), h.dimmed());
        }
        if !c.ports.is_empty() {
            for p in &c.ports {
                let port_num = p["port"].as_u64().unwrap_or(0);
                let proto = p["protocol"].as_str().unwrap_or("tcp");
                let url = p["access_url"].as_str().unwrap_or("");
                println!(
                    "     {} {}/{} → {}",
                    "Port:".cyan(),
                    port_num,
                    proto,
                    url.white()
                );
                if let Some(v6url) = p["ipv6_url"].as_str() {
                    println!("     {}      {}", "".dimmed(), v6url.dimmed());
                }
            }
        } else {
            println!(
                "     {} No ports exposed — ingress will not work",
                "⚠".yellow()
            );
            println!(
                "     {} Redeploy with {}",
                " ".dimmed(),
                "--port <port>".cyan()
            );
        }
        println!();

        // ── Resources ─────────────────────────────────────────────
        if c.cpu_limit.is_some() || c.memory_limit.is_some() {
            println!("   {}", "Resources:".cyan().bold());
            if let Some(cpu) = c.cpu_limit {
                println!("     {} {:.2} vCPU", "CPU:".cyan(), cpu);
            }
            if let Some(mem) = c.memory_limit {
                println!("     {} {} MB", "Memory:".cyan(), mem / 1024 / 1024);
            }
            println!();
        }

        // ── Timing ────────────────────────────────────────────────
        println!("   {}", "Timing:".cyan().bold());
        println!("     {} {}", "Created:".cyan(), c.created_at.dimmed());
        if let Some(s) = &c.started_at {
            println!("     {} {}", "Started:".cyan(), s.dimmed());
        }
        if let Some(f) = &c.finished_at {
            println!("     {} {}", "Stopped:".cyan(), f.dimmed());
            if let Some(code) = c.exit_code {
                let code_str = if code == 0 {
                    format!("{}", code).green()
                } else {
                    format!("{}", code).red()
                };
                println!("     {} {}", "Exit code:".cyan(), code_str);
            }
        }
        if let Some(r) = c.restart_count {
            if r > 0 {
                println!("     {} {}", "Restarts:".cyan(), r.to_string().yellow());
            }
        }

        // ── Command ───────────────────────────────────────────────
        if !c.command.is_empty() {
            println!();
            println!("   {} {}", "Command:".cyan(), c.command.join(" ").dimmed());
        }

        // ── Volumes ───────────────────────────────────────────────
        let real_mounts: Vec<&String> = c
            .volume_mounts
            .iter()
            .filter(|m| !m.starts_with("tmpfs:"))
            .collect();
        if !real_mounts.is_empty() {
            println!();
            println!("   {}", "Volumes:".cyan().bold());
            for mount in &real_mounts {
                println!("     {}", mount.dimmed());
            }
        }

        // ── Environment ───────────────────────────────────────────
        let public_env: Vec<&str> = c
            .env_vars
            .iter()
            .filter(|e| !e.starts_with("NK_") && !e.starts_with("HOME=") && !e.starts_with("PATH="))
            .map(|s| s.as_str())
            .collect();
        if !public_env.is_empty() {
            println!();
            println!("   {}", "Environment:".cyan().bold());
            for e in public_env {
                println!("     {}", e.dimmed());
            }
        }

        // ── Source ────────────────────────────────────────────────
        if oci_source.is_some() || oci_revision.is_some() {
            println!();
            println!("   {}", "Source:".cyan().bold());
            if let Some(src) = oci_source {
                println!("     {} {}", "Repo:".cyan(), src.dimmed());
            }
            if let Some(rev) = oci_revision {
                let short_rev = if rev.len() > 12 { &rev[..12] } else { rev };
                println!("     {} {}", "Commit:".cyan(), short_rev.dimmed());
            }
        }

        return Ok(());
    }

    // Fallback: if server returns old-style simple data (e.g. error or pre-update node)
    println!("{}", body);
    Ok(())
}

// ============= DECLARATIVE DEPLOYMENT HANDLERS =============

async fn handle_diff_interactive(
    container: Option<String>,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let name = match container {
        Some(c) => resolve_alias(&c),
        None => {
            let client = create_client()?;
            select_container_interactive(&client).await?
        }
    };

    // Load local spec
    let spec = match load_deployment_spec(&name) {
        Some(s) => s,
        None => {
            eprintln!("{} No .nk spec found for '{}'.", "⚠".yellow(), name);
            eprintln!(
                "   Run '{}' to generate one from the running container.",
                format!("nordkraft init {}", name).cyan()
            );
            return Ok(());
        }
    };

    // Fetch live state via inspect
    let live = match fetch_live_spec(&name).await? {
        Some(live) => live,
        None => {
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"status": "no_live_state", "message": "Container has no live state (never started or failed)"})
                );
            } else {
                println!(
                    "{} Container '{}' has no live state (never started or failed deployment).",
                    "⚠".yellow(),
                    name
                );
                println!(
                    "   Run '{}' to apply the spec.",
                    format!("nordkraft upgrade {}", name).cyan()
                );
            }
            return Ok(());
        }
    };

    if json_output {
        let diffs = compute_diff(&spec, &live);
        let changes: Vec<_> = diffs
            .iter()
            .filter(|d| d.kind != DiffKind::Same)
            .map(|d| {
                serde_json::json!({
                    "field": d.field,
                    "spec": d.spec_value,
                    "live": d.live_value,
                    "kind": format!("{:?}", d.kind).to_lowercase(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&changes)?);
    } else {
        let diffs = compute_diff(&spec, &live);
        display_diff(&diffs, &name);
    }

    Ok(())
}

async fn handle_upgrade_interactive(
    container: Option<String>,
    skip_confirm: bool,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let name = match container {
        Some(c) => resolve_alias(&c),
        None => {
            let client = create_client()?;
            select_container_interactive(&client).await?
        }
    };

    // Load local spec
    let spec = match load_deployment_spec(&name) {
        Some(s) => s,
        None => {
            return Err(format!(
                "No .nk spec found for '{}'. Run 'nordkraft init {}' first.",
                name, name
            )
            .into());
        }
    };

    // Fetch live state — may be None for failed/never-started containers
    let live = fetch_live_spec(&name).await?;
    let is_failed = live.is_none();

    // If we have live state, compute diff; otherwise skip diff and apply directly
    if let Some(ref live) = live {
        let diffs = compute_diff(&spec, live);
        let changes: Vec<&SpecDiff> = diffs.iter().filter(|d| d.kind != DiffKind::Same).collect();

        if changes.is_empty() {
            if !json_output {
                println!(
                    "{}",
                    format!("✅ {} is already up to date", name).green().bold()
                );
            }
            return Ok(());
        }

        if !json_output {
            display_diff(&diffs, &name);
            println!();
        }
    }

    if !json_output {
        if is_failed {
            println!(
                "   {} Container never started — will redeploy from spec.",
                "⚠".yellow()
            );
            println!(
                "   {}",
                format!("Image: {}", spec.deployment.image).dimmed()
            );
            println!();
        }

        if !skip_confirm {
            let prompt = if is_failed {
                "Redeploy from spec?"
            } else {
                "Apply changes?"
            };
            if !is_failed {
                println!(
                    "   {} This will restart the container. Volumes will be preserved.",
                    "⚠".yellow()
                );
            }
            let confirm = Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt(prompt)
                .default(false)
                .interact()
                .unwrap_or(false);

            if !confirm {
                println!("{}", "   Cancelled.".dimmed());
                return Ok(());
            }
        }

        println!();
        if is_failed {
            println!("{}", format!("🚀 Redeploying {}...", name).cyan());
        } else {
            println!("{}", format!("⏸️  Stopping {}...", name).cyan());
        }
    }

    // Resolve registry:// prefix in image (same as deploy path)
    let resolved_image = resolve_registry_image(&spec.deployment.image)?;
    if resolved_image != spec.deployment.image && !json_output {
        println!(
            "   {} Using private registry: {}",
            "→".dimmed(),
            resolved_image
        );
    }

    // Build upgrade request from the .nk spec (send full desired state)
    let upgrade = CliUpgradeRequest {
        image: Some(resolved_image),
        ports: if spec.network.ports.is_empty() {
            None
        } else {
            Some(
                spec.network
                    .ports
                    .iter()
                    .map(|p| PortSpec {
                        port: *p,
                        protocol: "tcp".to_string(),
                    })
                    .collect(),
            )
        },
        command: spec
            .deployment
            .command
            .as_ref()
            .map(|c| c.split_whitespace().map(|s| s.to_string()).collect()),
        env_vars: if spec.env.is_empty() {
            None
        } else {
            Some(spec.env.clone())
        },
        cpu_limit: Some(spec.resources.cpu),
        memory_limit: Some(spec.resources.memory.clone()),
        volume_path: spec.storage.volume_path.clone(),
        volume_size: Some(spec.storage.volume_size.clone()),
    };

    let client = create_client()?;
    let url = format!("{}/containers/{}/upgrade", *API_BASE_URL, name);
    let response = client.put(&url).json(&upgrade).send().await?;

    let result: UpgradeResponse = response
        .json()
        .await
        .map_err(|e| format!("Failed to parse upgrade response: {}", e))?;

    // Check for API-level errors (returned as 200 with {"error": "..."})
    if let Some(err) = &result.error {
        return Err(format!("Upgrade failed: {}", err).into());
    }

    if json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }

    // Bump revision in local .nk spec
    let mut updated_spec = spec.clone();
    updated_spec.deployment.revision = result
        .revision
        .unwrap_or(updated_spec.deployment.revision + 1);
    updated_spec.deployment.updated =
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let _ = save_deployment_spec(&updated_spec);

    println!(
        "{}",
        format!(
            "✅ Upgraded! Revision {} → {}",
            result.revision_old.unwrap_or(0),
            result.revision.unwrap_or(1)
        )
        .green()
        .bold()
    );

    if let Some(ip) = &result.container_ip {
        println!("   {} {}", "IPv4:".cyan(), ip);
    }
    if let Some(image) = &result.image {
        println!("   {} {}", "Image:".cyan(), image);
    }
    println!();
    println!(
        "   {} Updated {}",
        "💾".dimmed(),
        format!("~/.nordkraft/deployments/{}.nk", name).dimmed()
    );

    Ok(())
}

async fn handle_init_interactive(
    container: Option<String>,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let name = match container {
        Some(c) => resolve_alias(&c),
        None => {
            let client = create_client()?;
            select_container_interactive(&client).await?
        }
    };

    if !json_output {
        println!("{}", format!("🔍 Inspecting {}...", name).cyan());
    }

    let live = match fetch_live_spec(&name).await? {
        Some(live) => live,
        None => {
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"error": "Container has no live state (never started or failed)"})
                );
            } else {
                eprintln!(
                    "{} Container '{}' has no live state — cannot generate spec from a failed deployment.",
                    "⚠".yellow(), name
                );
                eprintln!(
                    "   Edit the existing .nk spec directly with '{}'.",
                    format!("nordkraft edit {}", name).cyan()
                );
            }
            return Ok(());
        }
    };

    // Check if spec already exists
    if let Some(existing) = load_deployment_spec(&name) {
        if !json_output {
            let overwrite = Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt(format!(
                    "{}.nk already exists (revision {}). Overwrite?",
                    name, existing.deployment.revision
                ))
                .default(false)
                .interact()
                .unwrap_or(false);

            if !overwrite {
                println!("{}", "   Cancelled.".dimmed());
                return Ok(());
            }
        }
    }

    save_deployment_spec(&live)?;

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "status": "ok",
                "spec": nk_path(&name).to_string_lossy(),
                "revision": 0
            }))?
        );
    } else {
        println!();
        println!(
            "   {} Generated {}",
            "💾".green(),
            format!("~/.nordkraft/deployments/{}.nk", name).cyan()
        );
        println!("   {} revision 0 (from running container)", "→".dimmed());
        println!();
        println!(
            "   Edit and run '{}' to apply changes.",
            format!("nordkraft upgrade {}", name).cyan()
        );
    }

    Ok(())
}

async fn handle_edit_interactive(
    container: Option<String>,
    _json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let name = match container {
        Some(c) => resolve_alias(&c),
        None => {
            // Try to pick from existing specs
            let specs = list_deployment_specs();
            if specs.is_empty() {
                return Err("No .nk specs found. Run 'nordkraft init <container>' first.".into());
            }
            let selection = Select::with_theme(&ColorfulTheme::default())
                .with_prompt("Select spec to edit")
                .items(&specs)
                .default(0)
                .interact()?;
            specs[selection].clone()
        }
    };

    let path = nk_path(&name);
    if !path.exists() {
        return Err(format!(
            "No .nk spec found for '{}'. Run 'nordkraft init {}' first.",
            name, name
        )
        .into());
    }

    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".to_string());

    // Read content before edit for change detection
    let before = std::fs::read_to_string(&path)?;

    let status = std::process::Command::new(&editor).arg(&path).status()?;

    if !status.success() {
        return Err(format!("{} exited with error", editor).into());
    }

    let after = std::fs::read_to_string(&path)?;

    if before == after {
        println!("{}", "   No changes made.".dimmed());
        return Ok(());
    }

    // Validate TOML
    match toml::from_str::<DeploymentSpec>(&after) {
        Ok(_) => {
            println!("{}", "📝 Spec updated.".green());
            println!();

            let show_diff = Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt("Show diff against live container?")
                .default(true)
                .interact()
                .unwrap_or(false);

            if show_diff {
                let _ = handle_diff_interactive(Some(name), false).await;
            }
        }
        Err(e) => {
            eprintln!("{} Invalid TOML after edit: {}", "⚠".yellow(), e);
            eprintln!("   The file was saved but may not be valid.");
        }
    }

    Ok(())
}

async fn handle_specs_list(json_output: bool) -> Result<(), Box<dyn std::error::Error>> {
    let specs = list_deployment_specs();

    if specs.is_empty() {
        if !json_output {
            println!("{}", "No deployment specs found.".dimmed());
            println!(
                "   Deploy a container or run '{}' to generate one.",
                "nordkraft init <container>".cyan()
            );
        }
        return Ok(());
    }

    // Build reverse alias map: container_name → alias
    let aliases = load_aliases();
    let reverse_aliases: std::collections::HashMap<&String, &String> =
        aliases.iter().map(|(alias, name)| (name, alias)).collect();

    if json_output {
        let items: Vec<serde_json::Value> = specs
            .iter()
            .filter_map(|name| {
                load_deployment_spec(name).map(|s| {
                    let mut item = serde_json::json!({
                        "name": s.deployment.name,
                        "image": s.deployment.image,
                        "revision": s.deployment.revision,
                        "updated": s.deployment.updated,
                    });
                    if let Some(alias) = reverse_aliases.get(&s.deployment.name) {
                        item["alias"] = serde_json::json!(alias);
                    }
                    item
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(());
    }

    println!("{}", "📋 Deployment specs".cyan().bold());
    println!();

    for name in &specs {
        if let Some(spec) = load_deployment_spec(name) {
            let display_name = if let Some(alias) = reverse_aliases.get(&spec.deployment.name) {
                format!("{} ({})", alias, spec.deployment.name)
            } else {
                spec.deployment.name.clone()
            };
            println!(
                "   {} {} {} {}",
                display_name.white().bold(),
                format!("r{}", spec.deployment.revision).dimmed(),
                spec.deployment.image.dimmed(),
                format!("({})", spec.resources.memory).dimmed()
            );
        } else {
            println!("   {} {}", name.white().bold(), "(invalid .nk file)".red());
        }
    }

    println!();
    println!(
        "   {} nordkraft diff <name>    {}",
        "→".dimmed(),
        "Compare spec vs live".dimmed()
    );
    println!(
        "   {} nordkraft edit <name>    {}",
        "→".dimmed(),
        "Edit spec in $EDITOR".dimmed()
    );

    Ok(())
}

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

fn print_quota_error(error: &ApiError) {
    eprintln!("{}", "❌ Deploy blocked — plan quota exceeded".red().bold());
    eprintln!();
    if let Some(plan) = &error.plan {
        eprintln!("   {} {}", "Plan:".cyan(), plan.white().bold());
    }
    if let Some(usage) = &error.usage {
        for key in &["cpu", "memory", "disk"] {
            if let Some(val) = usage.get(key).and_then(|v| v.as_str()) {
                eprintln!("   {} {}", format!("{}:", key.to_uppercase()).cyan(), val);
            }
        }
        if let Some(count) = usage.get("containers").and_then(|v| v.as_i64()) {
            eprintln!("   {} {}", "Containers:".cyan(), count);
        }
    }
    eprintln!();
    eprintln!("   {}", error.error.yellow());
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

/// Fetch live container state as a DeploymentSpec via the inspect endpoint.
/// Returns Ok(None) if the container exists in DB but has no live runtime state
/// (e.g. Failed deployment where container never started).
async fn fetch_live_spec(name: &str) -> Result<Option<DeploymentSpec>, Box<dyn std::error::Error>> {
    let client = create_client()?;
    let url = format!("{}/containers/{}", *API_BASE_URL, name);
    let response = client.get(&url).send().await?;

    if !response.status().is_success() {
        let error: ApiError = response.json().await?;
        return Err(format!("Failed to inspect '{}': {}", name, error.error).into());
    }

    let body = response.text().await?;

    // Check if the response is an error JSON (container not running / never started)
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&body) {
        if val.get("error").is_some() {
            return Ok(None);
        }
    }

    let inspect: ContainerInspectResponse = serde_json::from_str(&body)
        .map_err(|e| format!("Failed to parse inspect response: {}", e))?;

    Ok(Some(spec_from_inspect(&inspect)))
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
    }

    Ok(())
}

// ============= UPDATE HANDLER =============

const GITHUB_REPO: &str = "ft-karlsson/nordkraft-io";
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

/// If container is provided, resolve alias and return. Otherwise show interactive selection.
async fn get_container_or_select(
    container: Option<String>,
    json_output: bool,
) -> Result<String, Box<dyn std::error::Error>> {
    match container {
        Some(name) => Ok(resolve_alias(&name)),
        None => {
            if json_output {
                return Err("Container name required in JSON mode".into());
            }
            let client = create_client()?;
            select_container_interactive(&client).await
        }
    }
}

async fn select_container_interactive(
    client: &Client,
) -> Result<String, Box<dyn std::error::Error>> {
    let url = format!("{}/containers", *API_BASE_URL);
    let response = client.get(&url).send().await?;

    if !response.status().is_success() {
        return Err("Failed to fetch containers".into());
    }

    let data: ContainerListResponse = response.json().await?;

    if data.containers.is_empty() {
        return Err("No containers found. Deploy one first: nordkraft deploy nginx:alpine".into());
    }

    // Show aliases inline if they exist
    let aliases = load_aliases();
    let reverse_aliases: std::collections::HashMap<&String, &String> =
        aliases.iter().map(|(k, v)| (v, k)).collect();

    let items: Vec<String> = data
        .containers
        .iter()
        .map(|c| {
            let alias_str = reverse_aliases
                .get(&c.name)
                .map(|a| format!(" [{}]", a))
                .unwrap_or_default();
            format!("{}{} ({}) - {}", c.name, alias_str, c.status, c.image)
        })
        .collect();

    println!("{}", "📦 Select a container:".cyan());
    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Container")
        .items(&items)
        .default(0)
        .interact_on(&Term::stderr())?;

    Ok(data.containers[selection].name.clone())
}

fn prompt_deploy_interactive() -> Result<DeployArgs, Box<dyn std::error::Error>> {
    println!("{}", "🚀 Interactive Deploy".cyan().bold());
    println!();

    // Image selection
    let common_images = vec![
        "nginx:alpine",
        "httpd:alpine",
        "node:20-alpine",
        "python:3.12-alpine",
        "postgres:16-alpine",
        "redis:alpine",
        "Custom image...",
    ];

    let image_idx = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Select image")
        .items(&common_images)
        .default(0)
        .interact_on(&Term::stderr())?;

    let image = if image_idx == common_images.len() - 1 {
        Input::<String>::with_theme(&ColorfulTheme::default())
            .with_prompt("Enter custom image")
            .interact_text()?
    } else {
        common_images[image_idx].to_string()
    };

    // Port
    let port_str: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("Port to expose")
        .default("80".to_string())
        .interact_text()?;
    let port: u16 = port_str.parse().unwrap_or(80);

    // IPv6
    let enable_ipv6 = Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt("Enable IPv6 direct access?")
        .default(false)
        .interact()?;

    // Persistence
    let enable_persistence = Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt("Enable persistent storage?")
        .default(false)
        .interact()?;

    // Volume path (required if persistence enabled)
    let volume_path = if enable_persistence {
        let common_paths = vec![
            "/data",
            "/rails/storage",
            "/var/lib/postgresql/data",
            "/var/lib/mysql",
            "/app/data",
            "Custom path...",
        ];

        println!();
        println!("   {} Where does your app store data?", "📁".yellow());

        let path_idx = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Volume mount path")
            .items(&common_paths)
            .default(0)
            .interact_on(&Term::stderr())?;

        let path = if path_idx == common_paths.len() - 1 {
            Input::<String>::with_theme(&ColorfulTheme::default())
                .with_prompt("Enter container path")
                .interact_text()?
        } else {
            common_paths[path_idx].to_string()
        };
        Some(path)
    } else {
        None
    };

    // Memory
    let memory_options = vec!["256m", "512m", "1g", "2g"];
    let mem_idx = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Memory limit")
        .items(&memory_options)
        .default(1)
        .interact_on(&Term::stderr())?;

    let volume_size = if enable_persistence {
        let size_options = vec!["512m", "1g", "2g", "5g", "10g"];
        let size_idx = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Volume size")
            .items(&size_options)
            .default(1) // 1g
            .interact_on(&Term::stderr())?;
        size_options[size_idx].to_string()
    } else {
        "1g".to_string()
    };

    Ok(DeployArgs {
        image,
        from: None,
        port: vec![port],
        env: vec![],
        env_file: None,
        cpu: 0.5,
        memory: memory_options[mem_idx].to_string(),
        persistence: enable_persistence,
        volume_path,
        ipv6: enable_ipv6,
        garage: None,
        hardware: None,
        name: None,
        command: None,
        alias: None,
        volume_size,
    })
}

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

    // Deploy the registry container via the API
    let client = create_client()?;
    let request = DeployRequest {
        image: REGISTRY_IMAGE.to_string(),
        ports: vec![PortSpec {
            port: 5001,
            protocol: "tcp".to_string(),
        }],
        command: None,
        env_vars: HashMap::new(),
        cpu_limit: 0.5,
        memory_limit: "256m".to_string(),
        enable_persistence: true,
        volume_path: Some("/data".to_string()),
        volume_size: Some("1g".to_string()),
        enable_ipv6: false,
        target_garage: None,
        hardware_preference: None,
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
    let push_output = std::process::Command::new("podman")
        .args(["push", &registry_tag, "--tls-verify=false"])
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
    let url = format!("http://{}/v2/", config.address);

    let reachable = match client
        .get(&url)
        .timeout(Duration::from_secs(5))
        .send()
        .await
    {
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
        if let Ok(catalogs) = list_registry_repos(&client, &config.address).await {
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
    let repos = list_registry_repos(&client, &config.address).await?;

    if json_output {
        let mut all_images = Vec::new();
        for repo in &repos {
            if let Ok(tags) = list_registry_tags(&client, &config.address, repo).await {
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
        if let Ok(tags) = list_registry_tags(&client, &config.address, repo).await {
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
    address: &str,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    // The OCI spec doesn't mandate _catalog, but our registry stores manifests
    // with "name:tag" keys, so we list tags per-repo. Since we don't have _catalog,
    // we'll check known image names from the registry tags endpoint.
    // For now, we try the common Docker _catalog extension:
    let url = format!("http://{}/v2/_catalog", address);
    match client
        .get(&url)
        .timeout(Duration::from_secs(5))
        .send()
        .await
    {
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
    address: &str,
    repo: &str,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let url = format!("http://{}/v2/{}/tags/list", address, repo);
    let resp = client
        .get(&url)
        .timeout(Duration::from_secs(5))
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

fn get_wg_config_path() -> PathBuf {
    get_config_dir().join(WG_CONFIG_FILE)
}

fn get_connection_config_path() -> PathBuf {
    get_config_dir().join(CONNECTION_FILE)
}

fn load_connection_config() -> Option<ConnectionConfig> {
    let path = get_connection_config_path();
    if path.exists() {
        if let Ok(contents) = std::fs::read_to_string(&path) {
            if let Ok(config) = serde_json::from_str(&contents) {
                return Some(config);
            }
        }
    }
    None
}

fn save_connection_config(config: &ConnectionConfig) -> Result<(), Box<dyn std::error::Error>> {
    let dir = get_config_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(CONNECTION_FILE);
    std::fs::write(path, serde_json::to_string_pretty(config)?)?;
    Ok(())
}

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
    let output = Command::new("sudo")
        .args(["wg-quick", "up", WG_INTERFACE])
        .output()
        .map_err(|e| format!("Failed to run wg-quick up: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("already exists") {
            return Ok(());
        }
        return Err(format!("wg-quick up failed: {stderr}").into());
    }

    Ok(())
}

/// Bring WireGuard down
fn wg_down(_config_path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    use std::process::Command;

    let output = Command::new("sudo")
        .args(["wg-quick", "down", WG_INTERFACE])
        .output()
        .map_err(|e| format!("Failed to run wg-quick down: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Already down is not an error
        if stderr.contains("is not a WireGuard interface") {
            return Ok(());
        }
        return Err(format!("wg-quick down failed: {stderr}").into());
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
    println!("   nordkraft reset                         Full cleanup (remove all config)");
    println!();

    println!("{}", "JSON OUTPUT:".yellow().bold());
    println!("   nordkraft --json list                 Output as JSON (for scripting)");
    println!();

    println!("{}", "MORE INFO:".dimmed());
    println!("   https://docs.nordkraft.io");
}
