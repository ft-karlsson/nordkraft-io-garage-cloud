// src/services/route_manager.rs
// Ensure routes to hardware/agent nodes on controller or host.
//
// RouteReconciler: Periodically compares OS routing table with DB state.
// If routes are missing (e.g. after systemd-networkd restart triggered by
// unattended-upgrades / apt-daily-upgrade), re-provisions them from the database.

use std::net::IpAddr;
use std::process::Stdio;
use std::sync::Arc;
use sqlx::PgPool;
use tokio::process::Command;
use tokio::time::{interval, Duration};
use tracing::{debug, error, info, warn};

pub struct StaticRouteManager {
    use_sudo: bool,
}

impl Default for StaticRouteManager {
    fn default() -> Self {
        Self::new()
    }
}

impl StaticRouteManager {
    pub fn new() -> Self {
        let use_sudo = std::env::var("ROUTE_USE_SUDO")
            .unwrap_or_else(|_| "false".to_string())
            .parse()
            .unwrap_or(false);

        if use_sudo {
            warn!("🔧 Route manager will use sudo for ip commands");
        }

        Self { use_sudo }
    }

    /// Strip /32 or any CIDR suffix from IP address
    fn strip_cidr(ip: &str) -> &str {
        ip.split('/').next().unwrap_or(ip)
    }

    /// Add a /32 route for a container
    pub async fn add_container_route(
        &self,
        container_ip: &str,
        node_ip: &str,
        interface: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Strip CIDR suffix if present before validation
        let ip_only = Self::strip_cidr(container_ip);
        let node_ip_only = Self::strip_cidr(node_ip);

        let validated_ip = Self::validate_ip(ip_only)?;
        let validated_node_ip = Self::validate_ip(node_ip_only)?;
        let validated_interface = Self::validate_interface(interface)?;

        // Always use /32 for container routes
        let route_spec = format!("{}/32", validated_ip);

        let mut args = vec![
            "route",
            "add",
            &route_spec,
            "via",
            &validated_node_ip,
            "dev",
            &validated_interface,
        ];

        let cmd = if self.use_sudo { "sudo" } else { "ip" };
        if self.use_sudo {
            args.insert(0, "ip");
        }

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            Command::new(cmd)
                .args(&args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output(),
        )
        .await
        .map_err(|_| "Route command timeout")??;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);

            if stderr.contains("File exists") {
                debug!(
                    "Route already exists: {} via {}",
                    route_spec, validated_node_ip
                );
                return Ok(());
            }

            return Err(format!("Failed to add route: {}", stderr).into());
        }

        info!(
            "✅ Added route: {} via {} dev {}",
            route_spec, validated_node_ip, validated_interface
        );
        Ok(())
    }

    /// Remove a /32 route
    pub async fn remove_container_route(
        &self,
        container_ip: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Strip CIDR suffix if present before validation
        let ip_only = Self::strip_cidr(container_ip);
        let validated_ip = Self::validate_ip(ip_only)?;

        let route_spec = format!("{}/32", validated_ip);
        let mut args = vec!["route", "del", &route_spec];

        let cmd = if self.use_sudo { "sudo" } else { "ip" };
        if self.use_sudo {
            args.insert(0, "ip");
        }

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            Command::new(cmd)
                .args(&args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output(),
        )
        .await
        .map_err(|_| "Route command timeout")??;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);

            if stderr.contains("No such process") || stderr.contains("not found") {
                warn!("Route doesn't exist (already removed?): {}", route_spec);
                return Ok(());
            }

            return Err(format!("Failed to remove route: {}", stderr).into());
        }

        info!("🗑️ Removed route: {}", route_spec);
        Ok(())
    }

    /// Check if route exists
    pub async fn route_exists(
        &self,
        container_ip: &str,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        // Strip CIDR suffix if present before validation
        let ip_only = Self::strip_cidr(container_ip);
        let validated_ip = Self::validate_ip(ip_only)?;

        let output = Command::new("ip")
            .args(["route", "show", &format!("{}/32", validated_ip)])
            .output()
            .await?;

        Ok(!output.stdout.is_empty())
    }

    // SECURITY: Strict input validation
    fn validate_ip(ip: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let parsed: IpAddr = ip
            .parse()
            .map_err(|_| format!("Invalid IP address: {}", ip))?;

        Ok(parsed.to_string())
    }

    fn validate_interface(iface: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        const ALLOWED: &[&str] = &[
            "eth0", "eth1", "eth2", "eth3", "ens3", "ens4", "ens5", "enp0s3", "enp0s8", "wlan0",
            "wlan1", "br0", "br1",
        ];

        let is_valid = ALLOWED.contains(&iface)
            || iface.starts_with("eth")
            || iface.starts_with("ens")
            || iface.starts_with("enp");

        if !is_valid {
            return Err(format!("Invalid network interface: {}", iface).into());
        }

        Ok(iface.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_cidr() {
        assert_eq!(StaticRouteManager::strip_cidr("172.21.1.35"), "172.21.1.35");
        assert_eq!(
            StaticRouteManager::strip_cidr("172.21.1.35/32"),
            "172.21.1.35"
        );
        assert_eq!(StaticRouteManager::strip_cidr("10.0.0.0/24"), "10.0.0.0");
    }

    #[test]
    fn test_validate_ip() {
        assert!(StaticRouteManager::validate_ip("172.21.3.15").is_ok());
        assert!(StaticRouteManager::validate_ip("10.0.0.36").is_ok());
        assert!(StaticRouteManager::validate_ip("invalid").is_err());
        assert!(StaticRouteManager::validate_ip("999.999.999.999").is_err());
        assert!(StaticRouteManager::validate_ip("172.21.3.15; rm -rf /").is_err());
        // Now handles CIDR via strip_cidr, but validate_ip itself should fail on CIDR
        assert!(StaticRouteManager::validate_ip("172.21.3.15/32").is_err());
    }
}

// ============= ROUTE RECONCILER =============
// Periodically compares OS routing table with DB state.
// If routes are missing (e.g. after systemd-networkd restart triggered by
// unattended-upgrades / apt-daily-upgrade / netplan apply),
// re-provisions them from the database — reuses sync_routes_on_startup logic.

#[derive(Clone)]
pub struct RouteReconciler {
    route_manager: Arc<StaticRouteManager>,
    db_pool: PgPool,
    interval_secs: u64,
}

impl RouteReconciler {
    pub fn new(route_manager: Arc<StaticRouteManager>, db_pool: PgPool) -> Self {
        Self {
            route_manager,
            db_pool,
            interval_secs: 300, // 5 minutes, same as WgReconciler
        }
    }

    /// Spawn background reconciliation loop
    pub async fn start(&self) {
        let route_manager = Arc::clone(&self.route_manager);
        let db_pool = self.db_pool.clone();
        let interval_secs = self.interval_secs;

        tokio::spawn(async move {
            // Wait one interval before first check (sync_routes_on_startup handles boot)
            let mut tick = interval(Duration::from_secs(interval_secs));
            tick.tick().await; // skip immediate first tick

            loop {
                tick.tick().await;

                if let Err(e) = reconcile_routes(&route_manager, &db_pool).await {
                    error!("Route reconciler error: {}", e);
                }
            }
        });

        info!(
            "🔄 Route reconciler started (interval: {}s)",
            self.interval_secs
        );
    }
}

/// Compare DB routes with OS routing table, re-add any missing.
async fn reconcile_routes(
    route_manager: &StaticRouteManager,
    db_pool: &PgPool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use sqlx::Row;

    // Step 1: Get expected routes from DB (running containers with IPs)
    let db_routes = sqlx::query(
        "SELECT c.internal_ip, c.container_name, n.internal_ip AS node_ip, n.network_interface
         FROM containers c
         JOIN nodes n ON c.node_id = n.node_id
         WHERE c.status = 'running' AND c.internal_ip IS NOT NULL",
    )
    .fetch_all(db_pool)
    .await?;

    if db_routes.is_empty() {
        debug!("🔄 Route reconciler: no running containers in DB");
        return Ok(());
    }

    // Step 2: Check each expected route, re-add if missing
    let mut restored = 0u32;

    for row in &db_routes {
        let container_ip: String = row.get("internal_ip");
        let container_name: String = row.get("container_name");
        let node_ip: String = row.get("node_ip");
        let interface: String = row.get("network_interface");

        match route_manager.route_exists(&container_ip).await {
            Ok(true) => {
                // Route present, nothing to do
            }
            Ok(false) => {
                warn!(
                    "🔄 Route reconciler: route missing for {} ({}), re-adding",
                    container_name, container_ip
                );
                match route_manager
                    .add_container_route(&container_ip, &node_ip, &interface)
                    .await
                {
                    Ok(_) => {
                        info!(
                            "🔄 Route reconciler: restored route {} -> {}",
                            container_name, container_ip
                        );
                        restored += 1;
                    }
                    Err(e) => {
                        error!(
                            "🔄 Route reconciler: failed to restore route {} ({}): {}",
                            container_name, container_ip, e
                        );
                    }
                }
            }
            Err(e) => {
                warn!(
                    "🔄 Route reconciler: failed to check route {} ({}): {}",
                    container_name, container_ip, e
                );
            }
        }
    }

    if restored > 0 {
        warn!(
            "🔄 Route reconciler: restored {} missing route(s)",
            restored
        );
    } else {
        debug!(
            "🔄 Route reconciler: all {} routes present",
            db_routes.len()
        );
    }

    Ok(())
}
