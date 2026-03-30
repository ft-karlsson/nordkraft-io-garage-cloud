// config.rs - Complete configuration with nerdctl/Kata runtime support
use serde::Deserialize;
use std::env;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    // Core settings
    pub dev_mode: bool,
    pub dev_user_public_key: String,
    pub database_url: String,

    // Node operation configuration
    pub mode: OperationMode,
    pub node_id: String,
    pub bind_address: String,
    pub bind_port: u16,
    pub heartbeat_interval_seconds: u64,

    // NATS configuration
    pub nats_url: String,
    pub nats_enabled: bool,
    pub nats_reconnect_attempts: u32,
    pub nats_reconnect_delay_ms: u64,

    // Legacy HTTP configuration (for backward compatibility)
    pub controller_url: Option<String>,
    pub cluster_state_broadcast_interval: u64,

    // Agent networking (for return routes to controller)
    pub controller_internal_ip: String,
    pub agent_interface: String,
    pub vpn_network: String,
    pub container_network: String,
    pub local_network: String,

    // ACME/Let's Encrypt configuration
    pub acme_account_name: String,
    pub acme_http01_webrootfolder: String,

    // Container Runtime Configuration (nerdctl + Kata)
    pub container_runtime: String, // "nerdctl" (or "podman" for fallback)
    pub use_kata: bool,            // Enable Kata by default for agents
    pub disable_kata: bool,        // Explicit override to disable Kata
    pub kata_runtime_class: String, // "io.containerd.kata.v2"
    pub container_restart_policy: String, // "unless-stopped"

    // Admin API security (for signup-api → container-api provisioning)
    pub admin_api_key: String, // Shared secret for /api/admin/* endpoints
    pub admin_allowed_ips: Vec<String>, // Source IPs allowed to call admin endpoints
    pub wg_interface: String,  // WireGuard interface name (for wg set commands)

    // Quota enforcement (cloud.nordkraft.io = true, self-hosted = false)
    pub quota_enforced: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OperationMode {
    Controller,
    Agent,
    Hybrid,
}

impl OperationMode {
    /// Check if this mode can run containers (agent or hybrid)
    pub fn is_agent(&self) -> bool {
        matches!(self, OperationMode::Agent | OperationMode::Hybrid)
    }
}

impl std::str::FromStr for OperationMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "controller" => Ok(OperationMode::Controller),
            "agent" => Ok(OperationMode::Agent),
            "hybrid" => Ok(OperationMode::Hybrid),
            _ => Ok(OperationMode::Hybrid), // Default fallback
        }
    }
}

pub fn init_config() -> AppConfig {
    let mode: OperationMode = env::var("NORDKRAFT_MODE")
        .unwrap_or_else(|_| "hybrid".to_string())
        .parse()
        .unwrap_or(OperationMode::Hybrid);

    // Kata is ON by default for agent/hybrid modes, OFF for controller
    let default_use_kata = mode.is_agent();

    AppConfig {
        // Core configuration
        dev_mode: env::var("DEV_MODE")
            .unwrap_or_else(|_| "false".to_string())
            .parse()
            .unwrap_or(false),
        dev_user_public_key: env::var("DEV_USER_PUBLIC_KEY")
            .unwrap_or_else(|_| "dev_key_placeholder".to_string()),
        database_url: env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgresql://garage_user@localhost:5432/garage_cloud".to_string()),

        // Node configuration
        mode,
        node_id: env::var("NODE_ID").unwrap_or_else(|_| {
            if let Ok(hostname) = env::var("HOSTNAME") {
                format!("node-{}", hostname)
            } else {
                "node-1".to_string()
            }
        }),
        bind_address: env::var("BIND_ADDRESS").unwrap_or_else(|_| "127.0.0.1".to_string()),
        bind_port: env::var("BIND_PORT")
            .unwrap_or_else(|_| "8001".to_string())
            .parse()
            .unwrap_or(8001),
        heartbeat_interval_seconds: env::var("HEARTBEAT_INTERVAL")
            .unwrap_or_else(|_| "30".to_string())
            .parse()
            .unwrap_or(30),

        // NATS configuration
        nats_url: env::var("NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".to_string()),
        nats_enabled: env::var("NATS_ENABLED")
            .unwrap_or_else(|_| "true".to_string())
            .parse()
            .unwrap_or(true),
        nats_reconnect_attempts: env::var("NATS_RECONNECT_ATTEMPTS")
            .unwrap_or_else(|_| "10".to_string())
            .parse()
            .unwrap_or(10),
        nats_reconnect_delay_ms: env::var("NATS_RECONNECT_DELAY_MS")
            .unwrap_or_else(|_| "5000".to_string())
            .parse()
            .unwrap_or(5000),

        // Legacy HTTP fallback
        controller_url: env::var("CONTROLLER_URL").ok(),
        cluster_state_broadcast_interval: env::var("CLUSTER_STATE_BROADCAST_INTERVAL")
            .unwrap_or_else(|_| "60".to_string())
            .parse()
            .unwrap_or(60),

        // Agent networking - for setting up return routes
        controller_internal_ip: env::var("CONTROLLER_INTERNAL_IP")
            .unwrap_or_else(|_| "10.0.0.200".to_string()),
        agent_interface: env::var("AGENT_INTERFACE").unwrap_or_else(|_| "eth0".to_string()),
        vpn_network: env::var("VPN_NETWORK").unwrap_or_else(|_| "172.20.0.0/16".to_string()),
        container_network: env::var("CONTAINER_NETWORK")
            .unwrap_or_else(|_| "172.21.0.0/16".to_string()),
        local_network: env::var("LOCAL_NETWORK").unwrap_or_else(|_| "10.0.0.0/24".to_string()),

        // ACME/Let's Encrypt configuration
        acme_account_name: env::var("ACME_ACCOUNT_NAME")
            .unwrap_or_else(|_| "letsencrypt-production".to_string()),
        acme_http01_webrootfolder: env::var("ACME_HTTP01_WEBROOTFOLDER")
            .unwrap_or_else(|_| "/var/www/acme".to_string()),

        // Container Runtime Configuration (nerdctl + Kata)
        container_runtime: env::var("CONTAINER_RUNTIME").unwrap_or_else(|_| "nerdctl".to_string()),
        use_kata: env::var("USE_KATA")
            .map(|v| v.parse().unwrap_or(default_use_kata))
            .unwrap_or(default_use_kata),
        disable_kata: env::var("DISABLE_KATA")
            .unwrap_or_else(|_| "false".to_string())
            .parse()
            .unwrap_or(false),
        kata_runtime_class: env::var("KATA_RUNTIME_CLASS")
            .unwrap_or_else(|_| "io.containerd.kata.v2".to_string()),
        container_restart_policy: env::var("CONTAINER_RESTART_POLICY")
            .unwrap_or_else(|_| "unless-stopped".to_string()),

        // Admin API security
        admin_api_key: env::var("ADMIN_API_KEY")
            .expect("ADMIN_API_KEY must be set — generate one with: openssl rand -hex 32"),
        admin_allowed_ips: env::var("ADMIN_ALLOWED_IPS")
            .unwrap_or_else(|_| "127.0.0.1,172.20.0.254".to_string())
            .split(',')
            .map(|s| s.trim().to_string())
            .collect(),
        wg_interface: env::var("WG_INTERFACE").unwrap_or_else(|_| "wg0".to_string()),

        // Quota enforcement — default OFF for self-hosted, ON for cloud.nordkraft.io
        quota_enforced: env::var("QUOTA_ENFORCED")
            .unwrap_or_else(|_| "false".to_string())
            .parse()
            .unwrap_or(false),
    }
}
