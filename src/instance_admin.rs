//! Duplicating and deleting instances.
//!
//! An instance lives in two places: its metadata dir (`instances/<id>/`, msm.toml and lock files)
//! and its server dir (`config.server.path`, usually `servers/<id>/`). Backups and metrics are
//! keyed by the instance ID.

use std::path::Path;
use std::sync::Arc;

use crate::config::{data_dir, load_instance_dir};
use crate::state::{AppState, InstanceInfo, InstanceStatus, WsEvent};

const MSM_TOML: &str = "msm.toml";

fn is_stopped(status: &InstanceStatus) -> bool {
    matches!(status, InstanceStatus::Stopped | InstanceStatus::Crashed)
}

// ── Duplicate ─────────────────────────────────────────────────────────────────

/// Copies an instance, server files and world included, to a new instance `new_id`.
/// The source must be stopped so the world is copied in a consistent state.
pub async fn duplicate(
    state: &Arc<AppState>,
    source_id: &str,
    new_id: &str,
    display_name: &str,
) -> Result<InstanceInfo, String> {
    let (mut config, source_server, source_meta) = {
        let instances = state.instances.read().await;
        if instances.contains_key(new_id) {
            return Err(format!("An instance named '{}' already exists", new_id));
        }
        let src = instances.get(source_id).ok_or_else(|| format!("Instance '{}' not found", source_id))?;
        if !is_stopped(&src.status) {
            return Err("Stop the server before duplicating it.".to_string());
        }
        (src.config.clone(), src.config.server.path.clone(), src.instance_dir.clone())
    };

    let new_meta = data_dir().join("instances").join(new_id);
    let new_server = data_dir().join("servers").join(new_id);
    if new_server.starts_with(&source_server) {
        return Err(format!("Can't copy '{}' into itself", source_server.display()));
    }
    if new_meta.exists() {
        return Err(format!("'{}' already exists", new_meta.display()));
    }

    tracing::info!("Duplicating '{}' to '{}'…", source_id, new_id);
    let (from, to) = (source_server.clone(), new_server.clone());
    tokio::task::spawn_blocking(move || copy_server_dir(&from, &to))
        .await
        .map_err(|e| format!("Copy task failed: {}", e))??;

    config.instance.name = new_id.to_string();
    config.instance.display_name = Some(display_name.to_string());
    config.server.path = new_server.clone();
    // A copy is usually a test server: don't hand its pack to players until sharing is
    // switched on again, which also issues it a fresh link
    if let Some(dist) = config.distribution.as_mut() {
        dist.enabled = false;
        dist.token.clear();
    }

    let written = write_metadata(&source_meta, &new_meta, &config).await;
    if let Err(e) = written {
        remove_partial(&new_meta, &new_server).await;
        return Err(e);
    }

    let Some((id, inst_state)) = load_instance_dir(&new_meta).await else {
        remove_partial(&new_meta, &new_server).await;
        return Err("The copied instance could not be loaded".to_string());
    };
    let info = InstanceInfo::from(&inst_state);
    state.instances.write().await.insert(id, inst_state);
    let _ = state.log_tx.send(WsEvent::InstanceAdded { instance: info.clone() });
    tracing::info!("Duplicated '{}' to '{}'", source_id, new_id);
    Ok(info)
}

/// Creates `to` (failing if it exists, so two copies can't race into one dir) and copies `from` into it.
fn copy_server_dir(from: &Path, to: &Path) -> Result<(), String> {
    if !from.is_dir() {
        return Err(format!("Server directory '{}' not found", from.display()));
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Failed to create '{}': {}", parent.display(), e))?;
    }
    std::fs::create_dir(to).map_err(|e| format!("Failed to create '{}': {}", to.display(), e))?;
    copy_dir_contents(from, to, &[]).inspect_err(|_| {
        let _ = std::fs::remove_dir_all(to);
    })
}

/// Copies the source's lock files etc. into `new_meta`, then writes msm.toml last: the
/// instance watcher picks up any dir with an msm.toml, and must not see a half-made one.
async fn write_metadata(source_meta: &Path, new_meta: &Path, config: &crate::config::InstanceConfig) -> Result<(), String> {
    let (from, to) = (source_meta.to_path_buf(), new_meta.to_path_buf());
    tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(&to).map_err(|e| format!("Failed to create '{}': {}", to.display(), e))?;
        copy_dir_contents(&from, &to, &[MSM_TOML])
    })
    .await
    .map_err(|e| format!("Copy task failed: {}", e))??;

    let toml_str = toml::to_string_pretty(config).map_err(|e| e.to_string())?;
    tokio::fs::write(new_meta.join(MSM_TOML), toml_str)
        .await
        .map_err(|e| format!("Failed to write msm.toml: {}", e))
}

async fn remove_partial(meta: &Path, server: &Path) {
    let _ = tokio::fs::remove_dir_all(meta).await;
    let _ = tokio::fs::remove_dir_all(server).await;
}

/// Recursively copies the entries of `from` into the existing dir `to`, skipping top-level
/// names in `skip`. Symlinks are recreated as links rather than followed.
fn copy_dir_contents(from: &Path, to: &Path, skip: &[&str]) -> Result<(), String> {
    let entries = std::fs::read_dir(from).map_err(|e| format!("Failed to read '{}': {}", from.display(), e))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("Failed to read '{}': {}", from.display(), e))?;
        if skip.iter().any(|s| entry.file_name() == *s) {
            continue;
        }
        copy_entry(&entry.path(), &to.join(entry.file_name()))?;
    }
    Ok(())
}

fn copy_entry(from: &Path, to: &Path) -> Result<(), String> {
    let context = |e: std::io::Error| format!("Failed to copy '{}': {}", from.display(), e);
    let file_type = std::fs::symlink_metadata(from).map_err(context)?.file_type();
    if file_type.is_symlink() {
        let target = std::fs::read_link(from).map_err(context)?;
        return make_symlink(&target, to).map_err(context);
    }
    if file_type.is_dir() {
        std::fs::create_dir(to).map_err(context)?;
        return copy_dir_contents(from, to, &[]);
    }
    // fs::copy keeps permissions, so run.sh stays executable
    std::fs::copy(from, to).map(|_| ()).map_err(context)
}

#[cfg(unix)]
fn make_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(unix))]
fn make_symlink(_target: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other("symbolic links are only supported on Unix"))
}

// ── Delete ────────────────────────────────────────────────────────────────────

pub struct DeleteOptions {
    pub delete_server_files: bool,
    pub delete_backups: bool,
}

/// Removes an instance from MSM. Server files and backups are only deleted when asked for.
pub async fn delete(state: &Arc<AppState>, id: &str, opts: DeleteOptions) -> Result<(), String> {
    let trash = data_dir().join(format!(".deleting-{}-{}", id, chrono::Utc::now().timestamp_millis()));
    let has_process = state.processes.lock().await.contains_key(id);
    let server_path = {
        let mut instances = state.instances.write().await;
        let inst = instances.get(id).ok_or_else(|| format!("Instance '{}' not found", id))?;
        if !is_stopped(&inst.status) || has_process {
            return Err("Stop the server before deleting it.".to_string());
        }
        let server_path = inst.config.server.path.clone();
        let in_meta_dir = server_path.starts_with(&inst.instance_dir);
        if in_meta_dir && !opts.delete_server_files {
            return Err("This server's files are inside its MSM folder, so they can only be deleted together.".to_string());
        }
        if opts.delete_server_files {
            let others: Vec<&Path> = instances.values().filter(|i| i.id != id).map(|i| i.config.server.path.as_path()).collect();
            check_server_dir_deletable(&server_path, &data_dir(), &others)?;
        }
        // Move the metadata dir out of instances/ while holding the lock, so the instance
        // watcher can't rediscover it mid-delete
        std::fs::rename(&inst.instance_dir, &trash)
            .map_err(|e| format!("Failed to remove '{}': {}", inst.instance_dir.display(), e))?;
        instances.remove(id);
        server_path
    };

    state.client_packs.lock().await.remove(id);
    state.metrics_db.delete_instance(id);
    let _ = state.log_tx.send(WsEvent::InstanceRemoved { instance_id: id.to_string() });
    tracing::info!("Removed instance '{}'", id);

    let mut failures = vec![];
    if let Err(e) = remove_dir(&trash).await {
        failures.push(e);
    }
    if opts.delete_server_files {
        match remove_dir(&server_path).await {
            Ok(()) => tracing::info!("Deleted server files at '{}'", server_path.display()),
            Err(e) => failures.push(e),
        }
    }
    if opts.delete_backups {
        match remove_dir(&crate::backup::backup_dir(id)).await {
            Ok(()) => tracing::info!("Deleted backups of '{}'", id),
            Err(e) => failures.push(e),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!("The server was removed, but some files could not be deleted: {}", failures.join("; ")))
    }
}

async fn remove_dir(path: &Path) -> Result<(), String> {
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("'{}': {}", path.display(), e)),
    }
}

/// Guards against deleting more than one server's files: refuses paths that hold MSM's own
/// data, the filesystem root, or another instance's server files.
fn check_server_dir_deletable(server: &Path, data_dir: &Path, other_servers: &[&Path]) -> Result<(), String> {
    let refuse = |why: &str| Err(format!("Won't delete '{}': {}", server.display(), why));
    if !server.is_absolute() || server.parent().is_none() {
        return refuse("not a server directory");
    }
    if data_dir.starts_with(server) {
        return refuse("it contains MSM's data");
    }
    if other_servers.iter().any(|other| other.starts_with(server) || server.starts_with(other)) {
        return refuse("another instance uses files in it");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("msm-admin-test-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn deletable_checks() {
        let data = Path::new("/home/u/.local/share/msm");
        let ok = Path::new("/home/u/.local/share/msm/servers/a");
        assert!(check_server_dir_deletable(ok, data, &[Path::new("/home/u/.local/share/msm/servers/b")]).is_ok());
        assert!(check_server_dir_deletable(Path::new("/"), data, &[]).is_err());
        assert!(check_server_dir_deletable(Path::new("/home/u"), data, &[]).is_err());
        assert!(check_server_dir_deletable(data, data, &[]).is_err());
        assert!(check_server_dir_deletable(Path::new("relative/dir"), data, &[]).is_err());
        // Shared with, inside, or containing another instance's server
        assert!(check_server_dir_deletable(ok, data, &[ok]).is_err());
        assert!(check_server_dir_deletable(ok, data, &[&ok.join("sub")]).is_err());
        assert!(check_server_dir_deletable(&ok.join("sub"), data, &[ok]).is_err());
    }

    #[test]
    fn copies_tree_with_permissions_and_symlinks() {
        let root = scratch("copy");
        let src = root.join("src");
        std::fs::create_dir_all(src.join("world/region")).unwrap();
        std::fs::write(src.join("world/region/r.0.0.mca"), b"chunk").unwrap();
        std::fs::write(src.join("run.sh"), b"#!/bin/sh\n").unwrap();
        std::fs::write(src.join(MSM_TOML), b"x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(src.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
            std::os::unix::fs::symlink("world", src.join("link")).unwrap();
        }

        let dst = root.join("dst");
        copy_server_dir(&src, &dst).unwrap();
        assert_eq!(std::fs::read(dst.join("world/region/r.0.0.mca")).unwrap(), b"chunk");
        assert!(dst.join(MSM_TOML).exists(), "server copy keeps everything");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dst.join("run.sh")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
            assert_eq!(std::fs::read_link(dst.join("link")).unwrap(), Path::new("world"));
        }
        // The destination must not already exist
        assert!(copy_server_dir(&src, &dst).is_err());

        let meta = root.join("meta");
        std::fs::create_dir(&meta).unwrap();
        copy_dir_contents(&src, &meta, &[MSM_TOML]).unwrap();
        assert!(!meta.join(MSM_TOML).exists());
        assert!(meta.join("run.sh").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
