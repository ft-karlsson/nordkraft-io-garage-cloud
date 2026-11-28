//! Per-volume disk limits with XFS project quotas.
//!
//! Every persistent volume is one XFS project: its directory carries a
//! project id with the inherit flag, and the project has a hard block limit
//! of the size the customer asked for. The kernel enforces the limit; the
//! agent only sets numbers, so nothing has to stay running. Both the id and
//! the limit live inside the filesystem and survive reboots. `df` inside the
//! container shows the limit as the filesystem size, because XFS reports a
//! project quota through statfs and virtiofsd passes statfs through.
//!
//! This needs `/var/lib/nordkraft` on an XFS filesystem mounted with
//! `prjquota` (see setup/VOLUME_QUOTAS.md). Anywhere else the backend is
//! `Unavailable`: `VOLUME_QUOTA=auto` (default) then warns and deploys the
//! volume without a limit, `required` refuses the deploy, `off` says nothing.
//!
//! A sidecar `quota.json` next to each volume's `data` directory (outside
//! the bind mount, so the tenant cannot touch it) records the project id and
//! the size. The start-up reconcile re-applies both from it, which is what
//! brings quotas back after a copy to a new filesystem: rsync and tar do not
//! carry project ids.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tracing::{info, warn};

/// Size given to a volume whose request carried none (same default as the
/// controller's `volume_size`).
pub const DEFAULT_SIZE_MB: u64 = 1024;

/// Name of the sidecar file, in the volume's parent directory.
pub const SIDECAR: &str = "quota.json";

/// How the agent treats a node without XFS project quotas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QuotaMode {
    /// Never touch quotas (dev nodes, tests).
    Off,
    /// Enforce where the filesystem allows it, warn where it does not.
    Auto,
    /// Refuse to create a volume that cannot be limited.
    Required,
}

impl std::str::FromStr for QuotaMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "off" | "false" | "0" => Ok(QuotaMode::Off),
            "auto" | "" => Ok(QuotaMode::Auto),
            "required" | "strict" | "true" => Ok(QuotaMode::Required),
            other => Err(format!(
                "Unknown VOLUME_QUOTA '{}' (expected 'auto', 'required' or 'off')",
                other
            )),
        }
    }
}

/// What the start-up probe found under the data directory.
#[derive(Debug, Clone)]
pub enum Backend {
    /// XFS with project quotas, managed through this mount point.
    Xfs { mount: PathBuf },
    /// No enforcement possible; the reason is for the log.
    Unavailable { reason: String },
}

/// Sidecar contents: everything needed to re-apply a volume's quota.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuotaRecord {
    pub project_id: u32,
    pub size_mb: u64,
    pub container: String,
    pub applied_at: DateTime<Utc>,
}

// ============= PURE HELPERS (unit tested) =============

/// "1g" → 1024, "512m" → 512, "2048" → 2048 (bare numbers are MB, as the
/// controller reads them). None for anything that is not a positive size.
pub fn parse_size_mb(spec: &str) -> Option<u64> {
    let s = spec.trim().to_lowercase();
    let (num, mult) = if let Some(n) = s.strip_suffix("gb").or_else(|| s.strip_suffix('g')) {
        (n, 1024)
    } else if let Some(n) = s.strip_suffix("mb").or_else(|| s.strip_suffix('m')) {
        (n, 1)
    } else if let Some(n) = s.strip_suffix("tb").or_else(|| s.strip_suffix('t')) {
        (n, 1024 * 1024)
    } else {
        (s.as_str(), 1)
    };
    let n: u64 = num.trim().parse().ok()?;
    (n > 0).then_some(n.checked_mul(mult)?)
}

/// Deterministic project id for a volume: FNV-1a over the container name,
/// kept in 1..2^31 so it fits every tool's idea of a project id and is never
/// 0 (which means "no project"). Container names are unique platform-wide.
pub fn project_id_for(container: &str) -> u32 {
    const OFFSET: u32 = 0x811c_9dc5;
    const PRIME: u32 = 0x0100_0193;
    let mut h = OFFSET;
    for b in container.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(PRIME);
    }
    let id = h & 0x7fff_ffff;
    if id == 0 {
        1
    } else {
        id
    }
}

/// `findmnt -n -T <path> -o TARGET,FSTYPE,OPTIONS` → (target, fstype, options).
pub fn parse_findmnt(line: &str) -> Option<(String, String, String)> {
    let mut parts = line.split_whitespace();
    let target = parts.next()?.to_string();
    let fstype = parts.next()?.to_string();
    let options = parts.next().unwrap_or("").to_string();
    Some((target, fstype, options))
}

/// XFS mounted with project quotas enforced? The kernel spells the option
/// `prjquota` in /proc/mounts; `pquota` is the mount(8) alias. `pqnoenforce`
/// only accounts and is deliberately not accepted.
pub fn supports_project_quota(fstype: &str, options: &str) -> bool {
    fstype == "xfs" && options.split(',').any(|o| o == "prjquota" || o == "pquota")
}

/// `xfs_io -r -c lsproj <dir>` prints `projid = 42`.
pub fn parse_lsproj(output: &str) -> Option<u32> {
    output
        .lines()
        .find_map(|l| l.trim().strip_prefix("projid = "))
        .and_then(|v| v.trim().parse().ok())
}

/// `xfs_quota -x -c "quota -p -N -v -b <id>" <mount>` prints one line,
/// `#<id> <used KiB> <soft> <hard> ...`, or nothing when the project has
/// never been used. Returns the used kibibytes.
pub fn parse_quota_used_kib(output: &str) -> u64 {
    output
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            f.next()?; // project name or #id
            f.next()?.parse::<u64>().ok()
        })
        .next()
        .unwrap_or(0)
}

/// Where a volume's sidecar lives: next to its `data` directory.
pub fn sidecar_path(volume_dir: &Path) -> PathBuf {
    volume_dir.join(SIDECAR)
}

// ============= BACKEND =============

/// Probe the filesystem holding `base_path` once, at agent start.
pub async fn probe(base_path: &Path, mode: QuotaMode) -> Backend {
    if mode == QuotaMode::Off {
        return Backend::Unavailable {
            reason: "VOLUME_QUOTA=off".to_string(),
        };
    }
    let path = base_path.to_string_lossy().to_string();
    let out = Command::new("findmnt")
        .args(["-n", "-T", &path, "-o", "TARGET,FSTYPE,OPTIONS"])
        .output()
        .await;
    let line = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Ok(o) => {
            return Backend::Unavailable {
                reason: format!(
                    "findmnt failed: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
            }
        }
        Err(e) => {
            return Backend::Unavailable {
                reason: format!("findmnt not runnable: {e}"),
            }
        }
    };
    let Some((target, fstype, options)) = parse_findmnt(&line) else {
        return Backend::Unavailable {
            reason: format!("findmnt gave an unexpected line: {line:?}"),
        };
    };
    if !supports_project_quota(&fstype, &options) {
        return Backend::Unavailable {
            reason: format!(
                "{path} is on {fstype} at {target} (options {options}); \
                 volume quotas need xfs mounted with prjquota"
            ),
        };
    }
    for tool in ["xfs_quota", "xfs_io"] {
        if Command::new(tool).arg("-V").output().await.is_err() {
            return Backend::Unavailable {
                reason: format!("{tool} not found; install xfsprogs"),
            };
        }
    }
    Backend::Xfs {
        mount: PathBuf::from(target),
    }
}

impl Backend {
    pub fn is_enforcing(&self) -> bool {
        matches!(self, Backend::Xfs { .. })
    }

    pub fn describe(&self) -> String {
        match self {
            Backend::Xfs { mount } => format!("XFS project quotas on {}", mount.display()),
            Backend::Unavailable { reason } => format!("NOT enforced: {reason}"),
        }
    }

    async fn xfs_quota(&self, cmd: &str) -> Result<String, String> {
        let Backend::Xfs { mount } = self else {
            return Err("no quota backend".to_string());
        };
        let out = Command::new("xfs_quota")
            .args(["-x", "-c", cmd])
            .arg(mount)
            .output()
            .await
            .map_err(|e| format!("xfs_quota: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "xfs_quota -c {cmd:?}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// Project id currently on a directory (0 = none).
    pub async fn read_project_id(&self, dir: &Path) -> Result<u32, String> {
        let out = Command::new("xfs_io")
            .args(["-r", "-c", "lsproj"])
            .arg(dir)
            .output()
            .await
            .map_err(|e| format!("xfs_io: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "xfs_io lsproj {}: {}",
                dir.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        parse_lsproj(&String::from_utf8_lossy(&out.stdout))
            .ok_or_else(|| format!("xfs_io lsproj {}: no projid in output", dir.display()))
    }

    /// Kibibytes the project currently accounts for.
    pub async fn used_kib(&self, project_id: u32) -> Result<u64, String> {
        let out = self
            .xfs_quota(&format!("quota -p -N -v -b {project_id}"))
            .await?;
        Ok(parse_quota_used_kib(&out))
    }

    /// Pick a project id for a brand-new volume: the name's hash, moved on
    /// if that id already accounts for blocks (a collision with another
    /// volume on this node). The chosen id is then recorded in the sidecar,
    /// so this probe runs once per volume.
    pub async fn allocate_project_id(&self, container: &str) -> Result<u32, String> {
        let mut id = project_id_for(container);
        for _ in 0..16 {
            if self.used_kib(id).await? == 0 {
                return Ok(id);
            }
            warn!("project id {id} already in use, trying the next one");
            id = if id >= 0x7fff_ffff { 1 } else { id + 1 };
        }
        Err("could not find a free project id".to_string())
    }

    /// Put `dir` (and everything under it, with inherit) into the project.
    pub async fn assign(&self, dir: &Path, project_id: u32) -> Result<(), String> {
        let path = dir.to_string_lossy();
        self.xfs_quota(&format!("project -s -p {path} {project_id}"))
            .await
            .map(|_| ())
    }

    /// Hard block limit for the project, in MB. 0 removes the limit.
    pub async fn set_limit(&self, project_id: u32, size_mb: u64) -> Result<(), String> {
        self.xfs_quota(&format!("limit -p bhard={size_mb}m {project_id}"))
            .await
            .map(|_| ())
    }

    /// Make `dir` a quota-limited volume of `size_mb`, reusing the project
    /// id from an existing sidecar. Returns the record to persist.
    pub async fn apply(
        &self,
        container: &str,
        dir: &Path,
        size_mb: u64,
        existing: Option<&QuotaRecord>,
    ) -> Result<QuotaRecord, String> {
        let project_id = match existing {
            Some(r) => r.project_id,
            None => self.allocate_project_id(container).await?,
        };
        // Re-walk the tree only when the directory does not carry the id
        // yet (fresh volume, or files copied in from another filesystem).
        let on_disk = self.read_project_id(dir).await.unwrap_or(0);
        if on_disk != project_id {
            self.assign(dir, project_id).await?;
        }
        self.set_limit(project_id, size_mb).await?;
        let used_mb = self.used_kib(project_id).await.unwrap_or(0) / 1024;
        if used_mb > size_mb {
            warn!(
                "volume {} already holds {used_mb} MB, above its new {size_mb} MB limit; \
                 writes will fail until it shrinks",
                dir.display()
            );
        }
        info!(
            "📏 Volume quota: {} = {size_mb} MB (project {project_id})",
            dir.display()
        );
        Ok(QuotaRecord {
            project_id,
            size_mb,
            container: container.to_string(),
            applied_at: Utc::now(),
        })
    }

    /// Drop the limit of a project whose volume is gone.
    pub async fn release(&self, project_id: u32) {
        if let Err(e) = self.set_limit(project_id, 0).await {
            warn!("could not release quota of project {project_id}: {e}");
        }
    }
}

// ============= SIDECAR =============

pub async fn read_record(volume_dir: &Path) -> Option<QuotaRecord> {
    let raw = tokio::fs::read(sidecar_path(volume_dir)).await.ok()?;
    serde_json::from_slice(&raw).ok()
}

pub async fn write_record(volume_dir: &Path, record: &QuotaRecord) -> Result<(), String> {
    let path = sidecar_path(volume_dir);
    let tmp = volume_dir.join(format!("{SIDECAR}.tmp"));
    let body = serde_json::to_vec_pretty(record).map_err(|e| e.to_string())?;
    tokio::fs::write(&tmp, body)
        .await
        .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    tokio::fs::rename(&tmp, &path)
        .await
        .map_err(|e| format!("rename to {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_parse_like_the_controller() {
        assert_eq!(parse_size_mb("1g"), Some(1024));
        assert_eq!(parse_size_mb("10G"), Some(10240));
        assert_eq!(parse_size_mb("2gb"), Some(2048));
        assert_eq!(parse_size_mb("512m"), Some(512));
        assert_eq!(parse_size_mb(" 512M "), Some(512));
        assert_eq!(parse_size_mb("2048"), Some(2048));
        assert_eq!(parse_size_mb("1t"), Some(1024 * 1024));
        assert_eq!(parse_size_mb("0"), None);
        assert_eq!(parse_size_mb("0g"), None);
        assert_eq!(parse_size_mb("lots"), None);
        assert_eq!(parse_size_mb(""), None);
        assert_eq!(parse_size_mb("-1g"), None);
    }

    #[test]
    fn project_ids_are_stable_nonzero_and_31_bit() {
        let a = project_id_for("app-0d1c6f7e-1234-4a5b-9c8d-0e1f2a3b4c5d");
        assert_eq!(
            a,
            project_id_for("app-0d1c6f7e-1234-4a5b-9c8d-0e1f2a3b4c5d")
        );
        assert_ne!(
            a,
            project_id_for("app-0d1c6f7e-1234-4a5b-9c8d-0e1f2a3b4c5e")
        );
        assert!(a > 0 && a <= 0x7fff_ffff);
        assert!(project_id_for("") > 0);
    }

    #[test]
    fn findmnt_line_parses() {
        let (t, f, o) =
            parse_findmnt("/var/lib/nordkraft xfs rw,relatime,attr2,inode64,prjquota").unwrap();
        assert_eq!(t, "/var/lib/nordkraft");
        assert_eq!(f, "xfs");
        assert!(o.contains("prjquota"));
        assert!(parse_findmnt("").is_none());
        assert_eq!(parse_findmnt("/ ext4").map(|x| x.2), Some(String::new()));
    }

    #[test]
    fn only_xfs_with_prjquota_counts() {
        assert!(supports_project_quota("xfs", "rw,relatime,prjquota"));
        assert!(supports_project_quota("xfs", "rw,pquota"));
        assert!(!supports_project_quota("xfs", "rw,relatime,noquota"));
        assert!(!supports_project_quota("xfs", "rw,relatime,usrquota"));
        assert!(!supports_project_quota("ext4", "rw,relatime,prjquota"));
        assert!(!supports_project_quota("xfs", ""));
    }

    #[test]
    fn lsproj_output_parses() {
        assert_eq!(parse_lsproj("projid = 42\n"), Some(42));
        assert_eq!(parse_lsproj("projid = 0"), Some(0));
        assert_eq!(parse_lsproj("garbage"), None);
    }

    #[test]
    fn quota_usage_parses() {
        assert_eq!(
            parse_quota_used_kib("#42 10240 0 10485760 00 [--------] /var/lib/nordkraft\n"),
            10240
        );
        assert_eq!(parse_quota_used_kib(""), 0);
        assert_eq!(parse_quota_used_kib("#7 0 0 0 00 [--------] /x"), 0);
    }

    #[test]
    fn quota_mode_parses() {
        assert_eq!("auto".parse::<QuotaMode>(), Ok(QuotaMode::Auto));
        assert_eq!("".parse::<QuotaMode>(), Ok(QuotaMode::Auto));
        assert_eq!("required".parse::<QuotaMode>(), Ok(QuotaMode::Required));
        assert_eq!("OFF".parse::<QuotaMode>(), Ok(QuotaMode::Off));
        assert!("maybe".parse::<QuotaMode>().is_err());
    }

    #[test]
    fn sidecar_record_round_trips() {
        let r = QuotaRecord {
            project_id: 99,
            size_mb: 10240,
            container: "app-x".to_string(),
            applied_at: Utc::now(),
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: QuotaRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }
}
