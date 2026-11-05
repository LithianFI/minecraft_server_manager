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

/// Rescans the server's mods and checks which have a version for `minecraft_version`.
pub async fn plan_mods_for_version(
    state: &AppState,
    instance_id: &str,
    minecraft_version: &str,
) -> Result<crate::mod_mgr::VersionChangePlan, String> {
    let (server_path, instance_dir, loader) = {
        let instances = state.instances.read().await;
        let inst = instances.get(instance_id).ok_or_else(|| format!("Instance '{}' not found", instance_id))?;
        (
            inst.config.server.path.clone(),
            inst.instance_dir.clone(),
            inst.config.instance.loader.clone().unwrap_or_default(),
        )
    };
    if !server_path.join("mods").is_dir() {
        return Ok(Default::default());
    }
    // Rescan so mods added or removed by hand are included
    let lock = crate::mod_mgr::scan_mods(&state.http_client, &server_path, &instance_dir).await?;
    crate::mod_mgr::plan_version_change(&state.http_client, &lock, minecraft_version, &loader).await
}

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
    let (server_path, instance_dir, loader, old_mc, old_loader_version) = {
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
            inst.config.instance.loader_version.clone(),
        )
    };

    if version_key(&minecraft_version) < version_key(&old_mc) {
        send_fail(format!("Minecraft can't be downgraded ({} → {}): worlds don't load in older versions.", old_mc, minecraft_version));
        return;
    }

    // A new Minecraft version needs matching mods. Download them all before changing
    // anything, so a failed download leaves the server as it was.
    let mut mod_changes = None;
    if minecraft_version != old_mc {
        send_log(format!("Checking mods for Minecraft {}…", minecraft_version));
        let plan = match plan_mods_for_version(&state, &instance_id, &minecraft_version).await {
            Ok(plan) => plan,
            Err(e) => { send_fail(format!("Couldn't check mods, update cancelled: {}", e)); return; }
        };
        let mut downloads = Vec::new();
        let libraries = plan.dependencies.iter().map(|d| &d.file);
        for update in plan.updates.iter().chain(libraries) {
            send_log(format!("Downloading {} {}…", update.name, update.latest_version_number));
            match crate::mod_mgr::download_update(&state.http_client, update).await {
                Ok(bytes) => downloads.push((update.clone(), bytes)),
                Err(e) => { send_fail(format!("{}, update cancelled", e)); return; }
            }
        }
        mod_changes = Some((plan, downloads));
    }

    // Full backup first, so the update (loader files, and the world if Minecraft changes)
    // can be rolled back from the Backups tab
    send_log("Taking a full backup before changing anything…".to_string());
    let label = format!("Before updating to {} {} (MC {})", loader_name(&loader), loader_version, minecraft_version);
    match crate::backup::do_create_update_backup(&state, &instance_id, label).await {
        Ok(info) => send_log(format!("Backup saved: {} — roll back from the Backups tab if the update misbehaves", info.filename)),
        Err(e) => { send_fail(format!("Backup failed, update cancelled: {}", e)); return; }
    }

    let id_clone = instance_id.clone();
    let make_log: Arc<dyn Fn(String) -> WsEvent + Send + Sync + 'static> =
        Arc::new(move |msg| WsEvent::UpdateLog { instance_id: id_clone.clone(), message: msg });

    send_log(format!("Installing {} {}…", loader_name(&loader), loader_version));
    if let Err(e) = run_loader_installer(&state, &loader, &minecraft_version, &loader_version, &server_path, make_log.clone()).await {
        send_fail(e);
        return;
    }

    if let Some((plan, downloads)) = mod_changes {
        let mut lock = crate::mod_mgr::read_lock(&instance_dir);
        for (update, bytes) in &downloads {
            if let Err(e) = crate::mod_mgr::install_update(bytes, update, &server_path, &mut lock).await {
                let _ = crate::mod_mgr::write_lock(&instance_dir, &lock);
                send_fail(format!("{} — roll back from the Backups tab", e));
                return;
            }
            match plan.dependencies.iter().find(|d| d.file.project_id == update.project_id) {
                Some(d) => send_log(format!("Added {} {} (required by {})", update.name, update.latest_version_number, d.required_by)),
                None => send_log(format!("Updated {} to {}", update.name, update.latest_version_number)),
            }
        }
        if let Err(e) = crate::mod_mgr::write_lock(&instance_dir, &lock) {
            send_fail(e);
            return;
        }
        for missing in &plan.missing {
            let entry = &missing.entry;
            let reason = match &missing.needs {
                Some(library) => format!("it requires {}, which has no version for Minecraft {} yet", library, minecraft_version),
                None => format!("no version for Minecraft {} yet", minecraft_version),
            };
            match crate::mod_mgr::set_mod_enabled(&server_path, &instance_dir, &entry.filename, false) {
                Ok(_) => send_log(format!("⚠ Disabled {}: {}", entry.name, reason)),
                Err(e) => { send_fail(format!("{} — roll back from the Backups tab", e)); return; }
            }
        }
        for name in &plan.unknown {
            send_log(format!("⚠ {} isn't on Modrinth, so it was left as is — check it supports Minecraft {}", name, minecraft_version));
        }
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

    let loader_version = match test_start(
        &state, &instance_id, &loader, &minecraft_version, loader_version,
        old_loader_version.as_deref(), &server_path, &instance_dir, make_log,
    ).await {
        Ok(v) => v,
        Err(e) => { send_fail(e); return; }
    };

    send_log(format!("Updated to {} {} (MC {}).", loader_name(&loader), loader_version, minecraft_version));
    let _ = state.log_tx.send(WsEvent::UpdateDone {
        instance_id: instance_id.clone(),
        minecraft_version,
        loader_version,
    });
    crate::distribution::announce_update(&state, &instance_id).await;
}

/// How many older loader builds to try when the server doesn't start after an update.
const MAX_STEP_BACK: usize = 8;

/// Starts the server once after an update to check it boots. If it crashes, reinstalls
/// older builds of the loader for the same Minecraft version until one starts: beta loader
/// builds can break mods without any version range saying so. Returns the build installed.
#[allow(clippy::too_many_arguments)]
async fn test_start(
    state: &Arc<AppState>,
    instance_id: &str,
    loader: &str,
    minecraft_version: &str,
    loader_version: String,
    old_loader_version: Option<&str>,
    server_path: &Path,
    instance_dir: &Path,
    make_log: Arc<dyn Fn(String) -> WsEvent + Send + Sync + 'static>,
) -> Result<String, String> {
    set_test_starting(state, instance_id, true).await;
    let result = step_back_until_started(
        state, instance_id, loader, minecraft_version, loader_version,
        old_loader_version, server_path, instance_dir, make_log,
    ).await;
    set_test_starting(state, instance_id, false).await;
    result
}

#[allow(clippy::too_many_arguments)]
async fn step_back_until_started(
    state: &Arc<AppState>,
    instance_id: &str,
    loader: &str,
    minecraft_version: &str,
    loader_version: String,
    old_loader_version: Option<&str>,
    server_path: &Path,
    instance_dir: &Path,
    make_log: Arc<dyn Fn(String) -> WsEvent + Send + Sync + 'static>,
) -> Result<String, String> {
    let send_log = |msg: String| { let _ = state.log_tx.send(make_log(msg)); };
    let name = loader_name(loader);

    let fallbacks = match list_versions(&state.http_client, loader).await {
        Ok(versions) => step_back_candidates(&versions, minecraft_version, &loader_version, old_loader_version),
        Err(e) => {
            send_log(format!("⚠ Couldn't list older {} builds, so only {} will be tried: {}", name, loader_version, e));
            Vec::new()
        }
    };

    let requested = loader_version.clone();
    let mut current = loader_version;
    let mut tried = 0;
    loop {
        send_log(format!("Test-starting the server on {} {}…", name, current));
        if let Err(e) = crate::instance::start_instance(state.clone(), instance_id).await {
            send_log(format!("⚠ Skipped the test start: {}", e));
            return Ok(current);
        }
        match wait_for_boot(state, instance_id).await {
            Boot::Started => {
                send_log(format!("✓ Server started on {} {}. Stopping it again…", name, current));
                let _ = crate::instance::stop_instance(state.clone(), instance_id).await;
                if !crate::instance::wait_until_stopped(state, instance_id).await {
                    send_log("⚠ The server is taking a long time to stop.".to_string());
                }
                return Ok(current);
            }
            Boot::Stopped => {
                send_log("Test start interrupted: the server was stopped.".to_string());
                return Ok(current);
            }
            Boot::Crashed(summary) => {
                send_log(format!("✗ Crashed on {} {}: {}", name, current, describe_crash(summary.as_ref())));
                let Some(next) = fallbacks.get(tried) else {
                    let what = match fallbacks.last() {
                        Some(oldest) => format!("{} {} or the {} builds before it (down to {})", name, requested, fallbacks.len(), oldest),
                        None => format!("{} {}", name, requested),
                    };
                    return Err(format!(
                        "The server didn't start on {}. Roll back with the \"Before updating\" backup in the Backups tab.",
                        what
                    ));
                };
                tried += 1;
                send_log(format!("Trying the previous build, {} {} ({}/{})…", name, next, tried, fallbacks.len()));
                run_loader_installer(state, loader, minecraft_version, next, server_path, make_log.clone()).await?;
                save_loader_version(state, instance_id, instance_dir, next).await?;
                current = next.clone();
            }
        }
    }
}

/// Builds to fall back on when `requested` doesn't start, newest first: older builds for the
/// same Minecraft version, down to the one the server was on before the update.
fn step_back_candidates(
    versions: &[McVersionEntry],
    minecraft_version: &str,
    requested: &str,
    previous: Option<&str>,
) -> Vec<String> {
    let Some(entry) = versions.iter().find(|e| e.minecraft == minecraft_version) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for v in entry.loaders.iter().skip_while(|v| *v != requested).skip(1) {
        out.push(v.clone());
        if Some(v.as_str()) == previous || out.len() == MAX_STEP_BACK {
            break;
        }
    }
    out
}

enum Boot {
    Started,
    Crashed(Option<crate::crash::CrashSummary>),
    Stopped,
}

async fn wait_for_boot(state: &AppState, instance_id: &str) -> Boot {
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let instances = state.instances.read().await;
        let Some(inst) = instances.get(instance_id) else { return Boot::Stopped };
        match inst.status {
            InstanceStatus::Starting => {}
            InstanceStatus::Running => return Boot::Started,
            InstanceStatus::Crashed => return Boot::Crashed(inst.last_crash.clone()),
            InstanceStatus::Stopping | InstanceStatus::Stopped => return Boot::Stopped,
        }
    }
}

fn describe_crash(summary: Option<&crate::crash::CrashSummary>) -> String {
    let parts: Vec<&str> = summary
        .map(|s| [&s.description, &s.exception, &s.suspected_mods].into_iter().flatten().map(String::as_str).collect())
        .unwrap_or_default();
    if parts.is_empty() { "no details in the log".to_string() } else { parts.join(" · ") }
}

async fn set_test_starting(state: &AppState, instance_id: &str, on: bool) {
    if let Some(inst) = state.instances.write().await.get_mut(instance_id) {
        inst.test_starting = on;
    }
}

async fn save_loader_version(state: &AppState, instance_id: &str, instance_dir: &Path, version: &str) -> Result<(), String> {
    let mut instances = state.instances.write().await;
    let Some(inst) = instances.get_mut(instance_id) else { return Ok(()) };
    inst.config.instance.loader_version = Some(version.to_string());
    let toml_str = toml::to_string_pretty(&inst.config).map_err(|e| format!("Failed to serialize config: {}", e))?;
    std::fs::write(instance_dir.join("msm.toml"), toml_str).map_err(|e| format!("Failed to write msm.toml: {}", e))
}

pub(crate) fn neoforge_to_mc(nf: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn versions(mc: &str, builds: std::ops::RangeInclusive<u32>) -> Vec<McVersionEntry> {
        let loaders = builds.rev().map(|b| format!("26.3.0.{}-beta", b)).collect();
        vec![McVersionEntry { minecraft: mc.into(), loaders }]
    }

    #[test]
    fn steps_back_from_requested_build_capped() {
        let v = versions("26.3", 0..=48);
        let got = step_back_candidates(&v, "26.3", "26.3.0.48-beta", Some("26.1.0.19"));
        assert_eq!(got.len(), MAX_STEP_BACK);
        assert_eq!(got[0], "26.3.0.47-beta");
        assert_eq!(got[MAX_STEP_BACK - 1], "26.3.0.40-beta");
    }

    #[test]
    fn stops_at_previous_build() {
        let v = versions("26.3", 0..=48);
        let got = step_back_candidates(&v, "26.3", "26.3.0.48-beta", Some("26.3.0.45-beta"));
        assert_eq!(got, ["26.3.0.47-beta", "26.3.0.46-beta", "26.3.0.45-beta"]);
    }

    #[test]
    fn nothing_to_try_for_oldest_or_unknown_build() {
        let v = versions("26.3", 0..=48);
        assert!(step_back_candidates(&v, "26.3", "26.3.0.0-beta", None).is_empty());
        assert!(step_back_candidates(&v, "26.3", "26.3.0.99-beta", None).is_empty());
        assert!(step_back_candidates(&v, "26.4", "26.3.0.48-beta", None).is_empty());
    }
}
