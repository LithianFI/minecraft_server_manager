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

        let (tps, has_tps, player_count, ram_alert_msg) = {
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
                    tps_command(
                        inst.config.instance.loader.as_deref().unwrap_or_default(),
                        &inst.config.instance.minecraft_version,
                    ).is_some(),
                    inst.players.len(),
                    ram_alert,
                )
            } else {
                (None, false, 0, None)
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

        // Servers without a TPS command get a RAM-only DB row every 60 s (every 6th iteration);
        // the others write their rows from TPS parsing in instance.rs.
        if iter % 6 == 0 && !has_tps {
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

/// The console command that reports TPS for this server, if it has one.
pub fn tps_command(loader: &str, minecraft_version: &str) -> Option<&'static str> {
    match loader {
        "neoforge" => Some("neoforge tps"),
        "forge"    => Some("forge tps"),
        // Vanilla `/tick query` exists since 1.20.3 and works on Fabric, Quilt and vanilla
        _ if crate::setup::version_key(minecraft_version) >= vec![1, 20, 3] => Some("tick query"),
        _ => None,
    }
}

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
            let tps_cmd = {
                let instances = state.instances.read().await;
                instances.get(&instance_id).and_then(|i| {
                    tps_command(
                        i.config.instance.loader.as_deref().unwrap_or_default(),
                        &i.config.instance.minecraft_version,
                    )
                })
            };
            if let Some(cmd) = tps_cmd {
                let processes = state.processes.lock().await;
                if let Some(handle) = processes.get(&instance_id) {
                    let _ = handle.stdin_tx.send(cmd.to_string());
                }
            }
        }

        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

// ── TPS line parser (called from instance.rs log processing) ─────────────────

/// Parses TPS from the output of the server's TPS command (see [`tps_command`]):
///   Forge, NeoForge 1.20.x: "Overall: Mean tick time: 0.519 ms. Mean TPS: 20.000"
///   NeoForge 1.21+:         "Overall: 20.000 TPS (0.519 ms/tick)"
///   Vanilla `tick query`:   "Average time per tick: 2.3ms (Target: 50.0ms)"
/// Per-dimension lines are ignored. Numbers follow the JVM locale, so "20,000" is accepted too.
pub fn parse_tps(line: &str) -> Option<f32> {
    parse_overall_tps(line).or_else(|| parse_tick_query(line))
}

/// TPS from vanilla `tick query`: the server can't run faster than its target tick time.
fn parse_tick_query(line: &str) -> Option<f32> {
    let rest = &line[line.find("Average time per tick: ")? + "Average time per tick: ".len()..];
    let (avg, rest) = rest.split_once("ms (Target: ")?;
    let target = rest.split_once("ms)")?.0;
    let parse = |s: &str| s.trim().replace(',', ".").parse::<f32>().ok();
    let tick_ms = parse(avg)?.max(parse(target)?);
    (tick_ms > 0.0).then(|| 1000.0 / tick_ms)
}

fn parse_overall_tps(line: &str) -> Option<f32> {
    let rest = &line[line.find("Overall: ")? + "Overall: ".len()..];
    let (num, neoforge_style) = match rest.find("Mean TPS: ") {
        Some(pos) => (&rest[pos + "Mean TPS: ".len()..], false),
        None => (rest, true),
    };
    let end = num
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != ',')
        .unwrap_or(num.len());
    if neoforge_style && !num[end..].starts_with(" TPS") {
        return None;
    }
    num[..end].replace(',', ".").parse().ok()
}

#[cfg(test)]
mod tests {
    use super::parse_tps;

    #[test]
    fn parses_forge_overall() {
        let line = "[12:00:00] [Server thread/INFO] [minecraft/MinecraftServer]: Overall: Mean tick time: 0.519 ms. Mean TPS: 20.000";
        assert_eq!(parse_tps(line), Some(20.0));
    }

    #[test]
    fn parses_neoforge_overall() {
        let line = "[12:00:00] [Server thread/INFO] [minecraft/MinecraftServer]: Overall: 18.532 TPS (53.960 ms/tick)";
        assert_eq!(parse_tps(line), Some(18.532));
    }

    #[test]
    fn parses_comma_decimal() {
        assert_eq!(parse_tps("Overall: 19,750 TPS (50,633 ms/tick)"), Some(19.75));
        assert_eq!(parse_tps("Overall: Mean tick time: 0,519 ms. Mean TPS: 20,000"), Some(20.0));
    }

    #[test]
    fn parses_tick_query() {
        // Keeping up: capped at the target rate
        assert_eq!(parse_tps("Average time per tick: 2.3ms (Target: 50.0ms)"), Some(20.0));
        // Lagging: 80 ms per tick → 12.5 TPS
        assert_eq!(parse_tps("Average time per tick: 80.0ms (Target: 50.0ms)"), Some(12.5));
        assert_eq!(parse_tps("Average time per tick: 80,0ms (Target: 50,0ms)"), Some(12.5));
        // Sprinting has no target and says nothing about normal TPS
        assert_eq!(parse_tps("Average time per tick: 2.3ms"), None);
        assert_eq!(parse_tps("Target tick rate: 20.0 per second."), None);
    }

    #[test]
    fn picks_tps_command() {
        use super::tps_command;
        assert_eq!(tps_command("neoforge", "1.21.1"), Some("neoforge tps"));
        assert_eq!(tps_command("forge", "1.20.1"), Some("forge tps"));
        assert_eq!(tps_command("fabric", "1.21.1"), Some("tick query"));
        assert_eq!(tps_command("fabric", "1.20.3"), Some("tick query"));
        assert_eq!(tps_command("fabric", "26.1"), Some("tick query"));
        assert_eq!(tps_command("fabric", "1.20.2"), None);
        assert_eq!(tps_command("", "1.19.4"), None);
    }

    #[test]
    fn ignores_dimension_lines() {
        assert_eq!(parse_tps("Overworld: 20.000 TPS (0.519 ms/tick)"), None);
        assert_eq!(parse_tps("Dim minecraft:overworld (minecraft:overworld): Mean tick time: 0.519 ms. Mean TPS: 20.000"), None);
    }

    #[test]
    fn ignores_unrelated_lines() {
        assert_eq!(parse_tps("<Steve> Overall: this server is great"), None);
        assert_eq!(parse_tps("Unknown or incomplete command, see below for error"), None);
    }
}
