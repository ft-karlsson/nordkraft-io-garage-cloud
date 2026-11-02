//! Shelly Gen2/Gen3 over the LAN: `POST /rpc` with a JSON-RPC frame. With
//! device authentication on, the device answers 401 with a digest
//! challenge (SHA-256, user `admin`) and the request is repeated with the
//! `Authorization` header (RFC 7616).

use super::{Meter, ReadFuture};
use crate::config::MeterConfig;
use crate::shelly_rpc::{self, Component};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};

pub struct ShellyHttp {
    http: reqwest::Client,
    url: String,
    password: Option<String>,
    component: Component,
    component_id: u32,
    seq: AtomicU64,
}

impl ShellyHttp {
    pub fn new(cfg: &MeterConfig) -> Result<Self, String> {
        let base = cfg.url.clone().ok_or("shelly-http needs url")?;
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            http,
            url: format!("{}/rpc", base.trim_end_matches('/')),
            password: cfg.password.clone(),
            component: Component::parse(&cfg.component).ok_or("bad component")?,
            component_id: cfg.component_id,
            seq: AtomicU64::new(1),
        })
    }

    async fn call(&self, method: &str) -> Result<Vec<u8>, String> {
        let id = self.seq.fetch_add(1, Ordering::Relaxed);
        let frame = shelly_rpc::request(id, method, self.component_id);
        let resp = self
            .http
            .post(&self.url)
            .header("content-type", "application/json")
            .body(frame.clone())
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if resp.status() != reqwest::StatusCode::UNAUTHORIZED {
            return finish(resp).await;
        }
        let Some(password) = &self.password else {
            return Err("device requires authentication; set `password`".into());
        };
        let challenge = resp
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .ok_or("401 without a challenge")?
            .to_string();
        let header = digest_header(&challenge, "admin", password, "POST", "/rpc")?;
        let resp = self
            .http
            .post(&self.url)
            .header("content-type", "application/json")
            .header("authorization", header)
            .body(frame)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        finish(resp).await
    }
}

async fn finish(resp: reqwest::Response) -> Result<Vec<u8>, String> {
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| e.to_string())
}

impl Meter for ShellyHttp {
    fn source(&self) -> &'static str {
        "shelly-http"
    }

    fn read(&self) -> ReadFuture<'_> {
        Box::pin(async move {
            let body = self.call(self.component.status_method()).await?;
            let mut reading = shelly_rpc::parse_status(self.component, &body)?;
            if self.component == Component::Em1 {
                if let Ok(data) = self.call("EM1Data.GetStatus").await {
                    reading.energy_wh = shelly_rpc::em1_energy(&data);
                }
            }
            Ok(reading)
        })
    }
}

/// Parse `Digest k="v", k=v, ...` into pairs.
fn parse_challenge(header: &str) -> Result<Vec<(String, String)>, String> {
    let rest = header
        .strip_prefix("Digest ")
        .ok_or_else(|| format!("not a digest challenge: {header}"))?;
    let mut out = Vec::new();
    for part in rest.split(',') {
        let Some((k, v)) = part.trim().split_once('=') else {
            continue;
        };
        out.push((k.trim().to_string(), v.trim().trim_matches('"').to_string()));
    }
    Ok(out)
}

fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// RFC 7616 `Authorization` value for a SHA-256 digest challenge with
/// `qop=auth`, which is what Shelly Gen2+ sends.
pub fn digest_header(
    challenge: &str,
    user: &str,
    password: &str,
    method: &str,
    uri: &str,
) -> Result<String, String> {
    let fields = parse_challenge(challenge)?;
    let get = |k: &str| {
        fields
            .iter()
            .find(|(f, _)| f.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    };
    let realm = get("realm").ok_or("challenge has no realm")?;
    let nonce = get("nonce").ok_or("challenge has no nonce")?;
    let algorithm = get("algorithm").unwrap_or("SHA-256");
    if !algorithm.eq_ignore_ascii_case("SHA-256") {
        return Err(format!("unsupported digest algorithm {algorithm}"));
    }
    let cnonce = hex::encode(rand::random::<[u8; 8]>());
    let nc = "00000001";
    let ha1 = sha256_hex(&format!("{user}:{realm}:{password}"));
    let ha2 = sha256_hex(&format!("{method}:{uri}"));
    let response = sha256_hex(&format!("{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}"));
    Ok(format!(
        "Digest username=\"{user}\", realm=\"{realm}\", nonce=\"{nonce}\", uri=\"{uri}\", \
         qop=auth, nc={nc}, cnonce=\"{cnonce}\", response=\"{response}\", algorithm=SHA-256"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_header_has_the_parts() {
        let h = digest_header(
            "Digest qop=\"auth\", realm=\"shellyplugsg3-aabbcc\", nonce=\"1793650000\", algorithm=SHA-256",
            "admin",
            "secret",
            "POST",
            "/rpc",
        )
        .unwrap();
        assert!(h.starts_with("Digest username=\"admin\", realm=\"shellyplugsg3-aabbcc\""));
        assert!(h.contains("nonce=\"1793650000\""));
        assert!(h.contains("algorithm=SHA-256"));
        let response = h
            .split("response=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        assert_eq!(response.len(), 64);
        assert!(digest_header("Basic realm=x", "a", "b", "POST", "/rpc").is_err());
    }
}
