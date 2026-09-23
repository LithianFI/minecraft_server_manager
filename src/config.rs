use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::fs;

// Unknown keys are errors here: a misspelt `pasword` must not silently leave the dashboard open
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct GlobalConfig {
    pub discord: Option<DiscordConfig>,
    pub web: Option<WebConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct DiscordConfig {
    // Credentials may come from MSM_DISCORD_* environment variables instead of the file
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub token: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub guild_id: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub channel_id: u64,
    #[serde(default)]
    pub notify: DiscordNotifyConfig,
}

fn is_zero(n: &u64) -> bool { *n == 0 }

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
    #[serde(default = "default_true")]
    pub mods_installed: bool,
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
            mods_installed: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct WebConfig {
    pub port: Option<u16>,
    /// Dashboard password; when unset the dashboard is open to anyone who can reach it
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Read-only token for GET requests (e.g. Homepage), sent as `Authorization: Bearer <token>`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_token: Option<String>,
    /// Address players use to reach MSM from the internet, e.g. "https://mc.example.com:8081";
    /// used for links in Discord messages
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
    /// Optional second port that serves only the player pages (join page + mod downloads),
    /// so it can be exposed to the internet without the dashboard
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack_port: Option<u16>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distribution: Option<DistributionConfig>,
}

/// Sharing the server's mods with players (packwiz / Prism / .mrpack).
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct DistributionConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Secret part of the player links: /pack/<token>/
    #[serde(default)]
    pub token: String,
    /// Choices that override the automatic include/exclude decision, keyed by Modrinth project
    /// ID or "file:<jar name>" for mods that aren't on Modrinth
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub mods: std::collections::BTreeMap<String, bool>,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| pairs.iter().find(|(k, _)| *k == name).map(|(_, v)| v.to_string())
    }

    #[test]
    fn env_overrides_file() {
        let mut cfg: GlobalConfig = toml::from_str("[web]\nport = 7331\npassword = \"from-file\"\n").unwrap();
        cfg.apply_env(env(&[("MSM_WEB_PASSWORD", "from-env"), ("MSM_API_TOKEN", "tok")])).unwrap();
        let web = cfg.web.unwrap();
        assert_eq!(web.port, Some(7331));
        assert_eq!(web.password.as_deref(), Some("from-env"));
        assert_eq!(web.api_token.as_deref(), Some("tok"));
    }

    #[test]
    fn discord_from_env_keeps_file_notify_settings() {
        let mut cfg: GlobalConfig = toml::from_str("[discord.notify]\nbackup_done = false\n").unwrap();
        assert!(cfg.discord_bot().is_none(), "no credentials yet");
        cfg.apply_env(env(&[
            ("MSM_DISCORD_TOKEN", "abc"),
            ("MSM_DISCORD_GUILD_ID", "123"),
            ("MSM_DISCORD_CHANNEL_ID", "456"),
        ])).unwrap();
        let d = cfg.discord_bot().expect("bot configured");
        assert_eq!((d.token.as_str(), d.guild_id, d.channel_id), ("abc", 123, 456));
        assert!(!d.notify.backup_done);
    }

    #[test]
    fn repairs_year_based_neoforge_minecraft_version() {
        let meta = |mc: &str, loader: &str, lv: &str| InstanceMeta {
            name: "x".into(), display_name: None, minecraft_version: mc.into(),
            loader: Some(loader.into()), loader_version: Some(lv.into()), port: 25565,
            modrinth_project_id: None, modrinth_version_id: None,
        };
        let mut m = meta("1.26.1", "neoforge", "26.1.0.19-beta");
        assert_eq!(repair_minecraft_version(&mut m).as_deref(), Some("1.26.1"));
        assert_eq!(m.minecraft_version, "26.1");
        let mut m = meta("1.26.1.2", "neoforge", "26.1.2.7");
        repair_minecraft_version(&mut m);
        assert_eq!(m.minecraft_version, "26.1.2");
        // Already right, or not affected
        assert!(repair_minecraft_version(&mut meta("26.1", "neoforge", "26.1.0.19")).is_none());
        assert!(repair_minecraft_version(&mut meta("1.21.1", "neoforge", "21.1.200")).is_none());
        assert!(repair_minecraft_version(&mut meta("1.21.1", "fabric", "0.16.0")).is_none());
        // Other loaders, or no usable loader version: drop the bogus "1." prefix
        let mut m = meta("1.26.1", "fabric", "0.17.2");
        repair_minecraft_version(&mut m);
        assert_eq!(m.minecraft_version, "26.1");
        let mut m = meta("1.26.1", "neoforge", "");
        repair_minecraft_version(&mut m);
        assert_eq!(m.minecraft_version, "26.1");
    }

    #[test]
    fn rejects_misspelt_web_keys() {
        assert!(toml::from_str::<GlobalConfig>("[web]\npasword = \"x\"\n").is_err());
        assert!(toml::from_str::<GlobalConfig>("[wbe]\npassword = \"x\"\n").is_err());
    }

    #[test]
    fn rejects_bad_numbers() {
        let mut cfg = GlobalConfig::default();
        assert!(cfg.apply_env(env(&[("MSM_WEB_PORT", "eighty")])).is_err());
        assert!(cfg.apply_env(env(&[("MSM_WEB_PORT", "70000")])).is_err());
    }

    #[test]
    fn file_credentials_are_not_serialized_when_empty() {
        let cfg = GlobalConfig { discord: Some(DiscordConfig::default()), web: None };
        let out = toml::to_string(&cfg).unwrap();
        assert!(!out.contains("token") && !out.contains("guild_id"), "{}", out);
    }
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modrinth_project_id: Option<String>,
    /// Installed modpack version; unknown for packs imported before this was recorded
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modrinth_version_id: Option<String>,
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
    /// Reads config.toml only. A missing file is an empty config; an unreadable or invalid one
    /// is an error, so a typo can't silently switch off settings like the dashboard password.
    pub fn load() -> Result<Self, String> {
        let path = data_dir().join("config.toml");
        match std::fs::read_to_string(&path) {
            Ok(content) => toml::from_str(&content).map_err(|e| format!("{}: {}", path.display(), e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{}: {}", path.display(), e)),
        }
    }

    /// config.toml with MSM_* environment variables applied on top. The result is only used in
    /// memory — never save it, or environment secrets would end up in the file.
    pub fn load_effective() -> Result<Self, String> {
        let mut cfg = Self::load()?;
        cfg.apply_env(|name| std::env::var(name).ok().filter(|v| !v.is_empty()))?;
        Ok(cfg)
    }

    fn apply_env(&mut self, var: impl Fn(&str) -> Option<String>) -> Result<(), String> {
        let number = |name: &str| -> Result<Option<u64>, String> {
            var(name).map(|v| v.trim().parse().map_err(|_| format!("{} must be a number, got '{}'", name, v))).transpose()
        };

        let web = self.web.get_or_insert_with(Default::default);
        if let Some(port) = number("MSM_WEB_PORT")? {
            web.port = Some(u16::try_from(port).map_err(|_| format!("MSM_WEB_PORT out of range: {}", port))?);
        }
        if let Some(v) = var("MSM_WEB_PASSWORD") { web.password = Some(v); }
        if let Some(v) = var("MSM_API_TOKEN") { web.api_token = Some(v); }
        if let Some(v) = var("MSM_PUBLIC_URL") { web.public_url = Some(v); }
        if let Some(port) = number("MSM_PACK_PORT")? {
            web.pack_port = Some(u16::try_from(port).map_err(|_| format!("MSM_PACK_PORT out of range: {}", port))?);
        }

        let token = var("MSM_DISCORD_TOKEN");
        let guild = number("MSM_DISCORD_GUILD_ID")?;
        let channel = number("MSM_DISCORD_CHANNEL_ID")?;
        if token.is_some() || guild.is_some() || channel.is_some() {
            let d = self.discord.get_or_insert_with(Default::default);
            if let Some(v) = token { d.token = v; }
            if let Some(v) = guild { d.guild_id = v; }
            if let Some(v) = channel { d.channel_id = v; }
        }
        Ok(())
    }

    /// The Discord settings, if the bot has everything it needs to run.
    pub fn discord_bot(&self) -> Option<&DiscordConfig> {
        self.discord.as_ref().filter(|d| !d.token.is_empty() && d.guild_id != 0 && d.channel_id != 0)
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
        let mut config: InstanceConfig = match toml::from_str(&content) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("Failed to parse {}: {}", msm_toml.display(), e);
                return None;
            }
        };
        if let Some(bad) = repair_minecraft_version(&mut config.instance) {
            tracing::info!("'{}': corrected Minecraft version {} → {}", id, bad, config.instance.minecraft_version);
            if let Ok(toml_str) = toml::to_string_pretty(&config) {
                let _ = fs::write(&msm_toml, toml_str).await;
            }
        }
        config
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
            last_crash: None,
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
            modrinth_version_id: None,
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
        distribution: None,
    })
}

/// Older MSM versions read NeoForge 26.x (for Minecraft 26.x) as Minecraft "1.26.x", a version
/// that doesn't exist: after 1.21 Minecraft switched to year-based versions (26.1, …). Fixes it,
/// from the NeoForge version when there is one; returns the wrong value if it changed.
pub(crate) fn repair_minecraft_version(meta: &mut InstanceMeta) -> Option<String> {
    let rest = meta.minecraft_version.strip_prefix("1.")?;
    if !rest.split('.').next()?.parse::<u32>().is_ok_and(|m| m >= 26) {
        return None;
    }
    let from_loader = meta.loader_version.as_deref()
        .filter(|lv| meta.loader.as_deref() == Some("neoforge") && lv.split('.').next() == rest.split('.').next());
    let correct = match from_loader {
        Some(lv) => crate::setup::neoforge_to_mc(lv),
        None => rest.to_string(),
    };
    Some(std::mem::replace(&mut meta.minecraft_version, correct))
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
        if nf_ver.split('.').count() >= 2 {
            return Some(LoaderInfo {
                minecraft_version: crate::setup::neoforge_to_mc(nf_ver),
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
