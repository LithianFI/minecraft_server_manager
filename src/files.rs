//! Browsing and editing a server's config files from the dashboard.
//!
//! Only text files that already exist under a few config folders can be read or changed:
//! paths are checked component by component, and resolved paths (following symlinks) must
//! stay inside their folder.

use std::path::{Component, Path, PathBuf};

use serde::Serialize;

/// Largest file the editor will open or save.
const MAX_FILE_BYTES: u64 = 1024 * 1024;

#[derive(Serialize)]
pub struct FileEntry {
    pub name: String,
    /// Path relative to the server directory, used for further requests
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
}

/// Top-level folders that may be browsed: common config locations plus the world's
/// `serverconfig` (per-world Forge/NeoForge settings).
fn roots(server_path: &Path) -> Vec<String> {
    let level = crate::datapack_mgr::read_level_name(server_path).unwrap_or_else(|| "world".to_string());
    vec![
        "config".to_string(),
        "defaultconfigs".to_string(),
        "kubejs".to_string(),
        format!("{}/serverconfig", level),
    ]
}

/// Resolves `relative` (e.g. "config/create-common.toml") to a path inside an allowed folder.
fn resolve(server_path: &Path, relative: &str) -> Result<PathBuf, String> {
    let rel = Path::new(relative.trim_matches('/'));
    if !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err("Invalid path".to_string());
    }
    let root = roots(server_path)
        .into_iter()
        .find(|r| rel.starts_with(r))
        .ok_or_else(|| "That folder can't be edited from the dashboard".to_string())?;

    let full = server_path.join(rel);
    // Symlinks must not lead out of the folder
    let real_root = server_path.join(&root).canonicalize().map_err(|_| format!("{} doesn't exist", root))?;
    let real = full.canonicalize().map_err(|_| format!("{} doesn't exist", relative))?;
    if !real.starts_with(&real_root) {
        return Err("Invalid path".to_string());
    }
    Ok(real)
}

/// Lists a folder; an empty `relative` lists the allowed top-level folders that exist.
pub fn list(server_path: &Path, relative: &str) -> Result<Vec<FileEntry>, String> {
    if relative.trim_matches('/').is_empty() {
        return Ok(roots(server_path)
            .into_iter()
            .filter(|r| server_path.join(r).is_dir())
            .map(|r| FileEntry { name: r.clone(), path: r, is_dir: true, size: 0 })
            .collect());
    }

    let dir = resolve(server_path, relative)?;
    let base = relative.trim_matches('/');
    let mut entries: Vec<FileEntry> = std::fs::read_dir(&dir)
        .map_err(|e| format!("Can't read {}: {}", base, e))?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let meta = e.metadata().ok()?;
            Some(FileEntry { path: format!("{}/{}", base, name), name, is_dir: meta.is_dir(), size: meta.len() })
        })
        .collect();
    // Folders first, then by name
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(entries)
}

pub fn read(server_path: &Path, relative: &str) -> Result<String, String> {
    let path = resolve(server_path, relative)?;
    let meta = std::fs::metadata(&path).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("Not a file".to_string());
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(format!("File is larger than {} KiB — edit it over SSH instead", MAX_FILE_BYTES / 1024));
    }
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    if bytes.contains(&0) {
        return Err("This looks like a binary file and can't be edited here".to_string());
    }
    String::from_utf8(bytes).map_err(|_| "This file isn't UTF-8 text and can't be edited here".to_string())
}

/// Replaces an existing text file. Writes a temporary file first and renames it over the
/// original, so a failure part-way through never leaves a truncated config behind.
pub fn write(server_path: &Path, relative: &str, content: &str) -> Result<(), String> {
    if content.len() as u64 > MAX_FILE_BYTES {
        return Err(format!("File is larger than {} KiB", MAX_FILE_BYTES / 1024));
    }
    // Only existing text files: resolve() fails for new paths, read() refuses binary ones
    read(server_path, relative)?;
    let path = resolve(server_path, relative)?;

    let tmp = path.with_file_name(format!(
        ".{}.msm-tmp",
        path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    ));
    std::fs::write(&tmp, content).map_err(|e| format!("Failed to save: {}", e))?;
    if let Ok(meta) = std::fs::metadata(&path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("Failed to save: {}", e)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("msm-files-test-{}-{}", std::process::id(), rand_suffix()));
        std::fs::create_dir_all(dir.join("config/sub")).unwrap();
        std::fs::create_dir_all(dir.join("world/serverconfig")).unwrap();
        std::fs::create_dir_all(dir.join("mods")).unwrap();
        std::fs::write(dir.join("config/a.toml"), "x = 1\n").unwrap();
        std::fs::write(dir.join("config/blob.bin"), [0u8, 1, 2]).unwrap();
        std::fs::write(dir.join("world/serverconfig/s.toml"), "y = 2\n").unwrap();
        std::fs::write(dir.join("mods/m.jar"), "jar").unwrap();
        std::fs::write(dir.join("server.properties"), "level-name=world\n").unwrap();
        dir
    }

    fn rand_suffix() -> u128 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    }

    #[test]
    fn lists_roots_and_folders() {
        let s = server();
        let roots: Vec<String> = list(&s, "").unwrap().into_iter().map(|e| e.path).collect();
        assert_eq!(roots, vec!["config", "world/serverconfig"]);
        let names: Vec<String> = list(&s, "config").unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["sub", "a.toml", "blob.bin"]);
    }

    #[test]
    fn reads_and_writes_text() {
        let s = server();
        assert_eq!(read(&s, "config/a.toml").unwrap(), "x = 1\n");
        write(&s, "config/a.toml", "x = 2\n").unwrap();
        assert_eq!(read(&s, "config/a.toml").unwrap(), "x = 2\n");
        write(&s, "world/serverconfig/s.toml", "y = 3\n").unwrap();
    }

    #[test]
    fn refuses_outside_paths_binary_and_new_files() {
        let s = server();
        assert!(read(&s, "mods/m.jar").is_err());
        assert!(read(&s, "server.properties").is_err());
        assert!(read(&s, "config/../server.properties").is_err());
        assert!(read(&s, "/etc/passwd").is_err());
        assert!(read(&s, "config/blob.bin").is_err());
        assert!(write(&s, "config/new.toml", "z").is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(s.join("server.properties"), s.join("config/link.toml")).unwrap();
            assert!(read(&s, "config/link.toml").is_err(), "symlink out of config must be refused");
        }
    }
}
