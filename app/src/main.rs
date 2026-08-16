//! HoosierSDR desktop app (Tauri v2) — a thin shell over `hs-core`.
//!
//! All decode logic lives in the workspace crates; this file only wires the
//! decoder + RTL-SDR capture to the web UI over Tauri commands and events.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

use hs_catalog::CsvCatalog;
use hs_core::decoder::{ChannelDecoder, EqMode};

#[derive(Default)]
struct AppState {
    running: Arc<AtomicBool>,
    catalog: Arc<Mutex<Option<CsvCatalog>>>,
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
}

#[derive(Serialize, Clone)]
struct SpectrumMsg {
    bins_db: Vec<f32>,
}

/// Load a RadioReference talkgroup CSV from a file path; returns the number
/// of talkgroups parsed.
#[tauri::command]
fn load_catalog(path: String, state: State<AppState>) -> Result<usize, String> {
    let text = std::fs::read_to_string(&path).map_err(|e| format!("read {path}: {e}"))?;
    let cat = CsvCatalog::parse(&text);
    let n = cat.len();
    *state.catalog.lock().unwrap() = Some(cat);
    Ok(n)
}

/// Stop an in-progress live capture.
#[tauri::command]
fn stop_capture(state: State<AppState>) {
    state.running.store(false, Ordering::SeqCst);
}

/// Start live capture from an RTL-SDR. Emits `grant`, `status`, and
/// `spectrum` events; on stop emits `stopped`, or `error` on failure.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
fn start_capture(
    app: AppHandle,
    state: State<AppState>,
    freq: f64,
    rate: f64,
    gain: Option<f64>,
    cqpsk: bool,
    record_iq: Option<String>,
    record_log: Option<String>,
) -> Result<(), String> {
    if state.running.swap(true, Ordering::SeqCst) {
        return Err("already capturing".into());
    }
    let running = state.running.clone();
    let catalog = state.catalog.clone();
    std::thread::spawn(move || {
        let res = capture_loop(
            &app, &running, &catalog, freq, rate, gain, cqpsk, record_iq, record_log,
        );
        if let Err(e) = res {
            let _ = app.emit("error", e);
        }
        running.store(false, Ordering::SeqCst);
        let _ = app.emit("stopped", ());
    });
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn capture_loop(
    app: &AppHandle,
    running: &AtomicBool,
    catalog: &Mutex<Option<CsvCatalog>>,
    freq: f64,
    rate: f64,
    gain: Option<f64>,
    cqpsk: bool,
    record_iq: Option<String>,
    record_log: Option<String>,
) -> Result<(), String> {
    use hs_source::rtlsdr::RtlSdrSource;
    use hs_source::SdrSource;

    let mut src = RtlSdrSource::open("driver=rtlsdr", freq, rate, gain)
        .map_err(|e| format!("open RTL-SDR: {e:?}"))?;
    let mut dec = new_decoder(rate, cqpsk);
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
                    bins_db: power_spectrum(block, 256),
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
) -> Result<(), String> {
    let bytes = std::fs::read(&path).map_err(|e| format!("read {path}: {e}"))?;
    let iq: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    let mut dec = new_decoder(rate, cqpsk);
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
        },
    );
    Ok(())
}

fn new_decoder(rate: f64, cqpsk: bool) -> ChannelDecoder {
    if cqpsk {
        ChannelDecoder::new_cqpsk(rate)
    } else {
        ChannelDecoder::new(rate, EqMode::Bypass)
    }
}

/// Power spectrum (dB, DC-centered) of the first `n` complex samples of an
/// interleaved-IQ block, via a small direct DFT — enough for a waterfall.
fn power_spectrum(block: &[f32], n: usize) -> Vec<f32> {
    let pairs = (block.len() / 2).min(n);
    if pairs == 0 {
        return vec![-120.0; n];
    }
    let re: Vec<f32> = (0..pairs).map(|i| block[2 * i]).collect();
    let im: Vec<f32> = (0..pairs).map(|i| block[2 * i + 1]).collect();
    let mut out = vec![0f32; pairs];
    let two_pi = std::f32::consts::TAU;
    for (k, o) in out.iter_mut().enumerate() {
        let mut sr = 0.0f32;
        let mut si = 0.0f32;
        for t in 0..pairs {
            let ang = -two_pi * (k as f32) * (t as f32) / pairs as f32;
            let (s, c) = ang.sin_cos();
            sr += re[t] * c - im[t] * s;
            si += re[t] * s + im[t] * c;
        }
        let power = (sr * sr + si * si) / (pairs * pairs) as f32;
        *o = 10.0 * (power + 1e-12).log10();
    }
    // fftshift so DC is centered.
    let half = pairs / 2;
    let mut shifted = vec![0f32; pairs];
    shifted[..half].copy_from_slice(&out[pairs - half..]);
    shifted[half..].copy_from_slice(&out[..pairs - half]);
    shifted
}

fn main() {
    tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            load_catalog,
            start_capture,
            stop_capture,
            decode_file
        ])
        .run(tauri::generate_context!())
        .expect("error while running HoosierSDR");
}
