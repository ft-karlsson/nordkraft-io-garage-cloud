// services/volume_backup.rs — full volume backup and restore on the node.
//
// A backup is the container's volume directory as `tar | zstd`, streamed to
// the caller; nothing is written on the node. A restore is the reverse:
// the stream is extracted into a sibling directory next to the live volume,
// verified, and swapped in with two renames while the container is stopped.
//
// Consistency: when `/var/lib/nordkraft` sits on an LVM logical volume with
// free extents in its volume group, the export reads from a short-lived
// snapshot (crash-consistent, no downtime). Otherwise it reads the live
// directory and says so in the manifest.

use crate::services::container_manager::ContainerManager;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Directory for snapshot mount points. tmpfs, so a reboot forgets it.
pub const SNAP_MOUNT_ROOT: &str = "/run/nordkraft";
/// Prefix of the LVM snapshots this module creates; the boot sweep removes
/// every LV with this prefix.
pub const SNAP_LV_PREFIX: &str = "nk-bk-";
/// Minimum copy-on-write space reserved for a snapshot.
const SNAP_MIN_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Headroom that must stay free in the volume group after the snapshot.
const SNAP_VG_RESERVE_BYTES: u64 = 1024 * 1024 * 1024;
/// Free space that must remain on the volume filesystem after a restore.
const RESTORE_DISK_RESERVE_BYTES: u64 = 1024 * 1024 * 1024;
/// Chunk size for the export stream.
const CHUNK: usize = 256 * 1024;

/// Written as `.nordkraft/manifest.json` inside every archive.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub container_id: String,
    pub volume_path: String,
    pub created_at: String,
    /// "snapshot" or "live"
    pub consistency: String,
    pub node: String,
    /// Uncompressed size of the volume directory when the backup started.
    pub volume_bytes: u64,
}

/// The container's single volume, as the runtime sees it.
#[derive(Debug, Clone)]
pub struct VolumeTarget {
    pub container_id: String,
    pub host_path: PathBuf,
    pub mount_path: String,
    pub running: bool,
}

/// Result of a restore, returned to the caller as JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreReport {
    pub container_id: String,
    pub volume_path: String,
    pub received_bytes: u64,
    pub sha256: String,
    pub manifest: Option<Manifest>,
    pub restarted: bool,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// Resolve the volume
// ---------------------------------------------------------------------------

/// Find the container (owner-scoped) and its one bind-mounted volume.
pub async fn resolve_volume(
    cm: &ContainerManager,
    container: &str,
    owner_pubkey: &str,
) -> Result<VolumeTarget, BoxError> {
    let owned = cm
        .list_user_containers(owner_pubkey)
        .await?
        .into_iter()
        .find(|c| c.container_id == container || c.name == container)
        .ok_or("Container not found or access denied")?;

    let (mounts, running) = cm.inspect_mounts_and_state(&owned.container_id).await?;
    let (host_path, mount_path) = mounts
        .into_iter()
        .next()
        .ok_or("Container has no persistent volume (deploy with --persistence)")?;
    let host_path = PathBuf::from(host_path);
    if !host_path.is_dir() {
        return Err(format!(
            "volume directory {} is missing on this node",
            host_path.display()
        )
        .into());
    }
    Ok(VolumeTarget {
        // The app-… name is what the DB, the CLI and the manifest use; the
        // runtime's short id is an implementation detail.
        container_id: owned.name,
        host_path,
        mount_path,
        running,
    })
}

// ---------------------------------------------------------------------------
// LVM snapshot (best effort)
// ---------------------------------------------------------------------------

struct Snapshot {
    lv: String, // "vg/name"
    mount: PathBuf,
    /// The volume directory as seen through the snapshot mount.
    source: PathBuf,
}

impl Snapshot {
    async fn cleanup(self) {
        let _ = Command::new("umount").arg(&self.mount).output().await;
        let out = Command::new("lvremove")
            .args(["-f", &self.lv])
            .output()
            .await;
        match out {
            Ok(o) if o.status.success() => info!("🧹 Removed snapshot {}", self.lv),
            Ok(o) => warn!(
                "snapshot {} not removed: {}",
                self.lv,
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => warn!("snapshot {} not removed: {}", self.lv, e),
        }
        let _ = tokio::fs::remove_dir(&self.mount).await;
    }
}

/// Copy-on-write space to give a snapshot: at least SNAP_MIN_BYTES, at least
/// a fifth of the volume, never more than what the group can spare.
pub fn snapshot_size(volume_bytes: u64, vg_free: u64) -> Option<u64> {
    let wanted = SNAP_MIN_BYTES.max(volume_bytes / 5);
    if vg_free < wanted + SNAP_VG_RESERVE_BYTES {
        return None;
    }
    Some(wanted)
}

/// Mount options that make a snapshot of a journaled filesystem readable
/// without touching the origin's UUID bookkeeping.
pub fn snapshot_mount_opts(fstype: &str) -> &'static str {
    match fstype {
        "xfs" => "ro,nouuid",
        _ => "ro",
    }
}

/// `findmnt` prints a bind mount's source as `/dev/x[/sub/dir]`. Split that
/// into the device and the subtree (relative, empty when not a bind mount).
pub fn split_bind_source(source: &str) -> (String, PathBuf) {
    match (source.find('['), source.ends_with(']')) {
        (Some(i), true) => {
            let dev = source[..i].to_string();
            let sub = source[i + 1..source.len() - 1].trim_start_matches('/');
            (dev, PathBuf::from(sub))
        }
        _ => (source.to_string(), PathBuf::new()),
    }
}

/// Where `path` lives relative to its mount point, so the same directory can
/// be found under the snapshot mount.
pub fn path_under_mount(path: &Path, mount_target: &Path) -> Option<PathBuf> {
    path.strip_prefix(mount_target)
        .ok()
        .map(|p| p.to_path_buf())
}

/// Try to take an LVM snapshot of the filesystem holding `host_path`. Any
/// failure means "no snapshot" and the caller copies live; the reason is
/// logged so a surprising `live` manifest can be explained from the journal.
async fn try_snapshot(host_path: &Path, volume_bytes: u64, tag: &str) -> Option<Snapshot> {
    match snapshot_inner(host_path, volume_bytes, tag).await {
        Ok(s) => Some(s),
        Err(reason) => {
            warn!("snapshot not possible, falling back to live copy: {reason}");
            None
        }
    }
}

/// Run a discovery command and return its trimmed stdout, or why it failed.
async fn probe(cmd: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("{cmd}: cannot run: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{cmd} {}: exit {}: {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

async fn snapshot_inner(
    host_path: &Path,
    volume_bytes: u64,
    tag: &str,
) -> Result<Snapshot, String> {
    // Which block device and mount point hold the volume?
    let path = host_path.to_string_lossy().to_string();
    let line = probe(
        "findmnt",
        &["-n", "-T", &path, "-o", "SOURCE,TARGET,FSTYPE"],
    )
    .await?;
    let mut parts = line.split_whitespace();
    let (source, target, fstype) = match (parts.next(), parts.next(), parts.next()) {
        (Some(s), Some(t), Some(f)) => (s.to_string(), t.to_string(), f.to_string()),
        _ => return Err(format!("findmnt gave an unexpected line: {line:?}")),
    };
    if !source.starts_with("/dev/") {
        return Err(format!("{path} is on {source}, not a block device"));
    }
    // A bind mount reports its device as `/dev/x[/sub/dir]`: the filesystem is
    // `/dev/x` and the mount shows the subtree `/sub/dir` of it at `target`.
    let (source, bind_root) = split_bind_source(&source);

    // Is it an LVM logical volume, and does the group have room?
    let lvs = probe(
        "lvs",
        &[
            "--noheadings",
            "--units",
            "b",
            "--nosuffix",
            "-o",
            "vg_name,lv_name",
            &source,
        ],
    )
    .await?;
    let mut f = lvs.split_whitespace();
    let (vg, lv) = match (f.next(), f.next()) {
        (Some(v), Some(l)) => (v.to_string(), l.to_string()),
        _ => {
            return Err(format!(
                "{source} is not an LVM logical volume (lvs: {lvs:?})"
            ))
        }
    };
    let vgs = probe(
        "vgs",
        &[
            "--noheadings",
            "--units",
            "b",
            "--nosuffix",
            "-o",
            "vg_free",
            &vg,
        ],
    )
    .await?;
    let vg_free: u64 = vgs
        .parse()
        .map_err(|_| format!("vgs gave an unexpected free size: {vgs:?}"))?;
    let size = snapshot_size(volume_bytes, vg_free)
        .ok_or_else(|| format!("{vg} has only {vg_free} B free for a snapshot of {lv}"))?;

    let name = format!("{SNAP_LV_PREFIX}{tag}");
    let lv_ref = format!("{vg}/{lv}");
    probe(
        "lvcreate",
        &["-s", "-n", &name, "-L", &format!("{size}b"), &lv_ref],
    )
    .await?;
    let snap = Snapshot {
        lv: format!("{vg}/{name}"),
        mount: Path::new(SNAP_MOUNT_ROOT).join(format!("snap-{tag}")),
        source: PathBuf::new(),
    };
    if let Err(e) = tokio::fs::create_dir_all(&snap.mount).await {
        let msg = format!("cannot create {}: {e}", snap.mount.display());
        snap.cleanup().await;
        return Err(msg);
    }
    let dev = format!("/dev/{vg}/{name}");
    let mount_dir = snap.mount.to_string_lossy().to_string();
    if let Err(e) = probe(
        "mount",
        &["-o", snapshot_mount_opts(&fstype), &dev, &mount_dir],
    )
    .await
    {
        snap.cleanup().await;
        return Err(e);
    }
    let rel = match path_under_mount(host_path, Path::new(&target)) {
        Some(r) => r,
        None => {
            snap.cleanup().await;
            return Err(format!("{path} is not under mount point {target}"));
        }
    };
    let source = snap.mount.join(&bind_root).join(rel);
    if !source.is_dir() {
        let msg = format!("{} is not a directory in the snapshot", source.display());
        snap.cleanup().await;
        return Err(msg);
    }
    info!(
        "📸 Snapshot {} mounted at {}",
        snap.lv,
        snap.mount.display()
    );
    Ok(Snapshot { source, ..snap })
}

/// Remove snapshots and mounts left behind by a crash. Called once at agent
/// start; safe when there is nothing to do or no LVM at all.
pub async fn sweep_stale_snapshots() {
    if let Ok(mut rd) = tokio::fs::read_dir(SNAP_MOUNT_ROOT).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            if e.file_name().to_string_lossy().starts_with("snap-") {
                let p = e.path();
                let _ = Command::new("umount").arg(&p).output().await;
                let _ = tokio::fs::remove_dir(&p).await;
            }
        }
    }
    let out = match Command::new("lvs")
        .args(["--noheadings", "-o", "vg_name,lv_name"])
        .output()
        .await
    {
        Ok(o) if o.status.success() => o,
        _ => return,
    };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut f = line.split_whitespace();
        if let (Some(vg), Some(lv)) = (f.next(), f.next()) {
            if lv.starts_with(SNAP_LV_PREFIX) {
                let lv_ref = format!("{vg}/{lv}");
                warn!("🧹 Removing stale backup snapshot {}", lv_ref);
                let _ = Command::new("lvremove")
                    .args(["-f", &lv_ref])
                    .output()
                    .await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

/// What a running export hands to the HTTP layer.
pub struct Export {
    pub manifest: Manifest,
    /// Archive bytes; an `Err` means the producer failed mid-stream and the
    /// connection must be dropped so the client does not keep a torn file.
    pub stream: mpsc::Receiver<Result<Bytes, std::io::Error>>,
}

pub async fn dir_size_bytes(path: &Path) -> u64 {
    let out = Command::new("du").args(["-sb"]).arg(path).output().await;
    out.ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
        })
        .unwrap_or(0)
}

/// Start streaming the volume. The returned receiver yields the archive; the
/// producer task owns the snapshot and child processes and cleans up on every
/// exit path, including the receiver being dropped.
pub async fn export(target: &VolumeTarget, node_id: &str) -> Result<Export, BoxError> {
    let tag = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
    let volume_bytes = dir_size_bytes(&target.host_path).await;
    let snapshot = try_snapshot(&target.host_path, volume_bytes, &tag).await;
    let (source, consistency) = match &snapshot {
        Some(s) => (s.source.clone(), "snapshot"),
        None => (target.host_path.clone(), "live"),
    };
    let manifest = Manifest {
        container_id: target.container_id.clone(),
        volume_path: target.mount_path.clone(),
        created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        consistency: consistency.to_string(),
        node: node_id.to_string(),
        volume_bytes,
    };

    // The manifest rides in the archive as .nordkraft/manifest.json.
    let meta_dir = Path::new(SNAP_MOUNT_ROOT).join(format!("meta-{tag}"));
    let meta_sub = meta_dir.join(".nordkraft");
    tokio::fs::create_dir_all(&meta_sub).await?;
    tokio::fs::write(
        meta_sub.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )
    .await?;

    let mut tar = Command::new("tar")
        .arg("--numeric-owner")
        .args(["-cf", "-"])
        .arg("-C")
        .arg(&meta_dir)
        .arg("./.nordkraft")
        .arg("-C")
        .arg(&source)
        .arg(".")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let tar_out: Stdio = tar.stdout.take().ok_or("tar stdout")?.try_into()?;
    let mut zstd = Command::new("zstd")
        .args(["-T0", "-3", "-c", "-q"])
        .stdin(tar_out)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut out = zstd.stdout.take().ok_or("zstd stdout")?;

    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(8);
    let label = target.container_id.clone();
    tokio::spawn(async move {
        let mut buf = vec![0u8; CHUNK];
        let mut total: u64 = 0;
        let mut failed = false;
        loop {
            match out.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    total += n as u64;
                    if tx
                        .send(Ok(Bytes::copy_from_slice(&buf[..n])))
                        .await
                        .is_err()
                    {
                        warn!("backup {}: client went away after {} B", label, total);
                        failed = true;
                        break;
                    }
                }
                Err(e) => {
                    error!("backup {}: read error: {e}", label);
                    let _ = tx.send(Err(e)).await;
                    failed = true;
                    break;
                }
            }
        }
        // Both children must have exited cleanly for the archive to be whole.
        let tar_status = tar.wait().await;
        let zstd_status = zstd.wait().await;
        let ok = matches!(&tar_status, Ok(s) if s.success())
            && matches!(&zstd_status, Ok(s) if s.success());
        if !ok && !failed {
            let mut err = String::new();
            if let Some(mut e) = tar.stderr.take() {
                let _ = e.read_to_string(&mut err).await;
            }
            error!("backup {}: tar/zstd failed: {}", label, err.trim());
            let _ = tx
                .send(Err(std::io::Error::other(format!(
                    "archive failed: {}",
                    err.trim()
                ))))
                .await;
        } else if ok {
            info!("📦 backup {}: {} B streamed", label, total);
        }
        if let Some(s) = snapshot {
            s.cleanup().await;
        }
        let _ = tokio::fs::remove_dir_all(&meta_dir).await;
    });

    Ok(Export {
        manifest,
        stream: rx,
    })
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

/// Sibling directory the archive is extracted into before the swap.
pub fn restoring_path(host_path: &Path) -> PathBuf {
    sibling(host_path, ".restoring")
}

fn sibling(host_path: &Path, suffix: &str) -> PathBuf {
    let name = host_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "data".to_string());
    host_path.with_file_name(format!("{name}{suffix}"))
}

async fn free_bytes(path: &Path) -> Option<u64> {
    let out = Command::new("df")
        .args(["--output=avail", "-B1"])
        .arg(path)
        .output()
        .await
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .nth(1)
        .and_then(|s| s.trim().parse().ok())
}

/// Replace the volume with the archive read from `body`. Stops the container
/// first when it runs and starts it again afterwards, whether or not the
/// restore succeeded. The live directory is only touched by the final swap.
pub async fn import<R: AsyncRead + Unpin>(
    cm: &ContainerManager,
    owner_pubkey: &str,
    target: &VolumeTarget,
    mut body: R,
    expected_sha256: Option<&str>,
    declared_bytes: Option<u64>,
) -> Result<RestoreReport, BoxError> {
    if let (Some(declared), Some(free)) = (declared_bytes, free_bytes(&target.host_path).await) {
        // Compressed size is a floor for what extraction needs; be conservative.
        if free < declared.saturating_mul(3) + RESTORE_DISK_RESERVE_BYTES {
            return Err(format!(
                "not enough free space on the node for a {} B archive ({} B free)",
                declared, free
            )
            .into());
        }
    }

    let restoring = restoring_path(&target.host_path);
    let _ = tokio::fs::remove_dir_all(&restoring).await;
    tokio::fs::create_dir_all(&restoring).await?;

    if target.running {
        cm.stop_container(&target.container_id, owner_pubkey)
            .await?;
    }

    let result = extract_into(&restoring, &mut body, expected_sha256).await;

    let report = match result {
        Ok((received, sha, manifest)) => {
            let mut warnings = Vec::new();
            if let Some(m) = &manifest {
                if m.volume_path != target.mount_path {
                    warnings.push(format!(
                        "archive was taken from {} but this container mounts {}",
                        m.volume_path, target.mount_path
                    ));
                }
                if m.consistency == "live" {
                    warnings.push("archive was a live copy, not a snapshot".to_string());
                }
            }
            // The manifest must not end up inside the application's volume.
            let _ = tokio::fs::remove_dir_all(restoring.join(".nordkraft")).await;
            let _ = Command::new("chmod")
                .arg("777")
                .arg(&restoring)
                .output()
                .await;

            match swap_in(&target.host_path, &restoring).await {
                Ok(()) => Ok(RestoreReport {
                    container_id: target.container_id.clone(),
                    volume_path: target.mount_path.clone(),
                    received_bytes: received,
                    sha256: sha,
                    manifest,
                    restarted: false,
                    warnings,
                }),
                Err(e) => Err(e),
            }
        }
        Err(e) => {
            let _ = tokio::fs::remove_dir_all(&restoring).await;
            Err(e)
        }
    };

    let mut restarted = false;
    if target.running {
        match cm.start_container(&target.container_id, owner_pubkey).await {
            Ok(()) => restarted = true,
            Err(e) => error!(
                "restore {}: container did not start again: {e}",
                target.container_id
            ),
        }
    }
    report.map(|mut r| {
        r.restarted = restarted;
        r
    })
}

/// zstd -d | tar -x into `dir`, hashing the compressed bytes as they pass.
async fn extract_into<R: AsyncRead + Unpin>(
    dir: &Path,
    body: &mut R,
    expected_sha256: Option<&str>,
) -> Result<(u64, String, Option<Manifest>), BoxError> {
    let mut zstd = Command::new("zstd")
        .args(["-d", "-c", "-q"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let zstd_out: Stdio = zstd.stdout.take().ok_or("zstd stdout")?.try_into()?;
    let mut tar = Command::new("tar")
        .arg("--numeric-owner")
        .args(["-xf", "-"])
        .arg("-C")
        .arg(dir)
        .stdin(zstd_out)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut zin = zstd.stdin.take().ok_or("zstd stdin")?;

    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut received: u64 = 0;
    let copy: Result<(), BoxError> = async {
        loop {
            let n = body.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            received += n as u64;
            hasher.update(&buf[..n]);
            zin.write_all(&buf[..n]).await?;
        }
        zin.shutdown().await?;
        Ok(())
    }
    .await;
    drop(zin);

    let zstd_status = zstd.wait().await?;
    let tar_status = tar.wait().await?;
    copy?;
    if !zstd_status.success() {
        let mut err = String::new();
        if let Some(mut e) = zstd.stderr.take() {
            let _ = e.read_to_string(&mut err).await;
        }
        return Err(format!("archive is not a zstd stream: {}", err.trim()).into());
    }
    if !tar_status.success() {
        let mut err = String::new();
        if let Some(mut e) = tar.stderr.take() {
            let _ = e.read_to_string(&mut err).await;
        }
        return Err(format!("archive could not be extracted: {}", err.trim()).into());
    }
    if received == 0 {
        return Err("empty upload".into());
    }

    let sha = hex::encode(hasher.finalize());
    if let Some(expected) = expected_sha256 {
        if !expected.eq_ignore_ascii_case(&sha) {
            return Err(format!("checksum mismatch: expected {expected}, received {sha}").into());
        }
    }

    let manifest = tokio::fs::read(dir.join(".nordkraft").join("manifest.json"))
        .await
        .ok()
        .and_then(|b| serde_json::from_slice::<Manifest>(&b).ok());
    Ok((received, sha, manifest))
}

/// data → data.prev, data.restoring → data, then drop data.prev.
async fn swap_in(live: &Path, restoring: &Path) -> Result<(), BoxError> {
    let prev = sibling(live, ".prev");
    let _ = tokio::fs::remove_dir_all(&prev).await;
    if live.exists() {
        tokio::fs::rename(live, &prev).await?;
    }
    if let Err(e) = tokio::fs::rename(restoring, live).await {
        // Put the old data back; nothing was lost.
        if prev.exists() {
            let _ = tokio::fs::rename(&prev, live).await;
        }
        return Err(format!("could not move restored data into place: {e}").into());
    }
    if let Err(e) = tokio::fs::remove_dir_all(&prev).await {
        warn!("previous volume data left at {}: {e}", prev.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_size_floor_and_fifth() {
        // small volume → 2 GiB floor
        assert_eq!(
            snapshot_size(100 * 1024 * 1024, 100 << 30),
            Some(SNAP_MIN_BYTES)
        );
        // 50 GiB volume → 10 GiB
        assert_eq!(snapshot_size(50 << 30, 100 << 30), Some(10 << 30));
    }

    #[test]
    fn snapshot_size_refuses_without_headroom() {
        assert_eq!(snapshot_size(1 << 30, 2 << 30), None); // 2 GiB wanted + 1 GiB reserve > 2 GiB free
        assert_eq!(snapshot_size(1 << 30, 3 << 30), Some(SNAP_MIN_BYTES));
    }

    #[test]
    fn bind_mount_source_is_split() {
        let (dev, sub) = split_bind_source("/dev/mapper/ubuntu--vg-ubuntu--lv[/var/lib/nordkraft]");
        assert_eq!(dev, "/dev/mapper/ubuntu--vg-ubuntu--lv");
        assert_eq!(sub, PathBuf::from("var/lib/nordkraft"));
        let (dev, sub) = split_bind_source("/dev/mapper/ubuntu--vg-ubuntu--lv");
        assert_eq!(dev, "/dev/mapper/ubuntu--vg-ubuntu--lv");
        assert_eq!(sub, PathBuf::new());
        assert_eq!(
            Path::new("/run/snap").join(&sub).join("users/1"),
            PathBuf::from("/run/snap/users/1")
        );
    }

    #[test]
    fn mount_opts_by_fstype() {
        assert_eq!(snapshot_mount_opts("ext4"), "ro");
        assert_eq!(snapshot_mount_opts("xfs"), "ro,nouuid");
        assert_eq!(snapshot_mount_opts("btrfs"), "ro");
    }

    #[test]
    fn relative_path_under_root_mount() {
        let p = Path::new("/var/lib/nordkraft/users/1/volumes/app-x/data");
        assert_eq!(
            path_under_mount(p, Path::new("/")),
            Some(PathBuf::from(
                "var/lib/nordkraft/users/1/volumes/app-x/data"
            ))
        );
        assert_eq!(
            path_under_mount(p, Path::new("/var/lib/nordkraft")),
            Some(PathBuf::from("users/1/volumes/app-x/data"))
        );
        assert_eq!(path_under_mount(p, Path::new("/home")), None);
    }

    #[test]
    fn sibling_directories() {
        let live = Path::new("/var/lib/nordkraft/users/1/volumes/app-x/data");
        assert_eq!(
            restoring_path(live),
            PathBuf::from("/var/lib/nordkraft/users/1/volumes/app-x/data.restoring")
        );
        assert_eq!(
            sibling(live, ".prev"),
            PathBuf::from("/var/lib/nordkraft/users/1/volumes/app-x/data.prev")
        );
    }

    #[test]
    fn manifest_round_trip() {
        let m = Manifest {
            container_id: "app-1".into(),
            volume_path: "/rails/storage".into(),
            created_at: "2026-10-26T18:40:11Z".into(),
            consistency: "snapshot".into(),
            node: "ry-optiplex-1".into(),
            volume_bytes: 42,
        };
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(serde_json::from_str::<Manifest>(&json).unwrap(), m);
    }

    #[tokio::test]
    async fn swap_in_replaces_and_keeps_nothing_behind() {
        let tmp = std::env::temp_dir().join(format!("nk-swap-{}", uuid::Uuid::new_v4()));
        let live = tmp.join("data");
        let restoring = tmp.join("data.restoring");
        tokio::fs::create_dir_all(&live).await.unwrap();
        tokio::fs::create_dir_all(&restoring).await.unwrap();
        tokio::fs::write(live.join("old"), b"old").await.unwrap();
        tokio::fs::write(restoring.join("new"), b"new")
            .await
            .unwrap();

        swap_in(&live, &restoring).await.unwrap();

        assert!(live.join("new").exists());
        assert!(!live.join("old").exists());
        assert!(!restoring.exists());
        assert!(!tmp.join("data.prev").exists());
        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn extract_rejects_garbage_and_leaves_dir_empty() {
        let tmp = std::env::temp_dir().join(format!("nk-x-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&tmp).await.unwrap();
        let mut body: &[u8] = b"this is not zstd";
        let r = extract_into(&tmp, &mut body, None).await;
        assert!(r.is_err());
        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }
}
