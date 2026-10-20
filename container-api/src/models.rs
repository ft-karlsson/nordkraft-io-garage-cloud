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
    /// User labels (the `user.*` runtime labels, prefix stripped).
    #[serde(default)]
    pub labels: HashMap<String, String>,
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

// ============= REGISTRY AUTH =============

/// Credentials the agent presents when pulling from a tenant's own private registry.
/// Resolved by the controller from the registry container's stored deploy config and
/// scoped to exactly one registry host, so a pull from any other registry gets no
/// credentials at all.
#[derive(Serialize, Deserialize, Clone)]
pub struct RegistryAuth {
    /// "ip:port" exactly as it appears in the image reference
    pub host: String,
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for RegistryAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryAuth")
            .field("host", &self.host)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

// ============= SANDBOX LIFECYCLE =============

/// Per-container lifecycle, in the `[lifecycle]` section of a .nk spec.
/// The vocabulary follows Daytona/Mastra sandboxes so an orchestrator can
/// map its own settings 1:1:
///   restart       always | unless-stopped | on-failure | never
///   auto_stop     stop after this long without an exec/shell session ("15m"; "0" = never)
///   auto_delete   remove this long after the container stopped ("1h"; "0" = never)
///   max_lifetime  stop this long after creation, whatever happens ("24h"; "0" = never)
/// Durations: "90s", "15m", "2h", "1d" or plain seconds.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LifecycleSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_stop: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_delete: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_lifetime: Option<String>,
}

/// Lifecycle plus free-form labels, as carried on deploy/upgrade.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SandboxSpec {
    #[serde(default)]
    pub lifecycle: LifecycleSpec,
    #[serde(default)]
    pub labels: HashMap<String, String>,
}

impl SandboxSpec {
    pub fn is_empty(&self) -> bool {
        self.lifecycle == LifecycleSpec::default() && self.labels.is_empty()
    }
}

/// Parse "90s" / "15m" / "2h" / "1d" / "300" into seconds. "0" means never
/// and parses to Some(0); None is a parse error.
pub fn parse_duration_secs(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, mult) = match s.chars().last()? {
        's' => (&s[..s.len() - 1], 1),
        'm' => (&s[..s.len() - 1], 60),
        'h' => (&s[..s.len() - 1], 3600),
        'd' => (&s[..s.len() - 1], 86400),
        c if c.is_ascii_digit() => (s, 1),
        _ => return None,
    };
    num.trim().parse::<u64>().ok().map(|n| n * mult)
}

/// nerdctl's `--restart` value for a spec word; None when invalid.
pub fn restart_policy_flag(word: &str) -> Option<&'static str> {
    match word {
        "always" => Some("always"),
        "unless-stopped" => Some("unless-stopped"),
        "on-failure" => Some("on-failure"),
        "never" | "no" => Some("no"),
        _ => None,
    }
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
    #[serde(default)]
    pub lifecycle: LifecycleSpec,
    #[serde(default)]
    pub labels: HashMap<String, String>,
    /// Block until the container is running (or failed) before answering —
    /// the sandbox-API contract: when deploy returns, exec works. Remote
    /// deploys otherwise answer "scheduled" while the agent is still pulling
    /// the image. Default false keeps the CLI's streaming UX unchanged.
    #[serde(default)]
    pub wait: bool,
    /// Upper bound for `wait`, seconds (default 120, max 600).
    pub wait_timeout_s: Option<u64>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<LifecycleSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub labels: Option<HashMap<String, String>>,
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
    #[serde(default)]
    pub lifecycle: LifecycleSpec,
    #[serde(default)]
    pub labels: HashMap<String, String>,
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

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration_secs("90s"), Some(90));
        assert_eq!(parse_duration_secs("15m"), Some(900));
        assert_eq!(parse_duration_secs("2h"), Some(7200));
        assert_eq!(parse_duration_secs("1d"), Some(86400));
        assert_eq!(parse_duration_secs("300"), Some(300));
        assert_eq!(parse_duration_secs("0"), Some(0));
        assert_eq!(parse_duration_secs("soon"), None);
        assert_eq!(parse_duration_secs(""), None);
    }

    #[test]
    fn restart_words() {
        assert_eq!(restart_policy_flag("never"), Some("no"));
        assert_eq!(restart_policy_flag("always"), Some("always"));
        assert_eq!(restart_policy_flag("sometimes"), None);
    }
}
