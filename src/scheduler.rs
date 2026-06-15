use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::state::{AppState, InstanceStatus};

pub fn start_command_schedulers(state: Arc<AppState>) {
    tokio::spawn(run_scheduler(state));
}

async fn run_scheduler(state: Arc<AppState>) {
    // (instance_id, schedule_name) -> last_run timestamp
    let mut last_run: HashMap<(String, String), i64> = HashMap::new();

    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;

        let now = chrono::Utc::now().timestamp();

        // Collect (instance_id, schedule_name, interval, command) for running instances
        let due: Vec<(String, String, String)> = {
            let instances = state.instances.read().await;
            let mut out = vec![];
            for (id, inst) in instances.iter() {
                if !matches!(inst.status, InstanceStatus::Running) {
                    continue;
                }
                for entry in &inst.config.schedules {
                    if !entry.enabled {
                        continue;
                    }
                    let key = (id.clone(), entry.name.clone());
                    let last = last_run.get(&key).copied().unwrap_or(0);
                    if now - last >= entry.interval_secs as i64 {
                        out.push((id.clone(), entry.name.clone(), entry.command.clone()));
                    }
                }
            }
            out
        };

        for (instance_id, name, command) in due {
            last_run.insert((instance_id.clone(), name.clone()), now);
            let processes = state.processes.lock().await;
            if let Some(handle) = processes.get(&instance_id) {
                let _ = handle.stdin_tx.send(command.clone());
                tracing::info!("Scheduled '{}' sent to '{}': {}", name, instance_id, command);
            }
        }
    }
}
