// services/reaper.rs — enforce per-container lifecycle on this node.
//
// Every tick the reaper reads the containers it manages and applies the
// three timers carried as labels (seconds; 0 or absent = never):
//
//   nordkraft.max_lifetime  running longer than this since creation → stop
//   nordkraft.auto_stop     no session activity for this long       → stop
//   nordkraft.auto_delete   stopped for this long                   → remove
//
// "Activity" for auto_stop is the later of the container's start time and
// the last exec/shell session on it (tracked in memory by ContainerdExec,
// so a node restart counts as fresh activity — a stop, never a surprise).
//
// Stop uses the runtime's graceful stop; remove goes through
// ContainerManager::remove_container so volumes, CNI state and the
// controller's DB (via ContainerDeleted over NATS, or directly in hybrid)
// are handled exactly as for a user-initiated `nordkraft rm`.

use crate::services::container_manager::ContainerManager;
use crate::services::nats_service::NatsService;
use crate::stream::containerd_exec::ContainerdExec;
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

pub const LABEL_AUTO_STOP: &str = "nordkraft.auto_stop";
pub const LABEL_AUTO_DELETE: &str = "nordkraft.auto_delete";
pub const LABEL_MAX_LIFETIME: &str = "nordkraft.max_lifetime";

pub struct Reaper {
    pub runtime_binary: String,
    pub container_manager: Arc<ContainerManager>,
    pub exec: Arc<ContainerdExec>,
    pub nats: Option<Arc<NatsService>>,
    pub pool: Option<sqlx::PgPool>,
    pub node_id: String,
    pub interval: Duration,
}

/// What the reaper needs to know about one container.
#[derive(Debug, Clone)]
pub struct Observed {
    pub id: String,
    pub name: String,
    pub running: bool,
    pub created: DateTime<Utc>,
    pub started: Option<DateTime<Utc>>,
    pub finished: Option<DateTime<Utc>>,
    pub labels: HashMap<String, String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Stop(&'static str),
    Remove(&'static str),
    Keep,
}

fn label_secs(labels: &HashMap<String, String>, key: &str) -> Option<u64> {
    labels.get(key)?.parse::<u64>().ok().filter(|s| *s > 0)
}

/// Pure decision: given what we observed and the last session activity,
/// what should happen now?
pub fn decide(c: &Observed, now: DateTime<Utc>, last_session: Option<DateTime<Utc>>) -> Action {
    let age = (now - c.created).num_seconds().max(0) as u64;
    if c.running {
        if let Some(max) = label_secs(&c.labels, LABEL_MAX_LIFETIME) {
            if age >= max {
                return Action::Stop("max_lifetime reached");
            }
        }
        if let Some(idle_limit) = label_secs(&c.labels, LABEL_AUTO_STOP) {
            let mut last = c.started.unwrap_or(c.created);
            if let Some(s) = last_session {
                if s > last {
                    last = s;
                }
            }
            let idle = (now - last).num_seconds().max(0) as u64;
            if idle >= idle_limit {
                return Action::Stop("idle (auto_stop)");
            }
        }
        Action::Keep
    } else {
        if let Some(delete_after) = label_secs(&c.labels, LABEL_AUTO_DELETE) {
            // A container that never ran has no finished_at; count from creation.
            let since = c.finished.unwrap_or(c.created);
            if (now - since).num_seconds().max(0) as u64 >= delete_after {
                return Action::Remove("stopped (auto_delete)");
            }
        }
        Action::Keep
    }
}

impl Reaper {
    pub fn spawn(self: Arc<Self>) {
        tokio::spawn(async move {
            // Let the runtime settle after boot before the first pass.
            tokio::time::sleep(Duration::from_secs(20)).await;
            let mut tick = tokio::time::interval(self.interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            info!("♻️  lifecycle reaper every {:?}", self.interval);
            loop {
                tick.tick().await;
                if let Err(e) = self.pass().await {
                    warn!("reaper pass failed: {e}");
                }
            }
        });
    }

    async fn pass(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let containers = self.observe().await?;
        let live: HashSet<String> = containers.iter().map(|c| c.id.clone()).collect();
        self.exec.retain_activity(&live);
        let now = Utc::now();

        for c in containers {
            let last_session = self.exec.last_activity(&c.id).map(|instant| {
                let ago = instant.elapsed();
                now - chrono::Duration::from_std(ago).unwrap_or_default()
            });
            match decide(&c, now, last_session) {
                Action::Keep => {}
                Action::Stop(reason) => {
                    info!("♻️  stopping {} — {}", c.name, reason);
                    let out = tokio::process::Command::new(&self.runtime_binary)
                        .args(["stop", &c.name])
                        .output()
                        .await?;
                    if !out.status.success() {
                        warn!(
                            "reaper stop {}: {}",
                            c.name,
                            String::from_utf8_lossy(&out.stderr)
                        );
                        continue;
                    }
                    self.event(&c, "lifecycle", &format!("auto-stopped: {reason}"))
                        .await;
                }
                Action::Remove(reason) => {
                    info!("♻️  removing {} — {}", c.name, reason);
                    let owner = c.labels.get("owner_pubkey").cloned().unwrap_or_default();
                    let slot = c
                        .labels
                        .get("user_slot")
                        .and_then(|s| s.parse::<i32>().ok());
                    let r = self
                        .container_manager
                        .remove_container(
                            &c.name,
                            &owner,
                            slot,
                            self.nats.as_deref(),
                            Some(&self.node_id),
                            true,
                        )
                        .await;
                    match r {
                        Ok(()) => {
                            if let Some(pool) = &self.pool {
                                let _ = crate::storage::mark_container_deleted(pool, &c.name).await;
                            }
                            self.event(&c, "lifecycle", &format!("auto-removed: {reason}"))
                                .await;
                        }
                        Err(e) => warn!("reaper remove {}: {e}", c.name),
                    }
                }
            }
        }
        Ok(())
    }

    async fn event(&self, c: &Observed, phase: &str, message: &str) {
        if let (Some(nats), Some(owner)) = (&self.nats, c.labels.get("owner_pubkey")) {
            let _ = nats
                .publish_deploy_event(&c.name, owner, phase, message, true)
                .await;
        } else {
            debug!("event {} {phase}: {message}", c.name);
        }
    }

    /// `nerdctl ps -a` for our containers, then one `inspect` for the lot.
    async fn observe(&self) -> Result<Vec<Observed>, Box<dyn std::error::Error + Send + Sync>> {
        let ps = tokio::process::Command::new(&self.runtime_binary)
            .args(["ps", "-a", "-q", "--filter", "label=managed_by=nordkraft"])
            .output()
            .await?;
        if !ps.status.success() {
            return Err(String::from_utf8_lossy(&ps.stderr).into());
        }
        let ids: Vec<String> = String::from_utf8_lossy(&ps.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut args = vec!["inspect".to_string()];
        args.extend(ids);
        let out = tokio::process::Command::new(&self.runtime_binary)
            .args(&args)
            .output()
            .await?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).into());
        }
        Ok(parse_inspect_output(&out.stdout))
    }
}

/// `nerdctl inspect a b` prints one JSON array per id, back to back, rather
/// than a single array — so read the output as a stream of documents.
pub fn parse_inspect_output(raw: &[u8]) -> Vec<Observed> {
    let mut out = Vec::new();
    let stream = serde_json::Deserializer::from_slice(raw).into_iter::<serde_json::Value>();
    for doc in stream.flatten() {
        match doc {
            serde_json::Value::Array(items) => out.extend(items.iter().filter_map(parse_inspect)),
            other => out.extend(parse_inspect(&other)),
        }
    }
    out
}

/// Pull what we need out of one `nerdctl inspect` document.
pub fn parse_inspect(v: &serde_json::Value) -> Option<Observed> {
    let id = v["Id"].as_str()?.to_string();
    let name = v["Name"].as_str()?.trim_start_matches('/').to_string();
    let state = &v["State"];
    let running =
        state["Running"].as_bool().unwrap_or(false) || state["Status"].as_str() == Some("running");
    let ts = |s: &serde_json::Value| -> Option<DateTime<Utc>> {
        let s = s.as_str()?;
        if s.is_empty() || s.starts_with("0001-") {
            return None;
        }
        DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|d| d.with_timezone(&Utc))
    };
    let created = ts(&v["Created"])?;
    let labels: HashMap<String, String> = v["Config"]["Labels"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    Some(Observed {
        id,
        name,
        running,
        created,
        started: ts(&state["StartedAt"]),
        finished: ts(&state["FinishedAt"]),
        labels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(running: bool, age_s: i64, labels: &[(&str, &str)]) -> (Observed, DateTime<Utc>) {
        let now = Utc::now();
        let created = now - chrono::Duration::seconds(age_s);
        let o = Observed {
            id: "abc".into(),
            name: "web".into(),
            running,
            created,
            started: Some(created),
            finished: if running {
                None
            } else {
                Some(now - chrono::Duration::seconds(age_s / 2))
            },
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        (o, now)
    }

    #[test]
    fn no_labels_means_keep() {
        let (o, now) = obs(true, 100_000, &[]);
        assert_eq!(decide(&o, now, None), Action::Keep);
        let (o, now) = obs(false, 100_000, &[]);
        assert_eq!(decide(&o, now, None), Action::Keep);
    }

    #[test]
    fn zero_means_never() {
        let (o, now) = obs(
            true,
            100_000,
            &[(LABEL_AUTO_STOP, "0"), (LABEL_MAX_LIFETIME, "0")],
        );
        assert_eq!(decide(&o, now, None), Action::Keep);
    }

    #[test]
    fn max_lifetime_stops() {
        let (o, now) = obs(true, 7200, &[(LABEL_MAX_LIFETIME, "3600")]);
        assert_eq!(decide(&o, now, None), Action::Stop("max_lifetime reached"));
        let (o, now) = obs(true, 1800, &[(LABEL_MAX_LIFETIME, "3600")]);
        assert_eq!(decide(&o, now, None), Action::Keep);
    }

    #[test]
    fn idle_counts_from_last_session() {
        let (o, now) = obs(true, 7200, &[(LABEL_AUTO_STOP, "900")]);
        assert_eq!(decide(&o, now, None), Action::Stop("idle (auto_stop)"));
        let recent = now - chrono::Duration::seconds(60);
        assert_eq!(decide(&o, now, Some(recent)), Action::Keep);
    }

    #[test]
    fn stopped_containers_are_deleted_after_delay() {
        let (o, now) = obs(false, 7200, &[(LABEL_AUTO_DELETE, "600")]);
        assert_eq!(
            decide(&o, now, None),
            Action::Remove("stopped (auto_delete)")
        );
        let (o, now) = obs(false, 100, &[(LABEL_AUTO_DELETE, "600")]);
        assert_eq!(decide(&o, now, None), Action::Keep);
        // Running containers are never deleted by auto_delete alone.
        let (o, now) = obs(true, 100_000, &[(LABEL_AUTO_DELETE, "1")]);
        assert_eq!(decide(&o, now, None), Action::Keep);
    }

    #[test]
    fn inspect_output_is_a_document_stream() {
        let raw = br#"[{"Id":"a","Name":"/one","Created":"2026-10-18T10:00:00Z","State":{"Running":true},"Config":{}}]
[{"Id":"b","Name":"/two","Created":"2026-10-18T10:00:00Z","State":{"Running":false},"Config":{}}]
"#;
        let got = parse_inspect_output(raw);
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].name, "two");
    }

    #[test]
    fn parse_nerdctl_inspect() {
        let v = serde_json::json!({
            "Id": "deadbeef", "Name": "/web", "Created": "2026-10-18T10:00:00Z",
            "State": {"Status": "exited", "Running": false,
                      "StartedAt": "2026-10-18T10:00:01Z", "FinishedAt": "2026-10-18T11:00:00Z"},
            "Config": {"Labels": {"nordkraft.auto_delete": "60", "owner_pubkey": "k"}}
        });
        let o = parse_inspect(&v).unwrap();
        assert_eq!(o.name, "web");
        assert!(!o.running);
        assert!(o.finished.is_some());
        assert_eq!(o.labels["nordkraft.auto_delete"], "60");
        // Go's zero time means "never"
        let v2 = serde_json::json!({"Id":"x","Name":"y","Created":"2026-10-18T10:00:00Z",
            "State":{"Running":true,"FinishedAt":"0001-01-01T00:00:00Z"},"Config":{}});
        assert!(parse_inspect(&v2).unwrap().finished.is_none());
    }
}
