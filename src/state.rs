use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};

use crate::config::{GlobalConfig, InstanceConfig};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceStatus {
    Stopped,
    Starting,
    Running,
    Stopping,
    Crashed,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogLine {
    pub line: String,
    pub timestamp: i64,
}

pub struct InstanceState {
    pub id: String,
    pub instance_dir: PathBuf,
    pub config: InstanceConfig,
    pub status: InstanceStatus,
    pub players: HashSet<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub log_buffer: VecDeque<LogLine>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstanceInfo {
    pub id: String,
    pub name: String,
    pub display_name: String,
    pub minecraft_version: String,
    pub status: InstanceStatus,
    pub players: Vec<String>,
    pub started_at: Option<i64>,
    pub port: u16,
}

impl From<&InstanceState> for InstanceInfo {
    fn from(s: &InstanceState) -> Self {
        InstanceInfo {
            id: s.id.clone(),
            name: s.config.instance.name.clone(),
            display_name: s
                .config
                .instance
                .display_name
                .clone()
                .unwrap_or_else(|| s.config.instance.name.clone()),
            minecraft_version: s.config.instance.minecraft_version.clone(),
            status: s.status.clone(),
            players: s.players.iter().cloned().collect(),
            started_at: s.started_at.map(|t| t.timestamp()),
            port: s.config.instance.port,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsEvent {
    LogLine {
        instance_id: String,
        line: String,
        timestamp: i64,
    },
    StateChanged {
        instance_id: String,
        status: InstanceStatus,
    },
    PlayerJoined {
        instance_id: String,
        player: String,
    },
    PlayerLeft {
        instance_id: String,
        player: String,
    },
}

pub struct ProcessHandle {
    pub stdin_tx: mpsc::UnboundedSender<String>,
}

pub struct AppState {
    pub instances: RwLock<HashMap<String, InstanceState>>,
    pub processes: Mutex<HashMap<String, ProcessHandle>>,
    pub log_tx: broadcast::Sender<WsEvent>,
    pub global_config: GlobalConfig,
}
