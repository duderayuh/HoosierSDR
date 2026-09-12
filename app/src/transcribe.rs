//! Transcription: a persistent Python worker (faster-whisper or
//! openai-whisper) fed one call at a time over JSON lines. Results land in
//! the library as the *machine* transcript; edits live beside it.

use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use std::collections::HashMap;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::{library, AppState};

#[derive(Serialize, Deserialize, Clone)]
pub struct Settings {
    pub enabled: bool,
    pub engine: String,
    pub model: String,
    pub language: String,
    pub device: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            engine: "faster-whisper".into(),
            model: "base".into(),
            language: "en".into(),
            device: "auto".into(),
        }
    }
}

#[derive(Default)]
pub struct Worker {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    pub model: String,
    pub settings: Settings,
    pub busy: Arc<AtomicBool>,
    pub last_error: Option<String>,
    /// Bumped on every start; a reader thread only tears down its own child.
    generation: u64,
    /// Calls asked for explicitly (Transcribe now), served before the pump's.
    pub wanted: std::collections::VecDeque<i64>,
    /// The call the worker is on, while `busy`.
    job: Option<Job>,
    /// Jobs handed to the current worker process; its first also pays for
    /// loading (or, on first use, downloading) the model.
    jobs_started: u32,
    /// A worker has answered a job this run, so the model is on disk and a
    /// replacement only has to load it.
    model_ready: bool,
}

/// One call handed to the worker, and how long it may take.
struct Job {
    id: i64,
    since: std::time::Instant,
    limit: std::time::Duration,
}

/// Time allowed for the first job of the run: loading the model, or fetching
/// it on first use, comes before the transcription itself.
const FIRST_JOB_SECS: u64 = 600;
/// A replacement worker's first job: the weights are cached by then, so
/// loading takes seconds — without this, a restart after a stall followed by
/// another garbled clip stalled the queue for the full first-use allowance.
const RELOAD_JOB_SECS: u64 = 120;
/// Past the worker's own limit, how long before the app stops waiting and
/// replaces a worker that has gone silent.
const BACKSTOP_GRACE_SECS: u64 = 30;

/// The worker's limit for one clip. Every engine here runs well under real
/// time (mlx-whisper ~0.3 s for a 21 s call, faster-whisper on the CPU ~5 s),
/// so this only ends a job that has stopped making progress — which Whisper
/// can do on a garbled clip, and which stalled the whole queue behind it.
fn job_timeout_secs(first: bool, model_ready: bool, clip_secs: f64) -> u64 {
    let run = 30 + (clip_secs.max(0.0) * 4.0).ceil() as u64;
    match (first, model_ready) {
        (false, _) => run,
        (true, true) => run.max(RELOAD_JOB_SECS),
        (true, false) => run.max(FIRST_JOB_SECS),
    }
}

pub type Shared = Arc<Mutex<Worker>>;

/// Which importable module backs each engine — used to check that a Python
/// interpreter can actually run the engine before we hand it work.
fn engine_module(engine: &str) -> &'static str {
    match engine {
        "openai-whisper" => "whisper",
        "mlx-whisper" => "mlx_whisper",
        _ => "faster_whisper",
    }
}

fn python_cache() -> &'static Mutex<HashMap<String, String>> {
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Resolve a Python interpreter that can import `engine`'s module.
///
/// `Command::new("python3")` breaks for a Dock-launched .app on macOS: the app
/// inherits a minimal `PATH` (`/usr/bin:/bin:…`), where `python3` is Apple's
/// CommandLineTools 3.9 — which has no faster-whisper / whisper. Prefer real
/// installs (Homebrew, python.org framework) and confirm each by importing the
/// module before using it. `TRANSCRIBE_PYTHON` overrides everything for pinning.
fn find_python(engine: &str) -> String {
    if let Ok(p) = std::env::var("TRANSCRIBE_PYTHON") {
        let p = p.trim().to_string();
        if !p.is_empty() {
            return p;
        }
    }
    let module = engine_module(engine);
    if let Some(hit) = python_cache().lock().unwrap().get(module).cloned() {
        return hit;
    }
    let candidates = [
        "/usr/local/bin/python3",
        "/usr/local/bin/python3.13",
        "/usr/local/bin/python3.12",
        "/usr/local/bin/python3.11",
        "/opt/homebrew/bin/python3",
        "/opt/homebrew/bin/python3.13",
        "/opt/homebrew/bin/python3.12",
        "/opt/homebrew/bin/python3.11",
        "/Library/Frameworks/Python.framework/Versions/3.11/bin/python3",
        "python3",
    ];
    let mut found = "python3".to_string();
    for py in candidates {
        let probe = format!(
            "import importlib.util, sys; sys.exit(0 if importlib.util.find_spec('{module}') else 1)"
        );
        let ok = Command::new(py)
            .arg("-c")
            .arg(&probe)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            found = py.to_string();
            break;
        }
    }
    python_cache()
        .lock()
        .unwrap()
        .insert(module.to_string(), found.clone());
    found
}

fn script_path(app: &AppHandle) -> std::path::PathBuf {
    // Dev: beside the crate. Bundled: resource dir.
    let dev = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/transcribe.py");
    if dev.exists() {
        return dev;
    }
    app.path()
        .resource_dir()
        .map(|d| d.join("scripts/transcribe.py"))
        .unwrap_or(dev)
}

fn settings_path(app: &AppHandle) -> Option<std::path::PathBuf> {
    app.path()
        .app_config_dir()
        .ok()
        .map(|d| d.join("transcribe.json"))
}

pub fn load_settings(app: &AppHandle) -> Settings {
    settings_path(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

#[derive(Serialize)]
pub struct Probe {
    pub engines: Vec<String>,
    pub python: Option<String>,
    pub settings: Settings,
    pub running_model: Option<String>,
    pub last_error: Option<String>,
}

/// Which engines this machine has, and the saved settings.
#[tauri::command]
pub async fn transcribe_probe(app: AppHandle) -> Probe {
    tauri::async_runtime::spawn_blocking(move || transcribe_probe_blocking(app))
        .await
        .unwrap_or_else(|_| Probe {
            engines: vec![],
            python: None,
            settings: Settings::default(),
            running_model: None,
            last_error: Some("probe failed".into()),
        })
}

fn transcribe_probe_blocking(app: AppHandle) -> Probe {
    let state = app.state::<AppState>();
    let out = Command::new(find_python("faster-whisper"))
        .arg(script_path(&app))
        .arg("--probe")
        .output()
        .ok();
    let (engines, python) = out
        .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok())
        .map(|v| {
            (
                v["engines"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|e| e.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default(),
                v["python"].as_str().map(String::from),
            )
        })
        .unwrap_or_default();
    let w = state.transcriber.lock().unwrap();
    Probe {
        engines,
        python,
        settings: w.settings.clone(),
        running_model: w.child.as_ref().map(|_| w.model.clone()),
        last_error: w.last_error.clone(),
    }
}

#[tauri::command]
pub fn transcribe_configure(
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
    let mut w = state.transcriber.lock().unwrap();
    let restart = w.settings.engine != settings.engine
        || w.settings.model != settings.model
        || w.settings.language != settings.language
        || w.settings.device != settings.device;
    w.settings = settings;
    if restart {
        stop(&mut w);
    }
    Ok(())
}

fn stop(w: &mut Worker) {
    w.stdin = None;
    if let Some(mut c) = w.child.take() {
        let _ = c.kill();
        let _ = c.wait();
    }
    w.busy.store(false, Ordering::SeqCst);
}

/// Start the worker if it isn't running; returns false (with the error kept)
/// if it could not start.
fn ensure_started(app: &AppHandle, shared: &Shared) -> bool {
    let mut w = shared.lock().unwrap();
    if w.child.is_some() {
        return true;
    }
    let s = w.settings.clone();
    // Below the decoder and the audio thread in scheduling priority: a
    // transcription is a burst of many-core work the moment a call ends, and
    // it must never starve the radio stream that is decoding the next one.
    let py = find_python(&s.engine);
    let mut cmd = if cfg!(unix) {
        let mut c = Command::new("nice");
        c.args(["-n", "10", &py]);
        c
    } else {
        Command::new(&py)
    };
    let mut child = match cmd
        .arg(script_path(app))
        .args([
            "--engine",
            &s.engine,
            "--model",
            &s.model,
            "--language",
            &s.language,
            "--device",
            &s.device,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            w.last_error = Some(format!("python3: {e}"));
            return false;
        }
    };
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    w.child = Some(child);
    w.stdin = stdin;
    w.model = format!("{}/{}", s.engine, s.model);
    w.generation += 1;
    w.jobs_started = 0;
    w.job = None;
    let my_gen = w.generation;
    drop(w);

    // Reader thread: results → library + event.
    let app2 = app.clone();
    let shared2 = Arc::clone(shared);
    if let Some(out) = stdout {
        std::thread::spawn(move || {
            let state = app2.state::<AppState>();
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                if let Some(f) = v.get("fatal").and_then(|f| f.as_str()) {
                    let mut w = shared2.lock().unwrap();
                    w.last_error = Some(f.to_string());
                    if w.generation == my_gen {
                        stop(&mut w);
                    }
                    let _ = app2.emit("transcribe_error", f.to_string());
                    break;
                }
                if v.get("ready").is_some() {
                    let _ = app2.emit("transcribe_ready", v["model"].as_str().unwrap_or(""));
                    continue;
                }
                let id = v["id"].as_i64();
                if let (Some(id), Some(text)) = (id, v["text"].as_str()) {
                    let model = v["model"].as_str().unwrap_or("whisper");
                    // Transcript corrections: each call gets the global rules
                    // plus any rules that name its talkgroup.
                    let corrected = {
                        let guard = state.db.lock().unwrap();
                        let tg = guard.as_ref().and_then(|db| {
                            library::get(&db.lock().unwrap(), id)
                                .ok()
                                .flatten()
                                .map(|r| r.tg)
                        });
                        let corr = state.corrections.lock().unwrap();
                        let rules: Vec<(String, String)> = corr
                            .iter()
                            .filter(|(rt, _, _)| *rt == tg || rt.is_none())
                            .map(|(_, a, b)| (a.clone(), b.clone()))
                            .collect();
                        crate::apply_corrections(&rules, text)
                    };
                    let res = {
                        let guard = state.db.lock().unwrap();
                        match guard.as_ref() {
                            Some(db) => {
                                library::set_transcript(&db.lock().unwrap(), id, &corrected, model)
                            }
                            None => Err("library not open".into()),
                        }
                    };
                    if res.is_ok() {
                        let _ = app2.emit(
                            "transcript",
                            serde_json::json!({ "id": id, "text": corrected, "model": model }),
                        );
                        crate::alerts::on_transcript(&app2, id, &corrected);
                        crate::conversations::on_transcript(&app2, id, &corrected);
                        crate::analyzers::on_transcript(&app2, id, &corrected);
                        crate::dispatch::on_transcript(&app2, id, &corrected);
                    }
                } else if let Some(err) = v["error"].as_str() {
                    // A decode/transcribe failure on this file is terminal: mark
                    // it so the pump does not retry the same corrupt call forever
                    // and stall the whole queue behind it.
                    let label = match id {
                        Some(id) => {
                            let _ = {
                                let guard = state.db.lock().unwrap();
                                match guard.as_ref() {
                                    Some(db) => library::set_transcript(
                                        &db.lock().unwrap(),
                                        id,
                                        "",
                                        "transcribe-error",
                                    ),
                                    None => Err("library not open".into()),
                                }
                            };
                            id.to_string()
                        }
                        None => "?".to_string(),
                    };
                    let _ = app2.emit("transcribe_error", format!("call {label}: {err}"));
                }
                let mut w = shared2.lock().unwrap();
                if w.generation == my_gen {
                    w.job = None;
                }
                if id.is_some() {
                    w.model_ready = true;
                }
                w.busy.store(false, Ordering::SeqCst);
            }
            // Worker ended — but only tear down if it is still ours; a
            // replacement may already be running.
            let mut w = shared2.lock().unwrap();
            if w.generation == my_gen {
                stop(&mut w);
            }
        });
    }
    true
}

fn submit(shared: &Shared, id: i64, path: &str, clip_secs: f64) -> bool {
    let mut w = shared.lock().unwrap();
    if w.busy.load(Ordering::SeqCst) {
        return false;
    }
    let timeout = job_timeout_secs(w.jobs_started == 0, w.model_ready, clip_secs);
    let Some(stdin) = w.stdin.as_mut() else {
        return false;
    };
    let line = serde_json::json!({ "id": id, "path": path, "timeout": timeout }).to_string() + "\n";
    if stdin.write_all(line.as_bytes()).is_err() {
        stop(&mut w);
        return false;
    }
    w.jobs_started += 1;
    w.job = Some(Job {
        id,
        since: std::time::Instant::now(),
        limit: std::time::Duration::from_secs(timeout + BACKSTOP_GRACE_SECS),
    });
    w.busy.store(true, Ordering::SeqCst);
    true
}

/// The backstop: a worker that has not answered well past its own limit is
/// wedged somewhere the alarm cannot reach. Replace it, and mark its call so
/// the queue moves on instead of feeding it the same clip again.
fn check_stalled(app: &AppHandle) {
    let state = app.state::<AppState>();
    let stalled = {
        let mut w = state.transcriber.lock().unwrap();
        let over = w
            .job
            .as_ref()
            .filter(|j| w.busy.load(Ordering::SeqCst) && j.since.elapsed() > j.limit)
            .map(|j| (j.id, j.since.elapsed().as_secs()));
        if over.is_some() {
            w.job = None;
            stop(&mut w);
        }
        over
    };
    let Some((id, secs)) = stalled else { return };
    if let Some(db) = state.db.lock().unwrap().clone() {
        let _ = library::set_transcript(&db.lock().unwrap(), id, "", "transcribe-timeout");
    }
    let msg = format!("call {id}: no answer after {secs} s — restarted the transcriber");
    eprintln!("transcribe: {msg}");
    let _ = app.emit("transcribe_error", msg);
}

#[derive(Serialize)]
pub struct ModelInfo {
    pub engine: String,
    pub model: String,
    pub downloaded: bool,
    pub path: Option<String>,
}

/// Which model files are already on this machine, per engine.
#[tauri::command]
pub fn transcribe_models() -> Vec<ModelInfo> {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut out = Vec::new();
    for m in [
        "tiny",
        "base",
        "small",
        "medium",
        "large-v3",
        "distil-large-v3",
        "turbo",
    ] {
        // faster-whisper: Hugging Face hub cache, Systran/faster-whisper-<m>
        // (distil models live under Systran/faster-distil-whisper-*).
        let repo = if let Some(rest) = m.strip_prefix("distil-") {
            format!("models--Systran--faster-distil-whisper-{rest}")
        } else if m == "turbo" {
            "models--mobiuslabsgmbh--faster-whisper-large-v3-turbo".to_string()
        } else {
            format!("models--Systran--faster-whisper-{m}")
        };
        let p = std::path::Path::new(&home)
            .join(".cache/huggingface/hub")
            .join(&repo);
        let done = p.join("snapshots").exists();
        out.push(ModelInfo {
            engine: "faster-whisper".into(),
            model: m.into(),
            downloaded: done,
            path: done.then(|| p.to_string_lossy().into_owned()),
        });
        // openai-whisper: ~/.cache/whisper/<m>.pt
        let p = std::path::Path::new(&home)
            .join(".cache/whisper")
            .join(format!("{m}.pt"));
        let done = p.exists();
        out.push(ModelInfo {
            engine: "openai-whisper".into(),
            model: m.into(),
            downloaded: done,
            path: done.then(|| p.to_string_lossy().into_owned()),
        });
    }
    out
}

/// Download (and load once) a model in the background so the first real
/// transcription doesn't stall. Emits `transcribe_download` events:
/// {engine, model, state: "started"|"done"|"error", detail}.
#[tauri::command]
pub fn transcribe_download(app: AppHandle, engine: String, model: String) -> Result<(), String> {
    let script = script_path(&app);
    std::thread::spawn(move || {
        let _ = app.emit(
            "transcribe_download",
            serde_json::json!({"engine": engine, "model": model, "state": "started"}),
        );
        let out = Command::new(find_python(&engine))
            .arg(&script)
            .args(["--engine", &engine, "--model", &model, "--download"])
            .output();
        let (state, detail) = match out {
            Ok(o) if o.status.success() => (
                "done",
                String::from_utf8_lossy(&o.stdout).trim().to_string(),
            ),
            Ok(o) => (
                "error",
                format!(
                    "{}{}",
                    String::from_utf8_lossy(&o.stdout).trim(),
                    String::from_utf8_lossy(&o.stderr)
                        .lines()
                        .last()
                        .unwrap_or("")
                ),
            ),
            Err(e) => ("error", format!("python3: {e}")),
        };
        let _ = app.emit(
            "transcribe_download",
            serde_json::json!({"engine": engine, "model": model, "state": state, "detail": detail}),
        );
    });
    Ok(())
}

/// Transcribe one call now (even if auto-transcribe is off): queued ahead
/// of the pump's work; never blocks the caller.
#[tauri::command]
pub fn transcribe_call(app: AppHandle, state: State<AppState>, id: i64) -> Result<(), String> {
    {
        let db = state.db.lock().unwrap().clone().ok_or("library not open")?;
        let c = db.lock().unwrap();
        library::get(&c, id)?
            .and_then(|r| r.audio)
            .ok_or("no audio for that call")?;
    }
    let mut w = state.transcriber.lock().unwrap();
    if !w.wanted.contains(&id) {
        w.wanted.push_back(id);
    }
    drop(w);
    if !ensure_started(&app, &state.transcriber) {
        return Err(state
            .transcriber
            .lock()
            .unwrap()
            .last_error
            .clone()
            .unwrap_or("worker did not start".into()));
    }
    Ok(())
}

/// Background pump: feeds the worker the calls asked for explicitly, then,
/// while auto-transcribe is on, every untranscribed call.
pub fn spawn_pump(app: AppHandle) {
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(2));
        check_stalled(&app);
        let state = app.state::<AppState>();
        let (enabled, busy, wanted) = {
            let w = state.transcriber.lock().unwrap();
            (
                w.settings.enabled,
                w.busy.load(Ordering::SeqCst),
                w.wanted.front().copied(),
            )
        };
        if busy || (!enabled && wanted.is_none()) {
            continue;
        }
        let next = {
            let Some(db) = state.db.lock().unwrap().clone() else {
                continue;
            };
            let c = db.lock().unwrap();
            match wanted {
                Some(id) => {
                    state.transcriber.lock().unwrap().wanted.pop_front();
                    library::get(&c, id).ok().flatten()
                }
                None => library::untranscribed(&c, 1)
                    .ok()
                    .and_then(|v| v.into_iter().next()),
            }
        };
        let Some(row) = next else { continue };
        let Some(path) = row.audio else { continue };
        if !std::path::Path::new(&path).exists() {
            // Audio gone: mark so we don't loop on it.
            if let Some(db) = state.db.lock().unwrap().clone() {
                let _ = library::set_transcript(&db.lock().unwrap(), row.id, "", "missing-audio");
            }
            continue;
        }
        if !(ensure_started(&app, &state.transcriber)
            && submit(&state.transcriber, row.id, &path, row.secs))
        {
            // Keep an explicit request for the next attempt.
            if wanted == Some(row.id) {
                state.transcriber.lock().unwrap().wanted.push_front(row.id);
            }
            std::thread::sleep(std::time::Duration::from_secs(30));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_replacement_worker_does_not_get_the_first_download_allowance() {
        assert_eq!(job_timeout_secs(true, false, 2.0), FIRST_JOB_SECS);
        assert_eq!(job_timeout_secs(true, true, 2.0), RELOAD_JOB_SECS);
        assert_eq!(job_timeout_secs(false, true, 0.9), 34);
        // A long call keeps its proportional allowance even on a first job.
        assert_eq!(job_timeout_secs(true, true, 60.0), 270);
    }
}
