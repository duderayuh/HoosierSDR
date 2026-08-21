//! `--follow`: run the trunk follower, from a file or a live radio.
//!
//! This is the scanner. Everything else in the CLI decodes one channel that
//! the user names; here the control channel names the channels, and each call
//! it announces is decoded and written out as its own audio file.

use hs_core::decoder::Modulation;
use hs_core::follow::{measure_carrier, pick_modulation, Call, GrantGate, TrunkFollower};

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
    if let Err(msg) = hs_core::follow::in_band(sample_rate, center_hz, control_hz) {
        eprintln!("{msg}");
        eprintln!("Set --freq to the frequency the capture was tuned to.");
        std::process::exit(2);
    }
}

/// Print control-channel failover events; returns whether anything printed.
fn report_control_moves(out: &hs_core::follow::FollowOutput) -> bool {
    let mut printed = false;
    if let Some((from, to)) = out.control_moved {
        println!(
            "  control channel moved: {:.4} MHz went quiet, following {:.4} MHz",
            from as f64 / 1e6,
            to as f64 / 1e6
        );
        printed = true;
    }
    if let Some(alternates) = &out.control_lost {
        if alternates.is_empty() {
            println!(
                "  control channel lost, and the site announced no alternate.\n  \
                 If this persists, re-run --scan to find where the system went."
            );
        } else {
            let list = alternates
                .iter()
                .map(|hz| format!("{:.4} MHz", *hz as f64 / 1e6))
                .collect::<Vec<_>>()
                .join(", ");
            println!(
                "  control channel lost. Its announced alternates are outside this\n  \
                 band — retune to one of: {list}"
            );
        }
        printed = true;
    }
    printed
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
    let mut gate = GrantGate::new(5.0);
    for chunk in iq.chunks(block) {
        let out = f.process(chunk);
        syncs += out.control_syncs;
        gate.tick(0.1);
        report_control_moves(&out);
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
/// The samples come from a background reader thread, not the processing loop.
/// A single-threaded loop that read the radio and then decoded it dropped
/// samples on a slower machine: the RTL delivers at a fixed rate, and any time
/// spent decoding is time not spent draining its buffer, so the buffer
/// overflowed and the payload behind each frame sync was lost. The offline
/// decoder runs at more than ten times real time, so the decode was never the
/// problem — keeping the radio drained was. A dedicated reader thread does
/// nothing but read, handing blocks to the decoder through a bounded queue; if
/// the decoder ever falls behind, the queue drops its oldest block rather than
/// letting the radio's own buffer overflow, which also keeps latency bounded.
/// The reader runs during the startup measurement too, so that no longer
/// stalls the radio either.
// Only reachable from the live-capture path; a headless build still compiles
// it so the two paths cannot drift apart unnoticed.
#[cfg_attr(not(feature = "rtlsdr"), allow(dead_code))]
pub fn run_live<S: hs_source::SdrSource + Send + 'static>(
    mut src: S,
    sample_rate: f64,
    center_hz: f64,
    control_hz: f64,
    measured_hz: Option<f64>,
    cat: Option<&hs_core::catalog::CsvCatalog>,
    dump_iq: Option<&str>,
) {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc::{sync_channel, TrySendError};
    use std::sync::Arc;

    check_in_band(sample_rate, center_hz, control_hz);
    let block = (sample_rate as usize / 10) * 2;

    // Prime on a second of live air before building anything, so a freshly
    // tuned dongle's AGC has settled and the control channel's real frequency
    // and modulation can be measured off it.
    println!("Measuring the control channel (a few seconds)…");
    let mut buf = vec![0.0f32; block];
    let target = block * 30;
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

    // Hand the radio to a reader thread now — before the sweep — so it keeps
    // draining while the (several-second) measurement runs on this thread.
    // A queue of ~6 seconds; if the decoder ever falls behind, the reader
    // drops the oldest block to keep the radio's own buffer empty.
    let (tx, rx) = sync_channel::<Vec<f32>>(64);
    let drops = Arc::new(AtomicU64::new(0));
    let reader_drops = Arc::clone(&drops);
    let reader = std::thread::spawn(move || {
        let mut buf = vec![0.0f32; block];
        loop {
            match src.read(&mut buf) {
                Ok(0) => continue,
                Ok(n) => match tx.try_send(buf[..n].to_vec()) {
                    Ok(()) => {}
                    // Queue full: the decoder is behind. Drop this block rather
                    // than block the reader — the whole point is to keep the
                    // radio drained so its own buffer never overflows. A drop
                    // here is one clean lost block; a stall there corrupts the
                    // stream. Newest data keeps flowing.
                    Err(TrySendError::Full(_)) => {
                        reader_drops.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(TrySendError::Disconnected(_)) => return,
                },
                Err(hs_source::SourceError::Eof) => return,
                Err(e) => {
                    eprintln!("capture error: {e:?}");
                    return;
                }
            }
        }
    });

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

    // The reader has been filling the queue throughout the several-second
    // measurement; those blocks are now stale. Discard them and start on live
    // air, so the follower has no built-in latency and the drops that occurred
    // while nothing was consuming do not read as a live problem.
    while rx.try_recv().is_ok() {}
    drops.store(0, Ordering::Relaxed);

    let mut n = 0usize;
    let mut syncs = 0u32;
    let mut blocks_since_print = 0u32;
    let mut oob = 0u32;
    let mut enc = 0u32;
    let mut gate = GrantGate::new(5.0);
    let start = std::time::Instant::now();
    let mut total_pairs: u64 = 0;
    // Drops seen at the previous heartbeat, so the warning reflects blocks lost
    // *recently* rather than a stale one-time count.
    let mut last_drops: u64 = 0;
    // Write the received IQ straight to disk, flushed per block, rather than
    // buffering it for the end: a live run ends with Ctrl-C, which terminates
    // the process before any end-of-loop write would run, so a buffered dump
    // was always lost. Flushing each block means whatever ran is on disk.
    // A `.cu8` path is written in the RTL's native unsigned-8-bit format (two
    // bytes per complex sample, a quarter the size of f32), so a few seconds
    // fits in a small upload; any other extension is interleaved f32.
    let dump_cu8 = dump_iq.map(|p| p.ends_with(".cu8")).unwrap_or(false);
    let mut dump_file = dump_iq.map(|path| {
        let f = std::fs::File::create(path).unwrap_or_else(|e| {
            eprintln!("could not create {path}: {e}");
            std::process::exit(1);
        });
        (path.to_string(), std::io::BufWriter::new(f))
    });
    const HEARTBEAT_BLOCKS: u32 = 30;

    // The reader owns the radio now; blocks arrive through the queue until it
    // ends (EOF or error), at which point the sender drops and recv errors.
    while let Ok(chunk) = rx.recv() {
        let got = chunk.len();
        total_pairs += (got / 2) as u64;
        if let Some((_, w)) = dump_file.as_mut() {
            use std::io::Write;
            if dump_cu8 {
                let mut bytes = Vec::with_capacity(got);
                for v in &chunk[..got] {
                    bytes.push((v * 127.5 + 127.5).round().clamp(0.0, 255.0) as u8);
                }
                let _ = w.write_all(&bytes);
            } else {
                let mut bytes = Vec::with_capacity(got * 4);
                for v in &chunk[..got] {
                    bytes.extend_from_slice(&v.to_le_bytes());
                }
                let _ = w.write_all(&bytes);
            }
            let _ = w.flush();
        }
        let out = f.process(&chunk);
        syncs += out.control_syncs;
        gate.tick(0.1);
        blocks_since_print += 1;
        let mut printed = report_control_moves(&out);
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
            let dropped = drops.load(Ordering::Relaxed);
            let recent = dropped - last_drops;
            last_drops = dropped;
            let warn = if recent > 0 {
                "  ⚠ decoder behind"
            } else {
                ""
            };
            println!(
                "  … control up: {syncs} frame syncs, {n} calls followed, {oob} out of band, \
                 {enc} encrypted  |  {achieved:.2}/{want:.2} Msps, {dropped} dropped{warn}",
            );
        }
    }
    let _ = reader.join();
    if let Some((path, mut w)) = dump_file {
        use std::io::Write;
        let _ = w.flush();
        println!("received IQ saved to {path}");
    }
    println!("\nstopped: {syncs} control frame syncs, {n} calls followed, {oob} out of band");
}
