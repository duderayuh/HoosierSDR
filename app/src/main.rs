//! HoosierSDR desktop app (Tauri v2) — a thin shell over `hs-core`.
//!
//! All decode logic lives in the workspace crates; this file only wires the
//! decoder + RTL-SDR capture to the web UI over Tauri commands and events.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rusqlite::OptionalExtension;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use hs_catalog::CsvCatalog;
use hs_core::decoder::{ChannelDecoder, EqMode, Modulation};

mod alerts;
mod analyzers;
mod channels;
mod connections;
mod conversations;
mod devices;
mod digest;
mod dispatch;
mod dual;
mod encode;
mod events;
mod follow;
mod hook;
mod library;
mod models;
mod names;
mod player;
mod playlists;
mod remotes;
mod retention;
mod rr;
mod secrets;
mod status;
mod stream;
mod sysstat;
mod tiles;
mod transcribe;
mod units;
mod upload;
mod web;

#[derive(Default)]
struct AppState {
    running: Arc<AtomicBool>,
    /// The current run's own stop flag. Each run gets its own, so a Stop
    /// followed quickly by a Start on another radio cannot resurrect the
    /// previous loop (which is what happened when `running` was shared: the
    /// new start set it true again before the old loop had seen false, the
    /// old loop kept the RTL-SDR, and the new one hung joining it).
    run_flag: Arc<Mutex<Option<Arc<AtomicBool>>>>,
    /// Most calls decoded at once (1–24) and how much queued audio to keep.
    max_calls: Arc<std::sync::atomic::AtomicUsize>,
    /// Channelizer (true) or classic per-channel extraction. Applies on the
    /// next start.
    use_channelizer: Arc<AtomicBool>,
    /// Vocoder unvoiced-synthesis quality (1–64), next start.
    uv_quality: Arc<std::sync::atomic::AtomicI32>,
    /// Live gain handles of the radios in the current run, by picker key.
    gain_handles: Arc<Mutex<std::collections::HashMap<String, hs_source::GainHandle>>>,
    /// Bumped per capture/follow start; a finishing run only reports
    /// 'stopped' if it is still the current one.
    run_gen: Arc<std::sync::atomic::AtomicU64>,
    /// Set while a library re-encode is running (one at a time).
    reencode_running: Arc<AtomicBool>,
    /// The previous run's thread, joined before a new radio is opened.
    run_thread: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
    /// Live trunk-following runs (one per playlist/site) and the radios they
    /// read. Several runs share one radio through a `Tee` when their control
    /// channels sit inside the same capture.
    runs: Mutex<Vec<Run>>,
    radios: Mutex<std::collections::HashMap<String, Radio>>,
    run_seq: std::sync::atomic::AtomicU64,
    /// Serialises radio opening, so two runs started together see one
    /// opened radio instead of both trying to open the same device.
    start_lock: Mutex<()>,
    /// Talkgroup names, kept apart per RadioReference system.
    catalog: Arc<Mutex<rr::Catalogs>>,
    /// Live lockout / allowlist / hold / priorities per playlist (`""` for
    /// runs without one); sites sharing a playlist share the entry.
    filters: playlists::FilterTable,
    /// Transcript corrections: (tg, wrong, right); `tg` None = applies to every
    /// talkgroup (global rule — say "Rirey" on any channel → "Riley").
    corrections: Arc<Mutex<Vec<(Option<u16>, String, String)>>>,
    /// Radio-ID aliases, and the wildcard rules behind them.
    units: units::Units,
    unit_rules: units::Rules,
    /// Remember over-the-air aliases into the radio-ID table.
    learn_aliases: Arc<AtomicBool>,
    /// Script run after each call.
    hook: hook::Shared,
    /// Which talkgroups are recorded / streamed / uploaded.
    record_policy: Arc<Mutex<Policy>>,
    stream_policy: Arc<Mutex<Policy>>,
    upload_policy: Arc<Mutex<Policy>>,
    /// Keyword / emergency / activity alerts (Telegram, tones).
    alerts: alerts::Shared,
    /// Conversation rules and the conversations in progress.
    conversations: conversations::Shared,
    /// Periodic channel digests ("what's happening" roll-ups).
    digests: digest::Shared,
    /// Custom prompt analyzers: extract structured fields from a transcript
    /// and send a message when a condition holds (e.g. ECPR candidacy).
    analyzers: analyzers::Shared,
    /// Dispatch channels → geocoded, grouped incidents on the live map.
    dispatch: dispatch::Shared,
    /// Filename template for stored calls.
    names: Mutex<names::Settings>,
    /// The audio thread, started on first use. `Some(None)` = no device.
    audio: Mutex<Option<Option<player::Audio>>>,
    /// The call library (opened at startup).
    db: Arc<Mutex<Option<Arc<Mutex<rusqlite::Connection>>>>>,
    /// Where call audio lives.
    library_dir: Mutex<Option<std::path::PathBuf>>,
    /// Archive playback in progress: live calls are stored but not spoken.
    archive_mode: Arc<AtomicBool>,
    transcriber: transcribe::Shared,
    /// Stored audio format for new calls.
    format: Mutex<encode::Format>,
    /// Spectrum settings: (fft size, averaging blocks).
    spectrum: Arc<Mutex<(usize, usize)>>,
    /// Live Icecast/Broadcastify feed, when enabled.
    streamer: stream::Shared,
    /// Per-call uploads (rdio-scanner / OpenMHz / Broadcastify Calls).
    uploader: upload::Shared,
    /// Tailnet trust for the web server (other instances on the account).
    remotes: remotes::Shared,
    /// Broadcast of web frames (app events + live audio) to SSE clients.
    /// Initialised once by `web::spawn`; read by the follow loops to tap audio.
    web_frames: std::sync::OnceLock<tokio::sync::broadcast::Sender<crate::web::Frame>>,
    /// How the current run was started (mode, radio, frequencies, playlist
    /// names), so a remote desktop page joining mid-run can show the same
    /// controls as the local one. Cleared when the run ends.
    last_start: Mutex<Option<serde_json::Value>>,
    /// What the call library keeps, and for how long.
    retention: Mutex<retention::Settings>,
}

/// One live trunk-following run: a site (usually a playlist) being followed.
struct Run {
    id: u64,
    /// Short name for the UI: the playlist's name, else the site's.
    label: String,
    system: String,
    site: String,
    control_hz: f64,
    /// `source|device` of the radio it reads.
    radio: String,
    flag: Arc<AtomicBool>,
    playlist: Option<String>,
    /// RadioReference system id, when known (from the playlist, else a
    /// playlist on the same control channel).
    sid: Option<u32>,
}

/// An opened radio, shared by every run whose control channel fits.
struct Radio {
    tee: Arc<hs_core::stream::Tee>,
    center_hz: f64,
    /// Rate after normalisation (what the followers see).
    norm_rate: f64,
    name: String,
}

/// What the UI shows for a run (chips in the top bar, the run picker).
#[derive(Serialize, Clone)]
pub(crate) struct RunInfo {
    id: u64,
    label: String,
    system: String,
    site: String,
    control_mhz: f64,
    radio: String,
    playlist: Option<String>,
    sid: Option<u32>,
}

pub(crate) fn runs_info(state: &AppState) -> Vec<RunInfo> {
    state
        .runs
        .lock()
        .unwrap()
        .iter()
        .map(|r| RunInfo {
            id: r.id,
            label: r.label.clone(),
            system: r.system.clone(),
            site: r.site.clone(),
            control_mhz: r.control_hz / 1e6,
            radio: r.radio.clone(),
            playlist: r.playlist.clone(),
            sid: r.sid,
        })
        .collect()
}

/// Tell the UI which runs exist (after a start or a stop).
fn emit_runs(app: &AppHandle) {
    let _ = app.emit("runs", runs_info(&app.state::<AppState>()));
}

/// The live runs, for a page opening mid-session.
#[tauri::command]
fn runs_list(state: State<AppState>) -> Vec<RunInfo> {
    runs_info(&state)
}

/// Stop one run; the others (and the radio, if shared) carry on.
#[tauri::command]
fn stop_run(id: u64, state: State<AppState>) -> Result<(), String> {
    let runs = state.runs.lock().unwrap();
    let r = runs.iter().find(|r| r.id == id).ok_or("no such run")?;
    r.flag.store(false, Ordering::SeqCst);
    Ok(())
}

/// A follow event tagged with the run it came from, so the UI can tell
/// systems apart: `run` (id) and `system` (the run's label).
fn emit_run(app: &AppHandle, run: u64, label: &str, ev: follow::FollowEvent) {
    let mut v = serde_json::to_value(&ev).unwrap_or(serde_json::Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.insert("run".into(), run.into());
        o.insert("system".into(), label.into());
    }
    let _ = app.emit("follow", v);
}

/// Close radios nobody reads any more (join their reader threads so the
/// device is really released) before something opens one.
fn join_idle_radios(state: &AppState) {
    let mut radios = state.radios.lock().unwrap();
    let mut handles = state.gain_handles.lock().unwrap();
    radios.retain(|k, r| {
        if r.tee.alive() && r.tee.consumers() > 0 {
            true
        } else {
            r.tee.join();
            handles.remove(k);
            false
        }
    });
}

impl AppState {
    fn audio(&self) -> Option<player::Audio> {
        self.audio
            .lock()
            .unwrap()
            .get_or_insert_with(player::spawn)
            .clone()
    }
}

/// A per-talkgroup policy: `None` = everything; `Some((all, except))` =
/// everything or nothing by default, with `except` flipped the other way.
pub type Policy = Option<(bool, Vec<u16>)>;

pub fn policy_allows(p: &Policy, tg: u16) -> bool {
    match p {
        None => true,
        Some((all, except)) => *all != except.contains(&tg),
    }
}

/// Which talkgroups to record / stream / upload: each is `[all, [exceptions]]`.
#[tauri::command]
fn set_policies(state: State<AppState>, record: Policy, stream: Policy, upload: Policy) {
    *state.record_policy.lock().unwrap() = record;
    *state.stream_policy.lock().unwrap() = stream;
    *state.upload_policy.lock().unwrap() = upload;
}

/// Apply per-talkgroup transcript corrections: each `(wrong, right)` pair is a
/// case-insensitive, whole-word substitution (so "rirey"/"RIREY" → "Riley" but
/// "shirey" is left alone). Applied before a transcript is stored or acted on.
pub(crate) fn apply_corrections(rules: &[(String, String)], text: &str) -> String {
    let mut out = text.to_string();
    for (from, to) in rules {
        let from = from.trim();
        if from.is_empty() {
            continue;
        }
        let pat = format!(r"(?i)\b{}\b", regex::escape(from));
        if let Ok(re) = regex::Regex::new(&pat) {
            out = re
                .replace_all(&out, regex::NoExpand(to.as_str()))
                .into_owned();
        }
    }
    out
}

/// Transcript corrections (flat list; `tg` None = global, Some = per-talkgroup).
#[tauri::command]
fn corrections_get(state: State<AppState>) -> Vec<(Option<u16>, String, String)> {
    state.corrections.lock().unwrap().clone()
}

#[tauri::command]
fn corrections_set(
    app: AppHandle,
    state: State<AppState>,
    entries: Vec<(Option<u16>, String, String)>,
) {
    let list: Vec<(Option<u16>, String, String)> = entries
        .into_iter()
        .filter(|(_, a, b)| !a.trim().is_empty() && !b.trim().is_empty())
        .collect();
    *state.corrections.lock().unwrap() = list.clone();
    if let Ok(d) = app.path().app_config_dir() {
        let _ = std::fs::create_dir_all(&d);
        let _ = std::fs::write(
            d.join("corrections.json"),
            serde_json::to_string_pretty(&list).unwrap_or_default(),
        );
    }
}

/// Speaker gain 0 (mute) … 1 (unity) … 2. Applies to live calls, replay and
/// library playback alike.
#[tauri::command]
fn set_volume(gain: f32, state: State<AppState>) {
    if let Some(a) = state.audio() {
        a.set_volume(gain);
    }
}

#[tauri::command]
fn get_volume(state: State<AppState>) -> f32 {
    state.audio().map(|a| a.volume()).unwrap_or(1.0)
}

#[derive(Serialize)]
struct NamesView {
    settings: names::Settings,
    tokens: Vec<(String, String)>,
    example: String,
}

fn names_example(template: &str) -> String {
    names::render(
        template,
        &names::NameContext {
            stamp: "20260821-143012",
            tg: 20308,
            tg_name: "Sheriff Patrol",
            unit: 790065,
            unit_name: "Car 12",
            freq_hz: 851_812_500,
            system: "Statewide",
            site: "County",
            modulation: "CQPSK",
            secs: 4.6,
            emergency: false,
        },
    )
}

#[tauri::command]
fn names_get(state: State<AppState>) -> NamesView {
    let s = state.names.lock().unwrap().clone();
    NamesView {
        example: names_example(&s.template),
        tokens: names::TOKENS
            .iter()
            .map(|(t, d)| (t.to_string(), d.to_string()))
            .collect(),
        settings: s,
    }
}

#[tauri::command]
fn names_set(app: AppHandle, state: State<AppState>, template: String) -> Result<String, String> {
    let s = names::Settings {
        template: template.trim().to_string(),
    };
    let d = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("config dir: {e}"))?;
    std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    std::fs::write(
        d.join("names.json"),
        serde_json::to_string_pretty(&s).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("names.json: {e}"))?;
    let example = names_example(&s.template);
    *state.names.lock().unwrap() = s;
    Ok(example)
}

/// Preview a template without saving it.
#[tauri::command]
fn names_preview(template: String) -> String {
    names_example(&template)
}

#[tauri::command]
fn set_learn_aliases(on: bool, state: State<AppState>) {
    state.learn_aliases.store(on, Ordering::SeqCst);
}

/// The radio's oscillator error in parts per million: a positive value
/// means it runs high, so the requested frequency is lowered to land where
/// it should. Everything downstream keeps using nominal frequencies.
fn ppm_tune(freq: f64, ppm: Option<f64>) -> f64 {
    match ppm {
        Some(p) if p.is_finite() && p.abs() < 1000.0 => freq / (1.0 + p / 1e6),
        _ => freq,
    }
}

/// Stop the call being played and move to the next queued one.
#[tauri::command]
fn skip_call(state: State<AppState>) {
    if let Some(a) = state.audio() {
        a.skip();
    }
}

/// Calls waiting in the speaker queue (including the one playing).
#[derive(Serialize)]
struct QueueView {
    clips: usize,
    secs: f32,
    dropped: u64,
}

#[tauri::command]
fn audio_queued(state: State<AppState>) -> QueueView {
    match state.audio() {
        Some(a) => {
            let (secs, dropped) = a.backlog();
            QueueView {
                clips: a.queued(),
                secs,
                dropped,
            }
        }
        None => QueueView {
            clips: 0,
            secs: 0.0,
            dropped: 0,
        },
    }
}

/// Drop the playing call and everything queued (used when archive playback
/// starts or stops, so the two never interleave).
#[tauri::command]
fn clear_queue(state: State<AppState>) {
    if let Some(a) = state.audio() {
        a.clear();
    }
}

/// Play the last completed call again.
#[tauri::command]
fn replay_last(state: State<AppState>) -> Result<(), String> {
    state.audio().ok_or("no audio output device")?.replay_last();
    Ok(())
}

/// Front-end diagnostics land in the terminal that launched the app, so a
/// page that silently does nothing can say why.
#[tauri::command]
fn ui_log(msg: String) {
    eprintln!("[ui] {msg}");
}

// ---------------- audio format + spectrum ----------------

#[derive(Serialize)]
struct FormatInfo {
    format: encode::Format,
    ffmpeg: Option<String>,
}

#[tauri::command]
fn format_get(state: State<AppState>) -> FormatInfo {
    FormatInfo {
        format: state.format.lock().unwrap().clone(),
        ffmpeg: encode::ffmpeg_available(),
    }
}

#[tauri::command]
fn format_set(
    app: AppHandle,
    state: State<AppState>,
    format: encode::Format,
) -> Result<(), String> {
    if format.codec != "wav" && encode::ffmpeg_available().is_none() {
        return Err("ffmpeg is not installed (brew install ffmpeg); WAV only until it is".into());
    }
    *state.format.lock().unwrap() = format.clone();
    if let Ok(d) = app.path().app_config_dir() {
        let _ = std::fs::create_dir_all(&d);
        let _ = std::fs::write(
            d.join("format.json"),
            serde_json::to_string_pretty(&format).unwrap_or_default(),
        );
    }
    Ok(())
}

/// Re-encode every stored audio file that isn't already at the current
/// format. Runs on a background thread; emits `reencode_progress` (per file),
/// `reencode_error` (per failed file), and `reencode_done` (summary).
#[tauri::command]
fn library_reencode(app: AppHandle, state: State<AppState>) -> Result<(), String> {
    let format = state.format.lock().unwrap().clone();
    if format.codec != "wav" && encode::ffmpeg_available().is_none() {
        return Err("ffmpeg is not installed (brew install ffmpeg); WAV only until it is".into());
    }
    if state.reencode_running.swap(true, Ordering::SeqCst) {
        return Err("a re-encode is already running".into());
    }
    let rows = match state.db.lock().unwrap().clone() {
        Some(db) => {
            let c = db.lock().unwrap();
            library::all_audio(&c)?
        }
        None => {
            state.reencode_running.store(false, Ordering::SeqCst);
            return Err("library not open".into());
        }
    };
    let total = rows.len();
    std::thread::spawn(move || {
        let mut converted = 0usize;
        let mut skipped = 0usize;
        let mut failed = 0usize;
        for (i, (id, path)) in rows.iter().enumerate() {
            let _ = app.emit(
                "reencode_progress",
                serde_json::json!({
                    "done": i, "total": total,
                    "converted": converted, "skipped": skipped, "failed": failed,
                    "current": path,
                }),
            );
            let src = std::path::Path::new(path);
            if !src.exists() {
                skipped += 1;
                continue;
            }
            let out = match encode::reencode(src, &format) {
                Ok(p) => p,
                Err(e) => {
                    failed += 1;
                    let _ = app.emit(
                        "reencode_error",
                        serde_json::json!({ "id": id, "path": path, "error": e }),
                    );
                    continue;
                }
            };
            if out.as_path() == src {
                skipped += 1;
                continue;
            }
            let _ = std::fs::remove_file(src);
            let st = app.state::<AppState>();
            if let Some(db) = st.db.lock().unwrap().clone() {
                let c = db.lock().unwrap();
                if library::set_audio(&c, *id, out.to_str().unwrap_or_default()).is_err() {
                    failed += 1;
                    continue;
                }
            }
            converted += 1;
        }
        app.state::<AppState>()
            .reencode_running
            .store(false, Ordering::SeqCst);
        let _ = app.emit(
            "reencode_done",
            serde_json::json!({ "total": total, "converted": converted, "skipped": skipped, "failed": failed }),
        );
    });
    Ok(())
}

/// Waterfall FFT size (256–4096) and number of blocks averaged (1–16).
#[tauri::command]
fn spectrum_set(state: State<AppState>, fft: usize, average: usize) {
    let fft = fft.clamp(256, 4096).next_power_of_two();
    *state.spectrum.lock().unwrap() = (fft, average.clamp(1, 16));
}

// ---------------- library ----------------

fn with_db<T>(
    state: &State<AppState>,
    f: impl FnOnce(&rusqlite::Connection) -> Result<T, String>,
) -> Result<T, String> {
    let guard = state.db.lock().unwrap();
    let db = guard.as_ref().ok_or("library not open")?;
    let c = db.lock().unwrap();
    f(&c)
}

#[tauri::command]
fn library_search(
    app: AppHandle,
    state: State<AppState>,
    query: library::Query,
) -> Result<Vec<library::CallRow>, String> {
    let mut rows = with_db(&state, |c| {
        let mut rows = library::search(c, &query)?;
        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
        let mut fired = events::fired_for(c, &ids);
        for r in rows.iter_mut() {
            r.fired = fired.remove(&r.id).unwrap_or_default();
        }
        Ok(rows)
    })?;
    let sids = playlists::sids_by_system_name(&app);
    for r in rows.iter_mut() {
        r.tg_desc = upload::tg_meta(&state.catalog, sids.get(&r.system).copied(), r.tg).desc;
    }
    Ok(rows)
}

#[tauri::command]
fn library_get(
    app: AppHandle,
    state: State<AppState>,
    id: i64,
) -> Result<Option<library::CallRow>, String> {
    let mut row = with_db(&state, |c| {
        let mut row = library::get(c, id)?;
        if let Some(r) = row.as_mut() {
            r.fired = events::fired_for(c, &[r.id]).remove(&r.id).unwrap_or_default();
        }
        Ok(row)
    })?;
    if let Some(r) = row.as_mut() {
        let sid = playlists::sids_by_system_name(&app).get(&r.system).copied();
        r.tg_desc = upload::tg_meta(&state.catalog, sid, r.tg).desc;
    }
    Ok(row)
}

#[tauri::command]
fn library_star(state: State<AppState>, id: i64, on: bool) -> Result<(), String> {
    with_db(&state, |c| library::set_starred(c, id, on))
}

/// A human correction; empty text clears it. The machine transcript stays.
#[tauri::command]
fn library_set_edited(state: State<AppState>, id: i64, text: String) -> Result<(), String> {
    let t = text.trim();
    with_db(&state, |c| {
        library::set_edited(c, id, (!t.is_empty()).then_some(t))
    })
}

#[tauri::command]
fn library_stats(state: State<AppState>) -> Result<(i64, f64, i64, String), String> {
    let dir = state
        .library_dir
        .lock()
        .unwrap()
        .clone()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    with_db(&state, library::stats).map(|(n, secs, tr)| (n, secs, tr, dir))
}

#[tauri::command]
fn library_prune(state: State<AppState>, days: u32) -> Result<usize, String> {
    with_db(&state, |c| library::prune(c, days))
}

/// Export a cart to a folder with a chain-of-custody manifest; returns the
/// manifest path.
#[tauri::command]
fn library_export(
    app: AppHandle,
    state: State<AppState>,
    ids: Vec<i64>,
    dest: String,
) -> Result<String, String> {
    let dest = std::path::PathBuf::from(shellexpand_home(&dest));
    let ver = format!("HoosierSDR {}", app.package_info().version);
    with_db(&state, |c| library::export(c, &ids, &dest, &ver))
        .map(|p| p.to_string_lossy().into_owned())
}

/// The newest library call on a talkgroup that has audio (Discovery's
/// "play what this talkgroup said").
#[tauri::command]
fn tg_latest_call(state: State<AppState>, tg: u16) -> Result<Option<library::CallRow>, String> {
    with_db(&state, |c| {
        let mut st = c
            .prepare(
                "SELECT id FROM calls WHERE tg = ?1 AND audio IS NOT NULL ORDER BY id DESC LIMIT 1",
            )
            .map_err(|e| e.to_string())?;
        let id: Option<i64> = st
            .query_row([tg], |r| r.get(0))
            .optional()
            .map_err(|e| e.to_string())?;
        match id {
            Some(id) => library::get(c, id),
            None => Ok(None),
        }
    })
}

/// Play a library call through the speaker, ahead of anything queued.
#[tauri::command]
async fn library_play(app: AppHandle, id: i64) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let path = with_db(&state, |c| library::get(c, id))?
            .and_then(|r| r.audio)
            .ok_or("no audio for that call")?;
        let pcm = encode::decode_to_pcm(std::path::Path::new(&path))?;
        state.audio().ok_or("no audio output device")?.play(pcm, 0);
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// While archive playback is on, live calls are stored but not spoken.
#[tauri::command]
fn set_archive_mode(state: State<AppState>, on: bool) {
    state.archive_mode.store(on, Ordering::SeqCst);
    if let Some(a) = state.audio() {
        a.clear();
    }
}

/// Replay a saved call through the default audio device.
#[tauri::command]
async fn play_wav(app: AppHandle, path: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let pcm = encode::decode_to_pcm(std::path::Path::new(&shellexpand_home(&path)))?;
        state.audio().ok_or("no audio output device")?.play(pcm, 0);
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Serialize, Clone)]
struct GrantMsg {
    tg: u16,
    name: String,
    source: u32,
    freq_mhz: f64,
    encrypted: bool,
}

#[derive(Serialize, Clone)]
struct StatusMsg {
    syncs: usize,
    grants: usize,
    voice_secs: f64,
    blocks: u64,
    modulation: String,
    /// CQPSK carrier-lock quality 0..1, or -1 on the C4FM path (no metric).
    lock: f32,
    /// Fraction of equalizer tap energy off the cursor — live simulcast-
    /// distortion severity; -1 when unavailable (C4FM, bypass, unacquired).
    echo_frac: f32,
    /// Energy-weighted RMS spread of the learned echo delays, µs; -1 when
    /// unavailable.
    echo_spread_us: f32,
    /// Percentage of samples at the ADC rails since the last status —
    /// front-end overload; the cure is less gain.
    clip_pct: f32,
    /// Mean frame-sync bit errors (of 48) — the receiver's own decode quality.
    sync_err: f64,
    /// Samples/blocks lost between the radio and the decoder so far.
    dropped: u64,
    /// Composite voice quality (0..1, see `hs_core::decoder::VoiceQuality`)
    /// of the most recently decoded voice frame — combines FEC error count,
    /// demodulator confidence, and (CQPSK) carrier lock into the one number
    /// this app shows for "how good does this call sound right now"; -1
    /// before any voice has decoded.
    voice_quality: f32,
}

#[derive(Serialize, Clone)]
struct SpectrumMsg {
    bins_db: Vec<f32>,
}

/// Load a RadioReference talkgroup CSV from a file path; returns the number
/// of talkgroups parsed.
#[tauri::command]
fn load_catalog(app: AppHandle, path: String, state: State<AppState>) -> Result<usize, String> {
    let path = shellexpand_home(&path);
    let text = std::fs::read_to_string(&path).map_err(|e| format!("read {path}: {e}"))?;
    let n = CsvCatalog::parse(&text).len();
    if n == 0 {
        return Err(
            "no talkgroups found in that CSV (expected RadioReference export columns)".into(),
        );
    }
    // Keep a copy so it is merged in on every start.
    if let Ok(d) = rr::catalogs_dir(&app) {
        let stem = std::path::Path::new(&path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "import".into());
        let _ = std::fs::write(d.join(format!("csv_{stem}.csv")), &text);
    }
    let all = rr::load_catalogs(&app);
    let total = all.len().max(n);
    *state.catalog.lock().unwrap() = all;
    Ok(total)
}

/// Stop an in-progress live capture.
#[tauri::command]
fn stop_capture(state: State<AppState>) {
    for r in state.runs.lock().unwrap().iter() {
        r.flag.store(false, Ordering::SeqCst);
    }
    if let Some(f) = state.run_flag.lock().unwrap().as_ref() {
        f.store(false, Ordering::SeqCst);
        state.running.store(false, Ordering::SeqCst);
    }
}

/// How many calls the follower decodes at once (1–24). Each costs a
/// channel decoder (two until the site modulation is confirmed); the
/// channelizer makes a dozen cheap. Lower it if the STREAM drop counter climbs. Applies on the
/// next start.
#[tauri::command]
fn set_max_calls(n: usize, state: State<AppState>) {
    state.max_calls.store(n.clamp(1, 24), Ordering::SeqCst);
}

/// Vocoder unvoiced-synthesis quality (1–64; mbelib's own default is 3).
/// Higher renders unvoiced sounds (s, f, sh) from more sine components:
/// smoother and less metallic, at a little CPU. Applies on the next start.
#[tauri::command]
fn set_uv_quality(q: i32, state: State<AppState>) {
    state.uv_quality.store(q.clamp(1, 64), Ordering::SeqCst);
}

/// Traffic-channel extraction: shared channelizer (true, cheap at any call
/// count) or the classic per-channel decimator (false, the original path).
/// Applies on the next start — kept so the two can be compared on air.
#[tauri::command]
fn set_channelizer(on: bool, state: State<AppState>) {
    state.use_channelizer.store(on, Ordering::SeqCst);
}

/// Drop queued audio older than `secs` (0 = keep everything): with many
/// talkgroups on the air the queue otherwise falls minutes behind.
#[tauri::command]
fn set_queue_limit(secs: f32, state: State<AppState>) {
    if let Some(a) = state.audio() {
        a.set_queue_limit(secs);
    }
}

/// Start live capture from an RTL-SDR or Airspy (`source`). Emits `grant`, `status`, and
/// `spectrum` events; on stop emits `stopped`, or `error` on failure.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
fn start_capture(
    app: AppHandle,
    state: State<AppState>,
    source: String,
    freq: f64,
    rate: f64,
    gain: Option<f64>,
    cqpsk: bool,
    eq: String,
    record_iq: Option<String>,
    record_log: Option<String>,
    ppm: Option<f64>,
    device: Option<String>,
) -> Result<(), String> {
    if state.running.swap(true, Ordering::SeqCst) {
        return Err("already capturing".into());
    }
    *state.last_start.lock().unwrap() = Some(serde_json::json!({
        "mode": "capture", "source": source, "device": device, "rate": rate, "freq": freq,
        "gain": gain, "ppm": ppm, "cqpsk": cqpsk, "eq": eq,
    }));
    let running = Arc::new(AtomicBool::new(true));
    *state.run_flag.lock().unwrap() = Some(running.clone());
    let catalog = state.catalog.clone();
    let spectrum_cfg = state.spectrum.clone();
    let my_gen = state.run_gen.fetch_add(1, Ordering::SeqCst) + 1;
    let prev = take_previous(&state);
    let handle = std::thread::spawn(move || {
        join_previous(prev);
        join_idle_radios(&app.state::<AppState>());
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            capture_loop(
                &app,
                &running,
                &catalog,
                &source,
                freq,
                rate,
                gain,
                cqpsk,
                &eq,
                record_iq,
                record_log,
                spectrum_cfg,
                ppm,
                device.as_deref(),
            )
        }))
        .unwrap_or_else(|p| Err(format!("capture crashed: {}", panic_text(&p))));
        finish_run(&app, my_gen, res);
    });
    *state.run_thread.lock().unwrap() = Some(handle);
    Ok(())
}

/// Open a particular radio: `source` is "airspy" or "rtlsdr"; `device` is
/// the Airspy serial (hex) or the Seify args that name one RTL-SDR — or
/// `None` for the first of that kind found. `gain` is the legacy overall
/// gain (RTL-SDR dB; `None` = AGC) used when no per-device settings exist.
/// Turn a driver's debug string into something a listener can act on. A
/// USB timeout or I/O error from an RTL-SDR is not a settings problem: the
/// dongle has stopped answering on the bus (seen after an I/O error mid-call
/// on one that then re-enumerated with serial 00000001 — its EEPROM was no
/// longer readable either) and every open will fail until it is unplugged and
/// plugged back in. Saying so beats a wall of `rtlsdr_demod_read_reg failed
/// with -7`.
pub(crate) fn usb_advice(err: &str) -> String {
    let e = err.to_string();
    if e.contains("Usb(Timeout)") || e.contains("Usb(Io)") || e.contains("Usb(Pipe)") {
        format!(
            "{e} — the dongle is not answering on USB. Unplug it and plug it back in \
             (a hung RTL-SDR keeps failing every open, and may show serial 00000001 \
             until it is power-cycled); prefer a powered hub or a direct port, and \
             don't enumerate it through Soapy while it is hung — librtlsdr can crash \
             probing a wedged device."
        )
    } else if e.contains("Usb(Busy)") || e.contains("Usb(Access)") {
        format!("{e} — another program (or another copy of this app) has the radio open.")
    } else if e.contains("Usb(NoDevice)") || e.contains("NotFound") {
        format!("{e} — no such radio on the bus; check the picker in Devices.")
    } else {
        e
    }
}

fn open_device(
    source: &str,
    device: Option<&str>,
    freq: f64,
    rate: f64,
    gain: Option<f64>,
) -> Result<Box<dyn hs_source::SdrSource + Send>, String> {
    open_device_with_gain(source, device, freq, rate, gain, None).map(|(s, _)| s)
}

/// As [`open_device`], applying a full gain setting and returning the
/// handle that changes it while streaming.
fn open_device_with_gain(
    source: &str,
    device: Option<&str>,
    freq: f64,
    rate: f64,
    gain: Option<f64>,
    setting: Option<hs_source::GainSetting>,
) -> Result<(Box<dyn hs_source::SdrSource + Send>, hs_source::GainHandle), String> {
    use hs_source::airspy::AirspySource;
    use hs_source::rtlsdr::RtlSdrSource;
    use hs_source::soapy::SoapyRtlSource;
    use hs_source::GainSetting;
    Ok(match source {
        "airspy" => {
            let serial = device
                .filter(|d| !d.is_empty())
                .and_then(|d| u64::from_str_radix(d.trim_start_matches("0x"), 16).ok());
            let mut src = AirspySource::open(serial, freq, rate, None)
                .map_err(|e| format!("open Airspy: {}", usb_advice(&format!("{e:?}"))))?;
            if let Some(g) = setting
                .as_ref()
                .filter(|g| !matches!(g, GainSetting::Manual(_)))
            {
                src.set_gain(g).map_err(|e| format!("Airspy gain: {e:?}"))?;
            }
            let h = src.gain_handle();
            (Box::new(src), h)
        }
        "soapy" => {
            let args = device
                .filter(|d| !d.is_empty())
                .unwrap_or("driver=soapy,soapy_driver=rtlsdr");
            let db = match setting {
                Some(GainSetting::Manual(db)) => Some(db),
                Some(GainSetting::Agc) => None,
                _ => gain,
            };
            let src = SoapyRtlSource::open(args, freq, rate, db)
                .map_err(|e| format!("open RTL-SDR (Soapy): {}", usb_advice(&format!("{e:?}"))))?;
            let h = src.gain_handle();
            (Box::new(src), h)
        }
        _ => {
            let args = device.filter(|d| !d.is_empty()).unwrap_or("driver=rtlsdr");
            let db = match setting {
                Some(GainSetting::Manual(db)) => Some(db),
                Some(GainSetting::Agc) => None,
                _ => gain,
            };
            let src = RtlSdrSource::open(args, freq, rate, db)
                .map_err(|e| format!("open RTL-SDR: {}", usb_advice(&format!("{e:?}"))))?;
            let h = src.gain_handle();
            (Box::new(src), h)
        }
    })
}

/// An extra radio the UI asks to park on part of the site's span.
#[derive(serde::Deserialize, Clone, Debug)]
struct ExtraSpec {
    source: String,
    device: Option<String>,
    center: f64,
    rate: f64,
    gain: Option<f64>,
    ppm: Option<f64>,
    label: Option<String>,
}

/// Follow a trunked site live: `freq` is the band centre the radio tunes to,
/// `control` the control channel inside it. Emits `follow` events
/// (`{kind: measured|call_start|call|notice|status|spectrum, ...}`), then
/// `stopped`, or `error`. Completed calls are played as they finish when
/// `play` is set, and written to `calls_dir` when given.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
fn start_follow(
    app: AppHandle,
    state: State<AppState>,
    source: String,
    freq: f64,
    rate: f64,
    gain: Option<f64>,
    control: f64,
    calls_dir: Option<String>,
    play: bool,
    hang_ms: Option<u32>,
    system_name: Option<String>,
    site_name: Option<String>,
    ppm: Option<f64>,
    device: Option<String>,
    modulation: Option<String>,
    extra: Option<Vec<ExtraSpec>>,
    playlist: Option<String>,
) -> Result<u64, String> {
    // A capture or dual-SDR session owns its radio outright; follow runs
    // stack — each site is its own run, sharing a radio when it fits.
    if state.running.load(Ordering::SeqCst) && state.runs.lock().unwrap().is_empty() {
        return Err("already running".into());
    }
    *state.last_start.lock().unwrap() = Some(serde_json::json!({
        "mode": "follow", "source": source, "device": device, "rate": rate, "freq": freq,
        "control": control, "gain": gain, "ppm": ppm, "modulation": modulation, "play": play,
        "hang_ms": hang_ms, "system_name": system_name, "site_name": site_name,
    }));
    if let Some(r) = state
        .runs
        .lock()
        .unwrap()
        .iter()
        .find(|r| (r.control_hz - control).abs() < 1.0)
    {
        return Err(format!(
            "already following {} on {:.4} MHz",
            r.label,
            control / 1e6
        ));
    }
    let taken_controls: Vec<f64> = state.runs.lock().unwrap().iter().map(|r| r.control_hz).collect();
    let radio_key = format!("{source}|{}", device.clone().unwrap_or_default());
    if let Some(r) = state.radios.lock().unwrap().get(&radio_key).filter(|r| r.tee.alive() && r.tee.consumers() > 0) {
        let half = r.norm_rate * 0.4;
        if (control - r.center_hz).abs() >= half {
            return Err(format!(
                "{} is tuned to {:.4} MHz (±{:.2} MHz) for another system, and {:.4} MHz is outside that — stop the other system first, or start both together so the band centre covers both",
                r.name, r.center_hz / 1e6, half / 1e6, control / 1e6
            ));
        }
    }
    let playlist = playlist.filter(|s| !s.is_empty());
    let (pl_name, pl_sid) = match playlist.as_deref() {
        Some(id) => {
            let p = playlists::load(&app)
                .into_iter()
                .find(|p| p.id == id)
                .ok_or("no such playlist")?;
            (Some(p.name), Some(p.sid))
        }
        None => (None, None),
    };
    // Which system's names apply: the playlist's, else whichever saved site
    // uses this control channel. Without one, every loaded catalog merged.
    let sid = pl_sid.or_else(|| playlists::sid_for_control(&app, control));
    // The chip label: the site's name (several sites can share a playlist).
    let label = site_name
        .clone()
        .filter(|s| !s.is_empty())
        .or(pl_name)
        .or_else(|| system_name.clone().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| format!("{:.4} MHz", control / 1e6));
    let running = Arc::new(AtomicBool::new(true));
    let run_id = state.run_seq.fetch_add(1, Ordering::SeqCst) + 1;
    state.running.store(true, Ordering::SeqCst);
    state.runs.lock().unwrap().push(Run {
        id: run_id,
        label: label.clone(),
        system: system_name.clone().unwrap_or_default(),
        site: site_name.clone().unwrap_or_default(),
        control_hz: control,
        radio: radio_key.clone(),
        flag: running.clone(),
        playlist: playlist.clone(),
        sid,
    });
    emit_runs(&app);
    let max_calls = state.max_calls.load(Ordering::SeqCst).clamp(1, 24);
    let channelizer = state.use_channelizer.load(Ordering::SeqCst);
    let uv_quality = state.uv_quality.load(Ordering::SeqCst).clamp(1, 64);
    let catalog = state.catalog.clone();
    let filters = playlists::filters_for(&app, &state, playlist.as_deref());
    let units = state.units.clone();
    let unit_rules = state.unit_rules.clone();
    let learn_aliases = state.learn_aliases.clone();
    let record_policy = state.record_policy.clone();
    let stream_policy = state.stream_policy.clone();
    let upload_policy = state.upload_policy.clone();
    let name_template = state.names.lock().unwrap().template.clone();
    let db = state.db.clone();
    let library_dir = state.library_dir.lock().unwrap().clone();
    let archive_mode = state.archive_mode.clone();
    let format = state.format.lock().unwrap().clone();
    let spectrum = state.spectrum.clone();
    let streamer = state.streamer.clone();
    let uploader = state.uploader.clone();
    let audio = if play { state.audio() } else { None };
    let handle = std::thread::spawn(move || {
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<(), String> {
            // A radio that cannot span the requested band centre and the
            // control channel (an RTL-SDR at 2.4 MSPS covers ±1.2 MHz) is
            // centred on the control channel instead; calls outside that
            // span are reported as out of band rather than failing to start.
            let norm = hs_core::dsp::resample::normalize_ratio(rate)
                .map(|(_, _, r)| r)
                .unwrap_or(rate);
            let mut freq = freq;
            if (control - freq).abs() >= norm * 0.4 {
                emit_run(&app, run_id, &label, follow::FollowEvent::Notice {
                        text: format!(
                            "band centre {:.4} MHz can't reach the control channel at this rate — centred on {:.4} MHz instead (±{:.2} MHz)",
                            freq / 1e6,
                            control / 1e6,
                            norm * 0.4 / 1e6
                        ),
                    });
                freq = control;
            }
            // The radio: joined if another run already reads it and this
            // control channel sits inside its capture, opened otherwise.
            let src = {
                let st = app.state::<AppState>();
                let _serial = st.start_lock.lock().unwrap();
                join_idle_radios(&st);
                let mut radios = st.radios.lock().unwrap();
                match radios.get(&radio_key).filter(|r| r.tee.alive()) {
                    Some(r) => {
                        let half = r.norm_rate * 0.4;
                        if (control - r.center_hz).abs() >= half {
                            return Err(format!(
                                "{} is tuned to {:.4} MHz (±{:.2} MHz) for another system, and {:.4} MHz is outside that — stop the other system first, or start both together so the band centre covers both",
                                r.name, r.center_hz / 1e6, half / 1e6, control / 1e6
                            ));
                        }
                        freq = r.center_hz;
                        let s = r.tee.subscribe().ok_or("the shared radio closed")?;
                        emit_run(&app, run_id, &label, follow::FollowEvent::Notice {
                            text: format!("sharing {} with the other system (band centre {:.4} MHz)", r.name, freq / 1e6),
                        });
                        s
                    }
                    None => {
                        let primary_setting = devices::settings_for(&app, &source, device.as_deref()).gain_setting(&source);
                        let (raw, h) = open_device_with_gain(&source, device.as_deref(), ppm_tune(freq, ppm), rate, gain, primary_setting)?;
                        st.gain_handles.lock().unwrap().insert(radio_key.clone(), h);
                        // Normalise once here (10 → 9.6 MSPS on an Airspy) so
                        // every follower on this radio sees a clean rate and
                        // the resample is not repeated per run.
                        let norm = hs_core::stream::Normalized::new(raw);
                        let norm_rate = hs_source::SdrSource::sample_rate(&norm);
                        let tee = Arc::new(hs_core::stream::Tee::new(norm, 65536));
                        let s = tee.subscribe().ok_or("the radio closed at once")?;
                        radios.insert(radio_key.clone(), Radio {
                            tee,
                            center_hz: freq,
                            norm_rate,
                            name: format!("{source} {}", device.clone().unwrap_or_default()).trim().to_string(),
                        });
                        s
                    }
                }
            };
            // Extra radios go through the same shared pool as the primary:
            // one already open (another run's primary, or its extra) is
            // joined at the centre it has; otherwise it is opened here and
            // left in the pool, so a run started next can read it too. That
            // is how two radios cover four sites: each run hosts its own
            // control channel and decodes voice from every open radio. A
            // failure to open one is reported, not fatal.
            let mut extras = Vec::new();
            for (i, x) in extra.clone().unwrap_or_default().into_iter().enumerate() {
                let label = x.label.clone().unwrap_or_else(|| format!("radio {}", i + 2));
                let key = format!("{}|{}", x.source, x.device.clone().unwrap_or_default());
                if key == radio_key {
                    continue;
                }
                let st = app.state::<AppState>();
                let _serial = st.start_lock.lock().unwrap();
                let mut radios = st.radios.lock().unwrap();
                let joined = radios
                    .get(&key)
                    .filter(|r| r.tee.alive())
                    .and_then(|r| r.tee.subscribe().map(|s| (r.center_hz, s)));
                match joined {
                    Some((center_hz, s)) => {
                        if (center_hz - x.center).abs() > 1.0 {
                            emit_run(&app, run_id, &label, follow::FollowEvent::Notice {
                                text: format!("{label} is already open at {:.4} MHz for another system — reading it there", center_hz / 1e6),
                            });
                        }
                        extras.push(follow::ExtraRadio { center_hz, label, src: Box::new(s) });
                    }
                    None => {
                        let setting = devices::settings_for(&app, &x.source, x.device.as_deref()).gain_setting(&x.source);
                        match open_device_with_gain(&x.source, x.device.as_deref(), ppm_tune(x.center, x.ppm), x.rate, x.gain, setting) {
                            Ok((raw, h)) => {
                                st.gain_handles.lock().unwrap().insert(key.clone(), h);
                                let norm = hs_core::stream::Normalized::new(raw);
                                let norm_rate = hs_source::SdrSource::sample_rate(&norm);
                                let tee = Arc::new(hs_core::stream::Tee::new(norm, 65536));
                                match tee.subscribe() {
                                    Some(s) => {
                                        radios.insert(key, Radio { tee, center_hz: x.center, norm_rate, name: label.clone() });
                                        extras.push(follow::ExtraRadio { center_hz: x.center, label, src: Box::new(s) });
                                    }
                                    None => emit_run(&app, run_id, &label, follow::FollowEvent::Notice { text: format!("{label} not used: it closed at once") }),
                                }
                            }
                            Err(e) => {
                                emit_run(&app, run_id, &label, follow::FollowEvent::Notice { text: format!("{label} not used: {e}") });
                            }
                        }
                    }
                }
            }
            let calls_dir = match calls_dir.filter(|d| !d.trim().is_empty()) {
                Some(d) => {
                    let d = std::path::PathBuf::from(shellexpand_home(&d));
                    std::fs::create_dir_all(&d)
                        .map_err(|e| format!("calls dir {}: {e}", d.display()))?;
                    Some(d)
                }
                None => None,
            };
            // Hang after a terminator; the lost-terminator timeout scales with
            // it but never drops below the engine's 2 s.
            let hang_secs = hang_ms.map(|ms| {
                let h = ms as f64 / 1000.0;
                (h, (h * 4.0).max(2.0))
            });
            let params = follow::FollowParams {
                max_calls,
                taken_controls,
                channelizer,
                modulation: modulation.unwrap_or_default(),
                uv_quality,
                center_hz: freq,
                control_hz: control,
                calls_dir: library_dir.or(calls_dir),
                hang_secs,
                system_name: system_name.unwrap_or_default(),
                sid,
                site_name: site_name.unwrap_or_default(),
                name_template,
                format,
                live: true,
            };
            let player = if play { audio } else { None };
            if play && player.is_none() {
                emit_run(&app, run_id, &label, follow::FollowEvent::Notice {
                        text: "no audio output device — calls are not being played".into(),
                    });
            }
            // Clone the connection handle and release the outer lock at once:
            // holding it for the whole run would block every library command
            // (search, stats, transcription, uploads) until Stop — the app
            // looked frozen.
            let db_conn = db.lock().unwrap().clone();
            let live = follow::Live {
                filters: &filters,
                units: &units,
                unit_rules: &unit_rules,
                record: &record_policy,
                db: db_conn.as_deref(),
                spectrum: Some(&spectrum),
            };
            let allowed = |p: &Mutex<Policy>, tg: u16| {
                p.lock().map(|s| policy_allows(&s, tg)).unwrap_or(true)
            };
            follow::run_with_extras(src, extras, &params, &catalog, &live, &running, &mut |ev| {
                if let follow::FollowEvent::Call {
                    id: Some(id),
                    wav: Some(wav),
                    tg,
                    name,
                    source,
                    unit_name,
                    freq_mhz,
                    secs,
                    start,
                    site,
                    emergency,
                    patched_with,
                    voice_frame_errors,
                    ..
                } = &ev
                {
                    if let Some(u) = uploader.lock().unwrap().as_ref().filter(|_| allowed(&upload_policy, *tg)) {
                        let meta = upload::tg_meta(&catalog, sid, *tg);
                        u.submit(upload::Job {
                            id: *id,
                            audio: wav.clone(),
                            start: *start,
                            secs: *secs,
                            tg: *tg,
                            tg_name: name.clone(),
                            tg_desc: meta.desc,
                            tg_tag: meta.tag,
                            unit: *source,
                            unit_name: unit_name.clone(),
                            freq_hz: (*freq_mhz * 1e6).round() as u64,
                            emergency: *emergency,
                            patched_with: patched_with.clone(),
                            system: params.system_name.clone(),
                            site: *site,
                            voice_frame_errors: *voice_frame_errors,
                            group: meta.group,
                        });
                    }
                }
                if let follow::FollowEvent::Call {
                    id,
                    secs,
                    tg,
                    name,
                    desc,
                    source,
                    unit_name,
                    talker_alias,
                    freq_mhz,
                    modulation,
                    emergency,
                    patched_with,
                    wav,
                    ..
                } = &ev
                {
                    // The system named the radio: keep that, unless the
                    // listener already did.
                    if learn_aliases.load(Ordering::SeqCst) {
                        if let (Some(alias), true) = (talker_alias.as_deref(), *source != 0) {
                            if units::learn(&app, app.state::<AppState>().inner(), sid, *source, alias) {
                                emit_run(&app, run_id, &label, follow::FollowEvent::Notice {
                                        text: format!("learned alias “{alias}” for radio {source}"),
                                    });
                            }
                        }
                    }
                    conversations::on_call(
                        &app,
                        &alerts::CallFacts {
                            id: *id,
                            start: library::now(),
                            tg: *tg,
                            tg_name: name.clone(),
                            tg_desc: desc.clone(),
                            unit: *source,
                            unit_name: unit_name.clone(),
                            secs: *secs,
                            emergency: *emergency,
                            audio: wav.clone(),
                            transcript: None,
                            system: params.system_name.clone(),
                        },
                    );
                    alerts::on_call(
                        &app,
                        &alerts::CallFacts {
                            id: *id,
                            start: library::now(),
                            tg: *tg,
                            tg_name: name.clone(),
                            tg_desc: desc.clone(),
                            unit: *source,
                            unit_name: unit_name.clone(),
                            secs: *secs,
                            emergency: *emergency,
                            audio: wav.clone(),
                            transcript: None,
                            system: params.system_name.clone(),
                        },
                    );
                    if let Some(h) = app.state::<AppState>().hook.lock().unwrap().as_ref() {
                        h.submit(hook::CallInfo {
                            id: *id,
                            start: library::now(),
                            secs: *secs,
                            tg: *tg,
                            tg_name: name.clone(),
                            unit: *source,
                            unit_name: unit_name.clone(),
                            talker_alias: talker_alias.clone(),
                            freq_hz: (*freq_mhz * 1e6).round() as u64,
                            modulation: modulation.clone(),
                            emergency: *emergency,
                            patched_with: patched_with.clone(),
                            system: params.system_name.clone(),
                            audio: wav.clone(),
                            sidecar: wav.as_ref().map(|w| {
                                std::path::Path::new(w)
                                    .with_extension("json")
                                    .to_string_lossy()
                                    .into_owned()
                            }),
                        });
                    }
                }
                if let follow::FollowEvent::Call { pcm, priority, tg, .. } = &ev {
                    if !pcm.is_empty() {
                        if let Some(st) = streamer.lock().unwrap().as_ref().filter(|_| allowed(&stream_policy, *tg)) {
                            st.feed(pcm);
                        }
                        if let Some(pl) = player.as_ref() {
                            if !archive_mode.load(Ordering::SeqCst) {
                                pl.play(pcm.clone(), *priority);
                            }
                        }
                        // Live audio to web/SSE clients (independent of the
                        // record policy — this is listening, not storage).
                        if let Some(tx) = app.state::<AppState>().web_frames.get() {
                            let _ = tx.send(crate::web::Frame::audio(*tg, *priority, pcm));
                        }
                    }
                }
                emit_run(&app, run_id, &label, ev);
            })
        }))
        .unwrap_or_else(|p| Err(format!("follow crashed: {}", panic_text(&p))));
        finish_follow_run(&app, run_id, res);
    });
    let _ = handle; // runs to completion on its own; the radio table tracks the device
    Ok(run_id)
}

fn panic_text(p: &Box<dyn std::any::Any + Send>) -> String {
    p.downcast_ref::<String>()
        .cloned()
        .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "unknown panic".into())
}

/// Common epilogue for capture/follow threads: clear live state and tell the
/// UI — but only if this run is still the current one.
fn finish_run(app: &AppHandle, my_gen: u64, res: Result<(), String>) {
    let state = app.state::<AppState>();
    if let Err(e) = res {
        let _ = app.emit("error", e);
    }
    if state.run_gen.load(Ordering::SeqCst) == my_gen {
        state.running.store(false, Ordering::SeqCst);
        *state.last_start.lock().unwrap() = None;
        playlists::clear_holds(&state);
        state.archive_mode.store(false, Ordering::SeqCst);
        let _ = app.emit("stopped", ());
    }
}

/// Epilogue of one follow run: drop it from the table, tell the UI, and
/// when it was the last one, go back to standby.
fn finish_follow_run(app: &AppHandle, run_id: u64, res: Result<(), String>) {
    let state = app.state::<AppState>();
    if let Err(e) = res {
        let _ = app.emit("error", e);
    }
    let (remaining, orphaned) = {
        let mut runs = state.runs.lock().unwrap();
        let mine = runs.iter().find(|r| r.id == run_id).and_then(|r| r.playlist.clone());
        runs.retain(|r| r.id != run_id);
        let still_used = runs.iter().any(|r| r.playlist == mine);
        (runs.len(), if still_used { None } else { Some(mine) })
    };
    // A hold dies with the last run of its playlist.
    if let Some(pl) = orphaned {
        if let Some(f) = state.filters.lock().unwrap().get(pl.as_deref().unwrap_or("")) {
            f.lock().unwrap().hold = None;
        }
    }
    emit_runs(app);
    if remaining == 0 {
        state.running.store(false, Ordering::SeqCst);
        playlists::clear_holds(&state);
        state.archive_mode.store(false, Ordering::SeqCst);
        let _ = app.emit("stopped", ());
    }
}

/// Take the previous run's thread handle (on the caller's thread, before the
/// new one is spawned) so the new thread can wait for the old radio to close
/// — an Airspy refuses a second open. Joining inside the new thread after
/// the handle store would race into joining itself.
fn take_previous(state: &AppState) -> Option<std::thread::JoinHandle<()>> {
    state.run_thread.lock().unwrap().take()
}

fn join_previous(prev: Option<std::thread::JoinHandle<()>>) {
    if let Some(h) = prev {
        let _ = h.join();
    }
}

/// `~/x` → `$HOME/x`.
fn shellexpand_home(p: &str) -> String {
    match (p.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => p.to_string(),
    }
}

/// Recorded-IQ format, chosen by the output file's extension. The live decode
/// reads the `f32` stream regardless; this only decides how the sink encodes
/// the same samples to disk. Native `cu8` (RTL-SDR) / `cs16` (Airspy-class)
/// is 2–4× smaller than `cf32` and is what the offline CLI already ingests.
#[derive(Clone, Copy)]
enum IqFmt {
    Cf32,
    Cs16,
    Cu8,
}

fn iq_fmt_from_path(p: &str) -> IqFmt {
    let lower = p.to_ascii_lowercase();
    if lower.ends_with(".cu8") {
        IqFmt::Cu8
    } else if lower.ends_with(".cs16") {
        IqFmt::Cs16
    } else {
        IqFmt::Cf32
    }
}

fn write_iq_block(f: &mut std::fs::File, block: &[f32], fmt: IqFmt) -> std::io::Result<()> {
    match fmt {
        IqFmt::Cf32 => {
            let mut b = Vec::with_capacity(block.len() * 4);
            for s in block {
                b.extend_from_slice(&s.to_le_bytes());
            }
            f.write_all(&b)
        }
        IqFmt::Cs16 => {
            let mut b = Vec::with_capacity(block.len() * 2);
            for s in block {
                let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                b.extend_from_slice(&v.to_le_bytes());
            }
            f.write_all(&b)
        }
        IqFmt::Cu8 => {
            let mut b = Vec::with_capacity(block.len());
            for s in block {
                b.push(((s.clamp(-1.0, 1.0) * 0.5 + 0.5) * 255.0).round() as u8);
            }
            f.write_all(&b)
        }
    }
}

/// `My Spot #2` → `my_spot_2` — a safe, readable filename stem.
fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('_') {
            out.push('_');
        }
    }
    let t = out.trim_matches('_').to_string();
    if t.is_empty() {
        "survey".to_string()
    } else {
        t
    }
}

#[allow(clippy::too_many_arguments)]
fn capture_loop(
    app: &AppHandle,
    running: &AtomicBool,
    catalog: &Mutex<rr::Catalogs>,
    source: &str,
    freq: f64,
    rate: f64,
    gain: Option<f64>,
    cqpsk: bool,
    eq: &str,
    record_iq: Option<String>,
    record_log: Option<String>,
    spectrum_cfg: Arc<Mutex<(usize, usize)>>,
    ppm: Option<f64>,
    device: Option<&str>,
) -> Result<(), String> {
    use hs_core::stream::{Buffered, Normalized};
    use hs_source::SdrSource;

    let raw = open_device(source, device, ppm_tune(freq, ppm), rate, gain)?;
    // Normalize an Airspy's 2.5/10 MSPS to 2.4/9.6 on the fly, and drain the
    // radio on its own thread so a busy UI frame never costs samples.
    let mut src = Buffered::new(Normalized::new(raw), 65536);
    // Everything downstream runs at the rate the source *delivers*.
    let rate = src.sample_rate();
    let mut dec = new_decoder(rate, cqpsk, eq);
    let iq_fmt = record_iq.as_deref().map(iq_fmt_from_path);
    let mut iq_file = match record_iq {
        Some(p) => Some(std::fs::File::create(&p).map_err(|e| format!("record IQ: {e}"))?),
        None => None,
    };

    let mut buf = vec![0f32; 65536 * 2];
    let mut blocks = 0u64;
    let mut total_pcm = 0usize;
    // ADC-overload watch, per status window: both front ends deliver full
    // scale as ±1.0, so samples pinned near the rails mean too much gain
    // (see the same counter in `follow::run_with_extras`).
    let mut clip_win = 0u64;
    let mut clip_total = 0u64;

    let mut last_status = std::time::Instant::now();
    let mut last_spectrum = std::time::Instant::now();
    while running.load(Ordering::SeqCst) {
        let n = match src.read(&mut buf) {
            Ok(0) => continue,
            Ok(n) => n,
            Err(hs_source::SourceError::Eof) => break,
            Err(e) => return Err(format!("radio stopped: {e:?}")),
        };
        let block = &buf[..n];
        clip_win += block.iter().filter(|s| s.abs() >= 0.98).count() as u64;
        clip_total += n as u64;

        if let Some(f) = iq_file.as_mut() {
            let _ = write_iq_block(f, block, iq_fmt.unwrap_or(IqFmt::Cf32));
        }

        let out = dec.process(block);
        blocks += 1;
        total_pcm += out.pcm.len();

        for g in &out.grants {
            let name = catalog.lock().unwrap().label(None, g.talkgroup);
            let _ = app.emit(
                "grant",
                GrantMsg {
                    tg: g.talkgroup,
                    name,
                    source: g.source_unit,
                    freq_mhz: g.freq_hz as f64 / 1e6,
                    encrypted: g.encrypted,
                },
            );
        }

        // Wall-clock paced: ~12 waterfall rows and 4 status updates a
        // second whatever the block size (6.8 ms at 9.6 MSPS).
        if last_spectrum.elapsed().as_millis() >= 80 {
            last_spectrum = std::time::Instant::now();
            let _ = app.emit(
                "spectrum",
                SpectrumMsg {
                    bins_db: {
                        let (n, avg) = *spectrum_cfg.lock().unwrap();
                        power_spectrum_avg(block, n, avg)
                    },
                },
            );
        }
        if last_status.elapsed().as_millis() >= 250 {
            last_status = std::time::Instant::now();
            let echo = dec.cqpsk_echo();
            let clip_pct = if clip_total > 0 {
                100.0 * clip_win as f32 / clip_total as f32
            } else {
                0.0
            };
            clip_win = 0;
            clip_total = 0;
            let _ = app.emit(
                "status",
                StatusMsg {
                    syncs: dec.diagnostics().syncs.len(),
                    grants: dec.diagnostics().grants.len(),
                    voice_secs: total_pcm as f64 / 8000.0,
                    blocks,
                    modulation: format!("{:?}", dec.modulation()),
                    lock: dec.cqpsk_lock().unwrap_or(-1.0),
                    echo_frac: echo.map(|e| e.echo_frac).unwrap_or(-1.0),
                    echo_spread_us: echo.map(|e| e.rms_spread_us()).unwrap_or(-1.0),
                    clip_pct,
                    sync_err: dec.diagnostics().mean_sync_errors(),
                    dropped: src.dropped(),
                    voice_quality: dec.last_voice_quality().map(|q| q.score()).unwrap_or(-1.0),
                },
            );
        }
    }

    if let Some(p) = record_log {
        let _ = std::fs::write(&p, dec.diagnostics().to_json());
    }
    Ok(())
}

/// One pinned, timed capture in a drive survey.
#[derive(serde::Deserialize, Clone)]
struct SurveySpec {
    source: String,
    freq: f64,
    rate: f64,
    gain: Option<f64>,
    cqpsk: bool,
    eq: String,
    ppm: Option<f64>,
    device: Option<String>,
    lat: f64,
    lon: f64,
    label: String,
    seconds: f64,
    corpus: String,
    /// "cs16" (default), "cu8", or "cf32".
    format: Option<String>,
}

/// One completed survey pin, as recorded to disk.
#[derive(serde::Serialize, Clone)]
struct SurveyEntry {
    id: String,
    label: String,
    lat: f64,
    lon: f64,
    t: i64,
    seconds: f64,
    freq: f64,
    rate: f64,
    source: String,
    iq: String,
    log: String,
}

/// Pinned, timed capture for a drive survey: records IQ + diagnostics for
/// `seconds` at a tapped location, stamps the pin into a per-capture sidecar
/// and the corpus `survey.json`, then emits `survey_done` with the entry
/// (success or error) so the UI can mark the pin regardless.
#[tauri::command]
fn survey_capture(
    app: AppHandle,
    state: State<AppState>,
    spec: SurveySpec,
) -> Result<SurveyEntry, String> {
    if state.running.swap(true, Ordering::SeqCst) {
        return Err("already capturing".into());
    }
    let running = Arc::new(AtomicBool::new(true));
    *state.run_flag.lock().unwrap() = Some(running.clone());
    let catalog = state.catalog.clone();
    let spectrum_cfg = state.spectrum.clone();
    let my_gen = state.run_gen.fetch_add(1, Ordering::SeqCst) + 1;
    let prev = take_previous(&state);

    let corpus = shellexpand_home(&spec.corpus);
    let dir = std::path::Path::new(&corpus);
    std::fs::create_dir_all(dir).map_err(|e| format!("survey dir: {e}"))?;
    let ext = match spec.format.as_deref() {
        Some("cu8") => "cu8",
        Some("cf32") => "cf32",
        _ => "cs16",
    };
    let t = library::now();
    let stem = format!(
        "{}_{:.5}_{:.5}_{}",
        slugify(&spec.label),
        spec.lat,
        spec.lon,
        t
    );
    let iq_path = dir
        .join(format!("{stem}.{ext}"))
        .to_string_lossy()
        .into_owned();
    let log_path = dir
        .join(format!("{stem}.json"))
        .to_string_lossy()
        .into_owned();

    // The IQ file holds what the capture loop *delivers*, and an Airspy's
    // 2.5/10 MSPS is normalized to 2.4/9.6 on the way in. Record that rate,
    // not the requested one: the first survey file was stamped 10 MSPS while
    // holding 9.6, and decoding it at the stamped rate put every voice
    // channel 4% too far from centre — the control channel at zero offset
    // decoded, every granted channel was silent.
    let delivered_rate = hs_core::dsp::resample::normalize_ratio(spec.rate)
        .map(|(_, _, r)| r)
        .unwrap_or(spec.rate);
    let entry = SurveyEntry {
        id: stem.clone(),
        label: spec.label.clone(),
        lat: spec.lat,
        lon: spec.lon,
        t,
        seconds: spec.seconds,
        freq: spec.freq,
        rate: delivered_rate,
        source: spec.source.clone(),
        iq: iq_path.clone(),
        log: log_path.clone(),
    };

    let source = spec.source.clone();
    let freq = spec.freq;
    let rate = spec.rate;
    let gain = spec.gain;
    let cqpsk = spec.cqpsk;
    let eq = spec.eq.clone();
    let ppm = spec.ppm;
    let device = spec.device.clone();
    let seconds = spec.seconds;

    let ret = entry.clone();
    let handle = std::thread::spawn(move || {
        join_previous(prev);
        join_idle_radios(&app.state::<AppState>());
        let timer_flag = running.clone();
        let _timer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs_f64(seconds.max(1.0)));
            timer_flag.store(false, Ordering::SeqCst);
        });
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            capture_loop(
                &app,
                &running,
                &catalog,
                &source,
                freq,
                rate,
                gain,
                cqpsk,
                &eq,
                Some(iq_path.clone()),
                Some(log_path.clone()),
                spectrum_cfg,
                ppm,
                device.as_deref(),
            )
        }))
        .unwrap_or_else(|p| Err(format!("capture crashed: {}", panic_text(&p))));
        write_survey(&entry);
        finish_run(&app, my_gen, res);
        let _ = app.emit("survey_done", entry);
    });
    *state.run_thread.lock().unwrap() = Some(handle);
    Ok(ret)
}

/// Write the per-pin sidecar (`<stem>.survey.json` beside the log) and append
/// the entry to the corpus `survey.json`, replacing any prior entry with the
/// same id so re-recording a pin doesn't duplicate it.
fn write_survey(entry: &SurveyEntry) {
    let sidecar = std::path::Path::new(&entry.log)
        .with_extension("survey.json")
        .to_string_lossy()
        .into_owned();
    if let Ok(s) = serde_json::to_string_pretty(entry) {
        let _ = std::fs::write(&sidecar, s);
    }
    let corpus = std::path::Path::new(&entry.log)
        .parent()
        .unwrap_or(std::path::Path::new("."));
    let manifest = corpus.join("survey.json");
    let mut list: Vec<serde_json::Value> = std::fs::read_to_string(&manifest)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    list.retain(|v| v.get("id").and_then(|i| i.as_str()) != Some(entry.id.as_str()));
    if let Ok(v) = serde_json::to_value(entry) {
        list.push(v);
    }
    if let Ok(s) = serde_json::to_string_pretty(&list) {
        let _ = std::fs::write(&manifest, s);
    }
}

/// Identify one survey pin to delete by its on-disk paths.
#[derive(serde::Deserialize)]
struct SurveyDelete {
    id: String,
    iq: String,
    log: String,
}

/// Delete one survey pin and everything collected at that position: the IQ
/// capture, its diagnostics log, the per-pin sidecar, and the manifest entry.
#[tauri::command]
fn survey_delete(spec: SurveyDelete) -> Result<(), String> {
    let iq = shellexpand_home(&spec.iq);
    let log = shellexpand_home(&spec.log);
    let sidecar = std::path::Path::new(&log)
        .with_extension("survey.json")
        .to_string_lossy()
        .into_owned();

    // The IQ capture, the diagnostics log, and the sidecar (if present).
    for p in [iq.as_str(), log.as_str(), sidecar.as_str()] {
        match std::fs::remove_file(p) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("survey_delete: remove {p}: {e}"),
        }
    }

    // Rewrite the corpus manifest without this id.
    let manifest = std::path::Path::new(&log)
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("survey.json");
    let list: Vec<serde_json::Value> = std::fs::read_to_string(&manifest)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let kept: Vec<serde_json::Value> = list
        .into_iter()
        .filter(|v| v.get("id").and_then(|i| i.as_str()) != Some(spec.id.as_str()))
        .collect();
    if let Ok(s) = serde_json::to_string_pretty(&kept) {
        let _ = std::fs::write(&manifest, s);
    }
    Ok(())
}

/// Decode an on-disk `.cf32` recording; emits grants + a final status.
#[tauri::command]
async fn decode_file(
    app: AppHandle,
    path: String,
    rate: f64,
    cqpsk: bool,
    eq: String,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let bytes = std::fs::read(&path).map_err(|e| format!("read {path}: {e}"))?;
        let iq: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        let mut dec = new_decoder(rate, cqpsk, &eq);
        let out = dec.process(&iq);
        let cat = state.catalog.lock().unwrap();
        for g in &out.grants {
            let name = cat.label(None, g.talkgroup);
            let _ = app.emit(
                "grant",
                GrantMsg {
                    tg: g.talkgroup,
                    name,
                    source: g.source_unit,
                    freq_mhz: g.freq_hz as f64 / 1e6,
                    encrypted: g.encrypted,
                },
            );
        }
        let _ = app.emit(
            "status",
            StatusMsg {
                syncs: dec.diagnostics().syncs.len(),
                grants: out.grants.len(),
                voice_secs: out.pcm.len() as f64 / 8000.0,
                blocks: 1,
                modulation: format!("{:?}", dec.modulation()),
                lock: dec.cqpsk_lock().unwrap_or(-1.0),
                echo_frac: dec.cqpsk_echo().map(|e| e.echo_frac).unwrap_or(-1.0),
                echo_spread_us: dec.cqpsk_echo().map(|e| e.rms_spread_us()).unwrap_or(-1.0),
                // File replay: the recording's scale is unknown, so rail
                // detection would be meaningless.
                clip_pct: 0.0,
                sync_err: dec.diagnostics().mean_sync_errors(),
                dropped: 0,
                voice_quality: dec.last_voice_quality().map(|q| q.score()).unwrap_or(-1.0),
            },
        );
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Serialize, Clone)]
struct DecoderEventMsg {
    /// Short machine tag: "squelch", "dcs", "ani", "status", "gps", "grant".
    kind: String,
    /// Human-readable line for the event feed.
    text: String,
}

#[derive(Serialize, Clone)]
struct DecoderDoneMsg {
    decoder: String,
    /// Path to the WAV of recovered audio, if any was decoded.
    audio: Option<String>,
    audio_secs: f64,
    events: usize,
}

/// Render a decoder event for the app's event feed.
fn describe_event(ev: &hs_decoders::DecoderEvent) -> (String, String) {
    use hs_decoders::DecoderEvent as E;
    match ev {
        E::SquelchOpen => ("squelch".into(), "squelch open".into()),
        E::SquelchClose => ("squelch".into(), "squelch close".into()),
        E::Dcs { code, inverted } => (
            "dcs".into(),
            format!(
                "DCS code {code:03o}{}",
                if *inverted { " (inverted)" } else { "" }
            ),
        ),
        E::Ani { id, op } => (
            "ani".into(),
            match op {
                Some(o) => format!("ANI {id} ({o})"),
                None => format!("ANI {id}"),
            },
        ),
        E::Status { id, status } => ("status".into(), format!("status from {id}: {status}")),
        E::Gps { id, lat, lon } => ("gps".into(), format!("GPS {id}: {lat:.5}, {lon:.5}")),
        E::Grant {
            talkgroup, freq_hz, ..
        } => (
            "grant".into(),
            match freq_hz {
                Some(f) => format!("grant TG {talkgroup} → {:.4} MHz", f / 1e6),
                None => format!("grant TG {talkgroup}"),
            },
        ),
        E::Message(m) => ("message".into(), m.clone()),
        other => ("event".into(), format!("{other:?}")),
    }
}

/// Decode an on-disk `.cf32` recording with a non-P25 analog decoder (AM,
/// NBFM, DCS, …). Emits typed `decoderevent`s, writes the recovered audio to a
/// WAV beside the input, and finishes with a `decoderdone`.
#[tauri::command]
async fn decode_file_analog(
    app: AppHandle,
    path: String,
    rate: f64,
    decoder: String,
    squelch: f32,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let kind = hs_decoders::DecoderKind::from_name(&decoder)
            .ok_or_else(|| format!("unknown decoder '{decoder}'"))?;
        let bytes = std::fs::read(&path).map_err(|e| format!("read {path}: {e}"))?;
        let iq: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        let mut dec = hs_core::decoders::build(kind, rate, 0.0, squelch.clamp(0.0, 1.0))?;
        let out = dec.process(&iq);

        for ev in &out.events {
            let (kind, text) = describe_event(ev);
            let _ = app.emit("decoderevent", DecoderEventMsg { kind, text });
        }

        let audio_rate = dec.audio_rate();
        let audio_path = if out.audio.is_empty() {
            None
        } else {
            let p = format!("{path}.{}.wav", kind.name());
            hs_core::wav::write_wav(&p, audio_rate, &out.audio)
                .map_err(|e| format!("write {p}: {e}"))?;
            Some(p)
        };
        let _ = app.emit(
            "decoderdone",
            DecoderDoneMsg {
                decoder: kind.label().into(),
                audio: audio_path,
                audio_secs: out.audio.len() as f64 / audio_rate as f64,
                events: out.events.len(),
            },
        );
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Map the UI's equalizer selector to an `EqMode`. `cma` is the shipping
/// CQPSK default (the thesis); `dfe` adds decision feedback for the deep-null
/// simulcast burst; `bypass` is the conventional detect-first receiver.
fn eq_mode(eq: &str) -> EqMode {
    match eq {
        "dfe" => EqMode::Dfe,
        "cma" => EqMode::Enabled,
        _ => EqMode::Bypass,
    }
}

fn new_decoder(rate: f64, cqpsk: bool, eq: &str) -> ChannelDecoder {
    if cqpsk {
        ChannelDecoder::with_offset(rate, Modulation::Cqpsk, eq_mode(eq), 0.0)
    } else {
        ChannelDecoder::new(rate, EqMode::Bypass)
    }
}

/// Power spectrum (dB, DC-centered) of the first `n` complex samples of an
/// interleaved-IQ block, via a small direct DFT — enough for a waterfall.
/// `n`-bin power spectrum (dB) of an interleaved-IQ block: radix-2 FFT over
/// a Hann window, averaged over up to `avg` consecutive frames, DC in the
/// middle. Sized for a waterfall row, not for measurement.
fn power_spectrum_avg(block: &[f32], n: usize, avg: usize) -> Vec<f32> {
    let n = n.clamp(256, 4096).next_power_of_two();
    let pairs = block.len() / 2;
    let frames = (pairs / n).clamp(1, avg.max(1));
    if pairs < n {
        return vec![-100.0; n];
    }
    let win: Vec<f32> = (0..n)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos())
        .collect();
    let mut acc = vec![0.0f32; n];
    for f in 0..frames {
        let base = f * n * 2;
        let mut re: Vec<f32> = (0..n).map(|i| block[base + 2 * i] * win[i]).collect();
        let mut im: Vec<f32> = (0..n).map(|i| block[base + 2 * i + 1] * win[i]).collect();
        fft_in_place(&mut re, &mut im);
        for i in 0..n {
            acc[i] += re[i] * re[i] + im[i] * im[i];
        }
    }
    let scale = 1.0 / (frames as f32 * (n as f32 * 0.5).powi(2));
    let mut out: Vec<f32> = acc
        .iter()
        .map(|p| 10.0 * (p * scale + 1e-12).log10())
        .collect();
    out.rotate_left(n / 2);
    out
}

/// Iterative radix-2 Cooley–Tukey, in place. `re.len()` must be a power of 2.
fn fft_in_place(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * std::f32::consts::PI / len as f32;
        let (wr, wi) = (ang.cos(), ang.sin());
        let mut i = 0;
        while i < n {
            let (mut cr, mut ci) = (1.0f32, 0.0f32);
            for k in 0..len / 2 {
                let (ur, ui) = (re[i + k], im[i + k]);
                let (xr, xi) = (re[i + k + len / 2], im[i + k + len / 2]);
                let (vr, vi) = (xr * cr - xi * ci, xr * ci + xi * cr);
                re[i + k] = ur + vr;
                im[i + k] = ui + vi;
                re[i + k + len / 2] = ur - vr;
                im[i + k + len / 2] = ui - vi;
                let ncr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = ncr;
            }
            i += len;
        }
        len <<= 1;
    }
}

fn main() {
    tiles::register(tauri::Builder::default())
        .manage(AppState::default())
        .setup(|app| {
            crate::secrets::init(app.handle());
            crate::web::spawn(app.handle().clone());
            // A talkgroup catalog downloaded earlier is loaded on start.
            let state = app.state::<AppState>();
            *state.catalog.lock().unwrap() = rr::saved_catalog(app.handle());
            *state.units.lock().unwrap() = units::load(app.handle());
            *state.unit_rules.lock().unwrap() = units::load_rules(app.handle());
            if let Some(n) = app
                .path()
                .app_config_dir()
                .ok()
                .and_then(|d| std::fs::read_to_string(d.join("names.json")).ok())
                .and_then(|t| serde_json::from_str(&t).ok())
            {
                *state.names.lock().unwrap() = n;
            }
            // Transcript corrections: new flat format wins; fall back to the old
            // per-TG map and convert it (None = global rules, Some = per-TG).
            let loaded = app
                .path()
                .app_config_dir()
                .ok()
                .and_then(|d| std::fs::read_to_string(d.join("corrections.json")).ok())
                .and_then(|t| serde_json::from_str::<Vec<(Option<u16>, String, String)>>(&t).ok())
                .or_else(|| {
                    app.path()
                        .app_config_dir()
                        .ok()
                        .and_then(|d| std::fs::read_to_string(d.join("tg_corrections.json")).ok())
                        .and_then(|t| {
                            serde_json::from_str::<
                                std::collections::HashMap<u16, Vec<(String, String)>>,
                            >(&t)
                            .ok()
                        })
                        .map(|old| {
                            old.into_iter()
                                .flat_map(|(tg, pairs)| {
                                    pairs.into_iter().map(move |(a, b)| (Some(tg), a, b))
                                })
                                .collect()
                        })
                });
            if let Some(c) = loaded {
                *state.corrections.lock().unwrap() = c;
            }
            state.max_calls.store(12, Ordering::SeqCst);
            state.use_channelizer.store(true, Ordering::SeqCst);
            state.uv_quality.store(16, Ordering::SeqCst);
            *state.alerts.lock().unwrap() = alerts::load(app.handle());
            *state.conversations.lock().unwrap() = conversations::load(app.handle());
            *state.remotes.lock().unwrap() = remotes::load(app.handle());
            conversations::spawn_ticker(app.handle().clone());
            *state.digests.lock().unwrap() = digest::load(app.handle());
            digest::spawn_ticker(app.handle().clone());
            *state.analyzers.lock().unwrap() = analyzers::load(app.handle());
            *state.dispatch.lock().unwrap() = dispatch::load(app.handle());
            *state.retention.lock().unwrap() = retention::load(app.handle());
            retention::spawn_ticker(app.handle().clone());
            let hk = hook::load_settings(app.handle());
            if hk.enabled {
                *state.hook.lock().unwrap() = Some(hook::start(app.handle().clone(), hk));
            }
            // The call library lives in the app's data directory.
            if let Ok(base) = app.path().app_data_dir() {
                let lib = base.join("library");
                match library::open(&lib) {
                    Ok(c) => {
                        upload::ensure_schema(&c);
                        dispatch::ensure_schema(&c);
                        conversations::ensure_schema(&c);
                        events::ensure_schema(&c);
                        *state.db.lock().unwrap() = Some(Arc::new(Mutex::new(c)));
                        *state.library_dir.lock().unwrap() = Some(lib.join("calls"));
                    }
                    Err(e) => eprintln!("library: {e}"),
                }
            }
            state.transcriber.lock().unwrap().settings = transcribe::load_settings(app.handle());
            if let Some(f) = app
                .path()
                .app_config_dir()
                .ok()
                .and_then(|d| std::fs::read_to_string(d.join("format.json")).ok())
                .and_then(|t| serde_json::from_str(&t).ok())
            {
                *state.format.lock().unwrap() = f;
            }
            transcribe::spawn_pump(app.handle().clone());
            stream::autostart(app.handle());
            status::on_start(app.handle());
            let up = upload::load_settings(app.handle());
            if up.rdio.enabled || up.openmhz.enabled || up.broadcastify.enabled {
                *state.uploader.lock().unwrap() = Some(upload::start(app.handle().clone(), up));
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            load_catalog,
            start_capture,
            stop_capture,
            survey_capture,
            survey_delete,
            decode_file,
            decode_file_analog,
            start_follow,
            dual::dual_start,
            playlists::set_lockout,
            playlists::set_allowlist,
            playlists::set_hold,
            playlists::set_priorities,
            playlists::set_lockout_ranges,
            playlists::set_priority_ranges,
            playlists::sites_list,
            playlists::site_save,
            playlists::site_delete,
            corrections_get,
            corrections_set,
            set_volume,
            get_volume,
            names_get,
            names_set,
            names_preview,
            set_learn_aliases,
            set_policies,
            set_max_calls,
            set_queue_limit,
            set_channelizer,
            set_uv_quality,
            tg_latest_call,
            devices::devices_list,
            devices::devices_get,
            devices::devices_set,
            devices::gain_live,
            alerts::alerts_get,
            alerts::alerts_set,
            alerts::alerts_test,
            alerts::alerts_log,
            alerts::telegram_save,
            alerts::ollama_models,
            alerts::ollama_capabilities,
            connections::telegram_verify,
            connections::telegram_discover,
            connections::telegram_test_destination,
            conversations::conversations_get,
            conversations::conversations_set,
            conversations::conversations_state,
            conversations::conversation_test,
            conversations::conversation_resend,
            conversations::conversations_list,
            conversations::conversation_get,
            conversations::conversation_delete,
            conversations::conversations_stats,
            digest::digests_get,
            digest::digests_set,
            digest::digests_log,
            digest::digest_test,
            analyzers::analyzers_get,
            analyzers::analyzers_set,
            analyzers::analyzers_log,
            analyzers::analyzer_templates,
            analyzers::analyzer_template_import,
            analyzers::analyzer_template_export,
            analyzers::analyzer_test,
            analyzers::analyzer_cloud_get,
            analyzers::analyzer_cloud_save,
            analyzers::analyzer_cloud_clear_key,
            dispatch::dispatch_get,
            dispatch::dispatch_set,
            dispatch::dispatch_log,
            dispatch::dispatch_test,
            dispatch::dispatch_backfill,
            dispatch::dispatch_geocode,
            dispatch::dispatch_regeocode,
            dispatch::incidents_list,
            dispatch::incident_get,
            dispatch::incident_delete,
            dispatch::incident_locate,
            events::events_list,
            events::events_stats,
            channels::channel_activity,
            channels::channel_sets_get,
            channels::channel_sets_set,
            retention::retention_get,
            retention::retention_set,
            retention::retention_migrate,
            retention::retention_preview,
            retention::retention_apply,
            retention::retention_usage,
            hook::hook_get,
            hook::hook_configure,
            hook::hook_test,
            units::unit_rules_list,
            units::unit_rules_set,
            units::unit_resolve,
            rr::catalog_user_set,
            rr::save_text,
            skip_call,
            replay_last,
            clear_queue,
            audio_queued,
            units::units_list,
            units::unit_set,
            units::units_import,
            library_search,
            library_get,
            library_star,
            library_set_edited,
            library_stats,
            library_prune,
            library_export,
            library_play,
            set_archive_mode,
            format_get,
            format_set,
            library_reencode,
            stream::stream_get,
            stream::stream_configure,
            sysstat::sys_status,
            upload::uploads_get,
            upload::uploads_configure,
            upload::uploads_test,
            upload::upload_call,
            spectrum_set,
            transcribe::transcribe_probe,
            transcribe::transcribe_configure,
            transcribe::transcribe_call,
            models::transcribe_models,
            models::transcribe_delete,
            transcribe::transcribe_download,
            play_wav,
            ui_log,
            rr::rr_settings,
            rr::rr_save,
            rr::rr_download,
            rr::catalogs_list,
            rr::catalog_remove,
            rr::catalog_rows,
            rr::catalog_lookup,
            rr::rr_states,
            rr::rr_state,
            rr::rr_county,
            rr::rr_zip,
            playlists::playlists_list,
            playlists::playlist_save,
            playlists::playlist_delete,
            web::web_access_get,
            remotes::remotes_get,
            remotes::remotes_set,
            remotes::remotes_scan,
            remotes::remote_token_set,
            remotes::remote_open,
            runs_list,
            stop_run
        ])
        .build(tauri::generate_context!())
        .expect("error while running HoosierSDR")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                status::on_exit(app);
            }
        });
}

#[cfg(test)]
mod corrections_tests {
    use super::apply_corrections;

    #[test]
    fn corrections_are_word_boundary_and_case_insensitive() {
        let rules = vec![("Rirey".to_string(), "Riley".to_string())];
        assert_eq!(
            apply_corrections(&rules, "Unit 5 to Rirey station"),
            "Unit 5 to Riley station"
        );
        assert_eq!(
            apply_corrections(&rules, "rirey and RIREY"),
            "Riley and Riley"
        );
        assert_eq!(
            apply_corrections(&rules, "Rireyfield untouched"),
            "Rireyfield untouched"
        );
        assert_eq!(
            apply_corrections(&rules, "shirey untouched"),
            "shirey untouched"
        );
        // No rules → unchanged.
        assert_eq!(apply_corrections(&[], "hello"), "hello");
    }
}
