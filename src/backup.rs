use std::collections::HashMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use cron::Schedule;
use serde::{Deserialize, Serialize};

use crate::config::data_dir;
use crate::state::{AppState, InstanceStatus, WsEvent};

#[derive(Debug, Clone, Serialize)]
pub struct BackupInfo {
    pub filename: String,
    pub size_bytes: u64,
    pub created_at: i64,
    /// e.g. "Before updating SkyBlock Plus to 1.0.9"
    pub label: Option<String>,
    /// Kept out of the normal keep-N rotation (pre-update backups)
    pub pinned: bool,
    /// None for backups made before MSM recorded this
    pub world_only: Option<bool>,
}

/// Stored next to each archive as `<archive>.json`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct BackupMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(default)]
    pinned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    world_only: Option<bool>,
}

/// The instance settings that must match the server files, so a rollback puts them back too.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SettingsSnapshot {
    minecraft_version: String,
    loader: Option<String>,
    loader_version: Option<String>,
    modrinth_version_id: Option<String>,
    java_path: Option<String>,
}

/// Stored inside every archive as `.msm/backup.json`.
#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    instance_id: String,
    world_only: bool,
    settings: SettingsSnapshot,
}

const MANIFEST_PATH: &str = ".msm/backup.json";
/// How many pinned (pre-update) backups to keep per instance
const KEEP_PINNED: usize = 3;

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
            let meta = read_meta(&path);
            infos.push(BackupInfo {
                filename,
                size_bytes,
                created_at,
                label: meta.label,
                pinned: meta.pinned,
                world_only: meta.world_only,
            });
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
    create_backup(state, instance_id, None).await
}

/// A full, pinned backup taken before an update, so the update can be rolled back completely:
/// every server file plus the instance's version/loader/Java settings. Pinned backups are kept
/// out of the normal rotation (the newest few are kept).
pub async fn do_create_update_backup(state: &AppState, instance_id: &str, label: String) -> Result<BackupInfo, String> {
    create_backup(state, instance_id, Some(label)).await
}

async fn create_backup(state: &AppState, instance_id: &str, update_label: Option<String>) -> Result<BackupInfo, String> {
    let pinned = update_label.is_some();
    let (server_path, world_only, keep_count, running, settings) = {
        let instances = state.instances.read().await;
        let inst = instances
            .get(instance_id)
            .ok_or_else(|| format!("Instance '{}' not found", instance_id))?;
        let backup = &inst.config.backup;
        let meta = &inst.config.instance;
        (
            inst.config.server.path.clone(),
            // Update backups are always full: a rollback must also undo mod and loader changes
            !pinned && backup.as_ref().map(|b| b.world_only).unwrap_or(false),
            backup.as_ref().map(|b| b.keep_count).unwrap_or(10),
            inst.status == InstanceStatus::Running,
            SettingsSnapshot {
                minecraft_version: meta.minecraft_version.clone(),
                loader: meta.loader.clone(),
                loader_version: meta.loader_version.clone(),
                modrinth_version_id: meta.modrinth_version_id.clone(),
                java_path: inst.config.server.java_path.clone(),
            },
        )
    };
    let manifest = serde_json::to_vec_pretty(&Manifest {
        instance_id: instance_id.to_string(),
        world_only,
        settings,
    })
    .map_err(|e| e.to_string())?;

    let dir = backup_dir(instance_id);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("Failed to create backup dir: {}", e))?;

    let filename = backup_filename();
    let output_path = dir.join(&filename);
    let output_clone = output_path.clone();
    let server_clone = server_path.clone();

    // A running server keeps writing chunks: pause saving and flush everything
    // to disk first so the archive holds a consistent world.
    if running {
        pause_saving(state, instance_id).await?;
    }

    let archived = tokio::task::spawn_blocking(move || {
        create_tar_zst(&server_clone, &output_clone, world_only, &manifest)
    })
    .await;

    if running {
        send_to_server(state, instance_id, "save-on").await;
    }

    let size_bytes = archived
        .map_err(|e| format!("Backup task panicked: {}", e))?
        .map_err(|e| format!("Failed to create archive: {}", e))?;

    let meta = BackupMeta { label: update_label, pinned, world_only: Some(world_only) };
    write_meta(&output_path, &meta)?;
    apply_retention(instance_id, keep_count);

    let created_at = parse_backup_timestamp(&filename).unwrap_or_else(|| Utc::now().timestamp());
    let info = BackupInfo {
        filename: filename.clone(),
        size_bytes,
        created_at,
        label: meta.label,
        pinned,
        world_only: Some(world_only),
    };

    let _ = state.log_tx.send(WsEvent::BackupDone {
        instance_id: instance_id.to_string(),
        filename,
        size_bytes,
    });

    Ok(info)
}

async fn send_to_server(state: &AppState, instance_id: &str, command: &str) -> bool {
    let processes = state.processes.lock().await;
    processes
        .get(instance_id)
        .is_some_and(|h| h.stdin_tx.send(command.to_string()).is_ok())
}

/// Sends `save-off` + `save-all flush` and waits for the server to confirm the save.
async fn pause_saving(state: &AppState, instance_id: &str) -> Result<(), String> {
    // Subscribe before sending so the confirmation line can't be missed
    let mut rx = state.log_tx.subscribe();

    if !send_to_server(state, instance_id, "save-off").await
        || !send_to_server(state, instance_id, "save-all flush").await
    {
        return Err("Server stopped before the backup could start".to_string());
    }

    let confirmed = tokio::time::timeout(std::time::Duration::from_secs(120), async {
        loop {
            match rx.recv().await {
                Ok(WsEvent::LogLine { instance_id: id, line, .. })
                    if id == instance_id && line.contains("Saved the game") => return true,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return false,
            }
        }
    })
    .await;

    if confirmed != Ok(true) {
        send_to_server(state, instance_id, "save-on").await;
        return Err("Server did not confirm the world save (save-all flush) in time".to_string());
    }
    Ok(())
}

fn meta_path(archive: &Path) -> PathBuf {
    let mut name = archive.as_os_str().to_owned();
    name.push(".json");
    PathBuf::from(name)
}

fn read_meta(archive: &Path) -> BackupMeta {
    fs::read_to_string(meta_path(archive))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write_meta(archive: &Path, meta: &BackupMeta) -> Result<(), String> {
    let json = serde_json::to_string_pretty(meta).map_err(|e| e.to_string())?;
    fs::write(meta_path(archive), json).map_err(|e| format!("Failed to write backup info: {}", e))
}

/// The world folder named by `level-name` in server.properties
fn level_name(server: &Path) -> String {
    crate::datapack_mgr::read_level_name(server).unwrap_or_else(|| "world".to_string())
}

/// The world folder and its separate dimension folders (`<level>_nether`, `<level>_the_end`).
/// `world*` folders always count, so backups made before `level-name` was read still restore.
fn is_world_dir(name: &str, level: &str) -> bool {
    name == level || name.starts_with(&format!("{}_", level)) || name.starts_with("world")
}

fn create_tar_zst(source: &Path, output: &Path, world_only: bool, manifest: &[u8]) -> Result<u64, String> {
    let out_file =
        File::create(output).map_err(|e| format!("Failed to create backup file: {}", e))?;
    let encoder = zstd::Encoder::new(out_file, 3)
        .map_err(|e| format!("zstd encoder error: {}", e))?;
    let mut archive = tar::Builder::new(encoder);
    archive.follow_symlinks(false);

    // Written first so a restore can read it without scanning the whole archive
    let mut header = tar::Header::new_gnu();
    header.set_size(manifest.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(Utc::now().timestamp() as u64);
    header.set_cksum();
    archive
        .append_data(&mut header, MANIFEST_PATH, manifest)
        .map_err(|e| format!("Failed to write backup manifest: {}", e))?;

    if world_only {
        let level = level_name(source);
        let entries =
            fs::read_dir(source).map_err(|e| format!("Cannot read server dir: {}", e))?;
        let mut found = false;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if entry.path().is_dir() && is_world_dir(&name, &level) {
                archive
                    .append_dir_all(&name, entry.path())
                    .map_err(|e| format!("Failed to archive {}: {}", name, e))?;
                found = true;
            }
        }
        if !found {
            drop(archive);
            let _ = fs::remove_file(output);
            return Err(format!("No world folder '{}' found in the server directory", level));
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

pub fn delete_backup(instance_id: &str, filename: &str) -> Result<(), String> {
    if filename.contains('/') || filename.contains('\\') || filename.contains("..") {
        return Err("Invalid backup filename".to_string());
    }
    let path = backup_dir(instance_id).join(filename);
    if !path.exists() {
        return Err("Backup not found".to_string());
    }
    fs::remove_file(&path).map_err(|e| format!("Failed to delete backup: {}", e))?;
    let _ = fs::remove_file(meta_path(&path));
    Ok(())
}

pub fn backup_path(instance_id: &str, filename: &str) -> Result<PathBuf, String> {
    if filename.contains('/') || filename.contains('\\') || filename.contains("..") {
        return Err("Invalid backup filename".to_string());
    }
    let path = backup_dir(instance_id).join(filename);
    if !path.exists() {
        return Err("Backup not found".to_string());
    }
    Ok(path)
}

pub async fn copy_backup(src_instance: &str, filename: &str, dst_instance: &str) -> Result<(), String> {
    let src = backup_path(src_instance, filename)?;
    let dst_dir = backup_dir(dst_instance);
    tokio::fs::create_dir_all(&dst_dir)
        .await
        .map_err(|e| format!("Failed to create backup dir: {}", e))?;
    tokio::fs::copy(&src, dst_dir.join(filename))
        .await
        .map_err(|e| format!("Failed to copy backup: {}", e))?;
    Ok(())
}

/// Keeps the newest `keep_count` regular backups and the newest [`KEEP_PINNED`] pinned ones;
/// the two are counted separately so routine backups never push out a pre-update backup.
fn apply_retention(instance_id: &str, keep_count: usize) {
    let backups = list_backups(instance_id); // newest first
    let (pinned, regular): (Vec<_>, Vec<_>) = backups.iter().partition(|b| b.pinned);
    let dir = backup_dir(instance_id);
    for backup in regular.iter().skip(keep_count).chain(pinned.iter().skip(KEEP_PINNED)) {
        let path = dir.join(&backup.filename);
        if let Err(e) = fs::remove_file(&path) {
            tracing::warn!("Failed to delete old backup {}: {}", backup.filename, e);
        }
        let _ = fs::remove_file(meta_path(&path));
    }
}

#[derive(Debug, Serialize)]
pub struct RestoreOutcome {
    /// The backup replaced the whole server directory (otherwise only the world folders)
    pub full: bool,
    /// The instance's version/loader/Java settings were put back from the backup
    pub settings_restored: bool,
}

/// Restores a backup so the server matches it exactly: files added since the backup are
/// removed (for a world-only backup, only the world folders are replaced), and the instance's
/// version/loader/Java settings are put back when the backup recorded them.
pub async fn restore_backup(
    state: &AppState,
    instance_id: &str,
    filename: &str,
) -> Result<RestoreOutcome, String> {
    // Validate filename to prevent path traversal
    if filename.contains('/') || filename.contains('\\') || filename.contains("..") {
        return Err("Invalid backup filename".to_string());
    }

    let server_path = {
        let instances = state.instances.read().await;
        let inst = instances
            .get(instance_id)
            .ok_or_else(|| format!("Instance '{}' not found", instance_id))?;
        if !matches!(inst.status, InstanceStatus::Stopped | InstanceStatus::Crashed) {
            return Err("Instance must be stopped before restoring a backup".to_string());
        }
        inst.config.server.path.clone()
    };

    let backup_path = backup_dir(instance_id).join(filename);
    if !backup_path.exists() {
        return Err(format!("Backup '{}' not found", filename));
    }

    let (manifest, world_only) = tokio::task::spawn_blocking({
        let b = backup_path.clone();
        move || inspect_archive(&b)
    })
    .await
    .map_err(|e| format!("Restore task panicked: {}", e))??;

    tokio::task::spawn_blocking(move || clean_restore(&backup_path, &server_path, world_only))
        .await
        .map_err(|e| format!("Restore task panicked: {}", e))?
        .map_err(|e| format!("Restore failed: {}", e))?;

    // Settings only follow a backup of this same instance (not one copied from elsewhere)
    let mut settings_restored = false;
    if let Some(m) = manifest.filter(|m| m.instance_id == instance_id) {
        let mut instances = state.instances.write().await;
        if let Some(inst) = instances.get_mut(instance_id) {
            let meta = &mut inst.config.instance;
            meta.minecraft_version = m.settings.minecraft_version;
            meta.loader = m.settings.loader;
            meta.loader_version = m.settings.loader_version;
            meta.modrinth_version_id = m.settings.modrinth_version_id;
            inst.config.server.java_path = m.settings.java_path;
            let toml_str = toml::to_string_pretty(&inst.config).map_err(|e| e.to_string())?;
            fs::write(inst.instance_dir.join("msm.toml"), toml_str)
                .map_err(|e| format!("Files restored, but saving msm.toml failed: {}", e))?;
            let _ = state.log_tx.send(WsEvent::InstanceUpdated {
                instance: crate::state::InstanceInfo::from(&*inst),
            });
            settings_restored = true;
        }
    }

    tracing::info!("Restored backup '{}' to instance '{}'", filename, instance_id);
    Ok(RestoreOutcome { full: !world_only, settings_restored })
}

fn open_archive(path: &Path) -> Result<tar::Archive<zstd::Decoder<'static, std::io::BufReader<File>>>, String> {
    let file = File::open(path).map_err(|e| format!("Failed to open backup: {}", e))?;
    let decoder = zstd::Decoder::new(file).map_err(|e| format!("zstd decoder error: {}", e))?;
    Ok(tar::Archive::new(decoder))
}

/// First path component that isn't `.`: "./mods/x.jar" → "mods".
fn top_level(path: &Path) -> Option<String> {
    path.components()
        .find_map(|c| match c { std::path::Component::Normal(n) => Some(n.to_string_lossy().into_owned()), _ => None })
}

/// Reads the manifest, and whether the backup holds only world folders. Backups made before
/// manifests existed are classified by their contents.
fn inspect_archive(path: &Path) -> Result<(Option<Manifest>, bool), String> {
    let mut archive = open_archive(path)?;
    let mut all_world = true;
    for entry in archive.entries().map_err(|e| format!("Failed to read backup: {}", e))? {
        let mut entry = entry.map_err(|e| format!("Failed to read backup: {}", e))?;
        let entry_path = entry.path().map_err(|e| e.to_string())?.into_owned();
        if entry_path.ends_with(MANIFEST_PATH) {
            let mut json = String::new();
            std::io::Read::read_to_string(&mut entry, &mut json).map_err(|e| e.to_string())?;
            let manifest: Manifest = serde_json::from_str(&json).map_err(|e| format!("Invalid backup manifest: {}", e))?;
            let world_only = manifest.world_only;
            return Ok((Some(manifest), world_only));
        }
        match top_level(&entry_path) {
            Some(name) if !name.starts_with("world") => { all_world = false; break; }
            _ => {}
        }
    }
    Ok((None, all_world))
}

const STAGING_DIR: &str = ".msm-restore-staging";
const PREVIOUS_DIR: &str = ".msm-restore-previous";

/// Unpacks into a staging folder, moves the current files aside, moves the restored ones in, and
/// only then deletes the old files. If a step fails, the moves are undone.
fn clean_restore(backup: &Path, server: &Path, world_only: bool) -> Result<(), String> {
    fs::create_dir_all(server).map_err(|e| format!("Can't create {}: {}", server.display(), e))?;
    let staging = server.join(STAGING_DIR);
    let previous = server.join(PREVIOUS_DIR);
    for leftover in [&staging, &previous] {
        if leftover.exists() {
            fs::remove_dir_all(leftover).map_err(|e| format!("Can't clear {}: {}", leftover.display(), e))?;
        }
    }
    fs::create_dir_all(&staging).map_err(|e| e.to_string())?;

    let unpacked = (|| -> Result<(), String> {
        let mut archive = open_archive(backup)?;
        archive.set_preserve_permissions(true);
        for entry in archive.entries().map_err(|e| format!("Failed to read backup: {}", e))? {
            let mut entry = entry.map_err(|e| format!("Failed to read backup: {}", e))?;
            let entry_path = entry.path().map_err(|e| e.to_string())?.into_owned();
            if top_level(&entry_path).as_deref() == Some(".msm") {
                continue;
            }
            // unpack_in refuses paths that would land outside the staging folder
            entry.unpack_in(&staging).map_err(|e| format!("Failed to extract {}: {}", entry_path.display(), e))?;
        }
        Ok(())
    })();
    if let Err(e) = unpacked {
        let _ = fs::remove_dir_all(&staging);
        return Err(e);
    }

    let level = level_name(server);
    let replaced = |name: &str| name != STAGING_DIR && name != PREVIOUS_DIR && (!world_only || is_world_dir(name, &level));
    let names_in = |dir: &Path| -> Result<Vec<String>, String> {
        Ok(fs::read_dir(dir).map_err(|e| e.to_string())?
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect())
    };

    fs::create_dir_all(&previous).map_err(|e| e.to_string())?;
    let mut moved_out = Vec::new();
    let mut moved_in = Vec::new();
    let swap = (|| -> Result<(), String> {
        for name in names_in(server)?.into_iter().filter(|n| replaced(n)) {
            fs::rename(server.join(&name), previous.join(&name)).map_err(|e| format!("Can't move {} aside: {}", name, e))?;
            moved_out.push(name);
        }
        for name in names_in(&staging)? {
            fs::rename(staging.join(&name), server.join(&name)).map_err(|e| format!("Can't move {} into place: {}", name, e))?;
            moved_in.push(name);
        }
        Ok(())
    })();

    if let Err(e) = swap {
        // Put everything back the way it was
        for name in &moved_in {
            let _ = fs::rename(server.join(name), staging.join(name));
        }
        for name in &moved_out {
            let _ = fs::rename(previous.join(name), server.join(name));
        }
        let _ = fs::remove_dir_all(&staging);
        let _ = fs::remove_dir(&previous);
        return Err(e);
    }

    let _ = fs::remove_dir_all(&previous);
    let _ = fs::remove_dir_all(&staging);
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
    tokio::spawn(run_backup_scheduler(state));
}

/// Re-reads every instance's backup settings on each tick, so enabling, disabling or
/// rescheduling backups (or adding an instance) takes effect without restarting MSM.
async fn run_backup_scheduler(state: Arc<AppState>) {
    // Parsed schedules, so an invalid one is only warned about once
    let mut parsed: HashMap<String, Option<Schedule>> = HashMap::new();
    let mut last_check = Utc::now();

    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;
        let now = Utc::now();

        let configured: Vec<(String, String, bool)> = {
            let instances = state.instances.read().await;
            instances
                .values()
                .filter_map(|inst| {
                    let backup = inst.config.backup.as_ref()?;
                    if !backup.enabled {
                        return None;
                    }
                    let schedule = backup.effective_schedule().to_string();
                    Some((inst.id.clone(), schedule, matches!(inst.status, InstanceStatus::Running)))
                })
                .collect()
        };

        for (instance_id, schedule_str, is_running) in configured {
            let schedule = parsed.entry(schedule_str.clone()).or_insert_with(|| {
                Schedule::from_str(&normalize_cron(&schedule_str))
                    .inspect_err(|e| {
                        tracing::warn!("Invalid backup schedule for '{}': '{}' — {}", instance_id, schedule_str, e)
                    })
                    .ok()
            });
            // Due if a scheduled time fell between the previous check and now
            let due = schedule
                .as_ref()
                .and_then(|s| s.after(&last_check).next())
                .is_some_and(|next| next <= now);
            if !due {
                continue;
            }

            // Only back up the active server
            if !is_running {
                tracing::debug!("Skipping scheduled backup for '{}': not running", instance_id);
                continue;
            }

            tracing::info!("Running scheduled backup for '{}'", instance_id);
            tokio::spawn(trigger_backup(state.clone(), instance_id));
        }

        last_check = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let d = std::env::temp_dir().join(format!("msm-backup-{}-{}-{}", tag, std::process::id(), nanos));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// Relative path → content for every file under `dir`
    fn snapshot(dir: &Path) -> std::collections::BTreeMap<String, String> {
        fn walk(root: &Path, dir: &Path, out: &mut std::collections::BTreeMap<String, String>) {
            for e in fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() { walk(root, &p, out) } else {
                    out.insert(p.strip_prefix(root).unwrap().to_string_lossy().into_owned(), fs::read_to_string(&p).unwrap_or_default());
                }
            }
        }
        let mut out = Default::default();
        walk(dir, dir, &mut out);
        out
    }

    fn manifest(world_only: bool) -> Vec<u8> {
        serde_json::to_vec(&Manifest {
            instance_id: "test".into(),
            world_only,
            settings: SettingsSnapshot {
                minecraft_version: "26.1".into(), loader: Some("fabric".into()), loader_version: Some("0.18.5".into()),
                modrinth_version_id: Some("old".into()), java_path: None,
            },
        }).unwrap()
    }

    fn server() -> PathBuf {
        let s = temp_dir("server");
        write(&s.join("mods/pack-1.0.jar"), "old mod");
        write(&s.join("mods/removed-later.jar"), "gone after update");
        write(&s.join("config/pack.toml"), "setting = 1");
        write(&s.join("world/region/r.0.0.mca"), "old chunks");
        write(&s.join("run.sh"), "java old");
        s
    }

    #[test]
    fn full_restore_matches_backup_exactly() {
        let s = server();
        let before = snapshot(&s);
        let archive = temp_dir("out").join("b.tar.zst");
        create_tar_zst(&s, &archive, false, &manifest(false)).unwrap();

        // Simulate an update: add, change and delete files
        write(&s.join("mods/pack-2.0.jar"), "new mod");
        write(&s.join("config/pack.toml"), "setting = 2");
        write(&s.join("world/region/r.1.0.mca"), "newly explored, new format");
        fs::remove_file(s.join("mods/removed-later.jar")).unwrap();
        write(&s.join("run.sh"), "java new");

        let (m, world_only) = inspect_archive(&archive).unwrap();
        assert!(!world_only);
        assert_eq!(m.unwrap().settings.modrinth_version_id.as_deref(), Some("old"));
        clean_restore(&archive, &s, world_only).unwrap();

        assert_eq!(snapshot(&s), before, "server directory should match the backup exactly");
        assert!(!s.join(STAGING_DIR).exists() && !s.join(PREVIOUS_DIR).exists());
        assert!(!s.join(".msm").exists(), "manifest must not be extracted into the server");
    }

    #[test]
    fn world_only_restore_leaves_other_files() {
        let s = server();
        let archive = temp_dir("out").join("w.tar.zst");
        create_tar_zst(&s, &archive, true, &manifest(true)).unwrap();

        write(&s.join("world/region/r.1.0.mca"), "newer chunk");
        write(&s.join("mods/pack-2.0.jar"), "new mod");

        let (_, world_only) = inspect_archive(&archive).unwrap();
        assert!(world_only);
        clean_restore(&archive, &s, world_only).unwrap();

        assert!(!s.join("world/region/r.1.0.mca").exists(), "world replaced by the backup's");
        assert!(s.join("world/region/r.0.0.mca").exists());
        assert!(s.join("mods/pack-2.0.jar").exists(), "non-world files untouched");
    }

    #[test]
    fn world_only_uses_level_name() {
        let s = temp_dir("server");
        write(&s.join("server.properties"), "motd=hi\nlevel-name=Crooked\n");
        write(&s.join("Crooked/region/r.0.0.mca"), "old chunks");
        write(&s.join("Crooked_nether/region/r.0.0.mca"), "nether");
        write(&s.join("mods/pack.jar"), "mod");
        let archive = temp_dir("out").join("w.tar.zst");
        create_tar_zst(&s, &archive, true, &manifest(true)).unwrap();

        write(&s.join("Crooked/region/r.1.0.mca"), "newer chunk");
        write(&s.join("mods/pack-2.0.jar"), "new mod");
        clean_restore(&archive, &s, true).unwrap();

        assert!(s.join("Crooked/region/r.0.0.mca").exists());
        assert!(!s.join("Crooked/region/r.1.0.mca").exists(), "world replaced by the backup's");
        assert!(s.join("Crooked_nether/region/r.0.0.mca").exists());
        assert!(s.join("mods/pack-2.0.jar").exists(), "non-world files untouched");

        // No world folder at all: refuse rather than save an empty backup
        fs::remove_dir_all(s.join("Crooked")).unwrap();
        fs::remove_dir_all(s.join("Crooked_nether")).unwrap();
        let empty = temp_dir("out").join("e.tar.zst");
        assert!(create_tar_zst(&s, &empty, true, &manifest(true)).is_err());
        assert!(!empty.exists());
    }

    #[test]
    fn classifies_legacy_backups_without_manifest() {
        let s = server();
        let out = temp_dir("out");
        // Old-format archives: same as today's minus the manifest entry
        for (name, world_only) in [("full.tar.zst", false), ("world.tar.zst", true)] {
            let path = out.join(name);
            let enc = zstd::Encoder::new(File::create(&path).unwrap(), 3).unwrap();
            let mut tar = tar::Builder::new(enc);
            if world_only { tar.append_dir_all("world", s.join("world")).unwrap(); } else { tar.append_dir_all(".", &s).unwrap(); }
            tar.into_inner().unwrap().finish().unwrap();
            let (m, detected) = inspect_archive(&path).unwrap();
            assert!(m.is_none());
            assert_eq!(detected, world_only, "{name}");
        }
    }

    #[test]
    fn failed_restore_leaves_server_untouched() {
        let s = server();
        let before = snapshot(&s);
        let bogus = temp_dir("out").join("broken.tar.zst");
        fs::write(&bogus, b"not an archive").unwrap();
        assert!(clean_restore(&bogus, &s, false).is_err());
        assert_eq!(snapshot(&s), before);
    }
}
