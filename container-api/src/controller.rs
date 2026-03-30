// controller.rs - REFACTORED
//
// Changes from original:
// 1. Extracted handlers into separate impl blocks
// 2. Separated controller vs agent concerns
// 3. Moved database operations into storage module calls
// 4. Reduced nesting with early returns and helper methods
// 5. Clear section organization

use crate::config::{AppConfig, OperationMode};
use crate::models::{ContainerInfo, NodeInfo};
use crate::services::container_manager::ContainerManager;
use crate::services::nats_service::{NatsMessage, NatsService, NatsSubjects};
use crate::storage;
use chrono::Utc;
use futures::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{interval, Duration};
use tracing::{debug, error, info, warn};

// =============================================================================
// ORCHESTRATOR SERVICE - Core struct
// =============================================================================

#[derive(Clone)]
pub struct OrchestratorService {
    pub mode: OperationMode,
    pub node_id: String,
    pub nodes: Arc<RwLock<HashMap<String, NodeInfo>>>,
    pub nats_service: Option<Arc<NatsService>>,
    container_manager: Arc<ContainerManager>,
    config: AppConfig,
}

impl OrchestratorService {
    pub fn new(
        config: &AppConfig,
        nats_service: Option<Arc<NatsService>>,
        container_manager: Arc<ContainerManager>,
    ) -> Self {
        Self {
            mode: config.mode.clone(),
            node_id: config.node_id.clone(),
            nodes: Arc::new(RwLock::new(HashMap::new())),
            nats_service,
            container_manager,
            config: config.clone(),
        }
    }
}

// =============================================================================
// NODE MANAGEMENT
// =============================================================================

impl OrchestratorService {
    pub async fn register_node(
        &self,
        node: NodeInfo,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!("📝 Registering node: {}", node.id);
        self.nodes
            .write()
            .await
            .insert(node.id.clone(), node.clone());

        // Broadcast updated state if we're controller
        if let Some(nats) = &self.nats_service {
            if nats.is_controller() {
                let nodes_snapshot = self.nodes.read().await.values().cloned().collect();
                let _ = nats.broadcast_cluster_state(nodes_snapshot).await;
            }
        }
        Ok(())
    }

    pub async fn get_nodes(&self) -> Vec<NodeInfo> {
        self.nodes.read().await.values().cloned().collect()
    }

    pub async fn update_node_heartbeat(
        &self,
        node_id: &str,
        status: String,
        timestamp: chrono::DateTime<Utc>,
    ) {
        let mut nodes = self.nodes.write().await;
        if let Some(node) = nodes.get_mut(node_id) {
            node.status = status;
            node.last_heartbeat = timestamp;
        }
    }

    pub async fn update_cluster_state(&self, cluster_nodes: Vec<NodeInfo>) {
        let mut nodes = self.nodes.write().await;
        nodes.clear();
        for node in cluster_nodes {
            nodes.insert(node.id.clone(), node);
        }
        debug!("📊 Updated cluster state");
    }
}

// =============================================================================
// MULTI-NODE QUERIES
// =============================================================================

impl OrchestratorService {
    /// Query all nodes for user's containers
    pub async fn query_all_nodes_for_containers(&self, owner_pubkey: &str) -> Vec<ContainerInfo> {
        let Some(nats) = &self.nats_service else {
            return vec![];
        };

        let query_id = uuid::Uuid::new_v4().to_string();
        let response_subject = format!("nordkraft.query.{}.response", query_id);

        let mut subscriber = match nats.get_client().subscribe(response_subject.clone()).await {
            Ok(sub) => sub,
            Err(e) => {
                error!("Failed to subscribe to query responses: {}", e);
                return vec![];
            }
        };

        // Publish query
        let query_msg = NatsMessage::ContainerQuery {
            query_id: query_id.clone(),
            owner_pubkey: owner_pubkey.to_string(),
            timestamp: Utc::now(),
        };

        if let Err(e) = nats
            .publish_message("nordkraft.nodes.container.query".to_string(), &query_msg)
            .await
        {
            error!("Failed to publish container query: {}", e);
            return vec![];
        }

        // Collect responses with timeout
        self.collect_query_responses(&mut subscriber, Duration::from_millis(500))
            .await
    }

    async fn collect_query_responses(
        &self,
        subscriber: &mut async_nats::Subscriber,
        timeout: Duration,
    ) -> Vec<ContainerInfo> {
        let mut containers = Vec::new();
        let start = tokio::time::Instant::now();

        while start.elapsed() < timeout {
            tokio::select! {
                Some(msg) = subscriber.next() => {
                    if let Ok(NatsMessage::ContainerQueryResponse { containers: node_containers, .. }) =
                        serde_json::from_slice::<NatsMessage>(&msg.payload)
                    {
                        containers.extend(node_containers);
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }
        containers
    }

    /// Request logs from nodes
    pub async fn request_container_logs(
        &self,
        owner_pubkey: &str,
        container_id_or_name: &str,
        lines: Option<usize>,
        preferred_node: Option<String>,
    ) -> Option<String> {
        let nats = self.nats_service.as_ref()?;
        let query_id = uuid::Uuid::new_v4().to_string();
        let response_subject = NatsSubjects::logs_response_for_query(&query_id);

        let mut sub = nats.get_client().subscribe(response_subject).await.ok()?;

        let req = NatsMessage::ContainerLogsRequest {
            query_id: query_id.clone(),
            container_id: container_id_or_name.to_string(),
            owner_pubkey: owner_pubkey.to_string(),
            lines,
            timestamp: Utc::now(),
        };

        // Send to specific node or broadcast
        match preferred_node {
            Some(node_id) => {
                let _ = nats.send_to_node(&node_id, req).await;
            }
            None => {
                for node in self.get_nodes().await {
                    let _ = nats.send_to_node(&node.id, req.clone()).await;
                }
            }
        }

        // Wait for response
        self.wait_for_logs_response(&mut sub, Duration::from_millis(800))
            .await
    }

    async fn wait_for_logs_response(
        &self,
        sub: &mut async_nats::Subscriber,
        timeout: Duration,
    ) -> Option<String> {
        let deadline = tokio::time::Instant::now() + timeout;

        while tokio::time::Instant::now() < deadline {
            tokio::select! {
                maybe_msg = sub.next() => {
                    if let Some(msg) = maybe_msg {
                        if let Ok(NatsMessage::ContainerLogsResponse { success, logs, error, .. }) =
                            serde_json::from_slice::<NatsMessage>(&msg.payload)
                        {
                            if success {
                                return logs;
                            }
                            error!("Logs error: {:?}", error);
                        }
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(30)) => {}
            }
        }
        None
    }

    /// Request rich inspect data from the node that owns the container.
    pub async fn request_container_inspect(
        &self,
        owner_pubkey: &str,
        container_id: &str,
        _preferred_node: Option<String>,
    ) -> Option<crate::services::nats_service::ContainerInspectData> {
        let nats = self.nats_service.as_ref()?;
        let query_id = uuid::Uuid::new_v4().to_string();
        let response_subject = NatsSubjects::container_inspect_response(&query_id);

        let mut sub = nats.get_client().subscribe(response_subject).await.ok()?;

        let req = NatsMessage::ContainerInspectRequest {
            query_id: query_id.clone(),
            container_id: container_id.to_string(),
            owner_pubkey: owner_pubkey.to_string(),
            timestamp: Utc::now(),
        };

        // Broadcast to all agents on dedicated inspect subject — same pattern as
        // query_all_nodes_for_containers. The agent that owns the container responds,
        // the rest do nothing (container not found → no response sent).
        if let Err(e) = nats
            .publish_message(NatsSubjects::container_inspect_broadcast(), &req)
            .await
        {
            error!("Failed to broadcast inspect request: {}", e);
            return None;
        }

        // Wait for first successful response (1.5s timeout)
        let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
        while tokio::time::Instant::now() < deadline {
            tokio::select! {
                maybe_msg = sub.next() => {
                    if let Some(msg) = maybe_msg {
                        if let Ok(NatsMessage::ContainerInspectResponse { success, data, error, .. }) =
                            serde_json::from_slice::<NatsMessage>(&msg.payload)
                        {
                            if success {
                                return data.map(|b| *b);
                            }
                            error!("Inspect error from agent: {:?}", error);
                            return None;
                        }
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(30)) => {}
            }
        }
        None
    }
}

// =============================================================================
// BACKGROUND TASKS - Entry point
// =============================================================================

impl OrchestratorService {
    pub async fn run_background_tasks(&self) {
        if matches!(self.mode, OperationMode::Controller | OperationMode::Hybrid) {
            self.start_controller_tasks().await;
        }
        if matches!(self.mode, OperationMode::Agent | OperationMode::Hybrid) {
            self.start_agent_tasks().await;
        }
    }
}

// =============================================================================
// CONTROLLER TASKS
// =============================================================================

impl OrchestratorService {
    async fn start_controller_tasks(&self) {
        self.spawn_node_cleanup_task();
        self.spawn_controller_subscriptions();
    }

    fn spawn_node_cleanup_task(&self) {
        let nodes = Arc::clone(&self.nodes);
        tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                let cutoff = Utc::now() - chrono::Duration::seconds(300);
                nodes
                    .write()
                    .await
                    .retain(|_, node| node.last_heartbeat > cutoff);
            }
        });
    }

    fn spawn_controller_subscriptions(&self) {
        let Some(nats) = &self.nats_service else {
            return;
        };

        let orchestrator = self.clone();
        let nats_clone = Arc::clone(nats);

        tokio::spawn(async move {
            let _ = nats_clone
                .start_controller_subscriptions(
                    // Handlers are now simple closures that delegate to methods
                    make_node_register_handler(orchestrator.clone()),
                    make_heartbeat_handler(orchestrator.clone()),
                    make_node_status_handler(orchestrator.clone()),
                    make_job_result_handler(),
                    make_container_deleted_handler(),
                )
                .await;

            // Subscribe to deployment results — update container status in DB
            let _ = nats_clone
                .subscribe_to_messages(
                    NatsSubjects::CONTAINER_DEPLOYMENT_RESULT.to_string(),
                    make_deployment_result_handler(),
                )
                .await;

            // Subscribe to upgrade results — same status update
            let _ = nats_clone
                .subscribe_to_messages(
                    NatsSubjects::CONTAINER_UPGRADE_RESULT.to_string(),
                    make_upgrade_result_handler(),
                )
                .await;
        });
    }
}

// =============================================================================
// AGENT TASKS
// =============================================================================

impl OrchestratorService {
    async fn start_agent_tasks(&self) {
        let Some(nats) = &self.nats_service else {
            return;
        };

        self.spawn_agent_registration(nats);
        nats.start_heartbeat_task(30).await;
        self.spawn_agent_subscriptions(nats);
    }

    fn spawn_agent_registration(&self, nats: &Arc<NatsService>) {
        let config = self.config.clone();
        let nats_clone = Arc::clone(nats);

        tokio::spawn(async move {
            let node_info = NodeInfo {
                id: config.node_id.clone(),
                address: config.bind_address.clone(),
                port: config.bind_port,
                status: "online".to_string(),
                last_heartbeat: Utc::now(),
            };
            let _ = nats_clone.register_node(node_info).await;
            info!("✅ Registered with controller via NATS");
        });
    }

    fn spawn_agent_subscriptions(&self, nats: &Arc<NatsService>) {
        let orchestrator = self.clone();
        let nats_clone = Arc::clone(nats);

        tokio::spawn(async move {
            let _ = nats_clone
                .start_agent_subscriptions(
                    make_cluster_state_handler(orchestrator.clone()),
                    make_container_assignment_handler(orchestrator.clone(), nats_clone.clone()),
                    make_container_delete_handler(orchestrator.clone()),
                    make_container_query_handler(orchestrator.clone(), nats_clone.clone()),
                    make_container_start_handler(orchestrator.clone()),
                    make_container_stop_handler(orchestrator.clone()),
                    make_container_logs_handler(orchestrator.clone(), nats_clone.clone()),
                    make_container_inspect_handler(orchestrator.clone(), nats_clone.clone()),
                    make_container_upgrade_handler(orchestrator.clone(), nats_clone.clone()),
                )
                .await;
        });
    }
}

// =============================================================================
// CONTROLLER HANDLERS (extracted for readability)
// =============================================================================

fn make_node_register_handler(
    orchestrator: OrchestratorService,
) -> impl Fn(NodeInfo) + Send + Sync + 'static {
    move |node: NodeInfo| {
        let orchestrator = orchestrator.clone();
        tokio::spawn(async move {
            let _ = orchestrator.register_node(node).await;
        });
    }
}

fn make_heartbeat_handler(
    orchestrator: OrchestratorService,
) -> impl Fn(String, String, chrono::DateTime<Utc>, Option<crate::services::nats_service::NodeMetrics>)
       + Send
       + Sync
       + 'static {
    move |node_id, status, timestamp, _metrics| {
        let orchestrator = orchestrator.clone();
        tokio::spawn(async move {
            orchestrator
                .update_node_heartbeat(&node_id, status, timestamp)
                .await;
        });
    }
}

fn make_node_status_handler(
    orchestrator: OrchestratorService,
) -> impl Fn(String, String, chrono::DateTime<Utc>) + Send + Sync + 'static {
    move |node_id, status, timestamp| {
        let orchestrator = orchestrator.clone();
        tokio::spawn(async move {
            orchestrator
                .update_node_heartbeat(&node_id, status, timestamp)
                .await;
        });
    }
}

fn make_job_result_handler(
) -> impl Fn(String, String, String, Option<String>, Option<String>, chrono::DateTime<Utc>)
       + Send
       + Sync
       + 'static {
    |_job_id, _node_id, _status, _result, _error, _timestamp| {
        // No-op for backward compatibility
    }
}

fn make_container_deleted_handler() -> impl Fn(String, String) + Send + Sync + 'static {
    move |container_name: String, node_id: String| {
        info!(
            "🗑️ Container {} deleted on node {}",
            container_name, node_id
        );
        tokio::spawn(async move {
            if let Err(e) = handle_container_deletion_cleanup(&container_name).await {
                error!("Cleanup failed for {}: {}", container_name, e);
            }
        });
    }
}

/// Handle deployment result from agent — update container status in DB
fn make_deployment_result_handler() -> impl Fn(NatsMessage) + Send + Sync + 'static {
    move |msg: NatsMessage| {
        if let NatsMessage::ContainerDeploymentResult {
            container_name,
            success,
            error,
            ..
        } = msg
        {
            let status = if success { "running" } else { "failed" };
            info!(
                "📋 Deployment result: {} → {}{}",
                container_name,
                status,
                error
                    .as_ref()
                    .map(|e| format!(" ({})", e))
                    .unwrap_or_default()
            );

            tokio::spawn(async move {
                if let Err(e) = update_container_status_from_result(&container_name, status).await {
                    error!(
                        "Failed to update container status for {}: {}",
                        container_name, e
                    );
                }
            });
        }
    }
}

/// Handle upgrade result from agent — same status update
fn make_upgrade_result_handler() -> impl Fn(NatsMessage) + Send + Sync + 'static {
    move |msg: NatsMessage| {
        if let NatsMessage::ContainerUpgradeResult {
            container_name,
            success,
            error,
            ..
        } = msg
        {
            let status = if success { "running" } else { "failed" };
            info!(
                "📋 Upgrade result: {} → {}{}",
                container_name,
                status,
                error
                    .as_ref()
                    .map(|e| format!(" ({})", e))
                    .unwrap_or_default()
            );

            tokio::spawn(async move {
                if let Err(e) = update_container_status_from_result(&container_name, status).await {
                    error!(
                        "Failed to update container status for {}: {}",
                        container_name, e
                    );
                }
            });
        }
    }
}

/// Connect to DB and update container status
async fn update_container_status_from_result(
    container_name: &str,
    status: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let db_url = std::env::var("DATABASE_URL")?;
    let pool = sqlx::PgPool::connect(&db_url).await?;
    storage::update_container_status(&pool, container_name, status).await
}

async fn handle_container_deletion_cleanup(
    container_name: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let db_url = std::env::var("DATABASE_URL")?;
    let pool = sqlx::PgPool::connect(&db_url).await?;

    if let Some(ip) = storage::get_container_ip(&pool, container_name).await? {
        let route_manager = crate::services::route_manager::StaticRouteManager::new();
        route_manager.remove_container_route(&ip).await?;
        info!("✅ Route removed for {} ({})", container_name, ip);
    }

    storage::mark_container_deleted(&pool, container_name).await?;
    info!("🏁 Cleanup completed for {}", container_name);
    Ok(())
}

// =============================================================================
// AGENT HANDLERS
// =============================================================================

fn make_cluster_state_handler(
    orchestrator: OrchestratorService,
) -> impl Fn(Vec<NodeInfo>) + Send + Sync + 'static {
    move |cluster_nodes| {
        let orchestrator = orchestrator.clone();
        tokio::spawn(async move {
            orchestrator.update_cluster_state(cluster_nodes).await;
        });
    }
}

fn make_container_assignment_handler(
    orchestrator: OrchestratorService,
    nats: Arc<NatsService>,
) -> impl Fn(NatsMessage) + Send + Sync + 'static {
    let node_id = orchestrator.node_id.clone();
    let cm = orchestrator.container_manager.clone();

    move |msg: NatsMessage| {
        let NatsMessage::ContainerAssignment {
            job_id,
            container_name,
            owner_pubkey,
            tenant_id,
            image,
            allocated_ip,
            subnet,
            ports,
            command,
            env_vars,
            cpu_limit,
            memory_limit,
            user_slot,
            persistence_enabled,
            volume_path,
            enable_ipv6,
            ipv6_address: _, // Ignored - SLAAC used instead
        } = msg
        else {
            return;
        };

        info!(
            "📥 NATS assignment: slot={:?}, persistence={}, ipv6={}",
            user_slot, persistence_enabled, enable_ipv6
        );

        let cm = cm.clone();
        let nats = nats.clone();
        let node = node_id.clone();

        tokio::spawn(async move {
            // Emit: pulling phase
            let _ = nats
                .publish_deploy_event(
                    &container_name,
                    &owner_pubkey,
                    "pulling",
                    &format!("Pulling image: {}", image),
                    true,
                )
                .await;

            let result = cm
                .deploy_secure_container(
                    &owner_pubkey,
                    &tenant_id,
                    &subnet,
                    &image,
                    ports,
                    command,
                    env_vars,
                    cpu_limit,
                    memory_limit,
                    user_slot,
                    persistence_enabled,
                    volume_path,
                    allocated_ip,
                    Some(container_name.clone()),
                    enable_ipv6,
                )
                .await;

            let msg = match result {
                Ok((name, _, ip, _, deployed_ipv6)) => {
                    info!(
                        "✅ Container {} deployed: IPv4={}, IPv6={:?}",
                        name, ip, deployed_ipv6
                    );

                    // Emit: running phase
                    let _ = nats
                        .publish_deploy_event(
                            &name,
                            &owner_pubkey,
                            "running",
                            &format!("Container running at {}", ip),
                            true,
                        )
                        .await;

                    NatsMessage::ContainerDeploymentResult {
                        job_id,
                        container_name: name,
                        node_id: node,
                        success: true,
                        container_ip: Some(ip),
                        error: None,
                        timestamp: Utc::now(),
                        ipv6_address: deployed_ipv6,
                    }
                }
                Err(e) => {
                    error!("❌ Container deployment failed: {}", e);

                    // Emit: failed phase
                    let _ = nats
                        .publish_deploy_event(
                            &container_name,
                            &owner_pubkey,
                            "failed",
                            &format!("Deploy failed: {}", e),
                            false,
                        )
                        .await;

                    NatsMessage::ContainerDeploymentResult {
                        job_id,
                        container_name,
                        node_id: node,
                        success: false,
                        container_ip: None,
                        error: Some(e.to_string()),
                        timestamp: Utc::now(),
                        ipv6_address: None,
                    }
                }
            };

            let _ = nats
                .publish_message(NatsSubjects::CONTAINER_DEPLOYMENT_RESULT.to_string(), &msg)
                .await;
        });
    }
}

fn make_container_delete_handler(
    orchestrator: OrchestratorService,
) -> impl Fn(NatsMessage) + Send + Sync + 'static {
    let cm = orchestrator.container_manager.clone();
    let nats = orchestrator.nats_service.clone();
    let node = orchestrator.node_id.clone();

    move |msg: NatsMessage| {
        let NatsMessage::ContainerDelete {
            container_id,
            owner_pubkey,
            user_slot,
            ..
        } = msg
        else {
            return;
        };

        let cm = cm.clone();
        let nats = nats.as_ref().map(Arc::clone);
        let node = node.clone();

        tokio::spawn(async move {
            let _ = cm
                .remove_container(
                    &container_id,
                    &owner_pubkey,
                    user_slot,
                    nats.as_ref().map(|n| n.as_ref()),
                    Some(&node),
                )
                .await;
        });
    }
}

fn make_container_query_handler(
    orchestrator: OrchestratorService,
    nats: Arc<NatsService>,
) -> impl Fn(String, String) + Send + Sync + 'static {
    let cm = orchestrator.container_manager.clone();
    let node_id = orchestrator.node_id.clone();

    move |query_id: String, owner_pubkey: String| {
        let cm = cm.clone();
        let nats = nats.clone();
        let node_id = node_id.clone();

        tokio::spawn(async move {
            if let Ok(containers) = cm.list_user_containers(&owner_pubkey).await {
                let response = NatsMessage::ContainerQueryResponse {
                    query_id: query_id.clone(),
                    node_id,
                    containers,
                    timestamp: Utc::now(),
                };
                let _ = nats
                    .publish_message(format!("nordkraft.query.{}.response", query_id), &response)
                    .await;
            }
        });
    }
}

fn make_container_start_handler(
    orchestrator: OrchestratorService,
) -> impl Fn(String, String) + Send + Sync + 'static {
    let cm = orchestrator.container_manager.clone();
    move |container_id, owner_pubkey| {
        let cm = cm.clone();
        tokio::spawn(async move {
            let _ = cm.start_container(&container_id, &owner_pubkey).await;
        });
    }
}

fn make_container_stop_handler(
    orchestrator: OrchestratorService,
) -> impl Fn(String, String) + Send + Sync + 'static {
    let cm = orchestrator.container_manager.clone();
    move |container_id, owner_pubkey| {
        let cm = cm.clone();
        tokio::spawn(async move {
            let _ = cm.stop_container(&container_id, &owner_pubkey).await;
        });
    }
}

fn make_container_upgrade_handler(
    orchestrator: OrchestratorService,
    nats: Arc<NatsService>,
) -> impl Fn(NatsMessage) + Send + Sync + 'static {
    let node_id = orchestrator.node_id.clone();
    let cm = orchestrator.container_manager.clone();

    move |msg: NatsMessage| {
        let NatsMessage::ContainerUpgrade {
            container_name,
            owner_pubkey,
            tenant_id,
            subnet,
            user_slot,
            config,
            container_ip,
        } = msg
        else {
            return;
        };

        info!(
            "📥 NATS upgrade: {} → image={}",
            container_name, config.image
        );

        let cm = cm.clone();
        let nats = nats.clone();
        let node = node_id.clone();

        tokio::spawn(async move {
            // Emit: upgrading phase
            let _ = nats
                .publish_deploy_event(
                    &container_name,
                    &owner_pubkey,
                    "upgrading",
                    &format!("Upgrading to image: {}", config.image),
                    true,
                )
                .await;

            // Step 1: Stop the container
            info!("⏸️  Upgrade: stopping {}", container_name);
            if let Err(e) = cm.stop_container(&container_name, &owner_pubkey).await {
                warn!("Stop failed (may already be stopped): {}", e);
            }

            // Step 2: Remove container shell WITHOUT volume cleanup.
            // We call nerdctl rm directly — NOT remove_container() which deletes volumes.
            info!("🗑️  Upgrade: removing container shell {}", container_name);
            let _ = tokio::process::Command::new("nerdctl")
                .args(["rm", "-f", &container_name])
                .output()
                .await;

            // Emit: pulling phase
            let _ = nats
                .publish_deploy_event(
                    &container_name,
                    &owner_pubkey,
                    "pulling",
                    &format!("Pulling image: {}", config.image),
                    true,
                )
                .await;

            // Step 3: Redeploy with same name + same IP + updated config
            info!(
                "🚀 Upgrade: redeploying {} with image {}",
                container_name, config.image
            );

            let ports_opt = if config.ports.is_empty() {
                None
            } else {
                Some(config.ports.clone())
            };

            let env_opt = if config.env_vars.is_empty() {
                None
            } else {
                Some(config.env_vars.clone())
            };

            let result = cm
                .deploy_secure_container(
                    &owner_pubkey,
                    &tenant_id,
                    &subnet,
                    &config.image,
                    ports_opt,
                    config.command.clone(),
                    env_opt,
                    Some(config.cpu_limit),
                    Some(config.memory_limit.clone()),
                    user_slot,
                    config.enable_persistence,
                    config.volume_path.clone(),
                    Some(container_ip.clone()),
                    Some(container_name.clone()),
                    config.enable_ipv6,
                )
                .await;

            let msg = match result {
                Ok((name, _, ip, _, _)) => {
                    info!("✅ Upgrade complete: {} → {}", name, config.image);

                    // Emit: running phase
                    let _ = nats
                        .publish_deploy_event(
                            &name,
                            &owner_pubkey,
                            "running",
                            &format!("Upgrade complete, running at {}", ip),
                            true,
                        )
                        .await;

                    NatsMessage::ContainerUpgradeResult {
                        container_name: name,
                        node_id: node,
                        success: true,
                        container_ip: Some(ip),
                        error: None,
                        timestamp: Utc::now(),
                    }
                }
                Err(e) => {
                    error!(
                        "❌ Upgrade FAILED for {}: {}. Container is DOWN.",
                        container_name, e
                    );

                    // Emit: failed phase
                    let _ = nats
                        .publish_deploy_event(
                            &container_name,
                            &owner_pubkey,
                            "failed",
                            &format!("Upgrade failed: {}", e),
                            false,
                        )
                        .await;

                    NatsMessage::ContainerUpgradeResult {
                        container_name,
                        node_id: node,
                        success: false,
                        container_ip: None,
                        error: Some(e.to_string()),
                        timestamp: Utc::now(),
                    }
                }
            };

            let _ = nats
                .publish_message(NatsSubjects::CONTAINER_UPGRADE_RESULT.to_string(), &msg)
                .await;
        });
    }
}

fn make_container_logs_handler(
    orchestrator: OrchestratorService,
    nats: Arc<NatsService>,
) -> impl Fn(String, String, String, Option<usize>) + Send + Sync + 'static {
    let cm = orchestrator.container_manager.clone();
    let node_id = orchestrator.node_id.clone();

    move |query_id, container_id, owner_pubkey, lines| {
        let cm = cm.clone();
        let nats = nats.clone();
        let node_id = node_id.clone();

        tokio::spawn(async move {
            let (success, logs, error) = match cm
                .get_container_logs(&container_id, &owner_pubkey, lines)
                .await
            {
                Ok(logs) => (true, Some(logs), None),
                Err(e) => (false, None, Some(e.to_string())),
            };

            let msg = NatsMessage::ContainerLogsResponse {
                query_id: query_id.clone(),
                node_id,
                container_id,
                success,
                logs,
                error,
                timestamp: Utc::now(),
            };

            let _ = nats
                .publish_message(NatsSubjects::logs_response_for_query(&query_id), &msg)
                .await;
        });
    }
}

fn make_container_inspect_handler(
    orchestrator: OrchestratorService,
    nats: Arc<NatsService>,
) -> impl Fn(String, String, String) + Send + Sync + 'static {
    let cm = orchestrator.container_manager.clone();
    let node_id = orchestrator.node_id.clone();

    move |query_id, container_id, owner_pubkey| {
        let cm = cm.clone();
        let nats = nats.clone();
        let node_id = node_id.clone();

        tokio::spawn(async move {
            let (success, data, error) = match cm
                .inspect_container(&container_id, &owner_pubkey, &node_id)
                .await
            {
                Ok(d) => (true, Some(Box::new(d)), None),
                Err(e) => (false, None, Some(e.to_string())),
            };

            let msg = NatsMessage::ContainerInspectResponse {
                query_id: query_id.clone(),
                node_id,
                container_id,
                success,
                data,
                error,
                timestamp: Utc::now(),
            };

            let _ = nats
                .publish_message(NatsSubjects::container_inspect_response(&query_id), &msg)
                .await;
        });
    }
}
