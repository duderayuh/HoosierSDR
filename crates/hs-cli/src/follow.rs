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
        "  CALL  {name:<20} unit {:<9} {:.4} MHz  {m:5}  {secs:.1}s{patch}",
        c.source_unit,
        c.freq_hz as f64 / 1e6
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
    for chunk in iq.chunks(block) {
        let out = f.process(chunk);
        syncs += out.control_syncs;
        for (tg, hz) in &out.started {
            let name = match cat {
                Some(k) => k.label(*tg),
                None => format!("TG {tg}"),
            };
            println!("  start {name} on {:.4} MHz", *hz as f64 / 1e6);
        }
        for c in &out.completed {
            n += 1;
            report_call(c, cat, n);
        }
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
) {
    check_in_band(sample_rate, center_hz, control_hz);
    let block = (sample_rate as usize / 10) * 2;
    let mut buf = vec![0.0f32; block];

    let found = match measured_hz {
        Some(m) => Some((m, Modulation::Cqpsk)),
        None => {
            // Half a second of air is plenty to see a continuously-keyed
            // control channel; accumulate it a block at a time.
            let mut prime: Vec<f32> = Vec::with_capacity(block * 5);
            while prime.len() < block * 5 {
                match src.read(&mut buf) {
                    Ok(0) => continue,
                    Ok(n) => prime.extend_from_slice(&buf[..n]),
                    Err(e) => {
                        eprintln!("capture error while measuring: {e:?}");
                        return;
                    }
                }
            }
            measure_carrier(&prime, sample_rate, center_hz, control_hz)
        }
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

    let mut n = 0usize;
    let mut syncs = 0u32;
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
        let out = f.process(&buf[..got]);
        syncs += out.control_syncs;
        for (tg, hz) in &out.started {
            let name = match cat {
                Some(k) => k.label(*tg),
                None => format!("TG {tg}"),
            };
            println!("  start {name} on {:.4} MHz", *hz as f64 / 1e6);
        }
        for c in &out.completed {
            n += 1;
            report_call(c, cat, n);
        }
    }
    println!("\nstopped: {syncs} control frame syncs, {n} calls");
}
