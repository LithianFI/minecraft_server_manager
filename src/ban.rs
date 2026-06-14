use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::config::data_dir;
use crate::state::{AppState, InstanceStatus};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BannedPlayer {
    pub uuid: String,
    pub name: String,
    pub created: String,
    pub source: String,
    pub expires: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BannedIp {
    pub ip: String,
    pub created: String,
    pub source: String,
    pub expires: String,
    pub reason: String,
}

fn now_str() -> String {
    Utc::now().format("%Y-%m-%d %H:%M:%S %z").to_string()
}

fn banned_players_path() -> PathBuf { data_dir().join("banned-players.json") }
fn banned_ips_path() -> PathBuf     { data_dir().join("banned-ips.json") }

pub fn read_banned_players() -> Vec<BannedPlayer> {
    std::fs::read_to_string(banned_players_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn read_banned_ips() -> Vec<BannedIp> {
    std::fs::read_to_string(banned_ips_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn write_banned_players(entries: &[BannedPlayer]) -> Result<(), String> {
    serde_json::to_string_pretty(entries)
        .map_err(|e| e.to_string())
        .and_then(|s| std::fs::write(banned_players_path(), s).map_err(|e| e.to_string()))
}

pub fn write_banned_ips(entries: &[BannedIp]) -> Result<(), String> {
    serde_json::to_string_pretty(entries)
        .map_err(|e| e.to_string())
        .and_then(|s| std::fs::write(banned_ips_path(), s).map_err(|e| e.to_string()))
}

pub fn new_player_ban(uuid: String, name: String, reason: String) -> BannedPlayer {
    BannedPlayer { uuid, name, created: now_str(), source: "MSM".into(), expires: "forever".into(), reason }
}

pub fn new_ip_ban(ip: String, reason: String) -> BannedIp {
    BannedIp { ip, created: now_str(), source: "MSM".into(), expires: "forever".into(), reason }
}

pub fn sync_to_server(server_path: &Path, players: &[BannedPlayer], ips: &[BannedIp]) {
    if let Ok(s) = serde_json::to_string_pretty(players) {
        let _ = std::fs::write(server_path.join("banned-players.json"), s);
    }
    if let Ok(s) = serde_json::to_string_pretty(ips) {
        let _ = std::fs::write(server_path.join("banned-ips.json"), s);
    }
}

pub async fn sync_all(state: &AppState, players: &[BannedPlayer], ips: &[BannedIp]) {
    let instances = state.instances.read().await;
    let processes = state.processes.lock().await;

    for inst in instances.values() {
        sync_to_server(&inst.config.server.path, players, ips);

        if matches!(inst.status, InstanceStatus::Running) {
            if let Some(handle) = processes.get(&inst.id) {
                let _ = handle.stdin_tx.send("banlist reload".to_string());
            }
        }
    }
}

pub async fn lookup_player(
    client: &reqwest::Client,
    username: &str,
) -> Result<(String, String), String> {
    // Reuse Mojang lookup from whitelist logic
    crate::whitelist::lookup_player(client, username)
        .await
        .map(|e| (e.uuid, e.name))
}
