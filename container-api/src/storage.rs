// storage.rs
// Storage layer to persist any necessary data for container-api to function.

// TODO:  I need a abstraction layer here to make sure upper levels does not need to
// moving to another storage layer here.
// for now i will throw any functions data related to postgres here.

use crate::services::route_manager::StaticRouteManager;
use sqlx::{PgPool, Row};

/// Build IPv6 URL for a port
pub fn build_ipv6_url(ipv6_address: &str, port: u16) -> String {
    match port {
        80 => format!("http://[{}]/", ipv6_address),
        443 => format!("https://[{}]/", ipv6_address),
        _ => format!("[{}]:{}", ipv6_address, port),
    }
}

// ============= INGRESS DB CALLS =============

#[derive(Debug)]
pub struct ContainerDb {
    pub(crate) container_id: String,
    pub(crate) container_name: String,
    #[allow(dead_code)]
    node_id: String,
}

#[derive(Debug)]
pub struct IngressRouteDb {
    pub(crate) id: i32,
    pub(crate) container_id: String,
    pub(crate) subdomain: String,
    pub(crate) mode: String,
    pub(crate) target_ip: String,
    pub(crate) target_port: i32,
    pub(crate) public_port: Option<i32>,
    pub(crate) haproxy_backend_name: Option<String>,
    pub(crate) haproxy_frontend_name: Option<String>,
    pub(crate) haproxy_acl_name: Option<String>,
    #[allow(dead_code)]
    haproxy_server_name: Option<String>,
    pub(crate) pfsense_rule_id: Option<String>,
    pub(crate) pfsense_static_route_id: Option<String>,
    pub(crate) ip_version: String,
    pub(crate) firewall_open: bool,
    pub(crate) is_active: bool,
    pub(crate) created_at: String,
}

pub async fn is_subdomain_available(
    pool: &PgPool,
    subdomain: &str,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ingress_routes WHERE subdomain = $1 AND is_active = true",
    )
    .bind(subdomain)
    .fetch_one(pool)
    .await?;

    Ok(count == 0)
}

pub async fn get_container_info(
    pool: &PgPool,
    container_id: &str,
    user_id: &str,
) -> Result<Option<ContainerDb>, Box<dyn std::error::Error + Send + Sync>> {
    let row = sqlx::query(
        "SELECT container_id, container_name, node_id FROM containers 
         WHERE container_id = $1 AND user_id = $2 AND status != 'deleted'",
    )
    .bind(container_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| ContainerDb {
        container_id: r.get("container_id"),
        container_name: r.get("container_name"),
        node_id: r.get("node_id"),
    }))
}

pub async fn get_container_ipv4(
    pool: &PgPool,
    container_id: &str,
    user_id: &str,
) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
    let ip: Option<String> = sqlx::query_scalar(
        "SELECT internal_ip FROM containers WHERE container_id = $1 AND user_id = $2",
    )
    .bind(container_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?
    .flatten();

    Ok(ip)
}

pub async fn get_container_node_info(
    pool: &PgPool,
    container_id: &str,
    user_id: &str,
) -> Result<Option<(String, String)>, Box<dyn std::error::Error + Send + Sync>> {
    let row = sqlx::query(
        r#"
        SELECT c.node_id, n.lan_ip
        FROM containers c
        JOIN nodes n ON c.node_id = n.node_id
        WHERE c.container_id = $1 AND c.user_id = $2
        "#,
    )
    .bind(container_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| (r.get("node_id"), r.get("lan_ip"))))
}

pub async fn get_ingress_by_container(
    pool: &PgPool,
    container_id: &str,
) -> Result<Option<IngressRouteDb>, Box<dyn std::error::Error + Send + Sync>> {
    let row = sqlx::query(
        r#"
        SELECT id, container_id, subdomain, mode, target_ip, target_port,
               public_port, haproxy_backend_name, haproxy_frontend_name,
               haproxy_acl_name, haproxy_server_name, pfsense_rule_id, 
               pfsense_static_route_id, ip_version, firewall_open, is_active,
               created_at::TEXT
        FROM ingress_routes
        WHERE container_id = $1
        "#,
    )
    .bind(container_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| IngressRouteDb {
        id: r.get("id"),
        container_id: r.get("container_id"),
        subdomain: r.get("subdomain"),
        mode: r.get("mode"),
        target_ip: r.get("target_ip"),
        target_port: r.get("target_port"),
        public_port: r.get("public_port"),
        haproxy_backend_name: r.get("haproxy_backend_name"),
        haproxy_frontend_name: r.get("haproxy_frontend_name"),
        haproxy_acl_name: r.get("haproxy_acl_name"),
        haproxy_server_name: r.get("haproxy_server_name"),
        pfsense_rule_id: r.get("pfsense_rule_id"),
        pfsense_static_route_id: r.get("pfsense_static_route_id"),
        ip_version: r.get("ip_version"),
        firewall_open: r.get("firewall_open"),
        is_active: r.get("is_active"),
        created_at: r.get("created_at"),
    }))
}

pub async fn get_ingress_by_container_with_owner(
    pool: &PgPool,
    container_id: &str,
    user_id: &str,
) -> Result<Option<IngressRouteDb>, Box<dyn std::error::Error + Send + Sync>> {
    let row = sqlx::query(
        r#"
        SELECT id, container_id, subdomain, mode, target_ip, target_port,
               public_port, haproxy_backend_name, haproxy_frontend_name,
               haproxy_acl_name, haproxy_server_name, pfsense_rule_id, 
               pfsense_static_route_id, ip_version, firewall_open, is_active,
               created_at::TEXT
        FROM ingress_routes
        WHERE container_id = $1 AND user_id = $2
        "#,
    )
    .bind(container_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| IngressRouteDb {
        id: r.get("id"),
        container_id: r.get("container_id"),
        subdomain: r.get("subdomain"),
        mode: r.get("mode"),
        target_ip: r.get("target_ip"),
        target_port: r.get("target_port"),
        public_port: r.get("public_port"),
        haproxy_backend_name: r.get("haproxy_backend_name"),
        haproxy_frontend_name: r.get("haproxy_frontend_name"),
        haproxy_acl_name: r.get("haproxy_acl_name"),
        haproxy_server_name: r.get("haproxy_server_name"),
        pfsense_rule_id: r.get("pfsense_rule_id"),
        pfsense_static_route_id: r.get("pfsense_static_route_id"),
        ip_version: r.get("ip_version"),
        firewall_open: r.get("firewall_open"),
        is_active: r.get("is_active"),
        created_at: r.get("created_at"),
    }))
}

pub async fn get_user_ingress_routes(
    pool: &PgPool,
    user_id: &str,
) -> Result<Vec<IngressRouteDb>, Box<dyn std::error::Error + Send + Sync>> {
    let rows = sqlx::query(
        r#"
        SELECT id, container_id, subdomain, mode, target_ip, target_port,
               public_port, haproxy_backend_name, haproxy_frontend_name,
               haproxy_acl_name, haproxy_server_name, pfsense_rule_id,
               pfsense_static_route_id, ip_version, firewall_open, is_active,
               created_at::TEXT
        FROM ingress_routes
        WHERE user_id = $1 AND is_active = true
        ORDER BY created_at DESC
        "#,
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| IngressRouteDb {
            id: r.get("id"),
            container_id: r.get("container_id"),
            subdomain: r.get("subdomain"),
            mode: r.get("mode"),
            target_ip: r.get("target_ip"),
            target_port: r.get("target_port"),
            public_port: r.get("public_port"),
            haproxy_backend_name: r.get("haproxy_backend_name"),
            haproxy_frontend_name: r.get("haproxy_frontend_name"),
            haproxy_acl_name: r.get("haproxy_acl_name"),
            haproxy_server_name: r.get("haproxy_server_name"),
            pfsense_rule_id: r.get("pfsense_rule_id"),
            pfsense_static_route_id: r.get("pfsense_static_route_id"),
            ip_version: r.get("ip_version"),
            firewall_open: r.get("firewall_open"),
            is_active: r.get("is_active"),
            created_at: r.get("created_at"),
        })
        .collect())
}

#[allow(clippy::too_many_arguments)]
pub async fn insert_ingress_route(
    pool: &PgPool,
    user_id: &str,
    container_id: &str,
    subdomain: &str,
    full_domain: &str,
    mode: &str,
    target_ip: &str,
    target_port: i32,
    public_port: Option<i32>,
    haproxy_backend_name: Option<&str>,
    haproxy_frontend_name: Option<&str>,
    haproxy_acl_name: Option<&str>,
    haproxy_server_name: Option<&str>,
    pfsense_rule_id: Option<&str>,
    firewall_open: bool,
    pfsense_static_route_id: Option<&str>,
    ip_version: &str,
) -> Result<i32, Box<dyn std::error::Error + Send + Sync>> {
    let id: i32 = sqlx::query_scalar(
        r#"
        INSERT INTO ingress_routes (
            user_id, container_id, subdomain, full_domain, mode,
            target_ip, target_port, public_port,
            haproxy_backend_name, haproxy_frontend_name, haproxy_acl_name, haproxy_server_name,
            pfsense_rule_id, firewall_open, pfsense_static_route_id, ip_version, is_active
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, true)
        RETURNING id
        "#,
    )
    .bind(user_id)
    .bind(container_id)
    .bind(subdomain)
    .bind(full_domain)
    .bind(mode)
    .bind(target_ip)
    .bind(target_port)
    .bind(public_port)
    .bind(haproxy_backend_name)
    .bind(haproxy_frontend_name)
    .bind(haproxy_acl_name)
    .bind(haproxy_server_name)
    .bind(pfsense_rule_id)
    .bind(firewall_open)
    .bind(pfsense_static_route_id)
    .bind(ip_version)
    .fetch_one(pool)
    .await?;

    Ok(id)
}

pub async fn delete_ingress_route(
    pool: &PgPool,
    id: i32,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    sqlx::query("DELETE FROM ingress_routes WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn allocate_tcp_port(
    pool: &PgPool,
) -> Result<i32, Box<dyn std::error::Error + Send + Sync>> {
    let port: i32 = sqlx::query_scalar("SELECT allocate_ingress_port()")
        .fetch_one(pool)
        .await?;
    Ok(port)
}

pub async fn release_tcp_port(
    pool: &PgPool,
    port: i32,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    sqlx::query("SELECT release_ingress_port($1)")
        .bind(port)
        .execute(pool)
        .await?;
    Ok(())
}

/// Allocate next available container IP from user's subnet
pub async fn allocate_container_ip(
    pool: &PgPool,
    user_id: &str,
    subnet: &str, // e.g. "172.21.1.0/24"
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let base = subnet.split('/').next().ok_or("Invalid subnet")?;
    let parts: Vec<&str> = base.split('.').collect();
    if parts.len() != 4 {
        return Err("Invalid subnet format".into());
    }
    let prefix = format!("{}.{}.{}.", parts[0], parts[1], parts[2]);

    // Get IPs in use by this user's non-deleted containers
    let used_ips: Vec<String> = sqlx::query_scalar(
        "SELECT internal_ip FROM containers 
         WHERE user_id = $1 AND status != 'deleted' AND internal_ip IS NOT NULL",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    let used: std::collections::HashSet<u8> = used_ips
        .iter()
        .filter_map(|ip| ip.split('.').next_back()?.parse().ok())
        .collect();

    // .0 = network, .1 = gateway, .255 = broadcast
    for i in 2..255u8 {
        if !used.contains(&i) {
            return Ok(format!("{}{}", prefix, i));
        }
    }

    Err("No available IPs in subnet".into())
}

/// Get container IP by name (regardless of status)
pub async fn get_container_ip(
    pool: &sqlx::PgPool,
    container_name: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT internal_ip FROM containers WHERE container_name = $1")
        .bind(container_name)
        .fetch_optional(pool)
        .await
}

/// Mark container as deleted
pub async fn mark_container_deleted(
    pool: &sqlx::PgPool,
    container_name: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE containers SET status = 'deleted', updated_at = NOW() WHERE container_name = $1",
    )
    .bind(container_name)
    .execute(pool)
    .await?;
    Ok(())
}

/// Get user's container subnet for specific garage
pub async fn get_user_garage_subnet(
    pool: &PgPool,
    user_id: &str,
    garage_id: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let subnet = sqlx::query_scalar::<_, String>(
        "SELECT container_subnet FROM garage_container_allocations 
         WHERE user_id = $1 AND garage_id = $2",
    )
    .bind(user_id)
    .bind(garage_id)
    .fetch_optional(pool)
    .await?;

    subnet.ok_or_else(|| "No container subnet allocated for user in garage".into())
}

/// Select best hardware node in garage
pub async fn select_garage_hardware_node(
    pool: &PgPool,
    garage_id: &str,
    hardware_preference: &Option<String>,
    architecture: &Option<String>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let node_id = match (hardware_preference, architecture) {
        (Some(hw), Some(arch)) => {
            sqlx::query_scalar::<_, String>(
                "SELECT node_id FROM nodes 
                 WHERE garage_id = $1 AND status = 'active' 
                 AND hardware_type = $2 AND architecture = $3
                 ORDER BY current_users ASC LIMIT 1",
            )
            .bind(garage_id)
            .bind(hw)
            .bind(arch)
            .fetch_optional(pool)
            .await?
        }
        (Some(hw), None) => {
            sqlx::query_scalar::<_, String>(
                "SELECT node_id FROM nodes 
                 WHERE garage_id = $1 AND status = 'active' 
                 AND hardware_type = $2
                 ORDER BY current_users ASC LIMIT 1",
            )
            .bind(garage_id)
            .bind(hw)
            .fetch_optional(pool)
            .await?
        }
        (None, Some(arch)) => {
            sqlx::query_scalar::<_, String>(
                "SELECT node_id FROM nodes 
                 WHERE garage_id = $1 AND status = 'active' 
                 AND architecture = $2
                 ORDER BY current_users ASC LIMIT 1",
            )
            .bind(garage_id)
            .bind(arch)
            .fetch_optional(pool)
            .await?
        }
        (None, None) => {
            sqlx::query_scalar::<_, String>(
                "SELECT node_id FROM nodes 
                 WHERE garage_id = $1 AND status = 'active'
                 ORDER BY current_users ASC LIMIT 1",
            )
            .bind(garage_id)
            .fetch_optional(pool)
            .await?
        }
    };

    node_id.ok_or_else(|| "No suitable node found in garage".into())
}

/// Track container deployment in database
pub async fn track_container_deployment(
    pool: &PgPool,
    container_id: &str,
    container_name: &str,
    user_id: &str,
    node_id: &str,
    image: &str,
    internal_ip: Option<&str>,
    cpu_limit: Option<f32>,
    memory_limit: Option<&str>,
    volume_size: Option<&str>,
    enable_persistence: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cpu = cpu_limit.unwrap_or(0.5) as f64;
    let mem_mb = parse_memory_spec_mb(memory_limit.unwrap_or("512m"));
    let vol_mb = if enable_persistence {
        parse_memory_spec_mb(volume_size.unwrap_or("1g"))
    } else {
        0
    };

    sqlx::query(
        "INSERT INTO containers 
         (container_id, container_name, user_id, node_id, image, internal_ip, status, cpu_limit, memory_limit_mb, volume_size_mb, created_at) 
         VALUES ($1, $2, $3, $4, $5, $6, 'deploying', $7, $8, $9, NOW())",
    )
    .bind(container_id)
    .bind(container_name)
    .bind(user_id)
    .bind(node_id)
    .bind(image)
    .bind(internal_ip)
    .bind(cpu)
    .bind(mem_mb)
    .bind(vol_mb)
    .execute(pool)
    .await?;
    Ok(())
}

/// Update container status (called when agent reports deployment result)
pub async fn update_container_status(
    pool: &PgPool,
    container_name: &str,
    status: &str,
    status_message: Option<&str>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    sqlx::query(
        "UPDATE containers SET status = $1, status_message = $2, updated_at = NOW() 
         WHERE container_name = $3 AND status != 'deleted'",
    )
    .bind(status)
    .bind(status_message)
    .bind(container_name)
    .execute(pool)
    .await?;
    info!("📋 Container {} status → {}", container_name, status);
    Ok(())
}

/// Get containers in deploying/failed state from DB.
/// These don't exist in nerdctl runtime — only in our database.
/// Used by list endpoint to show the full picture.
pub async fn get_non_running_containers(
    pool: &PgPool,
    user_id: &str,
) -> Result<Vec<crate::models::ContainerInfo>, Box<dyn std::error::Error + Send + Sync>> {
    let rows = sqlx::query(
        "SELECT container_name, image, status, status_message, internal_ip, created_at::TEXT
         FROM containers
         WHERE user_id = $1 AND status IN ('deploying', 'failed')
         ORDER BY created_at DESC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    let containers = rows
        .iter()
        .map(|row| {
            let status: String = row.get("status");
            let status_message: Option<String> = row.get("status_message");

            // Build a display status string that includes the reason for failed
            let display_status = if status == "failed" {
                if let Some(msg) = &status_message {
                    // Take first line, truncate
                    let first_line = msg.lines().next().unwrap_or(msg);
                    let short: String = first_line.chars().take(80).collect();
                    format!("failed: {}", short)
                } else {
                    "failed".to_string()
                }
            } else {
                status
            };

            crate::models::ContainerInfo {
                container_id: row.get::<String, _>("container_name").clone(),
                name: row.get("container_name"),
                image: row.get("image"),
                status: display_status,
                pod_id: None,
                created_at: row.get::<Option<String>, _>("created_at").unwrap_or_default(),
                ports: vec![],
                container_ip: row.get("internal_ip"),
                ipv6_address: None,
                ipv6_enabled: false,
            }
        })
        .collect();

    Ok(containers)
}

/// Find which node has a container
pub async fn find_container_node(
    pool: &PgPool,
    container_id_or_name: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let node_id = sqlx::query_scalar::<_, String>(
        "SELECT node_id FROM containers 
         WHERE (container_id = $1 OR container_name = $1) 
           AND status != 'deleted'
         ORDER BY created_at DESC
         LIMIT 1",
    )
    .bind(container_id_or_name)
    .fetch_optional(pool)
    .await?;

    node_id.ok_or_else(|| "Container not found in database".into())
}

/// Get node's network configuration
pub async fn get_node_network_info(
    pool: &PgPool,
    node_id: &str,
) -> Result<(String, String), Box<dyn std::error::Error + Send + Sync>> {
    let row = sqlx::query(
        "SELECT internal_ip, COALESCE(network_interface, 'eth0') as interface
         FROM nodes 
         WHERE node_id = $1",
    )
    .bind(node_id)
    .fetch_one(pool)
    .await?;

    Ok((
        row.get::<String, _>("internal_ip"),
        row.get::<String, _>("interface"),
    ))
}

// route managemenet

// ============= DATABASE INTEGRATION =============

/// Startup: Sync routes from database (crash recovery)
pub async fn sync_routes_on_startup(
    route_manager: &StaticRouteManager,
    db: &PgPool,
) -> Result<SyncReport, Box<dyn std::error::Error + Send + Sync>> {
    info!("🔄 Syncing routes from database...");

    // Get all running containers with their node routing info
    let db_routes = sqlx::query(
        "SELECT c.internal_ip, c.container_name, n.internal_ip AS node_ip, n.network_interface
         FROM containers c
         JOIN nodes n ON c.node_id = n.node_id
         WHERE c.status = 'running' AND c.internal_ip IS NOT NULL",
    )
    .fetch_all(db)
    .await?;

    // Collect expected IPs so we can clean stale routes after
    let mut expected_ips: std::collections::HashSet<String> = std::collections::HashSet::new();

    let mut success = 0;
    let mut skipped = 0;
    let mut failed = 0;

    for row in &db_routes {
        let container_ip: String = row.get("internal_ip");
        let container_name: String = row.get("container_name");
        let node_ip: String = row.get("node_ip");
        let interface: String = row.get("network_interface");

        expected_ips.insert(container_ip.clone());

        match route_manager.route_exists(&container_ip).await {
            Ok(true) => {
                debug!(
                    "Route already exists: {} ({})",
                    container_name, container_ip
                );
                skipped += 1;
            }
            Ok(false) => {
                match route_manager
                    .add_container_route(&container_ip, &node_ip, &interface)
                    .await
                {
                    Ok(_) => {
                        info!("✅ Synced route: {} -> {}", container_name, container_ip);
                        success += 1;
                    }
                    Err(e) => {
                        error!(
                            "❌ Failed to sync route {} ({}): {}",
                            container_name, container_ip, e
                        );
                        failed += 1;
                    }
                }
            }
            Err(e) => {
                warn!(
                    "❌ Failed to check route {} ({}): {}",
                    container_name, container_ip, e
                );
                failed += 1;
            }
        }
    }

    // Clean stale routes: any 172.21.x.x route not in our expected set
    let stale = cleanup_stale_routes(route_manager, &expected_ips).await;
    info!(
        "🔄 Route sync complete: {} added, {} skipped, {} failed, {} stale removed",
        success, skipped, failed, stale
    );

    Ok(SyncReport {
        added: success,
        skipped,
        failed,
        stale_removed: stale,
    })
}

#[derive(Debug, Clone)]
pub struct SyncReport {
    pub added: u32,
    pub skipped: u32,
    pub failed: u32,
    pub stale_removed: u32,
}

/// Remove OS routes that don't match any running container
async fn cleanup_stale_routes(
    route_manager: &StaticRouteManager,
    expected_ips: &std::collections::HashSet<String>,
) -> u32 {
    // Get current container routes from OS (172.21.x.x)
    let os_routes = match list_container_routes().await {
        Ok(routes) => routes,
        Err(e) => {
            warn!("Could not list OS routes for cleanup: {}", e);
            return 0;
        }
    };

    let mut removed = 0;
    for route_ip in os_routes {
        if !expected_ips.contains(&route_ip) {
            match route_manager.remove_container_route(&route_ip).await {
                Ok(_) => {
                    info!("🧹 Removed stale route: {}", route_ip);
                    removed += 1;
                }
                Err(e) => {
                    warn!("Failed to remove stale route {}: {}", route_ip, e);
                }
            }
        }
    }

    removed
}

/// List all container routes currently in the OS routing table
pub async fn list_container_routes() -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>>
{
    let output = if cfg!(target_os = "macos") {
        tokio::process::Command::new("netstat")
            .args(["-rn", "-f", "inet"])
            .output()
            .await?
    } else {
        tokio::process::Command::new("ip")
            .args(["route", "show"])
            .output()
            .await?
    };

    if !output.status.success() {
        return Err(format!(
            "Failed to list routes: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut ips = Vec::new();

    for line in stdout.lines() {
        let first = line.split_whitespace().next().unwrap_or("");
        if first.starts_with("172.21.") {
            let ip = first.split('/').next().unwrap_or(first);
            ips.push(ip.to_string());
        }
    }

    Ok(ips)
}

// ============= PLAN QUOTA ENFORCEMENT =============

/// Parsed plan resource limits
#[derive(Debug, Clone)]
pub struct PlanLimits {
    pub plan_id: String,
    pub display_name: String,
    pub max_vcpus: f32,      // e.g. 2.0 from "2 vCPU"
    pub max_memory_mb: i64,  // e.g. 4096 from "4GB RAM"
    pub max_storage_mb: i64, // e.g. 102400 from "100GB SSD"
}

/// Current resource usage for a user across all active containers
#[derive(Debug, Clone)]
pub struct ResourceUsage {
    pub total_cpu: f32,
    pub total_memory_mb: i64,
    pub total_disk_mb: i64,
    pub container_count: i64,
}

/// Result of a quota check
#[derive(Debug)]
pub struct QuotaCheckResult {
    pub allowed: bool,
    pub reason: Option<String>,
    pub plan: PlanLimits,
    pub current_usage: ResourceUsage,
}

/// Parse plan column "2 vCPU" → 2.0
fn parse_cpu_limit(raw: &str) -> f32 {
    raw.split_whitespace()
        .next()
        .and_then(|s| s.parse::<f32>().ok())
        .unwrap_or(1.0)
}

/// Parse plan column "4GB RAM" → 4096 (MB)
fn parse_memory_limit_mb(raw: &str) -> i64 {
    let token = raw.split_whitespace().next().unwrap_or("512MB");
    if let Some(gb) = token.strip_suffix("GB") {
        gb.parse::<i64>().unwrap_or(1) * 1024
    } else if let Some(mb) = token.strip_suffix("MB") {
        mb.parse::<i64>().unwrap_or(512)
    } else {
        token.parse::<i64>().unwrap_or(512)
    }
}

/// Parse container memory spec "512m" / "1g" → MB
fn parse_memory_spec_mb(spec: &str) -> i64 {
    let spec = spec.trim().to_lowercase();
    if let Some(g) = spec.strip_suffix('g') {
        g.parse::<i64>().unwrap_or(1) * 1024
    } else if let Some(m) = spec.strip_suffix('m') {
        m.parse::<i64>().unwrap_or(512)
    } else {
        spec.parse::<i64>().unwrap_or(512)
    }
}

/// Parse plan storage column "100GB SSD" → 102400 (MB)
fn parse_storage_limit_mb(raw: &str) -> i64 {
    let token = raw.split_whitespace().next().unwrap_or("100GB");
    if let Some(tb) = token.strip_suffix("TB") {
        tb.parse::<i64>().unwrap_or(1) * 1024 * 1024
    } else if let Some(gb) = token.strip_suffix("GB") {
        gb.parse::<i64>().unwrap_or(100) * 1024
    } else if let Some(mb) = token.strip_suffix("MB") {
        mb.parse::<i64>().unwrap_or(1024)
    } else {
        token.parse::<i64>().unwrap_or(102400)
    }
}

/// Get the plan limits for a user
pub async fn get_user_plan_limits(
    pool: &PgPool,
    user_id: &str,
) -> Result<PlanLimits, Box<dyn std::error::Error + Send + Sync>> {
    let row = sqlx::query(
        r#"
        SELECT p.id, p.display_name, p.cpu, p.memory, p.storage
        FROM users u
        JOIN plans p ON u.plan_id = p.id
        WHERE u.id = $1 AND p.is_active = true
        "#,
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .map_err(|e| format!("Failed to fetch plan for user {}: {}", user_id, e))?;

    let cpu_raw: String = row.get("cpu");
    let memory_raw: String = row.get("memory");
    let storage_raw: String = row.get("storage");

    Ok(PlanLimits {
        plan_id: row.get("id"),
        display_name: row.get("display_name"),
        max_vcpus: parse_cpu_limit(&cpu_raw),
        max_memory_mb: parse_memory_limit_mb(&memory_raw),
        max_storage_mb: parse_storage_limit_mb(&storage_raw),
    })
}

/// Get current total resource usage for a user (active containers only)
pub async fn get_user_resource_usage(
    pool: &PgPool,
    user_id: &str,
) -> Result<ResourceUsage, Box<dyn std::error::Error + Send + Sync>> {
    let row = sqlx::query(
        r#"
        SELECT
            COALESCE(SUM(cpu_limit), 0)::FLOAT8 as total_cpu,
            COALESCE(SUM(memory_limit_mb), 0)::BIGINT as total_memory_mb,
            COALESCE(SUM(volume_size_mb), 0)::BIGINT as total_disk_mb,
            COUNT(*)::BIGINT as container_count
        FROM containers
        WHERE user_id = $1 AND status != 'deleted'
        "#,
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;

    Ok(ResourceUsage {
        total_cpu: row.get::<f64, _>("total_cpu") as f32,
        total_memory_mb: row.get::<i64, _>("total_memory_mb"),
        total_disk_mb: row.get::<i64, _>("total_disk_mb"),
        container_count: row.get::<i64, _>("container_count"),
    })
}

/// Check if deploying a new container would exceed the user's plan limits.
pub async fn check_plan_quota(
    pool: &PgPool,
    user_id: &str,
    requested_cpu: f32,
    requested_memory: &str,
    requested_disk: &str,
    enable_persistence: bool,
) -> Result<QuotaCheckResult, Box<dyn std::error::Error + Send + Sync>> {
    let plan = get_user_plan_limits(pool, user_id).await?;
    let usage = get_user_resource_usage(pool, user_id).await?;

    let req_memory_mb = parse_memory_spec_mb(requested_memory);
    let req_disk_mb = if enable_persistence {
        parse_memory_spec_mb(requested_disk)
    } else {
        0
    };
    let new_total_cpu = usage.total_cpu + requested_cpu;
    let new_total_memory = usage.total_memory_mb + req_memory_mb;
    let new_total_disk = usage.total_disk_mb + req_disk_mb;

    if new_total_cpu > plan.max_vcpus {
        return Ok(QuotaCheckResult {
            allowed: false,
            reason: Some(format!(
                "CPU limit exceeded: requesting {:.1} vCPU, current usage {:.1}/{:.1} vCPU (plan: {})",
                requested_cpu, usage.total_cpu, plan.max_vcpus, plan.display_name
            )),
            plan,
            current_usage: usage,
        });
    }

    if new_total_memory > plan.max_memory_mb {
        return Ok(QuotaCheckResult {
            allowed: false,
            reason: Some(format!(
                "Memory limit exceeded: requesting {}MB, current usage {}MB/{}MB (plan: {})",
                req_memory_mb, usage.total_memory_mb, plan.max_memory_mb, plan.display_name
            )),
            plan,
            current_usage: usage,
        });
    }

    if new_total_disk > plan.max_storage_mb {
        return Ok(QuotaCheckResult {
            allowed: false,
            reason: Some(format!(
                "Storage limit exceeded: requesting {}MB, current usage {}MB/{}MB (plan: {})",
                req_disk_mb, usage.total_disk_mb, plan.max_storage_mb, plan.display_name
            )),
            plan,
            current_usage: usage,
        });
    }

    Ok(QuotaCheckResult {
        allowed: true,
        reason: None,
        plan,
        current_usage: usage,
    })
}

// ============= CONTAINER CONFIG PERSISTENCE =============
// Stores the full deploy parameters so `nordkraft upgrade` can
// merge partial changes against the last-known config.
//
// Migration SQL (run once):
//
//   CREATE TABLE IF NOT EXISTS container_config (
//       container_name TEXT PRIMARY KEY,
//       user_id        TEXT NOT NULL,
//       config         JSONB NOT NULL,
//       revision       INTEGER NOT NULL DEFAULT 1,
//       created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
//       updated_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
//   );
//
//   CREATE INDEX idx_container_config_user ON container_config(user_id);

use crate::models::{ContainerConfig, PortSpec, UpgradeRequest};
use std::collections::HashMap;

/// Store the full deploy config after a successful deployment.
/// Called from deploy_container route — insert or update on conflict.
pub async fn store_container_config(
    pool: &PgPool,
    user_id: &str,
    config: &ContainerConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let config_json = serde_json::to_value(config)?;

    sqlx::query(
        "INSERT INTO container_config (container_name, user_id, config, revision, created_at, updated_at)
         VALUES ($1, $2, $3, 1, NOW(), NOW())
         ON CONFLICT (container_name) DO UPDATE SET
           config = $3,
           revision = container_config.revision + 1,
           updated_at = NOW()",
    )
    .bind(&config.container_name)
    .bind(user_id)
    .bind(&config_json)
    .execute(pool)
    .await?;

    Ok(())
}

/// Load stored deploy config for a container.
/// Returns None if the container was deployed before config tracking existed.
pub async fn get_container_config(
    pool: &PgPool,
    container_name: &str,
    user_id: &str,
) -> Result<Option<ContainerConfig>, Box<dyn std::error::Error + Send + Sync>> {
    let row = sqlx::query(
        "SELECT config FROM container_config
         WHERE container_name = $1 AND user_id = $2",
    )
    .bind(container_name)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;

    match row {
        Some(row) => {
            let config_json: serde_json::Value = row.get("config");
            let config: ContainerConfig = serde_json::from_value(config_json)?;
            Ok(Some(config))
        }
        None => Ok(None),
    }
}

/// Get the current revision number for a container config.
pub async fn get_container_config_revision(
    pool: &PgPool,
    container_name: &str,
) -> Result<i32, Box<dyn std::error::Error + Send + Sync>> {
    let revision: Option<i32> =
        sqlx::query_scalar("SELECT revision FROM container_config WHERE container_name = $1")
            .bind(container_name)
            .fetch_optional(pool)
            .await?;

    Ok(revision.unwrap_or(0))
}

/// Merge an UpgradeRequest over an existing ContainerConfig.
/// Only fields present in the upgrade override the stored values.
pub fn merge_upgrade(base: &ContainerConfig, upgrade: &UpgradeRequest) -> ContainerConfig {
    ContainerConfig {
        container_name: base.container_name.clone(),
        image: upgrade.image.clone().unwrap_or_else(|| base.image.clone()),
        ports: upgrade.ports.clone().unwrap_or_else(|| base.ports.clone()),
        command: if upgrade.command.is_some() {
            upgrade.command.clone()
        } else {
            base.command.clone()
        },
        env_vars: upgrade
            .env_vars
            .clone()
            .unwrap_or_else(|| base.env_vars.clone()),
        cpu_limit: upgrade.cpu_limit.unwrap_or(base.cpu_limit),
        memory_limit: upgrade
            .memory_limit
            .clone()
            .unwrap_or_else(|| base.memory_limit.clone()),
        enable_persistence: base.enable_persistence, // never changes on upgrade
        volume_path: if upgrade.volume_path.is_some() {
            upgrade.volume_path.clone()
        } else {
            base.volume_path.clone()
        },
        volume_size: upgrade
            .volume_size
            .clone()
            .unwrap_or_else(|| base.volume_size.clone()),
        enable_ipv6: base.enable_ipv6, // never changes on upgrade
    }
}

/// Build a ContainerConfig from deploy parameters (called after deploy).
#[allow(clippy::too_many_arguments)]
pub fn build_container_config(
    container_name: &str,
    image: &str,
    ports: &Option<Vec<PortSpec>>,
    command: &Option<Vec<String>>,
    env_vars: &Option<HashMap<String, String>>,
    cpu_limit: Option<f32>,
    memory_limit: &Option<String>,
    enable_persistence: bool,
    volume_path: &Option<String>,
    volume_size: &Option<String>,
    enable_ipv6: bool,
) -> ContainerConfig {
    ContainerConfig {
        container_name: container_name.to_string(),
        image: image.to_string(),
        ports: ports.clone().unwrap_or_default(),
        command: command.clone(),
        env_vars: env_vars.clone().unwrap_or_default(),
        cpu_limit: cpu_limit.unwrap_or(0.5),
        memory_limit: memory_limit.clone().unwrap_or_else(|| "512m".to_string()),
        enable_persistence,
        volume_path: volume_path.clone(),
        volume_size: volume_size.clone().unwrap_or_else(|| "1g".to_string()),
        enable_ipv6,
    }
}
