use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
};
use serde::{Deserialize, Serialize};

use crate::{
    backup::{self, BackupInfo},
    ban,
    ban::{BannedIp, BannedPlayer},
    config::{data_dir, BackupConfig, InstanceConfig, InstanceMeta, RestartConfig, ServerConfig},
    ftb, instance, mod_mgr, modpack, setup, whitelist,
    mod_mgr::{ModEntry, ModSearchHit, ModUpdate},
    whitelist::WhitelistEntry,
    state::{AppState, InstanceInfo, InstanceState, InstanceStatus, LogLine},
};

#[derive(Serialize)]
pub struct ApiError {
    pub error: String,
}

type ApiResult<T> = Result<T, (StatusCode, Json<ApiError>)>;

fn err(code: StatusCode, msg: impl ToString) -> (StatusCode, Json<ApiError>) {
    (code, Json(ApiError { error: msg.to_string() }))
}

pub async fn list_instances(State(state): State<Arc<AppState>>) -> Json<Vec<InstanceInfo>> {
    let instances = state.instances.read().await;
    let mut infos: Vec<InstanceInfo> = instances.values().map(|s| s.into()).collect();
    infos.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    Json(infos)
}

pub async fn get_logs(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<LogLine>>> {
    let instances = state.instances.read().await;
    let inst = instances
        .get(&id)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("Instance '{}' not found", id)))?;
    Ok(Json(inst.log_buffer.iter().cloned().collect()))
}

pub async fn start_instance(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    instance::start_instance(state, &id)
        .await
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(|e| err(StatusCode::BAD_REQUEST, e))
}

pub async fn stop_instance(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    instance::stop_instance(state, &id)
        .await
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(|e| err(StatusCode::BAD_REQUEST, e))
}

pub async fn switch_instance(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    instance::switch_instance(state, &id)
        .await
        .map(|_| StatusCode::ACCEPTED)
        .map_err(|e| err(StatusCode::BAD_REQUEST, e))
}

#[derive(Deserialize)]
pub struct CmdRequest {
    pub command: String,
}

pub async fn send_command(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<CmdRequest>,
) -> ApiResult<StatusCode> {
    instance::send_command(state, &id, req.command)
        .await
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(|e| err(StatusCode::BAD_REQUEST, e))
}

#[derive(Deserialize)]
pub struct AddInstanceRequest {
    pub id: String,
    pub display_name: String,
    pub server_path: String,
    pub minecraft_version: String,
    pub port: u16,
    pub java_path: Option<String>,
}

pub async fn add_instance(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AddInstanceRequest>,
) -> ApiResult<Json<InstanceInfo>> {
    let id = slugify(&req.id);

    if id.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "Invalid instance ID"));
    }

    {
        let instances = state.instances.read().await;
        if instances.contains_key(&id) {
            return Err(err(
                StatusCode::CONFLICT,
                format!("Instance '{}' already exists", id),
            ));
        }
    }

    let server_path = std::path::PathBuf::from(&req.server_path);
    if !server_path.exists() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("Server path '{}' does not exist", req.server_path),
        ));
    }

    let instance_dir = data_dir().join("instances").join(&id);
    tokio::fs::create_dir_all(&instance_dir)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to create directory: {}", e)))?;

    let config = InstanceConfig {
        restart: None,
        instance: InstanceMeta {
            name: id.clone(),
            display_name: Some(req.display_name.clone()),
            minecraft_version: req.minecraft_version.clone(),
            loader: Some("neoforge".to_string()),
            loader_version: None,
            port: req.port,
            modrinth_project_id: None,
        },
        server: ServerConfig {
            path: server_path.clone(),
            java_opts: None,
            java_path: req.java_path.filter(|s| !s.trim().is_empty()),
        },
        backup: Some(BackupConfig {
            enabled: false,
            schedule: None,
            keep_count: 10,
            world_only: false,
        }),
    };

    let toml_str = toml::to_string_pretty(&config)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    tokio::fs::write(instance_dir.join("msm.toml"), toml_str)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let inst_state = InstanceState {
        id: id.clone(),
        instance_dir,
        config,
        status: InstanceStatus::Stopped,
        players: std::collections::HashSet::new(),
        started_at: None,
        log_buffer: std::collections::VecDeque::new(),
        ram_mb: None,
        tps: None,
        restart_attempts: 0,
    };

    let info = InstanceInfo::from(&inst_state);

    {
        let mut instances = state.instances.write().await;
        instances.insert(id, inst_state);
    }

    Ok(Json(info))
}

pub async fn list_backups(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<BackupInfo>>> {
    let exists = state.instances.read().await.contains_key(&id);
    if !exists {
        return Err(err(StatusCode::NOT_FOUND, format!("Instance '{}' not found", id)));
    }
    Ok(Json(backup::list_backups(&id)))
}

pub async fn create_backup(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let exists = state.instances.read().await.contains_key(&id);
    if !exists {
        return Err(err(StatusCode::NOT_FOUND, format!("Instance '{}' not found", id)));
    }
    tokio::spawn(backup::trigger_backup(state, id));
    Ok(StatusCode::ACCEPTED)
}

pub async fn restore_backup(
    State(state): State<Arc<AppState>>,
    Path((id, filename)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    backup::restore_backup(&state, &id, &filename)
        .await
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(|e| err(StatusCode::BAD_REQUEST, e))
}

pub async fn delete_backup(
    State(state): State<Arc<AppState>>,
    Path((id, filename)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let exists = state.instances.read().await.contains_key(&id);
    if !exists {
        return Err(err(StatusCode::NOT_FOUND, format!("Instance '{}' not found", id)));
    }
    backup::delete_backup(&id, &filename)
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(|e| err(StatusCode::BAD_REQUEST, e))
}

pub async fn download_backup(
    State(state): State<Arc<AppState>>,
    Path((id, filename)): Path<(String, String)>,
) -> Result<impl axum::response::IntoResponse, (StatusCode, String)> {
    let exists = state.instances.read().await.contains_key(&id);
    if !exists {
        return Err((StatusCode::NOT_FOUND, format!("Instance '{}' not found", id)));
    }
    let path = backup::backup_path(&id, &filename)
        .map_err(|e| (StatusCode::NOT_FOUND, e))?;
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let disposition = format!("attachment; filename=\"{}\"", filename);
    Ok((
        [
            ("content-type", "application/octet-stream".to_string()),
            ("content-disposition", disposition),
        ],
        bytes,
    ))
}

#[derive(Deserialize)]
pub struct CopyBackupRequest {
    pub target_instance_id: String,
}

pub async fn copy_backup(
    State(state): State<Arc<AppState>>,
    Path((id, filename)): Path<(String, String)>,
    Json(req): Json<CopyBackupRequest>,
) -> ApiResult<StatusCode> {
    {
        let instances = state.instances.read().await;
        if !instances.contains_key(&id) {
            return Err(err(StatusCode::NOT_FOUND, format!("Instance '{}' not found", id)));
        }
        if !instances.contains_key(&req.target_instance_id) {
            return Err(err(StatusCode::NOT_FOUND, format!("Target instance '{}' not found", req.target_instance_id)));
        }
    }
    backup::copy_backup(&id, &filename, &req.target_instance_id)
        .await
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(|e| err(StatusCode::BAD_REQUEST, e))
}

// ── Mod handlers ──────────────────────────────────────────────────────────────

pub async fn list_mods(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<ModEntry>>> {
    let instances = state.instances.read().await;
    let inst = instances
        .get(&id)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("Instance '{}' not found", id)))?;
    Ok(Json(mod_mgr::read_lock(&inst.instance_dir).mods))
}

pub async fn scan_mods(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<ModEntry>>> {
    let (server_path, instance_dir) = {
        let instances = state.instances.read().await;
        let inst = instances
            .get(&id)
            .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("Instance '{}' not found", id)))?;
        (inst.config.server.path.clone(), inst.instance_dir.clone())
    };
    let lock = mod_mgr::scan_mods(&state.http_client, &server_path, &instance_dir)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(lock.mods))
}

pub async fn get_mod_updates(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<ModUpdate>>> {
    let (instance_dir, mc_version, loader) = {
        let instances = state.instances.read().await;
        let inst = instances
            .get(&id)
            .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("Instance '{}' not found", id)))?;
        (
            inst.instance_dir.clone(),
            inst.config.instance.minecraft_version.clone(),
            inst.config.instance.loader.clone().unwrap_or_else(|| "neoforge".to_string()),
        )
    };
    let lock = mod_mgr::read_lock(&instance_dir);
    let updates = mod_mgr::check_updates(&state.http_client, &lock, &mc_version, &loader)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(updates))
}

pub async fn update_single_mod(
    State(state): State<Arc<AppState>>,
    Path((id, project_id)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let (server_path, instance_dir, mc_version, loader) = {
        let instances = state.instances.read().await;
        let inst = instances
            .get(&id)
            .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("Instance '{}' not found", id)))?;
        (
            inst.config.server.path.clone(),
            inst.instance_dir.clone(),
            inst.config.instance.minecraft_version.clone(),
            inst.config.instance.loader.clone().unwrap_or_else(|| "neoforge".to_string()),
        )
    };
    let mut lock = mod_mgr::read_lock(&instance_dir);
    let updates = mod_mgr::check_updates(&state.http_client, &lock, &mc_version, &loader)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let update = updates
        .iter()
        .find(|u| u.project_id == project_id)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "No update available for this mod"))?
        .clone();
    mod_mgr::apply_update(&state.http_client, &update, &server_path, &mut lock)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    mod_mgr::write_lock(&instance_dir, &lock)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn update_all_mods(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let (server_path, instance_dir, mc_version, loader) = {
        let instances = state.instances.read().await;
        let inst = instances
            .get(&id)
            .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("Instance '{}' not found", id)))?;
        (
            inst.config.server.path.clone(),
            inst.instance_dir.clone(),
            inst.config.instance.minecraft_version.clone(),
            inst.config.instance.loader.clone().unwrap_or_else(|| "neoforge".to_string()),
        )
    };
    let mut lock = mod_mgr::read_lock(&instance_dir);
    let updates = mod_mgr::check_updates(&state.http_client, &lock, &mc_version, &loader)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    for update in &updates {
        mod_mgr::apply_update(&state.http_client, update, &server_path, &mut lock)
            .await
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{}: {}", update.name, e)))?;
    }
    mod_mgr::write_lock(&instance_dir, &lock)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(StatusCode::NO_CONTENT)
}

// ── Mod search + add ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SearchModsQuery {
    pub term: String,
}

pub async fn search_mods_for_instance(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(params): Query<SearchModsQuery>,
) -> ApiResult<Json<Vec<ModSearchHit>>> {
    let (mc_version, loader) = {
        let instances = state.instances.read().await;
        let inst = instances
            .get(&id)
            .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("Instance '{}' not found", id)))?;
        (
            inst.config.instance.minecraft_version.clone(),
            inst.config.instance.loader.clone().unwrap_or_else(|| "neoforge".to_string()),
        )
    };
    let hits = mod_mgr::search_mods(&state.http_client, &params.term, &mc_version, &loader)
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, e))?;
    Ok(Json(hits))
}

#[derive(Deserialize)]
pub struct AddModRequest {
    pub project_id: String,
    pub version_id: String,
}

pub async fn add_mod_to_instance(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<AddModRequest>,
) -> ApiResult<Json<Vec<ModEntry>>> {
    let (server_path, instance_dir, mc_version, loader) = {
        let instances = state.instances.read().await;
        let inst = instances
            .get(&id)
            .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("Instance '{}' not found", id)))?;
        (
            inst.config.server.path.clone(),
            inst.instance_dir.clone(),
            inst.config.instance.minecraft_version.clone(),
            inst.config.instance.loader.clone().unwrap_or_else(|| "neoforge".to_string()),
        )
    };
    let entries = mod_mgr::add_mod(
        &state.http_client,
        &req.project_id,
        &req.version_id,
        &mc_version,
        &loader,
        &server_path,
        &instance_dir,
    )
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(entries))
}

// ── Version update ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct UpdateVersionRequest {
    pub neoforge_version: String,
}

pub async fn update_server_version(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<UpdateVersionRequest>,
) -> ApiResult<StatusCode> {
    if req.neoforge_version.trim().is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "neoforge_version is required"));
    }
    let exists = state.instances.read().await.contains_key(&id);
    if !exists {
        return Err(err(StatusCode::NOT_FOUND, format!("Instance '{}' not found", id)));
    }
    tokio::spawn(setup::update_server_version(state, id, req.neoforge_version));
    Ok(StatusCode::ACCEPTED)
}

// ── Setup (NeoForge installer) ─────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct InstallRequest {
    pub version: String,
    pub server_path: String,
}

pub async fn install_neoforge(
    State(state): State<Arc<AppState>>,
    Json(req): Json<InstallRequest>,
) -> ApiResult<StatusCode> {
    if req.version.trim().is_empty() || req.server_path.trim().is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "version and server_path are required"));
    }
    tokio::spawn(setup::install_neoforge(state, req.version, req.server_path));
    Ok(StatusCode::ACCEPTED)
}

#[derive(Deserialize)]
pub struct ImportModpackRequest {
    pub version_id: String,
    #[serde(default)]
    pub server_path: String,
    pub instance_name: String,
    pub port: u16,
}

pub async fn import_modpack(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ImportModpackRequest>,
) -> ApiResult<StatusCode> {
    if req.version_id.trim().is_empty() || req.instance_name.trim().is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "version_id and instance_name are required"));
    }
    tokio::spawn(modpack::import_modpack(state, modpack::ImportRequest {
        version_id: req.version_id,
        server_path: req.server_path,
        instance_name: req.instance_name,
        port: req.port,
    }));
    Ok(StatusCode::ACCEPTED)
}

// ── Java ──────────────────────────────────────────────────────────────────────

pub async fn get_java_installs() -> impl IntoResponse {
    let (sys_ver, installs) = tokio::task::spawn_blocking(|| {
        (crate::java::java_version("java"), crate::java::list_java_installs())
    }).await.unwrap_or((None, vec![]));

    Json(serde_json::json!({
        "system_version": sys_ver,
        "installs": installs,
    }))
}

#[derive(Deserialize)]
pub struct JavaConfigRequest {
    pub java_path: Option<String>,
}

pub async fn set_java_config(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<JavaConfigRequest>,
) -> ApiResult<StatusCode> {
    let instance_dir = {
        let instances = state.instances.read().await;
        instances
            .get(&id)
            .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("Instance '{}' not found", id)))?
            .instance_dir.clone()
    };

    {
        let mut instances = state.instances.write().await;
        if let Some(inst) = instances.get_mut(&id) {
            inst.config.server.java_path = req.java_path.filter(|s| !s.trim().is_empty());
            if let Ok(toml_str) = toml::to_string_pretty(&inst.config) {
                let _ = std::fs::write(instance_dir.join("msm.toml"), toml_str);
            }
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

// ── FTB ───────────────────────────────────────────────────────────────────────

pub async fn ftb_search(
    State(state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let term = params.get("term").cloned().unwrap_or_default();

    let search: serde_json::Value = match state.http_client
        .get("https://api.modpacks.ch/public/modpack/search/20")
        .query(&[("term", &term)])
        .header("User-Agent", "msm/0.1")
        .send().await
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.to_string())
    {
        Ok(r) => match r.json().await.map_err(|e: reqwest::Error| e.to_string()) {
            Ok(v) => v,
            Err(e) => return (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": e}))).into_response(),
        },
        Err(e) => return (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": e}))).into_response(),
    };

    // Pack IDs (search returns either [id, ...] or [{id, ...}, ...])
    let pack_ids: Vec<u64> = search.get("packs")
        .and_then(|p| p.as_array())
        .map(|arr| arr.iter().filter_map(|p| {
            p.as_u64().or_else(|| p.get("id").and_then(|id| id.as_u64()))
        }).take(12).collect())
        .unwrap_or_default();

    // Fetch details for each pack in parallel
    let fetches: Vec<_> = pack_ids.iter().map(|&id| {
        let client = state.http_client.clone();
        async move {
            client
                .get(format!("https://api.modpacks.ch/public/modpack/{}", id))
                .header("User-Agent", "msm/0.1")
                .send().await.ok()?
                .error_for_status().ok()?
                .json::<serde_json::Value>().await.ok()
        }
    }).collect();

    let packs: Vec<serde_json::Value> = futures_util::future::join_all(fetches).await
        .into_iter()
        .flatten()
        .collect();

    Json(serde_json::json!({ "packs": packs })).into_response()
}

#[derive(Deserialize)]
pub struct FtbImportRequest {
    pub pack_id: u64,
    pub version_id: u64,
    #[serde(default)]
    pub server_path: String,
    pub instance_name: String,
    pub port: u16,
}

pub async fn import_ftb_pack(
    State(state): State<Arc<AppState>>,
    Json(req): Json<FtbImportRequest>,
) -> ApiResult<StatusCode> {
    if req.instance_name.trim().is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "instance_name is required"));
    }
    tokio::spawn(ftb::import_ftb(state, ftb::FtbImportRequest {
        pack_id: req.pack_id,
        version_id: req.version_id,
        server_path: req.server_path,
        instance_name: req.instance_name,
        port: req.port,
    }));
    Ok(StatusCode::ACCEPTED)
}

// ── Whitelist ─────────────────────────────────────────────────────────────────

pub async fn get_whitelist() -> Json<Vec<WhitelistEntry>> {
    Json(whitelist::read_master())
}

#[derive(Deserialize)]
pub struct AddWhitelistRequest {
    pub username: String,
}

pub async fn add_to_whitelist(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AddWhitelistRequest>,
) -> ApiResult<Json<WhitelistEntry>> {
    let username = req.username.trim();
    if username.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "username is required"));
    }

    let entry = whitelist::lookup_player(&state.http_client, username)
        .await
        .map_err(|e| err(StatusCode::BAD_REQUEST, e))?;

    let mut entries = whitelist::read_master();
    if entries.iter().any(|e| e.name.eq_ignore_ascii_case(&entry.name)) {
        return Err(err(StatusCode::CONFLICT, format!("'{}' is already whitelisted", entry.name)));
    }
    entries.push(entry.clone());
    whitelist::write_master(&entries).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    whitelist::sync_all(&state, &entries).await;

    Ok(Json(entry))
}

pub async fn remove_from_whitelist(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    let mut entries = whitelist::read_master();
    let before = entries.len();
    entries.retain(|e| !e.name.eq_ignore_ascii_case(&name));
    if entries.len() == before {
        return Err(err(StatusCode::NOT_FOUND, format!("'{}' is not whitelisted", name)));
    }
    whitelist::write_master(&entries).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    whitelist::sync_all(&state, &entries).await;
    Ok(StatusCode::NO_CONTENT)
}

// ── Bans ──────────────────────────────────────────────────────────────────────

pub async fn get_banned_players() -> Json<Vec<BannedPlayer>> {
    Json(ban::read_banned_players())
}

pub async fn get_banned_ips() -> Json<Vec<BannedIp>> {
    Json(ban::read_banned_ips())
}

#[derive(Deserialize)]
pub struct BanPlayerRequest {
    pub username: String,
    #[serde(default)]
    pub reason: String,
}

pub async fn ban_player(
    State(state): State<Arc<AppState>>,
    Json(req): Json<BanPlayerRequest>,
) -> ApiResult<Json<BannedPlayer>> {
    let username = req.username.trim();
    if username.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "username is required"));
    }
    let reason = if req.reason.is_empty() {
        "Banned by an operator.".to_string()
    } else {
        req.reason.clone()
    };

    let (uuid, name) = ban::lookup_player(&state.http_client, username)
        .await
        .map_err(|e| err(StatusCode::BAD_REQUEST, e))?;

    let mut players = ban::read_banned_players();
    if players.iter().any(|p| p.name.eq_ignore_ascii_case(&name)) {
        return Err(err(StatusCode::CONFLICT, format!("'{}' is already banned", name)));
    }

    let entry = ban::new_player_ban(uuid, name, reason);
    players.push(entry.clone());
    ban::write_banned_players(&players).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    ban::sync_all(&state, &players, &ban::read_banned_ips()).await;
    Ok(Json(entry))
}

pub async fn unban_player(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    let mut players = ban::read_banned_players();
    let before = players.len();
    players.retain(|p| !p.name.eq_ignore_ascii_case(&name));
    if players.len() == before {
        return Err(err(StatusCode::NOT_FOUND, format!("'{}' is not banned", name)));
    }
    ban::write_banned_players(&players).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    ban::sync_all(&state, &players, &ban::read_banned_ips()).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct BanIpRequest {
    pub ip: String,
    #[serde(default)]
    pub reason: String,
}

pub async fn ban_ip(
    State(state): State<Arc<AppState>>,
    Json(req): Json<BanIpRequest>,
) -> ApiResult<Json<BannedIp>> {
    let ip = req.ip.trim().to_string();
    if ip.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "ip is required"));
    }
    let reason = if req.reason.is_empty() {
        "Banned by an operator.".to_string()
    } else {
        req.reason.clone()
    };

    let mut ips = ban::read_banned_ips();
    if ips.iter().any(|e| e.ip == ip) {
        return Err(err(StatusCode::CONFLICT, format!("'{}' is already banned", ip)));
    }
    let entry = ban::new_ip_ban(ip, reason);
    ips.push(entry.clone());
    ban::write_banned_ips(&ips).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    ban::sync_all(&state, &ban::read_banned_players(), &ips).await;
    Ok(Json(entry))
}

pub async fn unban_ip(
    State(state): State<Arc<AppState>>,
    Path(ip): Path<String>,
) -> ApiResult<StatusCode> {
    let mut ips = ban::read_banned_ips();
    let before = ips.len();
    ips.retain(|e| e.ip != ip);
    if ips.len() == before {
        return Err(err(StatusCode::NOT_FOUND, format!("'{}' is not banned", ip)));
    }
    ban::write_banned_ips(&ips).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    ban::sync_all(&state, &ban::read_banned_players(), &ips).await;
    Ok(StatusCode::NO_CONTENT)
}

// ── Server properties ─────────────────────────────────────────────────────────

pub async fn get_properties(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Json<std::collections::HashMap<String, String>>> {
    let server_path = {
        let instances = state.instances.read().await;
        instances
            .get(&id)
            .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("Instance '{}' not found", id)))?
            .config.server.path.clone()
    };
    let props = parse_server_properties(&server_path)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(props))
}

pub async fn set_properties(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(updates): Json<std::collections::HashMap<String, String>>,
) -> ApiResult<StatusCode> {
    let server_path = {
        let instances = state.instances.read().await;
        instances
            .get(&id)
            .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("Instance '{}' not found", id)))?
            .config.server.path.clone()
    };
    let mut props = parse_server_properties(&server_path).unwrap_or_default();
    for (k, v) in updates {
        props.insert(k, v);
    }
    write_server_properties(&server_path, &props)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(StatusCode::NO_CONTENT)
}

fn parse_server_properties(
    server_path: &std::path::Path,
) -> Result<std::collections::HashMap<String, String>, String> {
    let content = std::fs::read_to_string(server_path.join("server.properties"))
        .map_err(|e| format!("Cannot read server.properties: {}", e))?;
    let mut map = std::collections::HashMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() { continue; }
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    Ok(map)
}

fn write_server_properties(
    server_path: &std::path::Path,
    props: &std::collections::HashMap<String, String>,
) -> Result<(), String> {
    let path = server_path.join("server.properties");
    // Preserve comments and order from existing file, update values in-place
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut updated: Vec<String> = Vec::new();
    let mut written: std::collections::HashSet<&str> = std::collections::HashSet::new();

    for line in existing.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') || trimmed.is_empty() {
            updated.push(line.to_string());
            continue;
        }
        if let Some((k, _)) = trimmed.split_once('=') {
            let k = k.trim();
            if let Some(v) = props.get(k) {
                updated.push(format!("{}={}", k, v));
                written.insert(k);
            } else {
                updated.push(line.to_string());
            }
        } else {
            updated.push(line.to_string());
        }
    }
    // Append any new keys not in the original file
    for (k, v) in props {
        if !written.contains(k.as_str()) {
            updated.push(format!("{}={}", k, v));
        }
    }
    std::fs::write(&path, updated.join("\n") + "\n")
        .map_err(|e| format!("Cannot write server.properties: {}", e))
}

// ── Restart config ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct RestartConfigRequest {
    pub auto_restart: bool,
    pub max_attempts: u32,
    pub delay_secs: u64,
    pub schedule: Option<String>,
    pub warning_secs: u64,
}

pub async fn update_restart_config(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<RestartConfigRequest>,
) -> ApiResult<StatusCode> {
    let instance_dir = {
        let instances = state.instances.read().await;
        instances
            .get(&id)
            .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("Instance '{}' not found", id)))?
            .instance_dir.clone()
    };

    let new_cfg = RestartConfig {
        auto_restart: req.auto_restart,
        max_attempts: req.max_attempts,
        delay_secs: req.delay_secs,
        schedule: req.schedule.filter(|s| !s.trim().is_empty()),
        warning_secs: req.warning_secs,
    };

    {
        let mut instances = state.instances.write().await;
        if let Some(inst) = instances.get_mut(&id) {
            inst.config.restart = Some(new_cfg);
            if let Ok(toml_str) = toml::to_string_pretty(&inst.config) {
                let _ = std::fs::write(instance_dir.join("msm.toml"), toml_str);
            }
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

fn slugify(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}
