use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::data_dir;
use crate::state::{AppState, InstanceStatus};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhitelistEntry {
    pub uuid: String,
    pub name: String,
}

pub fn master_path() -> PathBuf {
    data_dir().join("whitelist.json")
}

pub fn read_master() -> Vec<WhitelistEntry> {
    std::fs::read_to_string(master_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn write_master(entries: &[WhitelistEntry]) -> Result<(), String> {
    let json = serde_json::to_string_pretty(entries).map_err(|e| e.to_string())?;
    std::fs::write(master_path(), json).map_err(|e| e.to_string())
}

pub fn sync_to_server(server_path: &Path, entries: &[WhitelistEntry]) {
    if let Ok(json) = serde_json::to_string_pretty(entries) {
        let _ = std::fs::write(server_path.join("whitelist.json"), json);
    }
}

pub async fn sync_all(state: &AppState, entries: &[WhitelistEntry]) {
    let instances = state.instances.read().await;
    let processes = state.processes.lock().await;

    for inst in instances.values() {
        sync_to_server(&inst.config.server.path, entries);

        if matches!(inst.status, InstanceStatus::Running) {
            if let Some(handle) = processes.get(&inst.id) {
                let _ = handle.stdin_tx.send("whitelist reload".to_string());
            }
        }
    }
}

pub async fn lookup_player(
    client: &reqwest::Client,
    username: &str,
) -> Result<WhitelistEntry, String> {
    #[derive(Deserialize)]
    struct MojangProfile {
        id: String,
        name: String,
    }

    let url = format!(
        "https://api.mojang.com/users/profiles/minecraft/{}",
        username
    );
    let resp = client
        .get(&url)
        .header("User-Agent", "msm/0.1")
        .send()
        .await
        .map_err(|e| e.to_string())?;

    match resp.status().as_u16() {
        200 => {}
        404 => return Err(format!("Player '{}' not found", username)),
        204 => return Err(format!("Player '{}' not found", username)),
        s => return Err(format!("Mojang API returned HTTP {}", s)),
    }

    let profile: MojangProfile = resp.json().await.map_err(|e| e.to_string())?;
    let uuid = format_uuid(&profile.id)?;
    Ok(WhitelistEntry { uuid, name: profile.name })
}

fn format_uuid(raw: &str) -> Result<String, String> {
    if raw.len() != 32 {
        return Err(format!("Unexpected UUID length from Mojang: '{}'", raw));
    }
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &raw[0..8],
        &raw[8..12],
        &raw[12..16],
        &raw[16..20],
        &raw[20..32]
    ))
}
