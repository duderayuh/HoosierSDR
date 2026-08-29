//! Embedded HTTP server for remote (Tailscale) access to the running app.
//!
//! The desktop app is already "the server" — it owns the live `AppState`
//! (catalog, decode loop, audio, alerts, …). This module exposes that same
//! state over HTTP so a phone on the tailnet can drive and watch it, without
//! a second process or any state sync.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    extract::{FromRequestParts, State},
    http::{request::Parts, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager};

use crate::AppState;

mod api;

const DEFAULT_PORT: u16 = 8042;
const MOBILE_HTML: &str = include_str!("mobile.html");

fn port() -> u16 {
    std::env::var("HS_WEB_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_PORT)
}

/// Shared state handed to every axum handler.
pub struct WebState {
    pub app: AppHandle,
    pub token: String,
}

fn generate_token() -> String {
    let seed = format!(
        "hoosiersdr-web-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let h = Sha256::digest(seed.as_bytes());
    h.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

/// Start the server on its own thread + tokio runtime. Never blocks the UI.
pub fn spawn(app: AppHandle) {
    let token = match crate::secrets::get("web_token") {
        Some(t) => t,
        None => {
            let t = generate_token();
            if crate::secrets::set("web_token", &t).is_err() {
                eprintln!("[web] WARNING: could not persist web token");
            }
            eprintln!("[web] access token (saved to secrets.json): {t}");
            t
        }
    };

    let state = Arc::new(WebState { app, token });

    let router = Router::new()
        .route("/", get(mobile_page))
        .route("/api/health", get(health))
        .route("/api/status", get(status))
        .route("/api/command", post(command))
        .with_state(state);

    std::thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[web] tokio runtime: {e}");
                return;
            }
        };
        rt.block_on(async move {
            let addr = SocketAddr::from(([0, 0, 0, 0], port()));
            let listener = match tokio::net::TcpListener::bind(addr).await {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("[web] bind {addr} failed: {e}");
                    return;
                }
            };
            eprintln!("[web] listening on http://{addr} (and on your Tailscale IP)");
            if let Err(e) = axum::serve(listener, router).await {
                eprintln!("[web] server error: {e}");
            }
        });
    });
}

async fn mobile_page() -> impl IntoResponse {
    axum::response::Html(MOBILE_HTML)
}

async fn health() -> &'static str {
    "ok"
}

#[derive(Serialize)]
struct Status {
    running: bool,
    catalog_len: usize,
    server_time: u64,
}

async fn status(State(st): State<Arc<WebState>>, _auth: Auth) -> Json<Status> {
    let s = st.app.state::<AppState>();
    let running = s.running.load(std::sync::atomic::Ordering::SeqCst);
    let catalog_len = s.catalog.lock().unwrap().as_ref().map_or(0, |c| c.len());
    let server_time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Json(Status {
        running,
        catalog_len,
        server_time,
    })
}

#[derive(Deserialize)]
struct CommandRequest {
    command: String,
    #[serde(default)]
    args: serde_json::Value,
}

async fn command(
    State(st): State<Arc<WebState>>,
    _auth: Auth,
    Json(req): Json<CommandRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    match api::dispatch(&st.app, &req.command, &req.args) {
        Ok(v) => Ok(Json(v)),
        Err(e) => Err((StatusCode::BAD_REQUEST, e)),
    }
}

/// Require the shared token on every `/api/*` route.
pub struct Auth;

fn extract_token(parts: &Parts) -> Option<String> {
    if let Some(q) = parts.uri.query() {
        for kv in q.split('&') {
            let mut it = kv.splitn(2, '=');
            if it.next() == Some("token") {
                return it.next().map(|v| v.to_string());
            }
        }
    }
    if let Some(h) = parts
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
    {
        if let Some(t) = h.strip_prefix("Bearer ") {
            return Some(t.trim().to_string());
        }
    }
    parts
        .headers
        .get("x-token")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
}

impl FromRequestParts<Arc<WebState>> for Auth {
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<WebState>,
    ) -> Result<Self, Self::Rejection> {
        match extract_token(parts) {
            Some(t) if t == state.token => Ok(Auth),
            _ => Err((StatusCode::UNAUTHORIZED, "invalid or missing token")),
        }
    }
}
