//! HTTP client, API base URL resolution and the request/response types the
//! controller speaks. Shared by every command.

use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;

use super::config::*;
use super::spec::LifecycleSpec;

pub(crate) static API_BASE_URL: LazyLock<String> = LazyLock::new(|| resolve_api_base_url().0);

/// Resolve the API base URL and report where it came from.
///
///   1. API_BASE_URL env var               (one-off override)
///   2. ~/.nordkraft/api_url               (`nordkraft config set-api …`)
///   3. connection.json api_endpoint       (written by `nordkraft setup`)
///   4. http://172.20.0.254:8001/api       (controller default over WireGuard)
pub(crate) fn resolve_api_base_url() -> (String, &'static str) {
    if let Ok(url) = std::env::var("API_BASE_URL") {
        if !url.is_empty() {
            return (url, "API_BASE_URL env var");
        }
    }

    if let Some(url) = load_api_url_override() {
        return (url, "~/.nordkraft/api_url (nordkraft config set-api)");
    }

    if let Some(config) = load_connection_config() {
        if let Some(endpoint) = config.api_endpoint {
            if !endpoint.is_empty() {
                return (endpoint, "~/.nordkraft/connection.json");
            }
        }
    }

    (
        "http://172.20.0.254:8001/api".to_string(),
        "default (controller over WireGuard)",
    )
}

pub(crate) fn api_url_override_path() -> PathBuf {
    get_config_dir().join(API_URL_FILE)
}

pub(crate) fn load_api_url_override() -> Option<String> {
    let s = fs::read_to_string(api_url_override_path()).ok()?;
    let s = s.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Normalise a user-supplied base URL: add scheme, strip trailing slash,
/// make sure it ends in /api.
pub(crate) fn normalise_api_url(input: &str) -> String {
    let mut url = input.trim().trim_end_matches('/').to_string();
    if !url.starts_with("http://") && !url.starts_with("https://") {
        url = format!("http://{}", url);
    }
    if !url.ends_with("/api") {
        url.push_str("/api");
    }
    url
}

pub(crate) static PUBLIC_API_URL: LazyLock<String> = LazyLock::new(|| {
    // 1. Env var override
    if let Ok(url) = std::env::var("PUBLIC_API_URL") {
        return url;
    }

    // 2. From connection config if set (for re-setup / self-hosted)
    if let Some(config) = load_connection_config() {
        if let Some(url) = config.public_api_url {
            if !url.is_empty() {
                return url;
            }
        }
    }

    // 3. Default — NordKraft.io cloud
    "https://cloud.nordkraft.io/api".to_string()
});

pub(crate) fn create_client() -> Result<Client, Box<dyn std::error::Error>> {
    Ok(Client::builder().timeout(Duration::from_secs(30)).build()?)
}

/// Client for calls that wait on a graceful container stop server-side
/// (stop-timeout is 30s, so the HTTP timeout must be longer than that).
pub(crate) fn create_slow_client() -> Result<Client, Box<dyn std::error::Error>> {
    Ok(Client::builder().timeout(Duration::from_secs(90)).build()?)
}

/// Pull `{"error": "..."}` out of an API reply, or fall back to the status.
pub(crate) async fn api_error_text(resp: reqwest::Response) -> String {
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
        .unwrap_or_else(|| {
            if text.is_empty() {
                format!("HTTP {status}")
            } else {
                text
            }
        })
}

pub(crate) fn unreachable_api(e: reqwest::Error) -> String {
    if e.is_connect() || e.is_timeout() {
        "Cannot reach the API. Is WireGuard connected? (nordkraft connect)".to_string()
    } else {
        format!("Network error: {e}")
    }
}

// Claim API types
#[derive(Debug, Serialize)]
pub(crate) struct ClaimRequest {
    pub(crate) token: String,
    pub(crate) wireguard_public_key: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ClaimApiResponse {
    pub(crate) success: bool,
    pub(crate) data: Option<ClaimData>,
    pub(crate) error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ClaimData {
    pub(crate) wireguard_ip: String,
    pub(crate) server_public_key: String,
    pub(crate) server_endpoint: String,
    pub(crate) allowed_ips: Vec<String>,
    pub(crate) user_id: String,
    pub(crate) full_name: String,
    pub(crate) email: String,
    pub(crate) plan_id: String,
    pub(crate) assigned_garage: String,
}

// Auth
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct AuthResponse {
    pub(crate) authenticated: bool,
    pub(crate) user: Option<AuthUser>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct AuthUser {
    pub(crate) id: String,
    pub(crate) full_name: String,
    pub(crate) email: String,
    pub(crate) wireguard_ip: String,
    pub(crate) wireguard_public_key: String,
    pub(crate) plan_id: String,
    pub(crate) account_status: String,
    #[serde(default)]
    pub(crate) allowed_actions: Vec<String>,
    pub(crate) primary_garage_id: Option<String>,
    pub(crate) user_slot: Option<u32>,
}

// Containers
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ContainerInfo {
    pub(crate) container_id: String,
    pub(crate) name: String,
    pub(crate) image: String,
    pub(crate) status: String,
    #[serde(default)]
    pub(crate) status_message: Option<String>,
    pub(crate) pod_id: Option<String>,
    pub(crate) created_at: String,
    #[serde(default)]
    pub(crate) ports: Vec<PortInfo>,
    pub(crate) container_ip: Option<String>,
    pub(crate) ipv6_address: Option<String>,
    #[serde(default)]
    pub(crate) ipv6_enabled: bool,
    #[serde(default)]
    pub(crate) labels: HashMap<String, String>,
    /// Attributed share of the node's measured watts, when the node is metered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) watts: Option<f64>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(untagged)]
pub(crate) enum PortInfo {
    Simple(String),
    Detailed {
        port: u16,
        protocol: Option<String>,
        access_url: Option<String>,
        ipv6_url: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
pub(crate) struct ContainerListResponse {
    pub(crate) containers: Vec<ContainerInfo>,
    // #[serde(default)]
    // source: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct DeployRequest {
    pub(crate) image: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) ports: Vec<PortSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) command: Option<Vec<String>>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub(crate) env_vars: HashMap<String, String>,
    pub(crate) cpu_limit: f32,
    pub(crate) memory_limit: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) enable_persistence: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) volume_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) volume_size: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) enable_ipv6: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) target_garage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) hardware_preference: Option<String>,
    #[serde(skip_serializing_if = "LifecycleSpec::is_empty")]
    pub(crate) lifecycle: LifecycleSpec,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub(crate) labels: HashMap<String, String>,
    /// Ask the controller to answer only once the node reports the container
    /// running (or failed). Needed before anything that acts on the container.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) wait: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) wait_timeout_s: Option<u64>,
}

/// How long `deploy --volume-from` lets the controller wait for the node
/// (image pull included) before giving up. The controller caps it at 600.
pub(crate) const DEPLOY_WAIT_SECS: u64 = 300;

#[derive(Debug, Serialize)]
pub(crate) struct PortSpec {
    pub(crate) port: u16,
    pub(crate) protocol: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct DeployResponse {
    pub(crate) status: String,
    pub(crate) container_name: Option<String>,
    pub(crate) container_ip: Option<String>,
    pub(crate) ipv6_address: Option<String>,
    #[serde(default)]
    pub(crate) ipv6_enabled: bool,
    pub(crate) garage: Option<String>,
    pub(crate) node: Option<String>,
    pub(crate) message: Option<String>,
    #[serde(default)]
    pub(crate) ipv6_urls: Vec<String>,
    /// Bridge/dev mode: URLs of the published ports on the host
    #[serde(default)]
    pub(crate) access_urls: Vec<String>,
    #[serde(default)]
    pub(crate) network_mode: Option<String>,
}

// Upgrade
#[derive(Debug, Serialize)]
pub(crate) struct CliUpgradeRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ports: Option<Vec<PortSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) command: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) env_vars: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cpu_limit: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) memory_limit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) volume_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) volume_size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) lifecycle: Option<LifecycleSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) labels: Option<HashMap<String, String>>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct UpgradeResponse {
    #[serde(default)]
    pub(crate) status: Option<String>,
    pub(crate) container_name: Option<String>,
    pub(crate) container_ip: Option<String>,
    pub(crate) node: Option<String>,
    pub(crate) image: Option<String>,
    pub(crate) revision_old: Option<u32>,
    pub(crate) revision: Option<u32>,
    #[serde(default)]
    pub(crate) error: Option<String>,
    #[serde(default)]
    pub(crate) message: Option<String>,
}

// Ingress
#[derive(Debug, Serialize)]
pub(crate) struct IngressEnableRequest {
    pub(crate) subdomain: String,
    pub(crate) mode: String,
    pub(crate) target_port: u16,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct IngressEnableResponse {
    pub(crate) status: String,
    pub(crate) url: Option<String>,
    pub(crate) subdomain: Option<String>,
    pub(crate) domain: Option<String>,
    pub(crate) ssl: Option<bool>,
    pub(crate) certificate: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct IngressStatusResponse {
    pub(crate) enabled: bool,
    pub(crate) subdomain: Option<String>,
    pub(crate) url: Option<String>,
    pub(crate) target_port: Option<u16>,
    pub(crate) ssl: Option<bool>,
    pub(crate) certificate_status: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct IngressListResponse {
    pub(crate) routes: Vec<IngressRoute>,
    pub(crate) count: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct IngressRoute {
    pub(crate) container_id: String,
    pub(crate) subdomain: String,
    pub(crate) url: String,
    pub(crate) target_port: u16,
    #[serde(default)]
    pub(crate) is_active: bool,
    #[serde(default)]
    pub(crate) mode: Option<String>,
    #[serde(default)]
    pub(crate) target_ip: Option<String>,
    #[serde(default)]
    pub(crate) firewall_open: Option<bool>,
    #[serde(default)]
    pub(crate) public_port: Option<u16>,
    #[serde(default)]
    pub(crate) created_at: Option<String>,
}

// IPv6
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Ipv6OpenResponse {
    pub(crate) status: String,
    pub(crate) ipv6_address: Option<String>,
    #[serde(default)]
    pub(crate) ports: Vec<u16>,
    pub(crate) rule_id: Option<String>,
    #[serde(default)]
    pub(crate) access_urls: Vec<String>,
    pub(crate) message: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Ipv6StatusResponse {
    pub(crate) container_name: Option<String>,
    pub(crate) ipv6_address: Option<String>,
    #[serde(default)]
    pub(crate) exposed_ports: Vec<u16>,
    pub(crate) firewall_status: Option<String>,
    pub(crate) rule_id: Option<String>,
    pub(crate) pfsense_configured: Option<bool>,
    #[serde(default)]
    pub(crate) access_urls: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Ipv6ListResponse {
    pub(crate) allocations: Vec<Ipv6Allocation>,
    pub(crate) count: Option<u32>,
    pub(crate) pfsense_configured: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Ipv6Allocation {
    pub(crate) container_name: String,
    pub(crate) ipv6_address: String,
    #[serde(default)]
    pub(crate) exposed_ports: Vec<u16>,
    pub(crate) firewall_status: String,
    pub(crate) rule_id: Option<String>,
    #[serde(default)]
    pub(crate) access_urls: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct Ipv6PortsRequest {
    pub(crate) ports: Vec<u16>,
}

// Network
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct NetworkInfoResponse {
    pub(crate) garage: Option<String>,
    pub(crate) container_subnet: Option<String>,
    pub(crate) user_ip: Option<String>,
}

// Nodes
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct NodesResponse {
    pub(crate) nodes: Vec<NodeInfo>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct NodeInfo {
    pub(crate) id: String,
    pub(crate) address: Option<String>,
    pub(crate) port: Option<u16>,
    pub(crate) status: String,
    pub(crate) last_heartbeat: Option<String>,
}

// Status
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct StatusResponse {
    pub(crate) status: String,
    pub(crate) timestamp: Option<String>,
    #[serde(default)]
    pub(crate) version: Option<String>,
    #[serde(default)]
    pub(crate) node_id: Option<String>,
    #[serde(default)]
    pub(crate) mode: Option<String>,
    #[serde(default)]
    pub(crate) dev: Option<bool>,
    #[serde(default)]
    pub(crate) network_mode: Option<String>,
    #[serde(default)]
    pub(crate) auth: Option<String>,
}

// Logs
#[derive(Debug, Deserialize)]
pub(crate) struct LogsResponse {
    pub(crate) logs: String,
    #[serde(default)]
    pub(crate) source: Option<String>,
}

// Inspect - mirrors ContainerInspectData from server
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ContainerInspectResponse {
    #[serde(default)]
    pub(crate) container_id: Option<String>,
    pub(crate) name: String,
    pub(crate) image: String,
    pub(crate) image_digest: Option<String>,
    pub(crate) status: String,
    pub(crate) created_at: String,
    pub(crate) started_at: Option<String>,
    pub(crate) finished_at: Option<String>,
    pub(crate) exit_code: Option<i64>,
    pub(crate) restart_count: Option<i64>,
    pub(crate) container_ip: Option<String>,
    pub(crate) ipv6_address: Option<String>,
    #[serde(default)]
    pub(crate) ipv6_enabled: bool,
    #[serde(default)]
    pub(crate) ports: Vec<serde_json::Value>,
    #[serde(default)]
    pub(crate) env_vars: Vec<String>,
    #[serde(default)]
    pub(crate) command: Vec<String>,
    pub(crate) hostname: Option<String>,
    pub(crate) node_id: String,
    pub(crate) runtime: String,
    pub(crate) cpu_limit: Option<f64>,
    pub(crate) memory_limit: Option<i64>,
    #[serde(default)]
    pub(crate) persistence_enabled: bool,
    #[serde(default)]
    pub(crate) volume_mounts: Vec<String>,
    #[serde(default)]
    pub(crate) labels: std::collections::HashMap<String, String>,
}

// Generic
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ApiResponse {
    pub(crate) status: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) message: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ApiError {
    pub(crate) error: String,
    #[serde(default)]
    pub(crate) plan: Option<String>,
    // #[serde(default)]
    // plan_id: Option<String>,
    #[serde(default)]
    pub(crate) usage: Option<serde_json::Value>,
}
