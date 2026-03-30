// routes/ipv6.rs - IPv6 firewall management
//
// The ipv6_firewall_rules table only tracks: container_name, ports, rule_id, sync status
// IPv6 addresses come from actual containers (SLAAC assigned) - in future we might want to assign static IPv6 from a pool.

use crate::controller::OrchestratorService;
use crate::guards::AuthenticatedUser;
use crate::services::pfsense_client::PfSenseClientTrait;
use crate::AppState;
use rocket::serde::json::Json;
use rocket::State;
use std::sync::Arc;
use tracing::{error, info, warn};

/// Get container IPv6 by listing containers (same as /containers route does)
async fn get_container_with_ipv6(
    container_id: &str,
    user_pubkey: &str,
    app_state: &AppState,
    orchestrator: &OrchestratorService,
) -> Result<(String, String), String> {
    // Get containers the exact same way /containers list does
    let containers = if let Some(nats) = &orchestrator.nats_service {
        if nats.is_controller() {
            // Multi-node: query all nodes
            orchestrator
                .query_all_nodes_for_containers(user_pubkey)
                .await
        } else {
            // Agent: local only
            app_state
                .container_manager
                .list_user_containers(user_pubkey)
                .await
                .map_err(|e| e.to_string())?
        }
    } else {
        // No NATS: local only
        app_state
            .container_manager
            .list_user_containers(user_pubkey)
            .await
            .map_err(|e| e.to_string())?
    };

    // Find the container
    let container = containers
        .iter()
        .find(|c| c.container_id == container_id || c.name == container_id)
        .ok_or_else(|| format!("Container {} not found", container_id))?;

    // Get IPv6 from the container info (already populated by list_user_containers!)
    let ipv6 = container
        .ipv6_address
        .as_ref()
        .ok_or_else(|| format!("Container {} has no IPv6 address", container.name))?;

    info!("✅ Found container {} with IPv6: {}", container.name, ipv6);

    Ok((container.name.clone(), ipv6.clone()))
}

/// Get firewall state from the NEW ipv6_firewall_rules table
async fn get_firewall_state(
    pool: &sqlx::PgPool,
    container_name: &str,
    user_id: &str,
) -> Result<(Vec<u16>, Option<String>, bool), Box<dyn std::error::Error + Send + Sync>> {
    let result = sqlx::query_as::<_, (Vec<i32>, Option<String>, bool)>(
        "SELECT exposed_ports, pfsense_rule_id, pfsense_rule_synced
         FROM ipv6_firewall_rules
         WHERE container_name = $1 AND user_id = $2
         ORDER BY created_at DESC
         LIMIT 1",
    )
    .bind(container_name)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;

    if let Some((ports_i32, rule_id, synced)) = result {
        let ports: Vec<u16> = ports_i32.into_iter().map(|p| p as u16).collect();
        Ok((ports, rule_id, synced))
    } else {
        // No record - use defaults
        Ok((vec![80, 443], None, false))
    }
}

/// Ensure firewall rule record exists in database
async fn ensure_firewall_record(
    pool: &sqlx::PgPool,
    container_name: &str,
    user_id: &str,
    exposed_ports: &[u16],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let ports_i32: Vec<i32> = exposed_ports.iter().map(|&p| p as i32).collect();

    // Insert or update
    sqlx::query(
        "INSERT INTO ipv6_firewall_rules (container_name, user_id, exposed_ports, created_at, updated_at)
         VALUES ($1, $2, $3, NOW(), NOW())
         ON CONFLICT (container_name) DO UPDATE 
         SET exposed_ports = $3, updated_at = NOW()",
    )
    .bind(container_name)
    .bind(user_id)
    .bind(&ports_i32)
    .execute(pool)
    .await?;

    Ok(())
}

/// Open IPv6 firewall for container
#[post("/ipv6/<container_id>/open")]
pub async fn open_ipv6_firewall(
    container_id: String,
    user: AuthenticatedUser,
    app_state: &State<AppState>,
    orchestrator: &State<OrchestratorService>,
    pfsense: &State<Arc<dyn PfSenseClientTrait>>,
    pool: &rocket::State<sqlx::PgPool>,
) -> Json<serde_json::Value> {
    info!("🔓 Opening IPv6 firewall for container: {}", container_id);

    // Step 1: Get container and IPv6 (same way /containers does)
    let (container_name, ipv6_address) = match get_container_with_ipv6(
        &container_id,
        &user.0.wireguard_public_key,
        app_state,
        orchestrator,
    )
    .await
    {
        Ok((name, ipv6)) => (name, ipv6),
        Err(e) => {
            error!("❌ Failed to get container: {}", e);
            return Json(serde_json::json!({"error": e}));
        }
    };

    // Step 2: Get firewall state from NEW table
    let (exposed_ports, existing_rule_id, _synced) =
        match get_firewall_state(pool.inner(), &container_name, &user.0.id).await {
            Ok(state) => state,
            Err(e) => {
                error!("❌ Database error: {}", e);
                return Json(serde_json::json!({"error": e.to_string()}));
            }
        };

    // Step 3: Create or verify pfSense rule
    let rule_id = if let Some(rule_id) = existing_rule_id {
        info!("♻️  Firewall rule already exists: {}", rule_id);
        rule_id
    } else {
        info!(
            "🔥 Creating pfSense firewall rule for {} ports {:?}",
            ipv6_address, exposed_ports
        );

        let ports_i32: Vec<i32> = exposed_ports.iter().map(|&p| p as i32).collect();

        match (**pfsense)
            .add_container_rule(&ipv6_address, &ports_i32, &container_name, &user.0.id)
            .await
        {
            Ok(rule_info) => {
                info!("✅ Created pfSense rule: {}", rule_info.rule_id);

                // Ensure record exists and update with rule ID
                if let Err(e) = ensure_firewall_record(
                    pool.inner(),
                    &container_name,
                    &user.0.id,
                    &exposed_ports,
                )
                .await
                {
                    warn!("⚠️  Failed to create firewall record: {}", e);
                }

                // Update with rule ID
                if let Err(e) = sqlx::query(
                    "UPDATE ipv6_firewall_rules 
                     SET pfsense_rule_id = $1, pfsense_rule_synced = true, updated_at = NOW()
                     WHERE container_name = $2 AND user_id = $3",
                )
                .bind(&rule_info.rule_id)
                .bind(&container_name)
                .bind(&user.0.id)
                .execute(pool.inner())
                .await
                {
                    warn!("⚠️  Failed to update rule ID in database: {}", e);
                }

                rule_info.rule_id
            }
            Err(e) => {
                error!("❌ Failed to create pfSense rule: {}", e);
                return Json(serde_json::json!({"error": e.to_string()}));
            }
        }
    };

    // Step 4: Generate access URLs
    let access_urls: Vec<String> = exposed_ports
        .iter()
        .map(|&port| match port {
            80 => format!("http://[{}]/", ipv6_address),
            443 => format!("https://[{}]/", ipv6_address),
            _ => format!("http://[{}]:{}/", ipv6_address, port),
        })
        .collect();

    Json(serde_json::json!({
        "status": "opened",
        "container_name": container_name,
        "ipv6_address": ipv6_address,
        "ports": exposed_ports,
        "rule_id": rule_id,
        "access_urls": access_urls,
        "message": "Firewall opened - container now accessible from internet via IPv6"
    }))
}

/// Close IPv6 firewall for container
#[post("/ipv6/<container_id>/close")]
pub async fn close_ipv6_firewall(
    container_id: String,
    user: AuthenticatedUser,
    app_state: &State<AppState>,
    orchestrator: &State<OrchestratorService>,
    pfsense: &State<Arc<dyn PfSenseClientTrait>>,
    pool: &rocket::State<sqlx::PgPool>,
) -> Json<serde_json::Value> {
    info!("🔒 Closing IPv6 firewall for container: {}", container_id);

    // Step 1: Get container info
    let (container_name, ipv6_address) = match get_container_with_ipv6(
        &container_id,
        &user.0.wireguard_public_key,
        app_state,
        orchestrator,
    )
    .await
    {
        Ok((name, ipv6)) => (name, ipv6),
        Err(e) => {
            error!("❌ Failed to get container: {}", e);
            return Json(serde_json::json!({"error": e}));
        }
    };

    // Step 2: Get rule ID from NEW table
    let (_ports, rule_id, _synced) =
        match get_firewall_state(pool.inner(), &container_name, &user.0.id).await {
            Ok(state) => state,
            Err(e) => {
                error!("❌ Database error: {}", e);
                return Json(serde_json::json!({"error": e.to_string()}));
            }
        };

    let rule_id = match rule_id {
        Some(id) => id,
        None => {
            return Json(serde_json::json!({
                "status": "already_closed",
                "message": "No firewall rule exists for this container"
            }));
        }
    };

    // Step 3: Remove pfSense rule
    info!("🔥 Removing pfSense rule: {}", rule_id);
    match (**pfsense).remove_rule(&rule_id).await {
        Ok(_) => {
            info!("✅ Removed pfSense rule: {}", rule_id);

            // Update NEW table
            if let Err(e) = sqlx::query(
                "UPDATE ipv6_firewall_rules 
                 SET pfsense_rule_id = NULL, pfsense_rule_synced = false, updated_at = NOW()
                 WHERE container_name = $1 AND user_id = $2",
            )
            .bind(&container_name)
            .bind(&user.0.id)
            .execute(pool.inner())
            .await
            {
                warn!("⚠️  Failed to clear rule ID in database: {}", e);
            }
        }
        Err(e) => {
            error!("❌ Failed to remove pfSense rule: {}", e);
            return Json(serde_json::json!({"error": e.to_string()}));
        }
    }

    Json(serde_json::json!({
        "status": "closed",
        "container_name": container_name,
        "ipv6_address": ipv6_address,
        "message": "Firewall closed - container no longer accessible from internet"
    }))
}

/// Get IPv6 firewall status
#[get("/ipv6/<container_id>/status")]
pub async fn get_ipv6_status(
    container_id: String,
    user: AuthenticatedUser,
    app_state: &State<AppState>,
    orchestrator: &State<OrchestratorService>,
    pool: &rocket::State<sqlx::PgPool>,
) -> Json<serde_json::Value> {
    // Get container info with REAL IPv6
    let (container_name, ipv6_address) = match get_container_with_ipv6(
        &container_id,
        &user.0.wireguard_public_key,
        app_state,
        orchestrator,
    )
    .await
    {
        Ok((name, ipv6)) => (name, ipv6),
        Err(e) => {
            return Json(serde_json::json!({"error": e}));
        }
    };

    // Get firewall state from NEW table
    let (exposed_ports, rule_id, synced) =
        match get_firewall_state(pool.inner(), &container_name, &user.0.id).await {
            Ok(state) => state,
            Err(e) => {
                return Json(serde_json::json!({"error": e.to_string()}));
            }
        };

    let firewall_status = if rule_id.is_some() { "open" } else { "closed" };

    let access_urls: Vec<String> = if rule_id.is_some() {
        exposed_ports
            .iter()
            .map(|&port| match port {
                80 => format!("http://[{}]/", ipv6_address),
                443 => format!("https://[{}]/", ipv6_address),
                _ => format!("http://[{}]:{}/", ipv6_address, port),
            })
            .collect()
    } else {
        vec![]
    };

    Json(serde_json::json!({
        "container_name": container_name,
        "ipv6_address": ipv6_address,
        "exposed_ports": exposed_ports,
        "firewall_status": firewall_status,
        "rule_id": rule_id,
        "pfsense_configured": synced,
        "access_urls": access_urls
    }))
}

/// List all IPv6 allocations for user - NOW USES REAL CONTAINER IPv6!
#[get("/ipv6/list")]
pub async fn list_ipv6_allocations(
    user: AuthenticatedUser,
    app_state: &State<AppState>,
    orchestrator: &State<OrchestratorService>,
    pool: &rocket::State<sqlx::PgPool>,
) -> Json<serde_json::Value> {
    // ========== KEY FIX: Get containers with REAL IPv6 addresses ==========
    let containers = if let Some(nats) = &orchestrator.nats_service {
        if nats.is_controller() {
            orchestrator
                .query_all_nodes_for_containers(&user.0.wireguard_public_key)
                .await
        } else {
            app_state
                .container_manager
                .list_user_containers(&user.0.wireguard_public_key)
                .await
                .unwrap_or_default()
        }
    } else {
        app_state
            .container_manager
            .list_user_containers(&user.0.wireguard_public_key)
            .await
            .unwrap_or_default()
    };

    // Filter to only IPv6-enabled containers
    let ipv6_containers: Vec<_> = containers
        .iter()
        .filter(|c| c.ipv6_address.is_some())
        .collect();

    // Build allocations list with REAL IPv6 and firewall status
    let mut allocations = Vec::new();

    for container in ipv6_containers {
        let ipv6_addr = container.ipv6_address.as_ref().unwrap();
        let container_name = &container.name;

        // Get firewall status from NEW table
        let (exposed_ports, rule_id, synced) =
            match get_firewall_state(pool.inner(), container_name, &user.0.id).await {
                Ok(state) => state,
                Err(_) => (vec![80], None, false),
            };

        let firewall_status = if rule_id.is_some() { "open" } else { "closed" };

        let access_urls: Vec<String> = if rule_id.is_some() {
            exposed_ports
                .iter()
                .map(|&port| match port {
                    80 => format!("http://[{}]/", ipv6_addr),
                    443 => format!("https://[{}]/", ipv6_addr),
                    _ => format!("http://[{}]:{}/", ipv6_addr, port),
                })
                .collect()
        } else {
            vec![]
        };

        allocations.push(serde_json::json!({
            "container_name": container_name,
            "ipv6_address": ipv6_addr,
            "exposed_ports": exposed_ports,
            "firewall_status": firewall_status,
            "rule_id": rule_id,
            "pfsense_configured": synced,
            "access_urls": access_urls
        }));
    }

    Json(serde_json::json!({
        "allocations": allocations,
        "count": allocations.len(),
        "pfsense_configured": true
    }))
}

/// Update exposed ports for container IPv6
#[post("/ipv6/<container_id>/ports", data = "<ports_request>")]
pub async fn update_ipv6_ports(
    container_id: String,
    ports_request: Json<serde_json::Value>,
    user: AuthenticatedUser,
    app_state: &State<AppState>,
    orchestrator: &State<OrchestratorService>,
    pfsense: &State<Arc<dyn PfSenseClientTrait>>,
    pool: &rocket::State<sqlx::PgPool>,
) -> Json<serde_json::Value> {
    // Parse ports
    let new_ports: Vec<u16> = match ports_request.get("ports") {
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_u64().map(|n| n as u16))
            .collect(),
        _ => {
            return Json(serde_json::json!({"error": "Invalid ports format"}));
        }
    };

    if new_ports.is_empty() {
        return Json(serde_json::json!({"error": "No valid ports provided"}));
    }

    // Get container info with REAL IPv6
    let (container_name, ipv6_address) = match get_container_with_ipv6(
        &container_id,
        &user.0.wireguard_public_key,
        app_state,
        orchestrator,
    )
    .await
    {
        Ok((name, ipv6)) => (name, ipv6),
        Err(e) => {
            return Json(serde_json::json!({"error": e}));
        }
    };

    // Get current firewall state from NEW table
    let (_old_ports, rule_id, _synced) =
        match get_firewall_state(pool.inner(), &container_name, &user.0.id).await {
            Ok(state) => state,
            Err(e) => {
                return Json(serde_json::json!({"error": e.to_string()}));
            }
        };

    let rule_id = match rule_id {
        Some(id) => id,
        None => {
            return Json(serde_json::json!({
                "error": "No firewall rule exists. Open firewall first."
            }));
        }
    };

    // Update pfSense rule
    let ports_i32: Vec<i32> = new_ports.iter().map(|&p| p as i32).collect();
    match (**pfsense)
        .update_rule_ports(
            &rule_id,
            &ipv6_address,
            &ports_i32,
            &container_name,
            &user.0.id,
        )
        .await
    {
        Ok(_) => {
            info!("✅ Updated pfSense rule ports: {:?}", new_ports);

            // Update NEW table
            if let Err(e) = sqlx::query(
                "UPDATE ipv6_firewall_rules 
                 SET exposed_ports = $1, updated_at = NOW()
                 WHERE container_name = $2 AND user_id = $3",
            )
            .bind(&ports_i32)
            .bind(&container_name)
            .bind(&user.0.id)
            .execute(pool.inner())
            .await
            {
                warn!("⚠️  Failed to update ports in database: {}", e);
            }
        }
        Err(e) => {
            return Json(serde_json::json!({"error": e.to_string()}));
        }
    }

    Json(serde_json::json!({
        "status": "updated",
        "container_name": container_name,
        "ipv6_address": ipv6_address,
        "new_ports": new_ports,
        "firewall_status": "open"
    }))
}
