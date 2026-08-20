//! Trunk following for the desktop app: the same engine the CLI's `--follow`
//! drives (`hs_core::follow::TrunkFollower`), with events instead of println.
//!
//! `run` is deliberately free of Tauri so it can be exercised headless over a
//! recording (see the tests) — the loop is verified on real off-air IQ; only
//! the button that starts it needs a human.

use hs_core::catalog::CsvCatalog;
use hs_core::follow::{in_band, measure_carrier, GrantGate, TrunkFollower};
use hs_core::stream::{Buffered, Normalized};
use hs_source::{SdrSource, SourceError};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};

pub struct FollowParams {
    /// Where the radio is tuned (band centre), Hz.
    pub center_hz: f64,
    /// Nominal control-channel frequency, Hz.
    pub control_hz: f64,
    /// Directory to write one WAV per completed call into, if any.
    pub calls_dir: Option<std::path::PathBuf>,
}

/// Everything the loop tells the front end. Serialized as `{kind: ...}`.
#[derive(Serialize, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FollowEvent {
    /// The control channel was found; following starts.
    Measured {
        control_mhz: f64,
        modulation: String,
        correction_hz: f64,
        rate: f64,
    },
    CallStart {
        tg: u16,
        name: String,
        freq_mhz: f64,
    },
    /// A call finished; its audio is in `pcm` (8 kHz mono), not serialized.
    Call {
        tg: u16,
        name: String,
        source: u32,
        freq_mhz: f64,
        modulation: String,
        secs: f64,
        wav: Option<String>,
        #[serde(skip)]
        pcm: Vec<i16>,
    },
    /// Something worth a line in the feed: out-of-band grant, encrypted
    /// call, control channel moved or lost.
    Notice {
        text: String,
    },
    Status {
        control_syncs: u32,
        calls: usize,
        out_of_band: u32,
        encrypted: u32,
        msps: f64,
        want_msps: f64,
        dropped: u64,
        elapsed_secs: f64,
    },
    Spectrum {
        bins_db: Vec<f32>,
    },
}

fn mod_name(m: Option<hs_core::decoder::Modulation>) -> String {
    match m {
        Some(hs_core::decoder::Modulation::C4fm) => "C4FM".into(),
        Some(hs_core::decoder::Modulation::Cqpsk) => "CQPSK".into(),
        None => "?".into(),
    }
}

/// Follow a trunk from `src` until `running` clears or the source ends.
/// `emit` receives every event in order, on this thread.
pub fn run<S: SdrSource + Send + 'static>(
    src: S,
    p: &FollowParams,
    catalog: Option<&CsvCatalog>,
    running: &AtomicBool,
    emit: &mut dyn FnMut(FollowEvent),
) -> Result<(), String> {
    let mut src = Normalized::new(src);
    let rate = src.sample_rate();
    in_band(rate, p.center_hz, p.control_hz)?;

    // Prime on ~3 s of air and measure where the control channel really is
    // (and which modulation it uses), reading the source directly so the
    // measurement's several seconds don't register as queue drops.
    let block = (rate as usize / 10) * 2;
    let mut buf = vec![0.0f32; block];
    let target = block * 30;
    let mut prime: Vec<f32> = Vec::with_capacity(target);
    while prime.len() < target {
        if !running.load(Ordering::SeqCst) {
            return Ok(());
        }
        match src.read(&mut buf) {
            Ok(0) => continue,
            Ok(n) => prime.extend_from_slice(&buf[..n]),
            Err(SourceError::Eof) => break,
            Err(e) => return Err(format!("capture error while measuring: {e:?}")),
        }
    }
    if prime.is_empty() {
        return Err("the source delivered no samples".into());
    }
    let Some((measured_hz, modulation)) = measure_carrier(&prime, rate, p.center_hz, p.control_hz)
    else {
        return Err(format!(
            "could not find control {:.4} MHz: nothing decoded within ±12.5 kHz of it on either \
             modulation. A site lists several control-capable frequencies but only one carries \
             the control channel at a time — scan the band to see which.",
            p.control_hz / 1e6
        ));
    };
    let mut f = TrunkFollower::new(rate, p.center_hz, p.control_hz, measured_hz, modulation);
    emit(FollowEvent::Measured {
        control_mhz: measured_hz / 1e6,
        modulation: mod_name(Some(modulation)),
        correction_hz: f.correction_hz(),
        rate,
    });

    let mut rep = Reporter {
        catalog,
        calls_dir: p.calls_dir.as_deref(),
        syncs: 0,
        calls: 0,
        oob: 0,
        enc: 0,
        gate: GrantGate::new(50),
    };
    // The primed IQ carries whatever the site granted while we measured;
    // decode it too, so a call that began during startup is not lost. The
    // follower runs far faster than real time, so this catches up quickly.
    let out = f.process(&prime);
    rep.report(out, emit);
    drop(prime);

    // Now hand the radio to a reader thread and decode from its queue. Any
    // drops the radio itself counted while we were measuring and catching up
    // aren't live problems; report relative to here. (Read the radio's own
    // counter before wrapping: the wrapper only learns it after its first
    // read.)
    let drop_base = src.dropped();
    let mut src = Buffered::new(src, 65536);
    let mut buf = vec![0.0f32; 65536 * 2];
    let start = std::time::Instant::now();
    let mut total_pairs = 0u64;
    let mut blocks = 0u64;
    let mut last_status = std::time::Instant::now();

    while running.load(Ordering::SeqCst) {
        let n = match src.read(&mut buf) {
            Ok(0) => continue,
            Ok(n) => n,
            Err(SourceError::Eof) => break,
            Err(e) => return Err(format!("capture error: {e:?}")),
        };
        let chunk = &buf[..n];
        total_pairs += (n / 2) as u64;
        blocks += 1;
        let out = f.process(chunk);
        rep.report(out, emit);
        if blocks.is_multiple_of(4) {
            emit(FollowEvent::Spectrum {
                bins_db: super::power_spectrum(chunk, 256),
            });
        }
        if last_status.elapsed().as_secs_f64() >= 1.0 {
            last_status = std::time::Instant::now();
            let secs = start.elapsed().as_secs_f64().max(1e-3);
            emit(rep.status(total_pairs, secs, rate, src.dropped().saturating_sub(drop_base)));
        }
    }
    // Flush calls still in progress when the run ends.
    let out = hs_core::follow::FollowOutput {
        completed: f.finish(),
        ..Default::default()
    };
    rep.report(out, emit);
    let secs = start.elapsed().as_secs_f64().max(1e-3);
    emit(rep.status(total_pairs, secs, rate, src.dropped().saturating_sub(drop_base)));
    Ok(())
}

/// Turns follower output into events and keeps the running counts.
struct Reporter<'a> {
    catalog: Option<&'a CsvCatalog>,
    calls_dir: Option<&'a std::path::Path>,
    syncs: u32,
    calls: usize,
    oob: u32,
    enc: u32,
    gate: GrantGate,
}

impl Reporter<'_> {
    fn name_of(&self, tg: u16) -> String {
        match self.catalog {
            Some(k) => k.label(tg),
            None => format!("TG {tg}"),
        }
    }

    fn status(&self, total_pairs: u64, secs: f64, rate: f64, dropped: u64) -> FollowEvent {
        FollowEvent::Status {
            control_syncs: self.syncs,
            calls: self.calls,
            out_of_band: self.oob,
            encrypted: self.enc,
            msps: total_pairs as f64 / secs / 1e6,
            want_msps: rate / 1e6,
            dropped,
            elapsed_secs: secs,
        }
    }

    fn report(&mut self, out: hs_core::follow::FollowOutput, emit: &mut dyn FnMut(FollowEvent)) {
        self.syncs += out.control_syncs;
        self.gate.tick();
        if let Some((old, new)) = out.control_moved {
            emit(FollowEvent::Notice {
                text: format!(
                    "control channel moved {:.4} → {:.4} MHz",
                    old as f64 / 1e6,
                    new as f64 / 1e6
                ),
            });
        }
        if let Some(alts) = &out.control_lost {
            let text = if alts.is_empty() {
                "control channel lost — nothing announced to move to".to_string()
            } else {
                format!(
                    "control channel lost; alternates outside this band: {}",
                    alts.iter()
                        .map(|a| format!("{:.4}", *a as f64 / 1e6))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            emit(FollowEvent::Notice { text });
        }
        for (tg, hz) in &out.started {
            emit(FollowEvent::CallStart {
                tg: *tg,
                name: self.name_of(*tg),
                freq_mhz: *hz as f64 / 1e6,
            });
        }
        for (tg, hz) in &out.grants_out_of_band {
            if self.gate.fresh(*hz) {
                self.oob += 1;
                emit(FollowEvent::Notice {
                    text: format!(
                        "{} on {:.4} MHz — outside the tuned band, not followed",
                        self.name_of(*tg),
                        *hz as f64 / 1e6
                    ),
                });
            }
        }
        for (tg, hz) in &out.grants_encrypted {
            if self.gate.fresh(*hz) {
                self.enc += 1;
                emit(FollowEvent::Notice {
                    text: format!("{} encrypted — skipped", self.name_of(*tg)),
                });
            }
        }
        for c in out.completed {
            self.calls += 1;
            let wav = self.calls_dir.and_then(|dir| {
                let path = dir.join(format!(
                    "{}_tg{}_{}.wav",
                    chrono_stamp(),
                    c.talkgroup,
                    (c.freq_hz as f64 / 1e6 * 10_000.0).round() as u64
                ));
                match hs_core::wav::write_wav(path.to_str()?, 8000, &c.pcm) {
                    Ok(()) => Some(path.to_string_lossy().into_owned()),
                    Err(e) => {
                        emit(FollowEvent::Notice {
                            text: format!("could not write {}: {e}", path.display()),
                        });
                        None
                    }
                }
            });
            emit(FollowEvent::Call {
                tg: c.talkgroup,
                name: self.name_of(c.talkgroup),
                source: c.source_unit,
                freq_mhz: c.freq_hz as f64 / 1e6,
                modulation: mod_name(c.modulation),
                secs: c.pcm.len() as f64 / 8000.0,
                wav,
                pcm: c.pcm,
            });
        }
    }
}

/// `YYYYMMDD-HHMMSS` local-ish stamp without pulling in a date crate: UTC.
fn chrono_stamp() -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Civil-from-days (Howard Hinnant), UTC.
    let days = (t / 86400) as i64;
    let secs = t % 86400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// An `airspy_rx -t 2` recording as a source, exactly as the CLI loads it.
    fn cs16_source(path: &str, rate: f64) -> Option<impl SdrSource + Send + 'static> {
        let bytes = std::fs::read(path).ok()?;
        let mut f32s: Vec<u8> = Vec::with_capacity(bytes.len() * 2);
        for c in bytes.chunks_exact(2) {
            let v = i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0;
            f32s.extend_from_slice(&v.to_le_bytes());
        }
        Some(hs_source::IqFileSource::new(
            std::io::Cursor::new(f32s),
            rate,
            851e6,
        ))
    }

    /// The whole app follow path over a real off-air Airspy capture (10 s,
    /// NAC 0x260, control at 851.5375 MHz, one voice channel in band and the
    /// rest granted outside it): finds the control channel, starts the
    /// in-band call, reports the out-of-band ones, and runs drop-free.
    #[test]
    fn follows_a_recorded_airspy_site() {
        let path =
            std::env::var("HOME").unwrap() + "/hoosier-field/live_airspy_851M_2500k_nac260.cs16";
        let Some(src) = cs16_source(&path, 2_500_000.0) else {
            eprintln!("no {path}; skipping");
            return;
        };
        let p = FollowParams {
            center_hz: 851e6,
            control_hz: 851_537_500.0,
            calls_dir: None,
        };
        let running = AtomicBool::new(true);
        let mut events = Vec::new();
        run(src, &p, None, &running, &mut |e| events.push(e)).expect("follow");
        let measured = events
            .iter()
            .find_map(|e| match e {
                FollowEvent::Measured {
                    control_mhz,
                    modulation,
                    ..
                } => Some((*control_mhz, modulation.clone())),
                _ => None,
            })
            .expect("control channel measured");
        assert!(
            (measured.0 - 851.5375).abs() < 0.002,
            "found at {}",
            measured.0
        );
        let starts = events
            .iter()
            .filter(|e| matches!(e, FollowEvent::CallStart { .. }))
            .count();
        let oob = events
            .iter()
            .filter(|e| {
                matches!(e, FollowEvent::Notice { text } if text.contains("outside the tuned band"))
            })
            .count();
        let last = events
            .iter()
            .rev()
            .find_map(|e| match e {
                FollowEvent::Status {
                    control_syncs,
                    dropped,
                    ..
                } => Some((*control_syncs, *dropped)),
                _ => None,
            })
            .expect("status");
        assert!(last.0 > 50, "control syncs {}", last.0);
        assert_eq!(last.1, 0, "dropped");
        assert!(starts >= 1, "no in-band call started");
        assert!(oob >= 1, "no out-of-band grant reported");
        eprintln!(
            "measured {measured:?}, {starts} starts, {oob} out-of-band, {} syncs",
            last.0
        );
    }

    /// Real hardware, no GUI: open an Airspy, follow the site for 25 s, play
    /// completed calls. `cargo test --release -- --ignored live_airspy`.
    #[test]
    #[ignore]
    fn live_airspy_follow_25s() {
        let src = crate::open_source("airspy", 855e6, 10_000_000.0, None).expect("airspy");
        let p = FollowParams {
            center_hz: 855e6,
            control_hz: 851_537_500.0,
            calls_dir: None,
        };
        let running = std::sync::Arc::new(AtomicBool::new(true));
        let r = std::sync::Arc::clone(&running);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(25));
            r.store(false, Ordering::SeqCst);
        });
        let mut player = crate::player::Player::open();
        let (mut starts, mut calls, mut last) = (0, 0, None);
        run(src, &p, None, &running, &mut |e| match e {
            FollowEvent::Measured {
                control_mhz,
                modulation,
                ..
            } => {
                eprintln!("measured {control_mhz} {modulation}")
            }
            FollowEvent::CallStart { name, freq_mhz, .. } => {
                starts += 1;
                eprintln!("start {name} {freq_mhz}")
            }
            FollowEvent::Call {
                name, secs, pcm, ..
            } => {
                calls += 1;
                eprintln!("call {name} {secs:.1}s");
                if let Some(pl) = player.as_mut() {
                    pl.play(&pcm);
                }
            }
            FollowEvent::Status {
                control_syncs,
                msps,
                want_msps,
                dropped,
                ..
            } => last = Some((control_syncs, msps, want_msps, dropped)),
            _ => {}
        })
        .expect("follow");
        eprintln!("{starts} starts, {calls} calls, status {last:?}");
        let (syncs, _, _, dropped) = last.expect("status");
        assert!(syncs > 100);
        assert_eq!(dropped, 0);
        // Let queued audio drain before the stream is dropped.
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
}
