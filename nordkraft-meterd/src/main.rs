//! nordkraft-meterd: reads power meters and tells the controller what each
//! node draws.
//!
//! Runs wherever the meters are reachable: on the controller for a
//! Bluetooth plug, on any LAN host for a PDU. Every `interval_seconds` it
//! reads each meter in `meters.toml` and publishes one JSON sample per node
//! on the NATS subject `nordkraft.energy.node`:
//!
//! ```json
//! {"node_id":"ry-optiplex-1","ts":"2026-11-02T21:40:00Z","watts":27.3,"energy_wh":12345.6,"source":"shelly-ble"}
//! ```
//!
//! A failed reading publishes nothing. The controller then stops
//! attributing for that node until readings resume, so the numbers the
//! customer sees are always backed by a real reading.

mod config;
mod meters;
mod shelly_rpc;

use chrono::Utc;
use clap::{Parser, Subcommand};
use config::Config;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};

const SUBJECT: &str = "nordkraft.energy.node";

#[derive(Parser)]
#[command(name = "nordkraft-meterd", version, about)]
struct Cli {
    /// Meter configuration.
    #[arg(long, default_value = "/etc/nordkraft/meters.toml", global = true)]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Read every meter on its interval and publish to NATS (the default).
    Run,
    /// Read each meter once (or one node's) and print the samples. No NATS.
    Probe {
        /// Only this node's meter.
        node_id: Option<String>,
    },
    /// List Bluetooth devices advertising the Shelly RPC service.
    Scan {
        #[arg(long, default_value = "10")]
        seconds: u64,
    },
}

/// Mirrors `container-api::services::energy::NodePowerSample`.
#[derive(Serialize)]
struct Sample<'a> {
    node_id: &'a str,
    ts: chrono::DateTime<Utc>,
    watts: f64,
    energy_wh: Option<f64>,
    source: &'static str,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();
    let code = match cli.cmd.unwrap_or(Cmd::Run) {
        Cmd::Run => run(&cli.config).await,
        Cmd::Probe { node_id } => probe(&cli.config, node_id.as_deref()).await,
        Cmd::Scan { seconds } => scan(seconds).await,
    };
    std::process::exit(code);
}

fn load(path: &std::path::Path) -> Option<Config> {
    match Config::load(path) {
        Ok(c) => Some(c),
        Err(e) => {
            error!("{e}");
            None
        }
    }
}

async fn probe(path: &std::path::Path, only: Option<&str>) -> i32 {
    let Some(cfg) = load(path) else { return 2 };
    let mut failed = 0;
    for m in cfg
        .meters
        .iter()
        .filter(|m| only.is_none_or(|n| n == m.node_id))
    {
        let meter = match meters::build(m) {
            Ok(x) => x,
            Err(e) => {
                error!("{}: {e}", m.node_id);
                failed += 1;
                continue;
            }
        };
        let started = std::time::Instant::now();
        match tokio::time::timeout(Duration::from_secs(cfg.read_timeout_seconds), meter.read())
            .await
        {
            Ok(Ok(r)) => {
                let s = Sample {
                    node_id: &m.node_id,
                    ts: Utc::now(),
                    watts: r.watts,
                    energy_wh: r.energy_wh,
                    source: meter.source(),
                };
                println!("{}", serde_json::to_string(&s).unwrap());
                info!(
                    "{}: {:.1} W via {} in {:.1}s",
                    m.node_id,
                    r.watts,
                    meter.source(),
                    started.elapsed().as_secs_f64()
                );
            }
            Ok(Err(e)) => {
                error!("{}: {e}", m.node_id);
                failed += 1;
            }
            Err(_) => {
                error!(
                    "{}: timed out after {}s",
                    m.node_id, cfg.read_timeout_seconds
                );
                failed += 1;
            }
        }
    }
    if failed > 0 {
        1
    } else {
        0
    }
}

async fn scan(seconds: u64) -> i32 {
    #[cfg(feature = "ble")]
    {
        match meters::shelly_ble::scan(seconds).await {
            Ok(list) if list.is_empty() => {
                println!("nothing advertising the Shelly RPC service in {seconds}s (is BLE RPC enabled on the device?)");
                1
            }
            Ok(list) => {
                for (addr, name, rssi) in list {
                    println!(
                        "{addr}  {}  rssi {rssi}",
                        name.unwrap_or_else(|| "-".into())
                    );
                }
                0
            }
            Err(e) => {
                error!("{e}");
                1
            }
        }
    }
    #[cfg(not(feature = "ble"))]
    {
        let _ = seconds;
        error!("this build has no Bluetooth support (feature `ble`)");
        1
    }
}

async fn run(path: &std::path::Path) -> i32 {
    let Some(cfg) = load(path) else { return 2 };
    let nats = match async_nats::connect(&cfg.nats_url).await {
        Ok(c) => c,
        Err(e) => {
            error!("NATS {}: {e}", cfg.nats_url);
            return 2;
        }
    };
    info!(
        "⚡ nordkraft-meterd: {} meter(s), every {}s → {} on {}",
        cfg.meters.len(),
        cfg.interval_seconds,
        SUBJECT,
        cfg.nats_url
    );
    let mut tasks = Vec::new();
    for m in cfg.meters.clone() {
        let meter: Arc<Box<dyn meters::Meter>> = match meters::build(&m) {
            Ok(x) => Arc::new(x),
            Err(e) => {
                error!("{}: {e}", m.node_id);
                return 2;
            }
        };
        let nats = nats.clone();
        let interval = Duration::from_secs(cfg.interval_seconds.max(10));
        let read_timeout = Duration::from_secs(cfg.read_timeout_seconds);
        tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut failures: u32 = 0;
            loop {
                tick.tick().await;
                let result = tokio::time::timeout(read_timeout, meter.read()).await;
                match result {
                    Ok(Ok(r)) => {
                        if failures >= 3 {
                            info!("{}: readings resumed ({:.1} W)", m.node_id, r.watts);
                        }
                        failures = 0;
                        let s = Sample {
                            node_id: &m.node_id,
                            ts: Utc::now(),
                            watts: r.watts,
                            energy_wh: r.energy_wh,
                            source: meter.source(),
                        };
                        let payload = serde_json::to_vec(&s).unwrap();
                        if let Err(e) = nats.publish(SUBJECT, payload.into()).await {
                            warn!("{}: publish failed: {e}", m.node_id);
                        } else {
                            tracing::debug!("{}: {:.1} W", m.node_id, r.watts);
                        }
                    }
                    Ok(Err(e)) => {
                        failures += 1;
                        if failures <= 3 || failures.is_multiple_of(30) {
                            warn!("{}: reading failed ({failures} in a row): {e}", m.node_id);
                        }
                    }
                    Err(_) => {
                        failures += 1;
                        if failures <= 3 || failures.is_multiple_of(30) {
                            warn!("{}: reading timed out ({failures} in a row)", m.node_id);
                        }
                    }
                }
            }
        }));
    }
    let _ = futures::future::join_all(tasks).await;
    0
}
