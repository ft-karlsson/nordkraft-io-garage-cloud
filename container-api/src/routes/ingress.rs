// src/routes/ingress.rs
//
// API endpoints for managing HTTP/HTTPS/TCP ingress via HAProxy.
//
// MARK I INGRESS MODES (Simplified with Wildcard Cert):
// ======================================================
// 1. HTTP (port 80)   - Host header routing, no TLS
// 2. HTTPS (port 443) - HAProxy terminates TLS with WILDCARD cert (*.example.dk)
//                       NO per-subdomain cert issuance needed!
// 3. TCP (10000-10999)- Raw TCP passthrough, container handles TLS if needed
//
// KEY SIMPLIFICATION:
// -------------------
// The wildcard certificate *.example.dk is ALREADY bound to the HTTPS frontend.
// For new subdomains, we ONLY need to:
//   1. Create a backend pointing to the container
//   2. Create an ACL matching the Host header
//   3. Create an action routing ACL → backend
// That's it! No ACME, no polling, no cert binding per-subdomain.
//
// Endpoints:
//   POST   /api/ingress/<container_id>/enable  - Enable ingress for container
//   DELETE /api/ingress/<container_id>/disable - Disable ingress
//   GET    /api/ingress/<container_id>/status  - Get ingress status
//   GET    /api/ingress/list                   - List user's ingress routes

// TODO:
// This requires already setup of  front-ends for catching http
// and https on port 80 and 443 on pfsense HAproxy. This should be check if
//  exist on binary startup and created if not.

use crate::guards::AuthenticatedUser;
use crate::services::haproxy_client::HAProxyClientTrait;
use crate::services::pfsense_client::PfSenseClientTrait;
use crate::storage;
use crate::AppState;

use rocket::serde::json::Json;
use rocket::serde::{Deserialize, Serialize};
use rocket::State;
use std::sync::Arc;
use tracing::{info, warn};

// ============= REQUEST/RESPONSE TYPES =============

#[derive(Debug, Deserialize)]
pub struct EnableIngressRequest {
    /// Subdomain to use (e.g., "myapp" → myapp.example.dk)
    pub subdomain: String,
    /// Mode: "http" (default), "https", or "tcp"
    #[serde(default = "default_mode")]
    pub mode: String,
    /// Container port to forward to (default: 80 for http/https)
    pub target_port: Option<u16>,
}

fn default_mode() -> String {
    "https".to_string() // Default to HTTPS since we have wildcard cert
}

#[derive(Debug, Serialize)]
pub struct IngressInfo {
    pub container_id: String,
    pub subdomain: String,
    pub mode: String,
    pub url: String,
    pub target_ip: String,
    pub target_port: i32,
    pub public_port: Option<i32>,
    pub firewall_open: bool,
    pub is_active: bool,
    pub created_at: String,
}

// ============= VALIDATION =============

fn validate_subdomain(subdomain: &str) -> Result<(), String> {
    let subdomain = subdomain.to_lowercase();

    if subdomain.len() < 3 {
        return Err("Subdomain must be at least 3 characters".to_string());
    }
    if subdomain.len() > 63 {
        return Err("Subdomain must be 63 characters or less".to_string());
    }

    if !subdomain
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(
            "Subdomain can only contain lowercase letters, numbers, and hyphens".to_string(),
        );
    }

    if subdomain.starts_with('-') || subdomain.ends_with('-') {
        return Err("Subdomain cannot start or end with a hyphen".to_string());
    }

    if subdomain.contains("--") {
        return Err("Subdomain cannot contain consecutive hyphens".to_string());
    }

    // Reserved subdomains
    let reserved = [
        "www", "api", "admin", "mail", "ftp", "ssh", "vpn", "ns1", "ns2",
    ];
    if reserved.contains(&subdomain.as_str()) {
        return Err(format!("Subdomain '{}' is reserved", subdomain));
    }

    Ok(())
}

// ============= ROUTES =============

/// Enable ingress for a container
#[post("/ingress/<container_id>/enable", data = "<request>")]
pub async fn enable_ingress(
    container_id: String,
    request: Json<EnableIngressRequest>,
    user: AuthenticatedUser,
    _app_state: &State<AppState>,
    haproxy: &State<Arc<dyn HAProxyClientTrait>>,
    pfsense: &State<Arc<dyn PfSenseClientTrait>>,
    pool: &rocket::State<sqlx::PgPool>,
) -> Json<serde_json::Value> {
    let subdomain = request.subdomain.to_lowercase();
    let mode = request.mode.to_lowercase();

    // 1. Validate mode
    if mode != "http" && mode != "https" && mode != "tcp" {
        return Json(serde_json::json!({
            "error": "Invalid mode. Must be 'http', 'https', or 'tcp'",
            "modes": {
                "http": "Port 80 - Host header routing, no TLS",
                "https": "Port 443 - TLS termination with wildcard cert (recommended)",
                "tcp": "Dedicated port (10000-10999) - Raw TCP passthrough"
            }
        }));
    }

    // 2. Validate subdomain format
    if let Err(e) = validate_subdomain(&subdomain) {
        return Json(serde_json::json!({ "error": e }));
    }

    // 3. Check subdomain availability
    match storage::is_subdomain_available(pool.inner(), &subdomain).await {
        Ok(false) => {
            return Json(serde_json::json!({
                "error": format!("Subdomain '{}' is already taken", subdomain)
            }));
        }
        Err(e) => {
            return Json(serde_json::json!({
                "error": format!("Failed to check subdomain: {}", e)
            }));
        }
        Ok(true) => {}
    }

    // 4. Get container info with ownership check
    let container = match storage::get_container_info(pool.inner(), &container_id, &user.0.id).await
    {
        Ok(Some(c)) => c,
        Ok(None) => {
            return Json(serde_json::json!({
                "error": "Container not found or access denied"
            }));
        }
        Err(e) => {
            return Json(serde_json::json!({
                "error": format!("Database error: {}", e)
            }));
        }
    };

    debug!("Fetched info on container-id {}", container.container_id);

    // 5. Check container doesn't already have ingress
    if let Ok(Some(_)) = storage::get_ingress_by_container(pool.inner(), &container_id).await {
        return Json(serde_json::json!({
            "error": "Container already has ingress enabled. Disable it first."
        }));
    }

    // 6. Get container's IP address
    let target_ip: String =
        match storage::get_container_ipv4(pool.inner(), &container_id, &user.0.id).await {
            Ok(Some(ip)) => ip,
            Ok(None) => {
                return Json(serde_json::json!({
                    "error": "Container has no routable IP address"
                }));
            }
            Err(e) => {
                return Json(serde_json::json!({
                    "error": format!("Failed to get container IP: {}", e)
                }));
            }
        };

    let is_ipv4 = !target_ip.contains(':');
    let ip_version = if is_ipv4 { "ipv4" } else { "ipv6" };

    // 7. Get node info for static route (IPv4 only)
    let (_node_id, node_lan_ip): (Option<String>, Option<String>) = if is_ipv4 {
        match storage::get_container_node_info(pool.inner(), &container_id, &user.0.id).await {
            Ok(Some((nid, nip))) => (Some(nid), Some(nip)),
            Ok(None) => {
                return Json(serde_json::json!({
                    "error": "Could not determine container's host node"
                }));
            }
            Err(e) => {
                return Json(serde_json::json!({
                    "error": format!("Failed to get node info: {}", e)
                }));
            }
        }
    } else {
        (None, None)
    };

    // 8. Determine target port
    let target_port = request.target_port.unwrap_or(match mode.as_str() {
        "http" | "https" => 80, // Container usually listens on 80, HAProxy handles TLS
        _ => 0,
    });
    if target_port == 0 {
        return Json(serde_json::json!({
            "error": "target_port is required for TCP mode"
        }));
    }

    let base_domain = haproxy.get_base_domain();
    let public_ip = haproxy.get_public_ip();
    let full_domain = format!("{}.{}", subdomain, base_domain);

    // 9. Create static route for IPv4 containers
    let static_route_id = if is_ipv4 {
        if let Some(node_ip) = &node_lan_ip {
            match pfsense
                .add_static_route(
                    &format!("{}/32", target_ip),
                    node_ip,
                    &format!("{} ({})", container.container_name, subdomain),
                )
                .await
            {
                Ok(route) => Some(route.route_id),
                Err(e) => {
                    warn!("Failed to create static route: {}", e);
                    None
                }
            }
        } else {
            None
        }
    } else {
        None
    };

    // 10. Mode-specific setup
    match mode.as_str() {
        "http" => {
            match haproxy
                .create_http_ingress(&subdomain, &target_ip, target_port)
                .await
            {
                Ok(result) => {
                    if let Err(e) = storage::insert_ingress_route(
                        pool.inner(),
                        &user.0.id,
                        &container_id,
                        &subdomain,
                        &full_domain,
                        "http",
                        &target_ip,
                        target_port as i32,
                        None,
                        Some(&result.backend_name),
                        None,
                        Some(&result.acl_name),
                        Some(&result.server_name),
                        None,
                        true,
                        static_route_id.as_deref(),
                        ip_version,
                    )
                    .await
                    {
                        let _ = haproxy
                            .remove_http_ingress(&result.backend_name, &result.acl_name)
                            .await;
                        if let Some(route_id) = &static_route_id {
                            let _ = pfsense.remove_static_route(route_id).await;
                        }
                        return Json(serde_json::json!({
                            "error": format!("Failed to store ingress: {}", e)
                        }));
                    }

                    info!(
                        "✅ HTTP ingress enabled: {} → {}:{}",
                        full_domain, target_ip, target_port
                    );

                    Json(serde_json::json!({
                        "status": "enabled",
                        "mode": "http",
                        "subdomain": subdomain,
                        "url": format!("http://{}", full_domain),
                        "target": format!("{}:{}", target_ip, target_port),
                        "ip_version": ip_version,
                        "public_ip": public_ip,
                        "note": "No TLS - consider using 'https' mode for automatic TLS"
                    }))
                }
                Err(e) => {
                    if let Some(route_id) = &static_route_id {
                        let _ = pfsense.remove_static_route(route_id).await;
                    }
                    Json(serde_json::json!({
                        "error": format!("Failed to create ingress: {}", e)
                    }))
                }
            }
        }

        "https" => {
            match haproxy
                .create_https_ingress(&subdomain, &target_ip, target_port)
                .await
            {
                Ok(result) => {
                    if let Err(e) = storage::insert_ingress_route(
                        pool.inner(),
                        &user.0.id,
                        &container_id,
                        &subdomain,
                        &full_domain,
                        "https",
                        &target_ip,
                        target_port as i32,
                        None,
                        Some(&result.backend_name),
                        None,
                        Some(&result.acl_name),
                        Some(&result.server_name),
                        None,
                        true,
                        static_route_id.as_deref(),
                        ip_version,
                    )
                    .await
                    {
                        let _ = haproxy
                            .remove_https_ingress(&result.backend_name, &result.acl_name)
                            .await;
                        if let Some(route_id) = &static_route_id {
                            let _ = pfsense.remove_static_route(route_id).await;
                        }
                        return Json(serde_json::json!({
                            "error": format!("Failed to store ingress: {}", e)
                        }));
                    }

                    info!(
                        "✅ HTTPS ingress enabled: {} → {}:{}",
                        full_domain, target_ip, target_port
                    );

                    // Simple response - TLS works immediately via wildcard cert
                    Json(serde_json::json!({
                        "status": "enabled",
                        "mode": "https",
                        "subdomain": subdomain,
                        "url": format!("https://{}", full_domain),
                        "target": format!("{}:{}", target_ip, target_port),
                        "ip_version": ip_version,
                        "public_ip": public_ip,
                        "tls": {
                            "enabled": true,
                            "certificate": format!("*.{}", base_domain),
                            "termination": "haproxy",
                            "note": "TLS handled by wildcard certificate - works immediately"
                        }
                    }))
                }
                Err(e) => {
                    if let Some(route_id) = &static_route_id {
                        let _ = pfsense.remove_static_route(route_id).await;
                    }
                    Json(serde_json::json!({
                        "error": format!("Failed to create ingress: {}", e)
                    }))
                }
            }
        }

        "tcp" => {
            // Allocate TCP port
            let public_port = match storage::allocate_tcp_port(pool.inner()).await {
                Ok(port) => port,
                Err(e) => {
                    if let Some(route_id) = &static_route_id {
                        let _ = pfsense.remove_static_route(route_id).await;
                    }
                    return Json(serde_json::json!({
                        "error": format!("Failed to allocate TCP port: {}", e)
                    }));
                }
            };

            match haproxy
                .create_tcp_ingress(&subdomain, public_port as u16, &target_ip, target_port)
                .await
            {
                Ok(result) => {
                    // Create firewall rule for TCP port
                    let firewall_rule_id = match pfsense
                        .add_container_rule(
                            public_ip,
                            &[public_port],
                            &format!("ingress_tcp_{}", subdomain),
                            &user.0.id,
                        )
                        .await
                    {
                        Ok(rule) => Some(rule.rule_id),
                        Err(e) => {
                            warn!("Failed to create firewall rule: {}", e);
                            None
                        }
                    };

                    if let Err(e) = storage::insert_ingress_route(
                        pool.inner(),
                        &user.0.id,
                        &container_id,
                        &subdomain,
                        &full_domain,
                        "tcp",
                        &target_ip,
                        target_port as i32,
                        Some(public_port),
                        Some(&result.backend_name),
                        Some(&result.frontend_name),
                        None,
                        Some(&result.server_name),
                        firewall_rule_id.as_deref(),
                        firewall_rule_id.is_some(),
                        static_route_id.as_deref(),
                        ip_version,
                    )
                    .await
                    {
                        let _ = haproxy
                            .remove_tcp_ingress(&result.frontend_name, &result.backend_name)
                            .await;
                        let _ = storage::release_tcp_port(pool.inner(), public_port).await;
                        if let Some(rule_id) = &firewall_rule_id {
                            let _ = pfsense.remove_rule(rule_id).await;
                        }
                        if let Some(route_id) = &static_route_id {
                            let _ = pfsense.remove_static_route(route_id).await;
                        }
                        return Json(serde_json::json!({
                            "error": format!("Failed to store ingress: {}", e)
                        }));
                    }

                    info!(
                        "✅ TCP ingress enabled: {}:{} → {}:{}",
                        full_domain, public_port, target_ip, target_port
                    );

                    Json(serde_json::json!({
                        "status": "enabled",
                        "mode": "tcp",
                        "subdomain": subdomain,
                        "host": full_domain,
                        "port": public_port,
                        "url": format!("{}:{}", full_domain, public_port),
                        "target": format!("{}:{}", target_ip, target_port),
                        "ip_version": ip_version,
                        "public_ip": public_ip,
                        "note": "Raw TCP - container handles TLS if needed"
                    }))
                }
                Err(e) => {
                    let _ = storage::release_tcp_port(pool.inner(), public_port).await;
                    if let Some(route_id) = &static_route_id {
                        let _ = pfsense.remove_static_route(route_id).await;
                    }
                    Json(serde_json::json!({
                        "error": format!("Failed to create ingress: {}", e)
                    }))
                }
            }
        }
        _ => Json(serde_json::json!({ "error": "Invalid mode" })),
    }
}

/// Disable ingress for a container
#[delete("/ingress/<container_id>/disable")]
pub async fn disable_ingress(
    container_id: String,
    user: AuthenticatedUser,
    haproxy: &State<Arc<dyn HAProxyClientTrait>>,
    pfsense: &State<Arc<dyn PfSenseClientTrait>>,
    pool: &rocket::State<sqlx::PgPool>,
) -> Json<serde_json::Value> {
    // Get existing ingress with ownership check
    let ingress =
        match storage::get_ingress_by_container_with_owner(pool.inner(), &container_id, &user.0.id)
            .await
        {
            Ok(Some(r)) => r,
            Ok(None) => {
                return Json(serde_json::json!({
                    "error": "No ingress found for this container or access denied"
                }));
            }
            Err(e) => {
                return Json(serde_json::json!({
                    "error": format!("Database error: {}", e)
                }));
            }
        };

    let mut cleanup_errors = Vec::new();

    // Mode-specific cleanup
    match ingress.mode.as_str() {
        "http" => {
            if let (Some(backend), Some(acl)) =
                (&ingress.haproxy_backend_name, &ingress.haproxy_acl_name)
            {
                if let Err(e) = haproxy.remove_http_ingress(backend, acl).await {
                    cleanup_errors.push(format!("HAProxy cleanup: {}", e));
                }
            }
        }
        "https" => {
            if let (Some(backend), Some(acl)) =
                (&ingress.haproxy_backend_name, &ingress.haproxy_acl_name)
            {
                if let Err(e) = haproxy.remove_https_ingress(backend, acl).await {
                    cleanup_errors.push(format!("HAProxy cleanup: {}", e));
                }
            }
        }
        "tcp" => {
            if let (Some(frontend), Some(backend)) = (
                &ingress.haproxy_frontend_name,
                &ingress.haproxy_backend_name,
            ) {
                if let Err(e) = haproxy.remove_tcp_ingress(frontend, backend).await {
                    cleanup_errors.push(format!("HAProxy cleanup: {}", e));
                }
            }
            // Release TCP port
            if let Some(port) = ingress.public_port {
                let _ = storage::release_tcp_port(pool.inner(), port).await;
            }
            // Remove firewall rule
            if let Some(rule_id) = &ingress.pfsense_rule_id {
                if let Err(e) = pfsense.remove_rule(rule_id).await {
                    cleanup_errors.push(format!("Firewall rule cleanup: {}", e));
                }
            }
        }
        _ => {}
    }

    // Remove static route by destination IP (NOT by stored ID — pfSense IDs shift on reboot/changes)
    if ingress.pfsense_static_route_id.is_some() {
        let destination = format!("{}/32", ingress.target_ip);
        if let Err(e) = pfsense
            .remove_static_route_by_destination(&destination)
            .await
        {
            cleanup_errors.push(format!("Static route cleanup: {}", e));
        }
    }

    // Delete from database
    if let Err(e) = storage::delete_ingress_route(pool.inner(), ingress.id).await {
        return Json(serde_json::json!({
            "error": format!("Failed to delete ingress record: {}", e),
            "cleanup_errors": cleanup_errors
        }));
    }

    info!("✅ Ingress disabled for container: {}", container_id);

    if cleanup_errors.is_empty() {
        Json(serde_json::json!({
            "status": "disabled",
            "subdomain": ingress.subdomain,
            "mode": ingress.mode
        }))
    } else {
        Json(serde_json::json!({
            "status": "disabled",
            "subdomain": ingress.subdomain,
            "mode": ingress.mode,
            "warnings": cleanup_errors
        }))
    }
}

/// Get ingress status for a container
#[get("/ingress/<container_id>/status")]
pub async fn get_ingress_status(
    container_id: String,
    user: AuthenticatedUser,
    haproxy: &State<Arc<dyn HAProxyClientTrait>>,
    pool: &rocket::State<sqlx::PgPool>,
) -> Json<serde_json::Value> {
    let ingress =
        match storage::get_ingress_by_container_with_owner(pool.inner(), &container_id, &user.0.id)
            .await
        {
            Ok(Some(r)) => r,
            Ok(None) => {
                return Json(serde_json::json!({
                    "enabled": false,
                    "message": "No ingress configured for this container"
                }));
            }
            Err(e) => {
                return Json(serde_json::json!({
                    "error": format!("Database error: {}", e)
                }));
            }
        };

    let base_domain = haproxy.get_base_domain();
    let url = match ingress.mode.as_str() {
        "http" => format!("http://{}.{}", ingress.subdomain, base_domain),
        "https" => format!("https://{}.{}", ingress.subdomain, base_domain),
        "tcp" => format!(
            "{}.{}:{}",
            ingress.subdomain,
            base_domain,
            ingress.public_port.unwrap_or(0)
        ),
        _ => format!("{}.{}", ingress.subdomain, base_domain),
    };

    Json(serde_json::json!({
        "enabled": ingress.is_active,
        "subdomain": ingress.subdomain,
        "mode": ingress.mode,
        "url": url,
        "target_ip": ingress.target_ip,
        "target_port": ingress.target_port,
        "public_port": ingress.public_port,
        "ip_version": ingress.ip_version,
        "firewall_open": ingress.firewall_open,
        "created_at": ingress.created_at
    }))
}

/// List all ingress routes for the current user
#[get("/ingress/list")]
pub async fn list_ingress(
    user: AuthenticatedUser,
    haproxy: &State<Arc<dyn HAProxyClientTrait>>,
    pool: &rocket::State<sqlx::PgPool>,
) -> Json<serde_json::Value> {
    let routes = match storage::get_user_ingress_routes(pool.inner(), &user.0.id).await {
        Ok(r) => r,
        Err(e) => {
            return Json(serde_json::json!({
                "error": format!("Failed to list ingress routes: {}", e)
            }));
        }
    };

    let base_domain = haproxy.get_base_domain();

    let ingress_list: Vec<IngressInfo> = routes
        .into_iter()
        .map(|r| {
            let url = match r.mode.as_str() {
                "http" => format!("http://{}.{}", r.subdomain, base_domain),
                "https" => format!("https://{}.{}", r.subdomain, base_domain),
                "tcp" => format!(
                    "{}.{}:{}",
                    r.subdomain,
                    base_domain,
                    r.public_port.unwrap_or(0)
                ),
                _ => format!("{}.{}", r.subdomain, base_domain),
            };

            IngressInfo {
                container_id: r.container_id,
                subdomain: r.subdomain,
                mode: r.mode,
                url,
                target_ip: r.target_ip,
                target_port: r.target_port,
                public_port: r.public_port,
                firewall_open: r.firewall_open,
                is_active: r.is_active,
                created_at: r.created_at,
            }
        })
        .collect();

    Json(serde_json::json!({
        "routes": ingress_list,
        "count": ingress_list.len()
    }))
}
