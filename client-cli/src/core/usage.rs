//! Plan usage and deploy lifecycle events. Both come back as the
//! controller's JSON document; the shape is the controller's to define.

use super::aliases::resolve_alias;
use super::api::*;
use super::CoreResult;

/// `GET /usage`: plan name and limits, current usage, and ratios 0..1.
pub(crate) async fn usage() -> CoreResult<serde_json::Value> {
    let client = create_client()?;
    let url = format!("{}/usage", *API_BASE_URL);
    let response = client.get(&url).send().await?;
    if !response.status().is_success() {
        let error: ApiError = response.json().await?;
        return Err(format!("Failed to fetch usage: {}", error.error).into());
    }
    Ok(response.json().await?)
}

/// `GET /events`: the newest `limit` deploy events, optionally for one
/// container. Newest first.
pub(crate) async fn events(container: Option<&str>, limit: usize) -> CoreResult<serde_json::Value> {
    let client = create_client()?;
    let mut url = format!("{}/events?limit={}", *API_BASE_URL, limit);
    if let Some(name) = container {
        url = format!("{}&container={}", url, resolve_alias(name));
    }
    let response = client.get(&url).send().await?;
    if !response.status().is_success() {
        let error: ApiError = response.json().await?;
        return Err(format!("Failed to fetch events: {}", error.error).into());
    }
    Ok(response.json().await?)
}
