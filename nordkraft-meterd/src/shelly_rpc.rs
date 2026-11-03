//! Shelly Gen2/Gen3 JSON-RPC: the request frames and the parts of the
//! status replies that are a power reading. Shared by the HTTP and the
//! Bluetooth reader; only the transport differs.

use serde::Deserialize;

/// Which component to read and how its reply is shaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Component {
    /// `Switch.GetStatus`: `apower` W, `aenergy.total` Wh (plugs, Pro 1PM).
    Switch,
    /// `EM1.GetStatus`: `act_power` W; energy via `EM1Data.GetStatus`
    /// `total_act_energy` Wh (Pro EM, Pro 3EM in monophase profile).
    Em1,
}

impl Component {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "switch" => Some(Self::Switch),
            "em1" => Some(Self::Em1),
            _ => None,
        }
    }

    pub fn status_method(self) -> &'static str {
        match self {
            Self::Switch => "Switch.GetStatus",
            Self::Em1 => "EM1.GetStatus",
        }
    }
}

/// A JSON-RPC request frame, as the device wants it over any transport.
pub fn request(id: u64, method: &str, component_id: u32) -> Vec<u8> {
    request_with(id, method, serde_json::json!({ "id": component_id }))
}

/// Same frame with free-form params, for one-off calls (`nordkraft-meterd rpc`).
pub fn request_with(id: u64, method: &str, params: serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "id": id,
        "src": "nordkraft-meterd",
        "method": method,
        "params": params,
    }))
    .expect("static json")
}

#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub watts: f64,
    pub energy_wh: Option<f64>,
}

#[derive(Deserialize)]
struct Envelope {
    result: Option<serde_json::Value>,
    error: Option<serde_json::Value>,
}

/// The reading out of a status reply. `Em1` gives watts only; the energy
/// counter comes from a second call, see [`em1_energy`].
pub fn parse_status(component: Component, body: &[u8]) -> Result<Reading, String> {
    let env: Envelope = serde_json::from_slice(body).map_err(|e| format!("bad reply: {e}"))?;
    if let Some(err) = env.error {
        return Err(format!("device error: {err}"));
    }
    let r = env.result.ok_or("reply has no result")?;
    match component {
        Component::Switch => Ok(Reading {
            watts: r["apower"]
                .as_f64()
                .ok_or("no apower in Switch.GetStatus")?,
            energy_wh: r["aenergy"]["total"].as_f64(),
        }),
        Component::Em1 => Ok(Reading {
            watts: r["act_power"]
                .as_f64()
                .ok_or("no act_power in EM1.GetStatus")?,
            energy_wh: None,
        }),
    }
}

/// `EM1Data.GetStatus` → `total_act_energy` Wh.
pub fn em1_energy(body: &[u8]) -> Option<f64> {
    let env: Envelope = serde_json::from_slice(body).ok()?;
    env.result?["total_act_energy"].as_f64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_status() {
        let body = br#"{"id":1,"src":"shellyplugsg3-aabbcc","dst":"nordkraft-meterd","result":{"id":0,"source":"init","output":true,"apower":18.4,"voltage":231.2,"current":0.112,"aenergy":{"total":12345.678,"by_minute":[300.1,299.8,301.0],"minute_ts":1793650000},"temperature":{"tC":41.2,"tF":106.2}}}"#;
        let r = parse_status(Component::Switch, body).unwrap();
        assert_eq!(r.watts, 18.4);
        assert_eq!(r.energy_wh, Some(12345.678));
        let err = br#"{"id":1,"src":"x","error":{"code":401,"message":"Unauthorized"}}"#;
        assert!(parse_status(Component::Switch, err)
            .unwrap_err()
            .contains("401"));
    }

    #[test]
    fn em1_status_and_energy() {
        let body = br#"{"id":1,"src":"shellypro3em-x","result":{"id":2,"current":0.3,"voltage":230.0,"act_power":31.5,"aprt_power":40.0,"pf":0.8,"freq":50.0}}"#;
        let r = parse_status(Component::Em1, body).unwrap();
        assert_eq!(r.watts, 31.5);
        assert_eq!(r.energy_wh, None);
        let data = br#"{"id":2,"src":"x","result":{"id":2,"total_act_energy":9876.5,"total_act_ret_energy":0.0}}"#;
        assert_eq!(em1_energy(data), Some(9876.5));
    }

    #[test]
    fn request_frame() {
        let f = request(7, "Switch.GetStatus", 0);
        let v: serde_json::Value = serde_json::from_slice(&f).unwrap();
        assert_eq!(v["method"], "Switch.GetStatus");
        assert_eq!(v["params"]["id"], 0);
        assert_eq!(v["id"], 7);
    }
}
