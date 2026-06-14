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
    pub restart: Option<RestartConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct RestartConfig {
    #[serde(default)]
    pub auto_restart: bool,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    #[serde(default = "default_delay_secs")]
    pub delay_secs: u64,
    pub schedule: Option<String>,
    #[serde(default = "default_warning_secs")]
    pub warning_secs: u64,
}

fn default_max_attempts() -> u32 { 3 }
fn default_delay_secs() -> u64 { 10 }
fn default_warning_secs() -> u64 { 300 }

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub java_opts: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub java_path: Option<String>,
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
    let _ = fs::create_dir_all(&instances_dir).await;
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
        if let Some((id, state)) = load_instance_dir(&path).await {
            instances.insert(id, state);
        }
    }

    instances
}

/// Load a single instance dir, auto-generating msm.toml from run.sh if absent.
pub async fn load_instance_dir(path: &std::path::Path) -> Option<(String, crate::state::InstanceState)> {
    let id = path.file_name()?.to_string_lossy().to_string();
    let msm_toml = path.join("msm.toml");

    let config: InstanceConfig = if msm_toml.exists() {
        let content = fs::read_to_string(&msm_toml).await.ok()?;
        match toml::from_str(&content) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("Failed to parse {}: {}", msm_toml.display(), e);
                return None;
            }
        }
    } else {
        // Auto-detect: only import if this looks like a NeoForge/Forge server
        let auto = auto_detect_config(path)?;
        let toml_str = toml::to_string_pretty(&auto).ok()?;
        if let Err(e) = fs::write(&msm_toml, toml_str).await {
            tracing::warn!("Could not write auto-generated msm.toml for '{}': {}", id, e);
        } else {
            tracing::info!("Auto-detected server at '{}', generated msm.toml", path.display());
        }
        auto
    };

    let server_path = if config.server.path.is_absolute() {
        config.server.path.clone()
    } else {
        path.join(&config.server.path)
    };

    let mut resolved = config;
    resolved.server.path = server_path;

    Some((
        id.clone(),
        crate::state::InstanceState {
            id,
            instance_dir: path.to_path_buf(),
            config: resolved,
            status: crate::state::InstanceStatus::Stopped,
            players: std::collections::HashSet::new(),
            started_at: None,
            log_buffer: std::collections::VecDeque::new(),
            ram_mb: None,
            tps: None,
            restart_attempts: 0,
        },
    ))
}

/// Returns Some(config) if the dir has a run.sh and looks like a Minecraft server.
fn auto_detect_config(dir: &std::path::Path) -> Option<InstanceConfig> {
    if !dir.join("run.sh").exists() {
        return None;
    }

    let id = dir.file_name()?.to_string_lossy().to_string();

    let port = std::fs::read_to_string(dir.join("server.properties"))
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("server-port="))
                .and_then(|l| l["server-port=".len()..].trim().parse::<u16>().ok())
        })
        .unwrap_or(25565);

    let minecraft_version = detect_mc_version(dir).unwrap_or_else(|| "1.21.1".to_string());

    Some(InstanceConfig {
        restart: None,
        instance: InstanceMeta {
            name: id.clone(),
            display_name: None,
            minecraft_version,
            loader: Some("neoforge".to_string()),
            loader_version: None,
            port,
        },
        server: ServerConfig {
            path: dir.to_path_buf(),
            java_opts: None,
            java_path: None,
        },
        backup: Some(BackupConfig {
            enabled: false,
            schedule: None,
            keep_count: 10,
            world_only: false,
        }),
    })
}

/// Try to read the MC version from run.sh (NeoForge embeds it in the library path).
fn detect_mc_version(dir: &std::path::Path) -> Option<String> {
    let run_sh = std::fs::read_to_string(dir.join("run.sh")).ok()?;
    // NeoForge run.sh contains a path like .../neoforged/neoforge/21.1.172/...
    let marker = "/neoforged/neoforge/";
    let idx = run_sh.find(marker)?;
    let after = &run_sh[idx + marker.len()..];
    let nf_ver = after.split('/').next()?;
    let parts: Vec<&str> = nf_ver.split('.').collect();
    if parts.len() < 2 { return None; }
    let minor: u32 = parts[1].parse().ok()?;
    Some(if minor == 0 {
        format!("1.{}", parts[0])
    } else {
        format!("1.{}.{}", parts[0], parts[1])
    })
}
