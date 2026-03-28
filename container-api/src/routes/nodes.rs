// routes/nodes.rs - Node management routes
use crate::controller::OrchestratorService;
use crate::models::NodeInfo;
use rocket::serde::json::Json;

#[get("/nodes")]
pub async fn list_nodes(
    orchestrator: &rocket::State<OrchestratorService>,
) -> Json<serde_json::Value> {
    let nodes = orchestrator.get_nodes().await;
    Json(serde_json::json!({"nodes": nodes}))
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
