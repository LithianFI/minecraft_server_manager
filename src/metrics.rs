use std::sync::Arc;
use std::time::Duration;

use crate::state::{AppState, InstanceStatus, WsEvent};

// ── RAM polling ───────────────────────────────────────────────────────────────

pub async fn run_ram_task(state: Arc<AppState>, instance_id: String, pid: u32) {
    loop {
        tokio::time::sleep(Duration::from_secs(10)).await;

        let is_active = {
            let instances = state.instances.read().await;
            instances
                .get(&instance_id)
                .map(|i| matches!(i.status, InstanceStatus::Running | InstanceStatus::Starting))
                .unwrap_or(false)
        };
        if !is_active {
            break;
        }

        let Some(ram_mb) = read_proc_ram(pid) else { continue };

        let tps = {
            let mut instances = state.instances.write().await;
            if let Some(inst) = instances.get_mut(&instance_id) {
                inst.ram_mb = Some(ram_mb);
                inst.tps
            } else {
                None
            }
        };

        let _ = state.log_tx.send(WsEvent::Metrics {
            instance_id: instance_id.clone(),
            ram_mb,
            tps,
        });
    }
}

fn read_proc_ram(pid: u32) -> Option<u64> {
    let content = std::fs::read_to_string(format!("/proc/{}/status", pid)).ok()?;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb / 1024);
        }
    }
    None
}

// ── TPS polling ───────────────────────────────────────────────────────────────

pub async fn run_tps_task(state: Arc<AppState>, instance_id: String) {
    // Wait for the server to finish starting before first poll
    tokio::time::sleep(Duration::from_secs(60)).await;

    loop {
        let is_running = {
            let instances = state.instances.read().await;
            instances
                .get(&instance_id)
                .map(|i| i.status == InstanceStatus::Running)
                .unwrap_or(false)
        };
        if !is_running {
            break;
        }

        {
            let loader = {
                let instances = state.instances.read().await;
                instances.get(&instance_id)
                    .and_then(|i| i.config.instance.loader.clone())
                    .unwrap_or_default()
            };
            // forge tps is only available on Forge/NeoForge
            if loader == "neoforge" || loader == "forge" {
                let processes = state.processes.lock().await;
                if let Some(handle) = processes.get(&instance_id) {
                    let _ = handle.stdin_tx.send("forge tps".to_string());
                }
            }
        }

        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

// ── TPS line parser (called from instance.rs log processing) ─────────────────

pub fn parse_tps(line: &str) -> Option<f32> {
    let pos = line.find("TPS: ")?;
    let rest = &line[pos + 5..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}
