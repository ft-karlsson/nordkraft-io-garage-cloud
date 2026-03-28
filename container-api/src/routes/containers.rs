// routes/containers.rs - Container management routes with IPv6 support

use crate::controller::OrchestratorService;
use crate::guards::AuthenticatedUser;
use crate::models::{DeployRequest, UpgradeRequest};
use crate::services::nats_service::NatsMessage;
use crate::storage::build_ipv6_url;
use crate::storage::*;
use crate::AppState;
use rocket::serde::json::Json;
use tracing::{info, warn};

// ============= ROUTES =============

#[post("/containers/deploy", data = "<deploy_req>")]
pub async fn deploy_container(
    deploy_req: Json<DeployRequest>,
    user: AuthenticatedUser,
    app_state: &rocket::State<AppState>,
    orchestrator: &rocket::State<OrchestratorService>,
    pool: &rocket::State<sqlx::PgPool>,
) -> Json<serde_json::Value> {
    // Determine target garage
    let target_garage = deploy_req
        .target_garage
        .clone()
        .unwrap_or_else(|| user.0.primary_garage_id.clone());

    // Get container subnet for garage
    let container_subnet =
        match get_user_garage_subnet(pool.inner(), &user.0.id, &target_garage).await {
            Ok(subnet) => subnet,
            Err(e) => {
                return Json(serde_json::json!({
                    "error": format!("Failed to get garage subnet: {}", e)
                }));
            }
        };

    // Find best node
    let target_node = match select_garage_hardware_node(
        pool.inner(),
        &target_garage,
        &deploy_req.hardware_preference,
        &deploy_req.architecture,
    )
    .await
    {
        Ok(node) => node,
        Err(e) => {
            return Json(serde_json::json!({
                "error": format!("Failed to select node: {}", e)
            }));
        }
    };

    let container_name = format!("app-{}", uuid::Uuid::new_v4());

    // ========== PLAN QUOTA CHECK ==========
    if app_state.config.quota_enforced {
        let requested_cpu = deploy_req.cpu_limit.unwrap_or(0.5);
        let requested_memory = deploy_req
            .memory_limit
            .clone()
            .unwrap_or_else(|| "512m".to_string());
        let requested_disk = deploy_req
            .volume_size
            .clone()
            .unwrap_or_else(|| "1g".to_string());
        let persistence = deploy_req.enable_persistence.unwrap_or(false);

        match check_plan_quota(
            pool.inner(),
            &user.0.id,
            requested_cpu,
            &requested_memory,
            &requested_disk,
            persistence,
        )
        .await
        {
            Ok(quota) if !quota.allowed => {
                return Json(serde_json::json!({
                    "error": quota.reason.unwrap_or_else(|| "Plan quota exceeded".to_string()),
                    "plan_id": quota.plan.plan_id,
                    "plan": quota.plan.display_name,
                    "usage": {
                        "cpu": format!("{:.1}/{:.1} vCPU", quota.current_usage.total_cpu, quota.plan.max_vcpus),
                        "memory": format!("{}MB/{}MB", quota.current_usage.total_memory_mb, quota.plan.max_memory_mb),
                        "disk": format!("{}MB/{}MB", quota.current_usage.total_disk_mb, quota.plan.max_storage_mb),
                        "containers": quota.current_usage.container_count,
                    }
                }));
            }
            Err(e) => {
                warn!("❌ Quota check failed (blocking deploy): {}", e);
                return Json(serde_json::json!({
                    "error": format!("Quota check failed: {}. Deploy blocked.", e)
                }));
            }
            _ => {} // allowed
        }
    }

    // Allocate IPv4
    let allocated_ip = match allocate_container_ip(pool.inner(), &user.0.id, &container_subnet)
        .await
    {
        Ok(ip) => {
            // Add route if remote deployment
            if target_node != orchestrator.node_id {
                if let Ok((node_internal_ip, interface)) =
                    get_node_network_info(pool.inner(), &target_node).await
                {
                    match app_state
                        .route_manager
                        .add_container_route(&format!("{}/32", ip), &node_internal_ip, &interface)
                        .await
                    {
                        Ok(_) => {
                            info!(
                                "✅ Added route {}/32 via {} dev {}",
                                ip, node_internal_ip, interface
                            );
                            true
                        }
                        Err(e) => {
                            warn!("Failed to add route for {}: {}", ip, e);
                            false
                        }
                    };
                }
            }
            Some(ip)
        }
        Err(e) => {
            warn!("Failed to allocate IP: {}, using dynamic", e);
            None
        }
    };

    // ========== SLAAC IPv6: No pre-allocation, store AFTER deployment ==========
    // IPv6 will be assigned by SLAAC (non-deterministic address)
    // We'll store it in the database AFTER successful deployment
    // ============================================================================

    // Deploy
    if target_node == orchestrator.node_id {
        // LOCAL deployment
        match app_state
            .container_manager
            .deploy_secure_container(
                &user.0.wireguard_public_key,
                &format!("tenant-{}", &user.0.id[..8]),
                &container_subnet,
                &deploy_req.image,
                deploy_req.ports.clone(),
                deploy_req.command.clone(),
                deploy_req.env_vars.clone(),
                deploy_req.cpu_limit,
                deploy_req.memory_limit.clone(),
                user.0.user_slot,
                deploy_req.enable_persistence.unwrap_or(false),
                deploy_req.volume_path.clone(),
                allocated_ip.clone(),
                Some(container_name.clone()),
                deploy_req.enable_ipv6, // Pass bool, not Option<String>
            )
            .await
        {
            Ok((container_name, _pod_name, container_ip, ports, deployed_ipv6)) => {
                let _ = track_container_deployment(
                    pool.inner(),
                    &container_name,
                    &container_name,
                    &user.0.id,
                    &target_node,
                    &deploy_req.image,
                    Some(&container_ip),
                    deploy_req.cpu_limit,
                    deploy_req.memory_limit.as_deref(),
                    deploy_req.volume_size.as_deref(),
                    deploy_req.enable_persistence.unwrap_or(false),
                )
                .await;

                // Store full deploy config for future upgrades
                let config = build_container_config(
                    &container_name,
                    &deploy_req.image,
                    &deploy_req.ports,
                    &deploy_req.command,
                    &deploy_req.env_vars,
                    deploy_req.cpu_limit,
                    &deploy_req.memory_limit,
                    deploy_req.enable_persistence.unwrap_or(false),
                    &deploy_req.volume_path,
                    &deploy_req.volume_size,
                    deploy_req.enable_ipv6,
                );
                if let Err(e) = store_container_config(pool.inner(), &user.0.id, &config).await {
                    warn!("Failed to store container config: {}", e);
                }

                let mut response = serde_json::json!({
                    "status": "deployed",
                    "garage": target_garage,
                    "node": target_node,
                    "container_name": container_name,
                    "container_ip": container_ip,
                    "ports": ports,
                });

                // Add IPv6 info to response if enabled
                if let Some(ipv6_addr) = deployed_ipv6 {
                    response["ipv6_address"] = serde_json::json!(ipv6_addr);
                    response["ipv6_enabled"] = serde_json::json!(true);

                    // Build IPv6 access URLs
                    let ipv6_urls: Vec<String> = ports
                        .iter()
                        .map(|p| build_ipv6_url(&ipv6_addr, p.port))
                        .collect();
                    response["ipv6_urls"] = serde_json::json!(ipv6_urls);
                }

                Json(response)
            }
            Err(e) => Json(serde_json::json!({"error": e.to_string()})),
        }
    } else {
        // REMOTE deployment via NATS
        if let Some(nats) = &orchestrator.nats_service {
            let assignment = NatsMessage::ContainerAssignment {
                job_id: uuid::Uuid::new_v4().to_string(),
                container_name: container_name.clone(),
                owner_pubkey: user.0.wireguard_public_key.clone(),
                tenant_id: format!("tenant-{}", &user.0.id[..8]),
                image: deploy_req.image.clone(),
                allocated_ip: allocated_ip.clone(),
                subnet: container_subnet,
                ports: deploy_req.ports.clone(),
                command: deploy_req.command.clone(),
                env_vars: deploy_req.env_vars.clone(),
                cpu_limit: deploy_req.cpu_limit,
                memory_limit: deploy_req.memory_limit.clone(),
                user_slot: user.0.user_slot,
                persistence_enabled: deploy_req.enable_persistence.unwrap_or(false),
                volume_path: deploy_req.volume_path.clone(),
                enable_ipv6: deploy_req.enable_ipv6,
                ipv6_address: None, // Will be assigned by agent via SLAAC
            };
            info!("📤 Sending NATS assignment to node {}", target_node);

            match nats.send_to_node(&target_node, assignment).await {
                Ok(_) => {
                    let _ = track_container_deployment(
                        pool.inner(),
                        &container_name,
                        &container_name,
                        &user.0.id,
                        &target_node,
                        &deploy_req.image,
                        allocated_ip.as_deref(),
                        deploy_req.cpu_limit,
                        deploy_req.memory_limit.as_deref(),
                        deploy_req.volume_size.as_deref(),
                        deploy_req.enable_persistence.unwrap_or(false),
                    )
                    .await;

                    // Store full deploy config for future upgrades
                    let config = build_container_config(
                        &container_name,
                        &deploy_req.image,
                        &deploy_req.ports,
                        &deploy_req.command,
                        &deploy_req.env_vars,
                        deploy_req.cpu_limit,
                        &deploy_req.memory_limit,
                        deploy_req.enable_persistence.unwrap_or(false),
                        &deploy_req.volume_path,
                        &deploy_req.volume_size,
                        deploy_req.enable_ipv6,
                    );
                    if let Err(e) = store_container_config(pool.inner(), &user.0.id, &config).await
                    {
                        warn!("Failed to store container config: {}", e);
                    }

                    let mut response = serde_json::json!({
                        "status": "scheduled",
                        "garage": target_garage,
                        "node": target_node,
                        "container_name": container_name,
                        "container_ip": allocated_ip.clone().unwrap_or_else(|| "pending".to_string()),
                        "message": "Container scheduled for deployment on remote node. \
                                   Use 'nordkraft list' to check deployment status.",
                        "note": "IPv6 will be assigned via SLAAC when container starts (if enabled)"
                    });

                    if deploy_req.enable_ipv6 {
                        response["ipv6_enabled"] = serde_json::json!(true);
                    }

                    Json(response)
                }
                Err(e) => {
                    if let Some(ip) = &allocated_ip {
                        let _ = app_state
                            .route_manager
                            .remove_container_route(&format!("{}/32", ip))
                            .await;
                    }
                    Json(serde_json::json!({"error": format!("Failed to schedule: {}", e)}))
                }
            }
        } else {
            // Cleanup if no NATS
            if allocated_ip.is_some() {
                debug!("no cleanup needed");
            }
            Json(serde_json::json!({"error": "NATS not available"}))
        }
    }
}

#[get("/containers")]
pub async fn list_containers_route(
    user: AuthenticatedUser,
    app_state: &rocket::State<AppState>,
    orchestrator: &rocket::State<OrchestratorService>,
) -> Json<serde_json::Value> {
    // Controller: aggregate from all nodes
    if let Some(nats) = &orchestrator.nats_service {
        if nats.is_controller() {
            let containers = orchestrator
                .query_all_nodes_for_containers(&user.0.wireguard_public_key)
                .await;

            return Json(serde_json::json!({
                "containers": containers,
                "source": "multi-node"
            }));
        }
    }

    // Agent/standalone: local only
    match app_state
        .container_manager
        .list_user_containers(&user.0.wireguard_public_key)
        .await
    {
        Ok(containers) => Json(serde_json::json!({
            "containers": containers,
            "source": "local"
        })),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

#[delete("/containers/<container_id>")]
pub async fn delete_container(
    container_id: String,
    user: AuthenticatedUser,
    app_state: &rocket::State<AppState>,
    orchestrator: &rocket::State<OrchestratorService>,
    pool: &rocket::State<sqlx::PgPool>,
) -> Json<serde_json::Value> {
    // Check local first
    let is_local = app_state
        .container_manager
        .list_user_containers(&user.0.wireguard_public_key)
        .await
        .unwrap_or_default()
        .iter()
        .any(|c| c.container_id == container_id || c.name == container_id);

    if is_local {
        // LOCAL deletion
        match app_state
            .container_manager
            .remove_container(
                &container_id,
                &user.0.wireguard_public_key,
                user.0.user_slot,
                orchestrator.nats_service.as_deref(),
                Some(&orchestrator.node_id),
            )
            .await
        {
            Ok(_) => {
                // Get IP BEFORE marking as deleted
                match sqlx::query_scalar::<_, String>(
                    "SELECT internal_ip FROM containers 
                     WHERE (container_name = $1 OR container_id = $1) AND status != 'deleted'",
                )
                .bind(&container_id)
                .fetch_optional(pool.inner())
                .await
                {
                    Ok(Some(container_ip)) => {
                        info!(
                            "🛣️ Removing route for local container: {} ({})",
                            container_id, container_ip
                        );

                        // Remove the actual route
                        match app_state
                            .route_manager
                            .remove_container_route(&container_ip)
                            .await
                        {
                            Ok(_) => info!("✅ Route removed for {}", container_ip),
                            Err(e) => warn!("Failed to remove route: {}", e),
                        }
                    }
                    Ok(None) => {
                        info!("⚠️ No route found for container {}", container_id);
                    }
                    Err(e) => {
                        warn!("Database error looking up route: {}", e);
                    }
                }
                // Mark container as deleted in database (prevents ghost containers)
                let _ = sqlx::query(
                    "UPDATE containers SET status = 'deleted', updated_at = NOW() 
                     WHERE container_name = $1 OR container_id = $1",
                )
                .bind(&container_id)
                .execute(pool.inner())
                .await;

                Json(serde_json::json!({
                    "status": "deleted",
                    "location": "local"
                }))
            }
            Err(e) => Json(serde_json::json!({"error": e.to_string()})),
        }
    } else if let Some(nats) = &orchestrator.nats_service {
        // REMOTE deletion - just send message, let orchestrator handle cleanup
        let target_node = find_container_node(pool.inner(), &container_id).await.ok();

        let delete_msg = NatsMessage::ContainerDelete {
            container_id: container_id.clone(),
            container_name: container_id.clone(),
            owner_pubkey: user.0.wireguard_public_key.clone(),
            user_slot: user.0.user_slot,
        };

        if let Some(node_id) = target_node {
            match nats.send_to_node(&node_id, delete_msg).await {
                Ok(_) => {
                    info!(
                        "📤 Sent delete request for {} to node {}",
                        container_id, node_id
                    );

                    // Mark container as deleted in database
                    // The actual container removal happens on the remote node
                    let _ = sqlx::query(
                        "UPDATE containers SET status = 'deleted', updated_at = NOW() 
                         WHERE container_name = $1 OR container_id = $1",
                    )
                    .bind(&container_id)
                    .execute(pool.inner())
                    .await;

                    Json(serde_json::json!({
                        "status": "delete_initiated",
                        "node": node_id,
                        "message": "Container deletion in progress on remote node"
                    }))
                }
                Err(e) => Json(serde_json::json!({"error": e.to_string()})),
            }
        } else {
            // Broadcast to all nodes if we don't know which node has it
            info!(
                "📡 Broadcasting delete request for {} to all nodes",
                container_id
            );
            for node in orchestrator.get_nodes().await {
                let _ = nats.send_to_node(&node.id, delete_msg.clone()).await;
            }

            // Mark container as deleted in database
            let _ = sqlx::query(
                "UPDATE containers SET status = 'deleted', updated_at = NOW() 
                 WHERE container_name = $1 OR container_id = $1",
            )
            .bind(&container_id)
            .execute(pool.inner())
            .await;

            Json(serde_json::json!({
                "status": "delete_broadcast",
                "message": "Delete request sent to all nodes"
            }))
        }
    } else {
        Json(serde_json::json!({
            "error": "Remote deletion requires NATS"
        }))
    }
}

#[post("/containers/<container_id>/start")]
pub async fn start_container(
    container_id: String,
    user: AuthenticatedUser,
    app_state: &rocket::State<AppState>,
    orchestrator: &rocket::State<OrchestratorService>,
    pool: &rocket::State<sqlx::PgPool>,
) -> Json<serde_json::Value> {
    // Try local
    let is_local = app_state
        .container_manager
        .list_user_containers(&user.0.wireguard_public_key)
        .await
        .unwrap_or_default()
        .iter()
        .any(|c| c.container_id == container_id || c.name == container_id);

    if is_local {
        match app_state
            .container_manager
            .start_container(&container_id, &user.0.wireguard_public_key)
            .await
        {
            Ok(_) => Json(serde_json::json!({"status": "started"})),
            Err(e) => Json(serde_json::json!({"error": e.to_string()})),
        }
    } else if let Some(nats) = &orchestrator.nats_service {
        // Remote
        let target_node = find_container_node(pool.inner(), &container_id).await.ok();
        let msg = NatsMessage::ContainerStart {
            container_id: container_id.clone(),
            owner_pubkey: user.0.wireguard_public_key.clone(),
        };

        if let Some(node_id) = target_node {
            let _ = nats.send_to_node(&node_id, msg).await;
            Json(serde_json::json!({"status": "start_initiated", "node": node_id}))
        } else {
            for node in orchestrator.get_nodes().await {
                let _ = nats.send_to_node(&node.id, msg.clone()).await;
            }
            Json(serde_json::json!({"status": "start_broadcast"}))
        }
    } else {
        Json(serde_json::json!({"error": "NATS required"}))
    }
}

#[post("/containers/<container_id>/stop")]
pub async fn stop_container(
    container_id: String,
    user: AuthenticatedUser,
    app_state: &rocket::State<AppState>,
    orchestrator: &rocket::State<OrchestratorService>,
    pool: &rocket::State<sqlx::PgPool>,
) -> Json<serde_json::Value> {
    // Try local
    let is_local = app_state
        .container_manager
        .list_user_containers(&user.0.wireguard_public_key)
        .await
        .unwrap_or_default()
        .iter()
        .any(|c| c.container_id == container_id || c.name == container_id);

    if is_local {
        match app_state
            .container_manager
            .stop_container(&container_id, &user.0.wireguard_public_key)
            .await
        {
            Ok(_) => Json(serde_json::json!({"status": "stopped"})),
            Err(e) => Json(serde_json::json!({"error": e.to_string()})),
        }
    } else if let Some(nats) = &orchestrator.nats_service {
        // Remote
        let target_node = find_container_node(pool.inner(), &container_id).await.ok();
        let msg = NatsMessage::ContainerStop {
            container_id: container_id.clone(),
            owner_pubkey: user.0.wireguard_public_key.clone(),
        };

        if let Some(node_id) = target_node {
            let _ = nats.send_to_node(&node_id, msg).await;
            Json(serde_json::json!({"status": "stop_initiated", "node": node_id}))
        } else {
            for node in orchestrator.get_nodes().await {
                let _ = nats.send_to_node(&node.id, msg.clone()).await;
            }
            Json(serde_json::json!({"status": "stop_broadcast"}))
        }
    } else {
        Json(serde_json::json!({"error": "NATS required"}))
    }
}

#[get("/containers/<container_id>/logs?<lines>")]
pub async fn get_container_logs(
    container_id: String,
    lines: Option<usize>,
    user: AuthenticatedUser,
    app_state: &rocket::State<AppState>,
    orchestrator: &rocket::State<OrchestratorService>,
    pool: &rocket::State<sqlx::PgPool>,
) -> Json<serde_json::Value> {
    // Try local
    if let Ok(logs) = app_state
        .container_manager
        .get_container_logs(&container_id, &user.0.wireguard_public_key, lines)
        .await
    {
        return Json(serde_json::json!({"logs": logs, "source": "local"}));
    }

    // Remote
    let preferred_node = find_container_node(pool.inner(), &container_id).await.ok();
    match orchestrator
        .request_container_logs(
            &user.0.wireguard_public_key,
            &container_id,
            lines,
            preferred_node,
        )
        .await
    {
        Some(logs) => Json(serde_json::json!({"logs": logs, "source": "remote"})),
        None => Json(serde_json::json!({"error": "Failed to fetch logs"})),
    }
}

/// GET /containers/<id>  — rich inspect data for a single container.
/// 1. Check DB for which node owns this container
/// GET /containers/<id>  — rich inspect data for a single container.
/// Tries local first; if not local, broadcasts via NATS to all agent nodes.
/// The agent that owns the container responds; others stay silent.
#[get("/containers/<container_id>")]
pub async fn inspect_container(
    container_id: String,
    user: AuthenticatedUser,
    app_state: &rocket::State<AppState>,
    orchestrator: &rocket::State<OrchestratorService>,
) -> Json<serde_json::Value> {
    // Try local first (hybrid mode or single-node)
    if let Ok(data) = app_state
        .container_manager
        .inspect_container(
            &container_id,
            &user.0.wireguard_public_key,
            &orchestrator.node_id,
        )
        .await
    {
        return Json(
            serde_json::to_value(data)
                .unwrap_or_else(|_| serde_json::json!({"error": "serialization failed"})),
        );
    }

    // Broadcast via NATS — no need to know which node, agents self-select
    match orchestrator
        .request_container_inspect(&user.0.wireguard_public_key, &container_id, None)
        .await
    {
        Some(data) => Json(
            serde_json::to_value(data)
                .unwrap_or_else(|_| serde_json::json!({"error": "serialization failed"})),
        ),
        None => Json(serde_json::json!({"error": "Container not found or not accessible"})),
    }
}

// ============= UPGRADE =============

/// PUT /containers/<name>/upgrade — in-place upgrade preserving IP + volumes.
///
/// Flow:
///   1. Load stored config from container_config table
///   2. Merge partial UpgradeRequest over stored config
///   3. Stop + remove old container (volumes preserved on disk)
///   4. Redeploy with same name, same IP, updated config
///   5. Update DB records + stored config
#[put("/containers/<container_name>/upgrade", data = "<upgrade_req>")]
pub async fn upgrade_container(
    container_name: String,
    upgrade_req: Json<UpgradeRequest>,
    user: AuthenticatedUser,
    app_state: &rocket::State<AppState>,
    orchestrator: &rocket::State<OrchestratorService>,
    pool: &rocket::State<sqlx::PgPool>,
) -> Json<serde_json::Value> {
    // 1. Verify container exists and belongs to user
    let _container_info = match get_container_info(pool.inner(), &container_name, &user.0.id).await
    {
        Ok(Some(info)) => info,
        Ok(None) => {
            return Json(serde_json::json!({
                "error": format!("Container '{}' not found or access denied", container_name)
            }));
        }
        Err(_) => {
            return Json(serde_json::json!({
                "error": format!("Container '{}' not found or access denied", container_name)
            }));
        }
    };

    // 2. Load stored deploy config
    let stored_config = match get_container_config(pool.inner(), &container_name, &user.0.id).await
    {
        Ok(Some(config)) => config,
        Ok(None) => {
            return Json(serde_json::json!({
                "error": format!(
                    "No stored config for '{}'. This container was deployed before config tracking. \
                     Run 'nordkraft init {}' to generate a config, then retry.",
                    container_name, container_name
                )
            }));
        }
        Err(e) => {
            return Json(serde_json::json!({
                "error": format!("Failed to load config: {}", e)
            }));
        }
    };

    // 3. Merge upgrade over stored config
    let merged = merge_upgrade(&stored_config, &upgrade_req);

    // 4. Get current container IP (to preserve across upgrade)
    let container_ip = match get_container_ipv4(pool.inner(), &container_name, &user.0.id).await {
        Ok(Some(ip)) => ip,
        Ok(None) => {
            return Json(serde_json::json!({
                "error": format!("No IP found for '{}' — cannot preserve address across upgrade", container_name)
            }));
        }
        Err(e) => {
            return Json(serde_json::json!({
                "error": format!("Failed to get container IP: {}", e)
            }));
        }
    };

    // 5. Find which node runs this container
    let node_id = match find_container_node(pool.inner(), &container_name).await {
        Ok(n) => n,
        Err(e) => {
            return Json(serde_json::json!({
                "error": format!("Cannot find node for container: {}", e)
            }));
        }
    };

    // 6. Get subnet (needed by both local and remote paths)
    let container_subnet =
        match get_user_garage_subnet(pool.inner(), &user.0.id, &user.0.primary_garage_id).await {
            Ok(subnet) => subnet,
            Err(e) => {
                return Json(serde_json::json!({
                    "error": format!("Failed to get subnet: {}", e)
                }));
            }
        };

    let tenant_id = format!("tenant-{}", &user.0.id[..8]);

    // 7. Check if local or remote
    if node_id == orchestrator.node_id {
        // ===== LOCAL UPGRADE =====
        upgrade_local(
            &container_name,
            &merged,
            &container_ip,
            &container_subnet,
            &tenant_id,
            &node_id,
            &user,
            app_state,
            pool,
        )
        .await
    } else {
        // ===== REMOTE UPGRADE via NATS =====
        if let Some(nats) = &orchestrator.nats_service {
            let msg = NatsMessage::ContainerUpgrade {
                container_name: container_name.clone(),
                owner_pubkey: user.0.wireguard_public_key.clone(),
                tenant_id: tenant_id.clone(),
                subnet: container_subnet.clone(),
                user_slot: user.0.user_slot,
                config: merged.clone(),
                container_ip: container_ip.clone(),
            };

            match nats.send_to_node(&node_id, msg).await {
                Ok(_) => {
                    // Update config in DB immediately (agent handles container)
                    if let Err(e) = store_container_config(pool.inner(), &user.0.id, &merged).await
                    {
                        warn!("Failed to update stored config: {}", e);
                    }

                    // Update image in containers table
                    let _ = sqlx::query(
                        "UPDATE containers SET image = $1, updated_at = NOW()
                         WHERE container_name = $2 AND user_id = $3",
                    )
                    .bind(&merged.image)
                    .bind(&container_name)
                    .bind(&user.0.id)
                    .execute(pool.inner())
                    .await;

                    let old_rev = get_container_config_revision(pool.inner(), &container_name)
                        .await
                        .unwrap_or(0);

                    Json(serde_json::json!({
                        "status": "upgrade_scheduled",
                        "container_name": container_name,
                        "node": node_id,
                        "revision": old_rev,
                        "image": merged.image,
                        "message": "Upgrade scheduled on remote node"
                    }))
                }
                Err(e) => Json(serde_json::json!({
                    "error": format!("Failed to send upgrade to node {}: {}", node_id, e)
                })),
            }
        } else {
            Json(serde_json::json!({
                "error": "Remote upgrade requires NATS"
            }))
        }
    }
}

/// Perform a local in-place upgrade: stop → rm (preserve volumes) → redeploy.
#[allow(clippy::too_many_arguments)]
async fn upgrade_local(
    container_name: &str,
    merged: &crate::models::ContainerConfig,
    container_ip: &str,
    container_subnet: &str,
    tenant_id: &str,
    node_id: &str,
    user: &AuthenticatedUser,
    app_state: &rocket::State<AppState>,
    pool: &rocket::State<sqlx::PgPool>,
) -> Json<serde_json::Value> {
    let old_rev = get_container_config_revision(pool.inner(), container_name)
        .await
        .unwrap_or(0);

    // Step A: Stop the container
    info!("⏸️  Upgrade: stopping {}", container_name);
    if let Err(e) = app_state
        .container_manager
        .stop_container(container_name, &user.0.wireguard_public_key)
        .await
    {
        warn!("Stop failed (may already be stopped): {}", e);
    }

    // Step B: Remove container WITHOUT cleaning up volumes.
    // We call nerdctl rm directly — NOT remove_container() which deletes volumes.
    info!("🗑️  Upgrade: removing container shell {}", container_name);
    let rm_output = tokio::process::Command::new("nerdctl")
        .args(["rm", "-f", container_name])
        .output()
        .await;

    if let Err(e) = &rm_output {
        return Json(serde_json::json!({
            "error": format!("Failed to remove old container: {}", e)
        }));
    }

    // Step C: Redeploy with same name + same IP + updated config
    info!(
        "🚀 Upgrade: redeploying {} with image {}",
        container_name, merged.image
    );

    let ports_opt = if merged.ports.is_empty() {
        None
    } else {
        Some(merged.ports.clone())
    };

    match app_state
        .container_manager
        .deploy_secure_container(
            &user.0.wireguard_public_key,
            tenant_id,
            container_subnet,
            &merged.image,
            ports_opt,
            merged.command.clone(),
            if merged.env_vars.is_empty() {
                None
            } else {
                Some(merged.env_vars.clone())
            },
            Some(merged.cpu_limit),
            Some(merged.memory_limit.clone()),
            user.0.user_slot,
            merged.enable_persistence,
            merged.volume_path.clone(),
            Some(container_ip.to_string()),   // preserve IP
            Some(container_name.to_string()), // preserve name
            merged.enable_ipv6,
        )
        .await
    {
        Ok((new_name, _pod, new_ip, ports, deployed_ipv6)) => {
            // Update containers table
            let _ = sqlx::query(
                "UPDATE containers SET image = $1, status = 'running', updated_at = NOW()
                 WHERE container_name = $2 AND user_id = $3",
            )
            .bind(&merged.image)
            .bind(container_name)
            .bind(&user.0.id)
            .execute(pool.inner())
            .await;

            // Update stored config
            if let Err(e) = store_container_config(pool.inner(), &user.0.id, merged).await {
                warn!("Failed to update stored config: {}", e);
            }

            let new_rev = old_rev + 1;

            let mut response = serde_json::json!({
                "status": "upgraded",
                "container_name": new_name,
                "container_ip": new_ip,
                "node": node_id,
                "image": merged.image,
                "revision_old": old_rev,
                "revision": new_rev,
                "ports": ports,
            });

            if let Some(ipv6_addr) = deployed_ipv6 {
                response["ipv6_address"] = serde_json::json!(ipv6_addr);
                response["ipv6_enabled"] = serde_json::json!(true);
            }

            info!(
                "✅ Upgraded {} to revision {} (image: {})",
                container_name, new_rev, merged.image
            );

            Json(response)
        }
        Err(e) => {
            // Upgrade failed — container is now stopped and removed.
            // This is a bad state. Log prominently.
            warn!(
                "❌ UPGRADE FAILED for {}: {}. Container is DOWN. Manual intervention needed.",
                container_name, e
            );
            Json(serde_json::json!({
                "error": format!("Upgrade failed during redeploy: {}. Container is stopped — manual redeploy required.", e),
                "container_name": container_name,
                "state": "down",
            }))
        }
    }
}

// ============= PLAN USAGE =============

#[get("/usage")]
pub async fn get_usage(
    user: AuthenticatedUser,
    pool: &rocket::State<sqlx::PgPool>,
) -> Json<serde_json::Value> {
    let plan = match get_user_plan_limits(pool.inner(), &user.0.id).await {
        Ok(p) => p,
        Err(e) => {
            return Json(serde_json::json!({
                "error": format!("Failed to fetch plan: {}", e)
            }));
        }
    };

    let usage = match get_user_resource_usage(pool.inner(), &user.0.id).await {
        Ok(u) => u,
        Err(e) => {
            return Json(serde_json::json!({
                "error": format!("Failed to fetch usage: {}", e)
            }));
        }
    };

    Json(serde_json::json!({
        "plan": {
            "id": plan.plan_id,
            "name": plan.display_name,
            "limits": {
                "cpu": plan.max_vcpus,
                "memory_mb": plan.max_memory_mb,
                "storage_mb": plan.max_storage_mb,
            }
        },
        "usage": {
            "cpu": usage.total_cpu,
            "memory_mb": usage.total_memory_mb,
            "disk_mb": usage.total_disk_mb,
            "containers": usage.container_count,
        },
        "ratios": {
            "cpu": if plan.max_vcpus > 0.0 { usage.total_cpu as f64 / plan.max_vcpus as f64 } else { 0.0 },
            "memory": if plan.max_memory_mb > 0 { usage.total_memory_mb as f64 / plan.max_memory_mb as f64 } else { 0.0 },
            "disk": if plan.max_storage_mb > 0 { usage.total_disk_mb as f64 / plan.max_storage_mb as f64 } else { 0.0 },
        }
    }))
}
