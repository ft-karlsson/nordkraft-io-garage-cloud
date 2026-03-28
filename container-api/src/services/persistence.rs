// src/services/persistence.rs - Fixed version with proper container permissions

use chrono::Utc;
use std::path::{Path, PathBuf};
use std::process::Command;
use tokio::fs;
use tracing::{error, info, warn};

#[derive(Clone)]
pub struct PersistenceManager {
    base_path: PathBuf,
    #[allow(dead_code)] // For future quota enforcement
    enable_quotas: bool,
    enable_backups: bool,
    // rootless_mode: bool, // Track if we're running rootless
}

impl PersistenceManager {
    pub fn new() -> Self {
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
            enable_quotas: false,
            enable_backups: cfg!(target_os = "linux"),
        }
    }

    /// Create a persistent volume for a container
    pub async fn create_container_volume(
        &self,
        user_slot: u32,
        container_id: &str,
        volume_name: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let volume_path = self
            .base_path
            .join("users")
            .join(user_slot.to_string())
            .join("volumes")
            .join(container_id)
            .join(volume_name);

        // Create directory
        fs::create_dir_all(&volume_path).await?;

        // Set proper permissions for the container
        self.set_volume_permissions(&volume_path, user_slot).await?;

        info!("Created volume at: {}", volume_path.display());
        Ok(volume_path.to_string_lossy().to_string())
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

    /// Set XFS quota on volume (if XFS filesystem)
    /// Currently unused - requires XFS with project quotas enabled
    #[allow(dead_code)]
    pub async fn set_volume_quota(
        &self,
        volume_path: &str,
        size_mb: u32,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if !self.enable_quotas {
            return Ok(());
        }

        // This requires XFS filesystem with project quotas enabled
        // Skip if not available
        let output = Command::new("xfs_quota")
            .args(["-x", "-c", &format!("limit -p bhard={}m", size_mb), "/"])
            .output();

        match output {
            Ok(_) => info!("Set {}MB quota on {}", size_mb, volume_path),
            Err(e) => warn!("Could not set quota (XFS not available?): {}", e),
        }

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

        let volume_path = self
            .base_path
            .join("users")
            .join(user_slot.to_string())
            .join("volumes")
            .join(container_id);

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

        let volume_path = self
            .base_path
            .join("users")
            .join(user_slot.to_string())
            .join("volumes")
            .join(container_id);

        if volume_path.exists() {
            fs::remove_dir_all(volume_path).await?;
            info!("Removed volumes for container {}", container_id);
        }

        Ok(())
    }
}
