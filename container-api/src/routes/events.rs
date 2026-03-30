// routes/events.rs — Deploy event stream endpoint
//
// GET /api/events?container=NAME&limit=50
//
// Returns deploy lifecycle events for the authenticated user.
// Events are stored per-user in SQLite by the controller's event collector.

use crate::guards::AuthenticatedUser;
use crate::services::event_store::EventStore;
use rocket::serde::json::Json;
use std::sync::Arc;

#[get("/events?<container>&<limit>")]
pub async fn get_events(
    container: Option<String>,
    limit: Option<usize>,
    user: AuthenticatedUser,
    event_store: &rocket::State<Arc<EventStore>>,
) -> Json<serde_json::Value> {
    let limit = limit.unwrap_or(50).min(200);
    let user_id = &user.0.wireguard_public_key;

    match event_store.query_events(user_id, container.as_deref(), limit) {
        Ok(events) => Json(serde_json::json!({
            "events": events,
            "count": events.len(),
        })),
        Err(e) => Json(serde_json::json!({
            "error": format!("Failed to query events: {}", e),
            "events": [],
        })),
    }
}
