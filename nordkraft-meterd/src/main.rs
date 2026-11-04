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
/// Failed readings in a row before the process gives up and lets systemd
/// start a fresh one. A fresh process is the one sure way out of a stuck
/// Bluetooth session (the adapter, the system bus, a half-done exchange).
const MAX_FAILURES: u32 = 10;
/// Hanging up must not hold anything up for long.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(10);

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
    /// Send one RPC call to a node's meter and print the reply, for setup
    /// (e.g. `rpc dellopt1 WiFi.GetStatus`, or
    /// `rpc dellopt1 WiFi.SetConfig '{"config":{"sta":{"enable":false}}}'`).
    Rpc {
        node_id: String,
        method: String,
        /// JSON params; defaults to `{}`.
        params: Option<String>,
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
        Cmd::Rpc {
            node_id,
            method,
            params,
        } => rpc(&cli.config, &node_id, &method, params.as_deref()).await,
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

async fn rpc(path: &std::path::Path, node_id: &str, method: &str, params: Option<&str>) -> i32 {
    let Some(cfg) = load(path) else { return 2 };
    let Some(m) = cfg.meters.iter().find(|m| m.node_id == node_id) else {
        error!("no meter for node '{node_id}' in {}", path.display());
        return 2;
    };
    let params: serde_json::Value = match params.map(serde_json::from_str).transpose() {
        Ok(p) => p.unwrap_or_else(|| serde_json::json!({})),
        Err(e) => {
            error!("params are not JSON: {e}");
            return 2;
        }
    };
    let meter = match meters::build(m) {
        Ok(x) => x,
        Err(e) => {
            error!("{node_id}: {e}");
            return 2;
        }
    };
    let result = tokio::time::timeout(
        Duration::from_secs(cfg.read_timeout_seconds),
        meter.call(method, params),
    )
    .await;
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, meter.close()).await;
    match result {
        Ok(Ok(reply)) => {
            println!("{}", serde_json::to_string_pretty(&reply).unwrap());
            if reply.get("error").is_some() {
                1
            } else {
                0
            }
        }
        Ok(Err(e)) => {
            error!("{node_id}: {e}");
            1
        }
        Err(_) => {
            error!("{node_id}: timed out after {}s", cfg.read_timeout_seconds);
            1
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
        let result =
            tokio::time::timeout(Duration::from_secs(cfg.read_timeout_seconds), meter.read())
                .await;
        let _ = tokio::time::timeout(CLOSE_TIMEOUT, meter.close()).await;
        match result {
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
                println!("nothing with a name advertised in {seconds}s (is Bluetooth on, is the plug powered?)");
                1
            }
            Ok(list) => {
                let shellys = list.iter().filter(|s| s.shelly).count();
                for s in &list {
                    println!(
                        "{}  {:<34}  rssi {:>4}  {}",
                        s.address,
                        s.name.as_deref().unwrap_or("-"),
                        s.rssi,
                        if s.shelly { "shelly" } else { "" }
                    );
                }
                if shellys == 0 {
                    println!("no Shelly among them (is the app still connected to it? it only takes one connection)");
                    1
                } else {
                    0
                }
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
                        warn!("{}: reading failed ({failures} in a row): {e}", m.node_id);
                    }
                    Err(_) => {
                        failures += 1;
                        warn!("{}: reading timed out ({failures} in a row)", m.node_id);
                        // The exchange may be half done; start over next tick.
                        let _ = tokio::time::timeout(CLOSE_TIMEOUT, meter.close()).await;
                    }
                }
                if failures >= MAX_FAILURES {
                    error!(
                        "{}: {failures} readings failed in a row, giving up so systemd starts a fresh process",
                        m.node_id
                    );
                    return;
                }
            }
        }));
    }
    if tasks.is_empty() {
        error!("no meters in {}", path.display());
        return 2;
    }
    // One reader giving up ends the whole process (see MAX_FAILURES).
    let _ = futures::future::select_all(tasks).await;
    3
}
