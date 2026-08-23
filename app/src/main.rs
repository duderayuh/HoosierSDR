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
mod conversations;
mod devices;
mod dual;
mod encode;
mod follow;
mod hook;
mod library;
mod names;
mod player;
mod playlists;
mod rr;
mod secrets;
mod stream;
mod sysstat;
mod transcribe;
mod units;
mod upload;

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
    /// The previous run's thread, joined before a new radio is opened.
    run_thread: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
    catalog: Arc<Mutex<Option<CsvCatalog>>>,
    /// Talkgroups the listener has locked out; read by the follower live.
    lockout: Arc<Mutex<std::collections::HashSet<u16>>>,
    /// The active playlist's talkgroups (`None` = follow everything).
    allowlist: Arc<Mutex<Option<std::collections::HashSet<u16>>>>,
    /// Hold: follow only this talkgroup until released.
    hold: Arc<Mutex<Option<u16>>>,
    /// Talkgroup priorities (1 high … 99 low; unlisted 50).
    priorities: Arc<Mutex<std::collections::HashMap<u16, u8>>>,
    /// Locked-out and prioritised talkgroup ranges (inclusive).
    lockout_ranges: Arc<Mutex<Vec<(u16, u16)>>>,
    priority_ranges: Arc<Mutex<Vec<(u16, u16, u8)>>>,
    /// Per-talkgroup transcript corrections: tg → [(wrong, right)].
    tg_corrections: Arc<Mutex<std::collections::HashMap<u16, Vec<(String, String)>>>>,
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

/// Locked-out talkgroup ranges (inclusive), alongside the explicit lockout.
#[tauri::command]
fn set_lockout_ranges(ranges: Vec<(u16, u16)>, state: State<AppState>) {
    *state.lockout_ranges.lock().unwrap() = ranges;
}

/// Priority ranges (inclusive, 1 high … 99 low); explicit entries win.
#[tauri::command]
fn set_priority_ranges(ranges: Vec<(u16, u16, u8)>, state: State<AppState>) {
    *state.priority_ranges.lock().unwrap() = ranges;
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

/// Per-talkgroup transcript corrections (tg → [(wrong, right)]).
#[tauri::command]
fn tg_corrections_get(state: State<AppState>) -> Vec<(u16, Vec<(String, String)>)> {
    state
        .tg_corrections
        .lock()
        .unwrap()
        .iter()
        .map(|(k, v)| (*k, v.clone()))
        .collect()
}

#[tauri::command]
fn tg_corrections_set(
    app: AppHandle,
    state: State<AppState>,
    entries: Vec<(u16, Vec<(String, String)>)>,
) {
    let map: std::collections::HashMap<u16, Vec<(String, String)>> = entries
        .into_iter()
        .filter(|(_, v)| !v.is_empty())
        .collect();
    *state.tg_corrections.lock().unwrap() = map.clone();
    if let Ok(d) = app.path().app_config_dir() {
        let _ = std::fs::create_dir_all(&d);
        let _ = std::fs::write(
            d.join("tg_corrections.json"),
            serde_json::to_string_pretty(&map).unwrap_or_default(),
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
            system: "SAFE-T",
            site: "Marion",
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

/// Hold on one talkgroup (`None` releases). Overrides the playlist while set.
#[tauri::command]
fn set_hold(tg: Option<u16>, state: State<AppState>) {
    *state.hold.lock().unwrap() = tg;
}

/// Replace the talkgroup priority table.
#[tauri::command]
fn set_priorities(entries: Vec<(u16, u8)>, state: State<AppState>) {
    *state.priorities.lock().unwrap() = entries.into_iter().collect();
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

/// Replace the locked-out talkgroup set. Takes effect on the follower's next
/// block — a call of a newly locked talkgroup already up is dropped.
#[tauri::command]
fn set_lockout(tgs: Vec<u16>, state: State<AppState>) {
    *state.lockout.lock().unwrap() = tgs.into_iter().collect();
}

/// Restrict the follower to a playlist's talkgroups (`None` clears it).
#[tauri::command]
fn set_allowlist(tgs: Option<Vec<u16>>, state: State<AppState>) {
    *state.allowlist.lock().unwrap() = tgs.map(|t| t.into_iter().collect());
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
    state: State<AppState>,
    query: library::Query,
) -> Result<Vec<library::CallRow>, String> {
    with_db(&state, |c| library::search(c, &query))
}

#[tauri::command]
fn library_get(state: State<AppState>, id: i64) -> Result<Option<library::CallRow>, String> {
    with_db(&state, |c| library::get(c, id))
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
    /// Mean frame-sync bit errors (of 48) — the receiver's own decode quality.
    sync_err: f64,
    /// Samples/blocks lost between the radio and the decoder so far.
    dropped: u64,
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
    let merged = rr::merged_catalog(&app);
    let total = merged.as_ref().map_or(n, |c| c.len());
    *state.catalog.lock().unwrap() = merged;
    Ok(total)
}

/// Stop an in-progress live capture.
#[tauri::command]
fn stop_capture(state: State<AppState>) {
    state.running.store(false, Ordering::SeqCst);
    if let Some(f) = state.run_flag.lock().unwrap().as_ref() {
        f.store(false, Ordering::SeqCst);
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
    let running = Arc::new(AtomicBool::new(true));
    *state.run_flag.lock().unwrap() = Some(running.clone());
    let catalog = state.catalog.clone();
    let spectrum_cfg = state.spectrum.clone();
    let my_gen = state.run_gen.fetch_add(1, Ordering::SeqCst) + 1;
    let prev = take_previous(&state);
    let handle = std::thread::spawn(move || {
        join_previous(prev);
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
                .map_err(|e| format!("open Airspy: {e:?}"))?;
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
            let db = match setting {
                Some(GainSetting::Manual(db)) => Some(db),
                Some(GainSetting::Agc) => None,
                _ => gain,
            };
            let src = SoapyRtlSource::open(freq, rate, db)
                .map_err(|e| format!("open RTL-SDR (Soapy): {e:?}"))?;
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
                .map_err(|e| format!("open RTL-SDR: {e:?}"))?;
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
) -> Result<(), String> {
    if state.running.swap(true, Ordering::SeqCst) {
        return Err("already running".into());
    }
    let running = Arc::new(AtomicBool::new(true));
    *state.run_flag.lock().unwrap() = Some(running.clone());
    let max_calls = state.max_calls.load(Ordering::SeqCst).clamp(1, 24);
    let channelizer = state.use_channelizer.load(Ordering::SeqCst);
    let uv_quality = state.uv_quality.load(Ordering::SeqCst).clamp(1, 64);
    let catalog = state.catalog.clone();
    let lockout = state.lockout.clone();
    let allowlist = state.allowlist.clone();
    let hold = state.hold.clone();
    let priorities = state.priorities.clone();
    let lockout_ranges = state.lockout_ranges.clone();
    let priority_ranges = state.priority_ranges.clone();
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
    let my_gen = state.run_gen.fetch_add(1, Ordering::SeqCst) + 1;
    let prev = take_previous(&state);
    let handle = std::thread::spawn(move || {
        join_previous(prev);
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
                let _ = app.emit(
                    "follow",
                    follow::FollowEvent::Notice {
                        text: format!(
                            "band centre {:.4} MHz can't reach the control channel at this rate — centred on {:.4} MHz instead (±{:.2} MHz)",
                            freq / 1e6,
                            control / 1e6,
                            norm * 0.4 / 1e6
                        ),
                    },
                );
                freq = control;
            }
            let primary_setting = devices::settings_for(&app, &source, device.as_deref()).gain_setting(&source);
            let (src, h) = open_device_with_gain(&source, device.as_deref(), ppm_tune(freq, ppm), rate, gain, primary_setting)?;
            {
                let st = app.state::<AppState>();
                let mut hs = st.gain_handles.lock().unwrap();
                hs.clear();
                hs.insert(format!("{source}|{}", device.clone().unwrap_or_default()), h);
            }
            // Extra radios: a failure to open one is reported, not fatal.
            let mut extras = Vec::new();
            for (i, x) in extra.clone().unwrap_or_default().into_iter().enumerate() {
                let label = x.label.clone().unwrap_or_else(|| format!("radio {}", i + 2));
                let setting = devices::settings_for(&app, &x.source, x.device.as_deref()).gain_setting(&x.source);
                match open_device_with_gain(&x.source, x.device.as_deref(), ppm_tune(x.center, x.ppm), x.rate, x.gain, setting) {
                    Ok((src, h)) => {
                        let st = app.state::<AppState>();
                        st.gain_handles.lock().unwrap().insert(format!("{}|{}", x.source, x.device.clone().unwrap_or_default()), h);
                        extras.push(follow::ExtraRadio { center_hz: x.center, label, src })
                    }
                    Err(e) => {
                        let _ = app.emit("follow", follow::FollowEvent::Notice { text: format!("{label} not used: {e}") });
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
                channelizer,
                modulation: modulation.unwrap_or_default(),
                uv_quality,
                center_hz: freq,
                control_hz: control,
                calls_dir: library_dir.or(calls_dir),
                hang_secs,
                system_name: system_name.unwrap_or_default(),
                site_name: site_name.unwrap_or_default(),
                name_template,
                format,
                live: true,
            };
            let player = if play { audio } else { None };
            if play && player.is_none() {
                let _ = app.emit(
                    "follow",
                    follow::FollowEvent::Notice {
                        text: "no audio output device — calls are not being played".into(),
                    },
                );
            }
            // Clone the connection handle and release the outer lock at once:
            // holding it for the whole run would block every library command
            // (search, stats, transcription, uploads) until Stop — the app
            // looked frozen.
            let db_conn = db.lock().unwrap().clone();
            let live = follow::Live {
                lockout: &lockout,
                allowlist: &allowlist,
                hold: &hold,
                priorities: &priorities,
                lockout_ranges: &lockout_ranges,
                priority_ranges: &priority_ranges,
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
                    ..
                } = &ev
                {
                    if let Some(u) = uploader.lock().unwrap().as_ref().filter(|_| allowed(&upload_policy, *tg)) {
                        let group = catalog
                            .lock()
                            .ok()
                            .and_then(|cat| cat.as_ref().and_then(|c| c.get(*tg)).and_then(|t| t.category.clone()))
                            .unwrap_or_default();
                        u.submit(upload::Job {
                            id: *id,
                            audio: wav.clone(),
                            start: *start,
                            secs: *secs,
                            tg: *tg,
                            tg_name: name.clone(),
                            unit: *source,
                            unit_name: unit_name.clone(),
                            freq_hz: (*freq_mhz * 1e6).round() as u64,
                            emergency: *emergency,
                            patched_with: patched_with.clone(),
                            system: params.system_name.clone(),
                            site: *site,
                            group,
                        });
                    }
                }
                if let follow::FollowEvent::Call {
                    id,
                    secs,
                    tg,
                    name,
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
                            if units::learn(&app, app.state::<AppState>().inner(), *source, alias) {
                                let _ = app.emit(
                                    "follow",
                                    follow::FollowEvent::Notice {
                                        text: format!("learned alias “{alias}” for radio {source}"),
                                    },
                                );
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
                            unit: *source,
                            unit_name: unit_name.clone(),
                            secs: *secs,
                            emergency: *emergency,
                            audio: wav.clone(),
                            transcript: None,
                        },
                    );
                    alerts::on_call(
                        &app,
                        &alerts::CallFacts {
                            id: *id,
                            start: library::now(),
                            tg: *tg,
                            tg_name: name.clone(),
                            unit: *source,
                            unit_name: unit_name.clone(),
                            secs: *secs,
                            emergency: *emergency,
                            audio: wav.clone(),
                            transcript: None,
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
                    }
                }
                let _ = app.emit("follow", ev);
            })
        }))
        .unwrap_or_else(|p| Err(format!("follow crashed: {}", panic_text(&p))));
        finish_run(&app, my_gen, res);
    });
    *state.run_thread.lock().unwrap() = Some(handle);
    Ok(())
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
        *state.hold.lock().unwrap() = None;
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
    catalog: &Mutex<Option<CsvCatalog>>,
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

        if let Some(f) = iq_file.as_mut() {
            let _ = write_iq_block(f, block, iq_fmt.unwrap_or(IqFmt::Cf32));
        }

        let out = dec.process(block);
        blocks += 1;
        total_pcm += out.pcm.len();

        for g in &out.grants {
            let name = catalog
                .lock()
                .unwrap()
                .as_ref()
                .map(|c| c.label(g.talkgroup))
                .unwrap_or_else(|| format!("TG {}", g.talkgroup));
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
            let _ = app.emit(
                "status",
                StatusMsg {
                    syncs: dec.diagnostics().syncs.len(),
                    grants: dec.diagnostics().grants.len(),
                    voice_secs: total_pcm as f64 / 8000.0,
                    blocks,
                    modulation: format!("{:?}", dec.modulation()),
                    lock: dec.cqpsk_lock().unwrap_or(-1.0),
                    sync_err: dec.diagnostics().mean_sync_errors(),
                    dropped: src.dropped(),
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

    let entry = SurveyEntry {
        id: stem.clone(),
        label: spec.label.clone(),
        lat: spec.lat,
        lon: spec.lon,
        t,
        seconds: spec.seconds,
        freq: spec.freq,
        rate: spec.rate,
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
            let name = cat
                .as_ref()
                .map(|c| c.label(g.talkgroup))
                .unwrap_or_else(|| format!("TG {}", g.talkgroup));
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
                sync_err: dec.diagnostics().mean_sync_errors(),
                dropped: 0,
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
        "bypass" => EqMode::Bypass,
        _ => EqMode::Enabled,
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
    tauri::Builder::default()
        .manage(AppState::default())
        .setup(|app| {
            crate::secrets::init(app.handle());
            // A talkgroup catalog downloaded earlier is loaded on start.
            let state = app.state::<AppState>();
            if let Some(cat) = rr::saved_catalog(app.handle()) {
                *state.catalog.lock().unwrap() = Some(cat);
            }
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
            if let Some(c) = app
                .path()
                .app_config_dir()
                .ok()
                .and_then(|d| std::fs::read_to_string(d.join("tg_corrections.json")).ok())
                .and_then(|t| {
                    serde_json::from_str::<std::collections::HashMap<u16, Vec<(String, String)>>>(
                        &t,
                    )
                    .ok()
                })
            {
                *state.tg_corrections.lock().unwrap() = c;
            }
            state.max_calls.store(12, Ordering::SeqCst);
            state.use_channelizer.store(true, Ordering::SeqCst);
            state.uv_quality.store(16, Ordering::SeqCst);
            *state.alerts.lock().unwrap() = alerts::load(app.handle());
            *state.conversations.lock().unwrap() = conversations::load(app.handle());
            conversations::spawn_ticker(app.handle().clone());
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
            decode_file,
            start_follow,
            dual::dual_start,
            set_lockout,
            set_allowlist,
            set_hold,
            set_priorities,
            set_lockout_ranges,
            set_priority_ranges,
            tg_corrections_get,
            tg_corrections_set,
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
            conversations::conversations_get,
            conversations::conversations_set,
            conversations::conversations_state,
            conversations::conversation_test,
            conversations::conversation_resend,
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
            transcribe::transcribe_models,
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
            playlists::playlist_activate
        ])
        .run(tauri::generate_context!())
        .expect("error while running HoosierSDR");
}

#[cfg(test)]
mod corrections_tests {
    use super::apply_corrections;

    #[test]
    fn corrections_are_word_boundary_and_case_insensitive() {
        let rules = vec![("Rirey".to_string(), "Riley".to_string())];
        assert_eq!(apply_corrections(&rules, "Unit 5 to Rirey station"), "Unit 5 to Riley station");
        assert_eq!(apply_corrections(&rules, "rirey and RIREY"), "Riley and Riley");
        assert_eq!(apply_corrections(&rules, "Rireyfield untouched"), "Rireyfield untouched");
        assert_eq!(apply_corrections(&rules, "shirey untouched"), "shirey untouched");
        // No rules → unchanged.
        assert_eq!(apply_corrections(&[], "hello"), "hello");
    }
}
