use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;

use crate::state::{AppState, InstanceStatus, WsEvent};

// ── FTB API types ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct FtbVersionDetails {
    targets: Vec<FtbTarget>,
    files: Vec<FtbFile>,
}

#[derive(Deserialize)]
struct FtbTarget {
    name: String,
    version: String,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct FtbFile {
    #[serde(default)]
    id: u64,
    name: String,
    path: String,
    url: String,
    #[serde(default)]
    clientonly: bool,
}

// ── Public entry point ─────────────────────────────────────────────────────────

pub struct FtbImportRequest {
    pub pack_id: u64,
    pub version_id: u64,
    pub server_path: String,
    pub instance_name: String,
    pub port: u16,
}

pub async fn import_ftb(state: Arc<AppState>, req: FtbImportRequest) {
    let server_path_str = req.server_path.clone();
    match do_import(&state, req).await {
        Ok(()) => {
            let _ = state.log_tx.send(WsEvent::ModpackLog { message: "Done!".to_string() });
            let _ = state.log_tx.send(WsEvent::ModpackDone { server_path: server_path_str });
        }
        Err(e) => {
            let _ = state.log_tx.send(WsEvent::ModpackFailed { error: e });
        }
    }
}

async fn do_import(state: &Arc<AppState>, req: FtbImportRequest) -> Result<(), String> {
    let log = |msg: &str| {
        let _ = state.log_tx.send(WsEvent::ModpackLog { message: msg.to_string() });
    };

    log("Fetching modpack info from FTB…");

    let version: FtbVersionDetails = state.http_client
        .get(format!(
            "https://api.modpacks.ch/public/modpack/{}/{}",
            req.pack_id, req.version_id
        ))
        .header("User-Agent", "msm/0.1")
        .send().await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("FTB API error: {}", e))?
        .json().await
        .map_err(|e| format!("Failed to parse FTB version manifest: {}", e))?;

    // Extract game version and loader
    let mc_version = version.targets.iter()
        .find(|t| t.kind == "game" && t.name == "minecraft")
        .map(|t| t.version.clone())
        .ok_or_else(|| "No Minecraft version in FTB manifest".to_string())?;

    let loader_target = version.targets.iter().find(|t| t.kind == "modloader");
    let (loader_name, loader_version) = match loader_target {
        Some(t) => (Some(t.name.clone()), Some(t.version.clone())),
        None => (None, None),
    };

    log(&format!(
        "MC {} | {}{}",
        mc_version,
        loader_name.as_deref().unwrap_or("vanilla"),
        loader_version.as_deref().map(|v| format!(" {v}")).unwrap_or_default(),
    ));

    // Instance ID + collision check
    let instance_id = slugify(&req.instance_name);
    if instance_id.is_empty() {
        return Err("Instance name produces an empty ID".to_string());
    }
    if state.instances.read().await.contains_key(&instance_id) {
        return Err(format!("An instance named '{}' already exists", instance_id));
    }

    // Resolve server directory
    let server_path: PathBuf = if req.server_path.trim().is_empty() {
        crate::config::data_dir().join("servers").join(&instance_id)
    } else {
        PathBuf::from(&req.server_path)
    };

    log(&format!("Creating directory {}…", server_path.display()));
    tokio::fs::create_dir_all(&server_path).await
        .map_err(|e| format!("Failed to create directory: {}", e))?;

    // Install loader
    let make_log: Arc<dyn Fn(String) -> WsEvent + Send + Sync + 'static> =
        Arc::new(|msg| WsEvent::ModpackLog { message: msg });

    match loader_name.as_deref() {
        Some("neoforge") => {
            let ver = loader_version.as_deref().unwrap_or("").to_string();
            log(&format!("Installing NeoForge {}…", ver));
            crate::setup::download_and_run_installer(state, &ver, &server_path, make_log).await?;
        }
        Some("forge") => {
            let ver = loader_version.as_deref().unwrap_or("").to_string();
            log(&format!("Installing Forge {}…", ver));
            crate::modpack::install_forge(state, &mc_version, &ver, &server_path, make_log).await?;
        }
        Some("fabric") => {
            let ver = loader_version.as_deref().unwrap_or("").to_string();
            log(&format!("Installing Fabric {}…", ver));
            crate::modpack::install_fabric(state, &mc_version, &ver, &server_path, make_log).await?;
        }
        _ => {
            log("No known mod loader — skipping loader installation.");
        }
    }

    // Download files (skip client-only)
    let server_files: Vec<&FtbFile> = version.files.iter()
        .filter(|f| !f.clientonly)
        .collect();

    log(&format!("Downloading {} files…", server_files.len()));

    for (i, file) in server_files.iter().enumerate() {
        // Resolve destination: path field is like "mods", "config", etc.
        let clean = file.path.trim_matches('/').trim_start_matches('.').trim_matches('/');
        let dest = if clean.is_empty() {
            server_path.join(&file.name)
        } else {
            server_path.join(clean).join(&file.name)
        };

        // Path traversal guard
        if !dest.starts_with(&server_path) {
            return Err(format!("Unsafe path in manifest: {}/{}", file.path, file.name));
        }

        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await
                .map_err(|e| format!("Failed to create dir for {}: {}", file.name, e))?;
        }

        // Fall back to FTB file API if URL is absent
        let effective_url = if file.url.starts_with("http") {
            file.url.clone()
        } else {
            format!(
                "https://api.modpacks.ch/public/modpack/{}/{}/file/{}",
                req.pack_id, req.version_id, file.id
            )
        };

        let bytes = state.http_client
            .get(&effective_url)
            .header("User-Agent", "msm/0.1")
            .send().await
            .and_then(|r| r.error_for_status())
            .map_err(|e| format!("Failed to download {}: {}", file.name, e))?
            .bytes().await
            .map_err(|e| format!("Failed to read {}: {}", file.name, e))?;

        tokio::fs::write(&dest, &bytes[..]).await
            .map_err(|e| format!("Failed to write {}: {}", file.name, e))?;

        log(&format!("[{}/{}] {}/{}", i + 1, server_files.len(), file.path, file.name));
    }

    // Java version detection
    let required_java = crate::java::recommended_java(&mc_version);
    let java_path = tokio::task::spawn_blocking(move || crate::java::find_java(required_java))
        .await
        .unwrap_or(None);

    match &java_path {
        Some(p) => log(&format!(
            "Java {} required — found at {} — configuring instance.",
            required_java,
            p.display()
        )),
        None => {
            if crate::java::java_version("java") != Some(required_java) {
                log(&format!(
                    "Warning: Java {} required but not found. Set java_path manually in instance settings.",
                    required_java
                ));
            }
        }
    }

    // Register instance
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
            loader: loader_name,
            loader_version,
            port: req.port,
            modrinth_project_id: None,
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
    };

    let info = crate::state::InstanceInfo::from(&inst_state);
    state.instances.write().await.insert(instance_id, inst_state);
    let _ = state.log_tx.send(WsEvent::InstanceAdded { instance: info });

    Ok(())
}

fn slugify(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}
