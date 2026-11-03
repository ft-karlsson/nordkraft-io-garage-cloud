//! Energy transparency: watts and CO₂ per container.
//!
//! Three producers, one consumer:
//!
//! - a meter reader (`nordkraft-meterd`, or anything else that can talk to a
//!   meter) publishes what a node draws at the wall on `nordkraft.energy.node`;
//! - every agent publishes how much CPU time each of its containers used in
//!   the last interval on `nordkraft.energy.cpu` ([`cpu_sampler`]);
//! - the controller joins the two per node and interval ([`collector`],
//!   [`attribution`]), prices the result with Energinet's grid intensity
//!   ([`energinet`]), stores minute rows and rolls them up hourly ([`rollup`]).
//!
//! The wire formats below are plain JSON structs rather than `NatsMessage`
//! variants so the meter daemon needs nothing from this crate. The meter
//! sits wherever it can reach the hardware: on the controller for a
//! Bluetooth plug, on any host for a LAN PDU, on the node itself for an
//! estimator. The `node_id` in the sample is what ties it to a node.

pub mod attribution;
pub mod collector;
pub mod cpu_sampler;
pub mod energinet;
pub mod rollup;
pub mod store;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Meter readers publish here: one [`NodePowerSample`] per reading.
pub const SUBJECT_NODE_POWER: &str = "nordkraft.energy.node";
/// Agents publish here: one [`ContainerCpuSample`] per interval.
pub const SUBJECT_CONTAINER_CPU: &str = "nordkraft.energy.cpu";

/// What a node drew at the wall when the meter was read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodePowerSample {
    pub node_id: String,
    pub ts: DateTime<Utc>,
    /// Instantaneous active power, watts.
    pub watts: f64,
    /// The meter's lifetime energy counter in watt-hours, when it has one.
    /// Counter deltas are the honest kWh; `watts` sampled once a minute
    /// misses what happens between readings.
    #[serde(default)]
    pub energy_wh: Option<f64>,
    /// Which reader produced it: `shelly-ble`, `shelly-http`, `netio`, ...
    pub source: String,
}

/// CPU time one container used in the interval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerCpu {
    /// containerd id.
    pub container_id: String,
    /// nerdctl name, what the customer calls it.
    pub name: String,
    /// Microseconds of CPU in this interval. For Kata that is the sandbox
    /// cgroup, so the VMM's own work lands on the tenant that caused it.
    pub cpu_usec: u64,
}

/// CPU time of every named container on a node over one interval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerCpuSample {
    pub node_id: String,
    /// End of the interval.
    pub ts: DateTime<Utc>,
    pub interval_s: f64,
    pub containers: Vec<ContainerCpu>,
}
