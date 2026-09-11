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
    extract::{ConnectInfo, FromRequestParts, Path, Query, State},
    http::{header, request::Parts, StatusCode},
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
/// Frames a slow client may fall behind before it starts losing the oldest.
/// Spectrum frames are throttled before they enter the ring (see
/// `spawn`), so at this depth a remote desktop on a slow link loses
/// waterfall frames long before it loses audio.
const FRAME_BUFFER: usize = 512;

/// Waterfall/constellation frames forwarded to web clients per second, at
/// most. The desktop draws ~12/s locally; remote viewers get a calmer one.
const HEAVY_FPS: f64 = 5.0;

/// Which `follow` frames are heavy enough to be worth dropping for a client
/// that did not ask for them (a phone) or that is falling behind.
fn is_heavy(frame: &Frame) -> bool {
    frame.event == "follow"
        && matches!(
            frame.data.get("kind").and_then(|k| k.as_str()),
            Some("spectrum") | Some("constellation")
        )
}
/// Every event name the app emits — forwarded verbatim to SSE clients so a
/// phone sees the same live feed the desktop window does.
const APP_EVENTS: &[&str] = &[
    "follow",
    "grant",
    "status",
    "spectrum",
    "runs",
    "error",
    "stopped",
    "hook_error",
    "alert",
    "alert_error",
    "analyzer",
    "analyzers",
    "conversations",
    "decoderevent",
    "decoderdone",
    "digests",
    "dispatch",
    "dispatch_progress",
    "incident",
    "incident_deleted",
    "reencode_done",
    "reencode_error",
    "reencode_progress",
    "rr_progress",
    "survey_done",
    "transcribe_download",
    "transcribe_error",
    "transcribe_ready",
    "transcript",
];

pub fn port() -> u16 {
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
    pub snap: std::sync::Mutex<Snap>,
}

/// What a page joining mid-run needs to catch up: the last `measured` and
/// `site` frames (tuning and site panels), the last `status` (counters),
/// and the recent notices (the events log). Replayed to a new remote
/// desktop page ahead of the live feed.
#[derive(Default)]
pub struct Snap {
    /// Per run (several systems can be followed at once); key 0 = untagged.
    runs: std::collections::BTreeMap<u64, RunSnap>,
    /// Recent completed calls (all runs), for the call history. Marked
    /// `replayed` so the page adds the rows without re-sounding tones.
    calls: std::collections::VecDeque<serde_json::Value>,
}

#[derive(Default)]
struct RunSnap {
    measured: Option<serde_json::Value>,
    site: Option<serde_json::Value>,
    status: Option<serde_json::Value>,
    notices: std::collections::VecDeque<serde_json::Value>,
}

const SNAP_NOTICES: usize = 40;
const SNAP_CALLS: usize = 60;

impl Snap {
    fn note(&mut self, frame: &Frame) {
        match frame.event.as_str() {
            "follow" => {
                let run = frame.data.get("run").and_then(|r| r.as_u64()).unwrap_or(0);
                let kind = frame.data.get("kind").and_then(|k| k.as_str());
                if kind == Some("call") {
                    if self.calls.len() >= SNAP_CALLS {
                        self.calls.pop_front();
                    }
                    let mut d = frame.data.clone();
                    d["replayed"] = serde_json::Value::Bool(true);
                    self.calls.push_back(d);
                    return;
                }
                let r = self.runs.entry(run).or_default();
                match kind {
                    Some("measured") => {
                        r.measured = Some(frame.data.clone());
                        r.site = None;
                        r.notices.clear();
                    }
                    Some("site") => r.site = Some(frame.data.clone()),
                    Some("status") => r.status = Some(frame.data.clone()),
                    Some("notice") => {
                        if r.notices.len() >= SNAP_NOTICES {
                            r.notices.pop_front();
                        }
                        r.notices.push_back(frame.data.clone());
                    }
                    _ => {}
                }
            }
            // The run list: forget runs that have ended.
            "runs" => {
                let alive: std::collections::HashSet<u64> = frame
                    .data
                    .as_array()
                    .map(|a| a.iter().filter_map(|r| r.get("id").and_then(|i| i.as_u64())).collect())
                    .unwrap_or_default();
                self.runs.retain(|id, _| *id == 0 || alive.contains(id));
            }
            "stopped" | "error" => {
                self.runs.clear();
            }
            _ => {}
        }
    }

    /// The frames that rebuild the panels, run by run (the page keeps its
    /// single-system panels on the run it has picked), then the calls.
    fn events(&self) -> Vec<Frame> {
        let mut out = Vec::new();
        let f = |d: &serde_json::Value| Frame::event("follow", d.clone());
        for r in self.runs.values() {
            if let Some(m) = &r.measured {
                out.push(f(m));
            }
            if let Some(s) = &r.site {
                out.push(f(s));
            }
            out.extend(r.notices.iter().map(f));
            if let Some(s) = &r.status {
                out.push(f(s));
            }
        }
        out.extend(self.calls.iter().map(f));
        out
    }
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
    let heavy_gate = Arc::new(std::sync::Mutex::new(std::collections::HashMap::<String, std::time::Instant>::new()));
    for name in APP_EVENTS {
        let tx = frames.clone();
        let n = name.to_string();
        let gate = heavy_gate.clone();
        app.listen_any(*name, move |ev| {
            let data = serde_json::from_str(ev.payload())
                .unwrap_or_else(|_| serde_json::Value::String(ev.payload().to_string()));
            let frame = Frame::event(&n, data);
            if is_heavy(&frame) {
                // Throttle per kind so a burst of spectrum frames cannot
                // push audio out of the ring for a lagging client.
                let kind = frame.data["kind"].as_str().unwrap_or("").to_string();
                let mut g = gate.lock().unwrap();
                let now = std::time::Instant::now();
                if let Some(last) = g.get(&kind) {
                    if now.duration_since(*last).as_secs_f64() < 1.0 / HEAVY_FPS {
                        return;
                    }
                }
                g.insert(kind, now);
            }
            let _ = tx.send(frame);
        });
    }

    let state = Arc::new(WebState {
        app,
        token,
        frames,
        snap: std::sync::Mutex::new(Snap::default()),
    });
    // The tap above runs before `state` exists; feed the snapshot from a
    // subscriber of the same channel instead, so it sees every frame.
    {
        let st = state.clone();
        let mut rx = st.frames.subscribe();
        std::thread::spawn(move || loop {
            match rx.blocking_recv() {
                Ok(frame) => st.snap.lock().unwrap().note(&frame),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        });
    }

    let router = Router::new()
        .route("/", get(mobile_page))
        .route("/desktop", get(desktop_redirect))
        .route("/desktop/", get(desktop_index))
        .route("/desktop/{*path}", get(desktop_asset))
        .route("/tiles/{*path}", get(tile))
        .route("/api/health", get(health))
        .route("/api/status", get(status))
        .route("/api/snapshot", get(snapshot))
        .route("/api/events", get(events))
        .route("/api/command", post(command))
        .route("/api/audio/{id}", get(audio_clip))
        .route("/api/file", get(audio_file))
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
            // Connect-info gives the auth extractor the caller's address, which
            // the same-account tailnet trust path needs.
            if let Err(e) = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            {
                eprintln!("[web] server error: {e}");
            }
        });
    });
}

async fn mobile_page() -> impl IntoResponse {
    axum::response::Html(MOBILE_HTML)
}

// ---------------------------------------------------------------------------
// The desktop page itself, served for remote use. `shim.js` stands in for
// Tauri's IPC: `invoke` becomes `/api/command`, `listen` the SSE feed, and
// audio plays in the browser. In a debug build the files are read from the
// source tree so edits show up without a rebuild; release builds carry them.

const SHIM_JS: &str = include_str!("shim.js");

const DIST: &[(&str, &str, &str)] = &[
    ("index.html", "text/html; charset=utf-8", include_str!("../../dist/index.html")),
    ("app.js", "text/javascript; charset=utf-8", include_str!("../../dist/app.js")),
    ("style.css", "text/css; charset=utf-8", include_str!("../../dist/style.css")),
    ("favicon.svg", "image/svg+xml", include_str!("../../dist/favicon.svg")),
    ("vendor/leaflet.js", "text/javascript; charset=utf-8", include_str!("../../dist/vendor/leaflet.js")),
    ("vendor/leaflet.css", "text/css; charset=utf-8", include_str!("../../dist/vendor/leaflet.css")),
];

fn dist_file(name: &str) -> Option<(String, &'static str)> {
    let (_, mime, embedded) = DIST.iter().find(|(n, _, _)| *n == name)?;
    #[cfg(debug_assertions)]
    {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("dist").join(name);
        if let Ok(t) = std::fs::read_to_string(p) {
            return Some((t, mime));
        }
    }
    Some((embedded.to_string(), mime))
}

/// The desktop page with the IPC shim loaded ahead of the app script.
pub fn desktop_html() -> String {
    let (html, _) = dist_file("index.html").unwrap_or_default();
    let tag = "<script src=\"vendor/leaflet.js\"></script>";
    match html.find(tag) {
        Some(i) => format!("{}<script src=\"shim.js\"></script>\n  {}", &html[..i], &html[i..]),
        None => html.replace("<script src=\"app.js\"></script>", "<script src=\"shim.js\"></script><script src=\"app.js\"></script>"),
    }
}

async fn desktop_redirect() -> impl IntoResponse {
    axum::response::Redirect::permanent("/desktop/")
}

async fn desktop_index() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], desktop_html())
}

async fn desktop_asset(Path(path): Path<String>) -> axum::response::Response {
    if path == "shim.js" {
        return ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], SHIM_JS).into_response();
    }
    if path == "index.html" {
        return desktop_index().await.into_response();
    }
    match dist_file(&path) {
        Some((body, mime)) => ([(header::CONTENT_TYPE, mime)], body).into_response(),
        None => (StatusCode::NOT_FOUND, "no such asset").into_response(),
    }
}

/// Map tiles for the remote desktop, from the same on-disk cache the
/// `tiles://` scheme uses. Tiles are public data, so no token is needed.
async fn tile(State(st): State<Arc<WebState>>, Path(path): Path<String>) -> axum::response::Response {
    let app = st.app.clone();
    match tokio::task::spawn_blocking(move || crate::tiles::fetch(&app, &path)).await {
        Ok(Ok(png)) => (
            [
                (header::CONTENT_TYPE, "image/png"),
                (header::CACHE_CONTROL, "public, max-age=604800"),
            ],
            png,
        )
            .into_response(),
        Ok(Err(e)) => (StatusCode::NOT_FOUND, e).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

fn audio_mime(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("wav") => "audio/wav",
        Some("m4a") | Some("mp4") => "audio/mp4",
        Some("mp3") => "audio/mpeg",
        Some("ogg") | Some("opus") => "audio/ogg",
        Some("flac") => "audio/flac",
        _ => "application/octet-stream",
    }
}

async fn serve_audio(path: std::path::PathBuf) -> axum::response::Response {
    match tokio::fs::read(&path).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, audio_mime(&path))], bytes).into_response(),
        Err(e) => (StatusCode::NOT_FOUND, format!("{}: {e}", path.display())).into_response(),
    }
}

/// A library call's audio file, so the remote page can play it locally
/// (the desktop's `library_play` plays through the far machine's speakers).
async fn audio_clip(State(st): State<Arc<WebState>>, _auth: Auth, Path(id): Path<i64>) -> axum::response::Response {
    let app = st.app.clone();
    let found = tokio::task::spawn_blocking(move || {
        let state = app.state::<AppState>();
        crate::with_db(&state, |c| crate::library::get(c, id))
    })
    .await;
    match found {
        Ok(Ok(Some(row))) => match row.audio {
            Some(p) => serve_audio(std::path::PathBuf::from(p)).await,
            None => (StatusCode::NOT_FOUND, "no audio for that call").into_response(),
        },
        Ok(Ok(None)) => (StatusCode::NOT_FOUND, "no such call").into_response(),
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, e).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct FileQuery {
    path: String,
}

/// Resolve `path` if it is somewhere the app writes audio: the library folder
/// or the app's own data/cache/config directories. Anything else is refused,
/// so the token cannot be used to read arbitrary files.
fn resolve_allowed_audio_path(app: &AppHandle, path: &std::path::Path) -> Option<std::path::PathBuf> {
    let real = path.canonicalize().ok()?;
    let state = app.state::<AppState>();
    let mut roots: Vec<std::path::PathBuf> = Vec::new();
    if let Some(d) = state.library_dir.lock().unwrap().clone() {
        roots.push(d);
    }
    for d in [
        app.path().app_data_dir().ok(),
        app.path().app_cache_dir().ok(),
        app.path().app_config_dir().ok(),
        app.path().app_local_data_dir().ok(),
    ]
    .into_iter()
    .flatten()
    {
        roots.push(d);
    }

    let allowed = roots
        .iter()
        .filter_map(|r| r.canonicalize().ok())
        .any(|r| real.starts_with(&r));

    if allowed { Some(real) } else { None }
}

/// An audio file by path (what the desktop's `play_wav` takes), limited to
/// the app's own folders.
async fn audio_file(State(st): State<Arc<WebState>>, _auth: Auth, Query(q): Query<FileQuery>) -> axum::response::Response {
    let path = std::path::PathBuf::from(crate::shellexpand_home(&q.path));
    let app = st.app.clone();
    let p = path.clone();
    let resolved = tokio::task::spawn_blocking(move || resolve_allowed_audio_path(&app, &p))
        .await
        .ok()
        .flatten();
    let Some(path) = resolved else {
        return (StatusCode::FORBIDDEN, "not an app audio file").into_response();
    };
    serve_audio(path).await
}

/// Unauthenticated identity, so another instance can tell this is
/// HoosierSDR (and not some other program on the port) before it has a
/// token. Nothing about the radio system is disclosed here.
async fn health(State(st): State<Arc<WebState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "app": "HoosierSDR",
        "version": env!("CARGO_PKG_VERSION"),
        "host": hostname(),
        "trust_tailnet": crate::remotes::trust_enabled(&st.app),
    }))
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
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
    let catalog_len = s.catalog.lock().unwrap().len();
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

/// The run in progress, for a page that opens mid-run: whether anything is
/// running, how it was started, and the frames that rebuild the tuning,
/// site, events and status panels. The page applies these before it starts
/// reading the live feed.
async fn snapshot(State(st): State<Arc<WebState>>, _auth: Auth) -> Json<serde_json::Value> {
    let s = st.app.state::<AppState>();
    let running = s.running.load(std::sync::atomic::Ordering::SeqCst);
    let start = s.last_start.lock().unwrap().clone();
    let events: Vec<serde_json::Value> = if running {
        st.snap
            .lock()
            .unwrap()
            .events()
            .into_iter()
            .map(|f| serde_json::json!({ "event": f.event, "data": f.data }))
            .collect()
    } else {
        Vec::new()
    };
    let runs = crate::runs_info(&s);
    Json(serde_json::json!({ "running": running, "start": start, "runs": runs, "events": events }))
}

/// Server-Sent Events: the live feed (grants, calls, notices, status, …) plus
/// `audio` frames carrying each completed call's PCM. One long-lived
/// connection the mobile UI opens and leaves open.
#[derive(Deserialize)]
struct EventsQuery {
    /// Send waterfall/constellation frames (`1`). Off by default: a phone
    /// never draws them and they are most of the bytes.
    #[serde(default)]
    spectrum: u8,
}

async fn events(
    State(st): State<Arc<WebState>>,
    _auth: Auth,
    Query(q): Query<EventsQuery>,
) -> Sse<impl futures::Stream<Item = Result<SseEvent, std::convert::Infallible>>> {
    let rx = st.frames.subscribe();
    let heavy = q.spectrum != 0;
    // `BroadcastStream` yields `Ok(frame)` per frame and `Err(Lagged)` when a
    // client falls behind; it ends on its own when the channel closes. We
    // surface a lag as a frame so the client knows it missed audio.
    let stream = BroadcastStream::new(rx).filter_map(move |item| async move {
        match item {
            Ok(frame) if !heavy && is_heavy(&frame) => None,
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
        if matches!(extract_token(parts), Some(t) if t == state.token) {
            return Ok(Auth);
        }
        // No (or wrong) token: a device on this Tailscale account may still
        // be let in when the user has turned that on. Loopback and LAN
        // addresses never qualify — see `remotes::peer_trusted`.
        let ip = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0.ip());
        if let Some(ip) = ip {
            let app = state.app.clone();
            let trusted = tokio::task::spawn_blocking(move || crate::remotes::peer_trusted(&app, ip))
                .await
                .unwrap_or(false);
            if trusted {
                return Ok(Auth);
            }
        }
        Err((StatusCode::UNAUTHORIZED, "invalid or missing token"))
    }
}
