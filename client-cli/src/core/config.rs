//! `~/.nordkraft/` layout and the connection file written by `nordkraft setup`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub(crate) const CONFIG_DIR: &str = ".nordkraft";

pub(crate) const API_URL_FILE: &str = "api_url";

pub(crate) const ALIASES_FILE: &str = "aliases.json";

pub(crate) const DEPLOYMENTS_DIR: &str = "deployments";

pub(crate) const WG_CONFIG_FILE: &str = "wg.conf";

pub(crate) const WG_INTERFACE: &str = "nordkraft";

pub(crate) const CONNECTION_FILE: &str = "connection.json";

pub(crate) fn get_config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(CONFIG_DIR)
}

// Persisted connection info (written by setup, read by connect/disconnect)
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ConnectionConfig {
    pub(crate) user_id: String,
    pub(crate) full_name: String,
    pub(crate) email: String,
    pub(crate) plan_id: String,
    pub(crate) assigned_garage: String,
    pub(crate) wireguard_ip: String,
    pub(crate) server_public_key: String,
    pub(crate) server_endpoint: String,
    pub(crate) allowed_ips: Vec<String>,
    /// Controller API endpoint via WireGuard. Default: http://172.20.0.254:8001/api
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) api_endpoint: Option<String>,
    /// Public signup API endpoint. Default: https://cloud.nordkraft.io/api
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) public_api_url: Option<String>,
}

pub(crate) fn get_wg_config_path() -> PathBuf {
    get_config_dir().join(WG_CONFIG_FILE)
}

pub(crate) fn get_connection_config_path() -> PathBuf {
    get_config_dir().join(CONNECTION_FILE)
}

pub(crate) fn load_connection_config() -> Option<ConnectionConfig> {
    let path = get_connection_config_path();
    if path.exists() {
        if let Ok(contents) = std::fs::read_to_string(&path) {
            if let Ok(config) = serde_json::from_str(&contents) {
                return Some(config);
            }
        }
    }
    None
}

pub(crate) fn save_connection_config(
    config: &ConnectionConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let dir = get_config_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(CONNECTION_FILE);
    std::fs::write(path, serde_json::to_string_pretty(config)?)?;
    Ok(())
}
