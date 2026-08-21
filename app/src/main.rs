//! HoosierSDR desktop app (Tauri v2) — a thin shell over `hs-core`.
//!
//! All decode logic lives in the workspace crates; this file only wires the
//! decoder + RTL-SDR capture to the web UI over Tauri commands and events.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use hs_catalog::CsvCatalog;
use hs_core::decoder::{ChannelDecoder, EqMode, Modulation};

mod encode;
mod follow;
mod library;
mod player;
mod playlists;
mod rr;
mod stream;
mod transcribe;
mod units;

#[derive(Default)]
struct AppState {
    running: Arc<AtomicBool>,
    catalog: Arc<Mutex<Option<CsvCatalog>>>,
    /// Talkgroups the listener has locked out; read by the follower live.
    lockout: Arc<Mutex<std::collections::HashSet<u16>>>,
    /// The active playlist's talkgroups (`None` = follow everything).
    allowlist: Arc<Mutex<Option<std::collections::HashSet<u16>>>>,
    /// Hold: follow only this talkgroup until released.
    hold: Arc<Mutex<Option<u16>>>,
    /// Talkgroup priorities (1 high … 99 low; unlisted 50).
    priorities: Arc<Mutex<std::collections::HashMap<u16, u8>>>,
    /// Radio-ID aliases.
    units: units::Units,
    /// The audio thread, started on first use. `Some(None)` = no device.
    audio: Mutex<Option<Option<player::Audio>>>,
    /// The call library (opened at startup).
    db: Arc<Mutex<Option<Mutex<rusqlite::Connection>>>>,
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
#[tauri::command]
fn audio_queued(state: State<AppState>) -> usize {
    state.audio().map(|a| a.queued()).unwrap_or(0)
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

/// Play a library call through the speaker, ahead of anything queued.
#[tauri::command]
fn library_play(state: State<AppState>, id: i64) -> Result<(), String> {
    let path = with_db(&state, |c| library::get(c, id))?
        .and_then(|r| r.audio)
        .ok_or("no audio for that call")?;
    let pcm = encode::decode_to_pcm(std::path::Path::new(&path))?;
    state.audio().ok_or("no audio output device")?.play(pcm, 0);
    Ok(())
}

/// While archive playback is on, live calls are stored but not spoken.
#[tauri::command]
fn set_archive_mode(state: State<AppState>, on: bool) {
    state.archive_mode.store(on, Ordering::SeqCst);
    if on {
        if let Some(a) = state.audio() {
            a.skip();
        }
    }
}

/// Replay a saved call through the default audio device.
#[tauri::command]
fn play_wav(path: String, state: State<AppState>) -> Result<(), String> {
    let pcm = encode::decode_to_pcm(std::path::Path::new(&shellexpand_home(&path)))?;
    state.audio().ok_or("no audio output device")?.play(pcm, 0);
    Ok(())
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
        return Err("no talkgroups found in that CSV (expected RadioReference export columns)".into());
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
) -> Result<(), String> {
    if state.running.swap(true, Ordering::SeqCst) {
        return Err("already capturing".into());
    }
    let running = state.running.clone();
    let catalog = state.catalog.clone();
    let spectrum_cfg = state.spectrum.clone();
    std::thread::spawn(move || {
        let res = capture_loop(
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
        );
        if let Err(e) = res {
            let _ = app.emit("error", e);
        }
        running.store(false, Ordering::SeqCst);
        let _ = app.emit("stopped", ());
    });
    Ok(())
}

/// Open the radio the UI picked. `gain` is applied to an RTL-SDR; the Airspy
/// R2's firmware takes none (see `hs_source::airspy`).
fn open_source(
    source: &str,
    freq: f64,
    rate: f64,
    gain: Option<f64>,
) -> Result<Box<dyn hs_source::SdrSource + Send>, String> {
    use hs_source::airspy::AirspySource;
    use hs_source::rtlsdr::RtlSdrSource;
    Ok(match source {
        "airspy" => Box::new(
            AirspySource::open(None, freq, rate, gain)
                .map_err(|e| format!("open Airspy: {e:?}"))?,
        ),
        _ => Box::new(
            RtlSdrSource::open("driver=rtlsdr", freq, rate, gain)
                .map_err(|e| format!("open RTL-SDR: {e:?}"))?,
        ),
    })
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
) -> Result<(), String> {
    if state.running.swap(true, Ordering::SeqCst) {
        return Err("already running".into());
    }
    let running = state.running.clone();
    let catalog = state.catalog.clone();
    let lockout = state.lockout.clone();
    let allowlist = state.allowlist.clone();
    let hold = state.hold.clone();
    let priorities = state.priorities.clone();
    let units = state.units.clone();
    let db = state.db.clone();
    let library_dir = state.library_dir.lock().unwrap().clone();
    let archive_mode = state.archive_mode.clone();
    let format = state.format.lock().unwrap().clone();
    let spectrum = state.spectrum.clone();
    let streamer = state.streamer.clone();
    let audio = if play { state.audio() } else { None };
    std::thread::spawn(move || {
        let res = (|| -> Result<(), String> {
            let src = open_source(&source, freq, rate, gain)?;
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
                center_hz: freq,
                control_hz: control,
                calls_dir: library_dir.or(calls_dir),
                hang_secs,
                system_name: system_name.unwrap_or_default(),
                format,
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
            let db_guard = db.lock().unwrap();
            let live = follow::Live {
                lockout: &lockout,
                allowlist: &allowlist,
                hold: &hold,
                priorities: &priorities,
                units: &units,
                db: db_guard.as_ref(),
                spectrum: Some(&spectrum),
            };
            follow::run(src, &params, &catalog, &live, &running, &mut |ev| {
                if let follow::FollowEvent::Call { pcm, priority, .. } = &ev {
                    if !pcm.is_empty() {
                        if let Some(st) = streamer.lock().unwrap().as_ref() {
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
        })();
        if let Err(e) = res {
            let _ = app.emit("error", e);
        }
        running.store(false, Ordering::SeqCst);
        let _ = app.emit("stopped", ());
    });
    Ok(())
}

/// `~/x` → `$HOME/x`.
fn shellexpand_home(p: &str) -> String {
    match (p.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => p.to_string(),
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
) -> Result<(), String> {
    use hs_core::stream::{Buffered, Normalized};
    use hs_source::SdrSource;

    let raw = open_source(source, freq, rate, gain)?;
    // Normalize an Airspy's 2.5/10 MSPS to 2.4/9.6 on the fly, and drain the
    // radio on its own thread so a busy UI frame never costs samples.
    let mut src = Buffered::new(Normalized::new(raw), 65536);
    // Everything downstream runs at the rate the source *delivers*.
    let rate = src.sample_rate();
    let mut dec = new_decoder(rate, cqpsk, eq);
    let mut iq_file = match record_iq {
        Some(p) => Some(std::fs::File::create(&p).map_err(|e| format!("record IQ: {e}"))?),
        None => None,
    };

    let mut buf = vec![0f32; 65536 * 2];
    let mut blocks = 0u64;
    let mut total_pcm = 0usize;
    let mut since_spectrum = 0u32;

    while running.load(Ordering::SeqCst) {
        let n = match src.read(&mut buf) {
            Ok(0) => continue,
            Ok(n) => n,
            Err(_) => break,
        };
        let block = &buf[..n];

        if let Some(f) = iq_file.as_mut() {
            let mut bytes = Vec::with_capacity(block.len() * 4);
            for s in block {
                bytes.extend_from_slice(&s.to_le_bytes());
            }
            let _ = f.write_all(&bytes);
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

        // Spectrum ~10 Hz (every few blocks), status every block.
        since_spectrum += 1;
        if since_spectrum >= 3 {
            since_spectrum = 0;
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

    if let Some(p) = record_log {
        let _ = std::fs::write(&p, dec.diagnostics().to_json());
    }
    Ok(())
}

/// Decode an on-disk `.cf32` recording; emits grants + a final status.
#[tauri::command]
fn decode_file(
    app: AppHandle,
    state: State<AppState>,
    path: String,
    rate: f64,
    cqpsk: bool,
    eq: String,
) -> Result<(), String> {
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
            // A talkgroup catalog downloaded earlier is loaded on start.
            let state = app.state::<AppState>();
            if let Some(cat) = rr::saved_catalog(app.handle()) {
                *state.catalog.lock().unwrap() = Some(cat);
            }
            *state.units.lock().unwrap() = units::load(app.handle());
            // The call library lives in the app's data directory.
            if let Ok(base) = app.path().app_data_dir() {
                let lib = base.join("library");
                match library::open(&lib) {
                    Ok(c) => {
                        *state.db.lock().unwrap() = Some(Mutex::new(c));
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
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            load_catalog,
            start_capture,
            stop_capture,
            decode_file,
            start_follow,
            set_lockout,
            set_allowlist,
            set_hold,
            set_priorities,
            skip_call,
            replay_last,
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
            spectrum_set,
            transcribe::transcribe_probe,
            transcribe::transcribe_configure,
            transcribe::transcribe_call,
            play_wav,
            ui_log,
            rr::rr_settings,
            rr::rr_save,
            rr::rr_download,
            rr::catalogs_list,
            rr::catalog_remove,
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
