// src/services/peer_resolver.rs
//
// Embedded WireGuard peer resolver - no separate auth-api service needed.
// Directly calls `wg show wg0` to map VPN IPs → public keys.
// The peer cache refreshes every 30 seconds in the background.
//
// WgReconciler: Periodically compares DB state with WireGuard runtime.
// If peers are missing (e.g. after systemd-networkd restart triggered by
// unattended-upgrades), re-provisions them from the database.

use rocket::http::Status;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::RwLock;
use tokio::time::{interval, Duration};
use tracing::{debug, error, info, warn};

// ============= TYPES =============

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PeerInfo {
    pub public_key: String,
    pub vpn_ip: String,
    pub endpoint: Option<String>,
    pub latest_handshake: Option<String>,
    pub transfer_rx: Option<String>,
    pub transfer_tx: Option<String>,
}

// ============= PEER CACHE =============
// Thread-safe cache of WireGuard peers, refreshed in background

#[derive(Clone)]
pub struct PeerCache {
    peers: Arc<RwLock<HashMap<String, PeerInfo>>>,
    last_update: Arc<RwLock<String>>,
    wg_interface: String,
}

impl PeerCache {
    pub fn new(wg_interface: &str) -> Self {
        Self {
            peers: Arc::new(RwLock::new(HashMap::new())),
            last_update: Arc::new(RwLock::new(String::new())),
            wg_interface: wg_interface.to_string(),
        }
    }

    /// Initialize cache with first peer fetch and spawn background refresh
    pub async fn start(&self) {
        // Initial fetch
        match parse_wg_peers(&self.wg_interface).await {
            Ok(initial_peers) => {
                let count = initial_peers.len();
                *self.peers.write().await = initial_peers;
                *self.last_update.write().await = chrono::Utc::now().to_rfc3339();
                info!("🔑 Peer cache initialized with {} peers", count);
            }
            Err(e) => {
                warn!("⚠️ Failed to load initial peer cache: {}", e);
                warn!("   This is expected if WireGuard is not running (dev mode)");
            }
        }

        // Spawn background refresh task
        let peers = Arc::clone(&self.peers);
        let last_update = Arc::clone(&self.last_update);
        let wg_interface = self.wg_interface.clone();

        tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(30));
            let mut error_count: u32 = 0;

            loop {
                interval.tick().await;

                match parse_wg_peers(&wg_interface).await {
                    Ok(new_peers) => {
                        let count = new_peers.len();
                        *peers.write().await = new_peers;
                        *last_update.write().await = chrono::Utc::now().to_rfc3339();

                        if error_count > 0 {
                            info!(
                                "✅ Peer cache recovered after {} errors ({} peers)",
                                error_count, count
                            );
                            error_count = 0;
                        } else {
                            debug!("Refreshed peer cache: {} peers", count);
                        }
                    }
                    Err(e) => {
                        error_count += 1;
                        if error_count <= 3 || error_count.is_multiple_of(60) {
                            error!(
                                "Failed to refresh peer cache (attempt {}): {}",
                                error_count, e
                            );
                        }
                    }
                }
            }
        });
    }

    /// Resolve a VPN IP to its peer info (public key, etc.)
    pub async fn resolve(&self, vpn_ip: &str) -> Option<PeerInfo> {
        self.peers.read().await.get(vpn_ip).cloned()
    }

    /// Get all cached peers (useful for debugging/health checks)
    pub async fn all_peers(&self) -> Vec<PeerInfo> {
        self.peers.read().await.values().cloned().collect()
    }

    /// Get peer count and last update time
    pub async fn health(&self) -> (usize, String) {
        let count = self.peers.read().await.len();
        let updated = self.last_update.read().await.clone();
        (count, updated)
    }
}

// ============= WG RECONCILER =============
// Periodically compares DB state with WireGuard runtime state.
// If peers are missing (e.g. after systemd-networkd restart / netplan apply),
// re-provisions them from the database — same logic as boot_recovery.

use sqlx::{PgPool, Row};

#[derive(Clone)]
pub struct WgReconciler {
    wg_interface: String,
    db_pool: PgPool,
    interval_secs: u64,
}

impl WgReconciler {
    pub fn new(wg_interface: &str, db_pool: PgPool) -> Self {
        Self {
            wg_interface: wg_interface.to_string(),
            db_pool,
            interval_secs: 300, // 5 minutes
        }
    }

    /// Spawn background reconciliation loop
    pub async fn start(&self) {
        let wg_interface = self.wg_interface.clone();
        let db_pool = self.db_pool.clone();
        let interval_secs = self.interval_secs;

        tokio::spawn(async move {
            // Wait one interval before first check (boot_recovery handles startup)
            let mut tick = interval(Duration::from_secs(interval_secs));
            tick.tick().await; // skip immediate first tick

            loop {
                tick.tick().await;

                if let Err(e) = reconcile_peers(&wg_interface, &db_pool).await {
                    error!("WG reconciler error: {}", e);
                }
            }
        });

        info!(
            "🔄 WG reconciler started (interval: {}s)",
            self.interval_secs
        );
    }
}

/// Compare DB peers with WireGuard runtime, re-add any missing.
async fn reconcile_peers(
    wg_interface: &str,
    db_pool: &PgPool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Step 1: Get current WireGuard peers (public keys)
    let output = Command::new("wg")
        .args(["show", wg_interface, "peers"])
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("wg show peers failed: {}", stderr).into());
    }

    let wg_peers: std::collections::HashSet<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    // Step 2: Get expected peers from DB (exclude unclaimed NKINVITE tokens)
    let rows = sqlx::query(
        "SELECT wireguard_public_key, user_slot FROM users \
         WHERE account_status = 'active' AND user_slot IS NOT NULL \
         AND wireguard_public_key NOT LIKE 'NKINVITE%'",
    )
    .fetch_all(db_pool)
    .await?;

    // Step 3: Find missing peers and re-provision
    let mut restored = 0u32;

    for row in &rows {
        let public_key: String = row.get("wireguard_public_key");
        let user_slot: i32 = row.get("user_slot");
        let slot = user_slot as u32;

        if !wg_peers.contains(&public_key) {
            warn!(
                "🔄 Reconciler: peer missing for slot {} ({}...), re-adding",
                slot,
                &public_key[..8.min(public_key.len())]
            );

            // Re-add WireGuard peer
            let allowed_ips = format!("172.20.0.{}/32,172.21.{}.0/24", slot, slot);
            let wg_result = Command::new("wg")
                .args([
                    "set",
                    wg_interface,
                    "peer",
                    &public_key,
                    "allowed-ips",
                    &allowed_ips,
                ])
                .output()
                .await;

            match wg_result {
                Ok(o) if o.status.success() => {
                    info!("🔄 Reconciler: WG peer restored for slot {}", slot);
                }
                Ok(o) => {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    error!("🔄 Reconciler: wg set failed for slot {}: {}", slot, stderr);
                    continue;
                }
                Err(e) => {
                    error!("🔄 Reconciler: wg command failed for slot {}: {}", slot, e);
                    continue;
                }
            }

            // Re-add nftables rule (idempotent — checks for existing rule)
            let nft_check = Command::new("nft")
                .args(["list", "chain", "inet", "filter", "forward"])
                .output()
                .await;

            let needs_nft = match nft_check {
                Ok(o) if o.status.success() => {
                    let output_str = String::from_utf8_lossy(&o.stdout);
                    !output_str.contains(&format!("172.20.0.{}", slot))
                }
                _ => true,
            };

            if needs_nft {
                let nft_result = Command::new("bash")
                    .args(["-c", &format!(
                        "nft insert rule inet filter forward iifname \\\"wg0\\\" ip saddr 172.20.0.{slot} ip daddr 172.21.{slot}.0/24 accept comment \\\"tenant-{slot}\\\"",
                        slot = slot
                    )])
                    .output()
                    .await;

                match nft_result {
                    Ok(o) if o.status.success() => {
                        info!("🔄 Reconciler: nft rule restored for slot {}", slot);
                    }
                    Ok(o) => {
                        let stderr = String::from_utf8_lossy(&o.stderr);
                        error!(
                            "🔄 Reconciler: nft insert failed for slot {}: {}",
                            slot, stderr
                        );
                    }
                    Err(e) => {
                        error!("🔄 Reconciler: nft command failed for slot {}: {}", slot, e);
                    }
                }
            }

            restored += 1;
        }
    }

    if restored > 0 {
        warn!(
            "🔄 Reconciler: restored {} missing peer(s) + nft rules",
            restored
        );
    } else {
        debug!("🔄 Reconciler: all {} peers present", rows.len());
    }

    Ok(())
}

// ============= WG PARSING =============

/// Parse `wg show <interface>` output into a map of VPN IP → PeerInfo
async fn parse_wg_peers(
    wg_interface: &str,
) -> Result<HashMap<String, PeerInfo>, Box<dyn std::error::Error + Send + Sync>> {
    let output = Command::new("sudo")
        .args(["wg", "show", wg_interface])
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("wg show failed: {}", stderr).into());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut map = HashMap::new();
    let mut current_peer: Option<PeerInfo> = None;

    for line in stdout.lines() {
        let line = line.trim();

        if line.starts_with("peer:") {
            // Save previous peer
            if let Some(peer) = current_peer.take() {
                insert_peer(&mut map, peer);
            }
            // Start new peer
            let key = line.strip_prefix("peer:").unwrap_or("").trim().to_string();
            current_peer = Some(PeerInfo {
                public_key: key,
                vpn_ip: String::new(),
                endpoint: None,
                latest_handshake: None,
                transfer_rx: None,
                transfer_tx: None,
            });
        } else if let Some(peer) = current_peer.as_mut() {
            if line.starts_with("endpoint:") {
                peer.endpoint = Some(
                    line.strip_prefix("endpoint:")
                        .unwrap_or("")
                        .trim()
                        .to_string(),
                );
            } else if line.starts_with("allowed ips:") {
                let ips_str = line.strip_prefix("allowed ips:").unwrap_or("").trim();
                let mut vpn_ips = Vec::new();

                for ip_cidr in ips_str.split(',') {
                    let ip_cidr = ip_cidr.trim();
                    // Only keep WireGuard VPN IPs (172.20.x.x/32)
                    // Skip container subnets and other routes
                    if ip_cidr.starts_with("172.20.") && ip_cidr.ends_with("/32") {
                        if let Some(ip) = ip_cidr.split('/').next() {
                            vpn_ips.push(ip.to_string());
                        }
                    }
                }

                peer.vpn_ip = vpn_ips.join(",");
            } else if line.starts_with("latest handshake:") {
                peer.latest_handshake = Some(
                    line.strip_prefix("latest handshake:")
                        .unwrap_or("")
                        .trim()
                        .to_string(),
                );
            } else if line.starts_with("transfer:") {
                // "transfer: 1.15 KiB received, 856 B sent"
                let transfer = line.strip_prefix("transfer:").unwrap_or("").trim();
                let parts: Vec<&str> = transfer.split("received,").collect();
                if parts.len() == 2 {
                    peer.transfer_rx = Some(parts[0].trim().to_string());
                    peer.transfer_tx = Some(
                        parts[1]
                            .trim()
                            .strip_suffix("sent")
                            .unwrap_or(parts[1].trim())
                            .trim()
                            .to_string(),
                    );
                }
            }
        }
    }

    // Don't forget the last peer
    if let Some(peer) = current_peer.take() {
        insert_peer(&mut map, peer);
    }

    debug!("Parsed {} peer entries from WireGuard", map.len());
    Ok(map)
}

/// Insert a peer into the map, creating one entry per VPN IP
fn insert_peer(map: &mut HashMap<String, PeerInfo>, peer: PeerInfo) {
    if peer.vpn_ip.is_empty() {
        return;
    }

    for ip in peer.vpn_ip.split(',') {
        let ip = ip.trim();
        if !ip.is_empty() {
            let mut entry = peer.clone();
            entry.vpn_ip = ip.to_string();
            map.insert(ip.to_string(), entry);
        }
    }
}

// ============= PUBLIC RESOLVE FUNCTION =============
// This is the function called by guards.rs during authentication

/// Resolve a client IP to a PeerInfo using either the embedded cache (production)
/// or a mock response (dev mode).
pub async fn resolve_peer(
    client_ip: &str,
    peer_cache: &PeerCache,
    dev_mode: bool,
    dev_user_public_key: &str,
) -> Result<PeerInfo, Status> {
    // Dev mode: return mock peer
    if dev_mode {
        debug!("DEV MODE: Bypassing peer resolution for IP {}", client_ip);
        return Ok(PeerInfo {
            public_key: dev_user_public_key.to_string(),
            vpn_ip: client_ip.to_string(),
            endpoint: None,
            latest_handshake: None,
            transfer_rx: None,
            transfer_tx: None,
        });
    }

    // Production: look up in the peer cache
    match peer_cache.resolve(client_ip).await {
        Some(peer) => {
            info!(
                "✅ Resolved {} → public key {}...",
                client_ip,
                &peer.public_key[..20.min(peer.public_key.len())]
            );
            Ok(peer)
        }
        None => {
            warn!("❌ No WireGuard peer found for IP: {}", client_ip);
            Err(Status::Unauthorized)
        }
    }
}

// ============= TESTS =============

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_insert_peer_single_ip() {
        let mut map = HashMap::new();
        let peer = PeerInfo {
            public_key: "testkey123".to_string(),
            vpn_ip: "172.20.1.1".to_string(),
            endpoint: None,
            latest_handshake: None,
            transfer_rx: None,
            transfer_tx: None,
        };
        insert_peer(&mut map, peer);
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("172.20.1.1"));
    }

    #[test]
    fn test_insert_peer_empty_ip() {
        let mut map = HashMap::new();
        let peer = PeerInfo {
            public_key: "testkey123".to_string(),
            vpn_ip: String::new(),
            endpoint: None,
            latest_handshake: None,
            transfer_rx: None,
            transfer_tx: None,
        };
        insert_peer(&mut map, peer);
        assert_eq!(map.len(), 0);
    }
}
