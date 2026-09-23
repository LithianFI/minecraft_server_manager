use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    extract::{Request, State},
    http::{HeaderMap, Method, StatusCode, header},
    middleware::Next,
    response::{Html, IntoResponse, Json, Redirect, Response},
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::state::AppState;

const COOKIE_NAME: &str = "msm_session";
const SESSION_TTL: Duration = Duration::from_secs(30 * 24 * 3600);
const LOGIN_HTML: &str = include_str!("ui/login.html");

/// Logged-in browser sessions: token → expiry. Kept in memory, so a restart logs everyone out.
#[derive(Default)]
pub struct Sessions(Mutex<HashMap<String, Instant>>);

fn password(state: &AppState) -> Option<&str> {
    state.global_config.web.as_ref()?.password.as_deref().filter(|p| !p.is_empty())
}

fn api_token(state: &AppState) -> Option<&str> {
    state.global_config.web.as_ref()?.api_token.as_deref().filter(|t| !t.is_empty())
}

/// Compares secrets without leaking their length or content through timing.
fn secrets_equal(a: &str, b: &str) -> bool {
    let (ha, hb) = (Sha256::digest(a.as_bytes()), Sha256::digest(b.as_bytes()));
    ha.iter().zip(hb.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .expect("failed to read /dev/urandom");
    hex::encode(bytes)
}

fn session_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| c.trim().strip_prefix(COOKIE_NAME)?.strip_prefix('='))
}

async fn has_session(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(token) = session_cookie(headers) else { return false };
    let mut sessions = state.sessions.0.lock().await;
    match sessions.get(token) {
        Some(expiry) if *expiry > Instant::now() => true,
        Some(_) => {
            sessions.remove(token);
            false
        }
        None => false,
    }
}

fn has_api_token(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(expected) = api_token(state) else { return false };
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| secrets_equal(t.trim(), expected))
}

/// Browsers label every request with Sec-Fetch-Site. Anything that changes state must come
/// from the dashboard itself ("same-origin") or not from a web page at all ("none", or no
/// header for curl/scripts) — this stops other sites from driving MSM through your browser.
fn is_cross_site(req: &Request) -> bool {
    if matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        return false;
    }
    req.headers()
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|site| site != "same-origin" && site != "none")
}

pub async fn require_auth(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    if is_cross_site(&req) {
        return (StatusCode::FORBIDDEN, "Cross-site request refused").into_response();
    }
    if password(&state).is_none() {
        return next.run(req).await;
    }

    let path = req.uri().path();
    // Player pages are public; their random token is the secret
    let public = matches!(path, "/login" | "/api/login" | "/style.css") || path.starts_with("/pack/");
    // The API token is read-only: it never authorises changes
    let token_ok = *req.method() == Method::GET && has_api_token(&state, req.headers());

    if public || token_ok || has_session(&state, req.headers()).await {
        return next.run(req).await;
    }
    if path.starts_with("/api/") || path == "/events" {
        (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "Not logged in" }))).into_response()
    } else {
        Redirect::to("/login").into_response()
    }
}

pub fn warn_if_open(state: &AppState) {
    if password(state).is_none() {
        tracing::warn!(
            "No [web] password set in config.toml — anyone who can reach the dashboard can control your servers"
        );
    }
}

pub async fn login_page(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if password(&state).is_none() || has_session(&state, &headers).await {
        return Redirect::to("/").into_response();
    }
    Html(LOGIN_HTML).into_response()
}

#[derive(Deserialize)]
pub struct LoginRequest {
    pub password: String,
}

pub async fn login(State(state): State<Arc<AppState>>, Json(req): Json<LoginRequest>) -> Response {
    let Some(expected) = password(&state) else {
        return StatusCode::NO_CONTENT.into_response();
    };
    if !secrets_equal(&req.password, expected) {
        tracing::warn!("Failed dashboard login attempt");
        // Slow down password guessing
        tokio::time::sleep(Duration::from_secs(1)).await;
        return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "Wrong password" }))).into_response();
    }

    let token = random_token();
    {
        let mut sessions = state.sessions.0.lock().await;
        let now = Instant::now();
        sessions.retain(|_, expiry| *expiry > now);
        sessions.insert(token.clone(), now + SESSION_TTL);
    }
    let cookie = format!(
        "{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
        SESSION_TTL.as_secs()
    );
    (StatusCode::NO_CONTENT, [(header::SET_COOKIE, cookie)]).into_response()
}

pub async fn logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(token) = session_cookie(&headers) {
        state.sessions.0.lock().await.remove(token);
    }
    let cookie = format!("{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0");
    (StatusCode::NO_CONTENT, [(header::SET_COOKIE, cookie)]).into_response()
}

/// Tells the dashboard whether to show the log-out button.
pub async fn session_info(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "auth_enabled": password(&state).is_some() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_secrets() {
        assert!(secrets_equal("hunter2", "hunter2"));
        assert!(!secrets_equal("hunter2", "hunter3"));
        assert!(!secrets_equal("hunter2", "hunter22"));
        assert!(!secrets_equal("", "x"));
    }

    #[test]
    fn finds_session_cookie() {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, "theme=dark; msm_session=abc123; other=1".parse().unwrap());
        assert_eq!(session_cookie(&h), Some("abc123"));
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, "msm_sessionx=nope".parse().unwrap());
        assert_eq!(session_cookie(&h), None);
    }
}
