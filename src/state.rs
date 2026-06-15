use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};

use std::sync::Arc;
use crate::config::{DiscordNotifyConfig, GlobalConfig, InstanceConfig};

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
    pub ram_mb: Option<u64>,
    pub tps: Option<f32>,
    pub cpu_pct: Option<f32>,
    pub restart_attempts: u32,
    pub low_tps_streak: u32,
    pub high_ram_alerted: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstanceInfo {
    pub id: String,
    pub name: String,
    pub display_name: String,
    pub minecraft_version: String,
    pub loader: Option<String>,
    pub status: InstanceStatus,
    pub players: Vec<String>,
    pub started_at: Option<i64>,
    pub port: u16,
    pub ram_mb: Option<u64>,
    pub tps: Option<f32>,
    pub cpu_pct: Option<f32>,
    pub java_path: Option<String>,
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
            loader: s.config.instance.loader.clone(),
            status: s.status.clone(),
            players: s.players.iter().cloned().collect(),
            started_at: s.started_at.map(|t| t.timestamp()),
            port: s.config.instance.port,
            ram_mb: s.ram_mb,
            tps: s.tps,
            cpu_pct: s.cpu_pct,
            java_path: s.config.server.java_path.clone(),
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
    BackupDone {
        instance_id: String,
        filename: String,
        size_bytes: u64,
    },
    BackupFailed {
        instance_id: String,
        error: String,
    },
    Metrics {
        instance_id: String,
        ram_mb: u64,
        tps: Option<f32>,
        cpu_pct: Option<f32>,
    },
    SetupLog {
        message: String,
    },
    SetupDone {
        server_path: String,
    },
    SetupFailed {
        error: String,
    },
    InstanceAdded {
        instance: InstanceInfo,
    },
    AutoRestarting {
        instance_id: String,
        attempt: u32,
        max_attempts: u32,
    },
    UpdateLog {
        instance_id: String,
        message: String,
    },
    UpdateDone {
        instance_id: String,
        minecraft_version: String,
    },
    UpdateFailed {
        instance_id: String,
        error: String,
    },
    ModpackLog {
        message: String,
    },
    ModpackDone {
        server_path: String,
    },
    ModpackFailed {
        error: String,
    },
    HealthAlert {
        instance_id: String,
        kind: String,
        message: String,
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
    pub discord_notify: Arc<RwLock<DiscordNotifyConfig>>,
    pub http_client: reqwest::Client,
    pub metrics_db: crate::metrics_db::MetricsDb,
}
