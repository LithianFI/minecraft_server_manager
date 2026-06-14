use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::broadcast;

use crate::state::{AppState, InstanceStatus, WsEvent};

// ── Shared installer core ──────────────────────────────────────────────────────

async fn download_and_run_installer(
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

fn run_installer_blocking(
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

// ── Initial server setup ───────────────────────────────────────────────────────

pub async fn install_neoforge(state: Arc<AppState>, version: String, server_path: String) {
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

    match download_and_run_installer(&state, &version, &path, make_log).await {
        Ok(()) => {
            let _ = state.log_tx.send(WsEvent::SetupLog {
                message: "Installation complete.".to_string(),
            });
            let _ = state.log_tx.send(WsEvent::SetupDone { server_path });
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
    neoforge_version: String,
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
    let (server_path, instance_dir) = {
        let instances = state.instances.read().await;
        let inst = match instances.get(&instance_id) {
            Some(i) => i,
            None => { send_fail(format!("Instance '{}' not found", instance_id)); return; }
        };
        if !matches!(inst.status, InstanceStatus::Stopped | InstanceStatus::Crashed) {
            send_fail("Server must be stopped before updating.".to_string());
            return;
        }
        (inst.config.server.path.clone(), inst.instance_dir.clone())
    };

    let id_clone = instance_id.clone();
    let make_log: Arc<dyn Fn(String) -> WsEvent + Send + Sync + 'static> =
        Arc::new(move |msg| WsEvent::UpdateLog { instance_id: id_clone.clone(), message: msg });

    send_log(format!("Updating NeoForge to {}…", neoforge_version));

    match download_and_run_installer(&state, &neoforge_version, &server_path, make_log).await {
        Err(e) => { send_fail(e); return; }
        Ok(()) => {}
    }

    // Derive new MC version from NeoForge version string
    let mc_version = neoforge_to_mc(&neoforge_version);

    // Update in-memory state and msm.toml
    {
        let mut instances = state.instances.write().await;
        if let Some(inst) = instances.get_mut(&instance_id) {
            inst.config.instance.minecraft_version = mc_version.clone();
            inst.config.instance.loader_version = Some(neoforge_version.clone());

            if let Ok(toml_str) = toml::to_string_pretty(&inst.config) {
                let toml_path = instance_dir.join("msm.toml");
                let _ = std::fs::write(toml_path, toml_str);
            }
        }
    }

    send_log(format!("Updated to NeoForge {} (MC {}).", neoforge_version, mc_version));
    let _ = state.log_tx.send(WsEvent::UpdateDone {
        instance_id,
        minecraft_version: mc_version,
    });
}

fn neoforge_to_mc(nf: &str) -> String {
    let parts: Vec<&str> = nf.split('.').collect();
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
