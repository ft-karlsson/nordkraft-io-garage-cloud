// models.rs - All data structures in one place
use rocket::serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ============= USER =============

#[derive(Debug, Serialize, Clone)]
pub struct User {
    pub id: String,
    pub email: String,
    pub full_name: String,
    pub wireguard_public_key: String,
    pub wireguard_ip: String,
    pub plan_id: String,
    pub account_status: String,
    pub allowed_actions: Vec<String>,
    pub primary_garage_id: String,
    pub user_slot: Option<i32>,
}

// ============= CONTAINER =============

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ContainerInfo {
    pub container_id: String,
    pub name: String,
    pub image: String,
    pub status: String,
    pub pod_id: Option<String>,
    pub created_at: String,
    pub ports: Vec<PortMapping>,
    pub container_ip: Option<String>,
    #[serde(default)]
    pub ipv6_address: Option<String>,
    #[serde(default)]
    pub ipv6_enabled: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PortMapping {
    pub port: u16,
    pub protocol: String,
    pub access_url: String,
    #[serde(default)]
    pub ipv6_url: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PortSpec {
    pub port: u16,
    #[serde(default = "default_protocol")]
    pub protocol: String,
}

fn default_protocol() -> String {
    "tcp".to_string()
}

// ============= DEPLOYMENT =============

#[derive(Debug, Deserialize)]
pub struct DeployRequest {
    pub image: String,
    pub ports: Option<Vec<PortSpec>>,
    pub command: Option<Vec<String>>,
    pub env_vars: Option<HashMap<String, String>>,
    pub cpu_limit: Option<f32>,
    pub memory_limit: Option<String>,
    pub target_garage: Option<String>,
    pub hardware_preference: Option<String>,
    pub architecture: Option<String>,
    pub enable_persistence: Option<bool>,
    pub volume_path: Option<String>,
    pub volume_size: Option<String>, // e.g. "1g", "512m" — default 1g
    #[serde(default)]
    pub enable_ipv6: bool,
}

// ============= UPGRADE =============

/// Partial update request — all fields optional except what changes.
/// Only provided fields are applied; omitted fields keep their current values.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct UpgradeRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ports: Option<Vec<PortSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env_vars: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_limit: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_limit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_size: Option<String>,
}

/// Stored container config — the full set of deploy parameters.
/// Persisted in the `container_config` table so upgrade can
/// merge partial changes against the last-known config.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ContainerConfig {
    pub container_name: String,
    pub image: String,
    pub ports: Vec<PortSpec>,
    pub command: Option<Vec<String>>,
    pub env_vars: HashMap<String, String>,
    pub cpu_limit: f32,
    pub memory_limit: String,
    pub enable_persistence: bool,
    pub volume_path: Option<String>,
    pub volume_size: String, // e.g. "1g" — allocated disk for quota
    pub enable_ipv6: bool,
}

// ============= NODE =============

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    pub id: String,
    pub address: String,
    pub port: u16,
    pub status: String,
    pub last_heartbeat: chrono::DateTime<chrono::Utc>,
}

// ============= HELPERS =============

/// Build IPv6 URL for a port
pub fn build_ipv6_url(ipv6_address: &str, port: u16) -> String {
    match port {
        80 => format!("http://[{}]/", ipv6_address),
        443 => format!("https://[{}]/", ipv6_address),
        _ => format!("[{}]:{}", ipv6_address, port),
    }
}
