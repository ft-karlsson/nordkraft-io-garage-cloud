//! HTTPS ingress with auto-TLS: enable, disable, status, list.

use super::aliases::resolve_alias;
use super::api::*;
use super::containers::api_error;
use super::CoreResult;

/// `POST /ingress/{name}/enable`.
pub(crate) async fn enable(
    name: &str,
    subdomain: &str,
    port: u16,
    mode: &str,
) -> CoreResult<IngressEnableResponse> {
    let name = resolve_alias(name);
    let client = create_client()?;
    let request = IngressEnableRequest {
        subdomain: subdomain.to_string(),
        mode: mode.to_string(),
        target_port: port,
    };
    let url = format!("{}/ingress/{}/enable", *API_BASE_URL, name);
    let response = client.post(&url).json(&request).send().await?;
    if !response.status().is_success() {
        return Err(api_error(response).await);
    }
    Ok(response.json().await?)
}

/// `DELETE /ingress/{name}/disable`. The controller's reply body is not
/// always JSON, so None means "disabled, no details".
pub(crate) async fn disable(name: &str) -> CoreResult<Option<ApiResponse>> {
    let name = resolve_alias(name);
    let client = create_client()?;
    let url = format!("{}/ingress/{}/disable", *API_BASE_URL, name);
    let response = client.delete(&url).send().await?;
    if !response.status().is_success() {
        let text = response.text().await?;
        if let Ok(error) = serde_json::from_str::<ApiError>(&text) {
            return Err(error.error.into());
        }
        return Err(format!("Failed to disable ingress: {}", text).into());
    }
    let text = response.text().await?;
    Ok(serde_json::from_str::<ApiResponse>(&text).ok())
}

/// `GET /ingress/{name}/status`.
pub(crate) async fn status(name: &str) -> CoreResult<IngressStatusResponse> {
    let name = resolve_alias(name);
    let client = create_client()?;
    let url = format!("{}/ingress/{}/status", *API_BASE_URL, name);
    let response = client.get(&url).send().await?;
    if !response.status().is_success() {
        return Err(api_error(response).await);
    }
    Ok(response.json().await?)
}

/// `GET /ingress/list`.
pub(crate) async fn list() -> CoreResult<IngressListResponse> {
    let client = create_client()?;
    let url = format!("{}/ingress/list", *API_BASE_URL);
    let response = client.get(&url).send().await?;
    if !response.status().is_success() {
        return Err(api_error(response).await);
    }
    Ok(response.json().await?)
}
