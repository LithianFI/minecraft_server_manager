mod api;
mod config;
mod instance;
mod sse;
mod state;

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
        .with_state(state)
        .layer(CorsLayer::permissive());

    let addr = format!("0.0.0.0:{}", port);
    tracing::info!("Listening on http://localhost:{}", port);

    tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
        if let Err(e) = open::that(format!("http://localhost:{}", port)) {
            tracing::warn!("Could not open browser: {}", e);
        }
    });

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
