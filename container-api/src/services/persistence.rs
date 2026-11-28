// src/services/persistence.rs — per-tenant volume directories on the node,
// with a disk limit per volume (see volume_quota.rs).
//
// Layout under /var/lib/nordkraft:
//   users/<slot>/volumes/<container>/data        the volume (bind-mounted)
//   users/<slot>/volumes/<container>/quota.json  project id + size, agent-owned
//   users/<slot>/backups/                        tar.gz taken on removal

use crate::services::volume_quota::{self, Backend, QuotaMode, QuotaRecord};
use chrono::Utc;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use tokio::fs;
use tokio::sync::OnceCell;
use tracing::{error, info, warn};

#[derive(Clone)]
pub struct PersistenceManager {
    base_path: PathBuf,
    quota_mode: QuotaMode,
    /// Probed on first use (the constructor is sync), then fixed.
    quota: Arc<OnceCell<Backend>>,
    enable_backups: bool,
}

impl PersistenceManager {
    pub fn new(quota_mode: QuotaMode) -> Self {
        // Always use the production path when running as root
        let base_path = PathBuf::from("/var/lib/nordkraft");

        // Create base directory if it doesn't exist
        if !base_path.exists() {
            if let Err(e) = std::fs::create_dir_all(&base_path) {
                warn!(
                    "Could not create base directory {}: {}",
                    base_path.display(),
                    e
                );
            }
        }

        info!(
            "📂 Persistence manager using base path: {}",
            base_path.display()
        );

        Self {
            base_path,
            quota_mode,
            quota: Arc::new(OnceCell::new()),
            enable_backups: cfg!(target_os = "linux"),
        }
    }

    /// The quota backend, probed once.
    pub async fn quota(&self) -> &Backend {
        self.quota
            .get_or_init(|| async {
                let b = volume_quota::probe(&self.base_path, self.quota_mode).await;
                match (&b, self.quota_mode) {
                    (Backend::Xfs { .. }, _) => info!("📏 Volume quotas: {}", b.describe()),
                    (_, QuotaMode::Off) => info!("📏 Volume quotas off"),
                    (_, QuotaMode::Required) => error!(
                        "📏 Volume quotas {} — persistent deploys will be refused",
                        b.describe()
                    ),
                    (_, QuotaMode::Auto) => warn!(
                        "📏 Volume quotas {} — volumes share the node disk unlimited",
                        b.describe()
                    ),
                }
                b
            })
            .await
    }

    fn volume_dir(&self, user_slot: u32, container_id: &str) -> PathBuf {
        self.base_path
            .join("users")
            .join(user_slot.to_string())
            .join("volumes")
            .join(container_id)
    }

    /// Create a persistent volume for a container and put it under quota.
    /// `size_mb` None means the platform default (1 GB). Idempotent: an
    /// existing volume keeps its data and gets the (possibly new) limit.
    pub async fn create_container_volume(
        &self,
        user_slot: u32,
        container_id: &str,
        volume_name: &str,
        size_mb: Option<u64>,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let volume_dir = self.volume_dir(user_slot, container_id);
        let volume_path = volume_dir.join(volume_name);

        // Create directory
        fs::create_dir_all(&volume_path).await?;

        // Set proper permissions for the container
        self.set_volume_permissions(&volume_path, user_slot).await?;

        info!("Created volume at: {}", volume_path.display());

        self.apply_quota(
            container_id,
            &volume_dir,
            &volume_path,
            size_mb.unwrap_or(volume_quota::DEFAULT_SIZE_MB),
        )
        .await?;

        Ok(volume_path.to_string_lossy().to_string())
    }

    /// Set the limit on one volume, honouring the quota mode: an error is
    /// fatal only under `required`.
    async fn apply_quota(
        &self,
        container_id: &str,
        volume_dir: &Path,
        data_dir: &Path,
        size_mb: u64,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if self.quota_mode == QuotaMode::Off {
            return Ok(());
        }
        let backend = self.quota().await;
        let result = match backend {
            Backend::Xfs { .. } => {
                let existing = volume_quota::read_record(volume_dir).await;
                match backend
                    .apply(container_id, data_dir, size_mb, existing.as_ref())
                    .await
                {
                    Ok(record) => volume_quota::write_record(volume_dir, &record).await,
                    Err(e) => Err(e),
                }
            }
            Backend::Unavailable { reason } => Err(reason.clone()),
        };
        match (result, self.quota_mode) {
            (Ok(()), _) => Ok(()),
            (Err(e), QuotaMode::Required) => Err(format!(
                "volume quota of {size_mb} MB could not be enforced for {container_id}: {e}"
            )
            .into()),
            (Err(e), _) => {
                warn!(
                    "volume {} deployed WITHOUT its {size_mb} MB limit: {e}",
                    data_dir.display()
                );
                Ok(())
            }
        }
    }

    /// Re-apply every volume's quota from its sidecar. Run at agent start:
    /// limits survive reboots on their own, but a volume copied to a new
    /// filesystem has lost its project id, and this puts it back.
    pub async fn reconcile_quotas(&self) {
        if self.quota_mode == QuotaMode::Off || !self.quota().await.is_enforcing() {
            return;
        }
        let backend = self.quota().await;
        let (mut seen, mut fixed, mut failed) = (0usize, 0usize, 0usize);
        for (volume_dir, record) in self.all_records().await {
            seen += 1;
            let data_dir = volume_dir.join("data");
            if !data_dir.is_dir() {
                continue;
            }
            match backend
                .apply(&record.container, &data_dir, record.size_mb, Some(&record))
                .await
            {
                Ok(new) => {
                    if new.project_id != record.project_id || new.size_mb != record.size_mb {
                        let _ = volume_quota::write_record(&volume_dir, &new).await;
                    }
                    fixed += 1;
                }
                Err(e) => {
                    failed += 1;
                    warn!("quota reconcile {}: {e}", data_dir.display());
                }
            }
        }
        if seen > 0 || failed > 0 {
            info!("📏 Volume quota reconcile: {seen} volumes, {fixed} applied, {failed} failed");
        }
    }

    /// Every (volume dir, sidecar) under users/*/volumes/*.
    async fn all_records(&self) -> Vec<(PathBuf, QuotaRecord)> {
        let mut out = Vec::new();
        let Ok(mut users) = fs::read_dir(self.base_path.join("users")).await else {
            return out;
        };
        while let Ok(Some(user)) = users.next_entry().await {
            let Ok(mut volumes) = fs::read_dir(user.path().join("volumes")).await else {
                continue;
            };
            while let Ok(Some(vol)) = volumes.next_entry().await {
                if let Some(record) = volume_quota::read_record(&vol.path()).await {
                    out.push((vol.path(), record));
                }
            }
        }
        out
    }

    /// Free the project of a volume that is about to be removed.
    async fn release_quota(&self, volume_dir: &Path) {
        if self.quota_mode == QuotaMode::Off {
            return;
        }
        if let Some(record) = volume_quota::read_record(volume_dir).await {
            let backend = self.quota().await;
            if backend.is_enforcing() {
                backend.release(record.project_id).await;
            }
        }
    }

    /// Set permissions that work with podman containers
    async fn set_volume_permissions(
        &self,
        path: &Path,
        _user_slot: u32,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Simple permission for containers to write
        Command::new("chmod")
            .args(["777", path.to_str().unwrap()])
            .output()?;

        info!("Set permissions 777 on {}", path.display());
        Ok(())
    }

    /// Backup volume before deletion
    pub async fn backup_volume(
        &self,
        user_slot: u32,
        container_id: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        if !self.enable_backups {
            return Ok(String::new());
        }

        let volume_path = self.volume_dir(user_slot, container_id);

        if !volume_path.exists() {
            return Ok(String::new());
        }

        let backup_dir = self
            .base_path
            .join("users")
            .join(user_slot.to_string())
            .join("backups");

        fs::create_dir_all(&backup_dir).await?;

        let backup_file = backup_dir.join(format!(
            "{}-{}.tar.gz",
            container_id,
            Utc::now().timestamp()
        ));

        let output = Command::new("tar")
            .args([
                "-czf",
                backup_file.to_str().unwrap(),
                "-C",
                volume_path.parent().unwrap().to_str().unwrap(),
                volume_path.file_name().unwrap().to_str().unwrap(),
            ])
            .output()?;

        if !output.status.success() {
            error!("Backup failed: {}", String::from_utf8_lossy(&output.stderr));
            return Err("Backup failed".into());
        }

        info!("Created backup: {}", backup_file.display());
        Ok(backup_file.to_string_lossy().to_string())
    }

    /// Remove container volumes
    pub async fn remove_container_volumes(
        &self,
        user_slot: u32,
        container_id: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Backup first
        if let Err(e) = self.backup_volume(user_slot, container_id).await {
            warn!("Backup failed, continuing with removal: {}", e);
        }

        let volume_path = self.volume_dir(user_slot, container_id);

        if volume_path.exists() {
            self.release_quota(&volume_path).await;
            fs::remove_dir_all(volume_path).await?;
            info!("Removed volumes for container {}", container_id);
        }

        Ok(())
    }

    /// Remove everything a tenant has on this node: all volumes AND the volume
    /// backups taken on container removal. Used only by the admin tenant purge.
    /// Returns whether the directory existed.
    pub async fn remove_tenant_data(
        &self,
        user_slot: u32,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        // Slot 0 or > 255 is never a tenant; refuse rather than build an odd path.
        if !(1..=255).contains(&user_slot) {
            return Err(format!("refusing to remove data for invalid slot {}", user_slot).into());
        }

        let tenant_path = self.base_path.join("users").join(user_slot.to_string());

        if !tenant_path.exists() {
            return Ok(false);
        }

        if let Ok(mut volumes) = fs::read_dir(tenant_path.join("volumes")).await {
            while let Ok(Some(vol)) = volumes.next_entry().await {
                self.release_quota(&vol.path()).await;
            }
        }

        fs::remove_dir_all(&tenant_path).await?;
        info!("🧹 Removed all tenant data at {}", tenant_path.display());
        Ok(true)
    }
}
