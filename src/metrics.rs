use std::sync::Arc;
use std::time::Duration;

use crate::state::{AppState, InstanceStatus, WsEvent};

// ── RAM polling ───────────────────────────────────────────────────────────────

pub async fn run_ram_task(state: Arc<AppState>, instance_id: String, pid: u32) {
    let mut iter: u32 = 0;
    let mut prev_cpu: Option<(u64, u64)> = None; // (process_ticks, total_ticks)
    loop {
        tokio::time::sleep(Duration::from_secs(10)).await;
        iter += 1;

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

        let cpu_pct = {
            let cur = read_proc_cpu(pid);
            let pct = match (prev_cpu, cur) {
                (Some((pp, pt)), Some((cp, ct))) if ct > pt => {
                    let d_proc = cp.saturating_sub(pp) as f32;
                    let d_total = (ct - pt) as f32;
                    if d_total > 0.0 { Some((d_proc / d_total * 100.0).min(100.0)) } else { None }
                }
                _ => None,
            };
            prev_cpu = cur;
            pct
        };

        let (tps, loader, player_count, ram_alert_msg) = {
            let mut instances = state.instances.write().await;
            if let Some(inst) = instances.get_mut(&instance_id) {
                inst.ram_mb = Some(ram_mb);
                inst.cpu_pct = cpu_pct;
                let ram_alert = if let Some(a) = &inst.config.alerts {
                    if a.enabled && a.max_ram_mb > 0 {
                        let pct = ram_mb * 100 / a.max_ram_mb;
                        if pct >= a.ram_pct_max as u64 && !inst.high_ram_alerted {
                            inst.high_ram_alerted = true;
                            Some(format!("RAM at {}% ({}/{} MB)", pct, ram_mb, a.max_ram_mb))
                        } else {
                            if pct < a.ram_pct_max as u64 { inst.high_ram_alerted = false; }
                            None
                        }
                    } else { None }
                } else { None };
                (
                    inst.tps,
                    inst.config.instance.loader.clone().unwrap_or_default(),
                    inst.players.len(),
                    ram_alert,
                )
            } else {
                (None, String::new(), 0, None)
            }
        };

        let _ = state.log_tx.send(WsEvent::Metrics {
            instance_id: instance_id.clone(),
            ram_mb,
            tps,
            cpu_pct,
        });

        if let Some(msg) = ram_alert_msg {
            let _ = state.log_tx.send(crate::state::WsEvent::HealthAlert {
                instance_id: instance_id.clone(),
                kind: "ram".to_string(),
                message: msg,
            });
        }

        // For non-Forge/NeoForge servers write RAM to DB every 60 s (every 6th iteration).
        // Forge/NeoForge servers write their own rows from TPS parsing in instance.rs.
        if iter % 6 == 0 && loader != "neoforge" && loader != "forge" {
            state.metrics_db.record_metric(
                &instance_id,
                chrono::Utc::now().timestamp(),
                Some(ram_mb),
                None,
                player_count,
                cpu_pct,
            );
        }
    }
}

// Returns (process_ticks, total_cpu_ticks) for delta CPU% calculation.
fn read_proc_cpu(pid: u32) -> Option<(u64, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    let fields: Vec<&str> = stat.split_whitespace().collect();
    // utime is field 14 (0-indexed: 13), stime is field 15 (0-indexed: 14)
    let utime: u64 = fields.get(13)?.parse().ok()?;
    let stime: u64 = fields.get(14)?.parse().ok()?;
    let proc_ticks = utime + stime;

    let cpu_stat = std::fs::read_to_string("/proc/stat").ok()?;
    let first_line = cpu_stat.lines().next()?;
    let total: u64 = first_line
        .split_whitespace()
        .skip(1)
        .filter_map(|v| v.parse::<u64>().ok())
        .sum();

    Some((proc_ticks, total))
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
