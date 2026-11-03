//! `/etc/nordkraft/meters.toml`: which meter measures which node.

use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Where the controller's NATS listens. LAN-only in production.
    #[serde(default = "default_nats_url")]
    pub nats_url: String,
    /// Seconds between readings. The controller matches a reading to the
    /// agents' CPU samples when it is at most 150 s old, so keep this at
    /// or under 60.
    #[serde(default = "default_interval")]
    pub interval_seconds: u64,
    /// Seconds a single reading may take before it counts as failed.
    #[serde(default = "default_timeout")]
    pub read_timeout_seconds: u64,
    #[serde(default, rename = "meter")]
    pub meters: Vec<MeterConfig>,
}

fn default_nats_url() -> String {
    "nats://127.0.0.1:4222".into()
}
fn default_interval() -> u64 {
    60
}
fn default_timeout() -> u64 {
    30
}

/// One meter, one node. The `kind` picks the reader; the other fields are
/// what that reader needs (see setup/ENERGY.md for each).
#[derive(Debug, Clone, Deserialize)]
pub struct MeterConfig {
    /// The node this meter measures: `nodes.node_id` on the controller.
    pub node_id: String,
    /// `shelly-http`, `shelly-ble` or `netio`.
    pub kind: String,

    // shelly-http / netio
    /// Base URL, e.g. `http://10.0.0.50`.
    pub url: Option<String>,
    /// Shelly device auth (user is always `admin`) or NETIO basic auth.
    pub username: Option<String>,
    pub password: Option<String>,

    // shelly-*
    /// `switch` (plugs, Pro 1PM) or `em1` (Pro EM, Pro 3EM in monophase).
    #[serde(default = "default_component")]
    pub component: String,
    /// Component instance: `switch:0`, `em1:2`, ...
    #[serde(default)]
    pub component_id: u32,

    // shelly-ble
    /// Bluetooth address `AA:BB:CC:DD:EE:FF` (Linux). macOS hides addresses;
    /// use `name` there.
    pub address: Option<String>,
    /// Advertised name, e.g. `ShellyPlugSG3-ABCDEF123456`.
    pub name: Option<String>,

    // netio
    /// Outlet number on the PDU, 1-based.
    pub outlet: Option<u32>,
}

fn default_component() -> String {
    "switch".into()
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let cfg: Config = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        if cfg.meters.is_empty() {
            return Err(format!("{}: no [[meter]] entries", path.display()));
        }
        for m in &cfg.meters {
            m.validate()?;
        }
        Ok(cfg)
    }
}

impl MeterConfig {
    pub fn validate(&self) -> Result<(), String> {
        let need = |field: &str, v: &Option<String>| {
            if v.as_deref().map(str::trim).unwrap_or("").is_empty() {
                Err(format!(
                    "meter for {}: kind {} needs `{field}`",
                    self.node_id, self.kind
                ))
            } else {
                Ok(())
            }
        };
        match self.kind.as_str() {
            "shelly-http" => need("url", &self.url),
            "shelly-ble" => {
                if self.address.is_none() && self.name.is_none() {
                    Err(format!(
                        "meter for {}: shelly-ble needs `address` or `name`",
                        self.node_id
                    ))
                } else {
                    Ok(())
                }
            }
            "netio" => {
                need("url", &self.url)?;
                if self.outlet.unwrap_or(0) == 0 {
                    Err(format!(
                        "meter for {}: netio needs `outlet` (1-based)",
                        self.node_id
                    ))
                } else {
                    Ok(())
                }
            }
            other => Err(format!(
                "meter for {}: unknown kind '{other}' (shelly-http, shelly-ble, netio)",
                self.node_id
            )),
        }?;
        match self.component.as_str() {
            "switch" | "em1" => Ok(()),
            other => Err(format!(
                "meter for {}: unknown component '{other}' (switch, em1)",
                self.node_id
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_validates() {
        let cfg: Config = toml::from_str(
            r#"
            nats_url = "nats://10.0.0.200:4222"
            [[meter]]
            node_id = "ry-optiplex-1"
            kind = "shelly-ble"
            name = "ShellyPlugSG3-ABC"
            [[meter]]
            node_id = "ry-rpi4-2"
            kind = "netio"
            url = "http://10.0.0.60"
            outlet = 2
            "#,
        )
        .unwrap();
        assert_eq!(cfg.interval_seconds, 60);
        assert_eq!(cfg.meters.len(), 2);
        for m in &cfg.meters {
            m.validate().unwrap();
        }
        let bad: Config = toml::from_str(
            r#"
            [[meter]]
            node_id = "x"
            kind = "shelly-http"
            "#,
        )
        .unwrap();
        assert!(bad.meters[0].validate().is_err());
    }
}
