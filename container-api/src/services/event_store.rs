// src/services/event_store.rs
//
// Deploy lifecycle event storage backed by PostgreSQL.
// Controller subscribes to NATS `nordkraft.events.deploy.>` wildcard
// and persists events to the `deploy_events` table.
// CLI reads via `GET /api/events`.

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

// ============= TYPES =============

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployEvent {
    pub id: Option<i64>,
    pub container_name: String,
    pub user_id: String,
    pub node_id: String,
    pub phase: String,
    pub message: String,
    pub success: bool,
    pub created_at: Option<String>,
}

// ============= EVENT STORE =============

#[derive(Clone)]
pub struct EventStore {
    pool: PgPool,
}

impl EventStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Insert a deploy event
    pub async fn insert_event(
        &self,
        user_id: &str,
        event: &DeployEvent,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        sqlx::query(
            "INSERT INTO deploy_events (user_id, container_name, node_id, phase, message, success)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(user_id)
        .bind(&event.container_name)
        .bind(&event.node_id)
        .bind(&event.phase)
        .bind(&event.message)
        .bind(event.success)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Query events for a user, optionally filtered by container name
    pub async fn query_events(
        &self,
        user_id: &str,
        container_name: Option<&str>,
        limit: usize,
    ) -> Result<Vec<DeployEvent>, Box<dyn std::error::Error + Send + Sync>> {
        let events = if let Some(name) = container_name {
            sqlx::query_as::<_, DeployEventRow>(
                "SELECT id, container_name, user_id, node_id, phase, message, success, 
                        created_at::TEXT as created_at
                 FROM deploy_events
                 WHERE user_id = $1 AND container_name = $2
                 ORDER BY id DESC
                 LIMIT $3",
            )
            .bind(user_id)
            .bind(name)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, DeployEventRow>(
                "SELECT id, container_name, user_id, node_id, phase, message, success,
                        created_at::TEXT as created_at
                 FROM deploy_events
                 WHERE user_id = $1
                 ORDER BY id DESC
                 LIMIT $2",
            )
            .bind(user_id)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?
        };

        Ok(events.into_iter().map(|r| r.into()).collect())
    }
}

#[derive(sqlx::FromRow)]
struct DeployEventRow {
    id: i64,
    container_name: String,
    user_id: String,
    node_id: String,
    phase: String,
    message: String,
    success: bool,
    created_at: Option<String>,
}

impl From<DeployEventRow> for DeployEvent {
    fn from(row: DeployEventRow) -> Self {
        DeployEvent {
            id: Some(row.id),
            container_name: row.container_name,
            user_id: row.user_id,
            node_id: row.node_id,
            phase: row.phase,
            message: row.message,
            success: row.success,
            created_at: row.created_at,
        }
    }
}

// ============= NATS LISTENER =============

/// Start a background task that subscribes to all deploy events
/// via NATS wildcard `nordkraft.events.deploy.>` and writes them to PostgreSQL.
pub async fn start_event_collector(
    nats_client: async_nats::Client,
    event_store: Arc<EventStore>,
) {
    use futures::StreamExt;

    tokio::spawn(async move {
        let subject = "nordkraft.events.deploy.>";

        let mut subscriber = match nats_client.subscribe(subject.to_string()).await {
            Ok(s) => {
                info!("📋 Event collector subscribed to: {}", subject);
                s
            }
            Err(e) => {
                error!("❌ Event collector failed to subscribe: {}", e);
                return;
            }
        };

        while let Some(msg) = subscriber.next().await {
            // Extract user_id from subject: nordkraft.events.deploy.{user_id}
            let parts: Vec<&str> = msg.subject.as_str().split('.').collect();
            let user_id = if parts.len() >= 4 {
                parts[3..].join(".")
            } else {
                warn!("Event with unexpected subject: {}", msg.subject);
                continue;
            };

            // Parse the DeployEvent from NATS message
            let nats_msg: serde_json::Value = match serde_json::from_slice(&msg.payload) {
                Ok(v) => v,
                Err(e) => {
                    warn!("Failed to parse deploy event: {}", e);
                    continue;
                }
            };

            let event = DeployEvent {
                id: None,
                container_name: nats_msg["container_name"]
                    .as_str()
                    .unwrap_or("")
                    .to_string(),
                user_id: user_id.clone(),
                node_id: nats_msg["node_id"].as_str().unwrap_or("").to_string(),
                phase: nats_msg["phase"].as_str().unwrap_or("").to_string(),
                message: nats_msg["message"].as_str().unwrap_or("").to_string(),
                success: nats_msg["success"].as_bool().unwrap_or(true),
                created_at: None,
            };

            debug!(
                "📋 Event: [{}] {} → {} ({})",
                user_id, event.container_name, event.phase, event.message
            );

            if let Err(e) = event_store.insert_event(&user_id, &event).await {
                error!("Failed to store event for {}: {}", user_id, e);
            }
        }

        warn!("📋 Event collector subscription ended");
    });
}
