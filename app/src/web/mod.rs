//! Embedded HTTP server for remote (Tailscale) access to the running app.
//!
//! The desktop app is already "the server" — it owns the live `AppState`
//! (catalog, decode loop, audio, alerts, …). This module exposes that same
//! state over HTTP so a phone on the tailnet can drive and watch it, without
//! a second process or any state sync.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{FromRequestParts, State},
    http::{request::Parts, StatusCode},
    response::{
        sse::{Event as SseEvent, KeepAlive, Sse},
        IntoResponse,
    },
    routing::{get, post},
    Json, Router,
};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Listener, Manager};
use tokio_stream::wrappers::BroadcastStream;

use crate::AppState;

mod api;

const DEFAULT_PORT: u16 = 8042;
const MOBILE_HTML: &str = include_str!("mobile.html");
/// How many frames the SSE broadcast buffers per lagging client before it
/// starts dropping the oldest (audio frames are large, so keep this modest).
const FRAME_BUFFER: usize = 64;
/// Every event name the app emits — forwarded verbatim to SSE clients so a
/// phone sees the same live feed the desktop window does.
const APP_EVENTS: &[&str] = &[
    "follow",
    "error",
    "stopped",
    "hook_error",
    "alert_error",
    "analyzers",
    "conversations",
    "decoderevent",
    "digests",
    "survey_done",
    "transcribe_error",
    "transcribe_ready",
];

fn port() -> u16 {
    std::env::var("HS_WEB_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_PORT)
}

/// One frame pushed to SSE clients: either a forwarded app event, or a
/// completed call's audio.
#[derive(Clone, Serialize)]
pub struct Frame {
    /// SSE event name — the app event name (`follow`, `error`, …) or `audio`.
    pub event: String,
    /// JSON payload. For app events this is the emitted payload; for `audio`
    /// it is `{ tg, priority, pcm_b64 }`.
    pub data: serde_json::Value,
}

impl Frame {
    pub fn event(name: &str, data: serde_json::Value) -> Self {
        Self {
            event: name.to_string(),
            data,
        }
    }

    /// A completed call's audio: 8 kHz mono i16 PCM, base64-encoded so it can
    /// ride an SSE `data:` field and be decoded into a WebAudio buffer.
    pub fn audio(tg: u16, priority: u8, pcm: &[i16]) -> Self {
        use base64::Engine as _;
        let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
        let pcm_b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        Self {
            event: "audio".to_string(),
            data: serde_json::json!({ "tg": tg, "priority": priority, "pcm_b64": pcm_b64 }),
        }
    }
}

/// Shared state handed to every axum handler.
pub struct WebState {
    pub app: AppHandle,
    pub token: String,
    pub frames: tokio::sync::broadcast::Sender<Frame>,
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

/// Read the web token from the secret store, generating and persisting one on
/// first use. Shared by `spawn` and `web_access_get` so both see the same key.
fn get_or_create_token() -> String {
    match crate::secrets::get("web_token") {
        Some(t) => t,
        None => {
            let t = generate_token();
            if crate::secrets::set("web_token", &t).is_err() {
                eprintln!("[web] WARNING: could not persist web token");
            }
            eprintln!("[web] access token (saved to secrets.json): {t}");
            t
        }
    }
}

/// Best hostname/IP for a phone to reach this Mac: the Tailscale IPv4 when
/// available (CLI in PATH, or bundled inside Tailscale.app), else the machine
/// hostname (MagicDNS), else localhost.
fn best_host() -> String {
    // The Tailscale CLI is usually in PATH, but the Mac App Store build tucks
    // it inside the .app bundle — try both.
    let tailscale = [
        "tailscale",
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    ];
    for bin in tailscale {
        if let Ok(out) = std::process::Command::new(bin).args(["ip", "-4"]).output() {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !s.is_empty() {
                    return s;
                }
            }
        }
    }
    if let Ok(out) = std::process::Command::new("hostname").output() {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }
    "localhost".to_string()
}

/// What the desktop UI shows for "Remote access": the URL to open on a phone
/// and the token to type in. Unlike other secrets, the token is meant to be
/// read off-screen and entered elsewhere, so it is returned in the clear here.
#[derive(Serialize)]
pub struct WebAccess {
    pub url: String,
    pub token: String,
    pub port: u16,
}

#[tauri::command]
pub fn web_access_get(app: AppHandle) -> WebAccess {
    let _ = app; // the port/token are process-wide; `app` is here for symmetry
    WebAccess {
        url: format!("http://{}:{}", best_host(), port()),
        token: get_or_create_token(),
        port: port(),
    }
}

/// Start the server on its own thread + tokio runtime. Never blocks the UI.
pub fn spawn(app: AppHandle) {
    let token = get_or_create_token();

    let (frames, _) = tokio::sync::broadcast::channel::<Frame>(FRAME_BUFFER);

    // Publish the sender into AppState so the follow loops (main.rs, dual.rs)
    // can tap completed calls' audio into the same stream.
    let _ = app.state::<AppState>().web_frames.set(frames.clone());

    // Forward every app event to SSE clients. `listen_any` registers on the
    // Tauri event bus and fires synchronously on the emitting thread, so frame
    // order matches emit order (audio tap → Call event).
    for name in APP_EVENTS {
        let tx = frames.clone();
        let n = name.to_string();
        app.listen_any(*name, move |ev| {
            let data = serde_json::from_str(ev.payload())
                .unwrap_or_else(|_| serde_json::Value::String(ev.payload().to_string()));
            let _ = tx.send(Frame::event(&n, data));
        });
    }

    let state = Arc::new(WebState { app, token, frames });

    let router = Router::new()
        .route("/", get(mobile_page))
        .route("/api/health", get(health))
        .route("/api/status", get(status))
        .route("/api/events", get(events))
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

/// Server-Sent Events: the live feed (grants, calls, notices, status, …) plus
/// `audio` frames carrying each completed call's PCM. One long-lived
/// connection the mobile UI opens and leaves open.
async fn events(
    State(st): State<Arc<WebState>>,
    _auth: Auth,
) -> Sse<impl futures::Stream<Item = Result<SseEvent, std::convert::Infallible>>> {
    let rx = st.frames.subscribe();
    // `BroadcastStream` yields `Ok(frame)` per frame and `Err(Lagged)` when a
    // client falls behind; it ends on its own when the channel closes. We
    // surface a lag as a frame so the client knows it missed audio.
    let stream = BroadcastStream::new(rx).filter_map(|item| async move {
        match item {
            Ok(frame) => match SseEvent::default().event(frame.event).json_data(frame.data) {
                Ok(ev) => Some(Ok::<_, std::convert::Infallible>(ev)),
                Err(_) => Some(Ok::<_, std::convert::Infallible>(
                    SseEvent::default().data("{}"),
                )),
            },
            Err(_) => Some(Ok::<_, std::convert::Infallible>(
                SseEvent::default()
                    .event("lagged")
                    .data("client fell behind"),
            )),
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
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
    match api::dispatch(&st.app, &req.command, &req.args).await {
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
