mod api;
mod backup;
mod ban;
mod config;
mod discord;
mod instance;
mod metrics;
mod mod_mgr;
mod restart;
mod setup;
mod sse;
mod state;
mod whitelist;

use std::sync::Arc;

use axum::{
    Router,
    http::header,
    response::{Html, IntoResponse},
    routing::{get, post},
};
use tokio::sync::{Mutex, RwLock, broadcast};
use tower_http::cors::CorsLayer;

use state::AppState;

const HTML: &str = include_str!("ui/index.html");
const CSS: &str = include_str!("ui/style.css");
const JS: &str = include_str!("ui/app.js");

async fn serve_css() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], CSS)
}

async fn serve_js() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/javascript; charset=utf-8")], JS)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::new(
                std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
            ),
        )
        .init();

    let global_config = config::GlobalConfig::load().unwrap_or_default();
    let instances = config::discover_instances().await;

    tracing::info!("Discovered {} instance(s)", instances.len());

    let (log_tx, _) = broadcast::channel(2048);

    let state = Arc::new(AppState {
        instances: RwLock::new(instances),
        processes: Mutex::new(Default::default()),
        log_tx,
        global_config,
        http_client: reqwest::Client::new(),
    });

    let port = state
        .global_config
        .web
        .as_ref()
        .and_then(|w| w.port)
        .unwrap_or(8080);

    let app = Router::new()
        // Static assets
        .route("/", get(|| async { Html(HTML) }))
        .route("/style.css", get(serve_css))
        .route("/app.js", get(serve_js))
        // Server-sent events
        .route("/events", get(sse::sse_handler))
        // API
        .route("/api/instances", get(api::list_instances).post(api::add_instance))
        .route("/api/instances/{id}/logs", get(api::get_logs))
        .route("/api/instances/{id}/start", post(api::start_instance))
        .route("/api/instances/{id}/stop", post(api::stop_instance))
        .route("/api/instances/{id}/switch", post(api::switch_instance))
        .route("/api/instances/{id}/cmd", post(api::send_command))
        .route("/api/instances/{id}/backups", get(api::list_backups).post(api::create_backup))
        .route("/api/instances/{id}/backups/{filename}/restore", post(api::restore_backup))
        .route("/api/instances/{id}/mods", get(api::list_mods).post(api::scan_mods))
        .route("/api/instances/{id}/mods/updates", get(api::get_mod_updates))
        .route("/api/instances/{id}/mods/update-all", post(api::update_all_mods))
        .route("/api/instances/{id}/mods/{project_id}/update", post(api::update_single_mod))
        .route("/api/instances/{id}/update-version", post(api::update_server_version))
        .route("/api/whitelist", get(api::get_whitelist).post(api::add_to_whitelist))
        .route("/api/whitelist/{name}", axum::routing::delete(api::remove_from_whitelist))
        .route("/api/bans/players", get(api::get_banned_players).post(api::ban_player))
        .route("/api/bans/players/{name}", axum::routing::delete(api::unban_player))
        .route("/api/bans/ips", get(api::get_banned_ips).post(api::ban_ip))
        .route("/api/bans/ips/{ip}", axum::routing::delete(api::unban_ip))
        .route("/api/instances/{id}/properties", get(api::get_properties).post(api::set_properties))
        .route("/api/instances/{id}/restart-config", post(api::update_restart_config))
        .route("/api/setup/install-neoforge", post(api::install_neoforge))
        .with_state(state.clone())
        .layer(CorsLayer::permissive());

    let addr = format!("0.0.0.0:{}", port);
    tracing::info!("Listening on http://localhost:{}", port);

    backup::start_schedulers(state.clone());
    restart::start_restart_schedulers(state.clone());
    start_instance_watcher(state.clone());

    // Sync master whitelist and bans to all instance directories on startup
    {
        let wl = whitelist::read_master();
        whitelist::sync_all(&state, &wl).await;
        let players = ban::read_banned_players();
        let ips = ban::read_banned_ips();
        ban::sync_all(&state, &players, &ips).await;
    }

    if let Some(discord_cfg) = state.global_config.discord.clone() {
        discord::start_bot(state.clone(), discord_cfg);
    }

    tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
        if let Err(e) = open::that(format!("http://localhost:{}", port)) {
            tracing::warn!("Could not open browser: {}", e);
        }
    });

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

fn start_instance_watcher(state: Arc<AppState>) {
    tokio::spawn(async move {
        let instances_dir = config::data_dir().join("instances");
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;

            let mut entries = match tokio::fs::read_dir(&instances_dir).await {
                Ok(e) => e,
                Err(_) => continue,
            };

            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if !path.is_dir() { continue; }
                let id = path.file_name().unwrap().to_string_lossy().to_string();

                let already_known = state.instances.read().await.contains_key(&id);
                if already_known { continue; }

                if let Some((id, inst_state)) = config::load_instance_dir(&path).await {
                    let info = state::InstanceInfo::from(&inst_state);
                    state.instances.write().await.insert(id, inst_state);
                    let _ = state.log_tx.send(state::WsEvent::InstanceAdded { instance: info });
                }
            }
        }
    });
}
