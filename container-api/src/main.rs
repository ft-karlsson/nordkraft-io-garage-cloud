// main.rs - With pfSense firewall + HAProxy ingress + AGENT MACVLAN SETUP
#[macro_use]
extern crate rocket;
use sqlx::{PgPool, Row};
use std::sync::Arc;
use tracing::{error, info, warn};

mod config;
mod controller;
mod guards;
mod models;
mod routes;
mod services;
mod storage;

use config::AppConfig;
use controller::OrchestratorService;
use models::*;
use services::container_manager::ContainerManager;
use services::haproxy_client::{DummyHAProxyClient, HAProxyClient, HAProxyClientTrait};
use services::nats_service::NatsService;
use services::peer_resolver::{PeerCache, WgReconciler};
use services::pfsense_client::{DummyPfSenseClient, PfSenseClient, PfSenseClientTrait};
use services::route_manager::StaticRouteManager;

use crate::services::macvlan_manager::MacvlanManager;

use rocket::serde::{json::Json, Deserialize};

// ============= DATABASE =============

/// Database pool managed via Rocket State (no rocket_db_pools dependency)
pub type DbPool = PgPool;

// ============= APP STATE =============

pub struct AppState {
    pub config: AppConfig,
    pub container_manager: Arc<ContainerManager>,
    pub route_manager: Arc<StaticRouteManager>,
    pub peer_cache: PeerCache,
}

impl AppState {
    pub fn new(config: AppConfig, peer_cache: PeerCache) -> Self {
        let container_manager = Arc::new(ContainerManager::new(&config));
        let route_manager = Arc::new(StaticRouteManager::new());

        Self {
            config,
            container_manager,
            route_manager,
            peer_cache,
        }
    }

    pub async fn get_user_by_public_key(&self, public_key: &str, pool: &PgPool) -> Option<User> {
        // Dev mode shortcut
        if self.config.dev_mode && public_key == self.config.dev_user_public_key {
            return Some(User {
                id: "dev-user-id".to_string(),
                email: "dev@example.com".to_string(),
                full_name: "Development User".to_string(),
                wireguard_public_key: public_key.to_string(),
                wireguard_ip: "172.20.0.99".to_string(),
                plan_id: "dev-plan".to_string(),
                account_status: "active".to_string(),
                allowed_actions: vec![
                    "deploy".to_string(),
                    "list".to_string(),
                    "delete".to_string(),
                    "stop".to_string(),
                ],
                primary_garage_id: "ry".to_string(),
                user_slot: Some(99),
            });
        }

        // Production lookup
        let result = sqlx::query(
            "SELECT id, email, full_name, wireguard_public_key, wireguard_ip, 
                    plan_id, account_status, primary_garage_id, user_slot 
             FROM users 
             WHERE wireguard_public_key = $1 AND account_status = 'active'",
        )
        .bind(public_key)
        .fetch_optional(pool)
        .await
        .ok()?;

        result.map(|row| User {
            id: row.get("id"),
            email: row.get("email"),
            full_name: row.get("full_name"),
            wireguard_public_key: row.get("wireguard_public_key"),
            wireguard_ip: row.get("wireguard_ip"),
            plan_id: row.get("plan_id"),
            account_status: row.get("account_status"),
            allowed_actions: vec![
                "deploy".to_string(),
                "list".to_string(),
                "stop".to_string(),
                "delete".to_string(),
            ],
            primary_garage_id: row.get("primary_garage_id"),
            user_slot: row.get("user_slot"),
        })
    }
}

// ============= PFSENSE CLIENT INIT =============

fn init_pfsense_client() -> Arc<dyn PfSenseClientTrait> {
    let pfsense_url = std::env::var("PFSENSE_API_URL").ok();
    let pfsense_key = std::env::var("PFSENSE_API_KEY").ok();
    // print key to ensure envs are set
    println!("{:?}", pfsense_key);
    let pfsense_interface =
        std::env::var("PFSENSE_WAN_INTERFACE").unwrap_or_else(|_| "wan".to_string());
    let pfsense_verify_ssl = std::env::var("PFSENSE_VERIFY_SSL")
        .map(|v| v == "true")
        .unwrap_or(false);

    match (pfsense_url, pfsense_key) {
        (Some(url), Some(key)) if !url.is_empty() && !key.is_empty() => {
            match PfSenseClient::new(url, key, pfsense_interface, pfsense_verify_ssl) {
                Ok(client) => {
                    info!("✅ pfSense API client initialized");
                    Arc::new(client) as Arc<dyn PfSenseClientTrait>
                }
                Err(e) => {
                    warn!("⚠️ Failed to initialize pfSense client: {}", e);
                    Arc::new(DummyPfSenseClient::new()) as Arc<dyn PfSenseClientTrait>
                }
            }
        }
        _ => {
            info!("ℹ️ pfSense API not configured - firewall rules require manual management");
            Arc::new(DummyPfSenseClient::new()) as Arc<dyn PfSenseClientTrait>
        }
    }
}

// ============= HAPROXY CLIENT INIT =============

fn init_haproxy_client(_config: &AppConfig) -> Arc<dyn HAProxyClientTrait> {
    // Check if ingress is enabled
    let ingress_enabled = std::env::var("INGRESS_ENABLED")
        .map(|v| v == "true")
        .unwrap_or(false);

    if !ingress_enabled {
        info!("🚪 Ingress disabled, using dummy HAProxy client");
        let base_domain =
            std::env::var("INGRESS_BASE_DOMAIN").unwrap_or_else(|_| "example.dk".to_string());
        let public_ip =
            std::env::var("INGRESS_PUBLIC_IP").unwrap_or_else(|_| "203.0.113.1".to_string());
        return Arc::new(DummyHAProxyClient::new(base_domain, public_ip));
    }

    let pfsense_url = std::env::var("PFSENSE_API_URL").ok();
    let pfsense_key = std::env::var("PFSENSE_API_KEY").ok();
    let base_domain =
        std::env::var("INGRESS_BASE_DOMAIN").unwrap_or_else(|_| "example.dk".to_string());
    let public_ip =
        std::env::var("INGRESS_PUBLIC_IP").unwrap_or_else(|_| "203.0.113.1".to_string());
    let http_frontend =
        std::env::var("HAPROXY_HTTP_FRONTEND").unwrap_or_else(|_| "http_frontend".to_string());
    let https_frontend =
        std::env::var("HAPROXY_HTTPS_FRONTEND").unwrap_or_else(|_| "https_frontend".to_string());

    match (pfsense_url, pfsense_key) {
        (Some(url), Some(key)) if !url.is_empty() && !key.is_empty() => {
            match HAProxyClient::new(
                url,
                key,
                base_domain.clone(),
                public_ip.clone(),
                http_frontend,
                https_frontend,
            ) {
                Ok(client) => {
                    info!(
                        "✅ HAProxy client initialized for ingress: *.{}",
                        base_domain
                    );
                    Arc::new(client) as Arc<dyn HAProxyClientTrait>
                }
                Err(e) => {
                    error!("❌ Failed to create HAProxy client: {}", e);
                    Arc::new(DummyHAProxyClient::new(base_domain, public_ip))
                }
            }
        }
        _ => {
            warn!("⚠️ Ingress enabled but pfSense API not configured");
            Arc::new(DummyHAProxyClient::new(base_domain, public_ip))
        }
    }
}

// ============= AGENT NETWORK SETUP =============

/// Ensure agent has all necessary routes for VPN traffic
async fn ensure_agent_vpn_routes(
    config: &AppConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Only do this on agent nodes
    if matches!(config.mode, config::OperationMode::Controller) {
        return Ok(());
    }

    info!("🔧 Setting up agent VPN return routes...");

    // Get config values with defaults
    let controller_ip = std::env::var("CONTROLLER_IP").unwrap_or_else(|_| "10.0.0.200".to_string());
    let interface =
        std::env::var("AGENT_NETWORK_INTERFACE").unwrap_or_else(|_| "enp0s31f6".to_string());

    // VPN network route (for return traffic to VPN clients)
    add_route_if_missing(
        &config.vpn_network,
        &controller_ip,
        &interface,
        "VPN return route",
    )
    .await?;

    info!("✅ Agent VPN routes configured");
    info!(
        "   VPN {} via {} dev {}",
        config.vpn_network, controller_ip, interface
    );
    info!(
        "   Local network {} - already routable via {}",
        config.local_network, interface
    );
    Ok(())
}

async fn add_route_if_missing(
    network: &str,
    via: &str,
    dev: &str,
    description: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Check if route exists
    let check = tokio::process::Command::new("ip")
        .args(["route", "show", network])
        .output()
        .await?;

    if check.stdout.is_empty() {
        info!(
            "🔧 Adding route: {} ({}) via {} dev {}",
            network, description, via, dev
        );

        let result = tokio::process::Command::new("ip")
            .args(["route", "add", network, "via", via, "dev", dev])
            .output()
            .await?;

        if !result.status.success() {
            let stderr = String::from_utf8_lossy(&result.stderr);
            if !stderr.contains("File exists") {
                error!("Failed to add route for {}: {}", network, stderr);
                return Err(format!("Route addition failed: {}", stderr).into());
            }
        }

        info!("✅ Added route: {} via {} dev {}", network, via, dev);
    } else {
        info!("✅ Route already exists: {}", network);
    }

    Ok(())
}

// ============= AGENT MACVLAN SETUP (NEW!) =============

/// Ensure all per-tenant macvlan shims and routes exist on agent startup
/// This is CRITICAL for reboot survival - networks persist but shims don't.
/// Discovers all nk-tenant-* networks and recreates their shim interfaces + routes.
async fn ensure_agent_macvlan_setup(
    macvlan_manager: &MacvlanManager,
    config: &AppConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Only do this on agent/hybrid nodes that run containers
    if matches!(config.mode, config::OperationMode::Controller) {
        info!("⏭️ Skipping macvlan setup (controller mode)");
        return Ok(());
    }

    info!("🌐 Ensuring per-tenant macvlan setup on agent startup...");
    info!("   Recovering shim interfaces + routes after reboot");

    // Discover all nk-tenant-* networks and recreate their shims + routes
    match macvlan_manager.ensure_all_tenant_setups().await {
        Ok(count) => {
            info!("✅ Macvlan boot recovery: {} tenant networks ready", count);

            // Verify the setup
            match macvlan_manager.verify_setup().await {
                Ok(true) => info!("✅ Macvlan verification passed"),
                Ok(false) => {
                    warn!("⚠️ Macvlan verification found issues - containers may not be reachable")
                }
                Err(e) => warn!("⚠️ Could not verify macvlan setup: {}", e),
            }

            Ok(())
        }
        Err(e) => {
            error!("❌ Failed to setup macvlan: {}", e);
            error!("   Containers will exist but be UNREACHABLE!");
            // Don't panic - let the API start, but warn loudly
            // Admin can manually fix and containers will work
            Ok(())
        }
    }
}

// ============= MAIN =============

// ============= TENANT PROVISIONING (wg set + nft insert) =============

/// Add a WireGuard peer for a tenant slot. Instant, no restart needed.
async fn wg_add_peer(
    wg_interface: &str,
    public_key: &str,
    user_slot: u32,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let allowed_ips = format!("172.20.0.{}/32,172.21.{}.0/24", user_slot, user_slot);

    let output = tokio::process::Command::new("wg")
        .args([
            "set",
            wg_interface,
            "peer",
            public_key,
            "allowed-ips",
            &allowed_ips,
        ])
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("wg set failed for slot {}: {}", user_slot, stderr).into());
    }

    info!(
        "🔑 WireGuard peer added: slot {} → {}",
        user_slot,
        &public_key[..8]
    );
    Ok(())
}

/// Remove a WireGuard peer.
async fn wg_remove_peer(
    wg_interface: &str,
    public_key: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let output = tokio::process::Command::new("wg")
        .args(["set", wg_interface, "peer", public_key, "remove"])
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("wg peer remove failed: {}", stderr).into());
    }

    info!("🔑 WireGuard peer removed: {}", &public_key[..8]);
    Ok(())
}

/// Add an nftables forwarding rule for a tenant. Instant, no reload needed.
/// Uses `nft insert` so it goes at the top of the chain (before any drop rules).
async fn nft_add_tenant_rule(
    user_slot: u32,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // First check if rule already exists (idempotent)
    let check = tokio::process::Command::new("nft")
        .args(["list", "chain", "inet", "filter", "forward"])
        .output()
        .await?;

    if check.status.success() {
        let output_str = String::from_utf8_lossy(&check.stdout);
        let pattern = format!("172.20.0.{}", user_slot);
        if output_str.contains(&pattern) {
            info!("🔥 nftables rule already exists for slot {}", user_slot);
            return Ok(());
        }
    }

    // Insert rule at top of forward chain
    let output = tokio::process::Command::new("bash")
        .args(["-c", &format!(
            "nft insert rule inet filter forward iifname \\\"wg0\\\" ip saddr 172.20.0.{slot} ip daddr 172.21.{slot}.0/24 accept comment \\\"tenant-{slot}\\\"",
            slot = user_slot
        )])
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("nft insert failed for slot {}: {}", user_slot, stderr).into());
    }

    info!(
        "🔥 nftables rule added: slot {} (172.20.0.{} → 172.21.{}.0/24)",
        user_slot, user_slot, user_slot
    );
    Ok(())
}

/// Remove nftables rule for a tenant slot.
/// Remove nftables rule for a tenant slot.
async fn nft_remove_tenant_rule(
    user_slot: u32,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Find the rule handle, then delete by handle
    let output = tokio::process::Command::new("bash")
        .args(["-c", &format!(
            "nft -a list chain inet filter forward | grep 'tenant-{}' | grep -oP 'handle \\K[0-9]+'",
            user_slot
        )])
        .output()
        .await?;

    if output.status.success() {
        let handles = String::from_utf8_lossy(&output.stdout);
        for handle in handles.trim().lines() {
            let del = tokio::process::Command::new("nft")
                .args([
                    "delete",
                    "rule",
                    "inet",
                    "filter",
                    "forward",
                    "handle",
                    handle.trim(),
                ])
                .output()
                .await?;

            if del.status.success() {
                info!(
                    "🔥 nftables rule removed: slot {} (handle {})",
                    user_slot,
                    handle.trim()
                );
            }
        }
    }

    Ok(())
}

/// Provision a tenant: add WireGuard peer + nftables rule.
/// Called from admin endpoint on signup AND from boot recovery.
async fn provision_tenant(
    wg_interface: &str,
    public_key: &str,
    user_slot: u32,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!(
        "🔧 Provisioning tenant slot {} (key: {}...)",
        user_slot,
        &public_key[..8.min(public_key.len())]
    );

    // Step 1: WireGuard peer
    wg_add_peer(wg_interface, public_key, user_slot).await?;

    // Step 2: nftables forwarding rule
    nft_add_tenant_rule(user_slot).await?;

    info!("✅ Tenant slot {} fully provisioned", user_slot);
    Ok(())
}

/// Deprovision a tenant: remove WireGuard peer + nftables rule.
async fn deprovision_tenant(
    wg_interface: &str,
    public_key: &str,
    user_slot: u32,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("🔧 Deprovisioning tenant slot {}", user_slot);

    wg_remove_peer(wg_interface, public_key).await?;
    nft_remove_tenant_rule(user_slot).await?;

    info!("✅ Tenant slot {} deprovisioned", user_slot);
    Ok(())
}

// ============= BOOT RECOVERY =============

/// On startup, restore all WireGuard peers and nftables rules from database.
/// This is the single source of truth pattern — DB drives runtime state.
async fn boot_recovery_from_db(
    pool: &PgPool,
    wg_interface: &str,
) -> Result<BootRecoveryReport, Box<dyn std::error::Error + Send + Sync>> {
    info!("🔄 Boot recovery: restoring WireGuard peers + nftables rules from database...");

    let rows = sqlx::query(
        "SELECT wireguard_public_key, user_slot FROM users WHERE account_status = 'active' AND user_slot IS NOT NULL"
    )
    .fetch_all(pool)
    .await?;

    let mut report = BootRecoveryReport::default();

    for row in &rows {
        let public_key: String = row.get("wireguard_public_key");
        let user_slot: i32 = row.get("user_slot");
        let slot = user_slot as u32;

        // Restore WireGuard peer
        match wg_add_peer(wg_interface, &public_key, slot).await {
            Ok(_) => report.wg_peers_restored += 1,
            Err(e) => {
                error!("Failed to restore WG peer for slot {}: {}", slot, e);
                report.wg_peers_failed += 1;
            }
        }

        // Restore nftables rule
        match nft_add_tenant_rule(slot).await {
            Ok(_) => report.nft_rules_restored += 1,
            Err(e) => {
                error!("Failed to restore nft rule for slot {}: {}", slot, e);
                report.nft_rules_failed += 1;
            }
        }
    }

    info!(
        "🔄 Boot recovery complete: {} WG peers ({} failed), {} nft rules ({} failed)",
        report.wg_peers_restored,
        report.wg_peers_failed,
        report.nft_rules_restored,
        report.nft_rules_failed
    );

    Ok(report)
}

#[derive(Debug, Default)]
struct BootRecoveryReport {
    wg_peers_restored: u32,
    wg_peers_failed: u32,
    nft_rules_restored: u32,
    nft_rules_failed: u32,
}

// ============= ADMIN API GUARD =============

/// Guard for admin endpoints: validates API key + source IP
pub struct AdminAuth;

#[rocket::async_trait]
impl<'r> rocket::request::FromRequest<'r> for AdminAuth {
    type Error = String;

    async fn from_request(
        request: &'r rocket::Request<'_>,
    ) -> rocket::request::Outcome<Self, Self::Error> {
        let app_state = match request.guard::<&rocket::State<AppState>>().await {
            rocket::request::Outcome::Success(s) => s,
            _ => {
                return rocket::request::Outcome::Error((
                    rocket::http::Status::InternalServerError,
                    "App state unavailable".to_string(),
                ))
            }
        };

        // Check API key
        let provided_key = request.headers().get_one("X-Admin-API-Key").unwrap_or("");
        if provided_key != app_state.config.admin_api_key {
            warn!("🚫 Admin API: invalid API key from {:?}", request.remote());
            return rocket::request::Outcome::Error((
                rocket::http::Status::Forbidden,
                "Invalid admin API key".to_string(),
            ));
        }

        // Check source IP
        let client_ip = request
            .remote()
            .map(|addr| addr.ip().to_string())
            .unwrap_or_default();

        if !app_state.config.admin_allowed_ips.contains(&client_ip) {
            warn!(
                "🚫 Admin API: blocked source IP {} (allowed: {:?})",
                client_ip, app_state.config.admin_allowed_ips
            );
            return rocket::request::Outcome::Error((
                rocket::http::Status::Forbidden,
                "Source IP not allowed".to_string(),
            ));
        }

        rocket::request::Outcome::Success(AdminAuth)
    }
}

// ============= ADMIN ROUTES =============

#[derive(Debug, Deserialize)]
struct TenantProvisionRequest {
    public_key: String,
    user_slot: u32,
}

#[derive(Debug, Deserialize)]
struct TenantDeprovisionRequest {
    public_key: String,
    user_slot: u32,
}

/// Called by signup-api after DB insert. Adds WireGuard peer + nftables rule instantly.
#[post("/admin/tenant/provision", data = "<req>")]
async fn admin_provision_tenant(
    _admin: AdminAuth,
    req: Json<TenantProvisionRequest>,
    app_state: &rocket::State<AppState>,
) -> Json<serde_json::Value> {
    let wg_interface = &app_state.config.wg_interface;

    match provision_tenant(wg_interface, &req.public_key, req.user_slot).await {
        Ok(_) => Json(serde_json::json!({
            "status": "provisioned",
            "user_slot": req.user_slot,
            "vpn_ip": format!("172.20.0.{}", req.user_slot),
            "container_subnet": format!("172.21.{}.0/24", req.user_slot),
        })),
        Err(e) => {
            error!(
                "❌ Tenant provisioning failed for slot {}: {}",
                req.user_slot, e
            );
            Json(serde_json::json!({
                "error": format!("Provisioning failed: {}", e),
            }))
        }
    }
}

/// Called to remove a tenant's networking. Removes WireGuard peer + nftables rule.
#[post("/admin/tenant/deprovision", data = "<req>")]
async fn admin_deprovision_tenant(
    _admin: AdminAuth,
    req: Json<TenantDeprovisionRequest>,
    app_state: &rocket::State<AppState>,
) -> Json<serde_json::Value> {
    let wg_interface = &app_state.config.wg_interface;

    match deprovision_tenant(wg_interface, &req.public_key, req.user_slot).await {
        Ok(_) => Json(serde_json::json!({
            "status": "deprovisioned",
            "user_slot": req.user_slot,
        })),
        Err(e) => {
            error!(
                "❌ Tenant deprovisioning failed for slot {}: {}",
                req.user_slot, e
            );
            Json(serde_json::json!({
                "error": format!("Deprovisioning failed: {}", e),
            }))
        }
    }
}

/// Health check for admin API — shows active tenant count
#[get("/admin/tenants/status")]
async fn admin_tenants_status(
    _admin: AdminAuth,
    app_state: &rocket::State<AppState>,
) -> Json<serde_json::Value> {
    // Count current WireGuard peers
    let wg_output = tokio::process::Command::new("wg")
        .args(["show", &app_state.config.wg_interface, "peers"])
        .output()
        .await;

    let peer_count = wg_output
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().count())
        .unwrap_or(0);

    // Count current nft rules
    let nft_output = tokio::process::Command::new("bash")
        .args([
            "-c",
            "nft list chain inet filter forward 2>/dev/null | grep -c 'tenant-'",
        ])
        .output()
        .await;

    let nft_count: usize = nft_output
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse()
                .unwrap_or(0)
        })
        .unwrap_or(0);

    Json(serde_json::json!({
        "wireguard_peers": peer_count,
        "nftables_tenant_rules": nft_count,
        "wg_interface": app_state.config.wg_interface,
    }))
}

#[launch]
async fn rocket() -> _ {
    tracing_subscriber::fmt::init();

    let config = config::init_config();

    // =========================================================
    // DATABASE POOL - direct sqlx, no rocket_db_pools wrapper
    // =========================================================
    let database_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://nordkraft:nordkraft@localhost/nordkraft".to_string());

    let db_pool = match PgPool::connect(&database_url).await {
        Ok(pool) => {
            info!("✅ Database pool connected");
            pool
        }
        Err(e) => {
            // In agent mode we don't need a DB, so warn but continue
            warn!("⚠️ Database connection failed: {} (OK for agent mode)", e);
            // Create a pool that will fail on use - agents don't need it
            PgPool::connect_lazy(&database_url).unwrap()
        }
    };

    // =========================================================
    // EMBEDDED PEER CACHE - replaces the separate auth-api
    // Calls `sudo wg show wg0` every 5 seconds to build
    // VPN IP → WireGuard public key mapping in-process.
    // =========================================================
    let wg_interface = std::env::var("WG_INTERFACE").unwrap_or_else(|_| "wg0".to_string());
    let peer_cache = PeerCache::new(&wg_interface);
    peer_cache.start().await;

    // =========================================================
    // WG RECONCILER - re-provisions missing peers from DB
    // Catches peers lost to systemd-networkd restarts, netplan
    // apply, unattended-upgrades, etc. Runs every 5 minutes.
    // =========================================================
    if matches!(
        config.mode,
        config::OperationMode::Controller | config::OperationMode::Hybrid
    ) {
        let reconciler = WgReconciler::new(&wg_interface, db_pool.clone());
        reconciler.start().await;
    }

    let app_state = AppState::new(config.clone(), peer_cache);

    // Initialize clients
    let pfsense_client = init_pfsense_client();
    let haproxy_client = init_haproxy_client(&config);
    let macvlan_manager = Arc::new(MacvlanManager::with_defaults());

    // Initialize NATS
    let nats_service = if config.nats_enabled {
        match NatsService::new(
            &config.nats_url,
            config.node_id.clone(),
            matches!(
                config.mode,
                config::OperationMode::Controller | config::OperationMode::Hybrid
            ),
        )
        .await
        {
            Ok(service) => {
                info!("✅ NATS service initialized");
                Some(Arc::new(service))
            }
            Err(e) => {
                error!("❌ Failed to initialize NATS: {}", e);
                None
            }
        }
    } else {
        info!("📡 NATS disabled");
        None
    };

    // Create orchestrator
    let orchestrator = OrchestratorService::new(
        &config,
        nats_service.clone(),
        Arc::clone(&app_state.container_manager),
    );

    // Start background tasks
    orchestrator.run_background_tasks().await;

    // Controller-specific setup
    if let Some(nats) = &nats_service {
        if nats.is_controller() {
            nats.start_cluster_state_broadcast(
                config.cluster_state_broadcast_interval,
                Arc::clone(&orchestrator.nodes),
            )
            .await;

            // Sync routes from database using managed pool
            {
                match storage::sync_routes_on_startup(&app_state.route_manager, &db_pool).await {
                    Ok(storage::SyncReport {
                        added,
                        skipped,
                        failed,
                        stale_removed,
                    }) => {
                        info!(
                                "Routes synced: {added} added, {skipped} skipped, {failed} failed, {stale_removed} stale removed"
                            )
                    }
                    Err(e) => error!("Failed to sync routes: {}", e),
                }

                // =========================================================
                // BOOT RECOVERY: Restore WireGuard peers + nftables rules
                // Database is single source of truth. On every boot:
                //   1. `wg set` for each active tenant
                //   2. `nft insert rule` for each active tenant
                // No netplan peers, no static nftables tenant rules needed.
                // =========================================================
                match boot_recovery_from_db(&db_pool, &config.wg_interface).await {
                    Ok(report) => {
                        info!(
                            "✅ Boot recovery: {} WG peers, {} nft rules restored",
                            report.wg_peers_restored, report.nft_rules_restored
                        );
                        if report.wg_peers_failed > 0 || report.nft_rules_failed > 0 {
                            warn!(
                                "⚠️ Boot recovery failures: {} WG, {} nft",
                                report.wg_peers_failed, report.nft_rules_failed
                            );
                        }
                    }
                    Err(e) => error!("❌ Boot recovery failed: {}", e),
                }
            }
        }
    }

    // =========================================================
    // AGENT-SPECIFIC SETUP - CRITICAL FOR REBOOT SURVIVAL
    // =========================================================

    // Setup VPN return routes (so containers can reply to VPN clients)
    if let Err(e) = ensure_agent_vpn_routes(&config).await {
        error!("❌ CRITICAL: Failed to setup agent VPN routes: {}", e);
        panic!("Agent cannot start without return routes - security requirement");
    }

    // NEW: Setup macvlan shim + routes (so containers are reachable)
    // This is idempotent - safe to run every boot
    if let Err(e) = ensure_agent_macvlan_setup(&macvlan_manager, &config).await {
        error!("❌ WARNING: Macvlan setup failed: {}", e);
        // Don't panic - containers may still work if setup was manual
    }

    // Log startup info
    log_startup_info(&config, nats_service.is_some());

    // Build Rocket
    let rocket_config = rocket::Config::figment()
        .merge(("address", config.bind_address.clone()))
        .merge(("port", config.bind_port));

    let rocket = rocket::custom(rocket_config)
        .manage(app_state)
        .manage(orchestrator)
        .manage(pfsense_client)
        .manage(haproxy_client)
        .manage(macvlan_manager)
        .manage(db_pool);

    // Mount routes based on mode
    match config.mode {
        config::OperationMode::Agent => {
            // Agent: no database, minimal routes
            rocket.mount(
                "/api",
                routes![routes::status::get_status, routes::nodes::list_nodes,],
            )
        }
        _ => {
            // Controller/Hybrid: full functionality
            rocket.mount(
                "/api",
                routes![
                    // Container operations
                    routes::containers::deploy_container,
                    routes::containers::list_containers_route,
                    routes::containers::delete_container,
                    routes::containers::start_container,
                    routes::containers::stop_container,
                    routes::containers::get_container_logs,
                    routes::containers::inspect_container,
                    routes::containers::upgrade_container,
                    // Node operations
                    routes::nodes::list_nodes,
                    routes::nodes::register_node,
                    // Status & auth
                    routes::status::get_status,
                    routes::status::verify_auth,
                    routes::status::get_network_info,
                    // IPv6 firewall management
                    routes::ipv6::open_ipv6_firewall,
                    routes::ipv6::close_ipv6_firewall,
                    routes::ipv6::get_ipv6_status,
                    routes::ipv6::list_ipv6_allocations,
                    routes::ipv6::update_ipv6_ports,
                    // Ingress routes (HAProxy + ACME)
                    routes::ingress::enable_ingress,
                    routes::ingress::disable_ingress,
                    routes::ingress::get_ingress_status,
                    routes::ingress::list_ingress,
                    // Admin endpoints (signup-api → container-api provisioning)
                    admin_provision_tenant,
                    admin_deprovision_tenant,
                    admin_tenants_status,
                    // usage
                    routes::containers::get_usage,
                ],
            )
        }
    }
}

fn log_startup_info(config: &AppConfig, nats_connected: bool) {
    match config.mode {
        config::OperationMode::Controller => info!("🎛️ Starting in CONTROLLER mode"),
        config::OperationMode::Agent => info!("🤖 Starting in AGENT mode"),
        config::OperationMode::Hybrid => info!("🔄 Starting in HYBRID mode"),
    }

    if config.nats_enabled {
        if nats_connected {
            info!("✅ NATS connected: {}", config.nats_url);
        } else {
            warn!("⚠️ NATS connection failed");
        }
    }

    // Log ingress status
    let ingress_enabled = std::env::var("INGRESS_ENABLED")
        .map(|v| v == "true")
        .unwrap_or(false);
    if ingress_enabled {
        let base_domain =
            std::env::var("INGRESS_BASE_DOMAIN").unwrap_or_else(|_| "localhost.local".to_string());
        let public_ip =
            std::env::var("INGRESS_PUBLIC_IP").unwrap_or_else(|_| "127.0.0.1".to_string());
        info!("🚪 Ingress enabled: *.{} → {}", base_domain, public_ip);
    }

    if config.dev_mode {
        warn!("🚨 DEV MODE ENABLED 🚨");
        warn!("- Peer resolution bypassed");
        warn!("- Dev key: {}", config.dev_user_public_key);
    } else {
        info!("🔒 Production mode");
        info!("🔑 Embedded peer resolver active (no separate auth-api needed)");
    }

    info!("🔐 Admin API: allowed IPs = {:?}", config.admin_allowed_ips);
    info!("🔄 Boot recovery: DB → wg set + nft insert (no static tenant rules needed)");
}
