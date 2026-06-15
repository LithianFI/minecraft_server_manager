use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use sha2::{Digest, Sha512};

use crate::state::{AppState, InstanceStatus, WsEvent};

// ── Modrinth API types ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ModrinthVersionFile {
    url: String,
    filename: String,
    primary: bool,
}

#[derive(Deserialize)]
struct ModrinthVersion {
    project_id: String,
    files: Vec<ModrinthVersionFile>,
}

// ── mrpack manifest ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct PackIndex {
    name: String,
    dependencies: HashMap<String, String>,
    files: Vec<PackFile>,
}

#[derive(Deserialize)]
struct PackFile {
    path: String,
    hashes: PackHashes,
    downloads: Vec<String>,
    #[serde(default)]
    env: Option<EnvSpec>,
}

#[derive(Deserialize)]
struct PackHashes {
    sha512: Option<String>,
}

#[derive(Deserialize)]
struct EnvSpec {
    server: Option<String>,
}

// ── Fabric meta ────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct FabricInstallerMeta {
    version: String,
    stable: bool,
}

// ── Public entry point ─────────────────────────────────────────────────────────

pub struct ImportRequest {
    pub version_id: String,
    pub server_path: String,
    pub instance_name: String,
    pub port: u16,
}

pub async fn import_modpack(state: Arc<AppState>, req: ImportRequest) {
    let server_path = req.server_path.clone();
    match do_import(&state, req).await {
        Ok(()) => {
            let _ = state.log_tx.send(WsEvent::ModpackLog { message: "Done!".to_string() });
            let _ = state.log_tx.send(WsEvent::ModpackDone { server_path });
        }
        Err(e) => {
            let _ = state.log_tx.send(WsEvent::ModpackFailed { error: e });
        }
    }
}

async fn do_import(state: &Arc<AppState>, req: ImportRequest) -> Result<(), String> {
    let log = |msg: &str| {
        let _ = state.log_tx.send(WsEvent::ModpackLog { message: msg.to_string() });
    };

    // ── 1. Fetch version info ──────────────────────────────────────────────────
    log("Fetching modpack info from Modrinth…");

    let version: ModrinthVersion = state.http_client
        .get(format!("https://api.modrinth.com/v2/version/{}", req.version_id))
        .header("User-Agent", "msm/0.1")
        .send().await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("Failed to fetch version: {}", e))?
        .json().await
        .map_err(|e| format!("Failed to parse version info: {}", e))?;

    let modrinth_project_id = version.project_id.clone();
    let mrpack_file = version.files.iter()
        .find(|f| f.primary)
        .ok_or_else(|| "No primary file found in this version".to_string())?;

    // ── 2. Download mrpack ─────────────────────────────────────────────────────
    let msg = format!("Downloading {}…", mrpack_file.filename);
    log(&msg);

    let mrpack_bytes = state.http_client
        .get(&mrpack_file.url)
        .header("User-Agent", "msm/0.1")
        .send().await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("Download failed: {}", e))?
        .bytes().await
        .map_err(|e| format!("Download read error: {}", e))?;

    // ── 3. Parse manifest ──────────────────────────────────────────────────────
    log("Parsing modpack manifest…");

    let bytes_vec = mrpack_bytes.to_vec();
    let index = tokio::task::spawn_blocking({
        let b = bytes_vec.clone();
        move || parse_mrpack_index(&b)
    }).await.map_err(|_| "Manifest parse task panicked".to_string())??;

    let mc_version = index.dependencies.get("minecraft")
        .cloned()
        .ok_or_else(|| "Modpack has no minecraft version in dependencies".to_string())?;

    let (loader, loader_version) = detect_loader(&index.dependencies);

    let info_msg = format!(
        "Modpack: {} — MC {} | {}{}",
        index.name,
        mc_version,
        loader.as_deref().unwrap_or("vanilla"),
        loader_version.as_deref().map(|v| format!(" {}", v)).unwrap_or_default()
    );
    log(&info_msg);

    // ── 4. Check for ID collision ──────────────────────────────────────────────
    let instance_id = slugify(&req.instance_name);
    if instance_id.is_empty() {
        return Err("Instance name produces an empty ID".to_string());
    }
    if state.instances.read().await.contains_key(&instance_id) {
        return Err(format!("An instance named '{}' already exists", instance_id));
    }

    // ── 5. Create server directory ─────────────────────────────────────────────
    let server_path = if req.server_path.trim().is_empty() {
        crate::config::data_dir().join("servers").join(&instance_id)
    } else {
        PathBuf::from(&req.server_path)
    };
    let msg = format!("Creating directory {}…", server_path.display());
    log(&msg);
    tokio::fs::create_dir_all(&server_path).await
        .map_err(|e| format!("Failed to create directory: {}", e))?;

    // ── 6. Extract overrides ───────────────────────────────────────────────────
    log("Extracting config overrides…");
    {
        let b = bytes_vec.clone();
        let dest = server_path.clone();
        tokio::task::spawn_blocking(move || extract_overrides(&b, &dest))
            .await
            .map_err(|_| "Override extraction panicked".to_string())??;
    }

    // ── 7. Install loader ──────────────────────────────────────────────────────
    let make_log: Arc<dyn Fn(String) -> WsEvent + Send + Sync + 'static> =
        Arc::new(|msg| WsEvent::ModpackLog { message: msg });

    match loader.as_deref() {
        Some("neoforge") => {
            let nf_ver = loader_version.as_deref().unwrap_or("").to_string();
            let msg = format!("Installing NeoForge {}…", nf_ver);
            log(&msg);
            crate::setup::download_and_run_installer(state, &nf_ver, &server_path, make_log).await?;
        }
        Some("forge") => {
            let forge_ver = loader_version.as_deref().unwrap_or("").to_string();
            let msg = format!("Installing Forge {}…", forge_ver);
            log(&msg);
            install_forge(state, &mc_version, &forge_ver, &server_path, make_log).await?;
        }
        Some("fabric") => {
            let fabric_ver = loader_version.as_deref().unwrap_or("").to_string();
            let msg = format!("Installing Fabric loader {}…", fabric_ver);
            log(&msg);
            install_fabric(state, &mc_version, &fabric_ver, &server_path, make_log).await?;
        }
        _ => {
            log("No known mod loader — skipping loader installation.");
        }
    }

    // ── 8. Download mod files ──────────────────────────────────────────────────
    let server_files: Vec<&PackFile> = index.files.iter()
        .filter(|f| {
            f.env.as_ref()
                .and_then(|e| e.server.as_deref())
                .map(|s| s != "unsupported")
                .unwrap_or(true)
        })
        .collect();

    let msg = format!("Downloading {} mod files…", server_files.len());
    log(&msg);
    let _ = tokio::fs::create_dir_all(server_path.join("mods")).await;

    for (i, file) in server_files.iter().enumerate() {
        let dest = server_path.join(&file.path);
        if let Some(parent) = dest.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }

        let url = match file.downloads.first() {
            Some(u) => u,
            None => {
                let msg = format!("[{}/{}] Skipping {} (no download URL)", i + 1, server_files.len(), file.path);
                log(&msg);
                continue;
            }
        };

        let bytes = state.http_client
            .get(url)
            .header("User-Agent", "msm/0.1")
            .send().await
            .and_then(|r| r.error_for_status())
            .map_err(|e| format!("Failed to download {}: {}", file.path, e))?
            .bytes().await
            .map_err(|e| format!("Failed to read {}: {}", file.path, e))?;

        if let Some(expected) = &file.hashes.sha512 {
            let actual = hex::encode(Sha512::digest(&bytes));
            if &actual != expected {
                return Err(format!("Hash mismatch for {}", file.path));
            }
        }

        tokio::fs::write(&dest, &bytes[..]).await
            .map_err(|e| format!("Failed to write {}: {}", file.path, e))?;

        let msg = format!("[{}/{}] {}", i + 1, server_files.len(), file.path);
        log(&msg);
    }

    // ── 9. Detect Java version ─────────────────────────────────────────────────
    let required_java = crate::java::recommended_java(&mc_version);
    let java_path = tokio::task::spawn_blocking(move || crate::java::find_java(required_java))
        .await
        .unwrap_or(None);

    match &java_path {
        Some(p) => {
            let msg = format!(
                "Java {required_java} required — found at {} — configuring instance.",
                p.display()
            );
            log(&msg);
        }
        None => {
            // find_java returns None when system default is already correct
            if crate::java::java_version("java") != Some(required_java) {
                let msg = format!(
                    "Warning: Java {required_java} required but not found on this system. \
                     Set the Java path manually in instance settings."
                );
                log(&msg);
            }
        }
    }

    // ── 10. Register instance ──────────────────────────────────────────────────
    log("Registering instance…");

    let instance_dir = crate::config::data_dir().join("instances").join(&instance_id);
    tokio::fs::create_dir_all(&instance_dir).await
        .map_err(|e| format!("Failed to create instance directory: {}", e))?;

    let config = crate::config::InstanceConfig {
        restart: None,
        instance: crate::config::InstanceMeta {
            name: instance_id.clone(),
            display_name: Some(req.instance_name),
            minecraft_version: mc_version,
            loader,
            loader_version,
            port: req.port,
            modrinth_project_id: Some(modrinth_project_id),
        },
        server: crate::config::ServerConfig {
            path: server_path,
            java_opts: None,
            java_path: java_path.map(|p| p.to_string_lossy().into_owned()),
        },
        backup: Some(crate::config::BackupConfig {
            enabled: false,
            schedule: None,
            keep_count: 10,
            world_only: false,
        }),
        alerts: None,
        schedules: vec![],
    };

    let toml_str = toml::to_string_pretty(&config)
        .map_err(|e| format!("Failed to serialize config: {}", e))?;
    tokio::fs::write(instance_dir.join("msm.toml"), toml_str).await
        .map_err(|e| format!("Failed to write msm.toml: {}", e))?;

    let inst_state = crate::state::InstanceState {
        id: instance_id.clone(),
        instance_dir,
        config,
        status: InstanceStatus::Stopped,
        players: std::collections::HashSet::new(),
        started_at: None,
        log_buffer: std::collections::VecDeque::new(),
        ram_mb: None,
        tps: None,
        restart_attempts: 0,
        cpu_pct: None,
        low_tps_streak: 0,
        high_ram_alerted: false,
    };

    let info = crate::state::InstanceInfo::from(&inst_state);
    state.instances.write().await.insert(instance_id, inst_state);
    let _ = state.log_tx.send(WsEvent::InstanceAdded { instance: info });

    Ok(())
}

// ── Loader installers ──────────────────────────────────────────────────────────

pub(crate) async fn install_forge(
    state: &Arc<AppState>,
    mc_version: &str,
    forge_version: &str,
    work_dir: &Path,
    make_log: Arc<dyn Fn(String) -> WsEvent + Send + Sync + 'static>,
) -> Result<(), String> {
    let url = format!(
        "https://maven.minecraftforge.net/net/minecraftforge/forge/{mc}-{forge}/forge-{mc}-{forge}-installer.jar",
        mc = mc_version,
        forge = forge_version,
    );

    let _ = state.log_tx.send(make_log("Downloading Forge installer…".to_string()));

    let bytes = state.http_client
        .get(&url)
        .header("User-Agent", "msm/0.1")
        .send().await
        .map_err(|e| format!("Forge download failed: {}", e))?;

    if !bytes.status().is_success() {
        return Err(format!("Forge download failed: HTTP {}", bytes.status()));
    }

    let bytes = bytes.bytes().await.map_err(|e| format!("Read error: {}", e))?;
    let installer_path = work_dir.join(format!("forge-{}-{}-installer.jar", mc_version, forge_version));
    tokio::fs::write(&installer_path, &bytes[..]).await
        .map_err(|e| format!("Failed to write Forge installer: {}", e))?;

    let _ = state.log_tx.send(make_log("Running Forge installer — this may take a few minutes…".to_string()));

    let log_tx = state.log_tx.clone();
    let inst_path = installer_path.clone();
    let work_clone = work_dir.to_path_buf();
    let make_log_clone = make_log.clone();

    let result = tokio::task::spawn_blocking(move || {
        crate::setup::run_installer_blocking(&inst_path, &work_clone, &log_tx, make_log_clone)
    }).await;

    let _ = tokio::fs::remove_file(&installer_path).await;

    match result {
        Ok(r) => r,
        Err(_) => Err("Forge installer task panicked".to_string()),
    }
}

pub(crate) async fn install_fabric(
    state: &Arc<AppState>,
    mc_version: &str,
    loader_version: &str,
    work_dir: &Path,
    make_log: Arc<dyn Fn(String) -> WsEvent + Send + Sync + 'static>,
) -> Result<(), String> {
    let _ = state.log_tx.send(make_log("Fetching Fabric installer version…".to_string()));

    let metas: Vec<FabricInstallerMeta> = state.http_client
        .get("https://meta.fabricmc.net/v2/versions/installer")
        .header("User-Agent", "msm/0.1")
        .send().await
        .map_err(|e| format!("Failed to fetch Fabric installer list: {}", e))?
        .json().await
        .map_err(|e| format!("Failed to parse Fabric installer list: {}", e))?;

    let installer_version = metas.iter()
        .find(|m| m.stable)
        .or_else(|| metas.first())
        .map(|m| m.version.clone())
        .ok_or_else(|| "No Fabric installer versions found".to_string())?;

    let url = format!(
        "https://maven.fabricmc.net/net/fabricmc/fabric-installer/{v}/fabric-installer-{v}.jar",
        v = installer_version
    );

    let msg = format!("Downloading Fabric installer {}…", installer_version);
    let _ = state.log_tx.send(make_log(msg));

    let bytes = state.http_client
        .get(&url)
        .header("User-Agent", "msm/0.1")
        .send().await
        .map_err(|e| format!("Fabric download failed: {}", e))?;

    if !bytes.status().is_success() {
        return Err(format!("Fabric download failed: HTTP {}", bytes.status()));
    }

    let bytes = bytes.bytes().await.map_err(|e| format!("Read error: {}", e))?;
    let installer_path = work_dir.join(format!("fabric-installer-{}.jar", installer_version));
    tokio::fs::write(&installer_path, &bytes[..]).await
        .map_err(|e| format!("Failed to write Fabric installer: {}", e))?;

    let _ = state.log_tx.send(make_log("Running Fabric installer…".to_string()));

    let log_tx = state.log_tx.clone();
    let inst_path = installer_path.clone();
    let work_clone = work_dir.to_path_buf();
    let mc = mc_version.to_string();
    let loader = loader_version.to_string();
    let make_log_clone = make_log.clone();

    let result = tokio::task::spawn_blocking(move || {
        run_fabric_blocking(&inst_path, &work_clone, &mc, &loader, &log_tx, make_log_clone)
    }).await;

    let _ = tokio::fs::remove_file(&installer_path).await;

    match result {
        Ok(r) => r?,
        Err(_) => return Err("Fabric installer task panicked".to_string()),
    }

    // Create run.sh so MSM can detect the loader
    let run_sh = work_dir.join("run.sh");
    if !run_sh.exists() {
        tokio::fs::write(&run_sh, "#!/usr/bin/env bash\nexec java -jar fabric-server-launch.jar \"$@\"\n").await
            .map_err(|e| format!("Failed to create run.sh: {}", e))?;
    }

    Ok(())
}

fn run_fabric_blocking(
    installer: &Path,
    work_dir: &Path,
    mc_version: &str,
    loader_version: &str,
    log_tx: &tokio::sync::broadcast::Sender<WsEvent>,
    make_log: Arc<dyn Fn(String) -> WsEvent + Send + Sync + 'static>,
) -> Result<(), String> {
    use std::io::BufRead;

    let mut child = std::process::Command::new("java")
        .args([
            "-jar", installer.to_str().unwrap_or("installer.jar"),
            "server",
            "-mcversion", mc_version,
            "-loader", loader_version,
            "-downloadMinecraft",
        ])
        .current_dir(work_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to launch java: {}. Is Java installed?", e))?;

    let tx_out = log_tx.clone();
    let tx_err = log_tx.clone();
    let log_out = make_log.clone();
    let log_err = make_log.clone();

    if let Some(stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines().flatten() {
                let _ = tx_out.send(log_out(line));
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stderr).lines().flatten() {
                let _ = tx_err.send(log_err(line));
            }
        });
    }

    let status = child.wait().map_err(|e| format!("Fabric installer wait failed: {}", e))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("Fabric installer exited with code {}", status.code().unwrap_or(-1)))
    }
}

// ── mrpack helpers ─────────────────────────────────────────────────────────────

fn parse_mrpack_index(bytes: &[u8]) -> Result<PackIndex, String> {
    let cursor = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor)
        .map_err(|e| format!("Failed to open mrpack archive: {}", e))?;

    let mut index_file = archive.by_name("modrinth.index.json")
        .map_err(|_| "modrinth.index.json not found in mrpack".to_string())?;

    let mut contents = String::new();
    index_file.read_to_string(&mut contents)
        .map_err(|e| format!("Failed to read manifest: {}", e))?;

    serde_json::from_str(&contents)
        .map_err(|e| format!("Failed to parse manifest: {}", e))
}

fn extract_overrides(bytes: &[u8], dest: &Path) -> Result<(), String> {
    let cursor = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor)
        .map_err(|e| format!("Failed to open mrpack archive: {}", e))?;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)
            .map_err(|e| format!("ZIP read error: {}", e))?;

        let raw = entry.name().to_string();

        let relative = if let Some(s) = raw.strip_prefix("overrides/") {
            s
        } else if let Some(s) = raw.strip_prefix("server-overrides/") {
            s
        } else {
            continue;
        };

        if relative.is_empty() { continue; }

        let out_path = dest.join(relative);
        if !out_path.starts_with(dest) {
            return Err(format!("Unsafe path in archive: {}", raw));
        }

        if entry.is_dir() {
            std::fs::create_dir_all(&out_path)
                .map_err(|e| format!("mkdir {:?}: {}", out_path, e))?;
        } else {
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("mkdir {:?}: {}", parent, e))?;
            }
            let mut out = std::fs::File::create(&out_path)
                .map_err(|e| format!("create {:?}: {}", out_path, e))?;
            std::io::copy(&mut entry, &mut out)
                .map_err(|e| format!("write {:?}: {}", out_path, e))?;
        }
    }

    Ok(())
}

fn detect_loader(deps: &HashMap<String, String>) -> (Option<String>, Option<String>) {
    for (key, loader_name) in [
        ("neoforge", "neoforge"),
        ("forge", "forge"),
        ("fabric-loader", "fabric"),
        ("quilt-loader", "quilt"),
    ] {
        if let Some(ver) = deps.get(key) {
            return (Some(loader_name.to_string()), Some(ver.clone()));
        }
    }
    (None, None)
}

fn slugify(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}
