//! Grid CO₂ intensity from Energinet's Energi Data Service.
//!
//! `CO2Emis` is the measured 5-minute intensity per price area in gram CO₂
//! per kWh, published a few minutes behind the clock; `CO2EmisProg` is the
//! forecast for the hours ahead. Both are public, no key. The poller keeps
//! the last hours of measured values and the next hours of forecast in
//! `grid_co2`; the collector picks the measured value when it has one and
//! the forecast when the measured one is late.

use super::store;
use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use serde::Deserialize;
use sqlx::PgPool;
use tracing::{debug, info, warn};

pub const DEFAULT_API_URL: &str = "https://api.energidataservice.dk";

#[derive(Deserialize)]
struct Response {
    #[serde(default)]
    records: Vec<Record>,
}

#[derive(Deserialize)]
struct Record {
    #[serde(rename = "Minutes5UTC")]
    minutes5_utc: String,
    #[serde(rename = "CO2Emission")]
    co2_emission: Option<f64>,
}

pub struct EnerginetPoller {
    pool: PgPool,
    http: reqwest::Client,
    base: String,
    default_area: String,
}

impl EnerginetPoller {
    pub fn new(pool: PgPool, base: String, default_area: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .user_agent("nordkraft-controller energy (https://nordkraft.io)")
            .build()
            .expect("reqwest client");
        Self {
            pool,
            http,
            base: base.trim_end_matches('/').to_string(),
            default_area,
        }
    }

    pub fn spawn(self, every: std::time::Duration) {
        info!(
            "⚡ Energy: Energinet CO₂ poller every {}s ({})",
            every.as_secs(),
            self.base
        );
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                self.tick().await;
            }
        });
    }

    async fn tick(&self) {
        let mut areas = store::price_areas(&self.pool).await.unwrap_or_default();
        if !areas.iter().any(|a| a == &self.default_area) {
            areas.push(self.default_area.clone());
        }
        for area in areas {
            match self.fetch_measured(&area).await {
                Ok(rows) => match store::upsert_grid_co2(&self.pool, &area, false, &rows).await {
                    Ok(_) => debug!("Energinet {area}: {} measured rows", rows.len()),
                    Err(e) => warn!("Energinet {area}: store failed: {e}"),
                },
                Err(e) => warn!("Energinet {area}: CO2Emis fetch failed: {e}"),
            }
            match self.fetch_forecast(&area).await {
                Ok(rows) => match store::upsert_grid_co2(&self.pool, &area, true, &rows).await {
                    Ok(_) => debug!("Energinet {area}: {} forecast rows", rows.len()),
                    Err(e) => warn!("Energinet {area}: store failed: {e}"),
                },
                Err(e) => warn!("Energinet {area}: CO2EmisProg fetch failed: {e}"),
            }
        }
    }

    /// Last two hours of measured values.
    async fn fetch_measured(&self, area: &str) -> Result<Vec<(DateTime<Utc>, f64)>, String> {
        self.fetch(
            "CO2Emis",
            area,
            &[
                ("limit", "24".to_string()),
                ("sort", "Minutes5UTC desc".to_string()),
            ],
        )
        .await
    }

    /// From an hour ago through the next six hours of forecast.
    async fn fetch_forecast(&self, area: &str) -> Result<Vec<(DateTime<Utc>, f64)>, String> {
        let start = (Utc::now() - Duration::hours(1))
            .format("%Y-%m-%dT%H:%M")
            .to_string();
        self.fetch(
            "CO2EmisProg",
            area,
            &[
                ("limit", "84".to_string()),
                ("sort", "Minutes5UTC asc".to_string()),
                ("start", start),
            ],
        )
        .await
    }

    async fn fetch(
        &self,
        dataset: &str,
        area: &str,
        extra: &[(&str, String)],
    ) -> Result<Vec<(DateTime<Utc>, f64)>, String> {
        let url = format!("{}/dataset/{dataset}", self.base);
        let filter = format!("{{\"PriceArea\":\"{area}\"}}");
        let mut query: Vec<(&str, String)> = vec![("filter", filter)];
        query.extend(extra.iter().cloned());
        let resp = self
            .http
            .get(&url)
            .query(&query)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }
        let body: Response = resp.json().await.map_err(|e| e.to_string())?;
        Ok(parse_records(body.records))
    }
}

fn parse_records(records: Vec<Record>) -> Vec<(DateTime<Utc>, f64)> {
    records
        .into_iter()
        .filter_map(|r| {
            let g = r.co2_emission?;
            let ts = NaiveDateTime::parse_from_str(&r.minutes5_utc, "%Y-%m-%dT%H:%M:%S")
                .ok()?
                .and_utc();
            Some((ts, g))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_energinet_rows() {
        let body: Response = serde_json::from_str(
            r#"{"total":2,"dataset":"CO2Emis","records":[
                {"Minutes5UTC":"2026-11-02T20:40:00","Minutes5DK":"2026-11-02T21:40:00","PriceArea":"DK1","CO2Emission":43.0},
                {"Minutes5UTC":"2026-11-02T20:35:00","PriceArea":"DK1","CO2Emission":null}
            ]}"#,
        )
        .unwrap();
        let rows = parse_records(body.records);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, 43.0);
        assert_eq!(rows[0].0.to_rfc3339(), "2026-11-02T20:40:00+00:00");
    }
}
