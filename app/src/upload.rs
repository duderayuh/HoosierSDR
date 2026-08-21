//! Per-call sharing: rdio-scanner, OpenMHz and Broadcastify Calls, with the
//! exact form fields those servers read (taken from rdio-scanner's
//! `parsers.go`, trunk-server's `uploads.js` and Broadcastify's call-upload
//! article, cross-checked against trunk-recorder and SDRTrunk's uploaders).
//!
//! A worker thread takes each completed call with audio, transcodes once to
//! what each service accepts (rdio-scanner takes WAV; OpenMHz and
//! Broadcastify need m4a/mp3), and records the outcome per service in the
//! library so nothing is sent twice.

use serde::{Deserialize, Serialize};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Manager, State};

use crate::{library, AppState};

#[derive(Serialize, Deserialize, Clone, Default, PartialEq)]
pub struct Rdio {
    pub enabled: bool,
    /// Server base, e.g. http://192.168.1.10:3000 (the path is added).
    pub url: String,
    pub key: String,
    pub system: u32,
    pub system_label: String,
}

#[derive(Serialize, Deserialize, Clone, Default, PartialEq)]
pub struct OpenMhz {
    pub enabled: bool,
    /// Upload server, default https://api.openmhz.com
    pub url: String,
    pub short_name: String,
    pub api_key: String,
}

#[derive(Serialize, Deserialize, Clone, Default, PartialEq)]
pub struct BroadcastifyCalls {
    pub enabled: bool,
    pub api_key: String,
    pub system_id: u32,
    /// "m4a" or "mp3"
    pub format: String,
}

#[derive(Serialize, Deserialize, Clone, Default, PartialEq)]
pub struct Settings {
    pub rdio: Rdio,
    pub openmhz: OpenMhz,
    pub broadcastify: BroadcastifyCalls,
    /// Skip calls shorter than this (seconds).
    pub min_secs: f64,
}

/// What the worker needs about a call.
#[derive(Clone)]
pub struct Job {
    pub id: i64,
    pub audio: String,
    pub start: i64,
    pub secs: f64,
    pub tg: u16,
    pub tg_name: String,
    pub unit: u32,
    pub unit_name: Option<String>,
    pub freq_hz: u64,
    pub emergency: bool,
    pub patched_with: Vec<u16>,
    pub system: String,
}

#[derive(Serialize, Clone, Default)]
pub struct Status {
    pub queued: usize,
    pub sent: u64,
    pub failed: u64,
    pub last_error: Option<String>,
    pub last_ok: Option<String>,
}

pub struct Uploader {
    tx: Sender<Job>,
    pub status: Arc<Mutex<Status>>,
    pub settings: Arc<Mutex<Settings>>,
}

pub type Shared = Arc<Mutex<Option<Uploader>>>;

fn settings_path(app: &AppHandle) -> Option<std::path::PathBuf> {
    app.path()
        .app_config_dir()
        .ok()
        .map(|d| d.join("uploads.json"))
}

pub fn load_settings(app: &AppHandle) -> Settings {
    let mut s: Settings = settings_path(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    if s.openmhz.url.is_empty() {
        s.openmhz.url = "https://api.openmhz.com".into();
    }
    if s.broadcastify.format.is_empty() {
        s.broadcastify.format = "m4a".into();
    }
    s
}

// ---------------------------------------------------------------------------
// multipart
// ---------------------------------------------------------------------------

pub struct Multipart {
    boundary: String,
    body: Vec<u8>,
}

impl Multipart {
    pub fn new() -> Self {
        Self {
            boundary: format!(
                "----HoosierSDR{:x}",
                library::now() as u64 ^ 0x5eed_1234_abcd
            ),
            body: Vec::new(),
        }
    }
    pub fn text(mut self, name: &str, value: impl std::fmt::Display) -> Self {
        self.body.extend_from_slice(
            format!(
                "--{}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n",
                self.boundary
            )
            .as_bytes(),
        );
        self
    }
    pub fn file(mut self, name: &str, filename: &str, mime: &str, data: &[u8]) -> Self {
        self.body.extend_from_slice(
            format!(
                "--{}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: {mime}\r\n\r\n",
                self.boundary
            )
            .as_bytes(),
        );
        self.body.extend_from_slice(data);
        self.body.extend_from_slice(b"\r\n");
        self
    }
    pub fn finish(mut self) -> (String, Vec<u8>) {
        self.body
            .extend_from_slice(format!("--{}--\r\n", self.boundary).as_bytes());
        (
            format!("multipart/form-data; boundary={}", self.boundary),
            self.body,
        )
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(60)))
        .http_status_as_error(false)
        .build()
        .into()
}

fn post(url: &str, ctype: &str, body: Vec<u8>) -> Result<(u16, String), String> {
    let mut r = agent()
        .post(url)
        .header("Content-Type", ctype)
        .header("User-Agent", "HoosierSDR")
        .send(&body[..])
        .map_err(|e| e.to_string())?;
    let status = r.status().as_u16();
    let text = r.body_mut().read_to_string().unwrap_or_default();
    Ok((status, text))
}

fn mime_for(path: &str) -> &'static str {
    match std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
    {
        Some("m4a") => "audio/mp4",
        Some("mp3") => "audio/mpeg",
        Some("opus") => "audio/ogg",
        _ => "audio/wav",
    }
}

/// The file in the format a service wants, transcoding via ffmpeg if needed.
/// Returns (path, made_here) so temporaries can be removed.
fn audio_as(job: &Job, want: &[&str]) -> Result<(String, bool), String> {
    let ext = std::path::Path::new(&job.audio)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("wav");
    if want.contains(&ext) {
        return Ok((job.audio.clone(), false));
    }
    let f = crate::encode::Format {
        codec: want[0].to_string(),
        bitrate_kbps: 32,
        mode: "cbr".into(),
    };
    // Transcode from whatever we have; ffmpeg reads any of our formats.
    let out = std::path::Path::new(&job.audio).with_extension(format!("upload.{}", want[0]));
    let st = std::process::Command::new("ffmpeg")
        .args(["-y", "-loglevel", "error", "-i", &job.audio])
        .args(match f.codec.as_str() {
            "mp3" => vec![
                "-c:a",
                "libmp3lame",
                "-b:a",
                "32k",
                "-ar",
                "22050",
                "-ac",
                "1",
            ],
            _ => vec![
                "-c:a",
                "aac",
                "-b:a",
                "32k",
                "-ar",
                "16000",
                "-ac",
                "1",
                "-movflags",
                "+faststart",
            ],
        })
        .arg(&out)
        .output()
        .map_err(|e| format!("ffmpeg: {e}"))?;
    if !st.status.success() {
        return Err(format!(
            "ffmpeg: {}",
            String::from_utf8_lossy(&st.stderr).trim()
        ));
    }
    Ok((out.to_string_lossy().into_owned(), true))
}

fn src_list(job: &Job) -> String {
    serde_json::json!([{ "pos": 0.0, "src": job.unit, "tag": job.unit_name.clone().unwrap_or_default() }]).to_string()
}

// ---------------------------------------------------------------------------
// services
// ---------------------------------------------------------------------------

/// rdio-scanner: POST {url}/api/call-upload, multipart; accepts WAV.
pub fn send_rdio(cfg: &Rdio, job: &Job, base_override: Option<&str>) -> Result<String, String> {
    let base = base_override.unwrap_or(cfg.url.trim_end_matches('/'));
    let data = std::fs::read(&job.audio).map_err(|e| e.to_string())?;
    let name = std::path::Path::new(&job.audio)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "call.wav".into());
    let mut m = Multipart::new()
        .text("key", &cfg.key)
        .text("system", cfg.system)
        .text(
            "systemLabel",
            if cfg.system_label.is_empty() {
                &job.system
            } else {
                &cfg.system_label
            },
        )
        .text("dateTime", job.start)
        .text("frequency", job.freq_hz)
        .text(
            "frequencies",
            serde_json::json!([{ "freq": job.freq_hz, "pos": 0.0, "len": job.secs }]).to_string(),
        )
        .text("talkgroup", job.tg)
        .text("talkgroupLabel", &job.tg_name)
        .text("talkgroupName", &job.tg_name)
        .text("source", job.unit)
        .text("sources", src_list(job))
        .file("audio", &name, mime_for(&job.audio), &data)
        .text("audioName", &name)
        .text("audioType", mime_for(&job.audio));
    if !job.patched_with.is_empty() {
        m = m.text("patches", serde_json::json!(job.patched_with).to_string());
    }
    let (ctype, body) = m.finish();
    let (status, text) = post(&format!("{base}/api/call-upload"), &ctype, body)?;
    if (200..300).contains(&status) {
        Ok(text.trim().to_string())
    } else {
        Err(format!("HTTP {status}: {}", text.trim()))
    }
}

/// OpenMHz: POST {url}/{short}/upload, multipart; file must be .m4a or .mp3.
pub fn send_openmhz(
    cfg: &OpenMhz,
    job: &Job,
    base_override: Option<&str>,
) -> Result<String, String> {
    let base = base_override.unwrap_or(cfg.url.trim_end_matches('/'));
    let (path, tmp) = audio_as(job, &["m4a", "mp3"])?;
    let data = std::fs::read(&path).map_err(|e| e.to_string())?;
    let ext = std::path::Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("m4a");
    let m = Multipart::new()
        .text("freq", job.freq_hz)
        .text("start_time", job.start)
        .text("stop_time", job.start + job.secs.ceil() as i64)
        .text("call_length", job.secs.round() as i64)
        .text("talkgroup_num", job.tg)
        .text("emergency", u8::from(job.emergency))
        .text("api_key", &cfg.api_key)
        .text("source_list", src_list(job))
        .text(
            "patch_list",
            serde_json::json!(job.patched_with).to_string(),
        )
        .file(
            "call",
            &format!("call.{ext}"),
            "application/octet-stream",
            &data,
        );
    let (ctype, body) = m.finish();
    let r = post(&format!("{base}/{}/upload", cfg.short_name), &ctype, body);
    if tmp {
        let _ = std::fs::remove_file(&path);
    }
    let (status, text) = r?;
    if status == 200 {
        Ok("uploaded".into())
    } else {
        Err(format!("HTTP {status}: {}", text.trim()))
    }
}

/// Broadcastify Calls: POST metadata → "0 <url>" → PUT the file there.
pub fn send_broadcastify(
    cfg: &BroadcastifyCalls,
    job: &Job,
    base_override: Option<&str>,
) -> Result<String, String> {
    let want = if cfg.format == "mp3" { "mp3" } else { "m4a" };
    let (path, tmp) = audio_as(job, &[want])?;
    let data = std::fs::read(&path).map_err(|e| e.to_string())?;
    if tmp {
        let _ = std::fs::remove_file(&path);
    }
    let m = Multipart::new()
        .text("apiKey", &cfg.api_key)
        .text("systemId", cfg.system_id)
        .text("callDuration", format!("{:.1}", job.secs))
        .text("ts", job.start)
        .text("tg", job.tg)
        .text("src", job.unit)
        .text("freq", format!("{:.6}", job.freq_hz as f64 / 1e6))
        .text("enc", want);
    let (ctype, body) = m.finish();
    let url = base_override.unwrap_or("https://api.broadcastify.com/call-upload");
    let (status, text) = post(url, &ctype, body)?;
    let text = text.trim().to_string();
    if status != 200 {
        return Err(format!("HTTP {status}: {text}"));
    }
    if let Some(rest) = text.strip_prefix("0 ") {
        let put = agent()
            .put(rest.trim())
            .header(
                "Content-Type",
                if want == "mp3" {
                    "audio/mpeg"
                } else {
                    "audio/aac"
                },
            )
            .send(&data[..])
            .map_err(|e| e.to_string())?;
        let code = put.status().as_u16();
        if (200..300).contains(&code) {
            Ok("uploaded".into())
        } else {
            Err(format!("PUT HTTP {code}"))
        }
    } else if text.starts_with("1 SKIPPED") {
        Ok("skipped (already received)".into())
    } else {
        Err(text)
    }
}

// ---------------------------------------------------------------------------
// worker + state
// ---------------------------------------------------------------------------

fn record(app: &AppHandle, id: i64, service: &str, ok: bool, msg: &str) {
    let state = app.state::<AppState>();
    let guard = state.db.lock().unwrap();
    if let Some(db) = guard.as_ref() {
        let c = db.lock().unwrap();
        let _ = c.execute(
            "INSERT INTO uploads (call_id, service, ok, at, msg) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![id, service, ok, library::now(), msg],
        );
    }
}

fn already(app: &AppHandle, id: i64, service: &str) -> bool {
    let state = app.state::<AppState>();
    let guard = state.db.lock().unwrap();
    let Some(db) = guard.as_ref() else {
        return false;
    };
    let c = db.lock().unwrap();
    c.query_row(
        "SELECT COUNT(*) FROM uploads WHERE call_id = ?1 AND service = ?2 AND ok = 1",
        rusqlite::params![id, service],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}

pub fn ensure_schema(c: &rusqlite::Connection) {
    let _ = c.execute_batch(
        "CREATE TABLE IF NOT EXISTS uploads (id INTEGER PRIMARY KEY, call_id INTEGER NOT NULL, service TEXT NOT NULL, ok INTEGER NOT NULL, at INTEGER NOT NULL, msg TEXT);
         CREATE INDEX IF NOT EXISTS uploads_call ON uploads(call_id, service);",
    );
}

pub fn start(app: AppHandle, settings: Settings) -> Uploader {
    let (tx, rx) = channel::<Job>();
    let status = Arc::new(Mutex::new(Status::default()));
    let shared = Arc::new(Mutex::new(settings));
    let (st, se) = (Arc::clone(&status), Arc::clone(&shared));
    std::thread::spawn(move || {
        while let Ok(job) = rx.recv() {
            let s = se.lock().unwrap().clone();
            {
                let mut q = st.lock().unwrap();
                q.queued = q.queued.saturating_sub(1);
            }
            if job.secs < s.min_secs {
                continue;
            }
            type Sender<'a> = Box<dyn Fn() -> Result<String, String> + 'a>;
            let services: Vec<(&str, Sender<'_>)> = vec![
                ("rdio-scanner", Box::new(|| send_rdio(&s.rdio, &job, None))),
                ("openmhz", Box::new(|| send_openmhz(&s.openmhz, &job, None))),
                (
                    "broadcastify",
                    Box::new(|| send_broadcastify(&s.broadcastify, &job, None)),
                ),
            ];
            let enabled = [s.rdio.enabled, s.openmhz.enabled, s.broadcastify.enabled];
            for (i, (name, f)) in services.iter().enumerate() {
                if !enabled[i] || already(&app, job.id, name) {
                    continue;
                }
                let mut result = f();
                if result.is_err() {
                    std::thread::sleep(Duration::from_secs(5));
                    result = f();
                }
                let mut q = st.lock().unwrap();
                match &result {
                    Ok(m) => {
                        q.sent += 1;
                        q.last_ok = Some(format!("{name}: {m}"));
                    }
                    Err(e) => {
                        q.failed += 1;
                        q.last_error = Some(format!("{name}: {e}"));
                    }
                }
                record(
                    &app,
                    job.id,
                    name,
                    result.is_ok(),
                    &result.clone().unwrap_or_else(|e| e),
                );
            }
        }
    });
    Uploader {
        tx,
        status,
        settings: shared,
    }
}

impl Uploader {
    pub fn submit(&self, job: Job) {
        self.status.lock().unwrap().queued += 1;
        let _ = self.tx.send(job);
    }
}

#[derive(Serialize)]
pub struct View {
    settings: Settings,
    status: Status,
    ffmpeg: bool,
}

#[tauri::command]
pub fn uploads_get(app: AppHandle, state: State<AppState>) -> View {
    let guard = state.uploader.lock().unwrap();
    let (settings, status) = match guard.as_ref() {
        Some(u) => (
            u.settings.lock().unwrap().clone(),
            u.status.lock().unwrap().clone(),
        ),
        None => (load_settings(&app), Status::default()),
    };
    View {
        settings,
        status,
        ffmpeg: crate::encode::ffmpeg_available().is_some(),
    }
}

#[tauri::command]
pub fn uploads_configure(
    app: AppHandle,
    state: State<AppState>,
    settings: Settings,
) -> Result<(), String> {
    if let Some(p) = settings_path(&app) {
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        std::fs::write(
            &p,
            serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())?,
        )
        .map_err(|e| format!("{}: {e}", p.display()))?;
    }
    let mut guard = state.uploader.lock().unwrap();
    match guard.as_ref() {
        Some(u) => *u.settings.lock().unwrap() = settings,
        None => *guard = Some(start(app.clone(), settings)),
    }
    Ok(())
}

/// Credential checks: rdio-scanner's `test=1`, OpenMHz's `/authorize`,
/// Broadcastify's `test=1`.
#[tauri::command]
pub fn uploads_test(service: String, settings: Settings) -> Result<String, String> {
    match service.as_str() {
        "rdio" => {
            let (ctype, body) = Multipart::new()
                .text("key", &settings.rdio.key)
                .text("system", settings.rdio.system)
                .text("test", 1)
                .finish();
            let (status, text) = post(
                &format!(
                    "{}/api/call-upload",
                    settings.rdio.url.trim_end_matches('/')
                ),
                &ctype,
                body,
            )?;
            if text.to_lowercase().contains("incomplete call data") || (200..300).contains(&status)
            {
                Ok("rdio-scanner accepted the key".into())
            } else {
                Err(format!("HTTP {status}: {}", text.trim()))
            }
        }
        "openmhz" => {
            let (status, text) = post(
                &format!(
                    "{}/{}/authorize",
                    settings.openmhz.url.trim_end_matches('/'),
                    settings.openmhz.short_name
                ),
                "application/x-www-form-urlencoded",
                format!("api_key={}", settings.openmhz.api_key).into_bytes(),
            )?;
            if status == 200 {
                Ok("OpenMHz accepted the key".into())
            } else {
                Err(format!("HTTP {status}: {}", text.trim()))
            }
        }
        _ => {
            let (ctype, body) = Multipart::new()
                .text("apiKey", &settings.broadcastify.api_key)
                .text("systemId", settings.broadcastify.system_id)
                .text("test", 1)
                .finish();
            let (status, text) = post("https://api.broadcastify.com/call-upload", &ctype, body)?;
            if status == 200 && text.trim_start().starts_with("ok") {
                Ok("Broadcastify Calls accepted the key".into())
            } else {
                Err(format!("HTTP {status}: {}", text.trim()))
            }
        }
    }
}

/// Re-send one call to every enabled service (ignoring the sent record).
#[tauri::command]
pub fn upload_call(app: AppHandle, state: State<AppState>, id: i64) -> Result<(), String> {
    let row = {
        let guard = state.db.lock().unwrap();
        let db = guard.as_ref().ok_or("library not open")?;
        let c = db.lock().unwrap();
        library::get(&c, id)?.ok_or("no such call")?
    };
    let audio = row.audio.clone().ok_or("no audio for that call")?;
    {
        let guard = state.db.lock().unwrap();
        if let Some(db) = guard.as_ref() {
            let _ = db.lock().unwrap().execute(
                "DELETE FROM uploads WHERE call_id = ?1",
                rusqlite::params![id],
            );
        }
    }
    let job = Job {
        id: row.id,
        audio,
        start: row.start,
        secs: row.secs,
        tg: row.tg,
        tg_name: row.tg_name,
        unit: row.unit,
        unit_name: row.unit_name,
        freq_hz: row.freq_hz,
        emergency: row.emergency,
        patched_with: row.patched_with,
        system: row.system,
    };
    let mut guard = state.uploader.lock().unwrap();
    if guard.is_none() {
        *guard = Some(start(app.clone(), load_settings(&app)));
    }
    guard.as_ref().unwrap().submit(job);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// A one-shot HTTP server: captures the request, answers `reply`.
    fn serve(reply: &'static str) -> (String, std::thread::JoinHandle<Vec<u8>>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = format!("http://{}", l.local_addr().unwrap());
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 65536];
            // Read headers + body (Content-Length).
            loop {
                let n = s.read(&mut tmp).unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(hend) = text.find("\r\n\r\n") {
                    let cl = text.lines().find_map(|l| {
                        l.to_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap())
                    });
                    if let Some(cl) = cl {
                        if buf.len() >= hend + 4 + cl {
                            break;
                        }
                    }
                }
            }
            let _ = s.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                )
                .as_bytes(),
            );
            buf
        });
        (addr, h)
    }

    fn job(dir: &std::path::Path) -> Job {
        let pcm: Vec<i16> = (0..8000).map(|i| (i % 200) as i16).collect();
        let wav = dir.join("u.wav");
        hs_core::wav::write_wav(wav.to_str().unwrap(), 8000, &pcm).unwrap();
        Job {
            id: 1,
            audio: wav.to_string_lossy().into_owned(),
            start: 1_700_000_000,
            secs: 1.0,
            tg: 10103,
            tg_name: "IMPD North".into(),
            unit: 4900165,
            unit_name: Some("Car 12".into()),
            freq_hz: 857_387_500,
            emergency: false,
            patched_with: vec![],
            system: "SAFE-T".into(),
        }
    }

    #[test]
    fn rdio_scanner_request_carries_the_documented_fields() {
        let dir = std::env::temp_dir().join(format!("hs_up_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (addr, h) = serve("Call imported successfully.\n");
        let cfg = Rdio {
            enabled: true,
            url: String::new(),
            key: "K".into(),
            system: 7,
            system_label: "Lab".into(),
        };
        let r = send_rdio(&cfg, &job(&dir), Some(&addr)).unwrap();
        assert_eq!(r, "Call imported successfully.");
        let req = String::from_utf8_lossy(&h.join().unwrap()).to_string();
        assert!(req.starts_with("POST /api/call-upload HTTP/1.1"));
        for f in [
            "name=\"key\"\r\n\r\nK",
            "name=\"system\"\r\n\r\n7",
            "name=\"dateTime\"\r\n\r\n1700000000",
            "name=\"talkgroup\"\r\n\r\n10103",
            "name=\"frequency\"\r\n\r\n857387500",
            "name=\"talkgroupLabel\"\r\n\r\nIMPD North",
            "name=\"audio\"; filename=\"u.wav\"",
            "Content-Type: audio/wav",
            "name=\"sources\"",
        ] {
            assert!(req.contains(f), "missing {f}");
        }
    }

    #[test]
    fn openmhz_request_sends_an_m4a_when_ffmpeg_exists() {
        if crate::encode::ffmpeg_available().is_none() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("hs_up2_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (addr, h) = serve("");
        let cfg = OpenMhz {
            enabled: true,
            url: String::new(),
            short_name: "hoosier".into(),
            api_key: "A".into(),
        };
        send_openmhz(&cfg, &job(&dir), Some(&addr)).unwrap();
        let req = String::from_utf8_lossy(&h.join().unwrap()).to_string();
        assert!(req.starts_with("POST /hoosier/upload HTTP/1.1"));
        for f in [
            "name=\"api_key\"\r\n\r\nA",
            "name=\"talkgroup_num\"\r\n\r\n10103",
            "name=\"start_time\"\r\n\r\n1700000000",
            "name=\"call\"; filename=\"call.m4a\"",
            "name=\"source_list\"",
        ] {
            assert!(req.contains(f), "missing {f}");
        }
    }

    #[test]
    fn broadcastify_two_step_flow() {
        if crate::encode::ffmpeg_available().is_none() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("hs_up3_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Step 2 target first, so step 1 can point at it.
        let (put_addr, put_h) = serve("");
        let reply: &'static str = Box::leak(format!("0 {put_addr}/upload/abc").into_boxed_str());
        let (addr, h) = serve(reply);
        let cfg = BroadcastifyCalls {
            enabled: true,
            api_key: "B".into(),
            system_id: 42,
            format: "m4a".into(),
        };
        send_broadcastify(&cfg, &job(&dir), Some(&addr)).unwrap();
        let req = String::from_utf8_lossy(&h.join().unwrap()).to_string();
        for f in [
            "name=\"apiKey\"\r\n\r\nB",
            "name=\"systemId\"\r\n\r\n42",
            "name=\"ts\"\r\n\r\n1700000000",
            "name=\"freq\"\r\n\r\n857.387500",
            "name=\"enc\"\r\n\r\nm4a",
        ] {
            assert!(req.contains(f), "missing {f}");
        }
        let put = String::from_utf8_lossy(&put_h.join().unwrap()).to_string();
        assert!(put.starts_with("PUT /upload/abc HTTP/1.1"));
        assert!(put.to_lowercase().contains("content-type: audio/aac"));
    }
}
