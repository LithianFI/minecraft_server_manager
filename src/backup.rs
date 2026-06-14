use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use cron::Schedule;
use serde::Serialize;

use crate::config::data_dir;
use crate::state::{AppState, InstanceStatus, WsEvent};

#[derive(Debug, Clone, Serialize)]
pub struct BackupInfo {
    pub filename: String,
    pub size_bytes: u64,
    pub created_at: i64,
}

pub fn backup_dir(instance_id: &str) -> PathBuf {
    data_dir().join("backups").join(instance_id)
}

fn backup_filename() -> String {
    let now = Utc::now();
    format!("{}.tar.zst", now.format("%Y-%m-%d_%H-%M-%S"))
}

fn parse_backup_timestamp(filename: &str) -> Option<i64> {
    let stem = filename.strip_suffix(".tar.zst")?;
    // Try with seconds first, then without
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(stem, "%Y-%m-%d_%H-%M-%S") {
        return Some(dt.and_utc().timestamp());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(stem, "%Y-%m-%d_%H-%M") {
        return Some(dt.and_utc().timestamp());
    }
    None
}

pub fn list_backups(instance_id: &str) -> Vec<BackupInfo> {
    let dir = backup_dir(instance_id);
    let mut infos = Vec::new();

    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("zst") {
                continue;
            }
            let filename = path.file_name().unwrap().to_string_lossy().to_string();
            let size_bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
            let created_at = parse_backup_timestamp(&filename).unwrap_or(0);
            infos.push(BackupInfo { filename, size_bytes, created_at });
        }
    }

    infos.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    infos
}

pub async fn trigger_backup(state: Arc<AppState>, instance_id: String) {
    match do_create_backup(&state, &instance_id).await {
        Ok(info) => {
            tracing::info!("Backup created: {}/{}", instance_id, info.filename);
        }
        Err(e) => {
            tracing::error!("Backup failed for '{}': {}", instance_id, e);
            let _ = state.log_tx.send(WsEvent::BackupFailed {
                instance_id,
                error: e,
            });
        }
    }
}

pub async fn do_create_backup(state: &AppState, instance_id: &str) -> Result<BackupInfo, String> {
    let (server_path, world_only, keep_count) = {
        let instances = state.instances.read().await;
        let inst = instances
            .get(instance_id)
            .ok_or_else(|| format!("Instance '{}' not found", instance_id))?;
        let backup = &inst.config.backup;
        (
            inst.config.server.path.clone(),
            backup.as_ref().map(|b| b.world_only).unwrap_or(false),
            backup.as_ref().map(|b| b.keep_count).unwrap_or(10),
        )
    };

    let dir = backup_dir(instance_id);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("Failed to create backup dir: {}", e))?;

    let filename = backup_filename();
    let output_path = dir.join(&filename);
    let output_clone = output_path.clone();
    let server_clone = server_path.clone();

    let size_bytes = tokio::task::spawn_blocking(move || {
        create_tar_zst(&server_clone, &output_clone, world_only)
    })
    .await
    .map_err(|e| format!("Backup task panicked: {}", e))?
    .map_err(|e| format!("Failed to create archive: {}", e))?;

    apply_retention(instance_id, keep_count);

    let created_at = parse_backup_timestamp(&filename).unwrap_or_else(|| Utc::now().timestamp());
    let info = BackupInfo { filename: filename.clone(), size_bytes, created_at };

    let _ = state.log_tx.send(WsEvent::BackupDone {
        instance_id: instance_id.to_string(),
        filename,
        size_bytes,
    });

    Ok(info)
}

fn create_tar_zst(source: &Path, output: &Path, world_only: bool) -> Result<u64, String> {
    let out_file =
        File::create(output).map_err(|e| format!("Failed to create backup file: {}", e))?;
    let encoder = zstd::Encoder::new(out_file, 3)
        .map_err(|e| format!("zstd encoder error: {}", e))?;
    let mut archive = tar::Builder::new(encoder);
    archive.follow_symlinks(false);

    if world_only {
        let entries =
            fs::read_dir(source).map_err(|e| format!("Cannot read server dir: {}", e))?;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if entry.path().is_dir() && name.starts_with("world") {
                archive
                    .append_dir_all(&name, entry.path())
                    .map_err(|e| format!("Failed to archive {}: {}", name, e))?;
            }
        }
    } else {
        archive
            .append_dir_all(".", source)
            .map_err(|e| format!("Failed to archive server dir: {}", e))?;
    }

    let encoder =
        archive.into_inner().map_err(|e| format!("Archive finalize error: {}", e))?;
    encoder
        .finish()
        .map_err(|e| format!("zstd finish error: {}", e))?;

    let size = fs::metadata(output)
        .map_err(|e| format!("Cannot stat backup file: {}", e))?
        .len();
    Ok(size)
}

fn apply_retention(instance_id: &str, keep_count: usize) {
    let backups = list_backups(instance_id);
    if backups.len() > keep_count {
        let dir = backup_dir(instance_id);
        for backup in &backups[keep_count..] {
            if let Err(e) = fs::remove_file(dir.join(&backup.filename)) {
                tracing::warn!("Failed to delete old backup {}: {}", backup.filename, e);
            }
        }
    }
}

pub async fn restore_backup(
    state: &AppState,
    instance_id: &str,
    filename: &str,
) -> Result<(), String> {
    // Validate filename to prevent path traversal
    if filename.contains('/') || filename.contains('\\') || filename.contains("..") {
        return Err("Invalid backup filename".to_string());
    }

    {
        let instances = state.instances.read().await;
        let inst = instances
            .get(instance_id)
            .ok_or_else(|| format!("Instance '{}' not found", instance_id))?;
        if !matches!(inst.status, InstanceStatus::Stopped | InstanceStatus::Crashed) {
            return Err("Instance must be stopped before restoring a backup".to_string());
        }
    }

    let server_path = {
        let instances = state.instances.read().await;
        instances
            .get(instance_id)
            .map(|i| i.config.server.path.clone())
            .ok_or_else(|| "Instance not found".to_string())?
    };

    let backup_path = backup_dir(instance_id).join(filename);
    if !backup_path.exists() {
        return Err(format!("Backup '{}' not found", filename));
    }

    let server_clone = server_path.clone();
    let backup_clone = backup_path.clone();

    tokio::task::spawn_blocking(move || extract_tar_zst(&backup_clone, &server_clone))
        .await
        .map_err(|e| format!("Restore task panicked: {}", e))?
        .map_err(|e| format!("Restore failed: {}", e))?;

    tracing::info!("Restored backup '{}' to instance '{}'", filename, instance_id);
    Ok(())
}

fn extract_tar_zst(backup_path: &Path, target_dir: &Path) -> Result<(), String> {
    let in_file =
        File::open(backup_path).map_err(|e| format!("Failed to open backup: {}", e))?;
    let decoder =
        zstd::Decoder::new(in_file).map_err(|e| format!("zstd decoder error: {}", e))?;
    let mut archive = tar::Archive::new(decoder);
    archive
        .unpack(target_dir)
        .map_err(|e| format!("Failed to extract backup: {}", e))?;
    Ok(())
}

fn normalize_cron(schedule: &str) -> String {
    // The cron crate uses 6-field format (sec min hour dom month dow).
    // Standard cron is 5-field (min hour dom month dow), so prepend "0 " for seconds.
    let fields: Vec<&str> = schedule.split_whitespace().collect();
    if fields.len() == 5 {
        format!("0 {}", schedule)
    } else {
        schedule.to_string()
    }
}

pub fn start_schedulers(state: Arc<AppState>) {
    tokio::spawn(async move {
        let scheduled: Vec<(String, String)> = {
            let instances = state.instances.read().await;
            instances
                .values()
                .filter_map(|inst| {
                    let backup = inst.config.backup.as_ref()?;
                    if !backup.enabled {
                        return None;
                    }
                    let schedule = backup.schedule.as_ref()?.clone();
                    Some((inst.id.clone(), schedule))
                })
                .collect()
        };

        for (id, schedule_str) in scheduled {
            let state = state.clone();
            tokio::spawn(async move {
                run_backup_scheduler(state, id, schedule_str).await;
            });
        }
    });
}

async fn run_backup_scheduler(state: Arc<AppState>, instance_id: String, schedule_str: String) {
    let normalized = normalize_cron(&schedule_str);
    let schedule = match Schedule::from_str(&normalized) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                "Invalid backup schedule for '{}': '{}' — {}",
                instance_id,
                schedule_str,
                e
            );
            return;
        }
    };

    for next in schedule.upcoming(Utc) {
        let now = Utc::now();
        let duration = (next - now).to_std().unwrap_or(Duration::ZERO);
        tokio::time::sleep(duration).await;
        tracing::info!("Running scheduled backup for '{}'", instance_id);
        trigger_backup(state.clone(), instance_id.clone()).await;
    }
}
