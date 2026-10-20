// routes/containers.rs - Container management routes with IPv6 support

use crate::controller::OrchestratorService;
use crate::guards::AuthenticatedUser;
use crate::models::{DeployRequest, UpgradeRequest};
use crate::services::nats_service::NatsMessage;
use crate::storage::build_ipv6_url;
use crate::storage::*;
use crate::AppState;
use rocket::serde::json::Json;
use std::time::Duration;
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

    // ========== IMAGE SOURCE POLICY ==========
    // The agent pulls on the tenant's behalf and can reach every tenant subnet, so an
    // IP-based registry must be inside this tenant's own subnet. Checked before any
    // allocation happens.
    if let Err(e) =
        crate::services::image_policy::validate_image_source(&deploy_req.image, &container_subnet)
    {
        warn!(
            "🚫 Image policy refused deploy for user {}: {}",
            user.0.id, e
        );
        return Json(serde_json::json!({ "error": e }));
    }
    let registry_auth = resolve_registry_auth(pool.inner(), &user.0.id, &deploy_req.image).await;
    let sandbox = crate::models::SandboxSpec {
        lifecycle: deploy_req.lifecycle.clone(),
        labels: deploy_req.labels.clone(),
    };
    if let Some(bad) = [
        &deploy_req.lifecycle.auto_stop,
        &deploy_req.lifecycle.auto_delete,
        &deploy_req.lifecycle.max_lifetime,
    ]
    .into_iter()
    .flatten()
    .find(|d| crate::models::parse_duration_secs(d).is_none())
    {
        return Json(
            serde_json::json!({"error": format!("invalid lifecycle duration '{bad}' (use 90s, 15m, 2h, 1d or 0)")}),
        );
    }
    if let Some(r) = &deploy_req.lifecycle.restart {
        if crate::models::restart_policy_flag(r).is_none() {
            return Json(
                serde_json::json!({"error": format!("invalid restart policy '{r}' (always, unless-stopped, on-failure, never)")}),
            );
        }
    }

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
                registry_auth.clone(),
                Some(sandbox.clone()),
                None, // event_tx — local deploy, response is synchronous
            )
            .await
        {
            Ok((container_name, _pod_name, container_ip, ports, deployed_ipv6)) => {
                if deploy_req.wait {
                    // `nerdctl run -d` has returned, but the runtime listing exec
                    // resolves through can lag by a moment; make "deployed" mean
                    // "exec works now" here as well.
                    let limit =
                        Duration::from_secs(deploy_req.wait_timeout_s.unwrap_or(120).clamp(1, 600));
                    let deadline = tokio::time::Instant::now() + limit;
                    loop {
                        let seen = app_state
                            .container_manager
                            .list_user_containers(&user.0.wireguard_public_key)
                            .await
                            .map(|l| {
                                l.iter()
                                    .any(|c| c.name == container_name && c.status.starts_with("Up"))
                            })
                            .unwrap_or(false);
                        if seen || tokio::time::Instant::now() >= deadline {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                }
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
                    &sandbox,
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

                // Bridge mode: the container IP is internal to the host; what
                // the caller can actually reach is the published port.
                if app_state.container_manager.is_bridge() {
                    let urls = app_state
                        .container_manager
                        .access_urls(&container_name)
                        .await;
                    response["network_mode"] = serde_json::json!("bridge");
                    response["access_urls"] = serde_json::json!(urls);
                }

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
                registry_auth: registry_auth.clone(),
                sandbox: Some(sandbox.clone()),
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
                        &sandbox,
                    );
                    if let Err(e) = store_container_config(pool.inner(), &user.0.id, &config).await
                    {
                        warn!("Failed to store container config: {}", e);
                    }

                    if deploy_req.wait {
                        // Sandbox contract: don't answer until the agent reported
                        // the container running (or failed). The agent's
                        // ContainerDeploymentResult lands in the DB as the
                        // status; poll that rather than inventing a new channel.
                        let limit = Duration::from_secs(
                            deploy_req.wait_timeout_s.unwrap_or(120).clamp(1, 600),
                        );
                        return match wait_for_deploy(pool.inner(), &container_name, limit).await {
                            DeployOutcome::Running(ip) => Json(serde_json::json!({
                                "status": "deployed",
                                "garage": target_garage,
                                "node": target_node,
                                "container_name": container_name,
                                "container_ip": ip.or(allocated_ip).unwrap_or_default(),
                            })),
                            DeployOutcome::Failed(msg) => Json(serde_json::json!({
                                "error": format!("deploy failed on {target_node}: {msg}"),
                                "container_name": container_name,
                            })),
                            DeployOutcome::TimedOut(status) => Json(serde_json::json!({
                                "error": format!(
                                    "deploy still '{status}' after {}s — check `nordkraft list`",
                                    limit.as_secs()
                                ),
                                "container_name": container_name,
                            })),
                        };
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

/// `?label=key=value` (repeatable) keeps only containers carrying every
/// given user label — how an orchestrator finds "its" sandboxes.
enum DeployOutcome {
    Running(Option<String>),
    Failed(String),
    TimedOut(String),
}

/// Poll the containers row the agent's deployment result updates.
async fn wait_for_deploy(pool: &sqlx::PgPool, name: &str, limit: Duration) -> DeployOutcome {
    let deadline = tokio::time::Instant::now() + limit;
    let mut last = String::from("deploying");
    loop {
        let row: Option<(String, Option<String>)> =
            sqlx::query_as("SELECT status, internal_ip FROM containers WHERE container_name = $1")
                .bind(name)
                .fetch_optional(pool)
                .await
                .ok()
                .flatten();
        if let Some((status, ip)) = row {
            match status.as_str() {
                "running" => return DeployOutcome::Running(ip),
                "error" | "failed" => {
                    let msg: Option<String> = sqlx::query_scalar(
                        "SELECT status_message FROM containers WHERE container_name = $1",
                    )
                    .bind(name)
                    .fetch_optional(pool)
                    .await
                    .ok()
                    .flatten();
                    return DeployOutcome::Failed(msg.unwrap_or(status));
                }
                other => last = other.to_string(),
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return DeployOutcome::TimedOut(last);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[get("/containers?<label>")]
pub async fn list_containers_route(
    label: Vec<String>,
    user: AuthenticatedUser,
    app_state: &rocket::State<AppState>,
    orchestrator: &rocket::State<OrchestratorService>,
    pool: &rocket::State<sqlx::PgPool>,
) -> Json<serde_json::Value> {
    // Controller: aggregate from all nodes
    if let Some(nats) = &orchestrator.nats_service {
        if nats.is_controller() {
            let mut containers = orchestrator
                .query_all_nodes_for_containers(&user.0.wireguard_public_key)
                .await;

            // Merge deploying/failed containers from DB that aren't in agent response
            // (they don't exist in nerdctl runtime, only in our DB)
            let running_names: std::collections::HashSet<String> =
                containers.iter().map(|c| c.name.clone()).collect();

            if let Ok(db_containers) = get_non_running_containers(pool.inner(), &user.0.id).await {
                for db_c in db_containers {
                    if !running_names.contains(&db_c.name) {
                        containers.push(db_c);
                    }
                }
            }

            return Json(serde_json::json!({
                "containers": filter_by_labels(containers, &label),
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
            "containers": filter_by_labels(containers, &label),
            "source": "local"
        })),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

fn filter_by_labels(
    containers: Vec<crate::models::ContainerInfo>,
    wanted: &[String],
) -> Vec<crate::models::ContainerInfo> {
    if wanted.is_empty() {
        return containers;
    }
    let wanted: Vec<(&str, &str)> = wanted
        .iter()
        .map(|kv| kv.split_once('=').unwrap_or((kv.as_str(), "")))
        .collect();
    containers
        .into_iter()
        .filter(|c| {
            wanted
                .iter()
                .all(|(k, v)| c.labels.get(*k).map(String::as_str) == Some(*v))
        })
        .collect()
}

#[delete("/containers/<container_id>?<force>")]
pub async fn delete_container(
    container_id: String,
    force: Option<bool>,
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
                force.unwrap_or(false),
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

/// `?wait=true` blocks until the runtime reports the container Up (so an
/// exec issued right after works), `wait_timeout_s` bounds it (default 60,
/// max 600). Without it, start is fire-and-forget — same contract as deploy.
#[post("/containers/<container_id>/start?<wait>&<wait_timeout_s>")]
pub async fn start_container(
    container_id: String,
    wait: Option<String>,
    wait_timeout_s: Option<u64>,
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
            Ok(_) => {
                if wait_flag(&wait) {
                    let up = wait_until_up(app_state, orchestrator, &user, &container_id, wait_limit(wait_timeout_s)).await;
                    return Json(serde_json::json!({"status": if up { "running" } else { "timeout" }}));
                }
                Json(serde_json::json!({"status": "started"}))
            }
            Err(e) => Json(serde_json::json!({"error": e.to_string()})),
        }
    } else if let Some(nats) = &orchestrator.nats_service {
        // Remote
        let target_node = find_container_node(pool.inner(), &container_id).await.ok();
        let msg = NatsMessage::ContainerStart {
            container_id: container_id.clone(),
            owner_pubkey: user.0.wireguard_public_key.clone(),
        };

        let initiated = if let Some(node_id) = target_node {
            let _ = nats.send_to_node(&node_id, msg).await;
            serde_json::json!({"status": "start_initiated", "node": node_id})
        } else {
            for node in orchestrator.get_nodes().await {
                let _ = nats.send_to_node(&node.id, msg.clone()).await;
            }
            serde_json::json!({"status": "start_broadcast"})
        };
        if wait_flag(&wait) {
            let up = wait_until_up(app_state, orchestrator, &user, &container_id, wait_limit(wait_timeout_s)).await;
            let mut v = initiated;
            v["status"] = serde_json::json!(if up { "running" } else { "timeout" });
            return Json(v);
        }
        Json(initiated)
    } else {
        Json(serde_json::json!({"error": "NATS required"}))
    }
}

fn wait_flag(wait: &Option<String>) -> bool {
    matches!(
        wait.as_deref().map(|s| s.to_ascii_lowercase()).as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("")
    )
}

fn wait_limit(secs: Option<u64>) -> Duration {
    Duration::from_secs(secs.unwrap_or(60).clamp(1, 600))
}

/// Poll the runtime (local list, or every node over NATS on a controller)
/// until `name` reports `Up …`. True if it did before `limit`.
async fn wait_until_up(
    app_state: &rocket::State<AppState>,
    orchestrator: &rocket::State<OrchestratorService>,
    user: &AuthenticatedUser,
    name: &str,
    limit: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + limit;
    let is_controller = orchestrator
        .nats_service
        .as_ref()
        .map(|n| n.is_controller())
        .unwrap_or(false);
    loop {
        let list = if is_controller {
            orchestrator
                .query_all_nodes_for_containers(&user.0.wireguard_public_key)
                .await
        } else {
            app_state
                .container_manager
                .list_user_containers(&user.0.wireguard_public_key)
                .await
                .unwrap_or_default()
        };
        if list
            .iter()
            .any(|c| (c.name == name || c.container_id == name) && c.status.starts_with("Up"))
        {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
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

// ============= STORED CONFIG =============

/// GET /containers/<name>/config — the config this container was deployed with.
///
/// `nordkraft init` otherwise rebuilds a spec from `inspect`, which cannot see
/// every field (volume_size is never exposed by the runtime at all) and fills in
/// defaults for the ones it misses. This returns what was actually requested at
/// deploy time, straight from container_config, so a lost .nk can be restored
/// exactly rather than approximately.
#[get("/containers/<container_name>/config")]
pub async fn get_container_config_route(
    container_name: String,
    user: AuthenticatedUser,
    pool: &rocket::State<sqlx::PgPool>,
) -> Json<serde_json::Value> {
    let config = match get_container_config(pool.inner(), &container_name, &user.0.id).await {
        Ok(Some(config)) => config,
        Ok(None) => {
            return Json(serde_json::json!({
                "error": format!(
                    "No stored config for '{}'. It was deployed before config tracking, \
                     so only 'nordkraft init {}' can rebuild its spec.",
                    container_name, container_name
                )
            }));
        }
        Err(e) => {
            warn!("stored config lookup failed for {}: {}", container_name, e);
            return Json(serde_json::json!({
                "error": format!("Container '{}' not found or access denied", container_name)
            }));
        }
    };

    let revision = get_container_config_revision(pool.inner(), &container_name)
        .await
        .unwrap_or(0);

    Json(serde_json::json!({ "config": config, "revision": revision }))
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

    // 6b. Image source policy — before the container is stopped or removed.
    if let Err(e) =
        crate::services::image_policy::validate_image_source(&merged.image, &container_subnet)
    {
        warn!(
            "🚫 Image policy refused upgrade for user {}: {}",
            user.0.id, e
        );
        return Json(serde_json::json!({ "error": e }));
    }
    let registry_auth = resolve_registry_auth(pool.inner(), &user.0.id, &merged.image).await;

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
            registry_auth,
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
                registry_auth: registry_auth.clone(),
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

/// Credentials for the tenant's own private registry, when the image points at one.
/// Any other registry (public hostname, or an address the policy rejected) gets None.
async fn resolve_registry_auth(
    pool: &sqlx::PgPool,
    user_id: &str,
    image: &str,
) -> Option<crate::models::RegistryAuth> {
    let host = crate::services::image_policy::image_host(image)?;
    match find_registry_auth(pool, user_id, host).await {
        Ok(auth) => auth,
        Err(e) => {
            warn!("Registry credential lookup failed for {}: {}", host, e);
            None
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
    registry_auth: Option<crate::models::RegistryAuth>,
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
            registry_auth,
            Some(crate::models::SandboxSpec {
                lifecycle: merged.lifecycle.clone(),
                labels: merged.labels.clone(),
            }),
            None, // event_tx — local upgrade, response is synchronous
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
