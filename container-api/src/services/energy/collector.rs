//! Controller side: joins meter readings with the agents' CPU samples.
//!
//! A node's latest reading is kept in memory; when a CPU sample for that
//! node arrives, the reading (if fresh) is split between the containers
//! with [`super::attribution`], priced with the grid intensity of that
//! moment and written as minute rows. A node with no fresh reading simply
//! gets no rows: nothing is estimated in the dark.

use super::attribution::{self, Share};
use super::store::{self, MinuteRow, NodeMeta, TenantContainer};
use super::{ContainerCpuSample, NodePowerSample, SUBJECT_CONTAINER_CPU, SUBJECT_NODE_POWER};
use chrono::{DateTime, Duration, Utc};
use futures::StreamExt;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

/// A reading older than this is not used for attribution.
const READING_MAX_AGE_S: i64 = 150;
const META_CACHE_S: i64 = 300;
const TENANTS_CACHE_S: i64 = 60;

struct Cached<T> {
    at: DateTime<Utc>,
    value: T,
}

pub struct EnergyCollector {
    pool: PgPool,
    latest_power: Mutex<HashMap<String, NodePowerSample>>,
    /// When each node's agent last published a CPU sample, whether or not
    /// a reading was there to join it with. The one live signal the
    /// controller has per node, used by `/api/nodes`.
    last_cpu: Mutex<HashMap<String, DateTime<Utc>>>,
    meta: Mutex<HashMap<String, Cached<Option<NodeMeta>>>>,
    tenants: Mutex<HashMap<String, Cached<HashMap<String, TenantContainer>>>>,
}

impl EnergyCollector {
    pub fn new(pool: PgPool) -> Arc<Self> {
        Arc::new(Self {
            pool,
            latest_power: Mutex::new(HashMap::new()),
            last_cpu: Mutex::new(HashMap::new()),
            meta: Mutex::new(HashMap::new()),
            tenants: Mutex::new(HashMap::new()),
        })
    }

    pub async fn start(self: Arc<Self>, client: async_nats::Client) {
        match client.subscribe(SUBJECT_NODE_POWER).await {
            Ok(mut sub) => {
                let me = Arc::clone(&self);
                tokio::spawn(async move {
                    while let Some(msg) = sub.next().await {
                        match serde_json::from_slice::<NodePowerSample>(&msg.payload) {
                            Ok(s) => me.on_power(s).await,
                            Err(e) => warn!("Energy: bad node sample: {e}"),
                        }
                    }
                });
                info!("📥 Subscribed to: {SUBJECT_NODE_POWER}");
            }
            Err(e) => warn!("Energy: cannot subscribe to {SUBJECT_NODE_POWER}: {e}"),
        }
        match client.subscribe(SUBJECT_CONTAINER_CPU).await {
            Ok(mut sub) => {
                let me = Arc::clone(&self);
                tokio::spawn(async move {
                    while let Some(msg) = sub.next().await {
                        match serde_json::from_slice::<ContainerCpuSample>(&msg.payload) {
                            Ok(s) => me.on_cpu(s).await,
                            Err(e) => warn!("Energy: bad cpu sample: {e}"),
                        }
                    }
                });
                info!("📥 Subscribed to: {SUBJECT_CONTAINER_CPU}");
            }
            Err(e) => warn!("Energy: cannot subscribe to {SUBJECT_CONTAINER_CPU}: {e}"),
        }
    }

    async fn on_power(&self, s: NodePowerSample) {
        if !s.watts.is_finite() || s.watts < 0.0 {
            warn!("Energy: dropping reading {} W for {}", s.watts, s.node_id);
            return;
        }
        if let Err(e) = store::insert_node_sample(&self.pool, &s).await {
            warn!("Energy: cannot store reading for {}: {e}", s.node_id);
        }
        self.latest_power.lock().await.insert(s.node_id.clone(), s);
    }

    /// Per node: when its agent last published a CPU sample.
    pub async fn nodes_seen(&self) -> HashMap<String, DateTime<Utc>> {
        self.last_cpu.lock().await.clone()
    }

    /// Per node: when its meter last reported.
    pub async fn meters_seen(&self) -> HashMap<String, DateTime<Utc>> {
        self.latest_power
            .lock()
            .await
            .iter()
            .map(|(k, v)| (k.clone(), v.ts))
            .collect()
    }

    async fn on_cpu(&self, sample: ContainerCpuSample) {
        self.last_cpu
            .lock()
            .await
            .insert(sample.node_id.clone(), Utc::now());
        let reading = {
            let map = self.latest_power.lock().await;
            map.get(&sample.node_id).cloned()
        };
        let Some(reading) = reading else {
            debug!("Energy: no meter reading yet for {}", sample.node_id);
            return;
        };
        let age = (sample.ts - reading.ts).num_seconds().abs();
        if age > READING_MAX_AGE_S {
            debug!(
                "Energy: reading for {} is {age}s old, skipping interval",
                sample.node_id
            );
            return;
        }
        if sample.interval_s <= 0.0 || sample.interval_s > 600.0 {
            return;
        }

        let meta = self.node_meta(&sample.node_id).await;
        let tenants = self.tenants(&sample.node_id).await;
        let idle = meta.as_ref().and_then(|m| m.idle_watts);
        let area = meta
            .as_ref()
            .map(|m| m.price_area.clone())
            .unwrap_or_else(|| "DK1".to_string());

        // Every reported container takes part in the split, known to the
        // controller or not; only the known ones get rows.
        let shares: Vec<Share<'_>> = sample
            .containers
            .iter()
            .map(|c| Share {
                key: c.name.as_str(),
                cpu_usec: c.cpu_usec,
                vcpu: tenants.get(&c.name).map(|t| t.cpu_limit).unwrap_or(1.0),
            })
            .collect();
        let watts = attribution::attribute(reading.watts, idle, &shares);
        let method = attribution::method(idle);

        let grid = match store::grid_co2_at(&self.pool, &area, sample.ts).await {
            Ok(g) => g,
            Err(e) => {
                warn!("Energy: grid lookup failed: {e}");
                None
            }
        };

        let rows: Vec<MinuteRow> = sample
            .containers
            .iter()
            .zip(watts)
            .filter_map(|(c, w)| {
                let t = tenants.get(&c.name)?;
                let wh = w * sample.interval_s / 3600.0;
                Some(MinuteRow {
                    container_id: t.container_id.clone(),
                    ts: sample.ts,
                    user_id: t.user_id.clone(),
                    node_id: sample.node_id.clone(),
                    container_name: c.name.clone(),
                    interval_s: sample.interval_s,
                    watts: w,
                    wh,
                    co2_g: grid.as_ref().map(|g| wh / 1000.0 * g.g_per_kwh),
                    g_per_kwh: grid.as_ref().map(|g| g.g_per_kwh),
                    method,
                })
            })
            .collect();
        if rows.is_empty() {
            return;
        }
        if let Err(e) = store::insert_minutes(&self.pool, &rows).await {
            warn!(
                "Energy: cannot store {} rows for {}: {e}",
                rows.len(),
                sample.node_id
            );
        } else {
            debug!(
                "Energy: {} → {} containers, {:.1} W, grid {}",
                sample.node_id,
                rows.len(),
                reading.watts,
                grid.map(|g| format!("{:.0} g/kWh", g.g_per_kwh))
                    .unwrap_or_else(|| "n/a".into())
            );
        }
    }

    async fn node_meta(&self, node_id: &str) -> Option<NodeMeta> {
        let now = Utc::now();
        let mut cache = self.meta.lock().await;
        if let Some(c) = cache.get(node_id) {
            if now - c.at < Duration::seconds(META_CACHE_S) {
                return c.value.clone();
            }
        }
        let value = match store::node_meta(&self.pool, node_id).await {
            Ok(m) => m,
            Err(e) => {
                warn!("Energy: node lookup failed for {node_id}: {e}");
                None
            }
        };
        cache.insert(
            node_id.to_string(),
            Cached {
                at: now,
                value: value.clone(),
            },
        );
        value
    }

    async fn tenants(&self, node_id: &str) -> HashMap<String, TenantContainer> {
        let now = Utc::now();
        let mut cache = self.tenants.lock().await;
        if let Some(c) = cache.get(node_id) {
            if now - c.at < Duration::seconds(TENANTS_CACHE_S) {
                return c.value.clone();
            }
        }
        let value = match store::tenant_containers_on_node(&self.pool, node_id).await {
            Ok(m) => m,
            Err(e) => {
                warn!("Energy: container lookup failed for {node_id}: {e}");
                HashMap::new()
            }
        };
        cache.insert(
            node_id.to_string(),
            Cached {
                at: now,
                value: value.clone(),
            },
        );
        value
    }
}
