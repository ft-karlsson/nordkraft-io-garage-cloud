// src/services/macvlan_manager.rs
//
// Macvlan Network Manager for DUAL-STACK (IPv4 + IPv6) Direct Access
//
// ARCHITECTURE (Feb 2025 - PER-TENANT ISOLATION):
//
// ┌─────────────────────────────────────────────────────────────────────┐
// │                        NETWORK DESIGN                               │
// ├─────────────────────────────────────────────────────────────────────┤
// │                                                                     │
// │  VPN Client (172.20.0.{slot})                                      │
// │       │                                                             │
// │       ▼                                                             │
// │  WireGuard Tunnel                                                   │
// │       │                                                             │
// │       ▼                                                             │
// │  Controller (172.20.0.254)                                         │
// │       │ route: 172.21.{slot}.x/32 via 10.0.0.36                    │
// │       ▼                                                             │
// │  Dell Host (10.0.0.36)                                             │
// │       │                                                             │
// │       ├── enp0s31f6 (physical LAN)                                 │
// │       │                                                             │
// │       ├── nk-shim-t1 (172.21.1.1/32)                               │
// │       │       │  route: 172.21.1.0/24 dev nk-shim-t1               │
// │       │       │                                                     │
// │       │       └── nk-tenant-1 (macvlan, 172.21.1.0/24)            │
// │       │               ├── Container A  172.21.1.10                 │
// │       │               └── Container B  172.21.1.11                 │
// │       │                                                             │
// │       ├── nk-shim-t2 (172.21.2.1/32)                               │
// │       │       │  route: 172.21.2.0/24 dev nk-shim-t2               │
// │       │       │                                                     │
// │       │       └── nk-tenant-2 (macvlan, 172.21.2.0/24)            │
// │       │               └── Container C  172.21.2.2                  │
// │       │                                                             │
// │       └── nk-shim-t3 (172.21.3.1/32)                               │
// │               │  route: 172.21.3.0/24 dev nk-shim-t3               │
// │               │                                                     │
// │               └── nk-tenant-3 (macvlan, 172.21.3.0/24)            │
// │                       └── Container D  172.21.3.2                  │
// │                                                                     │
// │  ISOLATION: Different macvlan networks CANNOT communicate at L2.   │
// │  Tenant-1 containers cannot reach tenant-2 containers even on      │
// │  the same physical host. No firewall rules needed!                 │
// │                                                                     │
// └─────────────────────────────────────────────────────────────────────┘
//
// CRITICAL: SETUP ORDER MATTERS (per tenant)!
//
// 1. Create CNI network FIRST (before shim exists)
//    - CNI checks for IP conflicts
//    - If shim has 172.21.x.x IP, network creation fails with "overlap"
//
// 2. Create shim interface SECOND (after network exists)
//    - Must use /32 mask to avoid claiming the subnet
//    - IP 172.21.{slot}.1 matches the CNI-assigned gateway
//
// 3. Add route THIRD (after shim is up)
//    - "ip route add 172.21.{slot}.0/24 dev nk-shim-t{slot}"
//    - This tells kernel to use shim to reach containers
//
// 4. Add VPN return route ONCE (shared across all tenants)
//    - "ip route add 172.20.0.0/16 via <controller_ip>"

use std::env;
use std::process::Stdio;
use tokio::process::Command;
use tracing::{debug, error, info, warn};

/// Macvlan configuration
#[derive(Debug, Clone)]
pub struct MacvlanConfig {
    pub parent_interface: String, // Physical LAN interface (e.g., enp0s31f6)
    pub runtime: String,          // Container runtime (nerdctl/podman)
    pub vpn_network: String,      // VPN network to route (172.20.0.0/16)
    pub controller_ip: String,    // Controller IP for VPN routing
}

impl Default for MacvlanConfig {
    fn default() -> Self {
        Self {
            parent_interface: env::var("MACVLAN_INTERFACE")
                .unwrap_or_else(|_| "enp0s31f6".to_string()),
            runtime: env::var("CONTAINER_RUNTIME").unwrap_or_else(|_| "nerdctl".to_string()),
            vpn_network: env::var("VPN_NETWORK").unwrap_or_else(|_| "172.20.0.0/16".to_string()),
            controller_ip: env::var("CONTROLLER_IP").unwrap_or_else(|_| "10.0.0.200".to_string()),
        }
    }
}

/// Manages per-tenant macvlan networks for dual-stack direct LAN access
pub struct MacvlanManager {
    config: MacvlanConfig,
}

impl MacvlanManager {
    pub fn new(config: MacvlanConfig) -> Self {
        Self { config }
    }

    pub fn with_defaults() -> Self {
        Self::new(MacvlanConfig::default())
    }

    fn runtime(&self) -> &str {
        &self.config.runtime
    }

    // ============= NAMING CONVENTIONS =============

    /// Network name for a tenant slot: nk-tenant-{slot}
    pub fn network_name_for_slot(&self, user_slot: i32) -> String {
        format!("nk-tenant-{}", user_slot)
    }

    /// Shim interface name for a tenant slot: nk-shim-t{slot}
    /// NOTE: Linux IFNAMSIZ limit = 15 chars. "nk-shim-t" = 9 chars,
    /// leaving room for 6-digit slot numbers. Old "macvlan-shim-t" = 14 chars,
    /// which broke at slot >= 10 (16 chars).
    fn shim_name_for_slot(&self, user_slot: i32) -> String {
        format!("nk-shim-t{}", user_slot)
    }

    /// Subnet for a tenant slot: 172.21.{slot}.0/24
    fn subnet_for_slot(&self, user_slot: i32) -> String {
        format!("172.21.{}.0/24", user_slot)
    }

    /// Gateway/shim IP for a tenant slot: 172.21.{slot}.1
    fn gateway_ip_for_slot(&self, user_slot: i32) -> String {
        format!("172.21.{}.1", user_slot)
    }

    // ============= PER-TENANT SETUP (CORRECT ORDER!) =============

    /// Ensure complete network setup for a specific tenant slot
    ///
    /// ORDER IS CRITICAL:
    /// 1. Network first (CNI checks for IP conflicts)
    /// 2. Shim second (after network, to avoid overlap error)
    /// 3. Tenant route third (to reach containers via shim)
    /// 4. VPN route fourth (for return traffic - shared, idempotent)
    pub async fn ensure_network_for_slot(
        &self,
        user_slot: i32,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let network_name = self.network_name_for_slot(user_slot);
        let subnet = self.subnet_for_slot(user_slot);
        let shim_name = self.shim_name_for_slot(user_slot);
        let gateway_ip = self.gateway_ip_for_slot(user_slot);

        info!(
            "🌐 Ensuring tenant-{} network: {} (subnet: {}, shim: {})",
            user_slot, network_name, subnet, shim_name
        );

        // STEP 1: Create per-tenant macvlan network
        self.ensure_tenant_network(&network_name, &subnet).await?;

        // STEP 2: Create per-tenant shim interface
        self.ensure_tenant_shim(&shim_name, &gateway_ip).await?;

        // STEP 3: Add per-tenant container route
        self.ensure_tenant_route(&subnet, &shim_name).await?;

        // STEP 4: Ensure VPN return route (shared, idempotent)
        self.ensure_vpn_route().await?;

        info!(
            "✅ Tenant-{} network ready: {} via {}",
            user_slot, subnet, shim_name
        );
        Ok(())
    }

    // ============= STEP 1: PER-TENANT NETWORK =============

    /// Create a per-tenant macvlan network (must be done BEFORE shim)
    async fn ensure_tenant_network(
        &self,
        network_name: &str,
        subnet: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Check if network exists
        let check = Command::new(self.runtime())
            .args(["network", "inspect", network_name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await?;

        if check.success() {
            debug!("✅ Network '{}' already exists", network_name);
            return Ok(());
        }

        let parent_opt = format!("parent={}", self.config.parent_interface);

        info!(
            "🌐 Creating macvlan '{}' (subnet: {}, IPv6: SLAAC)",
            network_name, subnet
        );

        // Create with /24 subnet per tenant - CNI auto-assigns .{slot}.1 gateway
        let args = vec![
            "network",
            "create",
            "-d",
            "macvlan",
            "-o",
            &parent_opt,
            "--subnet",
            subnet,
            network_name,
        ];

        let output = Command::new(self.runtime()).args(&args).output().await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.contains("already exists") {
                return Err(
                    format!("Failed to create network '{}': {}", network_name, stderr).into(),
                );
            }
        }

        info!("✅ Created network '{}'", network_name);
        Ok(())
    }

    // ============= STEP 2: PER-TENANT SHIM INTERFACE =============

    /// Create a shim interface for a tenant (must be done AFTER network to avoid overlap)
    async fn ensure_tenant_shim(
        &self,
        shim_name: &str,
        gateway_ip: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Check if shim exists with correct IP
        let check = Command::new("ip")
            .args(["addr", "show", shim_name])
            .output()
            .await?;

        if check.status.success() {
            let stdout = String::from_utf8_lossy(&check.stdout);
            if stdout.contains(&format!("{}/32", gateway_ip)) || stdout.contains(gateway_ip) {
                debug!("✅ Shim '{}' exists with correct IP", shim_name);
                return Ok(());
            } else {
                // Wrong IP - recreate
                warn!("Shim '{}' has wrong IP, recreating...", shim_name);
                let _ = Command::new("ip")
                    .args(["link", "del", shim_name])
                    .output()
                    .await;
            }
        }

        // MIGRATION: Check for old-format shim name (macvlan-shim-tN) and remove it
        // so we can create the new shorter name instead
        if shim_name.starts_with("nk-shim-t") {
            if let Some(slot) = shim_name.strip_prefix("nk-shim-t") {
                let old_name = format!("macvlan-shim-t{}", slot);
                let old_check = Command::new("ip")
                    .args(["addr", "show", &old_name])
                    .output()
                    .await?;
                if old_check.status.success() {
                    info!("🔄 Migrating old shim '{}' → '{}'", old_name, shim_name);
                    let _ = Command::new("ip")
                        .args(["link", "del", &old_name])
                        .output()
                        .await;
                }
            }
        }

        info!(
            "🔧 Creating shim '{}' on '{}' with IP {}/32",
            shim_name, self.config.parent_interface, gateway_ip
        );

        // Create macvlan shim interface
        let create = Command::new("ip")
            .args([
                "link",
                "add",
                shim_name,
                "link",
                &self.config.parent_interface,
                "type",
                "macvlan",
                "mode",
                "bridge",
            ])
            .output()
            .await?;

        if !create.status.success() {
            let stderr = String::from_utf8_lossy(&create.stderr);
            if !stderr.contains("File exists") {
                return Err(format!("Failed to create shim '{}': {}", shim_name, stderr).into());
            }
        }

        // Add IP with /32 mask (CRITICAL: /32 to avoid claiming subnet)
        let add_ip = Command::new("ip")
            .args([
                "addr",
                "add",
                &format!("{}/32", gateway_ip),
                "dev",
                shim_name,
            ])
            .output()
            .await?;

        if !add_ip.status.success() {
            let stderr = String::from_utf8_lossy(&add_ip.stderr);
            if !stderr.contains("File exists") {
                warn!("Failed to add IP to shim '{}': {}", shim_name, stderr);
            }
        }

        // Bring up
        let up = Command::new("ip")
            .args(["link", "set", shim_name, "up"])
            .output()
            .await?;

        if !up.status.success() {
            let stderr = String::from_utf8_lossy(&up.stderr);
            return Err(format!("Failed to bring up shim '{}': {}", shim_name, stderr).into());
        }

        info!("✅ Shim '{}' ready with IP {}/32", shim_name, gateway_ip);
        Ok(())
    }

    // ============= STEP 3: PER-TENANT ROUTE =============

    /// Add route to reach tenant's containers via their shim
    async fn ensure_tenant_route(
        &self,
        subnet: &str,
        shim_name: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Check if route exists pointing at the right shim
        let check = Command::new("ip")
            .args(["route", "show", subnet])
            .output()
            .await?;

        let stdout = String::from_utf8_lossy(&check.stdout);
        if stdout.contains(shim_name) {
            debug!("✅ Tenant route exists: {} dev {}", subnet, shim_name);
            return Ok(());
        }

        // Remove any conflicting route first (e.g., leftover from old /16 setup)
        if !stdout.is_empty() {
            warn!(
                "Removing conflicting route for {} (was: {})",
                subnet,
                stdout.trim()
            );
            let _ = Command::new("ip")
                .args(["route", "del", subnet])
                .output()
                .await;
        }

        info!("🔧 Adding tenant route: {} dev {}", subnet, shim_name);

        let add = Command::new("ip")
            .args(["route", "add", subnet, "dev", shim_name])
            .output()
            .await?;

        if !add.status.success() {
            let stderr = String::from_utf8_lossy(&add.stderr);
            if !stderr.contains("File exists") {
                return Err(format!("Failed to add tenant route: {}", stderr).into());
            }
        }

        info!("✅ Tenant route: {} dev {}", subnet, shim_name);
        Ok(())
    }

    // ============= STEP 4: VPN ROUTE (SHARED) =============

    /// Add route for VPN return traffic (shared across all tenants)
    async fn ensure_vpn_route(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let check = Command::new("ip")
            .args(["route", "show", &self.config.vpn_network])
            .output()
            .await?;

        let stdout = String::from_utf8_lossy(&check.stdout);
        if stdout.contains(&self.config.controller_ip) {
            debug!(
                "✅ VPN route exists: {} via {}",
                self.config.vpn_network, self.config.controller_ip
            );
            return Ok(());
        }

        info!(
            "🔧 Adding VPN route: {} via {}",
            self.config.vpn_network, self.config.controller_ip
        );

        let add = Command::new("ip")
            .args([
                "route",
                "add",
                &self.config.vpn_network,
                "via",
                &self.config.controller_ip,
            ])
            .output()
            .await?;

        if !add.status.success() {
            let stderr = String::from_utf8_lossy(&add.stderr);
            if !stderr.contains("File exists") {
                warn!(
                    "Failed to add VPN route: {} - manual setup may be needed",
                    stderr
                );
            }
        }

        info!(
            "✅ VPN route: {} via {}",
            self.config.vpn_network, self.config.controller_ip
        );
        Ok(())
    }

    // ============= BOOT RECOVERY =============

    /// Discover all existing nk-tenant-* networks and ensure their shims + routes exist.
    /// Called on agent startup to recover from reboot (networks survive, shims don't).
    pub async fn ensure_all_tenant_setups(
        &self,
    ) -> Result<u32, Box<dyn std::error::Error + Send + Sync>> {
        info!("🔍 Discovering existing tenant networks for shim recovery...");

        // List all nerdctl networks, find nk-tenant-* ones
        let output = Command::new(self.runtime())
            .args(["network", "ls", "--format", "{{.Name}}"])
            .output()
            .await?;

        if !output.status.success() {
            return Err("Failed to list networks".into());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut recovered = 0u32;
        let mut has_legacy_macvlan = false;
        let mut has_tenant_1 = false;

        // First pass: detect legacy nk-macvlan and nk-tenant-1
        for line in stdout.lines() {
            let name = line.trim();
            if name == "nk-macvlan" {
                has_legacy_macvlan = true;
            }
            if name == "nk-tenant-1" {
                has_tenant_1 = true;
            }
        }

        // Legacy support: nk-macvlan exists but nk-tenant-1 doesn't
        // This is the original tenant-1 network with old naming
        // Uses /24 like all other tenants — NOT /16 which would conflict
        if has_legacy_macvlan && !has_tenant_1 {
            info!("🔧 Recovering legacy nk-macvlan as tenant-1 (shim=macvlan-shim, subnet=172.21.1.0/24)");

            if let Err(e) = self.ensure_tenant_shim("macvlan-shim", "172.21.0.1").await {
                error!("❌ Failed to recover legacy shim: {}", e);
            } else if let Err(e) = self
                .ensure_tenant_route("172.21.1.0/24", "macvlan-shim")
                .await
            {
                error!("❌ Failed to recover legacy route: {}", e);
            } else {
                recovered += 1;
                info!("✅ Recovered legacy nk-macvlan (tenant-1)");
            }
        }

        for line in stdout.lines() {
            let name = line.trim();
            if !name.starts_with("nk-tenant-") {
                continue;
            }

            // Extract slot number from "nk-tenant-{slot}"
            let slot_str = match name.strip_prefix("nk-tenant-") {
                Some(s) => s,
                None => continue,
            };
            let slot: i32 = match slot_str.parse() {
                Ok(s) => s,
                Err(_) => {
                    warn!("Skipping network with non-numeric slot: {}", name);
                    continue;
                }
            };

            let shim_name = self.shim_name_for_slot(slot);
            let gateway_ip = self.gateway_ip_for_slot(slot);
            let subnet = self.subnet_for_slot(slot);

            info!(
                "🔧 Recovering tenant-{}: shim={}, subnet={}",
                slot, shim_name, subnet
            );

            // Network already exists (it survived reboot), just need shim + route
            if let Err(e) = self.ensure_tenant_shim(&shim_name, &gateway_ip).await {
                error!("❌ Failed to recover shim for tenant-{}: {}", slot, e);
                continue;
            }

            if let Err(e) = self.ensure_tenant_route(&subnet, &shim_name).await {
                error!("❌ Failed to recover route for tenant-{}: {}", slot, e);
                continue;
            }

            recovered += 1;
            info!("✅ Recovered tenant-{} shim + route", slot);
        }

        // Always ensure VPN return route
        self.ensure_vpn_route().await?;

        info!(
            "✅ Boot recovery complete: {} tenant networks recovered",
            recovered
        );
        Ok(recovered)
    }

    // ============= CONTAINER NETWORK ARGS =============

    /// Get runtime args to attach container to per-tenant macvlan with static IPv4
    pub fn get_network_args_with_ipv4(&self, user_slot: i32, ipv4_address: &str) -> Vec<String> {
        vec![
            "--network".to_string(),
            self.network_name_for_slot(user_slot),
            "--ip".to_string(),
            ipv4_address.to_string(),
        ]
    }

    // ============= IP RETRIEVAL =============

    /// Get container's IPv4 address
    pub async fn get_container_ipv4(
        &self,
        container_name: &str,
    ) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
        let output = Command::new(self.runtime())
            .args(["inspect", container_name])
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("Failed to inspect '{}': {}", container_name, stderr).into());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);

        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&stdout) {
            if let Some(container) = parsed.as_array().and_then(|arr| arr.first()) {
                // Try NetworkSettings.IPAddress first
                if let Some(ip) = container
                    .get("NetworkSettings")
                    .and_then(|ns| ns.get("IPAddress"))
                    .and_then(|v| v.as_str())
                {
                    if !ip.is_empty() {
                        return Ok(Some(ip.to_string()));
                    }
                }

                // Try Networks map
                if let Some(networks) = container
                    .get("NetworkSettings")
                    .and_then(|ns| ns.get("Networks"))
                    .and_then(|n| n.as_object())
                {
                    for (_name, network) in networks {
                        if let Some(ip) = network.get("IPAddress").and_then(|v| v.as_str()) {
                            if !ip.is_empty() {
                                return Ok(Some(ip.to_string()));
                            }
                        }
                    }
                }
            }
        }

        Ok(None)
    }

    /// Get container's IPv6 address (SLAAC)
    pub async fn get_container_ipv6(
        &self,
        container_name: &str,
    ) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
        let output = Command::new(self.runtime())
            .args([
                "exec",
                container_name,
                "ip",
                "-6",
                "addr",
                "show",
                "scope",
                "global",
            ])
            .output()
            .await?;

        if !output.status.success() {
            return Ok(None);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);

        for line in stdout.lines() {
            let line = line.trim();
            if line.starts_with("inet6 ") && line.contains("scope global") {
                if let Some(addr_part) = line.strip_prefix("inet6 ") {
                    if let Some(addr) = addr_part.split_whitespace().next() {
                        let addr = addr.split('/').next().unwrap_or(addr);
                        if !addr.starts_with("fe80:") {
                            return Ok(Some(addr.to_string()));
                        }
                    }
                }
            }
        }

        Ok(None)
    }

    /// Wait for SLAAC IPv6
    pub async fn wait_for_slaac_ipv6(
        &self,
        container_name: &str,
    ) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
        for delay_ms in [500, 1000, 1500] {
            tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
            if let Ok(Some(ipv6)) = self.get_container_ipv6(container_name).await {
                return Ok(Some(ipv6));
            }
        }
        Ok(None)
    }

    // ============= DIAGNOSTICS =============

    /// Verify setup for all discovered tenant networks
    pub async fn verify_setup(&self) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let mut all_ok = true;

        // Check parent interface
        let link = Command::new("ip")
            .args(["link", "show", &self.config.parent_interface])
            .output()
            .await?;

        if link.status.success() {
            info!("✅ Parent interface '{}'", self.config.parent_interface);
        } else {
            error!(
                "❌ Parent interface '{}' not found!",
                self.config.parent_interface
            );
            return Ok(false);
        }

        // Discover tenant networks and verify each
        let output = Command::new(self.runtime())
            .args(["network", "ls", "--format", "{{.Name}}"])
            .output()
            .await?;

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let mut found_tenants = 0;

            // Check legacy nk-macvlan (tenant-1 with old naming)
            let has_legacy = stdout.lines().any(|l| l.trim() == "nk-macvlan");
            let has_tenant_1 = stdout.lines().any(|l| l.trim() == "nk-tenant-1");

            if has_legacy && !has_tenant_1 {
                found_tenants += 1;
                info!("✅ Network 'nk-macvlan' (legacy tenant-1)");

                let shim = Command::new("ip")
                    .args(["addr", "show", "macvlan-shim"])
                    .output()
                    .await?;

                if shim.status.success() {
                    let shim_out = String::from_utf8_lossy(&shim.stdout);
                    if shim_out.contains("172.21.0.1") {
                        info!("✅ Shim 'macvlan-shim' = 172.21.0.1 (legacy)");
                    } else {
                        warn!("⚠️ Shim 'macvlan-shim' exists but wrong IP");
                        all_ok = false;
                    }
                } else {
                    warn!("⚠️ Shim 'macvlan-shim' not found (legacy)");
                    all_ok = false;
                }

                let route = Command::new("ip")
                    .args(["route", "show", "172.21.1.0/24"])
                    .output()
                    .await?;

                let route_out = String::from_utf8_lossy(&route.stdout);
                if route_out.contains("macvlan-shim") {
                    info!("✅ Route: 172.21.1.0/24 dev macvlan-shim (legacy)");
                } else {
                    warn!("⚠️ Route missing: 172.21.1.0/24 dev macvlan-shim (legacy)");
                    all_ok = false;
                }
            }

            for line in stdout.lines() {
                let name = line.trim();
                if !name.starts_with("nk-tenant-") {
                    continue;
                }

                let slot_str = match name.strip_prefix("nk-tenant-") {
                    Some(s) => s,
                    None => continue,
                };
                let slot: i32 = match slot_str.parse() {
                    Ok(s) => s,
                    Err(_) => continue,
                };

                found_tenants += 1;
                let shim_name = self.shim_name_for_slot(slot);
                let gateway_ip = self.gateway_ip_for_slot(slot);
                let subnet = self.subnet_for_slot(slot);

                // Check network
                info!("✅ Network '{}'", name);

                // Check shim
                let shim = Command::new("ip")
                    .args(["addr", "show", &shim_name])
                    .output()
                    .await?;

                if shim.status.success() {
                    let shim_out = String::from_utf8_lossy(&shim.stdout);
                    if shim_out.contains(&gateway_ip) {
                        info!("✅ Shim '{}' = {}", shim_name, gateway_ip);
                    } else {
                        warn!("⚠️ Shim '{}' exists but wrong IP", shim_name);
                        all_ok = false;
                    }
                } else {
                    warn!("⚠️ Shim '{}' not found", shim_name);
                    all_ok = false;
                }

                // Check route
                let route = Command::new("ip")
                    .args(["route", "show", &subnet])
                    .output()
                    .await?;

                let route_out = String::from_utf8_lossy(&route.stdout);
                if route_out.contains(&shim_name) {
                    info!("✅ Route: {} dev {}", subnet, shim_name);
                } else {
                    warn!("⚠️ Route missing: {} dev {}", subnet, shim_name);
                    all_ok = false;
                }
            }

            if found_tenants == 0 {
                info!("ℹ️ No tenant networks found yet (first deploy will create one)");
            }
        }

        // Check VPN route
        let vpn = Command::new("ip")
            .args(["route", "show", &self.config.vpn_network])
            .output()
            .await?;

        let vpn_stdout = String::from_utf8_lossy(&vpn.stdout);
        if vpn_stdout.contains(&self.config.controller_ip) {
            info!(
                "✅ VPN route: {} via {}",
                self.config.vpn_network, self.config.controller_ip
            );
        } else {
            warn!("⚠️ VPN route missing");
            all_ok = false;
        }

        Ok(all_ok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_network_name_for_slot() {
        let manager = MacvlanManager::with_defaults();
        assert_eq!(manager.network_name_for_slot(1), "nk-tenant-1");
        assert_eq!(manager.network_name_for_slot(5), "nk-tenant-5");
        assert_eq!(manager.network_name_for_slot(254), "nk-tenant-254");
    }

    #[test]
    fn test_shim_name_for_slot() {
        let manager = MacvlanManager::with_defaults();
        assert_eq!(manager.shim_name_for_slot(1), "nk-shim-t1");
        assert_eq!(manager.shim_name_for_slot(3), "nk-shim-t3");
        assert_eq!(manager.shim_name_for_slot(13), "nk-shim-t13");
        assert_eq!(manager.shim_name_for_slot(999), "nk-shim-t999");
        // Verify all fit within Linux IFNAMSIZ (15 chars)
        for slot in [1, 9, 10, 99, 100, 999, 9999] {
            let name = manager.shim_name_for_slot(slot);
            assert!(name.len() <= 15, "shim name '{}' exceeds 15 chars!", name);
        }
    }

    #[test]
    fn test_subnet_for_slot() {
        let manager = MacvlanManager::with_defaults();
        assert_eq!(manager.subnet_for_slot(1), "172.21.1.0/24");
        assert_eq!(manager.subnet_for_slot(5), "172.21.5.0/24");
    }

    #[test]
    fn test_gateway_ip_for_slot() {
        let manager = MacvlanManager::with_defaults();
        assert_eq!(manager.gateway_ip_for_slot(1), "172.21.1.1");
        assert_eq!(manager.gateway_ip_for_slot(3), "172.21.3.1");
    }

    #[test]
    fn test_network_args() {
        let manager = MacvlanManager::with_defaults();
        let args = manager.get_network_args_with_ipv4(1, "172.21.1.10");
        assert_eq!(
            args,
            vec!["--network", "nk-tenant-1", "--ip", "172.21.1.10"]
        );
    }

    #[test]
    fn test_network_args_different_slot() {
        let manager = MacvlanManager::with_defaults();
        let args = manager.get_network_args_with_ipv4(3, "172.21.3.2");
        assert_eq!(args, vec!["--network", "nk-tenant-3", "--ip", "172.21.3.2"]);
    }

    #[test]
    fn test_default_config() {
        let config = MacvlanConfig::default();
        assert_eq!(config.controller_ip, "10.0.0.200");
        assert_eq!(config.vpn_network, "172.20.0.0/16");
    }
}
