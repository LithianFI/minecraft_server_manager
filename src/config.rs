use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::fs;

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct GlobalConfig {
    pub discord: Option<DiscordConfig>,
    pub web: Option<WebConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DiscordConfig {
    pub token: String,
    pub guild_id: u64,
    pub channel_id: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WebConfig {
    pub port: Option<u16>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InstanceConfig {
    pub instance: InstanceMeta,
    pub server: ServerConfig,
    pub backup: Option<BackupConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InstanceMeta {
    pub name: String,
    pub display_name: Option<String>,
    pub minecraft_version: String,
    pub loader: Option<String>,
    pub loader_version: Option<String>,
    pub port: u16,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BackupConfig {
    pub enabled: bool,
    pub schedule: Option<String>,
    pub keep_count: usize,
    pub world_only: bool,
}

pub fn data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("msm")
}

impl GlobalConfig {
    pub fn load() -> Result<Self, Box<dyn std::error::Error>> {
        let path = data_dir().join("config.toml");
        let content = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&content)?)
    }
}

pub async fn discover_instances() -> HashMap<String, crate::state::InstanceState> {
    let instances_dir = data_dir().join("instances");
    let mut instances = HashMap::new();

    let mut entries = match fs::read_dir(&instances_dir).await {
        Ok(e) => e,
        Err(_) => return instances,
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let msm_toml = path.join("msm.toml");
        let content = match fs::read_to_string(&msm_toml).await {
            Ok(c) => c,
            Err(_) => continue,
        };

        let config: InstanceConfig = match toml::from_str(&content) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("Failed to parse {}: {}", msm_toml.display(), e);
                continue;
            }
        };

        let id = path.file_name().unwrap().to_string_lossy().to_string();

        let server_path = if config.server.path.is_absolute() {
            config.server.path.clone()
        } else {
            path.join(&config.server.path)
        };

        let mut resolved = config;
        resolved.server.path = server_path;

        instances.insert(
            id.clone(),
            crate::state::InstanceState {
                id,
                instance_dir: path,
                config: resolved,
                status: crate::state::InstanceStatus::Stopped,
                players: std::collections::HashSet::new(),
                started_at: None,
                log_buffer: std::collections::VecDeque::new(),
            },
        );
    }

    instances
}
