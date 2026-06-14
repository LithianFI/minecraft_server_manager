use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use cron::Schedule;

use crate::instance;
use crate::state::{AppState, InstanceStatus};

pub fn start_restart_schedulers(state: Arc<AppState>) {
    tokio::spawn(async move {
        let scheduled: Vec<(String, String, u64)> = {
            let instances = state.instances.read().await;
            instances
                .values()
                .filter_map(|inst| {
                    let restart = inst.config.restart.as_ref()?;
                    let schedule = restart.schedule.as_ref()?.clone();
                    Some((inst.id.clone(), schedule, restart.warning_secs))
                })
                .collect()
        };

        for (id, schedule_str, warning_secs) in scheduled {
            let state = state.clone();
            tokio::spawn(async move {
                run_restart_scheduler(state, id, schedule_str, warning_secs).await;
            });
        }
    });
}

async fn run_restart_scheduler(
    state: Arc<AppState>,
    instance_id: String,
    schedule_str: String,
    warning_secs: u64,
) {
    let normalized = normalize_cron(&schedule_str);
    let schedule = match Schedule::from_str(&normalized) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                "Invalid restart schedule for '{}': '{}' — {}",
                instance_id, schedule_str, e
            );
            return;
        }
    };

    for next in schedule.upcoming(Utc) {
        let now = Utc::now();
        let until_restart = (next - now).to_std().unwrap_or(Duration::ZERO);

        // Sleep until warning_secs before restart (or immediately if already past that point)
        let sleep_first = until_restart.saturating_sub(Duration::from_secs(warning_secs));
        tokio::time::sleep(sleep_first).await;

        // Only proceed if server is running
        let is_running = state
            .instances
            .read()
            .await
            .get(&instance_id)
            .map(|i| matches!(i.status, InstanceStatus::Running))
            .unwrap_or(false);

        let actual_warning = until_restart
            .saturating_sub(sleep_first)
            .as_secs();

        if is_running {
            send_countdown(&state, &instance_id, actual_warning).await;
        } else {
            // Server isn't running, just wait out the remaining time
            tokio::time::sleep(Duration::from_secs(actual_warning)).await;
            continue;
        }

        tracing::info!("Scheduled restart firing for '{}'", instance_id);
        do_restart(&state, &instance_id).await;
    }
}

/// Sleeps through countdown milestones, sending in-game warnings via `say`.
/// Called with `total_secs` = seconds remaining until restart.
async fn send_countdown(state: &AppState, instance_id: &str, total_secs: u64) {
    // (seconds_before_restart, human label) — descending order
    const MILESTONES: &[(u64, &str)] = &[
        (300, "5 minutes"),
        (60,  "1 minute"),
        (30,  "30 seconds"),
        (10,  "10 seconds"),
    ];

    let mut slept = 0u64;

    for &(secs_before, label) in MILESTONES {
        if total_secs <= secs_before {
            continue; // milestone is outside our warning window
        }
        let fire_at = total_secs - secs_before; // seconds from start
        if fire_at <= slept {
            continue; // already past
        }
        tokio::time::sleep(Duration::from_secs(fire_at - slept)).await;
        slept = fire_at;
        say(state, instance_id, &format!("Server restarting in {}.", label)).await;
    }

    // Sleep to T-0
    let remaining = total_secs.saturating_sub(slept);
    if remaining > 0 {
        tokio::time::sleep(Duration::from_secs(remaining)).await;
    }
    say(state, instance_id, "Server is restarting now. See you soon!").await;
}

async fn say(state: &AppState, instance_id: &str, message: &str) {
    let processes = state.processes.lock().await;
    if let Some(handle) = processes.get(instance_id) {
        let _ = handle.stdin_tx.send(format!("say {}", message));
    }
}

async fn do_restart(state: &Arc<AppState>, instance_id: &str) {
    if let Err(e) = instance::stop_instance(state.clone(), instance_id).await {
        tracing::warn!("Scheduled restart: stop failed for '{}': {}", instance_id, e);
        return;
    }

    let stopped = tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            let done = state
                .instances
                .read()
                .await
                .get(instance_id)
                .map(|i| i.status == InstanceStatus::Stopped)
                .unwrap_or(true);
            if done { break; }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await;

    if stopped.is_err() {
        tracing::warn!("Scheduled restart: '{}' did not stop within 90s", instance_id);
        return;
    }

    if let Err(e) = instance::start_instance(state.clone(), instance_id).await {
        tracing::error!("Scheduled restart: start failed for '{}': {}", instance_id, e);
    }
}

fn normalize_cron(schedule: &str) -> String {
    let fields: Vec<&str> = schedule.split_whitespace().collect();
    if fields.len() == 5 { format!("0 {}", schedule) } else { schedule.to_string() }
}
