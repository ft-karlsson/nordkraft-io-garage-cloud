// container_manager.rs - nerdctl/containerd operations with Kata support
//
// nerdctl/containerd + Kata Containers
//
// NETWORK ARCHITECTURE (Jan 2025 - Pure Macvlan):
// ===============================================
// Pure macvlan dual-stack for direct IP access
//
//   Macvlan (per-user):
//     nk-macvlan-{slot} → 172.21.{slot}.0/24 (IPv4) + SLAAC (IPv6)
//     - Static IPv4: controller allocates)
//     - SLAAC IPv6:  router assigns)
//     - Parent interface: set by env

use crate::config::{AppConfig, NetworkMode};
use crate::models::SandboxSpec;
use crate::models::{ContainerInfo, PortMapping, PortSpec, RegistryAuth};
use crate::services::macvlan_manager::MacvlanManager;
use crate::services::nats_service::{NatsMessage, NatsService, NatsSubjects};
use crate::services::persistence::PersistenceManager;
use std::collections::HashMap;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

// ============= DEPLOY EVENT CHANNEL =============

/// Lightweight deploy lifecycle event sent via mpsc channel.
/// Non-blocking fire-and-forget — never slows down the deploy.
#[derive(Debug, Clone)]
pub struct DeployPhaseEvent {
    pub container_name: String,
    pub phase: String,
    pub message: String,
    pub success: bool,
}

/// Channel sender type for deploy events.
/// 256 buffer = plenty for a full deploy lifecycle (~6-8 events).
pub type DeployEventTx = mpsc::Sender<DeployPhaseEvent>;

/// Emit a deploy event. Non-blocking — silently drops if channel is full or closed.
/// Write a Docker-style credential store holding exactly one registry entry into a
/// private throwaway directory, for a single `nerdctl run`. The returned path is
/// passed as DOCKER_CONFIG and removed by the caller afterwards.
async fn write_pull_credentials(
    auth: &RegistryAuth,
) -> Result<std::path::PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    use base64::Engine as _;
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!("nk-pull-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir(&dir).await?;
    tokio::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).await?;

    let token = base64::engine::general_purpose::STANDARD
        .encode(format!("{}:{}", auth.username, auth.password));
    let config = serde_json::json!({ "auths": { auth.host.clone(): { "auth": token } } });

    let path = dir.join("config.json");
    tokio::fs::write(&path, serde_json::to_vec(&config)?).await?;
    tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).await?;
    Ok(dir)
}

fn emit(
    tx: &Option<DeployEventTx>,
    container_name: &str,
    phase: &str,
    message: &str,
    success: bool,
) {
    if let Some(tx) = tx {
        let _ = tx.try_send(DeployPhaseEvent {
            container_name: container_name.to_string(),
            phase: phase.to_string(),
            message: message.to_string(),
            success,
        });
    }
}

/// One published port in bridge mode (stored as the app_published label).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PublishedPort {
    pub port: u16,
    pub host_port: u16,
    pub protocol: String,
}

fn bridge_url(host: &str, host_port: u16, protocol: &str) -> String {
    match (host_port, protocol) {
        (80, "tcp") => format!("http://{}", host),
        (p, "tcp") => format!("http://{}:{}", host, p),
        (p, proto) => format!("{}:{}/{}", host, p, proto),
    }
}

/// Requested host port if it is free (not a listening socket, not published
/// by another Nordkraft container, not already taken in this deploy),
/// otherwise the first free port in 30000–39999. A nerdctl publish is an
/// iptables rule, not a socket, so the bind test alone is not enough.
fn pick_host_port(
    addr: &str,
    wanted: u16,
    taken: &[PublishedPort],
    published_elsewhere: &std::collections::HashSet<u16>,
) -> u16 {
    let in_use =
        |p: u16| taken.iter().any(|t| t.host_port == p) || published_elsewhere.contains(&p);
    let free = |p: u16| std::net::TcpListener::bind((addr, p)).is_ok();
    if !in_use(wanted) && free(wanted) {
        return wanted;
    }
    (30000u16..40000)
        .find(|&p| !in_use(p) && free(p))
        .unwrap_or(wanted)
}

/// Where the sandbox daemon appears inside a sandbox container.
pub const SANDBOXD_MOUNT: &str = "/opt/nordkraft/sandboxd";

pub struct ContainerManager {
    runtime_binary: String, // "nerdctl" or "podman"
    use_kata: bool,
    kata_runtime_class: String,
    restart_policy: String,
    persistence: PersistenceManager,
    macvlan: MacvlanManager,
    network_mode: NetworkMode,
    bridge_publish_addr: String,
    public_host: Option<String>,
    stop_timeout: u32,
    /// nordkraft-sandboxd binary on this node, bind-mounted into sandboxes.
    sandboxd_path: String,
}

impl ContainerManager {
    pub fn new(config: &AppConfig) -> Self {
        // Determine if we should use Kata
        // Default ON for agent/hybrid, but can be overridden
        let is_agent = config.mode.is_agent();
        let use_kata = if config.disable_kata {
            info!("⚠️ Kata explicitly DISABLED via DISABLE_KATA=true");
            false
        } else if is_agent && config.use_kata {
            // Check if Kata is actually available
            if Self::check_kata_available(&config.container_runtime) {
                info!("🛡️ Kata Containers ENABLED (VM-level isolation)");
                // Run comprehensive setup validation
                log_kata_setup_status();
                true
            } else {
                warn!("⚠️ Kata requested but not available - using container isolation");
                false
            }
        } else {
            info!("📦 Using standard container runtime (no Kata)");
            false
        };

        Self {
            runtime_binary: config.container_runtime.clone(),
            use_kata,
            kata_runtime_class: config.kata_runtime_class.clone(),
            restart_policy: config.container_restart_policy.clone(),
            sandboxd_path: config.sandboxd_path.clone(),
            persistence: PersistenceManager::new(),
            macvlan: MacvlanManager::with_defaults(),
            network_mode: config.network_mode,
            bridge_publish_addr: config.bridge_publish_addr.clone(),
            public_host: config.public_host.clone(),
            stop_timeout: config.container_stop_timeout,
        }
    }

    /// True when containers live behind nerdctl's default bridge with
    /// published ports instead of on a per-tenant macvlan.
    pub fn is_bridge(&self) -> bool {
        self.network_mode.is_bridge()
    }

    /// Host address a published port is reachable on (bridge mode only).
    pub fn publish_host(&self) -> &str {
        if let Some(h) = &self.public_host {
            return h;
        }
        if self.bridge_publish_addr == "0.0.0.0" {
            "127.0.0.1"
        } else {
            &self.bridge_publish_addr
        }
    }

    /// Check if Kata runtime is available
    fn check_kata_available(_runtime: &str) -> bool {
        // Check if kata-runtime binary exists
        if std::path::Path::new("/opt/kata/bin/kata-runtime").exists() {
            return true;
        }

        // Check if containerd has Kata configured
        if let Ok(content) = std::fs::read_to_string("/etc/containerd/config.toml") {
            if content.contains("io.containerd.kata") {
                return true;
            }
        }

        false
    }

    /// Deploy secure container
    ///
    /// Network architecture:
    /// - Macvlan network (dual-stack): nk-macvlan-{slot} → IPv4 (static) + IPv6 (SLAAC)
    #[allow(clippy::too_many_arguments)]
    pub async fn deploy_secure_container(
        &self,
        owner_pubkey: &str,
        tenant_id: &str,
        user_subnet: &str, // e.g., "172.21.1.0/24" - informational
        image: &str,
        ports: Option<Vec<PortSpec>>,
        command: Option<Vec<String>>,
        env_vars: Option<HashMap<String, String>>,
        cpu_limit: Option<f32>,
        memory_limit: Option<String>,
        user_slot: Option<i32>,
        enable_persistence: bool,
        volume_path: Option<String>,
        allocated_ip: Option<String>, // IPv4 from controller
        container_name: Option<String>,
        enable_ipv6: bool,
        registry_auth: Option<RegistryAuth>, // scoped pull credentials, tenant's own registry only
        sandbox: Option<SandboxSpec>,        // per-container lifecycle + labels
        event_tx: Option<DeployEventTx>,
    ) -> Result<
        (
            String,
            Option<String>,
            String,
            Vec<PortSpec>,
            Option<String>,
        ),
        Box<dyn std::error::Error + Send + Sync>,
    > {
        // Generate container name
        let container_name = container_name.unwrap_or_else(|| {
            format!(
                "nk-{}-{}",
                &tenant_id[..8.min(tenant_id.len())],
                &uuid::Uuid::new_v4().to_string()[..8]
            )
        });

        // The controller already enforced this; the agent checks independently.
        if let Err(e) = crate::services::image_policy::validate_image_source(image, user_subnet) {
            error!("🚫 Image policy refused pull for {}: {}", tenant_id, e);
            emit(&event_tx, &container_name, "failed", &e, false);
            return Err(e.into());
        }

        let bridge = self.is_bridge();
        let user_slot = user_slot.ok_or("user_slot required for network allocation")?;

        if bridge {
            emit(
                &event_tx,
                &container_name,
                "network",
                "Using bridge network with published ports",
                true,
            );
        } else {
            emit(
                &event_tx,
                &container_name,
                "network",
                "Setting up macvlan network for slot",
                true,
            );

            // ========== MACVLAN NETWORK (DUAL-STACK) ==========
            // Verify interface setup on first deploy
            if let Err(e) = self.macvlan.verify_setup().await {
                warn!("Interface verification failed: {} - continuing anyway", e);
            }

            self.macvlan.ensure_network_for_slot(user_slot).await?;
        }

        // Build command args
        let sandbox = sandbox.unwrap_or_default();
        let restart = sandbox
            .lifecycle
            .restart
            .as_deref()
            .and_then(crate::models::restart_policy_flag)
            .map(str::to_string)
            .unwrap_or_else(|| self.restart_policy.clone());
        let mut args = vec![
            "run".to_string(),
            "-d".to_string(),
            "--name".to_string(),
            container_name.clone(),
            "--restart".to_string(),
            restart,
            "--stop-timeout".to_string(),
            self.stop_timeout.to_string(),
        ];

        // ========== NETWORK CONFIGURATION ==========
        let allocated_ip = if bridge {
            // Bridge: nerdctl's default network, ports published on the host.
            // The controller-allocated IP is recorded but not enforced.
            args.extend_from_slice(&["--network".to_string(), "bridge".to_string()]);
            // Host port = the requested port if free, otherwise a free one from
            // the ephemeral range, so several containers can publish :80.
            let mut published: Vec<PublishedPort> = Vec::new();
            let elsewhere = self.host_ports_in_use().await;
            for p in ports.iter().flatten() {
                let proto = p.protocol.to_lowercase();
                let host_port =
                    pick_host_port(&self.bridge_publish_addr, p.port, &published, &elsewhere);
                let spec = if proto == "udp" {
                    format!("{}:{}:{}/udp", self.bridge_publish_addr, host_port, p.port)
                } else {
                    format!("{}:{}:{}", self.bridge_publish_addr, host_port, p.port)
                };
                args.extend_from_slice(&["-p".to_string(), spec]);
                published.push(PublishedPort {
                    port: p.port,
                    host_port,
                    protocol: proto,
                });
            }
            if !published.is_empty() {
                let json = serde_json::to_string(&published).unwrap_or_default();
                args.extend_from_slice(&["--label".to_string(), format!("app_published={}", json)]);
            }
            info!(
                "📡 Bridge network: published {:?} on {}",
                published
                    .iter()
                    .map(|p| format!("{}->{}", p.host_port, p.port))
                    .collect::<Vec<_>>(),
                self.bridge_publish_addr
            );
            allocated_ip.unwrap_or_default()
        } else {
            // Attach to macvlan with static IPv4 + IPv6 SLAAC
            let allocated_ip = allocated_ip.ok_or("allocated_ip required for macvlan")?;
            let network_args = self
                .macvlan
                .get_network_args_with_ipv4(user_slot, &allocated_ip);
            args.extend(network_args);

            info!(
                "📡 Macvlan dual-stack: {} with IPv4 {} + IPv6 SLAAC",
                self.macvlan.network_name_for_slot(user_slot),
                allocated_ip
            );
            allocated_ip
        };

        // Add Kata runtime if enabled
        if self.use_kata {
            args.extend_from_slice(&["--runtime".to_string(), self.kata_runtime_class.clone()]);
        }

        // Sandbox: nordkraft-sandboxd becomes PID 1. The image's command (or
        // the deploy's `command`) runs as its child; exec, streams, PTYs and
        // files are served from inside the micro-VM on the container's own
        // address, so none of it crosses the runtime's exec path. The binary
        // is mounted read-only outside /tmp (which is noexec here).
        if let Some(daemon) = &sandbox.daemon {
            if !std::path::Path::new(&self.sandboxd_path).is_file() {
                return Err(format!(
                    "sandbox daemon not installed on this node: {} missing (SANDBOXD_PATH)",
                    self.sandboxd_path
                )
                .into());
            }
            args.extend_from_slice(&[
                "-v".to_string(),
                format!("{}:{SANDBOXD_MOUNT}:ro", self.sandboxd_path),
                "--entrypoint".to_string(),
                SANDBOXD_MOUNT.to_string(),
                "-e".to_string(),
                format!("NORDKRAFT_SANDBOX_TOKEN={}", daemon.token),
                "-e".to_string(),
                format!("NORDKRAFT_SANDBOX_PORT={}", daemon.port),
                "--label".to_string(),
                format!("nordkraft.sandboxd={}", daemon.port),
            ]);
        }

        // Labels for ownership tracking
        args.extend_from_slice(&[
            "--label".to_string(),
            format!("owner_pubkey={}", owner_pubkey),
            "--label".to_string(),
            format!("tenant_id={}", tenant_id),
            "--label".to_string(),
            "managed_by=nordkraft".to_string(),
            "--label".to_string(),
            format!("app_image={}", image),
            "--label".to_string(),
            format!("created_at={}", chrono::Utc::now().to_rfc3339()),
            "--label".to_string(),
            format!("user_slot={}", user_slot),
            "--label".to_string(),
            format!("user_subnet={}", user_subnet),
        ]);
        // Lifecycle lives on the container itself (the agent has no database):
        // the reaper reads these labels. Seconds, 0 = never.
        for (key, value) in sandbox_labels(&sandbox) {
            args.extend_from_slice(&["--label".to_string(), format!("{key}={value}")]);
        }

        // DNS configuration - use public DNS since we're on LAN
        args.extend_from_slice(&[
            "--dns".to_string(),
            "8.8.8.8".to_string(),
            "--dns".to_string(),
            "8.8.4.4".to_string(),
        ]);

        // Port handling - NOTE: With macvlan, -p is INFORMATIONAL ONLY!
        // Containers are directly on the LAN, so ports are exposed directly.
        // Only label ports that were explicitly specified by the user — no defaults.
        let final_ports = ports.unwrap_or_default();

        // Store ports in label for later retrieval (important!)
        if !final_ports.is_empty() {
            let ports_json = serde_json::to_string(&final_ports).unwrap_or_default();
            args.extend_from_slice(&["--label".to_string(), format!("app_ports={}", ports_json)]);
        }

        // Add IPv6 label if enabled (for tracking)
        if enable_ipv6 {
            args.extend_from_slice(&["--label".to_string(), "ipv6_enabled=true".to_string()]);
        }

        // Security hardening (works with both runc and Kata)
        args.extend_from_slice(&[
            "--cap-drop".to_string(),
            "ALL".to_string(),
            "--cap-add".to_string(),
            "CHOWN".to_string(),
            "--cap-add".to_string(),
            "DAC_OVERRIDE".to_string(),
            "--cap-add".to_string(),
            "FOWNER".to_string(),
            "--cap-add".to_string(),
            "SETGID".to_string(),
            "--cap-add".to_string(),
            "SETUID".to_string(),
            "--cap-add".to_string(),
            "NET_BIND_SERVICE".to_string(),
        ]);

        // Read-only filesystem unless persistence enabled
        if !enable_persistence {
            args.extend_from_slice(&[
                "--tmpfs".to_string(),
                "/tmp:rw,noexec,nosuid,size=100m".to_string(),
                "--tmpfs".to_string(),
                "/var/tmp:rw,noexec,nosuid,size=100m".to_string(),
            ]);
        }

        // Resource limits
        args.extend_from_slice(&[
            "--cpus".to_string(),
            cpu_limit.unwrap_or(0.5).to_string(),
            "--memory".to_string(),
            memory_limit.clone().unwrap_or_else(|| "512m".to_string()),
            "--pids-limit".to_string(),
            "2000".to_string(),
        ]);

        // Environment variables
        if let Some(env) = env_vars {
            for (key, value) in env {
                args.extend_from_slice(&["-e".to_string(), format!("{}={}", key, value)]);
            }
        }

        // Persistent volume
        if enable_persistence {
            if self.use_kata {
                let (volumes_ok, issues) = validate_kata_volume_support();

                if !volumes_ok {
                    let issue_list = issues.join("\n  ");
                    return Err(format!(
                        "Cannot enable persistence with Kata - virtio-fs not properly configured:\n  {}\n\n\
                         Fix the issues above, or disable Kata for this deployment:\n  \
                         DISABLE_KATA=true or deploy without --persistence",
                        issue_list
                    ).into());
                }

                if !issues.is_empty() {
                    for issue in &issues {
                        warn!("{}", issue);
                    }
                }
            }

            let container_mount_path = volume_path.as_deref().unwrap_or("/data");

            match self
                .persistence
                .create_container_volume(user_slot as u32, &container_name, "data")
                .await
            {
                Ok(host_volume_path) => {
                    let mount_args =
                        self.get_volume_mount_args(&host_volume_path, container_mount_path, false);
                    args.extend(mount_args);

                    if self.use_kata {
                        info!(
                            "📁 Kata volume (virtio-fs validated): {} -> {}",
                            host_volume_path, container_mount_path
                        );
                    } else {
                        info!(
                            "📁 Bind mount volume: {} -> {}",
                            host_volume_path, container_mount_path
                        );
                    }
                }
                Err(e) => {
                    error!("Failed to create volume: {}", e);
                    return Err(format!("Volume creation failed: {}", e).into());
                }
            }
        }

        // Image
        // Detect private/insecure registries (IP-based, not docker.io/ghcr.io)
        if !image.starts_with("docker.io/")
            && !image.starts_with("ghcr.io/")
            && !image.starts_with("registry.k8s.io/")
            && image.contains(':')
            && image.split('/').next().is_some_and(|host| {
                host.contains(':') || host.parse::<std::net::Ipv4Addr>().is_ok()
            })
        {
            args.push("--insecure-registry".to_string());
        }
        args.push(image.to_string());

        // Optional command override
        if let Some(cmd) = command {
            args.extend(cmd);
        }

        debug!("Running: {} {}", self.runtime_binary, args.join(" "));

        emit(
            &event_tx,
            &container_name,
            "pulling",
            &format!("Pulling image: {}", image),
            true,
        );

        // Credentials, if any, live in a private throwaway DOCKER_CONFIG dir that holds
        // exactly one registry entry, scoped to this tenant's own registry.
        let pull_config_dir = match &registry_auth {
            Some(auth) => {
                info!("🔐 Pull credentials scoped to {}", auth.host);
                Some(write_pull_credentials(auth).await?)
            }
            None => None,
        };

        let mut cmd = tokio::process::Command::new(&self.runtime_binary);
        cmd.args(&args);
        if let Some(dir) = &pull_config_dir {
            cmd.env("DOCKER_CONFIG", dir);
        }
        let output = cmd.output().await;

        if let Some(dir) = &pull_config_dir {
            if let Err(e) = tokio::fs::remove_dir_all(dir).await {
                warn!(
                    "Could not remove pull credential dir {}: {}",
                    dir.display(),
                    e
                );
            }
        }
        let output = output?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            error!("Container creation failed: {}", stderr);

            // Extract clean error for events — nerdctl dumps progress bars + ANSI codes
            let clean_error = stderr
                .lines()
                .rfind(|l| {
                    l.contains("level=fatal") || l.contains("level=error") || l.contains("FATA")
                })
                .map(|l| {
                    // Strip nerdctl log prefix: time="..." level=fatal msg="..."
                    if let Some(msg_start) = l.find("msg=\"") {
                        let msg = &l[msg_start + 5..];
                        msg.trim_end_matches('"').to_string()
                    } else {
                        l.to_string()
                    }
                })
                .unwrap_or_else(|| {
                    // Fallback: first meaningful line, truncated
                    let first = stderr
                        .lines()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or("Unknown error");
                    first.chars().take(200).collect()
                });

            emit(&event_tx, &container_name, "failed", &clean_error, false);
            return Err(stderr.into());
        }

        emit(
            &event_tx,
            &container_name,
            "created",
            "Container created, verifying network",
            true,
        );

        // ========== IPv4 VERIFICATION ==========
        // Get container IPv4 from the macvlan network
        let container_ip = match self.macvlan.get_container_ipv4(&container_name).await? {
            Some(ip) => ip,
            None => {
                error!("Failed to get IPv4 from container. Removing...");
                emit(
                    &event_tx,
                    &container_name,
                    "failed",
                    "IPv4 assignment failed",
                    false,
                );
                let _ = tokio::process::Command::new(&self.runtime_binary)
                    .args(["rm", "-f", &container_name])
                    .output()
                    .await;
                return Err("Container IPv4 assignment failed".into());
            }
        };

        // Verify IPv4 assignment matches what we requested (macvlan only —
        // on a bridge the runtime hands out the address)
        if !bridge && container_ip != allocated_ip {
            error!(
                "🚨 IP MISMATCH! Expected {} but container got {}. \
                 This will break routing. Removing container.",
                allocated_ip, container_ip
            );
            emit(
                &event_tx,
                &container_name,
                "failed",
                &format!(
                    "IP mismatch: expected {} got {}",
                    allocated_ip, container_ip
                ),
                false,
            );
            let _ = tokio::process::Command::new(&self.runtime_binary)
                .args(["rm", "-f", &container_name])
                .output()
                .await;
            return Err(format!(
                "IP assignment failed: expected {} but got {}. \
                 Possible IP conflict or exhausted subnet.",
                allocated_ip, container_ip
            )
            .into());
        }
        info!("✅ IPv4 verification passed: {}", container_ip);

        // ========== SLAAC IPv6 ==========
        // Fetch the IPv6 address ASSIGNED BY THE ROUTER (not by us)
        let final_ipv6 = if enable_ipv6 && bridge {
            warn!("⚠️ IPv6 requested but not available in bridge mode — IPv4 only");
            None
        } else if enable_ipv6 {
            info!("🌐 Waiting for SLAAC to assign IPv6...");

            match self.macvlan.wait_for_slaac_ipv6(&container_name).await {
                Ok(Some(ipv6)) => {
                    info!("✅ IPv6 via SLAAC: {}", ipv6);
                    Some(ipv6)
                }
                Ok(None) => {
                    warn!(
                        "⚠️ IPv6 requested but SLAAC did not assign an address. \
                           Container is running with IPv4 only. \
                           Check: macvlan on physical interface, router sending RAs"
                    );
                    None
                }
                Err(e) => {
                    warn!(
                        "⚠️ Failed to get IPv6: {}. Container running with IPv4 only.",
                        e
                    );
                    None
                }
            }
        } else {
            None
        };

        info!(
            "✅ Deployed container: {} with IPv4: {}, IPv6: {:?}, Kata: {}",
            container_name, container_ip, final_ipv6, self.use_kata
        );

        emit(
            &event_tx,
            &container_name,
            "running",
            &format!(
                "Container running at {} (Kata: {})",
                container_ip, self.use_kata
            ),
            true,
        );

        // Return (name, pod_id=None, ipv4, ports, ipv6)
        Ok((container_name, None, container_ip, final_ports, final_ipv6))
    }

    /// Build volume mount args
    fn get_volume_mount_args(
        &self,
        host_path: &str,
        container_path: &str,
        read_only: bool,
    ) -> Vec<String> {
        let mount_spec = if read_only {
            format!("{}:{}:ro", host_path, container_path)
        } else {
            format!("{}:{}", host_path, container_path)
        };

        vec!["-v".to_string(), mount_spec]
    }

    /// List user's containers
    pub async fn list_user_containers(
        &self,
        owner_pubkey: &str,
    ) -> Result<Vec<ContainerInfo>, Box<dyn std::error::Error + Send + Sync>> {
        info!("📋 Listing containers for owner: {}", owner_pubkey);

        let output = tokio::process::Command::new(&self.runtime_binary)
            .args([
                "ps",
                "-a",
                "--format",
                "json",
                "--filter",
                &format!("label=owner_pubkey={}", owner_pubkey),
                "--filter",
                "label=managed_by=nordkraft",
            ])
            .output()
            .await?;

        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).into());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut containers = Vec::new();

        // Parse NDJSON (one JSON object per line)
        for line in stdout.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let container: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(e) => {
                    warn!("Failed to parse container JSON: {}", e);
                    continue;
                }
            };

            // Skip infra/pause containers
            let image = container["Image"].as_str().unwrap_or("");
            if image.contains("pause") || image.contains("infra") {
                continue;
            }

            // Get container name
            let name = container["Names"].as_str().unwrap_or("").to_string();
            if name.is_empty() {
                continue;
            }

            let container_id = container["ID"].as_str().unwrap_or("").to_string();
            let status = container["Status"].as_str().unwrap_or("").to_string();
            let created_at = container["CreatedAt"].as_str().unwrap_or("").to_string();

            // Get ports from labels
            let ports = self.get_ports_from_container(&container);

            // Get IPv4 from macvlan network
            let container_ip = self.macvlan.get_container_ipv4(&name).await.ok().flatten();

            // Get IPv6 - try label first, then fetch live via SLAAC
            let ipv6_address = match self.get_label_value(&container, "ipv6_address") {
                Some(ipv6) => Some(ipv6),
                None => {
                    // Fallback: fetch live from macvlan (SLAAC-assigned)
                    self.macvlan.get_container_ipv6(&name).await.ok().flatten()
                }
            };

            let ipv6_enabled = self
                .get_label_value(&container, "ipv6_enabled")
                .map(|v| v == "true")
                .unwrap_or(false)
                || ipv6_address.is_some();

            // Build port mappings with proper URLs
            let bridge_host = self.is_bridge().then(|| self.publish_host().to_string());
            let published = self.get_published_ports(&container);
            let port_mappings: Vec<PortMapping> = ports
                .iter()
                .map(|port_spec| {
                    let access_url = if let Some(ref host) = bridge_host {
                        let hp = published
                            .iter()
                            .find(|pp| pp.port == port_spec.port)
                            .map(|pp| pp.host_port)
                            .unwrap_or(port_spec.port);
                        bridge_url(host, hp, &port_spec.protocol)
                    } else if let Some(ref ip) = container_ip {
                        match (port_spec.port, port_spec.protocol.as_str()) {
                            (80, "tcp") => format!("http://{}", ip),
                            (443, "tcp") => format!("https://{}", ip),
                            (p, "tcp") => format!("{}:{}", ip, p),
                            (p, proto) => format!("{}:{}/{}", ip, p, proto),
                        }
                    } else {
                        format!(":{}", port_spec.port)
                    };

                    let ipv6_url = ipv6_address
                        .as_ref()
                        .map(|ipv6| crate::models::build_ipv6_url(ipv6, port_spec.port));

                    PortMapping {
                        port: port_spec.port,
                        protocol: port_spec.protocol.clone(),
                        access_url,
                        ipv6_url,
                    }
                })
                .collect();

            containers.push(ContainerInfo {
                container_id,
                name,
                image: image.to_string(),
                status,
                pod_id: None,
                created_at,
                ports: port_mappings,
                container_ip,
                ipv6_address,
                ipv6_enabled,
                labels: user_labels(container["Labels"].as_str().unwrap_or("")),
            });
        }

        info!("Found {} containers", containers.len());
        Ok(containers)
    }

    /// Get label value from container JSON
    fn get_label_value(&self, container: &serde_json::Value, key: &str) -> Option<String> {
        let labels_str = container["Labels"].as_str()?;
        let prefix = format!("{}=", key);

        for start_candidate in labels_str.match_indices(&prefix) {
            let start = start_candidate.0;

            // Must be at start of string or after comma
            if start > 0 && labels_str.as_bytes()[start - 1] != b',' {
                continue;
            }

            let value_start = start + prefix.len();
            let rest = &labels_str[value_start..];

            // Walk forward respecting bracket/brace nesting
            let mut depth = 0i32;
            let mut end = rest.len();
            for (i, ch) in rest.char_indices() {
                match ch {
                    '[' | '{' => depth += 1,
                    ']' | '}' => depth -= 1,
                    ',' if depth == 0 => {
                        end = i;
                        break;
                    }
                    _ => {}
                }
            }

            return Some(rest[..end].to_string());
        }

        None
    }

    /// Bridge mode: host↔container port mapping from the app_published label.
    fn get_published_ports(&self, container: &serde_json::Value) -> Vec<PublishedPort> {
        self.get_label_value(container, "app_published")
            .and_then(|s| serde_json::from_str::<Vec<PublishedPort>>(&s).ok())
            .unwrap_or_default()
    }

    /// Bridge mode: host ports already published by any Nordkraft container
    /// on this node (running or stopped — a stopped one keeps its mapping).
    async fn host_ports_in_use(&self) -> std::collections::HashSet<u16> {
        let mut set = std::collections::HashSet::new();
        let output = tokio::process::Command::new(&self.runtime_binary)
            .args([
                "ps",
                "-a",
                "--format",
                "json",
                "--filter",
                "label=managed_by=nordkraft",
            ])
            .output()
            .await;
        let Ok(output) = output else { return set };
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let Ok(c) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            for pp in self.get_published_ports(&c) {
                set.insert(pp.host_port);
            }
        }
        set
    }

    /// Bridge mode: URLs a caller can reach a container's published ports on.
    /// Empty in macvlan mode.
    pub async fn access_urls(&self, container_name: &str) -> Vec<String> {
        if !self.is_bridge() {
            return vec![];
        }
        let output = tokio::process::Command::new(&self.runtime_binary)
            .args(["inspect", container_name])
            .output()
            .await;
        let Ok(output) = output else { return vec![] };
        let Ok(raw) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
            return vec![];
        };
        let Some(c) = raw.as_array().and_then(|a| a.first()) else {
            return vec![];
        };
        let labels = &c["Config"]["Labels"];
        let published: Vec<PublishedPort> = labels["app_published"]
            .as_str()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        let host = self.publish_host();
        published
            .iter()
            .map(|p| bridge_url(host, p.host_port, &p.protocol))
            .collect()
    }

    /// Get ports from container labels — only returns ports if explicitly set via app_ports label.
    /// Base runtime images (node, python, ubuntu etc.) have no label and return empty.
    fn get_ports_from_container(&self, container: &serde_json::Value) -> Vec<PortSpec> {
        if let Some(ports_str) = self.get_label_value(container, "app_ports") {
            if let Ok(ports) = serde_json::from_str::<Vec<PortSpec>>(&ports_str) {
                return ports;
            }
        }
        vec![]
    }

    /// Stop a container
    pub async fn stop_container(
        &self,
        container_id: &str,
        owner_pubkey: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let containers = self.list_user_containers(owner_pubkey).await?;
        if !containers
            .iter()
            .any(|c| c.container_id == container_id || c.name == container_id)
        {
            return Err("Container not found or access denied".into());
        }

        let output = tokio::process::Command::new(&self.runtime_binary)
            .args(["stop", container_id])
            .output()
            .await?;

        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).into());
        }

        info!("⏹️ Stopped container: {}", container_id);
        Ok(())
    }

    /// Start a container
    pub async fn start_container(
        &self,
        container_id: &str,
        owner_pubkey: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let containers = self.list_user_containers(owner_pubkey).await?;
        if !containers
            .iter()
            .any(|c| c.container_id == container_id || c.name == container_id)
        {
            return Err("Container not found or access denied".into());
        }

        let output = tokio::process::Command::new(&self.runtime_binary)
            .args(["start", container_id])
            .output()
            .await?;

        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).into());
        }

        info!("▶️ Started container: {}", container_id);
        Ok(())
    }

    /// Remove a container
    pub async fn remove_container(
        &self,
        container_id: &str,
        owner_pubkey: &str,
        user_slot: Option<i32>,
        nats: Option<&NatsService>,
        node_id: Option<&str>,
        force: bool,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let containers = self.list_user_containers(owner_pubkey).await?;
        let container = containers
            .iter()
            .find(|c| c.container_id == container_id || c.name == container_id);

        if container.is_none() {
            return Err("Container not found or access denied".into());
        }

        let container_name = container.unwrap().name.clone();

        // Graceful stop first (SIGTERM, then SIGKILL after stop_timeout) —
        // unless forced, in which case `rm -f` kills it straight away.
        if !force {
            let _ = tokio::process::Command::new(&self.runtime_binary)
                .args([
                    "stop",
                    "-t",
                    &self.stop_timeout.to_string(),
                    &container_name,
                ])
                .output()
                .await;
        }

        // Remove container
        let output = tokio::process::Command::new(&self.runtime_binary)
            .args(["rm", "-f", &container_name])
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.contains("No such container") {
                return Err(stderr.into());
            }
        }

        // ========== CNI IPAM STATE CLEANUP ==========
        // Clean up CNI host-local IPAM state file to keep DB and CNI in sync
        if let Some(ref ip) = container.unwrap().container_ip {
            // Use the macvlan network name for cleanup
            if let Some(slot) = user_slot {
                let network_name = self.macvlan.network_name_for_slot(slot);
                let cni_file = format!("/var/lib/cni/networks/{}/{}", network_name, ip);
                match tokio::fs::remove_file(&cni_file).await {
                    Ok(_) => {
                        info!("🧹 Cleaned CNI IPAM state: {} for IP {}", network_name, ip);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        debug!("CNI state file already removed: {}", cni_file);
                    }
                    Err(e) => {
                        warn!(
                            "Failed to clean CNI state file {}: {} (continuing anyway)",
                            cni_file, e
                        );
                    }
                }
            }
        }

        // Clean up volume if it exists
        if let Some(slot) = user_slot {
            let _ = self
                .persistence
                .remove_container_volumes(slot as u32, &container_name)
                .await;
        }

        // Notify controller via NATS
        if let (Some(nats), Some(node)) = (nats, node_id) {
            let msg = NatsMessage::ContainerDeleted {
                container_name: container_name.clone(),
                node_id: node.to_string(),
            };
            let _ = nats
                .publish_message(NatsSubjects::CONTAINER_DELETED.to_string(), &msg)
                .await;
        }

        info!("🗑️ Removed container: {}", container_name);
        Ok(())
    }

    /// Get container logs
    pub async fn get_container_logs(
        &self,
        container_id: &str,
        owner_pubkey: &str,
        lines: Option<usize>,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let containers = self.list_user_containers(owner_pubkey).await?;
        if !containers
            .iter()
            .any(|c| c.container_id == container_id || c.name == container_id)
        {
            return Err("Container not found or access denied".into());
        }

        let mut args = vec!["logs".to_string()];
        if let Some(n) = lines {
            args.extend_from_slice(&["--tail".to_string(), n.to_string()]);
        }
        args.push(container_id.to_string());

        let output = tokio::process::Command::new(&self.runtime_binary)
            .args(&args)
            .output()
            .await?;

        // Combine stdout and stderr (logs can be in either)
        let mut logs = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.is_empty() && !stderr.contains("Error") {
            logs.push_str(&stderr);
        }

        Ok(logs)
    }

    /// Read cpu and memory limits from `nerdctl inspect --mode=native`, whose OCI
    /// runtime spec carries the limits the runtime actually enforces:
    ///
    ///   Spec.linux.resources.memory.limit          → bytes
    ///   Spec.linux.resources.cpu.quota / .period   → cores
    ///
    /// Returns (cpu_cores, memory_bytes); either may be None. Best effort by design:
    /// a limit we cannot read stays None so callers can say "unknown" instead of
    /// inventing a value.
    async fn inspect_native_limits(&self, container_id: &str) -> (Option<f64>, Option<i64>) {
        let output = match tokio::process::Command::new(&self.runtime_binary)
            .args(["inspect", "--mode=native", container_id])
            .output()
            .await
        {
            Ok(o) if o.status.success() => o,
            Ok(o) => {
                warn!(
                    "native inspect failed for {}: {}",
                    container_id,
                    String::from_utf8_lossy(&o.stderr).trim()
                );
                return (None, None);
            }
            Err(e) => {
                warn!("could not run native inspect for {}: {}", container_id, e);
                return (None, None);
            }
        };

        let raw: serde_json::Value = match serde_json::from_slice(&output.stdout) {
            Ok(v) => v,
            Err(e) => {
                warn!("unparseable native inspect for {}: {}", container_id, e);
                return (None, None);
            }
        };

        let null = serde_json::Value::Null;
        let first = raw.as_array().and_then(|arr| arr.first()).unwrap_or(&null);
        let resources = &first["Spec"]["linux"]["resources"];

        let memory = resources["memory"]["limit"].as_i64().filter(|&m| m > 0);

        let cpu = match (
            resources["cpu"]["quota"].as_i64(),
            resources["cpu"]["period"].as_i64(),
        ) {
            (Some(quota), Some(period)) if quota > 0 && period > 0 => {
                Some(quota as f64 / period as f64)
            }
            _ => None,
        };

        (cpu, memory)
    }

    /// Inspect a single container — calls `nerdctl inspect` and extracts rich data.
    /// Only succeeds if the container is owned by owner_pubkey (label check).
    pub async fn inspect_container(
        &self,
        container_id: &str,
        owner_pubkey: &str,
        node_id: &str,
    ) -> Result<
        crate::services::nats_service::ContainerInspectData,
        Box<dyn std::error::Error + Send + Sync>,
    > {
        // Ownership check via list first
        let owned = self
            .list_user_containers(owner_pubkey)
            .await?
            .into_iter()
            .find(|c| c.container_id == container_id || c.name == container_id)
            .ok_or("Container not found or access denied")?;

        // Run nerdctl inspect for rich data
        let output = tokio::process::Command::new(&self.runtime_binary)
            .args(["inspect", container_id])
            .output()
            .await?;

        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).into_owned().into());
        }

        let raw: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        // nerdctl inspect returns an array — take first element
        let c = raw
            .as_array()
            .and_then(|arr| arr.first())
            .ok_or("Empty inspect output")?;

        let state = &c["State"];
        let config = &c["Config"];
        let host_config = &c["HostConfig"];

        // Runtime: nerdctl doesn't expose runtime in HostConfig like Docker does.
        // Kata Containers is detectable via the tap0_kata network interface it creates.
        let runtime = {
            let networks = &c["NetworkSettings"]["Networks"];
            let is_kata = networks
                .as_object()
                .map(|nets| nets.keys().any(|k| k.contains("kata")))
                .unwrap_or(false);
            if is_kata {
                "io.containerd.kata.v2".to_string()
            } else {
                "runc".to_string()
            }
        };

        // CPU limit: NanoCPUs → cores
        let cpu_limit = host_config["NanoCpus"].as_i64().map(|n| n as f64 / 1e9);

        // Memory limit in bytes (0 = unlimited)
        let memory_limit = host_config["Memory"].as_i64().filter(|&m| m > 0);

        // nerdctl's dockercompat inspect omits HostConfig entirely on some builds
        // (observed with containerd + Kata), so NanoCpus/Memory read as null even
        // though cgroup limits are enforced and `nerdctl stats` reports them. Fall
        // back to the native view, whose OCI spec carries what the runtime applies.
        // Without this the CLI receives null and substitutes 0.5 cpu / 512m, which
        // silently writes wrong limits into every generated .nk spec.
        let (cpu_limit, memory_limit) = if cpu_limit.is_none() || memory_limit.is_none() {
            let (native_cpu, native_memory) = self.inspect_native_limits(container_id).await;
            (cpu_limit.or(native_cpu), memory_limit.or(native_memory))
        } else {
            (cpu_limit, memory_limit)
        };

        // Environment variables
        let env_vars: Vec<String> = config["Env"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        // Entrypoint + Cmd combined as effective command
        let mut command: Vec<String> = config["Entrypoint"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let cmd_part: Vec<String> = config["Cmd"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        command.extend(cmd_part);

        // Mounts (volumes)
        let volume_mounts: Vec<String> = c["Mounts"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        let src = m["Source"].as_str()?;
                        let dst = m["Destination"].as_str()?;
                        Some(format!("{}:{}", src, dst))
                    })
                    .collect()
            })
            .unwrap_or_default();

        let persistence_enabled = !volume_mounts.is_empty();

        // Labels as HashMap
        let labels: std::collections::HashMap<String, String> = config["Labels"]
            .as_object()
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        Ok(crate::services::nats_service::ContainerInspectData {
            container_id: c["Id"].as_str().unwrap_or(container_id).to_string(),
            name: owned.name.clone(),
            image: owned.image.clone(),
            image_digest: None, // nerdctl inspect doesn't expose digest directly
            status: state["Status"]
                .as_str()
                .unwrap_or(&owned.status)
                .to_string(),
            created_at: c["Created"]
                .as_str()
                .unwrap_or(&owned.created_at)
                .to_string(),
            started_at: state["StartedAt"].as_str().map(String::from),
            finished_at: state["FinishedAt"]
                .as_str()
                .filter(|s| !s.is_empty() && *s != "0001-01-01T00:00:00Z")
                .map(String::from),
            exit_code: state["ExitCode"].as_i64(),
            restart_count: labels
                .get("containerd.io/restart.count")
                .and_then(|v| v.parse::<i64>().ok()),
            container_ip: owned.container_ip,
            ipv6_address: owned.ipv6_address,
            ipv6_enabled: owned.ipv6_enabled,
            ports: owned
                .ports
                .iter()
                .map(|p| {
                    serde_json::json!({
                        "port": p.port,
                        "protocol": p.protocol,
                        "access_url": p.access_url,
                        "ipv6_url": p.ipv6_url
                    })
                })
                .collect(),
            env_vars,
            command,
            hostname: config["Hostname"].as_str().map(String::from),
            node_id: node_id.to_string(),
            runtime,
            cpu_limit,
            memory_limit,
            persistence_enabled,
            volume_mounts,
            labels,
        })
    }
}

// ============= KATA VOLUME VALIDATION =============

/// Check if virtiofsd binary exists
pub fn find_virtiofsd() -> Option<String> {
    let paths = [
        "/usr/libexec/virtiofsd",
        "/usr/lib/qemu/virtiofsd",
        "/usr/local/bin/virtiofsd",
        "/usr/bin/virtiofsd",
        "/opt/kata/libexec/virtiofsd",
    ];

    for path in paths {
        if std::path::Path::new(path).exists() {
            return Some(path.to_string());
        }
    }

    if let Ok(output) = std::process::Command::new("which")
        .arg("virtiofsd")
        .output()
    {
        if output.status.success() {
            return Some(String::from_utf8_lossy(&output.stdout).trim().to_string());
        }
    }

    None
}

/// Find Kata configuration file
pub fn find_kata_config() -> Option<String> {
    let configs = [
        "/opt/kata/share/defaults/kata-containers/configuration.toml",
        "/etc/kata-containers/configuration.toml",
        "/usr/share/kata-containers/defaults/configuration.toml",
        "/usr/share/defaults/kata-containers/configuration.toml",
    ];

    for path in configs {
        if std::path::Path::new(path).exists() {
            return Some(path.to_string());
        }
    }
    None
}

/// Check if Kata is configured for virtio-fs
pub fn check_kata_virtiofs_config() -> (bool, Option<String>, Option<String>) {
    let config_path = match find_kata_config() {
        Some(p) => p,
        None => return (false, None, None),
    };

    let content = match std::fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(_) => return (false, None, None),
    };

    let mut shared_fs: Option<String> = None;
    let mut virtio_fs_daemon: Option<String> = None;

    for line in content.lines() {
        let line = line.trim();

        if line.starts_with("shared_fs") {
            if let Some(value) = line.split('=').nth(1) {
                let value = value.trim().trim_matches('"').trim_matches('\'');
                shared_fs = Some(value.to_string());
            }
        }

        if line.starts_with("virtio_fs_daemon") {
            if let Some(value) = line.split('=').nth(1) {
                let value = value.trim().trim_matches('"').trim_matches('\'');
                virtio_fs_daemon = Some(value.to_string());
            }
        }
    }

    let is_virtiofs = shared_fs.as_deref() == Some("virtio-fs");
    let daemon_exists = virtio_fs_daemon
        .as_ref()
        .map(|p| std::path::Path::new(p).exists())
        .unwrap_or(false);

    (is_virtiofs && daemon_exists, shared_fs, virtio_fs_daemon)
}

/// Validate Kata volume support
pub fn validate_kata_volume_support() -> (bool, Vec<String>) {
    let mut issues = Vec::new();

    let virtiofsd_path = find_virtiofsd();
    if virtiofsd_path.is_none() {
        issues.push(
            "❌ virtiofsd NOT FOUND - Kata volumes will be EMPTY!\n   \
             Install: sudo apt install qemu-system-x86  (or)  sudo apt install virtiofsd"
                .to_string(),
        );
    } else {
        info!("✅ virtiofsd binary: {:?}", virtiofsd_path.as_ref());
    }

    let kata_config = find_kata_config();
    if kata_config.is_none() {
        issues.push(
            "❌ Kata configuration.toml NOT FOUND - is kata-containers installed?".to_string(),
        );
    } else {
        info!("✅ Kata config: {:?}", kata_config.as_ref());
    }

    let (virtiofs_configured, shared_fs, daemon_path) = check_kata_virtiofs_config();

    match shared_fs.as_deref() {
        Some("virtio-fs") => {
            info!("✅ Kata shared_fs = virtio-fs");
        }
        Some("virtio-9p") => {
            issues.push(
                "⚠️ Kata using virtio-9p (SLOW!) - recommend changing to virtio-fs".to_string(),
            );
        }
        Some(other) => {
            issues.push(format!(
                "❌ Kata shared_fs = '{}' - must be 'virtio-fs' for volume support",
                other
            ));
        }
        None => {
            issues.push("❌ Could not determine Kata shared_fs setting".to_string());
        }
    }

    if let Some(daemon) = daemon_path {
        if std::path::Path::new(&daemon).exists() {
            info!("✅ Kata virtio_fs_daemon: {}", daemon);
        } else {
            issues.push(format!(
                "❌ Kata virtio_fs_daemon path '{}' does not exist!",
                daemon
            ));
        }
    }

    let containerd_config = "/etc/containerd/config.toml";
    if std::path::Path::new(containerd_config).exists() {
        if let Ok(content) = std::fs::read_to_string(containerd_config) {
            if content.contains("kata") || content.contains("io.containerd.kata") {
                info!("✅ containerd config references Kata");
            } else {
                issues.push(
                    "⚠️ containerd config.toml doesn't mention Kata - may not be registered"
                        .to_string(),
                );
            }
        }
    } else {
        issues.push(format!(
            "❌ {} not found - containerd not configured",
            containerd_config
        ));
    }

    let can_use_volumes = virtiofs_configured && virtiofsd_path.is_some();
    (can_use_volumes, issues)
}

/// Log Kata setup status
pub fn log_kata_setup_status() {
    info!("🔍 Validating Kata Containers setup...");

    let (volumes_ok, issues) = validate_kata_volume_support();

    if volumes_ok && issues.is_empty() {
        info!("✅ Kata Containers fully validated - VM isolation with virtio-fs volumes ready");
    } else if volumes_ok {
        warn!("⚠️ Kata Containers has warnings:");
        for issue in &issues {
            warn!("   {}", issue);
        }
        info!("   Volumes should still work, but check warnings above");
    } else {
        error!("❌ Kata Containers volume support NOT READY:");
        for issue in &issues {
            error!("   {}", issue);
        }
        error!("   Deployments with persistence=true will have EMPTY volumes!");
        error!("   Either fix the issues above or set DISABLE_KATA=true");
    }
}

/// The `user.*` labels from a nerdctl `ps` label string ("a=b,c=d"), prefix
/// stripped. Values may contain '=' but not ','.
pub fn user_labels(labels: &str) -> HashMap<String, String> {
    labels
        .split(',')
        .filter_map(|kv| kv.split_once('='))
        .filter_map(|(k, v)| {
            k.strip_prefix("user.")
                .map(|k| (k.to_string(), v.to_string()))
        })
        .collect()
}

/// Lifecycle + user labels as nerdctl label pairs.
///   nordkraft.auto_stop / nordkraft.auto_delete / nordkraft.max_lifetime — seconds
///   nordkraft.restart — the spec word (what was asked for; nerdctl holds the policy)
///   user.<key> — free-form labels, kept out of Nordkraft's own namespace
pub fn sandbox_labels(sandbox: &SandboxSpec) -> Vec<(String, String)> {
    use crate::models::parse_duration_secs;
    let mut out = Vec::new();
    let lc = &sandbox.lifecycle;
    if let Some(r) = &lc.restart {
        out.push(("nordkraft.restart".into(), r.clone()));
    }
    for (name, value) in [
        ("auto_stop", &lc.auto_stop),
        ("auto_delete", &lc.auto_delete),
        ("max_lifetime", &lc.max_lifetime),
    ] {
        if let Some(secs) = value.as_deref().and_then(parse_duration_secs) {
            out.push((format!("nordkraft.{name}"), secs.to_string()));
        }
    }
    let mut keys: Vec<_> = sandbox.labels.keys().collect();
    keys.sort();
    for k in keys {
        out.push((format!("user.{k}"), sandbox.labels[k].clone()));
    }
    out
}

#[cfg(test)]
mod sandbox_label_tests {
    use super::*;

    #[test]
    fn labels_are_seconds_and_namespaced() {
        let mut sb = SandboxSpec::default();
        sb.lifecycle.auto_stop = Some("15m".into());
        sb.lifecycle.max_lifetime = Some("0".into());
        sb.lifecycle.restart = Some("never".into());
        sb.labels.insert("workspace".into(), "ws-1".into());
        let got = sandbox_labels(&sb);
        assert!(got.contains(&("nordkraft.auto_stop".into(), "900".into())));
        assert!(got.contains(&("nordkraft.max_lifetime".into(), "0".into())));
        assert!(got.contains(&("nordkraft.restart".into(), "never".into())));
        assert!(got.contains(&("user.workspace".into(), "ws-1".into())));
        assert!(!got.iter().any(|(k, _)| k == "nordkraft.auto_delete"));
    }

    #[test]
    fn user_labels_from_ps_string() {
        let got =
            user_labels("managed_by=nordkraft,user.workspace=ws-1,user.tier=a=b,owner_pubkey=k");
        assert_eq!(got.len(), 2);
        assert_eq!(got["workspace"], "ws-1");
        assert_eq!(got["tier"], "a=b");
    }
}
