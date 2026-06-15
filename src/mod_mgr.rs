use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModEntry {
    pub name: String,
    pub modrinth_project_id: String,
    pub modrinth_version_id: String,
    pub version_number: String,
    pub filename: String,
    pub sha512: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModLock {
    #[serde(default)]
    pub mods: Vec<ModEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModUpdate {
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
    #[serde(default)]
    dependencies: Vec<MrDependency>,
}

#[derive(Deserialize)]
struct MrDependency {
    version_id: Option<String>,
    project_id: Option<String>,
    dependency_type: String,
}

#[derive(Deserialize)]
struct MrProject {
    id: String,
    title: String,
}

// ── Lock file I/O ─────────────────────────────────────────────────────────────

pub fn read_lock(instance_dir: &Path) -> ModLock {
    let path = instance_dir.join("mods.lock.toml");
    let Ok(content) = fs::read_to_string(&path) else {
        return ModLock::default();
    };
    toml::from_str(&content).unwrap_or_default()
}

pub fn write_lock(instance_dir: &Path, lock: &ModLock) -> Result<(), String> {
    let content = toml::to_string_pretty(lock).map_err(|e| e.to_string())?;
    fs::write(instance_dir.join("mods.lock.toml"), content).map_err(|e| e.to_string())
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

// ── Scan mods directory ───────────────────────────────────────────────────────

pub async fn scan_mods(
    client: &reqwest::Client,
    server_path: &Path,
    instance_dir: &Path,
) -> Result<ModLock, String> {
    let mods_dir = server_path.join("mods");

    let jar_paths: Vec<PathBuf> = fs::read_dir(&mods_dir)
        .map_err(|e| format!("Cannot read mods directory: {}", e))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jar"))
        .collect();

    if jar_paths.is_empty() {
        let lock = ModLock::default();
        write_lock(instance_dir, &lock)?;
        return Ok(lock);
    }

    // Hash all JARs in a blocking thread
    let hashes: Vec<(PathBuf, String)> = tokio::task::spawn_blocking({
        let paths = jar_paths.clone();
        move || -> Vec<(PathBuf, String)> {
            paths.into_iter().filter_map(|p| sha512_file(&p).ok().map(|h| (p, h))).collect()
        }
    })
    .await
    .map_err(|e| format!("Hash task panicked: {}", e))?;

    let hash_to_path: HashMap<String, PathBuf> =
        hashes.iter().map(|(p, h)| (h.clone(), p.clone())).collect();
    let hash_list: Vec<&str> = hashes.iter().map(|(_, h)| h.as_str()).collect();

    // Identify JARs by hash via Modrinth
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

    // Batch-fetch project titles
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

    // Build lock entries for identified JARs
    let mut mods: Vec<ModEntry> = identified
        .iter()
        .filter_map(|(hash, version)| {
            let path = hash_to_path.get(hash)?;
            let filename = path.file_name()?.to_string_lossy().to_string();
            let name = project_titles
                .get(&version.project_id)
                .cloned()
                .unwrap_or_else(|| version.project_id.clone());
            Some(ModEntry {
                name,
                modrinth_project_id: version.project_id.clone(),
                modrinth_version_id: version.id.clone(),
                version_number: version.version_number.clone(),
                filename,
                sha512: hash.clone(),
            })
        })
        .collect();

    mods.sort_by(|a, b| a.name.cmp(&b.name));
    let lock = ModLock { mods };
    write_lock(instance_dir, &lock)?;
    Ok(lock)
}

// ── Check for available updates ───────────────────────────────────────────────

pub async fn check_updates(
    client: &reqwest::Client,
    lock: &ModLock,
    mc_version: &str,
    loader: &str,
) -> Result<Vec<ModUpdate>, String> {
    if lock.mods.is_empty() {
        return Ok(Vec::new());
    }

    let hash_to_mod: HashMap<&str, &ModEntry> =
        lock.mods.iter().map(|m| (m.sha512.as_str(), m)).collect();
    let hashes: Vec<&str> = lock.mods.iter().map(|m| m.sha512.as_str()).collect();

    let body = serde_json::json!({
        "hashes": hashes,
        "algorithm": "sha512",
        "loaders": [loader],
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

    let mut updates: Vec<ModUpdate> = resp
        .iter()
        .filter_map(|(hash, latest)| {
            let entry = hash_to_mod.get(hash.as_str())?;
            if latest.id == entry.modrinth_version_id {
                return None;
            }
            let file = latest.files.iter().find(|f| f.primary).or_else(|| latest.files.first())?;
            Some(ModUpdate {
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

// ── Search mods ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModSearchHit {
    pub project_id: String,
    pub slug: String,
    pub title: String,
    pub description: String,
    pub downloads: u64,
    pub icon_url: Option<String>,
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

pub async fn search_mods(
    client: &reqwest::Client,
    term: &str,
    mc_version: &str,
    loader: &str,
) -> Result<Vec<ModSearchHit>, String> {
    let facets = serde_json::json!([
        ["project_type:mod"],
        [format!("categories:{}", loader)],
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

    Ok(resp.hits.into_iter().map(|h| ModSearchHit {
        project_id: h.project_id,
        slug: h.slug,
        title: h.title,
        description: h.description,
        downloads: h.downloads,
        icon_url: h.icon_url,
    }).collect())
}

// ── Add a mod by version ID (resolves required dependencies) ─────────────────

pub async fn add_mod(
    client: &reqwest::Client,
    project_id: &str,
    version_id: &str,
    mc_version: &str,
    loader: &str,
    server_path: &Path,
    instance_dir: &Path,
) -> Result<Vec<ModEntry>, String> {
    let mods_dir = server_path.join("mods");
    if !mods_dir.exists() {
        std::fs::create_dir_all(&mods_dir)
            .map_err(|e| format!("Cannot create mods directory: {}", e))?;
    }

    let mut lock = read_lock(instance_dir);

    // BFS queue: (version_id, known_project_id)
    // known_project_id lets us skip the API call if already installed/visited.
    let mut queue: Vec<(String, Option<String>)> =
        vec![(version_id.to_string(), Some(project_id.to_string()))];
    let mut visited: HashSet<String> = HashSet::new(); // project IDs processed this run
    let mut installed: Vec<ModEntry> = Vec::new();

    while let Some((vid, known_pid)) = queue.pop() {
        // Early skip if we already know the project ID
        if let Some(ref pid) = known_pid {
            if visited.contains(pid) || lock.mods.iter().any(|m| &m.modrinth_project_id == pid) {
                continue;
            }
        }

        // Fetch version info
        let version: MrVersion = client
            .get(format!("{}/version/{}", MODRINTH, vid))
            .header("User-Agent", UA)
            .send()
            .await
            .map_err(|e| format!("Modrinth request failed: {}", e))?
            .json()
            .await
            .map_err(|e| format!("Modrinth parse error: {}", e))?;

        let pid = version.project_id.clone();

        // Skip if already installed or visited (covers cases where known_pid was None)
        if visited.contains(&pid) || lock.mods.iter().any(|m| m.modrinth_project_id == pid) {
            continue;
        }
        visited.insert(pid.clone());

        // Get primary file
        let file = version.files.iter().find(|f| f.primary)
            .or_else(|| version.files.first())
            .ok_or_else(|| format!("No files found for version {}", vid))?;

        // Download
        let bytes = client
            .get(&file.url)
            .header("User-Agent", UA)
            .send()
            .await
            .map_err(|e| format!("Download failed: {}", e))?
            .bytes()
            .await
            .map_err(|e| format!("Download read failed: {}", e))?;

        // Verify SHA512
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

        // Write file
        tokio::fs::write(mods_dir.join(&file.filename), &bytes[..])
            .await
            .map_err(|e| format!("Failed to write {}: {}", file.filename, e))?;

        // Fetch project title
        let project: MrProject = client
            .get(format!("{}/project/{}", MODRINTH, pid))
            .header("User-Agent", UA)
            .send()
            .await
            .map_err(|e| format!("Modrinth request failed: {}", e))?
            .json()
            .await
            .map_err(|e| format!("Modrinth parse error: {}", e))?;

        let entry = ModEntry {
            name: project.title,
            modrinth_project_id: pid.clone(),
            modrinth_version_id: version.id,
            version_number: version.version_number,
            filename: file.filename.clone(),
            sha512: actual,
        };

        lock.mods.retain(|m| m.modrinth_project_id != pid);
        lock.mods.push(entry.clone());
        installed.push(entry);

        // Enqueue required dependencies
        for dep in version.dependencies.iter().filter(|d| d.dependency_type == "required") {
            if let Some(dep_vid) = &dep.version_id {
                queue.push((dep_vid.clone(), dep.project_id.clone()));
            } else if let Some(dep_pid) = &dep.project_id {
                // Skip early if already known
                if visited.contains(dep_pid) || lock.mods.iter().any(|m| &m.modrinth_project_id == dep_pid) {
                    continue;
                }
                // Find the latest compatible version for this MC version + loader
                let versions: Vec<MrVersion> = client
                    .get(format!("{}/project/{}/version", MODRINTH, dep_pid))
                    .header("User-Agent", UA)
                    .query(&[
                        ("loaders", serde_json::json!([loader]).to_string()),
                        ("game_versions", serde_json::json!([mc_version]).to_string()),
                    ])
                    .send()
                    .await
                    .map_err(|e| format!("Dependency lookup failed for {}: {}", dep_pid, e))?
                    .json()
                    .await
                    .map_err(|e| format!("Dependency parse error for {}: {}", dep_pid, e))?;
                if let Some(v) = versions.into_iter().next() {
                    queue.push((v.id, Some(dep_pid.clone())));
                }
                // No compatible version available — skip silently
            }
        }
    }

    lock.mods.sort_by(|a, b| a.name.cmp(&b.name));
    write_lock(instance_dir, &lock)?;

    Ok(installed)
}

// ── Apply a single mod update ─────────────────────────────────────────────────

pub async fn apply_update(
    client: &reqwest::Client,
    update: &ModUpdate,
    server_path: &Path,
    lock: &mut ModLock,
) -> Result<(), String> {
    let mods_dir = server_path.join("mods");

    let old_filename = lock
        .mods
        .iter()
        .find(|m| m.modrinth_project_id == update.project_id)
        .map(|m| m.filename.clone())
        .ok_or_else(|| format!("Mod '{}' not in lock file", update.project_id))?;

    // Download
    let bytes = client
        .get(&update.download_url)
        .header("User-Agent", UA)
        .send()
        .await
        .map_err(|e| format!("Download failed for {}: {}", update.name, e))?
        .bytes()
        .await
        .map_err(|e| format!("Download read failed: {}", e))?;

    // Verify SHA512
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

    // Write new file
    tokio::fs::write(mods_dir.join(&update.filename), &bytes[..])
        .await
        .map_err(|e| format!("Failed to write {}: {}", update.filename, e))?;

    // Remove old file if filename changed
    if old_filename != update.filename {
        let old = mods_dir.join(&old_filename);
        if old.exists() {
            tokio::fs::remove_file(&old)
                .await
                .map_err(|e| format!("Failed to remove old file {}: {}", old_filename, e))?;
        }
    }

    // Update lock entry
    if let Some(entry) = lock.mods.iter_mut().find(|m| m.modrinth_project_id == update.project_id) {
        entry.modrinth_version_id = update.latest_version_id.clone();
        entry.version_number = update.latest_version_number.clone();
        entry.filename = update.filename.clone();
        entry.sha512 = update.sha512.clone();
    }

    Ok(())
}
