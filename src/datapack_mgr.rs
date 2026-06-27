use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatapackEntry {
    pub name: String,
    pub modrinth_project_id: String,
    pub modrinth_version_id: String,
    pub version_number: String,
    pub filename: String,
    pub sha512: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DatapackLock {
    #[serde(default)]
    pub datapacks: Vec<DatapackEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DatapackUpdate {
    pub project_id: String,
    pub name: String,
    pub installed_version_id: String,
    pub installed_version_number: String,
    pub latest_version_id: String,
    pub latest_version_number: String,
    pub download_url: String,
    pub filename: String,
    pub sha512: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatapackSearchHit {
    pub project_id: String,
    pub slug: String,
    pub title: String,
    pub description: String,
    pub downloads: u64,
    pub icon_url: Option<String>,
}

// ── Modrinth response types ───────────────────────────────────────────────────

#[derive(Deserialize)]
struct MrFileHashes {
    sha512: String,
}

#[derive(Deserialize)]
struct MrFile {
    url: String,
    filename: String,
    hashes: MrFileHashes,
    primary: bool,
}

#[derive(Deserialize)]
struct MrVersion {
    id: String,
    project_id: String,
    version_number: String,
    files: Vec<MrFile>,
}

#[derive(Deserialize)]
struct MrProject {
    id: String,
    title: String,
}

#[derive(Deserialize)]
struct MrSearchResponse {
    hits: Vec<MrSearchHit>,
}

#[derive(Deserialize)]
struct MrSearchHit {
    project_id: String,
    slug: String,
    title: String,
    description: String,
    downloads: u64,
    icon_url: Option<String>,
}

// ── Datapacks directory discovery ─────────────────────────────────────────────

pub fn find_datapacks_dir(server_path: &Path) -> PathBuf {
    let default = server_path.join("world").join("datapacks");
    if default.exists() {
        return default;
    }
    let level_name = read_level_name(server_path).unwrap_or_else(|| "world".to_string());
    server_path.join(level_name).join("datapacks")
}

fn read_level_name(server_path: &Path) -> Option<String> {
    let content = fs::read_to_string(server_path.join("server.properties")).ok()?;
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == "level-name" {
                let v = v.trim().to_string();
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
    }
    None
}

// ── Lock file I/O ─────────────────────────────────────────────────────────────

pub fn read_lock(instance_dir: &Path) -> DatapackLock {
    let path = instance_dir.join("datapacks.lock.toml");
    let Ok(content) = fs::read_to_string(&path) else {
        return DatapackLock::default();
    };
    toml::from_str(&content).unwrap_or_default()
}

pub fn write_lock(instance_dir: &Path, lock: &DatapackLock) -> Result<(), String> {
    let content = toml::to_string_pretty(lock).map_err(|e| e.to_string())?;
    fs::write(instance_dir.join("datapacks.lock.toml"), content).map_err(|e| e.to_string())
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn sha512_file(path: &Path) -> Result<String, String> {
    let mut f = fs::File::open(path)
        .map_err(|e| format!("Cannot open {}: {}", path.display(), e))?;
    let mut hasher = Sha512::new();
    let mut buf = vec![0u8; 65536];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 { break; }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

const MODRINTH: &str = "https://api.modrinth.com/v2";
const UA: &str = "msm/0.1 (minecraft-server-manager; contact@example.com)";

// ── Scan datapacks directory ──────────────────────────────────────────────────

pub async fn scan_datapacks(
    client: &reqwest::Client,
    server_path: &Path,
    instance_dir: &Path,
) -> Result<DatapackLock, String> {
    let datapacks_dir = find_datapacks_dir(server_path);

    let zip_paths: Vec<PathBuf> = fs::read_dir(&datapacks_dir)
        .map_err(|e| format!("Cannot read datapacks directory: {}", e))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("zip"))
        .collect();

    if zip_paths.is_empty() {
        let lock = DatapackLock::default();
        write_lock(instance_dir, &lock)?;
        return Ok(lock);
    }

    let hashes: Vec<(PathBuf, String)> = tokio::task::spawn_blocking({
        let paths = zip_paths.clone();
        move || -> Vec<(PathBuf, String)> {
            paths.into_iter().filter_map(|p| sha512_file(&p).ok().map(|h| (p, h))).collect()
        }
    })
    .await
    .map_err(|e| format!("Hash task panicked: {}", e))?;

    let hash_to_path: HashMap<String, PathBuf> =
        hashes.iter().map(|(p, h)| (h.clone(), p.clone())).collect();
    let hash_list: Vec<&str> = hashes.iter().map(|(_, h)| h.as_str()).collect();

    let body = serde_json::json!({ "hashes": hash_list, "algorithm": "sha512" });
    let identified: HashMap<String, MrVersion> = client
        .post(format!("{}/version_files", MODRINTH))
        .header("User-Agent", UA)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Modrinth request failed: {}", e))?
        .json()
        .await
        .map_err(|e| format!("Modrinth parse error: {}", e))?;

    let project_ids: Vec<String> = identified
        .values()
        .map(|v| v.project_id.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    let project_titles: HashMap<String, String> = if project_ids.is_empty() {
        HashMap::new()
    } else {
        let ids_json = serde_json::to_string(&project_ids).unwrap();
        let projects: Vec<MrProject> = client
            .get(format!("{}/projects", MODRINTH))
            .header("User-Agent", UA)
            .query(&[("ids", ids_json)])
            .send()
            .await
            .map_err(|e| format!("Modrinth projects request failed: {}", e))?
            .json()
            .await
            .unwrap_or_default();
        projects.into_iter().map(|p| (p.id, p.title)).collect()
    };

    let mut datapacks: Vec<DatapackEntry> = identified
        .iter()
        .filter_map(|(hash, version)| {
            let path = hash_to_path.get(hash)?;
            let filename = path.file_name()?.to_string_lossy().to_string();
            let name = project_titles
                .get(&version.project_id)
                .cloned()
                .unwrap_or_else(|| version.project_id.clone());
            Some(DatapackEntry {
                name,
                modrinth_project_id: version.project_id.clone(),
                modrinth_version_id: version.id.clone(),
                version_number: version.version_number.clone(),
                filename,
                sha512: hash.clone(),
            })
        })
        .collect();

    datapacks.sort_by(|a, b| a.name.cmp(&b.name));
    let lock = DatapackLock { datapacks };
    write_lock(instance_dir, &lock)?;
    Ok(lock)
}

// ── Check for available updates ───────────────────────────────────────────────

pub async fn check_updates(
    client: &reqwest::Client,
    lock: &DatapackLock,
    mc_version: &str,
) -> Result<Vec<DatapackUpdate>, String> {
    if lock.datapacks.is_empty() {
        return Ok(Vec::new());
    }

    let hash_to_pack: HashMap<&str, &DatapackEntry> =
        lock.datapacks.iter().map(|d| (d.sha512.as_str(), d)).collect();
    let hashes: Vec<&str> = lock.datapacks.iter().map(|d| d.sha512.as_str()).collect();

    let body = serde_json::json!({
        "hashes": hashes,
        "algorithm": "sha512",
        "game_versions": [mc_version]
    });

    let resp: HashMap<String, MrVersion> = client
        .post(format!("{}/version_files/update", MODRINTH))
        .header("User-Agent", UA)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Modrinth update check failed: {}", e))?
        .json()
        .await
        .map_err(|e| format!("Modrinth parse error: {}", e))?;

    let mut updates: Vec<DatapackUpdate> = resp
        .iter()
        .filter_map(|(hash, latest)| {
            let entry = hash_to_pack.get(hash.as_str())?;
            if latest.id == entry.modrinth_version_id {
                return None;
            }
            let file = latest.files.iter().find(|f| f.primary).or_else(|| latest.files.first())?;
            Some(DatapackUpdate {
                project_id: entry.modrinth_project_id.clone(),
                name: entry.name.clone(),
                installed_version_id: entry.modrinth_version_id.clone(),
                installed_version_number: entry.version_number.clone(),
                latest_version_id: latest.id.clone(),
                latest_version_number: latest.version_number.clone(),
                download_url: file.url.clone(),
                filename: file.filename.clone(),
                sha512: file.hashes.sha512.clone(),
            })
        })
        .collect();

    updates.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(updates)
}

// ── Search datapacks ──────────────────────────────────────────────────────────

pub async fn search_datapacks(
    client: &reqwest::Client,
    term: &str,
    mc_version: &str,
) -> Result<Vec<DatapackSearchHit>, String> {
    let facets = serde_json::json!([
        ["project_type:datapack"],
        [format!("versions:{}", mc_version)]
    ]);

    let resp: MrSearchResponse = client
        .get(format!("{}/search", MODRINTH))
        .header("User-Agent", UA)
        .query(&[
            ("query", term),
            ("facets", &facets.to_string()),
            ("limit", "20"),
        ])
        .send()
        .await
        .map_err(|e| format!("Modrinth search failed: {}", e))?
        .json()
        .await
        .map_err(|e| format!("Modrinth search parse error: {}", e))?;

    Ok(resp.hits.into_iter().map(|h| DatapackSearchHit {
        project_id: h.project_id,
        slug: h.slug,
        title: h.title,
        description: h.description,
        downloads: h.downloads,
        icon_url: h.icon_url,
    }).collect())
}

// ── Add a datapack by version ID ──────────────────────────────────────────────

pub async fn add_datapack(
    client: &reqwest::Client,
    project_id: &str,
    version_id: &str,
    server_path: &Path,
    instance_dir: &Path,
) -> Result<DatapackEntry, String> {
    let datapacks_dir = find_datapacks_dir(server_path);
    if !datapacks_dir.exists() {
        std::fs::create_dir_all(&datapacks_dir)
            .map_err(|e| format!("Cannot create datapacks directory: {}", e))?;
    }

    let mut lock = read_lock(instance_dir);

    let version: MrVersion = client
        .get(format!("{}/version/{}", MODRINTH, version_id))
        .header("User-Agent", UA)
        .send()
        .await
        .map_err(|e| format!("Modrinth request failed: {}", e))?
        .json()
        .await
        .map_err(|e| format!("Modrinth parse error: {}", e))?;

    let file = version.files.iter().find(|f| f.primary)
        .or_else(|| version.files.first())
        .ok_or_else(|| format!("No files found for version {}", version_id))?;

    let bytes = client
        .get(&file.url)
        .header("User-Agent", UA)
        .send()
        .await
        .map_err(|e| format!("Download failed: {}", e))?
        .bytes()
        .await
        .map_err(|e| format!("Download read failed: {}", e))?;

    let expected = file.hashes.sha512.clone();
    let actual = tokio::task::spawn_blocking({
        let b = bytes.clone();
        move || {
            let mut h = Sha512::new();
            h.update(&b);
            hex::encode(h.finalize())
        }
    })
    .await
    .map_err(|e| format!("Hash task panicked: {}", e))?;

    if actual != expected {
        return Err(format!(
            "SHA512 mismatch for {}: file may be corrupted",
            file.filename
        ));
    }

    tokio::fs::write(datapacks_dir.join(&file.filename), &bytes[..])
        .await
        .map_err(|e| format!("Failed to write {}: {}", file.filename, e))?;

    let project: MrProject = client
        .get(format!("{}/project/{}", MODRINTH, project_id))
        .header("User-Agent", UA)
        .send()
        .await
        .map_err(|e| format!("Modrinth request failed: {}", e))?
        .json()
        .await
        .map_err(|e| format!("Modrinth parse error: {}", e))?;

    let entry = DatapackEntry {
        name: project.title,
        modrinth_project_id: project_id.to_string(),
        modrinth_version_id: version.id,
        version_number: version.version_number,
        filename: file.filename.clone(),
        sha512: actual,
    };

    lock.datapacks.retain(|d| d.modrinth_project_id != project_id);
    lock.datapacks.push(entry.clone());
    lock.datapacks.sort_by(|a, b| a.name.cmp(&b.name));
    write_lock(instance_dir, &lock)?;

    Ok(entry)
}

// ── Apply a single datapack update ────────────────────────────────────────────

pub async fn apply_update(
    client: &reqwest::Client,
    update: &DatapackUpdate,
    server_path: &Path,
    lock: &mut DatapackLock,
) -> Result<(), String> {
    let datapacks_dir = find_datapacks_dir(server_path);

    let old_filename = lock
        .datapacks
        .iter()
        .find(|d| d.modrinth_project_id == update.project_id)
        .map(|d| d.filename.clone())
        .ok_or_else(|| format!("Datapack '{}' not in lock file", update.project_id))?;

    let bytes = client
        .get(&update.download_url)
        .header("User-Agent", UA)
        .send()
        .await
        .map_err(|e| format!("Download failed for {}: {}", update.name, e))?
        .bytes()
        .await
        .map_err(|e| format!("Download read failed: {}", e))?;

    let actual = tokio::task::spawn_blocking({
        let b = bytes.clone();
        move || {
            let mut h = Sha512::new();
            h.update(&b);
            hex::encode(h.finalize())
        }
    })
    .await
    .map_err(|e| format!("Hash task panicked: {}", e))?;

    if actual != update.sha512 {
        return Err(format!(
            "SHA512 mismatch for {}: file may be corrupted",
            update.filename
        ));
    }

    tokio::fs::write(datapacks_dir.join(&update.filename), &bytes[..])
        .await
        .map_err(|e| format!("Failed to write {}: {}", update.filename, e))?;

    if old_filename != update.filename {
        let old = datapacks_dir.join(&old_filename);
        if old.exists() {
            tokio::fs::remove_file(&old)
                .await
                .map_err(|e| format!("Failed to remove old file {}: {}", old_filename, e))?;
        }
    }

    if let Some(entry) = lock.datapacks.iter_mut().find(|d| d.modrinth_project_id == update.project_id) {
        entry.modrinth_version_id = update.latest_version_id.clone();
        entry.version_number = update.latest_version_number.clone();
        entry.filename = update.filename.clone();
        entry.sha512 = update.sha512.clone();
    }

    Ok(())
}
