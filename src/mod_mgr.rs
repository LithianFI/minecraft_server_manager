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
    /// Renamed to `<file>.jar.disabled`, so the loader skips it
    #[serde(default)]
    pub disabled: bool,
}

impl ModEntry {
    /// Jars Modrinth doesn't recognise have no project ID and can't be updated.
    pub fn on_modrinth(&self) -> bool {
        !self.modrinth_project_id.is_empty()
    }
}

const DISABLED_SUFFIX: &str = ".disabled";

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

/// What happens to each mod when the server moves to another Minecraft version.
#[derive(Debug, Clone, Serialize, Default)]
pub struct VersionChangePlan {
    /// Mods that get a new version for the target Minecraft
    pub updates: Vec<ModUpdate>,
    /// Mods whose installed version already supports the target (names)
    pub compatible: Vec<String>,
    /// Libraries the new versions require that aren't installed yet; installed with them
    pub dependencies: Vec<NewDependency>,
    /// Mods that can't move to the target yet; they get disabled
    pub missing: Vec<MissingMod>,
    /// Jars Modrinth doesn't know, so MSM can't tell whether they'll work (names)
    pub unknown: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NewDependency {
    #[serde(flatten)]
    pub file: ModUpdate,
    /// Name of the mod that needs it
    pub required_by: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MissingMod {
    #[serde(flatten)]
    pub entry: ModEntry,
    /// Set when the mod has a new version but a library it requires doesn't
    pub needs: Option<String>,
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
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            name.ends_with(".jar") || name.ends_with(".jar.disabled")
        })
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

    // Build lock entries; jars Modrinth doesn't know are listed by filename
    let mut mods: Vec<ModEntry> = hashes
        .iter()
        .filter_map(|(path, hash)| {
            let filename = path.file_name()?.to_string_lossy().to_string();
            let disabled = filename.ends_with(DISABLED_SUFFIX);
            Some(match identified.get(hash) {
                Some(version) => ModEntry {
                    name: project_titles
                        .get(&version.project_id)
                        .cloned()
                        .unwrap_or_else(|| version.project_id.clone()),
                    modrinth_project_id: version.project_id.clone(),
                    modrinth_version_id: version.id.clone(),
                    version_number: version.version_number.clone(),
                    filename,
                    sha512: hash.clone(),
                    disabled,
                },
                None => ModEntry {
                    name: filename.trim_end_matches(DISABLED_SUFFIX).trim_end_matches(".jar").to_string(),
                    modrinth_project_id: String::new(),
                    modrinth_version_id: String::new(),
                    version_number: String::new(),
                    filename,
                    sha512: hash.clone(),
                    disabled,
                },
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
    // Disabled mods and jars Modrinth doesn't know are left alone
    let checkable: Vec<&ModEntry> = lock.mods.iter().filter(|m| m.on_modrinth() && !m.disabled).collect();
    let latest = latest_versions(client, &checkable, mc_version, loader).await?;
    let mut updates: Vec<ModUpdate> = checkable
        .iter()
        .filter_map(|entry| to_update(entry, latest.get(&entry.sha512)?))
        .collect();
    updates.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(updates)
}

/// Checks which mods have a version for `mc_version` before moving the server to it.
pub async fn plan_version_change(
    client: &reqwest::Client,
    lock: &ModLock,
    mc_version: &str,
    loader: &str,
) -> Result<VersionChangePlan, String> {
    let checkable: Vec<&ModEntry> = lock.mods.iter().filter(|m| m.on_modrinth() && !m.disabled).collect();
    let latest = latest_versions(client, &checkable, mc_version, loader).await?;
    let mut plan = VersionChangePlan::default();
    // Versions being installed, to look up what they require
    let mut new_versions: Vec<(String, &MrVersion)> = Vec::new();
    for entry in checkable {
        let missing = MissingMod { entry: entry.clone(), needs: None };
        match latest.get(&entry.sha512) {
            Some(v) if v.id == entry.modrinth_version_id => plan.compatible.push(entry.name.clone()),
            Some(v) => match to_update(entry, v) {
                Some(update) => {
                    new_versions.push((entry.name.clone(), v));
                    plan.updates.push(update);
                }
                None => plan.missing.push(missing),
            },
            None => plan.missing.push(missing),
        }
    }

    let (dependencies, unavailable) = resolve_dependencies(client, lock, new_versions, mc_version, loader).await?;
    plan.dependencies = dependencies;
    // A mod whose required library has no version for the target can't load: disable it instead
    for (mod_name, library) in unavailable {
        if let Some(i) = plan.updates.iter().position(|u| u.name == mod_name) {
            let update = plan.updates.remove(i);
            if let Some(entry) = lock.mods.iter().find(|m| m.modrinth_project_id == update.project_id) {
                plan.missing.push(MissingMod { entry: entry.clone(), needs: Some(library) });
            }
        }
    }

    // Libraries are only needed by mods that still get updated
    plan.dependencies.retain(|d| plan.updates.iter().any(|u| u.name == d.required_by));

    plan.unknown = lock.mods.iter().filter(|m| !m.on_modrinth() && !m.disabled).map(|m| m.name.clone()).collect();
    plan.updates.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(plan)
}

/// Finds the required libraries of `new_versions` (and theirs, and so on) that aren't installed,
/// picking each one's newest version for `mc_version`. Also returns (mod name, library name) for
/// libraries that have no such version.
async fn resolve_dependencies(
    client: &reqwest::Client,
    lock: &ModLock,
    new_versions: Vec<(String, &MrVersion)>,
    mc_version: &str,
    loader: &str,
) -> Result<(Vec<NewDependency>, Vec<(String, String)>), String> {
    let mut known: HashSet<String> = lock.mods.iter().map(|m| m.modrinth_project_id.clone()).collect();
    // (project, pinned version, name of the mod that requires it)
    let mut queue: Vec<(String, Option<String>, String)> = Vec::new();
    let enqueue = |queue: &mut Vec<_>, known: &mut HashSet<String>, v: &MrVersion, name: &str| {
        for dep in v.dependencies.iter().filter(|d| d.dependency_type == "required") {
            if let Some(pid) = &dep.project_id {
                if known.insert(pid.clone()) {
                    queue.push((pid.clone(), dep.version_id.clone(), name.to_string()));
                }
            }
        }
    };
    for (name, v) in &new_versions {
        enqueue(&mut queue, &mut known, v, name);
    }

    let mut found: Vec<(MrVersion, String)> = Vec::new();
    let mut unavailable_ids: Vec<(String, String)> = Vec::new();
    while let Some((pid, pinned, required_by)) = queue.pop() {
        let version = match pinned {
            Some(vid) => Some(get_json::<MrVersion>(client, &format!("{}/version/{}", MODRINTH, vid)).await?),
            None => {
                let versions: Vec<MrVersion> = client
                    .get(format!("{}/project/{}/version", MODRINTH, pid))
                    .header("User-Agent", UA)
                    .query(&[
                        ("loaders", serde_json::json!([loader]).to_string()),
                        ("game_versions", serde_json::json!([mc_version]).to_string()),
                    ])
                    .send()
                    .await
                    .map_err(|e| format!("Dependency lookup failed for {}: {}", pid, e))?
                    .json()
                    .await
                    .map_err(|e| format!("Dependency parse error for {}: {}", pid, e))?;
                versions.into_iter().next()
            }
        };
        match version {
            Some(v) => {
                enqueue(&mut queue, &mut known, &v, &required_by);
                found.push((v, required_by));
            }
            None => unavailable_ids.push((required_by, pid)),
        }
    }

    // Libraries are listed by title, not ID
    let ids: Vec<&str> = found.iter().map(|(v, _)| v.project_id.as_str())
        .chain(unavailable_ids.iter().map(|(_, pid)| pid.as_str()))
        .collect();
    let titles: HashMap<String, String> = if ids.is_empty() {
        HashMap::new()
    } else {
        let url = format!("{}/projects?ids={}", MODRINTH, serde_json::to_string(&ids).unwrap());
        get_json::<Vec<MrProject>>(client, &url).await?.into_iter().map(|p| (p.id, p.title)).collect()
    };
    let title = |pid: &str| titles.get(pid).cloned().unwrap_or_else(|| pid.to_string());

    let mut dependencies: Vec<NewDependency> = found
        .iter()
        .filter_map(|(v, required_by)| {
            let file = v.files.iter().find(|f| f.primary).or_else(|| v.files.first())?;
            Some(NewDependency {
                file: ModUpdate {
                    project_id: v.project_id.clone(),
                    name: title(&v.project_id),
                    installed_version_id: String::new(),
                    installed_version_number: String::new(),
                    latest_version_id: v.id.clone(),
                    latest_version_number: v.version_number.clone(),
                    download_url: file.url.clone(),
                    filename: file.filename.clone(),
                    sha512: file.hashes.sha512.clone(),
                },
                required_by: required_by.clone(),
            })
        })
        .collect();
    dependencies.sort_by(|a, b| a.file.name.cmp(&b.file.name));
    let unavailable = unavailable_ids.into_iter().map(|(by, pid)| (by, title(&pid))).collect();
    Ok((dependencies, unavailable))
}

async fn get_json<T: serde::de::DeserializeOwned>(client: &reqwest::Client, url: &str) -> Result<T, String> {
    client
        .get(url)
        .header("User-Agent", UA)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("Modrinth request failed: {}", e))?
        .json()
        .await
        .map_err(|e| format!("Modrinth parse error: {}", e))
}

/// Newest version of each mod for `mc_version`, keyed by the installed file's sha512.
/// Mods without such a version are missing from the result.
async fn latest_versions(
    client: &reqwest::Client,
    mods: &[&ModEntry],
    mc_version: &str,
    loader: &str,
) -> Result<HashMap<String, MrVersion>, String> {
    if mods.is_empty() {
        return Ok(HashMap::new());
    }
    let hashes: Vec<&str> = mods.iter().map(|m| m.sha512.as_str()).collect();

    let body = serde_json::json!({
        "hashes": hashes,
        "algorithm": "sha512",
        "loaders": [loader],
        "game_versions": [mc_version]
    });

    client
        .post(format!("{}/version_files/update", MODRINTH))
        .header("User-Agent", UA)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Modrinth update check failed: {}", e))?
        .json()
        .await
        .map_err(|e| format!("Modrinth parse error: {}", e))
}

/// `None` when `latest` is what's installed already (or has no files).
fn to_update(entry: &ModEntry, latest: &MrVersion) -> Option<ModUpdate> {
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
    /// Modrinth's server support: "required", "optional", "unsupported" (client-only) or "unknown"
    pub server_side: String,
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
    #[serde(default)]
    server_side: String,
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
        server_side: h.server_side,
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
            disabled: false,
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

// ── Enable / disable / remove ─────────────────────────────────────────────────

fn find_mod<'a>(lock: &'a mut ModLock, filename: &str) -> Result<&'a mut ModEntry, String> {
    if filename.contains('/') || filename.contains('\\') || filename.contains("..") {
        return Err("Invalid filename".to_string());
    }
    lock.mods
        .iter_mut()
        .find(|m| m.filename == filename)
        .ok_or_else(|| format!("Mod '{}' not found — try scanning mods again", filename))
}

/// Renames `X.jar` ⇄ `X.jar.disabled`. Takes effect the next time the server starts.
pub fn set_mod_enabled(server_path: &Path, instance_dir: &Path, filename: &str, enabled: bool) -> Result<ModLock, String> {
    let mut lock = read_lock(instance_dir);
    let entry = find_mod(&mut lock, filename)?;
    if entry.disabled != enabled {
        return Ok(lock); // already in the requested state
    }
    let new_name = if enabled {
        filename.trim_end_matches(DISABLED_SUFFIX).to_string()
    } else {
        format!("{}{}", filename, DISABLED_SUFFIX)
    };
    let mods_dir = server_path.join("mods");
    fs::rename(mods_dir.join(filename), mods_dir.join(&new_name))
        .map_err(|e| format!("Failed to rename {}: {}", filename, e))?;
    entry.filename = new_name;
    entry.disabled = !enabled;
    write_lock(instance_dir, &lock)?;
    Ok(lock)
}

/// Deletes the mod's jar. Takes effect the next time the server starts.
pub fn remove_mod(server_path: &Path, instance_dir: &Path, filename: &str) -> Result<ModLock, String> {
    let mut lock = read_lock(instance_dir);
    find_mod(&mut lock, filename)?;
    let path = server_path.join("mods").join(filename);
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("Failed to delete {}: {}", filename, e)),
    }
    lock.mods.retain(|m| m.filename != filename);
    write_lock(instance_dir, &lock)?;
    Ok(lock)
}

pub async fn apply_update(
    client: &reqwest::Client,
    update: &ModUpdate,
    server_path: &Path,
    lock: &mut ModLock,
) -> Result<(), String> {
    let bytes = download_update(client, update).await?;
    install_update(&bytes, update, server_path, lock).await
}

/// Downloads the update's jar and checks its hash, without touching the server.
pub async fn download_update(client: &reqwest::Client, update: &ModUpdate) -> Result<Vec<u8>, String> {
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
    Ok(bytes.to_vec())
}

/// Swaps the old jar for the downloaded one and updates the lock entry. A mod that isn't
/// installed yet (a new library) is added.
pub async fn install_update(
    bytes: &[u8],
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
        .unwrap_or_else(|| update.filename.clone());

    // Write new file
    tokio::fs::write(mods_dir.join(&update.filename), bytes)
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
    match lock.mods.iter_mut().find(|m| m.modrinth_project_id == update.project_id) {
        Some(entry) => {
            entry.modrinth_version_id = update.latest_version_id.clone();
            entry.version_number = update.latest_version_number.clone();
            entry.filename = update.filename.clone();
            entry.sha512 = update.sha512.clone();
        }
        None => lock.mods.push(ModEntry {
            name: update.name.clone(),
            modrinth_project_id: update.project_id.clone(),
            modrinth_version_id: update.latest_version_id.clone(),
            version_number: update.latest_version_number.clone(),
            filename: update.filename.clone(),
            sha512: update.sha512.clone(),
            disabled: false,
        }),
    }

    Ok(())
}
