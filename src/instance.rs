use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::metrics;
use crate::state::{AppState, InstanceStatus, LogLine, ProcessHandle, WsEvent};

pub async fn start_instance(state: Arc<AppState>, instance_id: &str) -> Result<(), String> {
    {
        let instances = state.instances.read().await;
        for inst in instances.values() {
            if matches!(inst.status, InstanceStatus::Starting | InstanceStatus::Running) {
                return Err(format!(
                    "Instance '{}' is already running. Stop it first.",
                    inst.config.instance.display_name.as_deref().unwrap_or(&inst.config.instance.name)
                ));
            }
        }
    }

    let (instance_dir, server_path, java_opts, java_path) = {
        let instances = state.instances.read().await;
        let inst = instances
            .get(instance_id)
            .ok_or_else(|| format!("Instance '{}' not found", instance_id))?;
        (
            inst.instance_dir.clone(),
            inst.config.server.path.clone(),
            inst.config.server.java_opts.clone(),
            inst.config.server.java_path.clone(),
        )
    };

    ensure_eula(&server_path).await?;

    set_status(&state, instance_id, InstanceStatus::Starting).await;

    {
        let mut instances = state.instances.write().await;
        if let Some(inst) = instances.get_mut(instance_id) {
            inst.started_at = Some(chrono::Utc::now());
            inst.players.clear();
            inst.log_buffer.clear();
        }
    }

    let id = instance_id.to_string();
    tokio::spawn(run_with_restart(state.clone(), id, server_path, java_opts, java_path));

    tracing::info!("Starting instance '{}'", instance_id);
    let _ = instance_dir;
    Ok(())
}

/// Outer loop that runs the instance and handles auto-restart after crashes.
/// Owns the stdin channel lifecycle so run_instance_task never needs to call start_instance.
async fn run_with_restart(
    state: Arc<AppState>,
    instance_id: String,
    server_path: PathBuf,
    java_opts: Option<String>,
    java_path: Option<String>,
) {
    loop {
        let (stdin_tx, stdin_rx) = mpsc::unbounded_channel();
        state
            .processes
            .lock()
            .await
            .insert(instance_id.clone(), ProcessHandle { stdin_tx });

        run_instance_task(
            state.clone(),
            instance_id.clone(),
            server_path.clone(),
            java_opts.clone(),
            java_path.clone(),
            stdin_rx,
        )
        .await;

        // Read final status + restart config
        let (final_status, auto_restart, max_attempts, delay_secs, current_attempts) = {
            let instances = state.instances.read().await;
            if let Some(inst) = instances.get(&instance_id) {
                let r = inst.config.restart.as_ref();
                (
                    inst.status.clone(),
                    r.map(|r| r.auto_restart).unwrap_or(false),
                    r.map(|r| r.max_attempts).unwrap_or(3),
                    r.map(|r| r.delay_secs).unwrap_or(10),
                    inst.restart_attempts,
                )
            } else {
                break;
            }
        };

        if final_status != InstanceStatus::Crashed || !auto_restart || current_attempts >= max_attempts {
            break;
        }

        let attempt = current_attempts + 1;
        {
            let mut instances = state.instances.write().await;
            if let Some(inst) = instances.get_mut(&instance_id) {
                inst.restart_attempts = attempt;
            }
        }
        let _ = state.log_tx.send(WsEvent::AutoRestarting {
            instance_id: instance_id.clone(),
            attempt,
            max_attempts,
        });
        tracing::info!("Auto-restarting '{}' (attempt {}/{})", instance_id, attempt, max_attempts);
        tokio::time::sleep(Duration::from_secs(delay_secs)).await;

        // Re-prepare state for the next run
        set_status(&state, &instance_id, InstanceStatus::Starting).await;
        {
            let mut instances = state.instances.write().await;
            if let Some(inst) = instances.get_mut(&instance_id) {
                inst.started_at = Some(chrono::Utc::now());
                inst.players.clear();
                inst.log_buffer.clear();
            }
        }
        let _ = ensure_eula(&server_path).await;
    }
}

async fn run_instance_task(
    state: Arc<AppState>,
    instance_id: String,
    server_path: PathBuf,
    java_opts: Option<String>,
    java_path: Option<String>,
    mut stdin_rx: mpsc::UnboundedReceiver<String>,
) {
    let run_sh = server_path.join("run.sh");

    let mut cmd = Command::new("bash");
    cmd.arg(&run_sh)
        .arg("nogui")
        .current_dir(&server_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(opts) = java_opts {
        cmd.env("JAVA_TOOL_OPTIONS", opts);
    }
    if let Some(ref java) = java_path {
        // Prepend the specified java's bin dir to PATH so run.sh picks it up
        let java_p = std::path::Path::new(java);
        if let Some(bin_dir) = java_p.parent() {
            let current_path = std::env::var("PATH").unwrap_or_default();
            cmd.env("PATH", format!("{}:{}", bin_dir.display(), current_path));
            // Also set JAVA_HOME to the JDK root (parent of bin/)
            if let Some(home) = bin_dir.parent() {
                cmd.env("JAVA_HOME", home);
            }
        }
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to start {}: {}", run_sh.display(), e);
            set_status(&state, &instance_id, InstanceStatus::Crashed).await;
            let mut processes = state.processes.lock().await;
            processes.remove(&instance_id);
            return;
        }
    };

    let pid = child.id().unwrap_or(0);
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let mut stdin = child.stdin.take().unwrap();

    let state_out = state.clone();
    let id_out = instance_id.clone();
    let stdout_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            process_log_line(&state_out, &id_out, &line).await;
        }
    });

    let state_err = state.clone();
    let id_err = instance_id.clone();
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            process_log_line(&state_err, &id_err, &line).await;
        }
    });

    let stdin_task = tokio::spawn(async move {
        while let Some(cmd) = stdin_rx.recv().await {
            let _ = stdin.write_all(format!("{}\n", cmd).as_bytes()).await;
        }
    });

    let ram_task = tokio::spawn(metrics::run_ram_task(state.clone(), instance_id.clone(), pid));
    let tps_task = tokio::spawn(metrics::run_tps_task(state.clone(), instance_id.clone()));

    let exit_status = child.wait().await;

    stdout_task.abort();
    stderr_task.abort();
    stdin_task.abort();
    ram_task.abort();
    tps_task.abort();

    {
        let mut processes = state.processes.lock().await;
        processes.remove(&instance_id);
    }

    let was_stopping = {
        let instances = state.instances.read().await;
        instances
            .get(&instance_id)
            .map(|i| i.status == InstanceStatus::Stopping)
            .unwrap_or(false)
    };

    let final_status = match exit_status {
        Ok(s) if s.success() || was_stopping => InstanceStatus::Stopped,
        _ => InstanceStatus::Crashed,
    };

    {
        let mut instances = state.instances.write().await;
        if let Some(inst) = instances.get_mut(&instance_id) {
            inst.status = final_status.clone();
            inst.players.clear();
            inst.started_at = None;
            inst.ram_mb = None;
            inst.tps = None;
        }
    }

    let _ = state.log_tx.send(WsEvent::StateChanged {
        instance_id: instance_id.clone(),
        status: final_status.clone(),
    });

    tracing::info!("Instance '{}' exited with status {:?}", instance_id, final_status);
}

async fn process_log_line(state: &AppState, instance_id: &str, line: &str) {
    let timestamp = chrono::Utc::now().timestamp();

    {
        let mut instances = state.instances.write().await;
        if let Some(inst) = instances.get_mut(instance_id) {
            if inst.log_buffer.len() >= 1000 {
                inst.log_buffer.pop_front();
            }
            inst.log_buffer.push_back(LogLine {
                line: line.to_string(),
                timestamp,
            });
        }
    }

    let _ = state.log_tx.send(WsEvent::LogLine {
        instance_id: instance_id.to_string(),
        line: line.to_string(),
        timestamp,
    });

    if line.contains("Done (") && line.contains("! For help, type") {
        set_status(state, instance_id, InstanceStatus::Running).await;
        let mut instances = state.instances.write().await;
        if let Some(inst) = instances.get_mut(instance_id) {
            inst.restart_attempts = 0;
        }
    }

    if let Some(tps) = metrics::parse_tps(line) {
        let ram_mb = {
            let mut instances = state.instances.write().await;
            if let Some(inst) = instances.get_mut(instance_id) {
                inst.tps = Some(tps);
                inst.ram_mb.unwrap_or(0)
            } else {
                0
            }
        };
        let _ = state.log_tx.send(WsEvent::Metrics {
            instance_id: instance_id.to_string(),
            ram_mb,
            tps: Some(tps),
        });
    }

    if let Some(player) = parse_player_event(line, "joined the game") {
        {
            let mut instances = state.instances.write().await;
            if let Some(inst) = instances.get_mut(instance_id) {
                inst.players.insert(player.clone());
            }
        }
        let _ = state.log_tx.send(WsEvent::PlayerJoined {
            instance_id: instance_id.to_string(),
            player,
        });
    } else if let Some(player) = parse_player_event(line, "left the game") {
        {
            let mut instances = state.instances.write().await;
            if let Some(inst) = instances.get_mut(instance_id) {
                inst.players.remove(&player);
            }
        }
        let _ = state.log_tx.send(WsEvent::PlayerLeft {
            instance_id: instance_id.to_string(),
            player,
        });
    }
}

fn parse_player_event(line: &str, suffix: &str) -> Option<String> {
    // Log format: "[HH:MM:SS] [Server thread/INFO]: PlayerName joined the game"
    if !line.contains(suffix) {
        return None;
    }
    let msg = line.splitn(2, "]: ").nth(1)?;
    let name = msg.strip_suffix(&format!(" {}", suffix))?;
    // Basic sanity check: player names are 3-16 alphanumeric/underscore chars
    if name.len() >= 2 && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        Some(name.to_string())
    } else {
        None
    }
}

async fn set_status(state: &AppState, instance_id: &str, status: InstanceStatus) {
    {
        let mut instances = state.instances.write().await;
        if let Some(inst) = instances.get_mut(instance_id) {
            inst.status = status.clone();
        }
    }
    let _ = state.log_tx.send(WsEvent::StateChanged {
        instance_id: instance_id.to_string(),
        status,
    });
}

pub async fn stop_instance(state: Arc<AppState>, instance_id: &str) -> Result<(), String> {
    {
        let instances = state.instances.read().await;
        let inst = instances
            .get(instance_id)
            .ok_or_else(|| format!("Instance '{}' not found", instance_id))?;
        if !matches!(inst.status, InstanceStatus::Running | InstanceStatus::Starting) {
            return Err(format!(
                "Instance '{}' is not running",
                inst.config.instance.name
            ));
        }
    }

    set_status(&state, instance_id, InstanceStatus::Stopping).await;

    {
        let processes = state.processes.lock().await;
        if let Some(handle) = processes.get(instance_id) {
            let _ = handle.stdin_tx.send("stop".to_string());
        }
    }

    Ok(())
}

pub async fn switch_instance(state: Arc<AppState>, target_id: &str) -> Result<(), String> {
    {
        let instances = state.instances.read().await;
        if instances.get(target_id).is_none() {
            return Err(format!("Instance '{}' not found", target_id));
        }
    }

    let running_id = {
        let instances = state.instances.read().await;
        instances
            .iter()
            .find(|(_, inst)| {
                matches!(inst.status, InstanceStatus::Running | InstanceStatus::Starting)
            })
            .map(|(id, _)| id.clone())
    };

    if let Some(running_id) = running_id {
        if running_id == target_id {
            return Err(format!("Instance '{}' is already running", target_id));
        }

        stop_instance(state.clone(), &running_id).await?;

        let stop_result = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                {
                    let instances = state.instances.read().await;
                    if instances
                        .get(&running_id)
                        .map(|i| i.status == InstanceStatus::Stopped)
                        .unwrap_or(true)
                    {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        })
        .await;

        if stop_result.is_err() {
            return Err("Timed out waiting for current instance to stop".to_string());
        }
    }

    start_instance(state, target_id).await
}

pub async fn send_command(
    state: Arc<AppState>,
    instance_id: &str,
    command: String,
) -> Result<(), String> {
    let processes = state.processes.lock().await;
    match processes.get(instance_id) {
        Some(handle) => handle
            .stdin_tx
            .send(command)
            .map_err(|_| "Failed to send command — is the server still running?".to_string()),
        None => Err(format!("Instance '{}' is not running", instance_id)),
    }
}

async fn ensure_eula(server_path: &PathBuf) -> Result<(), String> {
    let eula_path = server_path.join("eula.txt");
    let accepted = tokio::fs::read_to_string(&eula_path)
        .await
        .map(|c| c.contains("eula=true"))
        .unwrap_or(false);

    if !accepted {
        tokio::fs::write(&eula_path, "eula=true\n")
            .await
            .map_err(|e| format!("Failed to write eula.txt: {}", e))?;
    }
    Ok(())
}
