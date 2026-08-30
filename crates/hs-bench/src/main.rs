//! hs-bench: the measuring instrument, built before the thing being measured.
//!
//! Two modes:
//!   * `synth` (default): generate a P25 transmission, pass it through a
//!     configurable two-ray + AWGN channel, and report decode metrics with
//!     the equalizer ENABLED vs BYPASSED — the A/B number that tests the
//!     project thesis without needing a field recording.
//!   * `file <path>`: run a raw interleaved-f32 IQ recording (`.cf32`) at a
//!     given sample rate through the decoder and report metrics. This is the
//!     entry point for a captured P25 control-channel corpus.
//!
//! Baselines from SDRTrunk / OP25 / GopherTrunk on the same corpus belong in
//! `results/baselines.md` (checked in); the corpus IQ itself is never
//! committed — see .gitignore.

mod channel;

use hs_core::decoder::{ChannelDecoder, EqMode};
use hs_p25::synth::{build_ldu1, build_tsdu};
use hs_p25::voice::ImbeFrame;
use std::io::Read;

const RATE: f64 = 48000.0;

#[derive(Default, Debug)]
struct Metrics {
    syncs: u32,
    grants: usize,
    pcm_samples: usize,
    #[allow(dead_code)] // reported in file mode, not the synth A/B table
    encrypted_skips: usize,
}

fn build_test_stream() -> Vec<u8> {
    // IDEN_UP + group voice grant, then one LDU of clear voice.
    let iden_args: u64 = {
        let iden = 1u64 << 60;
        let bw = 100u64 << 51;
        let sign = 1u64 << 50;
        let spacing = 100u64 << 32;
        let base = 851_012_500u64 / 5;
        iden | bw | sign | spacing | base
    };
    let channel = (1u64 << 12) | 10;
    let grant_args = (channel << 40) | (0x2F93u64 << 24) | 0xBEEF1;
    let mut stream = build_tsdu(0x293, &[(0x3D, 0, iden_args), (0x00, 0, grant_args)]);

    let mut frames: [ImbeFrame; 9] = [[[0u8; 23]; 8]; 9];
    let widths = [23usize, 23, 23, 23, 15, 15, 15, 7];
    for (k, fr) in frames.iter_mut().enumerate() {
        for (w, row) in fr.iter_mut().enumerate() {
            for (x, cell) in row.iter_mut().enumerate().take(widths[w]) {
                *cell = (((k + 1) * (w + 2) * (x + 5)) % 2) as u8;
            }
        }
    }
    stream.extend(build_ldu1(0x293, &frames));
    stream
}

fn modulate(dibits: &[u8]) -> Vec<f32> {
    use hs_dsp::modulator::C4fmModulator;
    use hs_dsp::C32;
    let mut m = C4fmModulator::new(RATE);
    let mut iq: Vec<C32> = Vec::new();
    for i in 0..400 {
        m.modulate(if i % 2 == 0 { 0b01 } else { 0b11 }, &mut iq);
    }
    for &d in dibits {
        m.modulate(d, &mut iq);
    }
    for _ in 0..200 {
        m.modulate(0b00, &mut iq);
    }
    let mut out = Vec::with_capacity(iq.len() * 2);
    for c in iq {
        out.push(c.re);
        out.push(c.im);
    }
    out
}

fn run_decoder(iq: &[f32], mode: EqMode) -> Metrics {
    let mut dec = ChannelDecoder::new(RATE, mode);
    let out = dec.process(iq);
    Metrics {
        syncs: out.syncs,
        grants: out.grants.len(),
        pcm_samples: out.pcm.len(),
        encrypted_skips: out.encrypted_skips.len(),
    }
}

fn synth_bench() {
    println!("HoosierSDR benchmark — synthetic P25 through two-ray + AWGN\n");
    println!(
        "{:>8}  {:>10}  {:>18}  {:>18}",
        "Es/N0", "echo", "BYPASS (sync/grant/pcm)", "EQUALIZED (sync/grant/pcm)"
    );

    let clean = modulate(&build_test_stream());
    // Sweep noise and a fixed simulcast echo (~2 symbols at 48 kHz = 20 samp).
    for &esno in &[30.0f32, 18.0, 12.0, 9.0, 6.0] {
        let echo_delay = 20;
        let echo_gain = 0.45;
        let iq = channel::impair(&clean, echo_delay, echo_gain, esno, 0x1234_5678);
        let b = run_decoder(&iq, EqMode::Bypass);
        let e = run_decoder(&iq, EqMode::Enabled);
        println!(
            "{:>6.0}dB  {:>4}smp×{:>0.2}  {:>7}/{:>2}/{:>6}      {:>7}/{:>2}/{:>6}",
            esno,
            echo_delay,
            echo_gain,
            b.syncs,
            b.grants,
            b.pcm_samples,
            e.syncs,
            e.grants,
            e.pcm_samples,
        );
    }
    println!(
        "\nThe C4FM column above is a control, not the thesis. Its echo is applied\n\
         at complex baseband, before the FM discriminator, and the C4FM path's\n\
         equalizer is a real symbol-domain LMS *after* the discriminator — a\n\
         nonlinearity it cannot see through. Equalized neither helps nor harms\n\
         there, as expected; C4FM is not where the project's claim lives."
    );

    cqpsk_thesis();

    println!("\nField-IQ corpus runner: `hs-bench file <path.cf32> [rate]`.");
}

/// The thesis, measured on the path it is about.
///
/// HoosierSDR's claim is that equalizing the complex symbol stream *before*
/// differential detection recovers CQPSK/LSM through multipath that a
/// conventional decoder — which detects differentially first — cannot. That is
/// the CMA stage on the CQPSK receiver, and comparing it against the bare
/// (bypass) front end is precisely HoosierSDR against the structure every other
/// open-source P25 decoder uses.
///
/// Measured as raw dibit bit-error rate through the front end, not full-frame
/// decode: the framer needs a clean run of syncs to report anything at all, so
/// under heavy ISI it reads zero for *both* receivers and hides the very gap
/// this is meant to show. BER exposes the gap directly — the same quantity the
/// `thesis_on_live_iq_cma_beats_bare_on_isi` unit test asserts on.
fn cqpsk_thesis() {
    use hs_dsp::cqpsk::{modulate_iq, CqpskReceiver};
    use hs_dsp::C32;

    println!("\nCQPSK / LSM — the thesis (CMA before differential detection):\n");
    println!(
        "{:>8}  {:>12}  {:>14}  {:>14}",
        "Es/N0", "echo", "BYPASS BER", "EQUALIZED BER"
    );

    // Random dibits, long enough to settle acquisition and leave a measured
    // tail. Deterministic seed for reproducibility.
    let mut st = 0x1234_5678u64;
    let dibits: Vec<u8> = (0..4000)
        .map(|_| {
            st ^= st << 13;
            st ^= st >> 7;
            st ^= st << 17;
            (st & 3) as u8
        })
        .collect();
    let sps = 10usize;
    let clean = modulate_iq(&dibits, sps, 0.2);
    let mut flat = Vec::with_capacity(clean.len() * 2);
    for c in &clean {
        flat.push(c.re);
        flat.push(c.im);
    }

    // Recover dibits through a receiver and score against the reference at the
    // best of the four π/2 rotations (the differential front end leaves an
    // unknown constant quarter-turn the frame sync would otherwise resolve).
    let decode = |iq: &[f32], equalized: bool| -> f64 {
        let mut recv = if equalized {
            CqpskReceiver::new(sps, 0.2)
        } else {
            CqpskReceiver::new_bare(sps, 0.2)
        };
        let mut out = Vec::new();
        let mut i = 0;
        while i + 1 < iq.len() {
            if let Some(d) = recv.push(C32::new(iq[i], iq[i + 1])) {
                out.push(d);
            }
            i += 2;
        }
        if out.len() < 400 {
            return 1.0;
        }
        // Score a settled 300-symbol window over both unknowns the front end
        // legitimately leaves: the π/2 rotation (via rotate_dibit) and the
        // constant delay from differential + filter latency.
        let win = 300usize;
        let start = out.len() - win - 50;
        let seg = &out[start..start + win];
        let mut best = 1.0f64;
        for k in 0..4u8 {
            let derot: Vec<u8> = seg
                .iter()
                .map(|&d| hs_dsp::cqpsk::rotate_dibit(d, k))
                .collect();
            for delay in 0..dibits.len().saturating_sub(win) {
                let r = &dibits[delay..delay + win];
                let mut bits = 0u32;
                for (a, b) in derot.iter().zip(r) {
                    bits += (((a ^ b) & 1) + ((a ^ b) >> 1 & 1)) as u32;
                }
                let e = bits as f64 / (2 * win) as f64;
                if e < best {
                    best = e;
                }
            }
        }
        best
    };

    for &(esno, echo_delay, echo_gain) in &[
        (30.0f32, 0usize, 0.0f32),
        (30.0, 20, 0.3),
        (30.0, 20, 0.6),
        (18.0, 20, 0.6),
        (12.0, 20, 0.6),
        (9.0, 20, 0.6),
    ] {
        let iq = channel::impair(&flat, echo_delay, echo_gain, esno, 0xC0FFEE);
        let b = decode(&iq, false);
        let e = decode(&iq, true);
        println!(
            "{:>6.0}dB  {:>6}smp×{:>0.2}  {:>13.3}  {:>13.3}",
            esno, echo_delay, echo_gain, b, e,
        );
    }
    println!(
        "\nLower is better. Rows sweep a two-symbol simulcast echo from none to\n\
         60% amplitude. BYPASS is the conventional differential-first receiver;\n\
         EQUALIZED runs the CMA equalizer ahead of the differential detector.\n\
         On the clean channel both are perfect, so the equalizer costs nothing.\n\
         As the echo deepens BYPASS collapses — at 60% its differential phase is\n\
         corrupted past recovery (BER → 1.0, a spectral null the receiver cannot\n\
         see through) — while EQUALIZED inverts the channel and decodes it. That\n\
         gap is the thesis: HoosierSDR decoding a simulcast channel the\n\
         structure every other open-source P25 decoder uses cannot."
    );
}

fn file_bench(path: &str, rate: f64) {
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("cannot open {path}: {e}");
            std::process::exit(1);
        }
    };
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes).expect("read IQ file");
    let n = bytes.len() / 4;
    let mut iq = Vec::with_capacity(n);
    for i in 0..n {
        iq.push(f32::from_le_bytes(
            bytes[i * 4..i * 4 + 4].try_into().unwrap(),
        ));
    }
    println!("Running {} ({} IQ samples @ {} Hz)\n", path, n / 2, rate);
    let mut dec = ChannelDecoder::new(rate, EqMode::Enabled);
    let out = dec.process(&iq);
    println!("vocoder:          {}", dec.vocoder_name());
    println!("frame syncs:      {}", out.syncs);
    println!("voice grants:     {}", out.grants.len());
    println!("encrypted skips:  {}", out.encrypted_skips.len());
    println!(
        "decoded PCM:      {} samples ({:.1}s)",
        out.pcm.len(),
        out.pcm.len() as f64 / 8000.0
    );
    for g in out.grants.iter().take(20) {
        println!(
            "  grant TG {:<6} src {:<8} -> {:.4} MHz{}",
            g.talkgroup,
            g.source_unit,
            g.freq_hz as f64 / 1e6,
            if g.encrypted {
                "  [ENCRYPTED, skipped]"
            } else {
                ""
            }
        );
    }
}

/// Time the channelizer against real time.
///
/// `--follow` on a live radio only works if the channelizer keeps up with the
/// air, and at 2.4 MHz it did not: this reported 1.6x slower than real time
/// when it was written. That is the number the optimisation work has to move,
/// so it lives here as a permanent instrument rather than in a shell one-liner.
fn chan_bench() {
    use hs_dsp::channelizer::Channelizer;

    // One control channel plus a couple of traffic channels — what following a
    // busy system actually costs, not the best case of a single channel.
    for (rate, channels) in [
        (240_000.0f64, 1usize),
        (2_400_000.0, 1),
        (2_400_000.0, 3),
        (2_400_000.0, 8),
    ] {
        let offsets: Vec<f64> = (0..channels).map(|i| i as f64 * 25_000.0).collect();
        let mut ch = Channelizer::new(rate, &offsets);

        // Four seconds of noise; content does not affect the cost.
        let secs = 4.0;
        let n = (rate * secs) as usize;
        let mut iq = Vec::with_capacity(n * 2);
        let mut x = 12345u32;
        for _ in 0..n {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            iq.push((x >> 16) as f32 / 32768.0 - 1.0);
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            iq.push((x >> 16) as f32 / 32768.0 - 1.0);
        }

        let t0 = std::time::Instant::now();
        let out = ch.process(&iq);
        let dt = t0.elapsed().as_secs_f64();
        let produced = out[0].len() / 2;

        println!(
            "{:>9.0} Hz  {} channel(s):  {:.3}s cpu for {:.1}s of air  =  {:.2}x real time{}",
            rate,
            channels,
            dt,
            secs,
            secs / dt,
            if dt > secs { "   TOO SLOW" } else { "" }
        );
        assert!(produced > 0, "channelizer produced nothing");
    }
}

/// What decoding a call twice actually costs, over a call long enough to
/// matter.
///
/// The follower hedges: it decodes each call with its control channel's
/// modulation *and* the other one, dropping the second as soon as the first
/// proves itself. On a two-second recording that saving is invisible next to
/// start-up costs, so measure it where it lives — a long transmission, which
/// is what a dispatch call actually is.
fn hedge_bench() {
    use hs_core::decoder::{ChannelDecoder, EqMode, Modulation};
    use hs_dsp::cqpsk::modulate_iq;

    // A 30-second CQPSK transmission: one LDU is 180 ms of voice, so ~167 of
    // them. Built once and decoded three ways.
    let mut frames: [ImbeFrame; 9] = [[[0u8; 23]; 8]; 9];
    let widths = [23usize, 23, 23, 23, 15, 15, 15, 7];
    for (k, fr) in frames.iter_mut().enumerate() {
        for (w, row) in fr.iter_mut().enumerate() {
            for (x, cell) in row.iter_mut().enumerate().take(widths[w]) {
                *cell = (((k + 1) * (w + 2) * (x + 5)) % 2) as u8;
            }
        }
    }
    let ldus = 167;
    let mut bits = Vec::new();
    for _ in 0..ldus {
        bits.extend(build_ldu1(0x261, &frames));
    }
    let sps = 10;
    let iq = modulate_iq(&bits, sps, 0.2);
    let mut flat = Vec::with_capacity(iq.len() * 2);
    for v in &iq {
        flat.push(v.re);
        flat.push(v.im);
    }
    let secs = bits.len() as f64 / 4800.0;

    let run = |mods: &[Modulation]| -> f64 {
        let mut decs: Vec<ChannelDecoder> = mods
            .iter()
            .map(|&m| {
                let eq = match m {
                    Modulation::Cqpsk => EqMode::Enabled,
                    Modulation::C4fm => EqMode::Bypass,
                };
                ChannelDecoder::with_offset(RATE, m, eq, 0.0)
            })
            .collect();
        let t0 = std::time::Instant::now();
        for d in decs.iter_mut() {
            d.process(&flat);
        }
        t0.elapsed().as_secs_f64()
    };

    let one = run(&[Modulation::Cqpsk]);
    let both = run(&[Modulation::Cqpsk, Modulation::C4fm]);

    println!("a {secs:.1}s call, decoded at {RATE:.0} Hz:\n");
    println!("  inherited modulation only : {one:.3}s cpu");
    println!("  both, the whole call      : {both:.3}s cpu");
    println!(
        "\n  hedging until confirmed saves {:.3}s per call ({:.0}% of the decode)",
        both - one,
        100.0 * (both - one) / both
    );
    println!(
        "  real time margin: {:.1}x with one decoder, {:.1}x with two",
        secs / one,
        secs / both
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        None | Some("synth") => synth_bench(),
        Some("chan") => chan_bench(),
        Some("hedge") => hedge_bench(),
        Some("file") => {
            let path = args
                .get(2)
                .expect("usage: hs-bench file <path.cf32> [rate]");
            let rate = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(48000.0);
            file_bench(path, rate);
        }
        Some(other) => {
            eprintln!(
                "unknown mode '{other}'. usage: hs-bench [synth | chan | hedge | file <path> [rate]]"
            );
            std::process::exit(2);
        }
    }
}
