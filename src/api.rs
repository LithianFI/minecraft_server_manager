use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};
use serde::{Deserialize, Serialize};

use crate::{
    backup::{self, BackupInfo},
    config::{data_dir, BackupConfig, InstanceConfig, InstanceMeta, ServerConfig},
    instance,
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
        instance: InstanceMeta {
            name: id.clone(),
            display_name: Some(req.display_name.clone()),
            minecraft_version: req.minecraft_version.clone(),
            loader: Some("neoforge".to_string()),
            loader_version: None,
            port: req.port,
        },
        server: ServerConfig {
            path: server_path.clone(),
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
