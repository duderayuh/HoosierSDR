//! `--follow`: run the trunk follower, from a file or a live radio.
//!
//! This is the scanner. Everything else in the CLI decodes one channel that
//! the user names; here the control channel names the channels, and each call
//! it announces is decoded and written out as its own audio file.

use hs_core::decoder::Modulation;
use hs_core::follow::{Call, TrunkFollower};

/// Network identifiers that passed their BCH check — the evidence that a
/// candidate is genuinely decoding rather than merely correlating.
fn clean_nids(f: &TrunkFollower) -> u32 {
    f.control_diagnostics()
        .nids
        .iter()
        .filter(|n| n.bch_errors == 0)
        .count() as u32
}

/// Find where a channel really is, given where it should be.
///
/// An uncalibrated tuner can sit several kilohertz off, which is more than the
/// demodulators tolerate, so the follower needs the control channel's measured
/// frequency rather than its nominal one. Rather than ask the user for a ppm
/// figure they have no easy way to obtain, find it here.
///
/// The obvious way — take the strongest spectral peak near the nominal
/// frequency — does not survive contact with a real capture. Two things beat
/// it: the tuner's own DC spike sits at the centre of the band and is often the
/// tallest thing in it, and a busy traffic channel one 25 kHz slot away is
/// routinely louder than the control channel itself. Both were observed on the
/// first recording this was tried against, and both produced a confident wrong
/// answer.
///
/// So measure the thing we actually care about instead of a proxy for it: try
/// each candidate and keep whichever one lets the control channel decode.
///
/// "Decode" has to mean more than frame syncs, though. Scoring on sync count
/// picked C4FM at zero offset over the correct CQPSK at −1 kHz, and followed
/// the system to zero calls — because a C4FM demodulator on a CQPSK signal
/// still syncs. The two modulations share a symbol rate and a frame structure,
/// so the sync correlator is not what separates them. What does is the network
/// identifier behind each sync: it carries a BCH code, so a wrong modulation
/// produces syncs whose NIDs do not check out. Counting *clean* NIDs scores
/// the thing that has to be right for anything downstream to work.
/// Returns `None` when nothing decoded anywhere in the window, which the
/// caller must report as such: a failure that prints like a successful
/// detection at zero error is worse than no detection at all. This function
/// used to return the nominal frequency in that case, and three control
/// channels that were simply not on the air were duly reported as "found at
/// nominal, tuner error +0 Hz" — indistinguishable from a real lock, and
/// nearly recorded as a measurement.
pub fn measure_carrier(
    iq: &[f32],
    sample_rate: f64,
    center_hz: f64,
    nominal_hz: f64,
) -> Option<(f64, Modulation)> {
    // Half a channel either way. Sweeping that whole span at the precision the
    // demodulators want would mean a hundred trial decodes, and each one
    // channelizes the probe from scratch. Coarse-then-fine gets the same
    // answer for a fraction of the cost: a kilohertz grid is tight enough that
    // the right slot still decodes something, and the refinement only has to
    // search its neighbourhood.
    const SEARCH_HZ: f64 = 12_500.0;
    const COARSE_HZ: f64 = 1_000.0;
    const FINE_HZ: f64 = 250.0;

    // Half a second is hundreds of control-channel frames — ample to separate
    // a locked candidate from an unlocked one.
    let want = sample_rate as usize;
    let probe = &iq[..want.min(iq.len())];

    let try_at = |cand: f64, m: Modulation| -> u32 {
        if (cand - center_hz).abs() >= sample_rate / 2.0 {
            return 0;
        }
        let mut f = TrunkFollower::new(sample_rate, center_hz, nominal_hz, cand, m);
        f.process(probe);
        clean_nids(&f)
    };

    // Modulation is swept alongside frequency rather than asked for. It is not
    // knowable from the frequency — a scan of one band found control channels
    // of both kinds — and getting it wrong looks exactly like being tuned to
    // the wrong place, so guessing would produce a confident silence.
    let mods = [Modulation::Cqpsk, Modulation::C4fm];
    let mut best = (0u32, nominal_hz, Modulation::Cqpsk);
    let coarse = (SEARCH_HZ / COARSE_HZ) as i32;
    for k in -coarse..=coarse {
        let cand = nominal_hz + k as f64 * COARSE_HZ;
        for m in mods {
            let syncs = try_at(cand, m);
            if syncs > best.0 {
                best = (syncs, cand, m);
            }
        }
    }
    if best.0 == 0 {
        return None;
    }
    let (centre, m) = (best.1, best.2);
    let fine = (COARSE_HZ / FINE_HZ) as i32;
    for k in -fine..=fine {
        let cand = centre + k as f64 * FINE_HZ;
        let syncs = try_at(cand, m);
        if syncs > best.0 {
            best = (syncs, cand, m);
        }
    }
    Some((best.1, best.2))
}

/// De-noises repeated grant announcements. A control channel repeats a grant
/// every cycle while its call is up, so the same out-of-band call is seen many
/// times a second; this reports each frequency at most once per cooldown.
struct GrantGate {
    /// freq -> blocks remaining before it may be reported again.
    seen: std::collections::HashMap<u64, u32>,
    cooldown: u32,
}

impl GrantGate {
    fn new(cooldown: u32) -> Self {
        Self {
            seen: std::collections::HashMap::new(),
            cooldown,
        }
    }
    /// True if this frequency should be reported now (and arms its cooldown).
    fn fresh(&mut self, freq: u64) -> bool {
        match self.seen.get_mut(&freq) {
            Some(c) if *c > 0 => false,
            _ => {
                self.seen.insert(freq, self.cooldown);
                true
            }
        }
    }
    /// Call once per processed block to age the cooldowns.
    fn tick(&mut self) {
        for c in self.seen.values_mut() {
            *c = c.saturating_sub(1);
        }
    }
}

fn mod_name(m: Modulation) -> &'static str {
    match m {
        Modulation::C4fm => "C4FM",
        Modulation::Cqpsk => "CQPSK",
    }
}

/// Report a completed call and save its audio.
pub fn report_call(c: &Call, cat: Option<&hs_core::catalog::CsvCatalog>, n: usize) {
    let name = match cat {
        Some(k) => k.label(c.talkgroup),
        None => format!("TG {}", c.talkgroup),
    };
    let m = c.modulation.map(mod_name).unwrap_or("?");
    let secs = c.pcm.len() as f64 / 8000.0;
    let patch = if c.patched_with.is_empty() {
        String::new()
    } else {
        format!("  (patched with {:?})", c.patched_with)
    };
    println!(
        "  CALL  {name:<20} unit {:<9} {:.4} MHz  {m:5}  {secs:.1}s  \
         (c4fm {} / cqpsk {} syncs){patch}",
        c.source_unit,
        c.freq_hz as f64 / 1e6,
        c.syncs_c4fm,
        c.syncs_cqpsk
    );
    if c.pcm.is_empty() {
        return;
    }
    let path = format!("call_{:03}_tg{}.wav", n, c.talkgroup);
    match crate::wav::write_wav(&path, crate::VOICE_RATE, &c.pcm) {
        Ok(()) => println!("        audio → {path}"),
        Err(e) => eprintln!("        could not write {path}: {e}"),
    }
}

/// Decide the modulation at a frequency the user supplied.
///
/// `--control-measured` names where the channel is but not what it speaks, and
/// the two are independent, so the shorter sweep still has to run.
pub fn pick_modulation(
    iq: &[f32],
    sample_rate: f64,
    center_hz: f64,
    nominal_hz: f64,
    measured_hz: f64,
) -> Option<Modulation> {
    let probe = &iq[..(sample_rate as usize).min(iq.len())];
    let score = |m: Modulation| {
        let mut f = TrunkFollower::new(sample_rate, center_hz, nominal_hz, measured_hz, m);
        f.process(probe);
        clean_nids(&f)
    };
    let (c4, cq) = (score(Modulation::C4fm), score(Modulation::Cqpsk));
    if c4 == 0 && cq == 0 {
        None
    } else if c4 > cq {
        Some(Modulation::C4fm)
    } else {
        Some(Modulation::Cqpsk)
    }
}

/// Say plainly that the control channel was not found, and why it might not
/// have been.
fn report_not_found(control_hz: f64) {
    eprintln!(
        "Could not find control {:.4} MHz: nothing decoded within ±12.5 kHz of it,\n\
         on either modulation.",
        control_hz / 1e6
    );
    eprintln!(
        "\nA system lists several control-capable frequencies but only one carries the\n\
         control channel at a time; the rest are alternates and are silent. Run --scan\n\
         to see which one this recording actually contains."
    );
}

/// Check the control channel is inside the captured band before anything is
/// built. Below the DSP layer this is an assertion — correct there, since a
/// channelizer asked for a frequency it does not have would silently hand back
/// a different one — but a mistyped `--freq` is an ordinary user error and
/// deserves an ordinary message rather than a panic.
fn check_in_band(sample_rate: f64, center_hz: f64, control_hz: f64) {
    let nyquist = sample_rate / 2.0;
    if (control_hz - center_hz).abs() >= nyquist {
        eprintln!(
            "control {:.4} MHz is outside the capture: centered at {:.4} MHz, \
             {:.4} MHz wide (covers {:.4}–{:.4} MHz).",
            control_hz / 1e6,
            center_hz / 1e6,
            sample_rate / 1e6,
            (center_hz - nyquist) / 1e6,
            (center_hz + nyquist) / 1e6,
        );
        eprintln!("Set --freq to the frequency the capture was tuned to.");
        std::process::exit(2);
    }
}

/// Follow a system in a recorded capture.
pub fn run_file(
    iq: &[f32],
    sample_rate: f64,
    center_hz: f64,
    control_hz: f64,
    measured_hz: Option<f64>,
    cat: Option<&hs_core::catalog::CsvCatalog>,
) {
    check_in_band(sample_rate, center_hz, control_hz);
    let found = match measured_hz {
        Some(m) => pick_modulation(iq, sample_rate, center_hz, control_hz, m).map(|md| (m, md)),
        None => measure_carrier(iq, sample_rate, center_hz, control_hz),
    };
    let Some((measured, modulation)) = found else {
        report_not_found(control_hz);
        std::process::exit(1);
    };
    let mut f = TrunkFollower::new(sample_rate, center_hz, control_hz, measured, modulation);
    println!(
        "Following control {:.4} MHz {} (found at {:.4}, tuner error {:+.0} Hz)\n",
        control_hz / 1e6,
        mod_name(modulation),
        measured / 1e6,
        f.correction_hz()
    );

    // Feed in blocks, exactly as the live path does, so a recording and a
    // radio behave identically.
    let block = (sample_rate as usize / 10) * 2;
    let mut n = 0usize;
    let mut syncs = 0u32;
    let name_of = |tg: u16| match cat {
        Some(k) => k.label(tg),
        None => format!("TG {tg}"),
    };
    let mut oob = 0usize;
    let mut gate = GrantGate::new(50);
    for chunk in iq.chunks(block) {
        let out = f.process(chunk);
        syncs += out.control_syncs;
        gate.tick();
        for (tg, hz) in &out.started {
            println!("  start {} on {:.4} MHz", name_of(*tg), *hz as f64 / 1e6);
        }
        for (tg, hz) in &out.grants_out_of_band {
            if gate.fresh(*hz) {
                oob += 1;
                println!(
                    "  (call {} on {:.4} MHz — outside the capture, not followed)",
                    name_of(*tg),
                    *hz as f64 / 1e6
                );
            }
        }
        for c in &out.completed {
            n += 1;
            report_call(c, cat, n);
        }
    }
    if oob > 0 {
        println!("({oob} more calls were granted onto channels outside this capture)");
    }
    // A recording ends mid-transmission far more often than not, so close out
    // whatever was still in flight rather than discarding its audio.
    let mut truncated = 0usize;
    for c in &f.finish() {
        n += 1;
        truncated += 1;
        report_call(c, cat, n);
    }

    println!("\ncontrol channel: {syncs} frame syncs");
    println!("calls completed: {n}");
    if truncated > 0 {
        println!("({truncated} still in progress when the recording ended)");
    }
    if syncs == 0 {
        println!(
            "\nThe control channel never decoded. Either it is not at {:.4} MHz, or it is\n\
             outside this capture. --scan finds the control channels a recording contains.",
            control_hz / 1e6
        );
    } else if n == 0 {
        println!(
            "\nThe control channel decoded but no call followed it. The granted traffic\n\
             channels are probably outside this capture — widen the sample rate so the\n\
             band the system actually uses fits inside it."
        );
    }
}

/// Follow a system live, from a radio.
///
/// The only difference from a recording is where the samples come from — and
/// one extra step at the start: a short priming read, so the tuner's frequency
/// error can be measured off real air before the follower is built. A radio
/// that has just been tuned also needs a moment for its own AGC to settle, so
/// the priming samples are measured and then discarded rather than decoded.
// Only reachable from the live-capture path; a headless build still compiles
// it so the two paths cannot drift apart unnoticed.
#[cfg_attr(not(feature = "rtlsdr"), allow(dead_code))]
pub fn run_live<S: hs_source::SdrSource>(
    src: &mut S,
    sample_rate: f64,
    center_hz: f64,
    control_hz: f64,
    measured_hz: Option<f64>,
    cat: Option<&hs_core::catalog::CsvCatalog>,
    dump_iq: Option<&str>,
) {
    check_in_band(sample_rate, center_hz, control_hz);
    let block = (sample_rate as usize / 10) * 2;
    let mut buf = vec![0.0f32; block];

    // Prime on a second of live air before building anything. A freshly tuned
    // dongle needs a moment for its AGC to settle, and the control channel's
    // real frequency and modulation both have to be measured off that air —
    // the sweep is not instant, so say so rather than look hung.
    println!("Measuring the control channel (a few seconds)…");
    let target = block * 10;
    let mut prime: Vec<f32> = Vec::with_capacity(target);
    while prime.len() < target {
        match src.read(&mut buf) {
            Ok(0) => continue,
            Ok(n) => prime.extend_from_slice(&buf[..n]),
            Err(e) => {
                eprintln!("capture error while measuring: {e:?}");
                return;
            }
        }
    }

    // Same decision run_file makes: if the caller supplied the measured
    // frequency, only the modulation is unknown; otherwise sweep for both.
    let found = match measured_hz {
        Some(m) => pick_modulation(&prime, sample_rate, center_hz, control_hz, m).map(|md| (m, md)),
        None => measure_carrier(&prime, sample_rate, center_hz, control_hz),
    };
    let Some(measured) = found else {
        report_not_found(control_hz);
        std::process::exit(1);
    };

    let mut f = TrunkFollower::new(sample_rate, center_hz, control_hz, measured.0, measured.1);
    println!(
        "Following control {:.4} MHz {} (found at {:.4}, tuner error {:+.0} Hz). Ctrl-C to stop.\n",
        control_hz / 1e6,
        mod_name(measured.1),
        measured.0 / 1e6,
        f.correction_hz()
    );

    let name_of = |tg: u16| match cat {
        Some(k) => k.label(tg),
        None => format!("TG {tg}"),
    };

    let mut n = 0usize;
    let mut syncs = 0u32;
    // Liveness: without a heartbeat, a control channel that is up but between
    // calls looks identical to a hang. Track activity and print a status line
    // every few seconds when nothing else has.
    let mut blocks_since_print = 0u32;
    let mut oob = 0u32;
    let mut enc = 0u32;
    let mut gate = GrantGate::new(50);
    // Throughput check: if the follower cannot process samples as fast as the
    // radio delivers them, the driver's buffer overflows and drops samples —
    // which preserves the short frame sync but shreds the longer payload that
    // carries grants, the exact "syncs climb, zero grants" signature. Compare
    // achieved sample rate against the requested one to catch it.
    let start = std::time::Instant::now();
    let mut total_pairs: u64 = 0;
    // Optional capture of exactly what the live path received, so it can be
    // decoded offline and compared against a clean recording. Capped so it
    // cannot grow without bound.
    let mut dump: Vec<f32> = Vec::new();
    let dump_cap = (sample_rate as usize) * 2 * 10; // ~10 s
                                                    // ~10 blocks per second (each block is a tenth of a second of air).
    const HEARTBEAT_BLOCKS: u32 = 30;
    loop {
        let got = match src.read(&mut buf) {
            Ok(0) => continue,
            Ok(k) => k,
            Err(hs_source::SourceError::Eof) => break,
            Err(e) => {
                eprintln!("capture error: {e:?}");
                break;
            }
        };
        total_pairs += (got / 2) as u64;
        if dump_iq.is_some() && dump.len() < dump_cap {
            dump.extend_from_slice(&buf[..got.min(dump_cap - dump.len())]);
        }
        let out = f.process(&buf[..got]);
        syncs += out.control_syncs;
        gate.tick();
        blocks_since_print += 1;
        let mut printed = false;
        for (tg, hz) in &out.started {
            println!("  start {} on {:.4} MHz", name_of(*tg), *hz as f64 / 1e6);
            printed = true;
        }
        for (tg, hz) in &out.grants_out_of_band {
            if gate.fresh(*hz) {
                oob += 1;
                println!(
                    "  (call {} on {:.4} MHz — outside the tuned band, not followed)",
                    name_of(*tg),
                    *hz as f64 / 1e6
                );
                printed = true;
            }
        }
        for (tg, hz) in &out.grants_encrypted {
            if gate.fresh(*hz) {
                enc += 1;
                println!("  (call {} encrypted — skipped)", name_of(*tg));
                printed = true;
            }
        }
        for c in &out.completed {
            n += 1;
            report_call(c, cat, n);
            printed = true;
        }
        if printed {
            blocks_since_print = 0;
        } else if blocks_since_print >= HEARTBEAT_BLOCKS {
            blocks_since_print = 0;
            let secs = start.elapsed().as_secs_f64().max(1e-3);
            let achieved = total_pairs as f64 / secs / 1e6;
            let want = sample_rate / 1e6;
            let drop_flag = if achieved < want * 0.95 {
                "  ⚠ SAMPLES DROPPING (can't keep up)"
            } else {
                ""
            };
            println!(
                "  … control up: {syncs} frame syncs, {n} calls followed, {oob} out of band, \
                 {enc} encrypted  |  {achieved:.2}/{want:.2} Msps{drop_flag}",
            );
        }
    }
    if let (Some(path), false) = (dump_iq, dump.is_empty()) {
        let mut bytes = Vec::with_capacity(dump.len() * 4);
        for v in &dump {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        match std::fs::write(path, bytes) {
            Ok(()) => println!(
                "wrote {} s of received IQ to {path}",
                dump.len() / 2 / sample_rate as usize
            ),
            Err(e) => eprintln!("could not write {path}: {e}"),
        }
    }
    println!("\nstopped: {syncs} control frame syncs, {n} calls followed, {oob} out of band");
}
