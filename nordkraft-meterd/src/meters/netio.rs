//! NETIO PowerBOX / PowerPDU: `GET /netio.json` with basic auth, one
//! `Outputs[]` entry per outlet with `Load` (W) and `Energy` (Wh).

use super::{Meter, ReadFuture, Reading};
use crate::config::MeterConfig;
use serde::Deserialize;

pub struct Netio {
    http: reqwest::Client,
    url: String,
    outlet: u32,
    auth: Option<(String, String)>,
}

#[derive(Deserialize)]
struct Doc {
    #[serde(rename = "Outputs", default)]
    outputs: Vec<Output>,
}

#[derive(Deserialize)]
struct Output {
    #[serde(rename = "ID")]
    id: u32,
    #[serde(rename = "Load")]
    load: Option<f64>,
    #[serde(rename = "Energy")]
    energy: Option<f64>,
}

impl Netio {
    pub fn new(cfg: &MeterConfig) -> Result<Self, String> {
        let base = cfg.url.clone().ok_or("netio needs url")?;
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            http,
            url: format!("{}/netio.json", base.trim_end_matches('/')),
            outlet: cfg.outlet.unwrap_or(1),
            auth: match (&cfg.username, &cfg.password) {
                (Some(u), Some(p)) => Some((u.clone(), p.clone())),
                _ => None,
            },
        })
    }
}

pub fn parse(body: &[u8], outlet: u32) -> Result<Reading, String> {
    let doc: Doc = serde_json::from_slice(body).map_err(|e| format!("bad netio.json: {e}"))?;
    let o = doc
        .outputs
        .iter()
        .find(|o| o.id == outlet)
        .ok_or_else(|| format!("outlet {outlet} not in netio.json"))?;
    Ok(Reading {
        watts: o.load.ok_or("outlet has no Load (not a metered model?)")?,
        energy_wh: o.energy,
    })
}

impl Meter for Netio {
    fn source(&self) -> &'static str {
        "netio"
    }

    fn read(&self) -> ReadFuture<'_> {
        Box::pin(async move {
            let mut req = self.http.get(&self.url);
            if let Some((u, p)) = &self.auth {
                req = req.basic_auth(u, Some(p));
            }
            let resp = req.send().await.map_err(|e| e.to_string())?;
            if !resp.status().is_success() {
                return Err(format!("HTTP {}", resp.status()));
            }
            let body = resp.bytes().await.map_err(|e| e.to_string())?;
            parse(&body, self.outlet)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_outlet() {
        let body = br#"{"Agent":{"Model":"4KF"},"Outputs":[{"ID":1,"Name":"dellopt1","State":1,"Load":27.3,"Energy":1234},{"ID":2,"Name":"rasp2","State":1,"Load":5.1,"Energy":99}]}"#;
        let r = parse(body, 2).unwrap();
        assert_eq!(r.watts, 5.1);
        assert_eq!(r.energy_wh, Some(99.0));
        assert!(parse(body, 3).is_err());
    }
}
