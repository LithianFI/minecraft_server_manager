//! Sharing a server's mods with players.
//!
//! MSM works out which files players need (from the modpack's file list and the server's mods),
//! then publishes them as a packwiz pack at `/pack/<token>/pack.toml`. Players either import a
//! ready-made Prism Launcher instance that syncs with packwiz-installer on every launch, or
//! download a `.mrpack` of the current state. Everything under `/pack/<token>/` is public; the
//! random token is the only secret.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    extract::{Path as UrlPath, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::state::AppState;

const MODRINTH: &str = "https://api.modrinth.com/v2";
const UA: &str = "msm/0.1";
const BOOTSTRAP_URL: &str =
    "https://github.com/packwiz/packwiz-installer-bootstrap/releases/latest/download/packwiz-installer-bootstrap.jar";

// ── What players get ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum Source {
    /// Downloaded by the launcher straight from Modrinth's CDN
    Remote { url: String, sha1: Option<String>, sha512: String, size: u64 },
    /// A file in the server directory, served by MSM
    ServerFile(PathBuf),
    /// A file inside the modpack's .mrpack (overrides), served by MSM
    PackEntry(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct ClientMod {
    /// Modrinth project ID, or "file:<jar name>"
    pub key: String,
    pub name: String,
    /// Install path on the client, e.g. "mods/create-6.0.jar"
    pub path: String,
    /// "pack" or "server"
    pub origin: &'static str,
    pub reason: String,
    /// MSM's own decision, before the user's choice
    pub auto: bool,
    pub included: bool,
    /// The user overrode MSM's decision
    pub overridden: bool,
    #[serde(skip)]
    client_only: bool,
    #[serde(skip)]
    source: Source,
}

#[derive(Debug, Clone)]
struct ClientFile {
    path: String,
    source: Source,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClientSet {
    pub name: String,
    pub version_label: String,
    pub minecraft: String,
    pub loader: Option<String>,
    pub loader_version: Option<String>,
    pub mods: Vec<ClientMod>,
    /// Config and other non-mod files from the modpack
    pub file_count: usize,
    #[serde(skip)]
    files: Vec<ClientFile>,
    #[serde(skip)]
    mrpack: Option<PathBuf>,
}

/// What Modrinth knows about a jar, cached per instance by SHA-512.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ModrinthFileInfo {
    url: String,
    sha1: Option<String>,
    size: u64,
    project_id: String,
    title: String,
    client_side: String,
}

fn cache_path(instance_dir: &Path) -> PathBuf {
    instance_dir.join("client-mods-cache.json")
}

/// Looks up download URLs and client support for jars by hash, using and updating the cache.
async fn modrinth_info(
    client: &reqwest::Client,
    instance_dir: &Path,
    hashes: &[String],
) -> HashMap<String, ModrinthFileInfo> {
    let mut cache: HashMap<String, ModrinthFileInfo> = std::fs::read_to_string(cache_path(instance_dir))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let missing: Vec<&String> = hashes.iter().filter(|h| !cache.contains_key(*h)).collect();
    if !missing.is_empty() {
        if let Ok(found) = lookup_modrinth(client, &missing).await {
            cache.extend(found);
            if let Ok(json) = serde_json::to_string(&cache) {
                let _ = std::fs::write(cache_path(instance_dir), json);
            }
        }
    }
    hashes.iter().filter_map(|h| cache.get(h).map(|i| (h.clone(), i.clone()))).collect()
}

async fn lookup_modrinth(client: &reqwest::Client, hashes: &[&String]) -> Result<HashMap<String, ModrinthFileInfo>, String> {
    #[derive(Deserialize)]
    struct Hashes { sha512: String, #[serde(default)] sha1: Option<String> }
    #[derive(Deserialize)]
    struct File { url: String, hashes: Hashes, #[serde(default)] size: u64 }
    #[derive(Deserialize)]
    struct Version { project_id: String, files: Vec<File> }
    #[derive(Deserialize)]
    struct Project { id: String, title: String, #[serde(default)] client_side: String }

    let versions: HashMap<String, Version> = client
        .post(format!("{}/version_files", MODRINTH))
        .header("User-Agent", UA)
        .json(&serde_json::json!({ "hashes": hashes, "algorithm": "sha512" }))
        .send().await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;

    let mut ids: Vec<&str> = versions.values().map(|v| v.project_id.as_str()).collect();
    ids.sort();
    ids.dedup();
    let projects: Vec<Project> = if ids.is_empty() { Vec::new() } else {
        client
            .get(format!("{}/projects", MODRINTH))
            .header("User-Agent", UA)
            .query(&[("ids", serde_json::to_string(&ids).unwrap_or_default())])
            .send().await.map_err(|e| e.to_string())?
            .json().await.map_err(|e| e.to_string())?
    };
    let projects: HashMap<String, Project> = projects.into_iter().map(|p| (p.id.clone(), p)).collect();

    Ok(versions
        .into_iter()
        .filter_map(|(hash, v)| {
            let file = v.files.into_iter().find(|f| f.hashes.sha512 == hash)?;
            let project = projects.get(&v.project_id);
            Some((hash, ModrinthFileInfo {
                url: file.url,
                sha1: file.hashes.sha1,
                size: file.size,
                project_id: v.project_id.clone(),
                title: project.map(|p| p.title.clone()).unwrap_or_else(|| v.project_id.clone()),
                client_side: project.map(|p| p.client_side.clone()).unwrap_or_default(),
            }))
        })
        .collect())
}

/// "https://cdn.modrinth.com/data/<project>/versions/…" → project ID
fn project_from_url(url: &str) -> Option<String> {
    let rest = url.split("/data/").nth(1)?;
    Some(rest.split('/').next()?.to_string()).filter(|s| !s.is_empty())
}

fn file_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// "create-6.0.4.jar" → "create-6.0.4"
fn display_name(path: &str) -> String {
    let name = file_name(path);
    name.trim_end_matches(".disabled").trim_end_matches(".jar").to_string()
}

/// Entries of a .mrpack's overrides folders that clients get: (zip entry, install path, client-only).
/// `client-overrides/` wins over `overrides/`; `server-overrides/` is never sent to players.
fn client_override_entries(mrpack: &Path) -> Result<Vec<(String, String, bool)>, String> {
    let file = std::fs::File::open(mrpack).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("Invalid .mrpack: {}", e))?;
    let mut by_path: BTreeMap<String, (String, bool)> = BTreeMap::new();
    for prefix in ["overrides/", "client-overrides/"] {
        for i in 0..archive.len() {
            let entry = archive.by_index(i).map_err(|e| e.to_string())?;
            if entry.is_dir() { continue; }
            if let Some(rel) = entry.name().strip_prefix(prefix) {
                // Same safety rule as installing a pack: plain relative paths only
                if !rel.is_empty() && crate::modpack::safe_pack_path(Path::new("/"), rel).is_ok() {
                    by_path.insert(rel.to_string(), (entry.name().to_string(), prefix == "client-overrides/"));
                }
            }
        }
    }
    Ok(by_path.into_iter().map(|(path, (entry, client_only))| (entry, path, client_only)).collect())
}

/// The installed modpack version's .mrpack, downloaded once and kept in the instance folder.
async fn cached_mrpack(state: &Arc<AppState>, instance_dir: &Path, version_id: &str) -> Result<PathBuf, String> {
    let path = instance_dir.join("pack-cache").join(format!("{}.mrpack", version_id));
    if !path.exists() {
        let (_, bytes, _) = crate::modpack::fetch_pack(state, version_id).await?;
        std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
        std::fs::write(&path, bytes).map_err(|e| format!("Failed to cache modpack: {}", e))?;
    }
    Ok(path)
}

struct InstanceSnapshot {
    display_name: String,
    server_path: PathBuf,
    instance_dir: PathBuf,
    minecraft: String,
    loader: Option<String>,
    loader_version: Option<String>,
    pack_version: Option<String>,
    choices: BTreeMap<String, bool>,
}

async fn build_client_set(state: &Arc<AppState>, snap: &InstanceSnapshot) -> Result<ClientSet, String> {
    // Keep the mod list current (someone may have changed mods/ by hand)
    let lock = match crate::mod_mgr::scan_mods(&state.http_client, &snap.server_path, &snap.instance_dir).await {
        Ok(lock) => lock,
        Err(_) => crate::mod_mgr::read_lock(&snap.instance_dir),
    };
    let enabled: Vec<&crate::mod_mgr::ModEntry> = lock.mods.iter().filter(|m| !m.disabled).collect();

    let mut mods: Vec<ClientMod> = Vec::new();
    let mut files: Vec<ClientFile> = Vec::new();
    let mut matched_server: std::collections::HashSet<String> = Default::default(); // server filenames covered by the pack
    let mut mrpack = None;
    let mut version_label = format!("MC {}", snap.minecraft);

    if let Some(version_id) = &snap.pack_version {
        let path = cached_mrpack(state, &snap.instance_dir, version_id).await?;
        let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
        let index = crate::modpack::parse_mrpack_index(&bytes)?;
        version_label = format!("{} ({})", index.name, version_id);

        for f in &index.files {
            let env = f.env.as_ref();
            let client = env.and_then(|e| e.client.as_deref()).unwrap_or("required");
            let server = env.and_then(|e| e.server.as_deref()).unwrap_or("required");
            if client == "unsupported" { continue; }
            let (Some(url), Some(sha512)) = (f.downloads.first(), f.hashes.sha512.clone()) else { continue };
            let fname = file_name(&f.path);
            let on_server = enabled.iter().find(|m| m.sha512 == sha512 || m.filename == fname);
            if let Some(m) = on_server { matched_server.insert(m.filename.clone()); }
            let client_only = server == "unsupported";
            let (auto, reason) = if client_only {
                (true, "Client-side mod from the modpack".to_string())
            } else if on_server.is_some() {
                (true, "In the modpack and on the server".to_string())
            } else {
                (false, "In the modpack, but removed or disabled on the server".to_string())
            };
            mods.push(ClientMod {
                key: project_from_url(url).unwrap_or_else(|| format!("file:{}", fname)),
                name: on_server.map(|m| m.name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| display_name(&f.path)),
                path: f.path.clone(),
                origin: "pack",
                reason,
                auto,
                included: auto,
                overridden: false,
                client_only,
                source: Source::Remote { url: url.clone(), sha1: f.hashes.sha1.clone(), sha512, size: f.file_size.unwrap_or(0) },
            });
        }

        let p = path.clone();
        let entries = tokio::task::spawn_blocking(move || client_override_entries(&p))
            .await
            .map_err(|e| e.to_string())??;
        for (entry, rel, client_only) in entries {
            if rel.starts_with("mods/") && rel.ends_with(".jar") {
                let fname = file_name(&rel);
                let on_server = enabled.iter().find(|m| m.filename == fname);
                if let Some(m) = on_server { matched_server.insert(m.filename.clone()); }
                let auto = client_only || on_server.is_some();
                mods.push(ClientMod {
                    key: format!("file:{}", fname),
                    name: display_name(&rel),
                    path: rel.clone(),
                    origin: "pack",
                    reason: if client_only { "Client-side file from the modpack".into() }
                        else if auto { "Bundled with the modpack".into() }
                        else { "Bundled with the modpack, but removed from the server".into() },
                    auto,
                    included: auto,
                    overridden: false,
                    client_only,
                    source: Source::PackEntry(entry),
                });
            } else {
                files.push(ClientFile { path: rel, source: Source::PackEntry(entry) });
            }
        }
        mrpack = Some(path);
    }

    // Server mods the modpack doesn't account for (added by hand or through MSM)
    let extra: Vec<&&crate::mod_mgr::ModEntry> = enabled.iter().filter(|m| !matched_server.contains(&m.filename)).collect();
    let hashes: Vec<String> = extra.iter().filter(|m| m.on_modrinth()).map(|m| m.sha512.clone()).collect();
    let info = modrinth_info(&state.http_client, &snap.instance_dir, &hashes).await;
    for m in extra {
        let path = format!("mods/{}", m.filename);
        let server_file = Source::ServerFile(snap.server_path.join("mods").join(&m.filename));
        let (key, name, source, auto, reason) = match info.get(&m.sha512) {
            Some(i) => {
                let (auto, reason) = match i.client_side.as_str() {
                    "unsupported" => (false, "Server-only on Modrinth"),
                    "optional" => (true, "Optional on clients (Modrinth)"),
                    _ => (true, "Needed on both sides (Modrinth)"),
                };
                let source = Source::Remote { url: i.url.clone(), sha1: i.sha1.clone(), sha512: m.sha512.clone(), size: i.size };
                (i.project_id.clone(), i.title.clone(), source, auto, reason.to_string())
            }
            None => (
                format!("file:{}", m.filename),
                if m.name.is_empty() { display_name(&m.filename) } else { m.name.clone() },
                server_file,
                false,
                "Not on Modrinth — choose whether players need it".to_string(),
            ),
        };
        mods.push(ClientMod { key, name, path, origin: "server", reason, auto, included: auto, overridden: false, client_only: false, source });
    }

    for m in &mut mods {
        if let Some(&choice) = snap.choices.get(&m.key) {
            m.overridden = choice != m.auto;
            m.included = choice;
        }
    }
    mods.sort_by(|a, b| b.included.cmp(&a.included).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));

    Ok(ClientSet {
        name: snap.display_name.clone(),
        version_label,
        minecraft: snap.minecraft.clone(),
        loader: snap.loader.clone(),
        loader_version: snap.loader_version.clone(),
        file_count: files.len(),
        mods,
        files,
        mrpack,
    })
}

// ── packwiz generation ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum Served {
    Bytes(Vec<u8>),
    ServerFile(PathBuf),
    PackEntry { mrpack: PathBuf, entry: String },
}

pub struct GeneratedPack {
    fingerprint: String,
    pub set: ClientSet,
    served: HashMap<String, Served>,
    /// When this version of the mod list was built (shown on the join page)
    generated_at: i64,
}

fn read_source(source: &Source, mrpack: Option<&Path>) -> Result<Vec<u8>, String> {
    match source {
        Source::ServerFile(p) => std::fs::read(p).map_err(|e| format!("{}: {}", p.display(), e)),
        Source::PackEntry(entry) => {
            let path = mrpack.ok_or("modpack archive missing")?;
            read_pack_entry(path, entry)
        }
        Source::Remote { .. } => Err("remote files are not served by MSM".to_string()),
    }
}

fn read_pack_entry(mrpack: &Path, entry: &str) -> Result<Vec<u8>, String> {
    let file = std::fs::File::open(mrpack).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    let mut f = archive.by_name(entry).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    f.read_to_end(&mut out).map_err(|e| e.to_string())?;
    Ok(out)
}

fn to_served(source: &Source, mrpack: Option<&Path>) -> Served {
    match source {
        Source::ServerFile(p) => Served::ServerFile(p.clone()),
        Source::PackEntry(e) => Served::PackEntry { mrpack: mrpack.map(Path::to_path_buf).unwrap_or_default(), entry: e.clone() },
        Source::Remote { .. } => unreachable!("remote files get a metafile"),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// packwiz's `[versions]` keys, which packwiz-installer also uses to update Prism instances.
fn packwiz_loader_key(loader: &str) -> Option<&'static str> {
    match loader {
        "neoforge" => Some("neoforge"),
        "forge" => Some("forge"),
        "fabric" => Some("fabric"),
        "quilt" => Some("quilt"),
        _ => None,
    }
}

/// Turns a client set into packwiz files. Blocking: hashes every file MSM serves itself.
fn generate_packwiz(set: ClientSet, fingerprint: String) -> Result<GeneratedPack, String> {
    #[derive(Serialize)]
    struct Download<'a> { url: &'a str, #[serde(rename = "hash-format")] hash_format: &'a str, hash: &'a str }
    #[derive(Serialize)]
    struct MetaFile<'a> { name: &'a str, filename: String, side: &'a str, download: Download<'a> }
    #[derive(Serialize)]
    struct IndexEntry { file: String, hash: String, #[serde(skip_serializing_if = "std::ops::Not::not")] metafile: bool }
    #[derive(Serialize)]
    struct Index { #[serde(rename = "hash-format")] hash_format: &'static str, files: Vec<IndexEntry> }
    #[derive(Serialize)]
    struct PackIndexRef { file: &'static str, #[serde(rename = "hash-format")] hash_format: &'static str, hash: String }
    #[derive(Serialize)]
    struct Pack<'a> { name: &'a str, version: &'a str, #[serde(rename = "pack-format")] pack_format: &'static str, index: PackIndexRef, versions: BTreeMap<&'static str, String> }

    let mrpack = set.mrpack.clone();
    let mut served: HashMap<String, Served> = HashMap::new();
    let mut entries = Vec::new();
    let mut used_meta: std::collections::HashSet<String> = Default::default();

    for m in set.mods.iter().filter(|m| m.included) {
        match &m.source {
            Source::Remote { url, sha512, .. } => {
                let dir = m.path.rsplit_once('/').map(|(d, _)| d).unwrap_or("mods");
                let stem: String = display_name(&m.path).chars()
                    .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' })
                    .collect();
                let mut meta_path = format!("{}/{}.pw.toml", dir, stem);
                let mut n = 2;
                while !used_meta.insert(meta_path.clone()) {
                    meta_path = format!("{}/{}-{}.pw.toml", dir, stem, n);
                    n += 1;
                }
                let toml = toml::to_string(&MetaFile {
                    name: &m.name,
                    filename: file_name(&m.path),
                    side: if m.client_only { "client" } else { "both" },
                    download: Download { url, hash_format: "sha512", hash: sha512 },
                }).map_err(|e| e.to_string())?;
                entries.push(IndexEntry { file: meta_path.clone(), hash: sha256_hex(toml.as_bytes()), metafile: true });
                served.insert(meta_path, Served::Bytes(toml.into_bytes()));
            }
            local => {
                let bytes = read_source(local, mrpack.as_deref())?;
                entries.push(IndexEntry { file: m.path.clone(), hash: sha256_hex(&bytes), metafile: false });
                served.insert(m.path.clone(), to_served(local, mrpack.as_deref()));
            }
        }
    }
    for f in &set.files {
        let bytes = read_source(&f.source, mrpack.as_deref())?;
        entries.push(IndexEntry { file: f.path.clone(), hash: sha256_hex(&bytes), metafile: false });
        served.insert(f.path.clone(), to_served(&f.source, mrpack.as_deref()));
    }
    entries.sort_by(|a, b| a.file.cmp(&b.file));

    let index = toml::to_string(&Index { hash_format: "sha256", files: entries }).map_err(|e| e.to_string())?;
    let mut versions = BTreeMap::new();
    versions.insert("minecraft", set.minecraft.clone());
    if let (Some(key), Some(v)) = (set.loader.as_deref().and_then(packwiz_loader_key), &set.loader_version) {
        versions.insert(key, v.clone());
    }
    let pack = toml::to_string(&Pack {
        name: &set.name,
        version: &set.version_label,
        pack_format: "packwiz:1.1.0",
        index: PackIndexRef { file: "index.toml", hash_format: "sha256", hash: sha256_hex(index.as_bytes()) },
        versions,
    }).map_err(|e| e.to_string())?;
    served.insert("index.toml".into(), Served::Bytes(index.into_bytes()));
    served.insert("pack.toml".into(), Served::Bytes(pack.into_bytes()));

    Ok(GeneratedPack { fingerprint, set, served, generated_at: chrono::Utc::now().timestamp() })
}

// ── Cache ─────────────────────────────────────────────────────────────────────

/// Changes whenever anything that affects what players get changes.
fn fingerprint(snap: &InstanceSnapshot) -> String {
    let mut h = Sha256::new();
    h.update(format!("{:?}|{:?}|{:?}|{:?}|{:?}", snap.minecraft, snap.loader, snap.loader_version, snap.pack_version, snap.choices));
    if let Ok(entries) = std::fs::read_dir(snap.server_path.join("mods")) {
        let mut list: Vec<String> = entries
            .flatten()
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                let mtime = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
                Some(format!("{}:{}:{}", e.file_name().to_string_lossy(), meta.len(), mtime))
            })
            .collect();
        list.sort();
        h.update(list.join("\n"));
    }
    hex::encode(h.finalize())
}

async fn snapshot(state: &AppState, instance_id: &str) -> Option<InstanceSnapshot> {
    let instances = state.instances.read().await;
    let inst = instances.get(instance_id)?;
    let meta = &inst.config.instance;
    Some(InstanceSnapshot {
        display_name: meta.display_name.clone().unwrap_or_else(|| meta.name.clone()),
        server_path: inst.config.server.path.clone(),
        instance_dir: inst.instance_dir.clone(),
        minecraft: meta.minecraft_version.clone(),
        loader: meta.loader.clone(),
        loader_version: meta.loader_version.clone(),
        pack_version: meta.modrinth_project_id.as_ref().and(meta.modrinth_version_id.clone()),
        choices: inst.config.distribution.as_ref().map(|d| d.mods.clone()).unwrap_or_default(),
    })
}

/// The current generated pack for an instance, rebuilt if anything changed since last time.
pub async fn current_pack(state: &Arc<AppState>, instance_id: &str) -> Result<Arc<GeneratedPack>, String> {
    let snap = snapshot(state, instance_id).await.ok_or("Instance not found")?;
    let fp = fingerprint(&snap);
    // One generation at a time; later requests reuse the result
    let mut packs = state.client_packs.lock().await;
    if let Some(p) = packs.get(instance_id).filter(|p| p.fingerprint == fp) {
        return Ok(p.clone());
    }
    let set = build_client_set(state, &snap).await?;
    // Scanning may have rewritten files; key the result by the state after the scan
    let fp = fingerprint(&snap);
    let pack = tokio::task::spawn_blocking(move || generate_packwiz(set, fp))
        .await
        .map_err(|e| e.to_string())??;
    let pack = Arc::new(pack);
    packs.insert(instance_id.to_string(), pack.clone());
    Ok(pack)
}

// ── Downloads ─────────────────────────────────────────────────────────────────

fn add_zip_file<W: Write + std::io::Seek>(zip: &mut zip::ZipWriter<W>, name: &str, bytes: &[u8]) -> Result<(), String> {
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    zip.start_file(name, opts).map_err(|e| e.to_string())?;
    zip.write_all(bytes).map_err(|e| e.to_string())
}

async fn bootstrap_jar(state: &AppState) -> Result<Vec<u8>, String> {
    let path = crate::config::data_dir().join("cache").join("packwiz-installer-bootstrap.jar");
    if let Ok(bytes) = std::fs::read(&path) {
        return Ok(bytes);
    }
    let bytes = state.http_client.get(BOOTSTRAP_URL).header("User-Agent", UA).send().await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("Couldn't download packwiz-installer-bootstrap: {}", e))?
        .bytes().await.map_err(|e| e.to_string())?
        .to_vec();
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let _ = std::fs::write(&path, &bytes);
    Ok(bytes)
}

/// Prism Launcher instance: the right Minecraft/loader, packwiz-installer as a pre-launch
/// command, and the server in the multiplayer list.
fn prism_instance_zip(set: &ClientSet, pack_url: &str, server_address: &str, bootstrap: &[u8]) -> Result<Vec<u8>, String> {
    let mut components = vec![serde_json::json!({ "uid": "net.minecraft", "version": set.minecraft, "important": true })];
    let loader_uid = match set.loader.as_deref() {
        Some("neoforge") => Some("net.neoforged"),
        Some("forge") => Some("net.minecraftforge"),
        Some("fabric") => Some("net.fabricmc.fabric-loader"),
        Some("quilt") => Some("org.quiltmc.quilt-loader"),
        _ => None,
    };
    if matches!(set.loader.as_deref(), Some("fabric") | Some("quilt")) {
        components.push(serde_json::json!({ "uid": "net.fabricmc.intermediary", "version": set.minecraft }));
    }
    if let (Some(uid), Some(v)) = (loader_uid, &set.loader_version) {
        components.push(serde_json::json!({ "uid": uid, "version": v }));
    }
    let mmc_pack = serde_json::to_vec_pretty(&serde_json::json!({ "components": components, "formatVersion": 1 }))
        .map_err(|e| e.to_string())?;

    // Legacy key=value format (like MultiMC exports), which Prism migrates on import.
    // '#' starts a comment and '\' escapes, so neither may appear unescaped in values.
    let clean = |s: &str| s.replace('\\', "\\\\").replace('#', "\\#").replace('\n', " ");
    let cfg = format!(
        "InstanceType=OneSix\nname={}\nOverrideCommands=true\nPreLaunchCommand=\"$INST_JAVA\" -jar packwiz-installer-bootstrap.jar {}\nJoinServerOnLaunch=true\nJoinServerOnLaunchAddress={}\n",
        clean(&set.name), clean(pack_url), clean(server_address),
    );

    let servers = fastnbt::Value::Compound(HashMap::from([(
        "servers".to_string(),
        fastnbt::Value::List(vec![fastnbt::Value::Compound(HashMap::from([
            ("name".to_string(), fastnbt::Value::String(set.name.clone())),
            ("ip".to_string(), fastnbt::Value::String(server_address.to_string())),
        ]))]),
    )]));
    let servers_dat = fastnbt::to_bytes(&servers).map_err(|e| e.to_string())?;

    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    add_zip_file(&mut zip, "instance.cfg", cfg.as_bytes())?;
    add_zip_file(&mut zip, "mmc-pack.json", &mmc_pack)?;
    add_zip_file(&mut zip, ".minecraft/packwiz-installer-bootstrap.jar", bootstrap)?;
    add_zip_file(&mut zip, ".minecraft/servers.dat", &servers_dat)?;
    Ok(zip.finish().map_err(|e| e.to_string())?.into_inner())
}

/// A .mrpack of what players get right now (Modrinth App and most other launchers).
fn mrpack_export(set: &ClientSet) -> Result<Vec<u8>, String> {
    let mut files = Vec::new();
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for m in set.mods.iter().filter(|m| m.included) {
        match &m.source {
            Source::Remote { url, sha1: Some(sha1), sha512, size } => files.push(serde_json::json!({
                "path": m.path,
                "hashes": { "sha1": sha1, "sha512": sha512 },
                "env": { "client": "required", "server": if m.client_only { "unsupported" } else { "required" } },
                "downloads": [url],
                "fileSize": size,
            })),
            // No SHA-1 known: ship the file itself (the Modrinth format requires both hashes)
            other => {
                let bytes = match other {
                    Source::Remote { .. } => continue,
                    local => read_source(local, set.mrpack.as_deref())?,
                };
                add_zip_file(&mut zip, &format!("overrides/{}", m.path), &bytes)?;
            }
        }
    }
    for f in &set.files {
        add_zip_file(&mut zip, &format!("overrides/{}", f.path), &read_source(&f.source, set.mrpack.as_deref())?)?;
    }
    let mut deps = serde_json::Map::new();
    deps.insert("minecraft".into(), set.minecraft.clone().into());
    let dep_key = match set.loader.as_deref() {
        Some("fabric") => Some("fabric-loader"),
        Some("quilt") => Some("quilt-loader"),
        Some("forge") => Some("forge"),
        Some("neoforge") => Some("neoforge"),
        _ => None,
    };
    if let (Some(k), Some(v)) = (dep_key, &set.loader_version) {
        deps.insert(k.into(), v.clone().into());
    }
    let index = serde_json::to_vec_pretty(&serde_json::json!({
        "formatVersion": 1,
        "game": "minecraft",
        "versionId": set.version_label,
        "name": set.name,
        "files": files,
        "dependencies": deps,
    })).map_err(|e| e.to_string())?;
    add_zip_file(&mut zip, "modrinth.index.json", &index)?;
    Ok(zip.finish().map_err(|e| e.to_string())?.into_inner())
}

// ── Public endpoints ──────────────────────────────────────────────────────────

async fn instance_for_token(state: &AppState, token: &str) -> Option<(String, u16)> {
    if token.len() < 16 { return None; }
    let instances = state.instances.read().await;
    instances.iter().find_map(|(id, inst)| {
        let d = inst.config.distribution.as_ref()?;
        (d.enabled && d.token == token).then(|| (id.clone(), inst.config.instance.port))
    })
}

/// Base URL players reach MSM at: `[web] public_url`, or the address this request came in on.
fn base_url(state: &AppState, headers: &HeaderMap) -> String {
    if let Some(url) = state.global_config.web.as_ref().and_then(|w| w.public_url.clone()) {
        return url.trim_end_matches('/').to_string();
    }
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("localhost");
    let proto = headers.get("x-forwarded-proto").and_then(|h| h.to_str().ok()).unwrap_or("http");
    format!("{}://{}", proto, host)
}

/// Minecraft server address for the multiplayer list: the host players use, plus the port.
fn server_address(base: &str, port: u16) -> String {
    let host = base.split("://").nth(1).unwrap_or(base).split('/').next().unwrap_or("");
    let host = if host.starts_with('[') { host.split(']').next().map(|h| format!("{}]", h)).unwrap_or_default() }
        else { host.split(':').next().unwrap_or(host).to_string() };
    if port == 25565 { host } else { format!("{}:{}", host, port) }
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "Not found").into_response()
}

fn pack_error(e: String) -> Response {
    tracing::warn!("Player pack: {}", e);
    (StatusCode::SERVICE_UNAVAILABLE, format!("The mod list is temporarily unavailable: {}", e)).into_response()
}

pub async fn pack_redirect(UrlPath(token): UrlPath<String>) -> Redirect {
    Redirect::permanent(&format!("/pack/{}/", token))
}

pub async fn join_page(
    State(state): State<Arc<AppState>>,
    UrlPath(token): UrlPath<String>,
    headers: HeaderMap,
) -> Response {
    let Some((id, port)) = instance_for_token(&state, &token).await else { return not_found() };
    let pack = match current_pack(&state, &id).await { Ok(p) => p, Err(e) => return pack_error(e) };
    let base = base_url(&state, &headers);
    Html(render_join_page(&pack, &format!("{}/pack/{}", base, token), &server_address(&base, port))).into_response()
}

pub async fn pack_file(
    State(state): State<Arc<AppState>>,
    UrlPath((token, rest)): UrlPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let Some((id, port)) = instance_for_token(&state, &token).await else { return not_found() };
    let pack = match current_pack(&state, &id).await { Ok(p) => p, Err(e) => return pack_error(e) };
    let base = base_url(&state, &headers);
    let slug = file_slug(&pack.set.name);

    match rest.as_str() {
        "_msm/prism.zip" => match build_prism(&state, &pack, &base, &token, port).await {
            Ok(bytes) => download(bytes, "application/zip", &format!("{}.zip", slug)),
            Err(e) => pack_error(e),
        },
        "_msm/pack.mrpack" => {
            let p = pack.clone();
            match tokio::task::spawn_blocking(move || mrpack_export(&p.set)).await {
                Ok(Ok(bytes)) => download(bytes, "application/x-modrinth-modpack+zip", &format!("{}.mrpack", slug)),
                Ok(Err(e)) => pack_error(e),
                Err(e) => pack_error(e.to_string()),
            }
        }
        path => match pack.served.get(path).cloned() {
            Some(Served::Bytes(b)) => {
                let ct = if path.ends_with(".toml") { "application/toml; charset=utf-8" } else { "application/octet-stream" };
                ([(header::CONTENT_TYPE, ct)], b).into_response()
            }
            Some(other) => {
                let result = tokio::task::spawn_blocking(move || match other {
                    Served::ServerFile(p) => std::fs::read(&p).map_err(|e| e.to_string()),
                    Served::PackEntry { mrpack, entry } => read_pack_entry(&mrpack, &entry),
                    Served::Bytes(b) => Ok(b),
                }).await;
                match result {
                    Ok(Ok(bytes)) => ([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response(),
                    _ => not_found(),
                }
            }
            None => not_found(),
        },
    }
}

async fn build_prism(state: &AppState, pack: &GeneratedPack, base: &str, token: &str, port: u16) -> Result<Vec<u8>, String> {
    let bootstrap = bootstrap_jar(state).await?;
    let pack_url = format!("{}/pack/{}/pack.toml", base, token);
    prism_instance_zip(&pack.set, &pack_url, &server_address(base, port), &bootstrap)
}

fn file_slug(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

/// Files the Discord bot hands out: always the .mrpack; the Prism instance only when
/// `public_url` is set, since the instance must point at an address players can reach.
pub struct PlayerDownloads {
    pub name: String,
    pub summary: String,
    pub join_url: Option<String>,
    pub mrpack: (String, Vec<u8>),
    pub prism: Option<(String, Vec<u8>)>,
}

pub async fn player_downloads(state: &Arc<AppState>, instance_id: &str, with_prism: bool) -> Result<PlayerDownloads, String> {
    let (token, port) = {
        let instances = state.instances.read().await;
        let inst = instances.get(instance_id).ok_or("Instance not found")?;
        let d = inst.config.distribution.as_ref().filter(|d| d.enabled).ok_or("Sharing isn't turned on for this server")?;
        (d.token.clone(), inst.config.instance.port)
    };
    let pack = current_pack(state, instance_id).await?;
    let slug = file_slug(&pack.set.name);
    let p = pack.clone();
    let mrpack = tokio::task::spawn_blocking(move || mrpack_export(&p.set)).await.map_err(|e| e.to_string())??;
    let public_base = state.global_config.web.as_ref().and_then(|w| w.public_url.clone()).map(|u| u.trim_end_matches('/').to_string());
    let prism = match (&public_base, with_prism) {
        (Some(base), true) => Some((format!("{}.zip", slug), build_prism(state, &pack, base, &token, port).await?)),
        _ => None,
    };
    let set = &pack.set;
    let loader = match (set.loader.as_deref(), &set.loader_version) {
        (Some(l), Some(v)) => format!("{} {}", crate::setup::loader_name(l), v),
        _ => "Vanilla".to_string(),
    };
    Ok(PlayerDownloads {
        name: set.name.clone(),
        summary: format!("Minecraft {} · {} · {} mods", set.minecraft, loader, set.mods.iter().filter(|m| m.included).count()),
        join_url: public_base.map(|b| format!("{}/pack/{}/", b, token)),
        mrpack: (format!("{}.mrpack", slug), mrpack),
        prism,
    })
}

fn download(bytes: Vec<u8>, content_type: &str, filename: &str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type.to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{}\"", filename.replace('"', ""))),
        ],
        bytes,
    ).into_response()
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

fn render_join_page(pack: &GeneratedPack, pack_base: &str, address: &str) -> String {
    let set = &pack.set;
    let loader = match (set.loader.as_deref(), &set.loader_version) {
        (Some(l), Some(v)) => format!("{} {}", crate::setup::loader_name(l), v),
        (Some(l), None) => crate::setup::loader_name(l).to_string(),
        _ => "Vanilla".to_string(),
    };
    let count = set.mods.iter().filter(|m| m.included).count();
    let mod_list: String = set.mods.iter().filter(|m| m.included)
        .map(|m| format!("<li>{}{}</li>", esc(&m.name), if m.client_only { " <span>client</span>" } else { "" }))
        .collect();
    include_str!("ui/join.html")
        .replace("{{name}}", &esc(&set.name))
        .replace("{{minecraft}}", &esc(&set.minecraft))
        .replace("{{loader}}", &esc(&loader))
        .replace("{{address}}", &esc(address))
        .replace("{{count}}", &count.to_string())
        .replace("{{mods}}", &mod_list)
        .replace("{{pack_url}}", &esc(&format!("{}/pack.toml", pack_base)))
        .replace("{{prism_url}}", &esc(&format!("{}/_msm/prism.zip", pack_base)))
        .replace("{{mrpack_url}}", &esc(&format!("{}/_msm/pack.mrpack", pack_base)))
        .replace("{{updated}}", &pack.generated_at.to_string())
}

// ── Dashboard API ─────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct DistributionStatus {
    pub enabled: bool,
    pub join_path: Option<String>,
    pub public_url: Option<String>,
    pub pack_port: Option<u16>,
    pub set: Option<ClientSet>,
    pub error: Option<String>,
}

pub async fn status(state: &Arc<AppState>, instance_id: &str) -> Result<DistributionStatus, String> {
    let dist = {
        let instances = state.instances.read().await;
        instances.get(instance_id).ok_or("Instance not found")?.config.distribution.clone().unwrap_or_default()
    };
    let web = state.global_config.web.clone().unwrap_or_default();
    let mut out = DistributionStatus {
        enabled: dist.enabled,
        join_path: dist.enabled.then(|| format!("/pack/{}/", dist.token)),
        public_url: web.public_url,
        pack_port: web.pack_port,
        set: None,
        error: None,
    };
    if dist.enabled {
        match current_pack(state, instance_id).await {
            Ok(p) => out.set = Some(p.set.clone()),
            Err(e) => out.error = Some(e),
        }
    }
    Ok(out)
}

fn random_token() -> String {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .expect("failed to read /dev/urandom");
    hex::encode(bytes)
}

/// Applies a change to an instance's sharing settings and saves msm.toml.
pub async fn update_config(
    state: &AppState,
    instance_id: &str,
    change: impl FnOnce(&mut crate::config::DistributionConfig),
) -> Result<(), String> {
    let mut instances = state.instances.write().await;
    let inst = instances.get_mut(instance_id).ok_or("Instance not found")?;
    let dist = inst.config.distribution.get_or_insert_with(Default::default);
    change(dist);
    if dist.token.is_empty() {
        dist.token = random_token();
    }
    let toml_str = toml::to_string_pretty(&inst.config).map_err(|e| e.to_string())?;
    std::fs::write(inst.instance_dir.join("msm.toml"), toml_str).map_err(|e| format!("Failed to save msm.toml: {}", e))
}

pub fn new_token() -> String {
    random_token()
}

/// After an update changes what players need, tell them (Discord) if the instance is shared.
pub async fn announce_update(state: &AppState, instance_id: &str) {
    let shared = state.instances.read().await.get(instance_id)
        .and_then(|i| i.config.distribution.as_ref())
        .is_some_and(|d| d.enabled);
    if shared {
        let join_url = public_join_url(state, instance_id).await;
        let _ = state.log_tx.send(crate::state::WsEvent::PlayerModsChanged { instance_id: instance_id.to_string(), join_url });
    }
}

/// Link to an instance's join page for Discord messages, if players can reach MSM publicly.
pub async fn public_join_url(state: &AppState, instance_id: &str) -> Option<String> {
    let base = state.global_config.web.as_ref()?.public_url.clone()?;
    let instances = state.instances.read().await;
    let d = instances.get(instance_id)?.config.distribution.as_ref()?;
    d.enabled.then(|| format!("{}/pack/{}/", base.trim_end_matches('/'), d.token))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_with(mods: Vec<ClientMod>) -> ClientSet {
        ClientSet {
            name: "Test Pack".into(), version_label: "1.0".into(), minecraft: "1.21.1".into(),
            loader: Some("fabric".into()), loader_version: Some("0.16.10".into()),
            file_count: 0, mods, files: vec![], mrpack: None,
        }
    }

    fn remote(name: &str, path: &str, client_only: bool, included: bool) -> ClientMod {
        ClientMod {
            key: name.into(), name: name.into(), path: path.into(), origin: "pack", reason: String::new(),
            auto: included, included, overridden: false, client_only,
            source: Source::Remote { url: format!("https://cdn.modrinth.com/data/{}/versions/x/{}", name, file_name(path)), sha1: Some("aa".into()), sha512: "bb".into(), size: 10 },
        }
    }

    #[test]
    fn packwiz_output_is_consistent() {
        let set = set_with(vec![
            remote("sodium", "mods/sodium-0.6.jar", true, true),
            remote("create", "mods/create 6.0.jar", false, true),
            remote("serverstuff", "mods/serverstuff.jar", false, false),
        ]);
        let pack = generate_packwiz(set, "fp".into()).unwrap();
        let Served::Bytes(index) = &pack.served["index.toml"] else { panic!() };
        let Served::Bytes(pack_toml) = &pack.served["pack.toml"] else { panic!() };
        let pack_toml = String::from_utf8(pack_toml.clone()).unwrap();
        assert!(pack_toml.contains(&format!("hash = \"{}\"", sha256_hex(index))), "pack.toml must carry index.toml's hash");
        assert!(pack_toml.contains("fabric = \"0.16.10\"") && pack_toml.contains("minecraft = \"1.21.1\""));
        assert!(pack_toml.contains("pack-format = \"packwiz:1.1.0\""));

        let index = String::from_utf8(index.clone()).unwrap();
        assert!(!index.contains("serverstuff"), "excluded mods are not published");
        let Served::Bytes(sodium) = &pack.served["mods/sodium-0.6.pw.toml"] else { panic!() };
        let sodium = String::from_utf8(sodium.clone()).unwrap();
        assert!(sodium.contains("side = \"client\"") && sodium.contains("hash-format = \"sha512\""));
        assert!(index.contains(&sha256_hex(sodium.as_bytes())), "index lists each metafile's hash");
        assert!(pack.served.contains_key("mods/create-6.0.pw.toml"), "unsafe characters in metafile names are replaced");
    }

    #[test]
    fn server_address_from_base_url() {
        assert_eq!(server_address("https://mc.example.com:8443", 25565), "mc.example.com");
        assert_eq!(server_address("http://192.168.1.5:8080", 25570), "192.168.1.5:25570");
        assert_eq!(server_address("http://[::1]:8080", 25565), "[::1]");
    }

    #[test]
    fn project_ids_from_modrinth_urls() {
        assert_eq!(project_from_url("https://cdn.modrinth.com/data/AANobbMI/versions/abc/sodium.jar").as_deref(), Some("AANobbMI"));
        assert_eq!(project_from_url("https://github.com/x/y/releases/a.jar"), None);
    }

    #[test]
    fn prism_zip_has_instance_files() {
        let set = set_with(vec![]);
        let bytes = prism_instance_zip(&set, "https://mc.example.com/pack/tok/pack.toml", "mc.example.com", b"jar").unwrap();
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let mut cfg = String::new();
        zip.by_name("instance.cfg").unwrap().read_to_string(&mut cfg).unwrap();
        assert!(cfg.contains("PreLaunchCommand=\"$INST_JAVA\" -jar packwiz-installer-bootstrap.jar https://mc.example.com/pack/tok/pack.toml"));
        assert!(cfg.contains("JoinServerOnLaunchAddress=mc.example.com"));
        let mut mmc = String::new();
        zip.by_name("mmc-pack.json").unwrap().read_to_string(&mut mmc).unwrap();
        assert!(mmc.contains("net.fabricmc.fabric-loader") && mmc.contains("net.fabricmc.intermediary"));
        assert!(zip.by_name(".minecraft/packwiz-installer-bootstrap.jar").is_ok());
        assert!(zip.by_name(".minecraft/servers.dat").is_ok());
    }
}
