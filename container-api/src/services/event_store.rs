// src/services/event_store.rs
//
// Per-user SQLite event store for deploy lifecycle events.
// Controller subscribes to NATS `nordkraft.events.deploy.>` wildcard
// and persists events to `/var/lib/nordkraft/events/{user_id}.db`.
// CLI reads via `GET /api/events`.

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

// ============= TYPES =============

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployEvent {
    pub id: Option<i64>,
    pub container_name: String,
    pub node_id: String,
    pub phase: String,
    pub message: String,
    pub success: bool,
    pub timestamp: String,
}

// ============= EVENT STORE =============

pub struct EventStore {
    base_dir: PathBuf,
}

impl EventStore {
    pub fn new(base_dir: &str) -> Self {
        Self {
            base_dir: PathBuf::from(base_dir),
        }
    }

    /// Get or create SQLite DB for a user
    fn db_path(&self, user_id: &str) -> PathBuf {
        self.base_dir.join(format!("{}.db", sanitize_id(user_id)))
    }

    fn open_db(&self, user_id: &str) -> Result<Connection, rusqlite::Error> {
        let path = self.db_path(user_id);
        let conn = Connection::open(&path)?;

        // WAL mode for concurrent reads
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS deploy_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                container_name TEXT NOT NULL,
                node_id TEXT NOT NULL,
                phase TEXT NOT NULL,
                message TEXT NOT NULL,
                success INTEGER NOT NULL DEFAULT 1,
                timestamp TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE INDEX IF NOT EXISTS idx_events_container 
                ON deploy_events(container_name, timestamp DESC);
            CREATE INDEX IF NOT EXISTS idx_events_timestamp 
                ON deploy_events(timestamp DESC);",
        )?;

        Ok(conn)
    }

    /// Insert a deploy event
    pub fn insert_event(
        &self,
        user_id: &str,
        event: &DeployEvent,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let conn = self.open_db(user_id)?;

        conn.execute(
            "INSERT INTO deploy_events (container_name, node_id, phase, message, success, timestamp)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                event.container_name,
                event.node_id,
                event.phase,
                event.message,
                event.success as i32,
                event.timestamp,
            ],
        )?;

        // Auto-prune: keep last 500 events per user
        conn.execute(
            "DELETE FROM deploy_events WHERE id NOT IN (
                SELECT id FROM deploy_events ORDER BY id DESC LIMIT 500
            )",
            [],
        )?;

        Ok(())
    }

    /// Query events for a user, optionally filtered by container name
    pub fn query_events(
        &self,
        user_id: &str,
        container_name: Option<&str>,
        limit: usize,
    ) -> Result<Vec<DeployEvent>, Box<dyn std::error::Error + Send + Sync>> {
        let path = self.db_path(user_id);
        if !path.exists() {
            return Ok(Vec::new());
        }

        let conn = self.open_db(user_id)?;

        let events = if let Some(name) = container_name {
            let mut stmt = conn.prepare(
                "SELECT id, container_name, node_id, phase, message, success, timestamp
                 FROM deploy_events
                 WHERE container_name = ?1
                 ORDER BY id DESC
                 LIMIT ?2",
            )?;

            let rows = stmt
                .query_map(params![name, limit as i64], row_to_event)?
                .filter_map(|r| r.ok())
                .collect::<Vec<_>>();
            rows
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, container_name, node_id, phase, message, success, timestamp
                 FROM deploy_events
                 ORDER BY id DESC
                 LIMIT ?1",
            )?;

            let rows = stmt
                .query_map(params![limit as i64], row_to_event)?
                .filter_map(|r| r.ok())
                .collect::<Vec<_>>();
            rows
        };

        Ok(events)
    }
}

fn row_to_event(row: &rusqlite::Row) -> Result<DeployEvent, rusqlite::Error> {
    Ok(DeployEvent {
        id: Some(row.get(0)?),
        container_name: row.get(1)?,
        node_id: row.get(2)?,
        phase: row.get(3)?,
        message: row.get(4)?,
        success: row.get::<_, i32>(5)? != 0,
        timestamp: row.get(6)?,
    })
}

/// Sanitize user ID for use as filename (WireGuard pubkeys contain / and +)
fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => c,
            _ => '_',
        })
        .collect()
}

// ============= NATS LISTENER =============

/// Start a background task that subscribes to all deploy events
/// via NATS wildcard `nordkraft.events.deploy.>` and writes them to SQLite.
pub async fn start_event_collector(nats_client: async_nats::Client, event_store: Arc<EventStore>) {
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
                node_id: nats_msg["node_id"].as_str().unwrap_or("").to_string(),
                phase: nats_msg["phase"].as_str().unwrap_or("").to_string(),
                message: nats_msg["message"].as_str().unwrap_or("").to_string(),
                success: nats_msg["success"].as_bool().unwrap_or(true),
                timestamp: nats_msg["timestamp"].as_str().unwrap_or("").to_string(),
            };

            debug!(
                "📋 Event: [{}] {} → {} ({})",
                user_id, event.container_name, event.phase, event.message
            );

            if let Err(e) = event_store.insert_event(&user_id, &event) {
                error!("Failed to store event for {}: {}", user_id, e);
            }
        }

        warn!("📋 Event collector subscription ended");
    });
}
