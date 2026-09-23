use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::state::{AppState, InstanceStatus, WsEvent};

// ── Shared installer core ──────────────────────────────────────────────────────

pub(crate) async fn download_and_run_installer(
    state: &Arc<AppState>,
    version: &str,
    work_dir: &Path,
    make_log: Arc<dyn Fn(String) -> WsEvent + Send + Sync + 'static>,
) -> Result<(), String> {
    let url = format!(
        "https://maven.neoforged.net/releases/net/neoforged/neoforge/{v}/neoforge-{v}-installer.jar",
        v = version
    );

    let log = make_log.clone();
    let _ = state.log_tx.send(log(format!("Downloading NeoForge {} installer…", version)));

    let bytes = match state
        .http_client
        .get(&url)
        .header("User-Agent", "msm/0.1")
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => match r.bytes().await {
            Ok(b) => b,
            Err(e) => return Err(format!("Download read error: {}", e)),
        },
        Ok(r) => return Err(format!("Download failed: HTTP {}", r.status())),
        Err(e) => return Err(format!("Download failed: {}", e)),
    };

    let installer_path = work_dir.join(format!("neoforge-{}-installer.jar", version));
    tokio::fs::write(&installer_path, &bytes[..])
        .await
        .map_err(|e| format!("Failed to write installer: {}", e))?;

    let _ = state.log_tx.send(make_log("Running installer — this may take several minutes…".to_string()));

    let log_tx   = state.log_tx.clone();
    let inst_path = installer_path.clone();
    let work_clone = work_dir.to_path_buf();

    let result = tokio::task::spawn_blocking(move || {
        run_installer_blocking(&inst_path, &work_clone, &log_tx, make_log)
    })
    .await;

    let _ = tokio::fs::remove_file(&installer_path).await;

    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("Installer task panicked.".to_string()),
    }
}

pub(crate) fn run_installer_blocking(
    installer: &Path,
    work_dir: &Path,
    log_tx: &broadcast::Sender<WsEvent>,
    make_log: Arc<dyn Fn(String) -> WsEvent + Send + Sync + 'static>,
) -> Result<(), String> {
    let mut child = std::process::Command::new("java")
        .arg("-jar")
        .arg(installer)
        .arg("--installServer")
        .current_dir(work_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to launch java: {}. Is Java installed?", e))?;

    let tx_out  = log_tx.clone();
    let tx_err  = log_tx.clone();
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

    let status = child.wait().map_err(|e| format!("Installer wait failed: {}", e))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("Installer exited with code {}", status.code().unwrap_or(-1)))
    }
}

// ── Available versions ─────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct McVersionEntry {
    pub minecraft: String,
    /// Loader versions for this Minecraft version, newest first.
    pub loaders: Vec<String>,
}

/// List Minecraft versions (newest first) with their available loader versions.
pub async fn list_versions(client: &reqwest::Client, loader: &str) -> Result<Vec<McVersionEntry>, String> {
    match loader {
        "neoforge" => neoforge_versions(client).await,
        "forge"    => forge_versions(client).await,
        "fabric"   => fabric_versions(client).await,
        other      => Err(format!("Unknown loader '{}'", other)),
    }
}

async fn get_json<T: DeserializeOwned>(client: &reqwest::Client, url: &str) -> Result<T, String> {
    client
        .get(url)
        .header("User-Agent", "msm/0.1")
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("Failed to fetch {}: {}", url, e))?
        .json()
        .await
        .map_err(|e| format!("Failed to parse {}: {}", url, e))
}

fn push_grouped(out: &mut Vec<McVersionEntry>, minecraft: String, loader_version: String) {
    match out.iter_mut().find(|e| e.minecraft == minecraft) {
        Some(e) => e.loaders.push(loader_version),
        None => out.push(McVersionEntry { minecraft, loaders: vec![loader_version] }),
    }
}

async fn neoforge_versions(client: &reqwest::Client) -> Result<Vec<McVersionEntry>, String> {
    #[derive(Deserialize)]
    struct Resp { versions: Vec<String> }

    let resp: Resp = get_json(
        client,
        "https://maven.neoforged.net/api/maven/versions/releases/net/neoforged/neoforge",
    ).await?;

    let mut out = Vec::new();
    // Listed oldest first; skip snapshot/alpha builds and April-fools versions (e.g. "0.25w14craftmine")
    for v in resp.versions.into_iter().rev() {
        let major_ok = v.split('.').next().and_then(|s| s.parse::<u32>().ok()).is_some_and(|m| m >= 20);
        if !major_ok || v.contains("alpha") || v.contains('+') { continue; }
        push_grouped(&mut out, neoforge_to_mc(&v), v);
    }
    Ok(out)
}

async fn forge_versions(client: &reqwest::Client) -> Result<Vec<McVersionEntry>, String> {
    let meta: std::collections::HashMap<String, Vec<String>> = get_json(
        client,
        "https://files.minecraftforge.net/net/minecraftforge/forge/maven-metadata.json",
    ).await?;

    let mut out: Vec<McVersionEntry> = meta
        .into_iter()
        // Forge only generates run.sh (which MSM launches) from 1.17 onwards
        .filter(|(mc, _)| version_key(mc) >= vec![1, 17])
        .map(|(mc, versions)| {
            let prefix = format!("{}-", mc);
            let loaders = versions.iter().rev()
                .map(|v| v.strip_prefix(&prefix).unwrap_or(v).to_string())
                .collect();
            McVersionEntry { minecraft: mc, loaders }
        })
        .collect();
    out.sort_by(|a, b| version_key(&b.minecraft).cmp(&version_key(&a.minecraft)));
    Ok(out)
}

async fn fabric_versions(client: &reqwest::Client) -> Result<Vec<McVersionEntry>, String> {
    #[derive(Deserialize)]
    struct Entry { version: String, stable: bool }

    let games: Vec<Entry> = get_json(client, "https://meta.fabricmc.net/v2/versions/game").await?;
    let loaders: Vec<Entry> = get_json(client, "https://meta.fabricmc.net/v2/versions/loader").await?;
    let loaders: Vec<String> = loaders.into_iter().map(|l| l.version).collect();

    // Both lists are already newest first; any loader works with any game version
    Ok(games
        .into_iter()
        .filter(|g| g.stable)
        .map(|g| McVersionEntry { minecraft: g.version, loaders: loaders.clone() })
        .collect())
}

/// Numeric components of a version string, for ordering ("1.20.1" → [1, 20, 1]).
pub(crate) fn version_key(v: &str) -> Vec<u32> {
    v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

// ── Initial server setup ───────────────────────────────────────────────────────

pub(crate) fn loader_name(loader: &str) -> &str {
    match loader {
        "neoforge" => "NeoForge",
        "forge"    => "Forge",
        "fabric"   => "Fabric",
        other      => other,
    }
}

/// Downloads and runs the installer for `loader` in `server_path`.
pub(crate) async fn run_loader_installer(
    state: &Arc<AppState>,
    loader: &str,
    minecraft_version: &str,
    loader_version: &str,
    server_path: &Path,
    make_log: Arc<dyn Fn(String) -> WsEvent + Send + Sync + 'static>,
) -> Result<(), String> {
    match loader {
        "neoforge" => download_and_run_installer(state, loader_version, server_path, make_log).await,
        "forge" => crate::modpack::install_forge(state, minecraft_version, loader_version, server_path, make_log).await,
        "fabric" => crate::modpack::install_fabric(state, minecraft_version, loader_version, server_path, make_log).await,
        other => Err(format!("Version updates aren't supported for loader '{}'", other)),
    }
}

pub async fn install_server(
    state: Arc<AppState>,
    loader: String,
    minecraft_version: String,
    loader_version: String,
    server_path: String,
) {
    let path = PathBuf::from(&server_path);

    let _ = state.log_tx.send(WsEvent::SetupLog {
        message: format!("Creating directory {}…", path.display()),
    });
    if let Err(e) = tokio::fs::create_dir_all(&path).await {
        let _ = state.log_tx.send(WsEvent::SetupFailed {
            error: format!("Failed to create directory: {}", e),
        });
        return;
    }

    let make_log: Arc<dyn Fn(String) -> WsEvent + Send + Sync + 'static> =
        Arc::new(|msg| WsEvent::SetupLog { message: msg });

    match run_loader_installer(&state, &loader, &minecraft_version, &loader_version, &path, make_log).await {
        Ok(()) => {
            let _ = state.log_tx.send(WsEvent::SetupLog {
                message: "Installation complete.".to_string(),
            });
            let java_path = crate::java::pick_for_minecraft(&minecraft_version, |msg| {
                let _ = state.log_tx.send(WsEvent::SetupLog { message: msg.to_string() });
            })
            .await;
            let _ = state.log_tx.send(WsEvent::SetupDone { server_path, java_path });
        }
        Err(e) => {
            let _ = state.log_tx.send(WsEvent::SetupFailed { error: e });
        }
    }
}

// ── In-place version update ────────────────────────────────────────────────────

pub async fn update_server_version(
    state: Arc<AppState>,
    instance_id: String,
    minecraft_version: String,
    loader_version: String,
) {
    let send_log = |msg: String| {
        let _ = state.log_tx.send(WsEvent::UpdateLog {
            instance_id: instance_id.clone(),
            message: msg,
        });
    };
    let send_fail = |error: String| {
        let _ = state.log_tx.send(WsEvent::UpdateFailed {
            instance_id: instance_id.clone(),
            error,
        });
    };

    // Validate: must be stopped
    let (server_path, instance_dir, loader, old_mc) = {
        let instances = state.instances.read().await;
        let inst = match instances.get(&instance_id) {
            Some(i) => i,
            None => { send_fail(format!("Instance '{}' not found", instance_id)); return; }
        };
        if !matches!(inst.status, InstanceStatus::Stopped | InstanceStatus::Crashed) {
            send_fail("Server must be stopped before updating.".to_string());
            return;
        }
        (
            inst.config.server.path.clone(),
            inst.instance_dir.clone(),
            inst.config.instance.loader.clone().unwrap_or_default(),
            inst.config.instance.minecraft_version.clone(),
        )
    };

    if version_key(&minecraft_version) < version_key(&old_mc) {
        send_fail(format!("Minecraft can't be downgraded ({} → {}): worlds don't load in older versions.", old_mc, minecraft_version));
        return;
    }

    // A Minecraft upgrade converts the world and can't be undone
    let mc_changed = minecraft_version != old_mc;
    if mc_changed {
        send_log(format!("Minecraft {} → {} can't be undone — backing up first…", old_mc, minecraft_version));
        match crate::backup::do_create_backup(&state, &instance_id).await {
            Ok(info) => send_log(format!("Backup saved: {}", info.filename)),
            Err(e) => { send_fail(format!("Backup failed, update cancelled: {}", e)); return; }
        }
    }

    let id_clone = instance_id.clone();
    let make_log: Arc<dyn Fn(String) -> WsEvent + Send + Sync + 'static> =
        Arc::new(move |msg| WsEvent::UpdateLog { instance_id: id_clone.clone(), message: msg });

    send_log(format!("Installing {} {}…", loader_name(&loader), loader_version));
    if let Err(e) = run_loader_installer(&state, &loader, &minecraft_version, &loader_version, &server_path, make_log).await {
        send_fail(e);
        return;
    }

    // Newer Minecraft versions can need a newer Java
    let new_java = if crate::java::recommended_java(&minecraft_version) != crate::java::recommended_java(&old_mc) {
        Some(crate::java::pick_for_minecraft(&minecraft_version, |msg| send_log(msg.to_string())).await)
    } else {
        None
    };

    // Update in-memory state and msm.toml
    {
        let mut instances = state.instances.write().await;
        if let Some(inst) = instances.get_mut(&instance_id) {
            inst.config.instance.minecraft_version = minecraft_version.clone();
            inst.config.instance.loader_version = Some(loader_version.clone());
            if let Some(java_path) = new_java {
                inst.config.server.java_path = java_path;
            }

            if let Ok(toml_str) = toml::to_string_pretty(&inst.config) {
                let toml_path = instance_dir.join("msm.toml");
                let _ = std::fs::write(toml_path, toml_str);
            }
        }
    }

    send_log(format!("Updated to {} {} (MC {}).", loader_name(&loader), loader_version, minecraft_version));
    let _ = state.log_tx.send(WsEvent::UpdateDone {
        instance_id,
        minecraft_version,
        loader_version,
    });
}

fn neoforge_to_mc(nf: &str) -> String {
    let parts: Vec<&str> = nf.split('.').collect();
    // Year-based versions: "26.1.2.x" → MC 26.1.2, "26.2.0.x" → MC 26.2
    if parts.len() >= 4 && parts[0].parse::<u32>().is_ok_and(|m| m >= 26) {
        return if parts[2] == "0" {
            format!("{}.{}", parts[0], parts[1])
        } else {
            format!("{}.{}.{}", parts[0], parts[1], parts[2])
        };
    }
    if parts.len() >= 2 {
        let minor: u32 = parts[1].parse().unwrap_or(1);
        if minor == 0 {
            format!("1.{}", parts[0])
        } else {
            format!("1.{}.{}", parts[0], parts[1])
        }
    } else {
        nf.to_string()
    }
}
