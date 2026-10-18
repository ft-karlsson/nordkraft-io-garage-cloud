// routes/status.rs - Status and health check routes
use crate::guards::AuthenticatedUser;
use crate::storage::get_user_garage_subnet;
use crate::AppState;
use rocket::serde::json::Json;
use sqlx::PgPool;

#[get("/status")]
pub async fn get_status(app_state: &rocket::State<AppState>) -> Json<serde_json::Value> {
    let c = &app_state.config;
    Json(serde_json::json!({
        "status": "online",
        "timestamp": chrono::Utc::now(),
        "version": env!("CARGO_PKG_VERSION"),
        "node_id": c.node_id,
        "mode": format!("{:?}", c.mode).to_lowercase(),
        "dev": c.dev_mode,
        "network_mode": if c.network_mode.is_bridge() { "bridge" } else { "macvlan" },
        "auth": if c.dev_mode { "none (dev mode)" } else { "wireguard peer" },
    }))
}

#[get("/auth/verify")]
pub async fn verify_auth(user: AuthenticatedUser) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "authenticated": true,
        "user": user.0
    }))
}

#[get("/network/info")]
pub async fn get_network_info(
    user: AuthenticatedUser,
    pool: &rocket::State<PgPool>,
) -> Json<serde_json::Value> {
    match get_user_garage_subnet(pool.inner(), &user.0.id, &user.0.primary_garage_id).await {
        Ok(subnet) => Json(serde_json::json!({
            "garage": user.0.primary_garage_id,
            "container_subnet": subnet,
        })),
        Err(e) => Json(serde_json::json!({
            "error": format!("No network allocation: {}", e)
        })),
    }
}
