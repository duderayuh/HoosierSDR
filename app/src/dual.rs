//! Dual-SDR priority trunk-following for the desktop app: SDR A locks the
//! control channel and decodes grants; SDR B, a narrow radio, hops between
//! voice channels, always covering the highest-priority open call.
//!
//! Reuses the `FollowEvent` surface (CallStart / Call / Grant / Notice) so the
//! existing feed shows grants and plays completed calls without a new UI
//! path. The physical retune goes through the voice radio's `FreqHandle`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use hs_catalog::CsvCatalog;
use hs_core::decoder::{ChannelDecoder, EqMode};
use hs_core::dual::{DualSdrFollower, Retune};
use hs_core::priority::PriorityMap;
use hs_core::stream::Buffered;
use hs_source::{FreqHandle, SdrSource};
use tauri::{AppHandle, Emitter, State};

use crate::follow::FollowEvent;

/// The app's shared catalog: `Arc<Mutex<Option<CsvCatalog>>>`.
type CatalogHandle = Arc<Mutex<Option<CsvCatalog>>>;

/// A call being followed on the voice radio.
struct CurrentCall {
    tg: u16,
    freq_hz: u64,
    priority: u8,
    /// Call keyup start time (unix seconds), for uploader stamping.
    start: i64,
}

fn mod_name(cqpsk: bool) -> String {
    if cqpsk {
        "CQPSK".into()
    } else {
        "C4FM".into()
    }
}

fn name_of(catalog: &CatalogHandle, tg: u16) -> String {
    catalog
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| c.label(tg))
        .unwrap_or_else(|| format!("TG {tg}"))
}

fn desc_of(catalog: &CatalogHandle, tg: u16) -> Option<String> {
    catalog
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|c| c.get(tg).and_then(|t| t.description.clone()))
}

/// Run the dual-SDR loop until `running` clears. `control_hz` is the control
/// channel frequency (also SDR B's park frequency). Emits `follow` events.
pub fn run(
    app: AppHandle,
    catalog: CatalogHandle,
    control_src: Box<dyn SdrSource + Send>,
    control_rate: f64,
    voice_src: Box<dyn SdrSource + Send>,
    voice_fh: FreqHandle,
    voice_rate: f64,
    control_hz: f64,
    cqpsk: bool,
    priority: PriorityMap,
    play: bool,
    running: Arc<AtomicBool>,
) -> Result<(), String> {
    let control = if cqpsk {
        ChannelDecoder::new_cqpsk(control_rate)
    } else {
        ChannelDecoder::new(control_rate, EqMode::Bypass)
    };

    let mut follower = DualSdrFollower::new(control, control_rate, priority.clone(), voice_rate);
    let mut control = Buffered::new(control_src, 65536);
    let mut voice = Buffered::new(voice_src, 65536);

    let player = if play { crate::player::spawn() } else { None };

    let mut current: Option<CurrentCall> = None;
    let mut pcm: Vec<i16> = Vec::new();

    let mut cbuf = vec![0.0f32; 65536 * 2];
    let mut vbuf = vec![0.0f32; 65536 * 2];

    loop {
        if !running.load(Ordering::SeqCst) {
            break;
        }

        match control.read(&mut cbuf) {
            Ok(n) if n > 0 => {
                let ev = follower.process_control(&cbuf[..n]);
                for g in &ev.grants {
                    let _ = app.emit(
                        "follow",
                        FollowEvent::Grant {
                            tg: g.talkgroup,
                            name: name_of(&catalog, g.talkgroup),
                            named: false,
                            freq_mhz: g.freq_hz as f64 / 1e6,
                            unit: g.source_unit,
                            encrypted: g.encrypted,
                        },
                    );
                }
                if let Some(r) = ev.retune {
                    handle_retune(
                        &app,
                        &catalog,
                        &r,
                        &voice_fh,
                        &mut follower,
                        control_hz,
                        cqpsk,
                        &priority,
                        &mut current,
                        &mut pcm,
                        player.as_ref(),
                    );
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }

        match voice.read(&mut vbuf) {
            Ok(n) if n > 0 => {
                let ev = follower.process_voice(&vbuf[..n]);
                pcm.extend_from_slice(&ev.pcm);
                if let Some(r) = ev.retune {
                    handle_retune(
                        &app,
                        &catalog,
                        &r,
                        &voice_fh,
                        &mut follower,
                        control_hz,
                        cqpsk,
                        &priority,
                        &mut current,
                        &mut pcm,
                        player.as_ref(),
                    );
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    if !pcm.is_empty() {
        finish_call(&app, &mut current, &mut pcm, player.as_ref());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn handle_retune(
    app: &AppHandle,
    catalog: &CatalogHandle,
    r: &Retune,
    voice_fh: &FreqHandle,
    follower: &mut DualSdrFollower,
    control_hz: f64,
    cqpsk: bool,
    priority: &PriorityMap,
    current: &mut Option<CurrentCall>,
    pcm: &mut Vec<i16>,
    player: Option<&crate::player::Audio>,
) {
    match r {
        Retune::Tune { freq_hz, talkgroup } => {
            // The previous call (if any) is over.
            finish_call(app, current, pcm, player);
            let pri = priority.lookup(*talkgroup);
            *current = Some(CurrentCall {
                tg: *talkgroup,
                freq_hz: *freq_hz,
                priority: pri,
                start: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0),
            });
            pcm.clear();
            let _ = app.emit(
                "follow",
                FollowEvent::CallStart {
                    tg: *talkgroup,
                    name: name_of(catalog, *talkgroup),
                    desc: desc_of(catalog, *talkgroup),
                    freq_mhz: *freq_hz as f64 / 1e6,
                    priority: pri,
                },
            );
            let _ = app.emit(
                "follow",
                FollowEvent::Notice {
                    text: format!(
                        "→ voice radio → {:.4} MHz ({} · {})",
                        *freq_hz as f64 / 1e6,
                        name_of(catalog, *talkgroup),
                        mod_name(cqpsk)
                    ),
                },
            );
            voice_fh.request(*freq_hz as f64);
            follower.retune_done(Some(*freq_hz));
        }
        Retune::Park => {
            finish_call(app, current, pcm, player);
            let _ = app.emit(
                "follow",
                FollowEvent::Notice {
                    text: "voice radio parked (no open calls)".into(),
                },
            );
            voice_fh.request(control_hz);
            follower.retune_done(None);
        }
    }
}

fn finish_call(
    app: &AppHandle,
    current: &mut Option<CurrentCall>,
    pcm: &mut Vec<i16>,
    player: Option<&crate::player::Audio>,
) {
    let Some(c) = current.take() else {
        pcm.clear();
        return;
    };
    let audio = std::mem::take(pcm);
    let secs = audio.len() as f64 / 8000.0;
    if let Some(pl) = player {
        if !audio.is_empty() {
            pl.play(audio.clone(), c.priority);
        }
    }
    let _ = app.emit(
        "follow",
        FollowEvent::Call {
            tg: c.tg,
            name: String::new(), // filled by the front end from its own lookup
            desc: None,
            service: None,
            category: None,
            source: 0,
            unit_name: None,
            freq_mhz: c.freq_hz as f64 / 1e6,
            modulation: String::new(),
            secs,
            start: c.start,
            site: None,
            emergency: false,
            encrypted: false,
            system: String::new(),
            site_name: String::new(),
            patched_with: Vec::new(),
            priority: c.priority,
            syncs_c4fm: 0,
            syncs_cqpsk: 0,
            voice_frame_errors: 0,
            talker_alias: None,
            wav: None,
            id: None,
            pcm: audio,
        },
    );
}

/// Start dual-SDR priority follow: open two radios, then run the loop on a
/// background thread. `device` pairs are the Airspy serial (hex) or Seify
/// args for an RTL-SDR.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub fn dual_start(
    app: AppHandle,
    state: State<crate::AppState>,
    control_source: String,
    control_device: Option<String>,
    control_rate: f64,
    voice_source: String,
    voice_device: Option<String>,
    voice_rate: f64,
    gain: Option<f64>,
    control: f64,
    cqpsk: bool,
    play: bool,
) -> Result<(), String> {
    if state.running.swap(true, Ordering::SeqCst) {
        return Err("already running".into());
    }
    if control <= 0.0 {
        state.running.store(false, Ordering::SeqCst);
        return Err("control channel frequency is required".into());
    }
    let running = Arc::new(AtomicBool::new(true));
    *state.run_flag.lock().unwrap() = Some(running.clone());

    let catalog = state.catalog.clone();
    let priorities = state.priorities.clone();
    let priority_ranges = state.priority_ranges.clone();

    let my_gen = state.run_gen.fetch_add(1, Ordering::SeqCst) + 1;
    let prev = crate::take_previous(&state);
    let handle = std::thread::spawn(move || {
        crate::join_previous(prev);
        let res =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<(), String> {
                // Open both radios. SDR B starts parked on the control channel.
                let ctrl_setting =
                    crate::devices::settings_for(&app, &control_source, control_device.as_deref())
                        .gain_setting(&control_source);
                let (control_src, _) = crate::open_device_with_gain(
                    &control_source,
                    control_device.as_deref(),
                    control,
                    control_rate,
                    gain,
                    ctrl_setting,
                )?;
                let voice_setting =
                    crate::devices::settings_for(&app, &voice_source, voice_device.as_deref())
                        .gain_setting(&voice_source);
                let (voice_src, _vh) = crate::open_device_with_gain(
                    &voice_source,
                    voice_device.as_deref(),
                    control,
                    voice_rate,
                    gain,
                    voice_setting,
                )?;
                let voice_fh = voice_src.freq_handle();

                // Priority: explicit entries + ranges, defaulting to 50 (matching
                // the single-wideband follow), then catalog base below it.
                let mut prio = PriorityMap::new();
                if let Some(cat) = catalog.lock().unwrap().as_ref() {
                    if let Ok(tgs) = hs_core::catalog::Catalog::talkgroups(cat, 0) {
                        for tg in tgs {
                            if let Some(p) = tg.priority {
                                prio.set_base(tg.id, p);
                            }
                        }
                    }
                }
                for (tg, p) in priorities.lock().unwrap().iter() {
                    prio.set_override(*tg, *p);
                }
                for (lo, hi, p) in priority_ranges.lock().unwrap().iter() {
                    for tg in *lo..=*hi {
                        prio.set_override(tg, *p);
                    }
                }

                let _ = app.emit(
                    "follow",
                    FollowEvent::Notice {
                        text: format!(
                            "dual-SDR: control {} @ {:.4} MHz, voice {} hopping",
                            control_source,
                            control / 1e6,
                            voice_source
                        ),
                    },
                );

                run(
                    app.clone(),
                    catalog,
                    control_src,
                    control_rate,
                    voice_src,
                    voice_fh,
                    voice_rate,
                    control,
                    cqpsk,
                    prio,
                    play,
                    running.clone(),
                )
            }))
            .unwrap_or_else(|p| Err(format!("dual crashed: {}", crate::panic_text(&p))));
        crate::finish_run(&app, my_gen, res);
    });
    *state.run_thread.lock().unwrap() = Some(handle);
    Ok(())
}
