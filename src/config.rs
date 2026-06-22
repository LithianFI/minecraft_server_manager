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
    #[serde(default)]
    pub notify: DiscordNotifyConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DiscordNotifyConfig {
    #[serde(default = "default_true")]
    pub server_started: bool,
    #[serde(default = "default_true")]
    pub server_stopped: bool,
    #[serde(default = "default_true")]
    pub server_crashed: bool,
    #[serde(default = "default_true")]
    pub backup_done: bool,
    #[serde(default = "default_true")]
    pub backup_failed: bool,
    #[serde(default = "default_true")]
    pub health_alerts: bool,
}

impl Default for DiscordNotifyConfig {
    fn default() -> Self {
        Self {
            server_started: true,
            server_stopped: true,
            server_crashed: true,
            backup_done: true,
            backup_failed: true,
            health_alerts: true,
        }
    }
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
    pub alerts: Option<AlertConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schedules: Vec<ScheduleEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct AlertConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_tps_min")]
    pub tps_min: f32,
    #[serde(default = "default_tps_consecutive")]
    pub tps_consecutive: u32,
    #[serde(default = "default_ram_pct_max")]
    pub ram_pct_max: u32,
    #[serde(default)]
    pub max_ram_mb: u64,
}

fn default_tps_min() -> f32 { 15.0 }
fn default_tps_consecutive() -> u32 { 3 }
fn default_ram_pct_max() -> u32 { 90 }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScheduleEntry {
    pub name: String,
    pub interval_secs: u64,
    pub command: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool { true }

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modrinth_project_id: Option<String>,
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

    pub fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = data_dir().join("config.toml");
        let content = toml::to_string_pretty(self)?;
        std::fs::write(path, content)?;
        Ok(())
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

    // Migrate instances where server files and MSM metadata share the same directory.
    migrate_server_dir(path, &mut resolved).await;

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
            cpu_pct: None,
            low_tps_streak: 0,
            high_ram_alerted: false,
            last_deaths: std::collections::HashMap::new(),
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

    let loader_info = detect_loader_info(dir);
    let minecraft_version = loader_info.as_ref()
        .map(|i| i.minecraft_version.clone())
        .unwrap_or_else(|| "unknown".to_string());
    let loader = loader_info.as_ref().map(|i| i.loader.clone());
    let loader_version = loader_info.as_ref().and_then(|i| i.loader_version.clone());

    Some(InstanceConfig {
        restart: None,
        instance: InstanceMeta {
            name: id.clone(),
            display_name: None,
            minecraft_version,
            loader,
            loader_version,
            port,
            modrinth_project_id: None,
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
        alerts: None,
        schedules: vec![],
    })
}

struct LoaderInfo {
    minecraft_version: String,
    loader: String,
    loader_version: Option<String>,
}

/// Detect MC version, loader, and loader version from run.sh.
/// Supports NeoForge, Forge, and Fabric.
fn detect_loader_info(dir: &std::path::Path) -> Option<LoaderInfo> {
    let run_sh = std::fs::read_to_string(dir.join("run.sh")).ok()?;

    // NeoForge: path like /neoforged/neoforge/21.1.172/
    if let Some(idx) = run_sh.find("/neoforged/neoforge/") {
        let after = &run_sh[idx + "/neoforged/neoforge/".len()..];
        let nf_ver = after.split('/').next()?;
        let parts: Vec<&str> = nf_ver.split('.').collect();
        if parts.len() >= 2 {
            let minor: u32 = parts[1].parse().ok()?;
            let mc_ver = if minor == 0 {
                format!("1.{}", parts[0])
            } else {
                format!("1.{}.{}", parts[0], parts[1])
            };
            return Some(LoaderInfo {
                minecraft_version: mc_ver,
                loader: "neoforge".to_string(),
                loader_version: Some(nf_ver.to_string()),
            });
        }
    }

    // Forge: path like /net/minecraftforge/forge/1.20.1-47.3.0/
    if let Some(idx) = run_sh.find("/net/minecraftforge/forge/") {
        let after = &run_sh[idx + "/net/minecraftforge/forge/".len()..];
        let ver_dir = after.split('/').next()?;
        // ver_dir is like "1.20.1-47.3.0"
        let (mc_ver, forge_ver) = ver_dir.split_once('-')?;
        return Some(LoaderInfo {
            minecraft_version: mc_ver.to_string(),
            loader: "forge".to_string(),
            loader_version: Some(forge_ver.to_string()),
        });
    }

    // Fabric: jar named fabric-server-mc.{mc}-loader.{loader}-launcher.{launcher}.jar
    if let Some(idx) = run_sh.find("fabric-server-mc.") {
        let after = &run_sh[idx + "fabric-server-mc.".len()..];
        let mc_end = after.find("-loader.")?;
        let mc_ver = after[..mc_end].to_string();
        let loader_after = &after[mc_end + "-loader.".len()..];
        let loader_end = loader_after.find('-').unwrap_or(loader_after.len());
        let loader_ver = loader_after[..loader_end].to_string();
        return Some(LoaderInfo {
            minecraft_version: mc_ver,
            loader: "fabric".to_string(),
            loader_version: Some(loader_ver),
        });
    }

    // Fabric fallback: run.sh references fabric-server-launch.jar
    if run_sh.contains("fabric-server-launch.jar") {
        return Some(LoaderInfo {
            minecraft_version: "unknown".to_string(),
            loader: "fabric".to_string(),
            loader_version: None,
        });
    }

    None
}

/// If server files and MSM metadata share the same directory (the old layout),
/// move the server files out to `servers/{id}/` and update msm.toml in place.
async fn migrate_server_dir(instance_dir: &std::path::Path, config: &mut InstanceConfig) {
    if config.server.path != instance_dir {
        return;
    }

    let id = match instance_dir.file_name() {
        Some(n) => n.to_string_lossy().to_string(),
        None => return,
    };

    let new_server_path = data_dir().join("servers").join(&id);

    if new_server_path.exists() {
        // Already migrated or a conflict — don't touch anything.
        return;
    }

    if fs::create_dir_all(&new_server_path).await.is_err() {
        tracing::warn!("Migration: could not create '{}'", new_server_path.display());
        return;
    }

    // Move everything except MSM metadata files to the new location.
    const KEEP: &[&str] = &["msm.toml", "mods.lock.toml"];
    let mut entries = match fs::read_dir(instance_dir).await {
        Ok(e) => e,
        Err(_) => return,
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        if KEEP.contains(&name.to_string_lossy().as_ref()) {
            continue;
        }
        let from = entry.path();
        let to = new_server_path.join(&name);
        if let Err(e) = fs::rename(&from, &to).await {
            tracing::warn!("Migration: could not move '{}': {}", from.display(), e);
        }
    }

    config.server.path = new_server_path.clone();

    if let Ok(toml_str) = toml::to_string_pretty(config) {
        let _ = fs::write(instance_dir.join("msm.toml"), toml_str).await;
    }

    tracing::info!(
        "Migrated instance '{}': server files moved to '{}'",
        id,
        new_server_path.display()
    );
}
