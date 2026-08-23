//! Trunk following for the desktop app: the same engine the CLI's `--follow`
//! drives (`hs_core::follow::TrunkFollower`), with events instead of println.
//!
//! `run` is deliberately free of Tauri so it can be exercised headless over a
//! recording (see the tests) — the loop is verified on real off-air IQ; only
//! the button that starts it needs a human.

use hs_core::catalog::CsvCatalog;
use hs_core::follow::{in_band, GrantGate, TrunkFollower};
use hs_core::stream::{Buffered, Normalized};
use hs_source::{SdrSource, SourceError};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};

pub struct FollowParams {
    /// Most calls decoded at once (1–24).
    pub max_calls: usize,
    /// Traffic channels via the shared channelizer (true) or classic
    /// per-channel decimation (false) — an on-air A/B switch.
    pub channelizer: bool,
    /// "c4fm" | "cqpsk" to force the traffic modulation; anything else
    /// arbitrates per call.
    pub modulation: String,
    /// Vocoder unvoiced-synthesis quality, 1–64.
    pub uv_quality: i32,
    /// Where the radio is tuned (band centre), Hz.
    pub center_hz: f64,
    /// Nominal control-channel frequency, Hz.
    pub control_hz: f64,
    /// Library folder: one WAV + JSON sidecar per completed call goes under
    /// `<calls_dir>/YYYY/MM/DD/`, and a row into the library database.
    pub calls_dir: Option<std::path::PathBuf>,
    /// Hang time after a terminator and quiet timeout, in seconds; `None`
    /// keeps the engine defaults.
    pub hang_secs: Option<(f64, f64)>,
    /// Label written into sidecars (the playlist's system name, if any).
    pub system_name: String,
    /// Site label, for filenames and sidecars.
    pub site_name: String,
    /// Filename template for stored calls (see `names`).
    pub name_template: String,
    /// Stored audio format; WAV is written first and replaced when another
    /// codec is chosen (ffmpeg).
    pub format: crate::encode::Format,
    /// Live radio: discard the IQ that queued up during the measurement so
    /// following starts on current air. Off for recordings, where every
    /// sample is wanted.
    pub live: bool,
}

/// Everything the loop reads live from the UI, shared by reference.
pub struct Live<'a> {
    pub lockout: &'a std::sync::Mutex<std::collections::HashSet<u16>>,
    pub allowlist: &'a std::sync::Mutex<Option<std::collections::HashSet<u16>>>,
    /// Hold: follow only this talkgroup until released.
    pub hold: &'a std::sync::Mutex<Option<u16>>,
    pub priorities: &'a std::sync::Mutex<std::collections::HashMap<u16, u8>>,
    /// Locked-out and prioritised talkgroup ranges (inclusive).
    pub lockout_ranges: &'a std::sync::Mutex<Vec<(u16, u16)>>,
    pub priority_ranges: &'a std::sync::Mutex<Vec<(u16, u16, u8)>>,
    pub units: &'a std::sync::Mutex<std::collections::HashMap<u32, String>>,
    /// Wildcard rules naming radios the table does not list.
    pub unit_rules: &'a std::sync::Mutex<Vec<crate::units::Rule>>,
    /// Which talkgroups' audio is stored: `None` = all; `Some((all, except))`
    /// = everything/nothing by default, with the listed exceptions flipped.
    /// A call that is not recorded still gets a library row, without a file.
    pub record: &'a std::sync::Mutex<crate::Policy>,
    /// The call library; `None` runs without persistence (tests).
    pub db: Option<&'a std::sync::Mutex<rusqlite::Connection>>,
    /// Waterfall (fft size, averaging); `None` = 256 × 1.
    pub spectrum: Option<&'a std::sync::Mutex<(usize, usize)>>,
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
        /// Where the radio actually is (may differ from what was typed).
        center_mhz: f64,
        /// The tuner error as parts per million of the control frequency —
        /// the figure a PPM setting wants.
        ppm: f64,
        /// Every band being decoded: (centre MHz, sample rate), primary first.
        bands: Vec<(f64, f64)>,
    },
    CallStart {
        tg: u16,
        name: String,
        freq_mhz: f64,
        priority: u8,
    },
    /// A call finished; its audio is in `pcm` (8 kHz mono), not serialized.
    Call {
        tg: u16,
        name: String,
        source: u32,
        unit_name: Option<String>,
        freq_mhz: f64,
        modulation: String,
        secs: f64,
        /// Call start time (unix seconds) — the time the keyup began, not the
        /// completion time. Carried so uploaders stamp the true start.
        start: i64,
        /// Numeric site ID from the control channel, if announced.
        site: Option<u8>,
        emergency: bool,
        patched_with: Vec<u16>,
        priority: u8,
        /// Frame syncs each decoder achieved — the evidence behind a call
        /// that produced no audio.
        syncs_c4fm: u32,
        syncs_cqpsk: u32,
        /// Over-the-air alias the radio's system broadcast, if any.
        talker_alias: Option<String>,
        wav: Option<String>,
        /// Library row id, once stored.
        id: Option<i64>,
        #[serde(skip)]
        pcm: Vec<i16>,
    },
    /// What the control channel has announced about the site; sent when it
    /// changes.
    Site {
        nac: Option<u16>,
        wacn: Option<u32>,
        sys_id: Option<u16>,
        control_mhz: f64,
        alternates_mhz: Vec<f64>,
        idens: Vec<(u8, f64, f64)>,
        patches: Vec<(u16, Vec<u16>)>,
        rfss: Option<u8>,
        site: Option<u8>,
        /// Neighbouring sites: (system id, rfss, site, control MHz or null).
        neighbours: Vec<(u16, u8, u8, Option<f64>)>,
    },
    /// Every grant the control channel issued (rate-limited per talkgroup
    /// and channel), followed or not — the raw material for discovery.
    Grant {
        tg: u16,
        name: String,
        named: bool,
        freq_mhz: f64,
        unit: u32,
        encrypted: bool,
    },
    /// A radio joined a talkgroup, registered, or left: the site's roster.
    Mobility {
        /// affiliated | refused | registered | located | deregistered
        what: String,
        unit: u32,
        unit_name: Option<String>,
        tg: Option<u16>,
        name: Option<String>,
    },
    /// A radio reported its position over packet data (LRRP).
    Location {
        unit: u32,
        unit_name: Option<String>,
        lat: f64,
        lon: f64,
    },
    /// Recent control-channel symbols for the constellation view.
    Constellation {
        modulation: String,
        points: Vec<(f32, f32)>,
    },
    /// The system broadcast a radio's alias during a call.
    TalkerAlias {
        tg: u16,
        name: String,
        alias: String,
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
        locked: u32,
        busy: u32,
        msps: f64,
        want_msps: f64,
        dropped: u64,
        elapsed_secs: f64,
        /// Signal power (dBFS) off the control channel.
        signal_dbfs: Option<f32>,
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

/// An extra radio covering another part of the site's span: its centre and
/// an opened source. Calls granted inside it are decoded from it.
pub struct ExtraRadio {
    pub center_hz: f64,
    pub label: String,
    pub src: Box<dyn SdrSource + Send>,
}

/// Follow a trunk from `src` until `running` clears or the source ends.
/// `emit` receives every event in order, on this thread. (The app itself
/// goes through [`run_with_extras`]; this stays for the headless tests.)
#[cfg_attr(not(test), allow(dead_code))]
pub fn run<S: SdrSource + Send + 'static>(
    src: S,
    p: &FollowParams,
    catalog: &std::sync::Mutex<Option<CsvCatalog>>,
    live: &Live<'_>,
    running: &AtomicBool,
    emit: &mut dyn FnMut(FollowEvent),
) -> Result<(), String> {
    run_with_extras(src, Vec::new(), p, catalog, live, running, emit)
}

/// As [`run`], with extra radios parked on other parts of the site's span —
/// the way Unitrunker uses one tuner for control and the rest for voice.
/// The primary radio is read blocking; the extras are drained between its
/// blocks, so each band's calls advance at its own radio's pace.
pub fn run_with_extras<S: SdrSource + Send + 'static>(
    src: S,
    extras: Vec<ExtraRadio>,
    p: &FollowParams,
    catalog: &std::sync::Mutex<Option<CsvCatalog>>,
    live: &Live<'_>,
    running: &AtomicBool,
    emit: &mut dyn FnMut(FollowEvent),
) -> Result<(), String> {
    let src = Normalized::new(src);
    let rate = src.sample_rate();
    in_band(rate, p.center_hz, p.control_hz)?;
    // Extra radios: normalised like the primary, each with its own reader
    // thread so none of them starves while the control channel is measured.
    let mut extra: Vec<(f64, String, f64, Buffered)> = extras
        .into_iter()
        .map(|e| {
            let n = Normalized::new(e.src);
            let r = n.sample_rate();
            (e.center_hz, e.label, r, Buffered::new(n, 65536))
        })
        .collect();

    // Hand the radio to a reader thread *first*, so it keeps draining while
    // the multi-second measurement runs — read synchronously, an RTL-SDR's
    // buffer overflows during the sweep and the stream afterwards is holes.
    let mut src = if p.live {
        Buffered::new(src, 65536)
    } else {
        Buffered::lossless(src, 65536)
    };

    // Prime on ~3 s of air and measure where the control channel really is
    // (and which modulation it uses).
    let block = (rate as usize / 10) * 2;
    let mut buf = vec![0.0f32; 65536 * 2];
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
    emit(FollowEvent::Notice {
        text: "measuring the control channel (frequency and modulation)…".into(),
    });
    let cancel = || !running.load(Ordering::SeqCst);
    let Some((measured_hz, modulation)) = hs_core::follow::measure_carrier_cancellable(
        &prime,
        rate,
        p.center_hz,
        p.control_hz,
        &cancel,
    ) else {
        if cancel() {
            return Ok(());
        }
        return Err(format!(
            "could not find control {:.4} MHz: nothing decoded within ±12.5 kHz of it on either \
             modulation. A site lists several control-capable frequencies but only one carries \
             the control channel at a time — scan the band to see which.",
            p.control_hz / 1e6
        ));
    };
    let mut f = TrunkFollower::new(rate, p.center_hz, p.control_hz, measured_hz, modulation);
    for (center, label, r, _) in &extra {
        f.add_band(*center, *r);
        emit(FollowEvent::Notice {
            text: format!(
                "{label} covers {:.4}–{:.4} MHz",
                (center - r * 0.5 + 12_500.0) / 1e6,
                (center + r * 0.5 - 12_500.0) / 1e6
            ),
        });
    }
    emit(FollowEvent::Measured {
        control_mhz: measured_hz / 1e6,
        modulation: mod_name(Some(modulation)),
        correction_hz: f.correction_hz(),
        rate,
        center_mhz: p.center_hz / 1e6,
        bands: f.bands().iter().map(|(c, r)| (c / 1e6, *r)).collect(),
        // The control channel appears `correction` away from nominal because
        // the oscillator runs the other way: a tuner `p` ppm high puts a
        // signal at F at baseband F − C·p, so the error is −correction/F.
        ppm: -f.correction_ppm(),
    });

    if let Some((h, q)) = p.hang_secs {
        f.set_hang(h, q);
    }
    f.set_max_calls(p.max_calls.clamp(1, 24));
    f.set_channelizer(p.channelizer);
    f.set_uv_quality(p.uv_quality);
    f.set_forced_modulation(match p.modulation.to_ascii_lowercase().as_str() {
        "c4fm" => Some(hs_core::decoder::Modulation::C4fm),
        "cqpsk" | "lsm" => Some(hs_core::decoder::Modulation::Cqpsk),
        _ => None,
    });
    if !matches!(p.modulation.to_ascii_lowercase().as_str(), "" | "auto") {
        emit(FollowEvent::Notice {
            text: format!(
                "traffic modulation forced to {}",
                p.modulation.to_uppercase()
            ),
        });
    }
    let mut last_site = f.site_info();
    let mut rep = Reporter {
        catalog,
        record: live.record,
        units: live.units,
        unit_rules: live.unit_rules,
        db: live.db,
        calls_dir: p.calls_dir.as_deref(),
        system_name: p.system_name.clone(),
        site_name: p.site_name.clone(),
        site_id: last_site.site,
        name_template: p.name_template.clone(),
        format: p.format.clone(),
        discovery: GrantGate::new(2.0),
        priority_ranges: Vec::new(),
        syncs: 0,
        calls: 0,
        oob: 0,
        enc: 0,
        locked: 0,
        busy: 0,
        gate: GrantGate::new(5.0),
        started: std::collections::HashMap::new(),
        priorities: std::collections::HashMap::new(),
        unnamed: std::collections::HashSet::new(),
    };
    let apply_live = |f: &mut TrunkFollower, rep: &mut Reporter| {
        let want = live.lockout.lock().unwrap();
        if *want != *f.lockout() {
            f.set_lockout(want.iter().copied());
        }
        let hold = *live.hold.lock().unwrap();
        let want: Option<std::collections::HashSet<u16>> = match hold {
            Some(tg) => Some([tg].into_iter().collect()),
            None => live.allowlist.lock().unwrap().clone(),
        };
        if want.as_ref() != f.allowlist() {
            f.set_allowlist(want);
        }
        let pri = live.priorities.lock().unwrap();
        if *pri != rep.priorities {
            rep.priorities = pri.clone();
            f.set_priorities(pri.iter().map(|(t, p)| (*t, *p)));
        }
        let lr = live.lockout_ranges.lock().unwrap();
        if lr.as_slice() != f.lockout_ranges() {
            f.set_lockout_ranges(lr.iter().copied());
        }
        let pr = live.priority_ranges.lock().unwrap();
        if pr.as_slice() != f.priority_ranges() {
            rep.priority_ranges = pr.clone();
            f.set_priority_ranges(pr.iter().copied());
        }
    };
    apply_live(&mut f, &mut rep);
    emit(site_event(&last_site));
    // The primed IQ carries whatever the site granted while we measured;
    // decode it too, so a call that began during startup is not lost. The
    // follower runs far faster than real time, so this catches up quickly.
    let out = f.process(&prime);
    rep.report(out, prime.len() as f64 / 2.0 / rate, emit);
    drop(prime);

    // The queue filled with stale blocks while we measured; a live run starts
    // on current air instead.
    if p.live {
        src.discard_queued();
        for (_, _, _, e) in extra.iter_mut() {
            e.discard_queued();
        }
    }
    let mut extra_buf = vec![0.0f32; 65536 * 2];
    let drop_base = src.dropped();
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
        // The UI may change lockout / hold / playlist / priorities any time.
        apply_live(&mut f, &mut rep);
        let out = f.process(chunk);
        rep.report(out, (n / 2) as f64 / rate, emit);
        // Drain whatever the extra radios have queued since the last block.
        for (bi, (_, label, r, e)) in extra.iter_mut().enumerate() {
            loop {
                match e.try_read(&mut extra_buf) {
                    Ok(0) => break,
                    Ok(m) => {
                        let out = f.process_band(bi, &extra_buf[..m]);
                        rep.report(out, (m / 2) as f64 / *r, emit);
                    }
                    Err(SourceError::Eof) => break,
                    Err(err) => {
                        emit(FollowEvent::Notice {
                            text: format!("{label}: capture error: {err:?}"),
                        });
                        break;
                    }
                }
            }
        }
        if blocks.is_multiple_of(40) {
            let site = f.site_info();
            if site != last_site {
                last_site = site;
                rep.site_id = last_site.site;
                emit(site_event(&last_site));
            }
        }
        if blocks.is_multiple_of(4) {
            let (n, avg) = live
                .spectrum
                .map(|m| *m.lock().unwrap())
                .unwrap_or((256, 1));
            emit(FollowEvent::Spectrum {
                bins_db: super::power_spectrum_avg(chunk, n, avg),
            });
        }
        if blocks.is_multiple_of(12) {
            emit(FollowEvent::Constellation {
                modulation: mod_name(Some(modulation)),
                points: f.control_symbols(),
            });
        }
        if last_status.elapsed().as_secs_f64() >= 1.0 {
            last_status = std::time::Instant::now();
            let secs = start.elapsed().as_secs_f64().max(1e-3);
            let extra_drops: u64 = extra.iter().map(|(_, _, _, e)| e.dropped()).sum();
            emit(rep.status(
                total_pairs,
                secs,
                rate,
                src.dropped().saturating_sub(drop_base) + extra_drops,
                f.control_power_dbfs(),
            ));
        }
    }
    // Flush calls still in progress when the run ends.
    let out = hs_core::follow::FollowOutput {
        completed: f.finish(),
        ..Default::default()
    };
    rep.report(out, 0.0, emit);
    let secs = start.elapsed().as_secs_f64().max(1e-3);
    emit(rep.status(
        total_pairs,
        secs,
        rate,
        src.dropped().saturating_sub(drop_base),
        None,
    ));
    Ok(())
}

fn site_event(s: &hs_core::follow::SiteInfo) -> FollowEvent {
    FollowEvent::Site {
        nac: s.nac,
        wacn: s.wacn,
        sys_id: s.sys_id,
        control_mhz: s.control_hz as f64 / 1e6,
        alternates_mhz: s.alternates_hz.iter().map(|h| *h as f64 / 1e6).collect(),
        idens: s
            .idens
            .iter()
            .map(|(id, b, sp)| (*id, *b as f64 / 1e6, *sp as f64 / 1e3))
            .collect(),
        patches: s.patches.clone(),
        rfss: s.rfss,
        site: s.site,
        neighbours: s
            .neighbours
            .iter()
            .map(|(sys, r, st, hz)| (*sys, *r, *st, hz.map(|h| h as f64 / 1e6)))
            .collect(),
    }
}

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Turns follower output into events and keeps the running counts.
struct Reporter<'a> {
    catalog: &'a std::sync::Mutex<Option<CsvCatalog>>,
    record: &'a std::sync::Mutex<crate::Policy>,
    /// Talkgroups already reported as unnamed, so the hint fires once each.
    unnamed: std::collections::HashSet<u16>,
    units: &'a std::sync::Mutex<std::collections::HashMap<u32, String>>,
    unit_rules: &'a std::sync::Mutex<Vec<crate::units::Rule>>,
    db: Option<&'a std::sync::Mutex<rusqlite::Connection>>,
    calls_dir: Option<&'a std::path::Path>,
    system_name: String,
    site_name: String,
    /// Numeric site ID from the control channel, if announced.
    site_id: Option<u8>,
    name_template: String,
    format: crate::encode::Format,
    /// Rate limit for discovery grant events, per (talkgroup, channel).
    discovery: GrantGate,
    priority_ranges: Vec<(u16, u16, u8)>,
    syncs: u32,
    calls: usize,
    oob: u32,
    enc: u32,
    locked: u32,
    busy: u32,
    gate: GrantGate,
    /// When each active call started (epoch s), by (tg, freq).
    started: std::collections::HashMap<(u16, u64), u64>,
    priorities: std::collections::HashMap<u16, u8>,
}

impl Reporter<'_> {
    fn name_of(&self, tg: u16) -> String {
        match self
            .catalog
            .lock()
            .ok()
            .and_then(|c| c.as_ref().map(|k| k.label(tg)))
        {
            Some(l) => l,
            None => format!("TG {tg}"),
        }
    }

    fn is_named(&self, tg: u16) -> bool {
        self.catalog
            .lock()
            .ok()
            .and_then(|c| c.as_ref().map(|k| k.get(tg).is_some()))
            .unwrap_or(false)
    }

    fn priority_of(&self, tg: u16) -> u8 {
        self.priorities.get(&tg).copied().unwrap_or_else(|| {
            self.priority_ranges
                .iter()
                .find(|(lo, hi, _)| (*lo..=*hi).contains(&tg))
                .map(|(_, _, p)| *p)
                .unwrap_or(50)
        })
    }

    fn unit_name(&self, id: u32) -> Option<String> {
        let units = self.units.lock().ok()?;
        let rules = self.unit_rules.lock().ok()?;
        crate::units::name_for(&units, &rules, id)
    }

    fn status(&self, total_pairs: u64, secs: f64, rate: f64, dropped: u64, signal_dbfs: Option<f32>) -> FollowEvent {
        FollowEvent::Status {
            control_syncs: self.syncs,
            calls: self.calls,
            out_of_band: self.oob,
            encrypted: self.enc,
            locked: self.locked,
            busy: self.busy,
            msps: (total_pairs as f64) / secs / 1e6,
            want_msps: rate / 1e6,
            dropped,
            elapsed_secs: secs,
            signal_dbfs,
        }
    }

    fn report(
        &mut self,
        out: hs_core::follow::FollowOutput,
        secs: f64,
        emit: &mut dyn FnMut(FollowEvent),
    ) {
        self.syncs += out.control_syncs;
        self.gate.tick(secs);
        self.discovery.tick(secs);
        // Discovery: every grant, followed or not, once per couple of
        // seconds per talkgroup-and-channel so repeats do not flood.
        for g in &out.grants {
            let key = ((g.talkgroup as u64) << 48) | (g.freq_hz & 0xFFFF_FFFF_FFFF);
            if self.discovery.fresh(key) {
                emit(FollowEvent::Grant {
                    tg: g.talkgroup,
                    name: self.name_of(g.talkgroup),
                    named: self.is_named(g.talkgroup),
                    freq_mhz: g.freq_hz as f64 / 1e6,
                    unit: g.source_unit,
                    encrypted: g.encrypted,
                });
            }
        }
        for m in &out.mobility {
            use hs_core::trunk::MobilityEvent;
            let (kind, unit, tg) = match *m {
                MobilityEvent::Affiliated {
                    unit,
                    group,
                    accepted,
                } => (
                    if accepted { "affiliated" } else { "refused" },
                    unit,
                    Some(group),
                ),
                MobilityEvent::Registered { unit, status } => (
                    if status == 0 { "registered" } else { "refused" },
                    unit,
                    None,
                ),
                MobilityEvent::Located { unit, group } => ("located", unit, Some(group)),
                MobilityEvent::Deregistered { unit } => ("deregistered", unit, None),
            };
            emit(FollowEvent::Mobility {
                what: kind.into(),
                unit,
                unit_name: self.unit_name(unit),
                tg,
                name: tg.map(|t| self.name_of(t)),
            });
        }
        for (unit, lat, lon) in &out.locations {
            emit(FollowEvent::Location {
                unit: *unit,
                unit_name: self.unit_name(*unit),
                lat: *lat,
                lon: *lon,
            });
        }
        for (tg, alias) in &out.talker_aliases {
            emit(FollowEvent::TalkerAlias {
                tg: *tg,
                name: self.name_of(*tg),
                alias: alias.clone(),
            });
        }
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
            self.started.insert((*tg, *hz), epoch_secs());
            if !self.is_named(*tg) && self.unnamed.insert(*tg) {
                emit(FollowEvent::Notice {
                    text: format!(
                        "TG {tg} is not in any loaded catalog — load this site's system in Playlists (its NAC is in Site details)"
                    ),
                });
            }
            emit(FollowEvent::CallStart {
                tg: *tg,
                name: self.name_of(*tg),
                freq_mhz: *hz as f64 / 1e6,
                priority: self.priority_of(*tg),
            });
        }
        for (tg, hz) in &out.grants_busy {
            if self.gate.fresh(*hz) {
                self.busy += 1;
                emit(FollowEvent::Notice {
                    text: format!(
                        "{} on {:.4} MHz not followed — every decoder busy with equal or higher priority",
                        self.name_of(*tg),
                        *hz as f64 / 1e6
                    ),
                });
            }
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
        for (_, hz) in &out.grants_locked {
            if self.gate.fresh(*hz) {
                self.locked += 1;
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
            let start = self
                .started
                .remove(&(c.talkgroup, c.freq_hz))
                .unwrap_or_else(epoch_secs);
            let secs = c.pcm.len() as f64 / 8000.0;
            let name = self.name_of(c.talkgroup);
            let unit_name = self.unit_name(c.source_unit);
            // A keyup with no voice leaves nothing worth a file; nor does a
            // talkgroup the listener chose not to record.
            let recordable = self
                .record
                .lock()
                .map(|r| crate::policy_allows(&r, c.talkgroup))
                .unwrap_or(true);
            let wav = self.calls_dir.filter(|_| !c.pcm.is_empty() && recordable).and_then(|root| {
                let stamp = chrono_stamp();
                let day = root.join(&stamp[0..4]).join(&stamp[4..6]).join(&stamp[6..8]);
                let rel = crate::names::render(
                    &self.name_template,
                    &crate::names::NameContext {
                        stamp: &stamp,
                        tg: c.talkgroup,
                        tg_name: if self.is_named(c.talkgroup) { &name } else { "" },
                        unit: c.source_unit,
                        unit_name: unit_name.as_deref().unwrap_or(""),
                        freq_hz: c.freq_hz,
                        system: &self.system_name,
                        site: &self.site_name,
                        modulation: &mod_name(c.modulation),
                        secs,
                        emergency: c.emergency,
                    },
                );
                let full = day.join(&rel);
                let dir = full.parent().map(|d| d.to_path_buf()).unwrap_or(day);
                let stem = full
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| stamp.clone());
                std::fs::create_dir_all(&dir).ok()?;
                let path = dir.join(format!("{stem}.wav"));
                match hs_core::wav::write_wav(path.to_str()?, 8000, &c.pcm) {
                    Ok(()) => {
                        // trunk-recorder-shaped sidecar, so rdio-scanner's
                        // dirwatch and friends can ingest the directory.
                        let side = serde_json::json!({
                            "freq": c.freq_hz,
                            "start_time": start,
                            "stop_time": start + secs.ceil() as u64,
                            "emergency": u8::from(c.emergency),
                            "encrypted": 0,
                            "call_length": secs.round() as u64,
                            "talkgroup": c.talkgroup,
                            "talkgroup_tag": name,
                            "talkgroup_description": name,
                            "audio_type": "digital",
                            "short_name": self.system_name,
                            "patched_talkgroups": c.patched_with,
                            "freqList": [{"freq": c.freq_hz, "time": start, "pos": 0.0, "len": secs}],
                            "srcList": [{"src": c.source_unit, "time": start, "pos": 0.0, "emergency": u8::from(c.emergency), "tag": unit_name.clone().unwrap_or_default()}],
                            "talker_alias": c.talker_alias,
                            "site": self.site_name,
                        });
                        let _ = std::fs::write(dir.join(format!("{stem}.json")), side.to_string());
                        // Derived format, if asked for; the WAV goes once it exists.
                        let stored = match crate::encode::transcode(&path, &self.format) {
                            Ok(p) if p != path => {
                                let _ = std::fs::remove_file(&path);
                                p
                            }
                            Ok(p) => p,
                            Err(e) => {
                                emit(FollowEvent::Notice { text: format!("kept WAV: {e}") });
                                path.clone()
                            }
                        };
                        Some(stored.to_string_lossy().into_owned())
                    }
                    Err(e) => {
                        emit(FollowEvent::Notice {
                            text: format!("could not write {}: {e}", path.display()),
                        });
                        None
                    }
                }
            });
            let id = self.db.and_then(|db| {
                let row = crate::library::CallRow {
                    id: 0,
                    start: start as i64,
                    secs,
                    tg: c.talkgroup,
                    tg_name: name.clone(),
                    unit: c.source_unit,
                    unit_name: unit_name.clone(),
                    freq_hz: c.freq_hz,
                    modulation: mod_name(c.modulation),
                    emergency: c.emergency,
                    patched_with: c.patched_with.clone(),
                    system: self.system_name.clone(),
                    site: self.site_name.clone(),
                    audio: wav.clone(),
                    ..Default::default()
                };
                match crate::library::insert(&*db.lock().ok()?, &row) {
                    Ok(id) => Some(id),
                    Err(e) => {
                        emit(FollowEvent::Notice {
                            text: format!("library: {e}"),
                        });
                        None
                    }
                }
            });
            emit(FollowEvent::Call {
                tg: c.talkgroup,
                name,
                source: c.source_unit,
                unit_name,
                freq_mhz: c.freq_hz as f64 / 1e6,
                modulation: mod_name(c.modulation),
                secs,
                start: start as i64,
                site: self.site_id,
                emergency: c.emergency,
                patched_with: c.patched_with.clone(),
                priority: self.priority_of(c.talkgroup),
                syncs_c4fm: c.syncs_c4fm,
                syncs_cqpsk: c.syncs_cqpsk,
                talker_alias: c.talker_alias.clone(),
                wav,
                id,
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

    static NO_LOCKOUT_RANGES: std::sync::Mutex<Vec<(u16, u16)>> = std::sync::Mutex::new(Vec::new());
    static NO_PRIORITY_RANGES: std::sync::Mutex<Vec<(u16, u16, u8)>> =
        std::sync::Mutex::new(Vec::new());
    static NO_RULES: std::sync::Mutex<Vec<crate::units::Rule>> = std::sync::Mutex::new(Vec::new());
    static NO_RECORD: std::sync::Mutex<crate::Policy> = std::sync::Mutex::new(None);

    #[allow(clippy::type_complexity)]
    fn live_defaults() -> (
        std::sync::Mutex<std::collections::HashSet<u16>>,
        std::sync::Mutex<Option<std::collections::HashSet<u16>>>,
        std::sync::Mutex<Option<u16>>,
        std::sync::Mutex<std::collections::HashMap<u16, u8>>,
        std::sync::Mutex<std::collections::HashMap<u32, String>>,
    ) {
        Default::default()
    }

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
            hang_secs: None,
            system_name: String::new(),
            format: Default::default(),
            max_calls: 6,
            channelizer: true,
            modulation: String::new(),
            uv_quality: 16,
            site_name: String::new(),
            name_template: String::new(),
            live: false,
        };
        let running = AtomicBool::new(true);
        let mut events = Vec::new();
        let (lockout, allow, hold, pri, units) = live_defaults();
        let live = Live {
            lockout: &lockout,
            allowlist: &allow,
            hold: &hold,
            priorities: &pri,
            units: &units,
            db: None,
            spectrum: None,
            lockout_ranges: &NO_LOCKOUT_RANGES,
            priority_ranges: &NO_PRIORITY_RANGES,
            unit_rules: &NO_RULES,
            record: &NO_RECORD,
        };
        run(
            src,
            &p,
            &std::sync::Mutex::new(None),
            &live,
            &running,
            &mut |e| events.push(e),
        )
        .expect("follow");
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

    /// Lock out the one talkgroup that has an in-band call in the recording:
    /// it must never start, and the skip must be counted.
    #[test]
    fn a_locked_out_talkgroup_is_never_followed() {
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
            hang_secs: None,
            system_name: String::new(),
            format: Default::default(),
            max_calls: 6,
            channelizer: true,
            modulation: String::new(),
            uv_quality: 16,
            site_name: String::new(),
            name_template: String::new(),
            live: false,
        };
        let running = AtomicBool::new(true);
        let mut events = Vec::new();
        let (_, allow, hold, pri, units) = live_defaults();
        let lockout = std::sync::Mutex::new([20308u16].into_iter().collect());
        let live = Live {
            lockout: &lockout,
            allowlist: &allow,
            hold: &hold,
            priorities: &pri,
            units: &units,
            db: None,
            spectrum: None,
            lockout_ranges: &NO_LOCKOUT_RANGES,
            priority_ranges: &NO_PRIORITY_RANGES,
            unit_rules: &NO_RULES,
            record: &NO_RECORD,
        };
        run(
            src,
            &p,
            &std::sync::Mutex::new(None),
            &live,
            &running,
            &mut |e| events.push(e),
        )
        .expect("follow");
        let starts = events
            .iter()
            .filter(|e| matches!(e, FollowEvent::CallStart { tg, .. } if *tg == 20308))
            .count();
        let locked = events
            .iter()
            .rev()
            .find_map(|e| match e {
                FollowEvent::Status { locked, .. } => Some(*locked),
                _ => None,
            })
            .unwrap_or(0);
        assert_eq!(starts, 0, "locked talkgroup was followed");
        assert!(locked >= 1, "lockout skip not counted");
    }

    /// A playlist that omits the in-band talkgroup never follows it; one that
    /// includes it does.
    #[test]
    fn a_playlist_restricts_what_is_followed() {
        let path =
            std::env::var("HOME").unwrap() + "/hoosier-field/live_airspy_851M_2500k_nac260.cs16";
        let starts_with = |allow: Option<Vec<u16>>| -> Option<usize> {
            let src = cs16_source(&path, 2_500_000.0)?;
            let p = FollowParams {
                center_hz: 851e6,
                control_hz: 851_537_500.0,
                calls_dir: None,
                hang_secs: None,
                system_name: String::new(),
                format: Default::default(),
                max_calls: 6,
                channelizer: true,
                modulation: String::new(),
                uv_quality: 16,
                site_name: String::new(),
                name_template: String::new(),
                live: false,
            };
            let running = AtomicBool::new(true);
            let mut n = 0;
            let (lockout, _, hold, pri, units) = live_defaults();
            let allow = std::sync::Mutex::new(allow.map(|v| v.into_iter().collect()));
            let live = Live {
                lockout: &lockout,
                allowlist: &allow,
                hold: &hold,
                priorities: &pri,
                units: &units,
                db: None,
                spectrum: None,
                lockout_ranges: &NO_LOCKOUT_RANGES,
                priority_ranges: &NO_PRIORITY_RANGES,
                unit_rules: &NO_RULES,
                record: &NO_RECORD,
            };
            run(
                src,
                &p,
                &std::sync::Mutex::new(None),
                &live,
                &running,
                &mut |e| {
                    if matches!(e, FollowEvent::CallStart { tg, .. } if tg == 20308) {
                        n += 1;
                    }
                },
            )
            .expect("follow");
            Some(n)
        };
        let Some(without) = starts_with(Some(vec![1, 2, 3])) else {
            eprintln!("no capture; skipping");
            return;
        };
        assert_eq!(without, 0, "followed a talkgroup outside the playlist");
        assert!(
            starts_with(Some(vec![20308])).unwrap() >= 1,
            "playlist member not followed"
        );
    }

    /// Hold narrows following to one talkgroup, overriding the playlist.
    #[test]
    fn hold_follows_only_the_held_talkgroup() {
        let path =
            std::env::var("HOME").unwrap() + "/hoosier-field/live_airspy_851M_2500k_nac260.cs16";
        let starts_with = |hold_tg: u16| -> Option<usize> {
            let src = cs16_source(&path, 2_500_000.0)?;
            let p = FollowParams {
                center_hz: 851e6,
                control_hz: 851_537_500.0,
                calls_dir: None,
                hang_secs: None,
                system_name: String::new(),
                format: Default::default(),
                max_calls: 6,
                channelizer: true,
                modulation: String::new(),
                uv_quality: 16,
                site_name: String::new(),
                name_template: String::new(),
                live: false,
            };
            let running = AtomicBool::new(true);
            let (lockout, allow, _, pri, units) = live_defaults();
            let hold = std::sync::Mutex::new(Some(hold_tg));
            let live = Live {
                lockout: &lockout,
                allowlist: &allow,
                hold: &hold,
                priorities: &pri,
                units: &units,
                db: None,
                spectrum: None,
                lockout_ranges: &NO_LOCKOUT_RANGES,
                priority_ranges: &NO_PRIORITY_RANGES,
                unit_rules: &NO_RULES,
                record: &NO_RECORD,
            };
            let mut n = 0;
            run(
                src,
                &p,
                &std::sync::Mutex::new(None),
                &live,
                &running,
                &mut |e| {
                    if matches!(e, FollowEvent::CallStart { tg, .. } if tg == 20308) {
                        n += 1;
                    }
                },
            )
            .expect("follow");
            Some(n)
        };
        let Some(other) = starts_with(1) else {
            eprintln!("no capture; skipping");
            return;
        };
        assert_eq!(
            other, 0,
            "hold on another talkgroup still followed TG 20308"
        );
        assert!(
            starts_with(20308).unwrap() >= 1,
            "held talkgroup not followed"
        );
    }

    /// Real hardware, no GUI: open an Airspy, follow the site for 25 s, play
    /// completed calls. `cargo test --release -- --ignored live_airspy`.
    #[test]
    #[ignore]
    fn live_airspy_follow_25s() {
        let src = crate::open_device("airspy", None, 855e6, 10_000_000.0, None).expect("airspy");
        let p = FollowParams {
            center_hz: 855e6,
            control_hz: 851_537_500.0,
            calls_dir: None,
            hang_secs: None,
            system_name: String::new(),
            format: Default::default(),
            max_calls: 6,
            channelizer: true,
            modulation: String::new(),
            uv_quality: 16,
            site_name: String::new(),
            name_template: String::new(),
            live: false,
        };
        let running = std::sync::Arc::new(AtomicBool::new(true));
        let r = std::sync::Arc::clone(&running);
        let secs: u64 = std::env::var("HS_LIVE_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(25);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(secs));
            r.store(false, Ordering::SeqCst);
        });
        let player = crate::player::spawn();
        let (mut starts, mut calls, mut last) = (0, 0, None);
        let (lockout, allow, hold, pri, units) = live_defaults();
        let live = Live {
            lockout: &lockout,
            allowlist: &allow,
            hold: &hold,
            priorities: &pri,
            units: &units,
            db: None,
            spectrum: None,
            lockout_ranges: &NO_LOCKOUT_RANGES,
            priority_ranges: &NO_PRIORITY_RANGES,
            unit_rules: &NO_RULES,
            record: &NO_RECORD,
        };
        run(
            src,
            &p,
            &std::sync::Mutex::new(None),
            &live,
            &running,
            &mut |e| match e {
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
                    if let Some(pl) = player.as_ref() {
                        pl.play(pcm, 50);
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
            },
        )
        .expect("follow");
        eprintln!("{starts} starts, {calls} calls, status {last:?}");
        let (syncs, _, _, dropped) = last.expect("status");
        assert!(syncs > 100);
        assert_eq!(dropped, 0);
        // Let queued audio drain before the stream is dropped.
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
}
