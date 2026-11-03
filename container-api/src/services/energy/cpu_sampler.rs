//! Agent side: how much CPU time each container used in the last interval,
//! read from containerd's task metrics. For a Kata container that is the
//! sandbox cgroup, which includes the VMM, so the isolation's own cost lands
//! on the tenant that asked for it. Published on `nordkraft.energy.cpu`
//! once per interval; the controller turns it into watts.
//!
//! The sampler is read-only against containerd and publishes nothing until
//! it has two readings to difference. A task whose counter went backwards
//! (restart) counts its current value as the interval's usage.

use super::{ContainerCpu, ContainerCpuSample, SUBJECT_CONTAINER_CPU};
use crate::stream::containerd_exec::{with_ns, ContainerdExec, ExecError};
use containerd_client::services::v1::{
    containers_client::ContainersClient, tasks_client::TasksClient, ListContainersRequest,
    MetricsRequest,
};
use prost::Message;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// `io.containerd.cgroups.v2.Metrics`, only the field we read
/// (`cpu` = 2, `CPUStat.usage_usec` = 1). Unknown fields are skipped.
#[derive(Clone, PartialEq, Message)]
struct V2Metrics {
    #[prost(message, optional, tag = "2")]
    cpu: Option<V2Cpu>,
}
#[derive(Clone, PartialEq, Message)]
struct V2Cpu {
    #[prost(uint64, tag = "1")]
    usage_usec: u64,
}

/// `io.containerd.cgroups.v1.Metrics`: `cpu` = 3, `CPUStat.usage` = 1,
/// `CPUUsage.total` = 1 in nanoseconds.
#[derive(Clone, PartialEq, Message)]
struct V1Metrics {
    #[prost(message, optional, tag = "3")]
    cpu: Option<V1Cpu>,
}
#[derive(Clone, PartialEq, Message)]
struct V1Cpu {
    #[prost(message, optional, tag = "1")]
    usage: Option<V1Usage>,
}
#[derive(Clone, PartialEq, Message)]
struct V1Usage {
    #[prost(uint64, tag = "1")]
    total: u64,
}

/// Cumulative CPU microseconds out of a task's metrics `Any`.
pub fn cpu_usec_from_any(type_url: &str, value: &[u8]) -> Option<u64> {
    if type_url.ends_with("cgroups.v2.Metrics") {
        V2Metrics::decode(value).ok()?.cpu.map(|c| c.usage_usec)
    } else if type_url.ends_with("cgroups.v1.Metrics") {
        V1Metrics::decode(value)
            .ok()?
            .cpu?
            .usage
            .map(|u| u.total / 1000)
    } else {
        None
    }
}

pub struct CpuSampler {
    pub exec: Arc<ContainerdExec>,
    pub nats: async_nats::Client,
    pub node_id: String,
    pub interval: Duration,
}

impl CpuSampler {
    pub fn spawn(self) {
        info!(
            "⚡ Energy: CPU sampler every {}s on {}",
            self.interval.as_secs(),
            self.node_id
        );
        tokio::spawn(async move { self.run().await });
    }

    async fn run(self) {
        let mut tick = tokio::time::interval(self.interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut prev: Option<(Instant, HashMap<String, u64>)> = None;
        loop {
            tick.tick().await;
            let now = Instant::now();
            let cur = match self.read().await {
                Ok(c) => c,
                Err(e) => {
                    warn!("Energy: task metrics unavailable: {e}");
                    prev = None;
                    continue;
                }
            };
            if let Some((t0, old)) = prev.take() {
                let interval_s = now.duration_since(t0).as_secs_f64();
                let containers: Vec<ContainerCpu> = cur
                    .iter()
                    .filter_map(|(id, (name, usec))| {
                        let before = *old.get(id)?;
                        let delta = if *usec >= before {
                            usec - before
                        } else {
                            *usec
                        };
                        Some(ContainerCpu {
                            container_id: id.clone(),
                            name: name.clone(),
                            cpu_usec: delta,
                        })
                    })
                    .collect();
                if !containers.is_empty() {
                    let sample = ContainerCpuSample {
                        node_id: self.node_id.clone(),
                        ts: chrono::Utc::now(),
                        interval_s,
                        containers,
                    };
                    match serde_json::to_vec(&sample) {
                        Ok(payload) => {
                            if let Err(e) = self
                                .nats
                                .publish(SUBJECT_CONTAINER_CPU, payload.into())
                                .await
                            {
                                warn!("Energy: publish failed: {e}");
                            }
                        }
                        Err(e) => warn!("Energy: serialize failed: {e}"),
                    }
                }
            }
            prev = Some((
                now,
                cur.into_iter().map(|(id, (_, usec))| (id, usec)).collect(),
            ));
        }
    }

    /// containerd id → (nerdctl name, cumulative CPU µs) for every named
    /// container with a live task. Unnamed containers (nothing nerdctl made)
    /// are left out; they are not anyone's.
    async fn read(&self) -> Result<HashMap<String, (String, u64)>, ExecError> {
        let ch = self.exec.grpc_channel().await?;
        let ns = self.exec.namespace();

        let mut containers = ContainersClient::new(ch.clone());
        let names: HashMap<String, String> = containers
            .list(with_ns(ListContainersRequest { filters: vec![] }, ns))
            .await?
            .into_inner()
            .containers
            .into_iter()
            .filter_map(|c| {
                let name = c.labels.get("nerdctl/name")?.clone();
                Some((c.id, name))
            })
            .collect();

        let mut tasks = TasksClient::new(ch);
        let metrics = tasks
            .metrics(with_ns(MetricsRequest { filters: vec![] }, ns))
            .await?
            .into_inner()
            .metrics;

        let mut out = HashMap::new();
        for m in metrics {
            let Some(name) = names.get(&m.id) else {
                continue;
            };
            let Some(any) = m.data else { continue };
            match cpu_usec_from_any(&any.type_url, &any.value) {
                Some(usec) => {
                    out.insert(m.id, (name.clone(), usec));
                }
                None => debug!("Energy: no cpu stat in {} for {}", any.type_url, m.id),
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_v2_and_v1_cpu_time() {
        let v2 = V2Metrics {
            cpu: Some(V2Cpu {
                usage_usec: 64_792_538,
            }),
        }
        .encode_to_vec();
        assert_eq!(
            cpu_usec_from_any("io.containerd.cgroups.v2.Metrics", &v2),
            Some(64_792_538)
        );
        let v1 = V1Metrics {
            cpu: Some(V1Cpu {
                usage: Some(V1Usage { total: 5_000_000 }),
            }),
        }
        .encode_to_vec();
        assert_eq!(
            cpu_usec_from_any("io.containerd.cgroups.v1.Metrics", &v1),
            Some(5_000)
        );
        assert_eq!(cpu_usec_from_any("something.else", &v2), None);
    }
}
