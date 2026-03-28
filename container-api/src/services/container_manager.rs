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

use crate::config::AppConfig;
use crate::models::{ContainerInfo, PortMapping, PortSpec};
use crate::services::macvlan_manager::MacvlanManager;
use crate::services::nats_service::{NatsMessage, NatsService, NatsSubjects};
use crate::services::persistence::PersistenceManager;
use std::collections::HashMap;
use tracing::{debug, error, info, warn};

pub struct ContainerManager {
    runtime_binary: String, // "nerdctl" or "podman"
    use_kata: bool,
    kata_runtime_class: String,
    restart_policy: String,
    persistence: PersistenceManager,
    macvlan: MacvlanManager,
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
            persistence: PersistenceManager::new(),
            macvlan: MacvlanManager::with_defaults(),
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

        // ========== MACVLAN NETWORK (DUAL-STACK) ==========
        // Ensure per-user macvlan network exists
        let user_slot = user_slot.ok_or("user_slot required for network allocation")?;

        // Verify interface setup on first deploy
        if let Err(e) = self.macvlan.verify_setup().await {
            warn!("Interface verification failed: {} - continuing anyway", e);
        }

        self.macvlan.ensure_network_for_slot(user_slot).await?;

        // Build command args
        let mut args = vec![
            "run".to_string(),
            "-d".to_string(),
            "--name".to_string(),
            container_name.clone(),
            "--restart".to_string(),
            self.restart_policy.clone(),
            "--stop-timeout".to_string(),
            "30".to_string(),
        ];

        // ========== NETWORK CONFIGURATION (MACVLAN ONLY) ==========
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

        // Add Kata runtime if enabled
        if self.use_kata {
            args.extend_from_slice(&["--runtime".to_string(), self.kata_runtime_class.clone()]);
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
        args.push(image.to_string());

        // Optional command override
        if let Some(cmd) = command {
            args.extend(cmd);
        }

        debug!("Running: {} {}", self.runtime_binary, args.join(" "));

        let output = tokio::process::Command::new(&self.runtime_binary)
            .args(&args)
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            error!("Container creation failed: {}", stderr);
            return Err(stderr.into());
        }

        // ========== IPv4 VERIFICATION ==========
        // Get container IPv4 from the macvlan network
        let container_ip = match self.macvlan.get_container_ipv4(&container_name).await? {
            Some(ip) => ip,
            None => {
                error!("Failed to get IPv4 from container. Removing...");
                let _ = tokio::process::Command::new(&self.runtime_binary)
                    .args(["rm", "-f", &container_name])
                    .output()
                    .await;
                return Err("Container IPv4 assignment failed".into());
            }
        };

        // Verify IPv4 assignment matches what we requested
        if container_ip != allocated_ip {
            error!(
                "🚨 IP MISMATCH! Expected {} but container got {}. \
                 This will break routing. Removing container.",
                allocated_ip, container_ip
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
        let final_ipv6 = if enable_ipv6 {
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
            let port_mappings: Vec<PortMapping> = ports
                .iter()
                .map(|port_spec| {
                    let access_url = if let Some(ref ip) = container_ip {
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
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let containers = self.list_user_containers(owner_pubkey).await?;
        let container = containers
            .iter()
            .find(|c| c.container_id == container_id || c.name == container_id);

        if container.is_none() {
            return Err("Container not found or access denied".into());
        }

        let container_name = container.unwrap().name.clone();

        // Stop first if running
        let _ = tokio::process::Command::new(&self.runtime_binary)
            .args(["stop", &container_name])
            .output()
            .await;

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
