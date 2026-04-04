// src/services/nats_service.rs
// COMPLETE - With IPv6 query/response support
use async_nats::{Client, Message};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{interval, Duration};
use tracing::{debug, error, info, warn};

use crate::{ContainerConfig, ContainerInfo, NodeInfo, PortSpec};

// ============= MESSAGE TYPES =============

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum NatsMessage {
    // Node Management
    NodeRegister {
        node: NodeInfo,
        timestamp: DateTime<Utc>,
    },
    NodeHeartbeat {
        node_id: String,
        status: String,
        timestamp: DateTime<Utc>,
        metrics: Option<NodeMetrics>,
    },
    NodeStatus {
        node_id: String,
        status: String,
        timestamp: DateTime<Utc>,
    },

    // Job Management
    JobSchedule {
        target_node: Option<String>,
        timestamp: DateTime<Utc>,
    },
    JobExecute {
        timestamp: DateTime<Utc>,
    },
    JobResult {
        job_id: String,
        node_id: String,
        status: String,
        result: Option<String>,
        error: Option<String>,
        timestamp: DateTime<Utc>,
    },
    JobCancel {
        job_id: String,
        reason: String,
        timestamp: DateTime<Utc>,
    },

    // Cluster Management
    ClusterState {
        nodes: Vec<NodeInfo>,
        timestamp: DateTime<Utc>,
    },
    NodeQuery {
        query_id: String,
        timestamp: DateTime<Utc>,
    },
    NodeQueryResponse {
        query_id: String,
        node: NodeInfo,
        timestamp: DateTime<Utc>,
    },

    // Container Assignment - with IPv6 and volume_path
    ContainerAssignment {
        job_id: String,
        container_name: String,
        owner_pubkey: String,
        tenant_id: String,
        image: String,
        allocated_ip: Option<String>,
        subnet: String,
        ports: Option<Vec<PortSpec>>,
        command: Option<Vec<String>>,
        env_vars: Option<HashMap<String, String>>,
        cpu_limit: Option<f32>,
        memory_limit: Option<String>,
        user_slot: Option<i32>,
        persistence_enabled: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        volume_path: Option<String>,
        #[serde(default)]
        enable_ipv6: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        ipv6_address: Option<String>,
    },

    // Container deletion
    ContainerDelete {
        container_id: String,
        container_name: String,
        owner_pubkey: String,
        user_slot: Option<i32>,
    },

    ContainerDeleted {
        container_name: String,
        node_id: String,
    },

    // Container Deployment Result - with IPv6
    ContainerDeploymentResult {
        job_id: String,
        container_name: String,
        node_id: String,
        success: bool,
        container_ip: Option<String>,
        error: Option<String>,
        timestamp: DateTime<Utc>,
        #[serde(skip_serializing_if = "Option::is_none")]
        ipv6_address: Option<String>,
    },

    ContainerUpgradeResult {
        container_name: String,
        node_id: String,
        success: bool,
        container_ip: Option<String>,
        error: Option<String>,
        timestamp: DateTime<Utc>,
    },

    // Real-time container query
    ContainerQuery {
        query_id: String,
        owner_pubkey: String,
        timestamp: DateTime<Utc>,
    },

    ContainerQueryResponse {
        query_id: String,
        node_id: String,
        containers: Vec<ContainerInfo>,
        timestamp: DateTime<Utc>,
    },

    // Container lifecycle (remote control)
    ContainerStart {
        container_id: String,
        owner_pubkey: String,
    },
    ContainerStop {
        container_id: String,
        owner_pubkey: String,
    },
    ContainerUpgrade {
        container_name: String,
        owner_pubkey: String,
        tenant_id: String,
        subnet: String,
        user_slot: Option<i32>,
        config: ContainerConfig,
        container_ip: String,
    },

    // Logs (request/response)
    ContainerLogsRequest {
        query_id: String,
        container_id: String,
        owner_pubkey: String,
        lines: Option<usize>,
        timestamp: DateTime<Utc>,
    },
    ContainerLogsResponse {
        query_id: String,
        node_id: String,
        container_id: String,
        success: bool,
        logs: Option<String>,
        error: Option<String>,
        timestamp: DateTime<Utc>,
    },

    // Container inspect - rich detail from agent's nerdctl inspect
    ContainerInspectRequest {
        query_id: String,
        container_id: String,
        owner_pubkey: String,
        timestamp: DateTime<Utc>,
    },
    ContainerInspectResponse {
        query_id: String,
        node_id: String,
        container_id: String,
        success: bool,
        data: Option<Box<ContainerInspectData>>,
        error: Option<String>,
        timestamp: DateTime<Utc>,
    },

    // NEW: IPv6 Query/Response Pattern
    // Controller asks agent: "What's the IPv6 for this container?"
    ContainerIPv6Query {
        query_id: String,
        container_id: String,
        user_id: String,
        timestamp: DateTime<Utc>,
    },

    // Deploy lifecycle events — agent publishes per-user events
    // so CLI can stream real-time feedback on deploy progress
    DeployEvent {
        container_name: String,
        user_id: String,
        node_id: String,
        phase: String, // "pulling", "pulled", "creating", "starting", "running", "failed"
        message: String,
        success: bool,
        timestamp: DateTime<Utc>,
    },

    // Agent responds with actual IPv6 from container
    ContainerIPv6Response {
        query_id: String,
        node_id: String,
        container_id: String,
        ipv6_address: Option<String>,
        exposed_ports: Vec<u16>,
        success: bool,
        error: Option<String>,
        timestamp: DateTime<Utc>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeMetrics {
    pub cpu_usage: f64,
    pub memory_usage: f64,
    pub container_count: u32,
    pub disk_usage: f64,
}

/// Rich container data from `nerdctl inspect` on the agent
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerInspectData {
    pub container_id: String,
    pub name: String,
    pub image: String,
    pub image_digest: Option<String>,
    pub status: String,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub exit_code: Option<i64>,
    pub restart_count: Option<i64>,
    pub container_ip: Option<String>,
    pub ipv6_address: Option<String>,
    pub ipv6_enabled: bool,
    pub ports: Vec<serde_json::Value>,
    pub env_vars: Vec<String>,
    pub command: Vec<String>,
    pub hostname: Option<String>,
    pub node_id: String,
    pub runtime: String,           // e.g. "io.containerd.kata.v2" or "runc"
    pub cpu_limit: Option<f64>,    // nanoCPUs → cores
    pub memory_limit: Option<i64>, // bytes
    pub persistence_enabled: bool,
    pub volume_mounts: Vec<String>,
    pub labels: std::collections::HashMap<String, String>,
}

// ============= NATS SUBJECTS =============

pub struct NatsSubjects;

impl NatsSubjects {
    // Controller subjects (controller publishes, agents subscribe)
    pub const CLUSTER_STATE: &'static str = "nordkraft.cluster.state";
    pub const NODE_QUERY: &'static str = "nordkraft.nodes.query";

    // Agent subjects (agents publish, controller subscribes)
    pub const NODE_REGISTER: &'static str = "nordkraft.nodes.register";
    pub const NODE_HEARTBEAT: &'static str = "nordkraft.nodes.heartbeat";
    pub const NODE_STATUS: &'static str = "nordkraft.nodes.status";
    pub const JOB_RESULT: &'static str = "nordkraft.jobs.result";
    pub const NODE_QUERY_RESPONSE: &'static str = "nordkraft.nodes.query.response";
    pub const CONTAINER_DEPLOYMENT_RESULT: &'static str = "nordkraft.containers.result";
    pub const CONTAINER_DELETED: &'static str = "nordkraft.containers.deleted";
    pub const CONTAINER_UPGRADE_RESULT: &'static str = "nordkraft.containers.upgrade.result";

    // NEW: IPv6 query subjects
    pub const CONTAINER_IPV6_QUERY: &'static str = "nordkraft.ipv6.query";

    // Response subject is query-specific: nordkraft.ipv6.{query_id}.response
    pub fn container_ipv6_response(query_id: &str) -> String {
        format!("nordkraft.ipv6.{}.response", query_id)
    }

    // Node-specific subjects (using node_id)
    pub fn job_execute_for_node(node_id: &str) -> String {
        format!("nordkraft.jobs.execute.{}", node_id)
    }

    pub fn logs_response_for_query(query_id: &str) -> String {
        format!("nordkraft.logs.{}.response", query_id)
    }

    pub fn container_assignment_for_node(node_id: &str) -> String {
        format!("nordkraft.containers.assign.{}", node_id)
    }

    pub fn container_delete_for_node(node_id: &str) -> String {
        format!("nordkraft.containers.delete.{}", node_id)
    }

    pub fn container_query() -> String {
        "nordkraft.containers.query".to_string()
    }

    pub fn container_query_response(query_id: &str) -> String {
        format!("nordkraft.containers.query.{}.response", query_id)
    }

    pub fn container_start_for_node(node_id: &str) -> String {
        format!("nordkraft.containers.start.{}", node_id)
    }

    pub fn container_stop_for_node(node_id: &str) -> String {
        format!("nordkraft.containers.stop.{}", node_id)
    }

    pub fn container_logs_request() -> String {
        "nordkraft.containers.logs.request".to_string()
    }

    pub fn container_logs_response(query_id: &str) -> String {
        format!("nordkraft.containers.logs.{}.response", query_id)
    }

    pub fn container_inspect_response(query_id: &str) -> String {
        format!("nordkraft.containers.inspect.{}.response", query_id)
    }

    pub fn container_inspect_broadcast() -> String {
        "nordkraft.containers.inspect.request".to_string()
    }

    // Per-user deploy event stream: nordkraft.events.deploy.{user_id}
    pub fn deploy_events_for_user(user_id: &str) -> String {
        format!("nordkraft.events.deploy.{}", user_id)
    }
}

// ============= NATS SERVICE =============

#[derive(Clone)]
pub struct NatsService {
    client: Client,
    node_id: String,
    is_controller: bool,
}

impl NatsService {
    pub async fn new(
        nats_url: &str,
        node_id: String,
        is_controller: bool,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        info!("Connecting to NATS server at: {}", nats_url);

        let client = async_nats::connect(nats_url).await?;

        info!("✅ Connected to NATS server");

        Ok(Self {
            client,
            node_id,
            is_controller,
        })
    }

    // ============= PUBLISHING =============

    pub async fn send_to_node(
        &self,
        node_id: &str,
        assignment: NatsMessage,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let subject = NatsSubjects::job_execute_for_node(node_id);
        self.publish_message(subject, &assignment).await
    }

    pub async fn publish_message(
        &self,
        subject: String,
        message: &NatsMessage,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let payload = serde_json::to_vec(message)?;
        self.client.publish(subject.clone(), payload.into()).await?;
        debug!("📤 Published message to {}: {:?}", subject, message);
        Ok(())
    }

    // Node Management
    pub async fn register_node(
        &self,
        node: NodeInfo,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let message = NatsMessage::NodeRegister {
            node,
            timestamp: Utc::now(),
        };
        self.publish_message(NatsSubjects::NODE_REGISTER.to_string(), &message)
            .await
    }

    pub async fn send_heartbeat(
        &self,
        status: String,
        metrics: Option<NodeMetrics>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let message = NatsMessage::NodeHeartbeat {
            node_id: self.node_id.clone(),
            status,
            timestamp: Utc::now(),
            metrics,
        };
        self.publish_message(NatsSubjects::NODE_HEARTBEAT.to_string(), &message)
            .await
    }

    pub async fn update_node_status(
        &self,
        status: String,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let message = NatsMessage::NodeStatus {
            node_id: self.node_id.clone(),
            status,
            timestamp: Utc::now(),
        };
        self.publish_message(NatsSubjects::NODE_STATUS.to_string(), &message)
            .await
    }

    // NEW: IPv6 Query Methods
    pub async fn query_container_ipv6(
        &self,
        container_id: String,
        user_id: String,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let query_id = uuid::Uuid::new_v4().to_string();

        info!(
            "🔍 Querying IPv6 for container: {} (query_id: {})",
            container_id, query_id
        );

        // Subscribe to response BEFORE sending query
        let response_subject = NatsSubjects::container_ipv6_response(&query_id);
        let mut subscriber = self.client.subscribe(response_subject.clone()).await?;

        // Send query to all agents
        let query_message = NatsMessage::ContainerIPv6Query {
            query_id: query_id.clone(),
            container_id: container_id.clone(),
            user_id,
            timestamp: Utc::now(),
        };

        self.publish_message(
            NatsSubjects::CONTAINER_IPV6_QUERY.to_string(),
            &query_message,
        )
        .await?;

        // Wait for response with timeout
        let timeout = tokio::time::sleep(Duration::from_secs(5));
        tokio::pin!(timeout);

        tokio::select! {
            msg = subscriber.next() => {
                if let Some(message) = msg {
                    let response: NatsMessage = serde_json::from_slice(&message.payload)?;

                    if let NatsMessage::ContainerIPv6Response {
                        query_id: resp_query_id,
                        node_id,
                        container_id: resp_container_id,
                        ipv6_address,
                        success,
                        error,
                        ..
                    } = response {
                        if resp_query_id == query_id && resp_container_id == container_id {
                            if success {
                                if let Some(ipv6) = ipv6_address {
                                    info!("✅ Got IPv6 from {}: {}", node_id, ipv6);
                                    return Ok(ipv6);
                                } else {
                                    return Err("Container has no IPv6 address".into());
                                }
                            } else {
                                return Err(format!("Agent error: {}", error.unwrap_or_else(|| "Unknown error".to_string())).into());
                            }
                        }
                    }
                }
                Err("Invalid response received".into())
            }
            _ = &mut timeout => {
                Err(format!("Timeout waiting for IPv6 query response for container {}", container_id).into())
            }
        }
    }

    pub async fn respond_to_ipv6_query(
        &self,
        query_id: String,
        container_id: String,
        ipv6_address: Option<String>,
        exposed_ports: Vec<u16>,
        success: bool,
        error: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let response = NatsMessage::ContainerIPv6Response {
            query_id: query_id.clone(),
            node_id: self.node_id.clone(),
            container_id,
            ipv6_address,
            exposed_ports,
            success,
            error,
            timestamp: Utc::now(),
        };

        let response_subject = NatsSubjects::container_ipv6_response(&query_id);
        self.publish_message(response_subject, &response).await
    }

    // Container Assignment
    #[allow(clippy::too_many_arguments)]
    pub async fn assign_container(
        &self,
        node_id: &str,
        job_id: String,
        container_name: String,
        owner_pubkey: String,
        tenant_id: String,
        image: String,
        allocated_ip: Option<String>,
        subnet: String,
        ports: Option<Vec<PortSpec>>,
        command: Option<Vec<String>>,
        env_vars: Option<HashMap<String, String>>,
        cpu_limit: Option<f32>,
        memory_limit: Option<String>,
        user_slot: Option<i32>,
        persistence_enabled: bool,
        volume_path: Option<String>,
        enable_ipv6: bool,
        ipv6_address: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let message = NatsMessage::ContainerAssignment {
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
            ipv6_address,
        };

        let subject = NatsSubjects::container_assignment_for_node(node_id);
        self.publish_message(subject, &message).await
    }

    pub async fn notify_container_delete(
        &self,
        node_id: &str,
        container_id: String,
        container_name: String,
        owner_pubkey: String,
        user_slot: Option<i32>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let message = NatsMessage::ContainerDelete {
            container_id,
            container_name,
            owner_pubkey,
            user_slot,
        };

        let subject = NatsSubjects::container_delete_for_node(node_id);
        self.publish_message(subject, &message).await
    }

    pub async fn report_container_deleted(
        &self,
        container_name: String,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let message = NatsMessage::ContainerDeleted {
            container_name,
            node_id: self.node_id.clone(),
        };

        self.publish_message(NatsSubjects::CONTAINER_DELETED.to_string(), &message)
            .await
    }

    pub async fn report_deployment_result(
        &self,
        job_id: String,
        container_name: String,
        success: bool,
        container_ip: Option<String>,
        ipv6_address: Option<String>,
        error: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let message = NatsMessage::ContainerDeploymentResult {
            job_id,
            container_name,
            node_id: self.node_id.clone(),
            success,
            container_ip,
            error,
            timestamp: Utc::now(),
            ipv6_address,
        };

        self.publish_message(
            NatsSubjects::CONTAINER_DEPLOYMENT_RESULT.to_string(),
            &message,
        )
        .await
    }

    /// Publish a deploy lifecycle event for a specific user.
    /// Agent calls this at each phase of container deployment so the CLI
    /// can stream real-time progress via `nordkraft events`.
    pub async fn publish_deploy_event(
        &self,
        container_name: &str,
        user_id: &str,
        phase: &str,
        message: &str,
        success: bool,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let event = NatsMessage::DeployEvent {
            container_name: container_name.to_string(),
            user_id: user_id.to_string(),
            node_id: self.node_id.clone(),
            phase: phase.to_string(),
            message: message.to_string(),
            success,
            timestamp: Utc::now(),
        };

        let subject = NatsSubjects::deploy_events_for_user(user_id);
        self.publish_message(subject, &event).await
    }

    // Container Query
    pub async fn query_containers(
        &self,
        owner_pubkey: String,
    ) -> Result<Vec<ContainerInfo>, Box<dyn std::error::Error + Send + Sync>> {
        let query_id = uuid::Uuid::new_v4().to_string();

        info!(
            "🔍 Broadcasting container query for owner: {} (query_id: {})",
            owner_pubkey, query_id
        );

        let response_subject = NatsSubjects::container_query_response(&query_id);
        let mut subscriber = self.client.subscribe(response_subject.clone()).await?;

        let query_message = NatsMessage::ContainerQuery {
            query_id: query_id.clone(),
            owner_pubkey,
            timestamp: Utc::now(),
        };

        self.publish_message(NatsSubjects::container_query(), &query_message)
            .await?;

        let mut all_containers = Vec::new();
        let timeout = tokio::time::sleep(Duration::from_millis(500));
        tokio::pin!(timeout);

        loop {
            tokio::select! {
                msg = subscriber.next() => {
                    if let Some(message) = msg {
                        let response: NatsMessage = serde_json::from_slice(&message.payload)?;

                        if let NatsMessage::ContainerQueryResponse {
                            query_id: resp_query_id,
                            node_id,
                            containers,
                            ..
                        } = response {
                            if resp_query_id == query_id {
                                info!("📦 Got {} containers from node: {}", containers.len(), node_id);
                                all_containers.extend(containers);
                            }
                        }
                    }
                }
                _ = &mut timeout => {
                    info!("⏱️  Query timeout - collected {} total containers", all_containers.len());
                    break;
                }
            }
        }

        Ok(all_containers)
    }

    pub async fn respond_to_container_query(
        &self,
        query_id: String,
        containers: Vec<ContainerInfo>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let response = NatsMessage::ContainerQueryResponse {
            query_id: query_id.clone(),
            node_id: self.node_id.clone(),
            containers,
            timestamp: Utc::now(),
        };

        let response_subject = NatsSubjects::container_query_response(&query_id);
        self.publish_message(response_subject, &response).await
    }

    // Container Lifecycle
    pub async fn send_container_start(
        &self,
        node_id: &str,
        container_id: String,
        owner_pubkey: String,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let message = NatsMessage::ContainerStart {
            container_id,
            owner_pubkey,
        };

        let subject = NatsSubjects::container_start_for_node(node_id);
        self.publish_message(subject, &message).await
    }

    pub async fn send_container_stop(
        &self,
        node_id: &str,
        container_id: String,
        owner_pubkey: String,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let message = NatsMessage::ContainerStop {
            container_id,
            owner_pubkey,
        };

        let subject = NatsSubjects::container_stop_for_node(node_id);
        self.publish_message(subject, &message).await
    }

    // Container Logs
    pub async fn request_container_logs(
        &self,
        container_id: String,
        owner_pubkey: String,
        lines: Option<usize>,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let query_id = uuid::Uuid::new_v4().to_string();

        info!(
            "📋 Requesting logs for container: {} (query_id: {})",
            container_id, query_id
        );

        let response_subject = NatsSubjects::container_logs_response(&query_id);
        let mut subscriber = self.client.subscribe(response_subject.clone()).await?;

        let request = NatsMessage::ContainerLogsRequest {
            query_id: query_id.clone(),
            container_id: container_id.clone(),
            owner_pubkey,
            lines,
            timestamp: Utc::now(),
        };

        self.publish_message(NatsSubjects::container_logs_request(), &request)
            .await?;

        let timeout = tokio::time::sleep(Duration::from_secs(5));
        tokio::pin!(timeout);

        tokio::select! {
            msg = subscriber.next() => {
                if let Some(message) = msg {
                    let response: NatsMessage = serde_json::from_slice(&message.payload)?;

                    if let NatsMessage::ContainerLogsResponse {
                        query_id: resp_query_id,
                        container_id: resp_container_id,
                        success,
                        logs,
                        error,
                        ..
                    } = response {
                        if resp_query_id == query_id && resp_container_id == container_id {
                            if success {
                                return Ok(logs.unwrap_or_default());
                            } else {
                                return Err(format!("Log request failed: {}", error.unwrap_or_else(|| "Unknown error".to_string())).into());
                            }
                        }
                    }
                }
                Err("Invalid response received".into())
            }
            _ = &mut timeout => {
                Err(format!("Timeout waiting for logs from container {}", container_id).into())
            }
        }
    }

    pub async fn respond_to_logs_request(
        &self,
        query_id: String,
        container_id: String,
        success: bool,
        logs: Option<String>,
        error: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let response = NatsMessage::ContainerLogsResponse {
            query_id: query_id.clone(),
            node_id: self.node_id.clone(),
            container_id,
            success,
            logs,
            error,
            timestamp: Utc::now(),
        };

        let response_subject = NatsSubjects::container_logs_response(&query_id);
        self.publish_message(response_subject, &response).await
    }

    // Cluster Management
    pub async fn broadcast_cluster_state(
        &self,
        nodes: Vec<NodeInfo>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let message = NatsMessage::ClusterState {
            nodes,
            timestamp: Utc::now(),
        };
        self.publish_message(NatsSubjects::CLUSTER_STATE.to_string(), &message)
            .await
    }

    pub async fn query_nodes(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let query_id = uuid::Uuid::new_v4().to_string();
        let message = NatsMessage::NodeQuery {
            query_id,
            timestamp: Utc::now(),
        };
        self.publish_message(NatsSubjects::NODE_QUERY.to_string(), &message)
            .await
    }

    // ============= SUBSCRIPTION =============

    pub async fn subscribe_to_messages<F>(
        &self,
        subject: String,
        handler: F,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    where
        F: Fn(NatsMessage) + Send + Sync + 'static,
    {
        let subject_for_log = subject.clone();
        let mut subscriber = self.client.subscribe(subject).await?;
        let handler = Arc::new(handler);

        info!("📥 Subscribed to: {}", subject_for_log);

        tokio::spawn(async move {
            while let Some(message) = subscriber.next().await {
                if let Ok(nats_message) = Self::parse_message(&message) {
                    handler(nats_message);
                } else {
                    warn!("Failed to parse message from {}", subject_for_log);
                }
            }
        });

        Ok(())
    }

    fn parse_message(message: &Message) -> Result<NatsMessage, serde_json::Error> {
        serde_json::from_slice(&message.payload)
    }

    // ============= BACKGROUND TASKS =============

    pub async fn start_controller_subscriptions<F1, F2, F3, F4, F5>(
        &self,
        on_node_register: F1,
        on_node_heartbeat: F2,
        on_node_status: F3,
        on_job_result: F4,
        on_container_deleted: F5,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    where
        F1: Fn(NodeInfo) + Send + Sync + 'static,
        F2: Fn(String, String, DateTime<Utc>, Option<NodeMetrics>) + Send + Sync + 'static,
        F3: Fn(String, String, DateTime<Utc>) + Send + Sync + 'static,
        F4: Fn(String, String, String, Option<String>, Option<String>, DateTime<Utc>)
            + Send
            + Sync
            + 'static,
        F5: Fn(String, String) + Send + Sync + 'static, // container_name, node_id
    {
        if !self.is_controller {
            return Ok(());
        }

        // Subscribe to node registrations
        self.subscribe_to_messages(NatsSubjects::NODE_REGISTER.to_string(), move |msg| {
            if let NatsMessage::NodeRegister { node, .. } = msg {
                on_node_register(node);
            }
        })
        .await?;

        // Subscribe to node heartbeats
        self.subscribe_to_messages(NatsSubjects::NODE_HEARTBEAT.to_string(), move |msg| {
            if let NatsMessage::NodeHeartbeat {
                node_id,
                status,
                timestamp,
                metrics,
            } = msg
            {
                on_node_heartbeat(node_id, status, timestamp, metrics);
            }
        })
        .await?;

        // Subscribe to node status updates
        self.subscribe_to_messages(NatsSubjects::NODE_STATUS.to_string(), move |msg| {
            if let NatsMessage::NodeStatus {
                node_id,
                status,
                timestamp,
            } = msg
            {
                on_node_status(node_id, status, timestamp);
            }
        })
        .await?;

        // Subscribe to job results
        self.subscribe_to_messages(NatsSubjects::JOB_RESULT.to_string(), move |msg| {
            if let NatsMessage::JobResult {
                job_id,
                node_id,
                status,
                result,
                error,
                timestamp,
            } = msg
            {
                on_job_result(job_id, node_id, status, result, error, timestamp);
            }
        })
        .await?;

        // Subscribe to container deletion notifications
        self.subscribe_to_messages(NatsSubjects::CONTAINER_DELETED.to_string(), move |msg| {
            if let NatsMessage::ContainerDeleted {
                container_name,
                node_id,
            } = msg
            {
                on_container_deleted(container_name, node_id);
            }
        })
        .await?;

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn start_agent_subscriptions<F3, F4, F5, F6, F7, F8, F9, F10, F11>(
        &self,
        on_cluster_state: F3,
        on_container_assignment: F4,
        on_container_delete: F5,
        on_container_query: F6,
        on_container_start: F7,
        on_container_stop: F8,
        on_container_logs: F9,
        on_container_inspect: F10,
        on_container_upgrade: F11,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    where
        F3: Fn(Vec<NodeInfo>) + Send + Sync + 'static,
        F4: Fn(NatsMessage) + Send + Sync + 'static,
        F5: Fn(NatsMessage) + Send + Sync + 'static,
        F6: Fn(String, String) + Send + Sync + 'static, // query_id, owner_pubkey
        F7: Fn(String, String) + Send + Sync + 'static, // container_id, owner_pubkey
        F8: Fn(String, String) + Send + Sync + 'static, // container_id, owner_pubkey
        F9: Fn(String, String, String, Option<usize>) + Send + Sync + 'static,
        F10: Fn(String, String, String) + Send + Sync + 'static, // query_id, container_id, owner_pubkey
        F11: Fn(NatsMessage) + Send + Sync + 'static,
    {
        if self.is_controller {
            return Ok(());
        }

        // Subscribe to job executions for this node
        let job_subject = NatsSubjects::job_execute_for_node(&self.node_id);
        let on_container_inspect = std::sync::Arc::new(on_container_inspect);
        let on_container_inspect_clone = std::sync::Arc::clone(&on_container_inspect);
        self.subscribe_to_messages(job_subject, move |msg| match msg {
            NatsMessage::ContainerAssignment { .. } => on_container_assignment(msg),
            NatsMessage::ContainerDelete { .. } => on_container_delete(msg),
            NatsMessage::ContainerStart {
                container_id,
                owner_pubkey,
            } => on_container_start(container_id, owner_pubkey),
            NatsMessage::ContainerStop {
                container_id,
                owner_pubkey,
            } => on_container_stop(container_id, owner_pubkey),
            NatsMessage::ContainerUpgrade { .. } => on_container_upgrade(msg),
            NatsMessage::ContainerLogsRequest {
                query_id,
                container_id,
                owner_pubkey,
                lines,
                ..
            } => on_container_logs(query_id, container_id, owner_pubkey, lines),
            NatsMessage::ContainerInspectRequest {
                query_id,
                container_id,
                owner_pubkey,
                ..
            } => on_container_inspect_clone(query_id, container_id, owner_pubkey),
            _ => {}
        })
        .await?;

        // Subscribe to container queries
        self.subscribe_to_messages("nordkraft.nodes.container.query".to_string(), move |msg| {
            if let NatsMessage::ContainerQuery {
                query_id,
                owner_pubkey,
                ..
            } = msg
            {
                on_container_query(query_id, owner_pubkey);
            }
        })
        .await?;

        // Subscribe to inspect requests (broadcast — all agents listen, only the one
        // that owns the container will respond)
        self.subscribe_to_messages(NatsSubjects::container_inspect_broadcast(), move |msg| {
            if let NatsMessage::ContainerInspectRequest {
                query_id,
                container_id,
                owner_pubkey,
                ..
            } = msg
            {
                on_container_inspect(query_id, container_id, owner_pubkey);
            }
        })
        .await?;

        // Subscribe to cluster state updates
        self.subscribe_to_messages(NatsSubjects::CLUSTER_STATE.to_string(), move |msg| {
            if let NatsMessage::ClusterState { nodes, .. } = msg {
                on_cluster_state(nodes);
            }
        })
        .await?;

        // Subscribe to node queries
        let nats_service = self.clone();
        self.subscribe_to_messages(NatsSubjects::NODE_QUERY.to_string(), move |msg| {
            if let NatsMessage::NodeQuery { query_id, .. } = msg {
                let nats_service = nats_service.clone();
                let query_id = query_id.clone();
                tokio::spawn(async move {
                    let node_info = NodeInfo {
                        id: nats_service.node_id.clone(),
                        address: "127.0.0.1".to_string(),
                        port: 8001,
                        status: "online".to_string(),
                        last_heartbeat: Utc::now(),
                    };

                    let response = NatsMessage::NodeQueryResponse {
                        query_id,
                        node: node_info,
                        timestamp: Utc::now(),
                    };

                    let _ = nats_service
                        .publish_message(NatsSubjects::NODE_QUERY_RESPONSE.to_string(), &response)
                        .await;
                });
            }
        })
        .await?;

        Ok(())
    }

    pub async fn start_heartbeat_task(&self, interval_seconds: u64) {
        if self.is_controller {
            return;
        }

        let nats_service = self.clone();
        tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(interval_seconds));

            loop {
                interval.tick().await;

                let metrics = Some(NodeMetrics {
                    cpu_usage: 0.0,
                    memory_usage: 0.0,
                    container_count: 0,
                    disk_usage: 0.0,
                });

                if let Err(e) = nats_service
                    .send_heartbeat("online".to_string(), metrics)
                    .await
                {
                    error!("Failed to send heartbeat: {}", e);
                }
            }
        });
    }

    pub async fn start_cluster_state_broadcast(
        &self,
        interval_seconds: u64,
        nodes: Arc<RwLock<HashMap<String, NodeInfo>>>,
    ) {
        if !self.is_controller {
            return;
        }

        let nats_service = self.clone();
        tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(interval_seconds));

            loop {
                interval.tick().await;

                let nodes_snapshot = {
                    let nodes_guard = nodes.read().await;
                    nodes_guard.values().cloned().collect::<Vec<_>>()
                };

                if let Err(e) = nats_service.broadcast_cluster_state(nodes_snapshot).await {
                    error!("Failed to broadcast cluster state: {}", e);
                }
            }
        });
    }

    // ============= HELPER FUNCTIONS =============

    pub fn get_client(&self) -> &Client {
        &self.client
    }

    pub fn get_node_id(&self) -> &str {
        &self.node_id
    }

    pub fn is_controller(&self) -> bool {
        self.is_controller
    }
}
