// routes/nodes.rs - Node management routes
use crate::controller::OrchestratorService;
use crate::models::NodeInfo;
use crate::services::energy::collector::EnergyCollector;
use chrono::{DateTime, Duration, Utc};
use rocket::serde::json::Json;
use sqlx::Row;
use std::collections::HashMap;
use std::sync::Arc;

/// Managed state: the energy collector, when this process runs one. Its
/// per-node "last CPU sample" is the controller's one steady live signal.
pub struct EnergyLiveness(pub Option<Arc<EnergyCollector>>);

/// A heartbeat or CPU sample this recent means the node is online.
const ONLINE_WITHIN: i64 = 180;

/// The garage's nodes: the `nodes` table is the list (what is registered),
/// liveness comes from the agents (a NATS heartbeat or a CPU sample in the
/// last 3 minutes). The in-memory map alone is not enough: it empties on a
/// controller restart until the agents re-register. No addresses: a
/// customer's view of the garage is its nodes and their state.
#[get("/nodes")]
pub async fn list_nodes(
    orchestrator: &rocket::State<OrchestratorService>,
    pool: &rocket::State<sqlx::PgPool>,
    liveness: &rocket::State<EnergyLiveness>,
) -> Json<serde_json::Value> {
    let in_memory: HashMap<String, NodeInfo> = orchestrator
        .get_nodes()
        .await
        .into_iter()
        .map(|n| (n.id.clone(), n))
        .collect();
    let (cpu_seen, meter_seen) = match &liveness.0 {
        Some(c) => (c.nodes_seen().await, c.meters_seen().await),
        None => (HashMap::new(), HashMap::new()),
    };
    let rows = sqlx::query(
        "SELECT node_id, name, location, garage_id, price_area
         FROM nodes WHERE status = 'active' ORDER BY node_id",
    )
    .fetch_all(pool.inner())
    .await;
    let rows = match rows {
        Ok(r) => r,
        Err(_) => {
            // No database (agent mode): what this process knows.
            let nodes: Vec<NodeInfo> = in_memory.into_values().collect();
            return Json(serde_json::json!({ "nodes": nodes }));
        }
    };
    let now = Utc::now();
    let fresh =
        |t: Option<&DateTime<Utc>>| t.is_some_and(|t| now - *t < Duration::seconds(ONLINE_WITHIN));
    let nodes: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let id: String = r.get("node_id");
            let heartbeat = in_memory.get(&id).map(|n| n.last_heartbeat);
            let cpu = cpu_seen.get(&id).copied();
            let last_seen = [heartbeat, cpu].into_iter().flatten().max();
            let online = fresh(heartbeat.as_ref()) || fresh(cpu.as_ref());
            serde_json::json!({
                "id": id,
                "name": r.get::<Option<String>, _>("name"),
                "location": r.get::<Option<String>, _>("location"),
                "garage": r.get::<Option<String>, _>("garage_id"),
                "price_area": r.get::<Option<String>, _>("price_area"),
                "status": if online { "online" } else { "offline" },
                "last_seen": last_seen,
                "meter_seen": meter_seen.get(&id),
            })
        })
        .collect();
    Json(serde_json::json!({ "nodes": nodes }))
}

#[post("/nodes/register", data = "<node_info>")]
pub async fn register_node(
    node_info: Json<NodeInfo>,
    orchestrator: &rocket::State<OrchestratorService>,
) -> Json<serde_json::Value> {
    match orchestrator.register_node(node_info.into_inner()).await {
        Ok(_) => Json(serde_json::json!({"status": "registered"})),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}
