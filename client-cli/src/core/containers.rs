//! Containers: list, inspect, logs, start, stop, remove. Names may be
//! aliases; every function resolves them.

use reqwest::Client;

use super::aliases::resolve_alias;
use super::api::*;
use super::CoreResult;

/// Turn a non-2xx reply into the error every handler has always shown:
/// the controller's `error` field, or the decode error if the body is not
/// an `ApiError`.
pub(crate) async fn api_error(response: reqwest::Response) -> Box<dyn std::error::Error> {
    match response.json::<ApiError>().await {
        Ok(e) => e.error.into(),
        Err(e) => Box::new(e),
    }
}

/// `GET /containers`, optionally filtered by `KEY=VALUE` labels.
pub(crate) async fn list(labels: &[String]) -> CoreResult<ContainerListResponse> {
    let client = create_client()?;
    let mut url = format!("{}/containers", *API_BASE_URL);
    if !labels.is_empty() {
        let qs = labels
            .iter()
            .map(|l| format!("label={}", crate::stream::urlencode(l)))
            .collect::<Vec<_>>()
            .join("&");
        url = format!("{url}?{qs}");
    }
    let response = client.get(&url).send().await?;
    if !response.status().is_success() {
        return Err(api_error(response).await);
    }
    Ok(response.json().await?)
}

/// What `GET /containers/{name}` came back with.
pub(crate) enum Inspect {
    /// The rich inspect document.
    Parsed(Box<ContainerInspectResponse>),
    /// Something older or simpler (an error object, a pre-update node);
    /// the body as the controller sent it.
    Raw(String),
}

pub(crate) async fn inspect(name: &str) -> CoreResult<Inspect> {
    let name = resolve_alias(name);
    let client = create_client()?;
    let url = format!("{}/containers/{}", *API_BASE_URL, name);
    let response = client.get(&url).send().await?;
    if !response.status().is_success() {
        return Err(api_error(response).await);
    }
    let body = response.text().await?;
    Ok(
        match serde_json::from_str::<ContainerInspectResponse>(&body) {
            Ok(c) => Inspect::Parsed(Box::new(c)),
            Err(_) => Inspect::Raw(body),
        },
    )
}

/// What `GET /containers/{name}/logs` came back with.
pub(crate) enum Logs {
    Ready(LogsResponse),
    /// The body was not a logs document — usually a container that is still
    /// starting. `status` is the container's status if it could be fetched.
    NotReady {
        status: Option<String>,
        raw: String,
    },
}

pub(crate) async fn logs(name: &str, lines: usize) -> CoreResult<Logs> {
    let name = resolve_alias(name);
    let client = create_client()?;
    let url = format!("{}/containers/{}/logs?lines={}", *API_BASE_URL, name, lines);
    let response = client.get(&url).send().await?;
    if !response.status().is_success() {
        return Err(api_error(response).await);
    }
    let body = response.text().await?;
    match serde_json::from_str::<LogsResponse>(&body) {
        Ok(logs) => Ok(Logs::Ready(logs)),
        Err(_) => Ok(Logs::NotReady {
            status: status_of(&client, &name).await,
            raw: body,
        }),
    }
}

/// Just the status field of one container, from the list endpoint. None if
/// the list could not be fetched or the container is not in it.
pub(crate) async fn status_of(client: &Client, name: &str) -> Option<String> {
    let url = format!("{}/containers", *API_BASE_URL);
    let response = client.get(&url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let data: ContainerListResponse = response.json().await.ok()?;
    data.containers
        .into_iter()
        .find(|c| c.name == name || c.container_id == name)
        .map(|c| c.status)
}

/// `POST /containers/{name}/start`; with `wait`, the controller answers only
/// once the node reports the container running (or a timeout).
pub(crate) async fn start(name: &str, wait: bool) -> CoreResult<ApiResponse> {
    let name = resolve_alias(name);
    let client = create_client()?;
    let url = if wait {
        format!("{}/containers/{}/start?wait=true", *API_BASE_URL, name)
    } else {
        format!("{}/containers/{}/start", *API_BASE_URL, name)
    };
    let response = client.post(&url).send().await?;
    if !response.status().is_success() {
        return Err(api_error(response).await);
    }
    Ok(response.json().await?)
}

/// `POST /containers/{name}/stop` — graceful, so it uses the slow client.
pub(crate) async fn stop(name: &str) -> CoreResult<ApiResponse> {
    let name = resolve_alias(name);
    let client = create_slow_client()?;
    let url = format!("{}/containers/{}/stop", *API_BASE_URL, name);
    let response = client.post(&url).send().await?;
    if !response.status().is_success() {
        return Err(api_error(response).await);
    }
    Ok(response.json().await?)
}

/// `DELETE /containers/{name}`; `force` kills instead of stopping first.
pub(crate) async fn remove(name: &str, force: bool) -> CoreResult<ApiResponse> {
    let name = resolve_alias(name);
    let client = create_slow_client()?;
    let url = if force {
        format!("{}/containers/{}?force=true", *API_BASE_URL, name)
    } else {
        format!("{}/containers/{}", *API_BASE_URL, name)
    };
    let response = client.delete(&url).send().await?;
    if !response.status().is_success() {
        return Err(api_error(response).await);
    }
    Ok(response.json().await?)
}
