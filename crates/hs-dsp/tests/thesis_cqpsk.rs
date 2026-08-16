//! The thesis, as a controlled experiment.
//!
//! Claim: on a channel with inter-symbol interference, placing an adaptive
//! equalizer BEFORE differential detection recovers the data, while
//! differential-detecting first (what every other open-source P25 decoder
//! does) does not — because differential detection is a nonlinearity that
//! scrambles the ISI irrecoverably.
//!
//! We build a CQPSK symbol stream, pass it through a complex two-ray channel
//! (a simulcast-like echo), and decode it two ways:
//!   A. differential detection then slice        — the baseline
//!   B. FSW-trained equalizer, THEN differential  — HoosierSDR
//! and assert B's symbol-error rate is dramatically lower than A's.

use hs_dsp::cqpsk::{differential_detect, dphase_to_dibit, modulate_symbols, EqualizedCqpsk};
use hs_dsp::C32;

/// Deterministic dibit stream: 24-symbol FSW-like preamble (known) + payload.
fn make_dibits(n_payload: usize) -> (Vec<u8>, usize) {
    // A fixed 24-dibit "sync" the equalizer is allowed to train on.
    let sync = [
        1u8, 3, 1, 3, 0, 2, 0, 2, 1, 1, 3, 3, 0, 0, 2, 2, 1, 0, 3, 2, 1, 3, 0, 2,
    ];
    let mut d = sync.to_vec();
    let train_len = d.len();
    let mut lfsr = 0xACE1u16;
    for _ in 0..n_payload {
        lfsr = (lfsr >> 1) ^ (if lfsr & 1 != 0 { 0xB400 } else { 0 });
        d.push((lfsr & 3) as u8);
    }
    (d, train_len)
}

/// Apply a symbol-spaced complex two-ray channel: y[n] = x[n] + g·e^{jθ}·x[n-1].
fn two_ray(symbols: &[C32], gain: f32, theta: f32) -> Vec<C32> {
    let echo = C32::new(gain * theta.cos(), gain * theta.sin());
    let mut out = vec![C32::ZERO; symbols.len()];
    for i in 0..symbols.len() {
        let mut y = symbols[i];
        if i > 0 {
            y = y + echo * symbols[i - 1];
        }
        out[i] = y;
    }
    out
}

/// Baseline: differential detection directly on the channel output.
fn decode_baseline(rx: &[C32], skip: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 1..rx.len() {
        if i < skip {
            continue;
        }
        out.push(dphase_to_dibit(differential_detect(rx[i], rx[i - 1])));
    }
    out
}

fn errors(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).filter(|(x, y)| x != y).count()
}

#[test]
fn equalizer_before_diff_detection_beats_baseline_on_isi() {
    let (dibits, train_len) = make_dibits(2000);
    let clean = modulate_symbols(&dibits);

    // Strong simulcast-like echo: 55% amplitude, quarter-turn phase.
    let rx = two_ray(&clean, 0.55, std::f32::consts::FRAC_PI_4);

    // --- Path A: baseline (diff-detect the raw channel output) ---
    let base = decode_baseline(&rx, train_len);
    let payload = &dibits[train_len..];
    let base_err = errors(&base[..payload.len()], payload);
    let base_ser = base_err as f64 / payload.len() as f64;

    // --- Path B: equalizer BEFORE differential detection ---
    // Feed the equalizer symbol-spaced (T-spaced here; the FSE structure still
    // applies with one sample/symbol). Train on the known sync symbols.
    let mut eq = EqualizedCqpsk::new(7, 0.02);
    let clean_syms = modulate_symbols(&dibits); // absolute reference symbols
    let train_samples: Vec<C32> = rx[..train_len].to_vec();
    let desired: Vec<Option<C32>> = clean_syms[..train_len].iter().map(|&s| Some(s)).collect();
    eq.train(&train_samples, &desired, 40);

    let mut eq_out = Vec::new();
    for &x in &rx {
        if let Some(d) = eq.push(x, true) {
            eq_out.push(d);
        }
    }
    // eq_out[i] corresponds to differential detection at symbol i+1; align to
    // payload the same way as the baseline.
    let eq_payload = &eq_out[train_len - 1..];
    let eq_err = errors(&eq_payload[..payload.len()], payload);
    let eq_ser = eq_err as f64 / payload.len() as f64;

    eprintln!(
        "two-ray ISI: baseline SER = {:.3}, equalized SER = {:.3}",
        base_ser, eq_ser
    );

    // The baseline should be badly degraded by the ISI...
    assert!(
        base_ser > 0.10,
        "baseline unexpectedly clean (SER {base_ser:.3}) — echo too weak to be a real test"
    );
    // ...and the equalizer-before-diff path should be dramatically better.
    assert!(
        eq_ser < base_ser / 2.0,
        "equalizer did not beat baseline: eq {eq_ser:.3} vs base {base_ser:.3}"
    );
}
