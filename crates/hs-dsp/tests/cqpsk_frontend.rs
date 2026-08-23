//! End-to-end CQPSK front end on realistic off-air IQ.
//!
//! Unlike `thesis_cqpsk` (symbol-synchronous, no carrier offset), this feeds
//! the full `CqpskReceiver` a continuous, RRC-shaped, oversampled signal with
//! a carrier frequency offset, a fractional timing offset, and AWGN — i.e.
//! what a tuner actually delivers — and asserts the receiver locks (carrier +
//! timing) and recovers the transmitted dibits.

use hs_dsp::cqpsk::{modulate_iq, CqpskReceiver};
use hs_dsp::C32;

const SPS: usize = 10;
const BETA: f64 = 0.2;

fn xorshift(seed: u64) -> impl FnMut() -> f32 {
    let mut s = seed | 1;
    move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        // Irwin–Hall-ish gaussian.
        let mut acc = 0.0f32;
        for _ in 0..4 {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            acc += (s >> 40) as f32 / (1u64 << 24) as f32;
        }
        (acc - 2.0) / 1.15
    }
}

/// Apply carrier frequency+phase offset, then AWGN, to an IQ stream.
fn impair(iq: &[C32], f_off: f32, phase0: f32, noise: f32, seed: u64) -> Vec<C32> {
    let mut g = xorshift(seed);
    let mut ph = phase0;
    let mut out = Vec::with_capacity(iq.len());
    for &s in iq {
        let rot = C32::new(ph.cos(), ph.sin());
        ph += f_off;
        let r = s * rot;
        out.push(C32::new(r.re + noise * g(), r.im + noise * g()));
    }
    out
}

fn ber(a: &[u8], b: &[u8]) -> f64 {
    let n = a.len().min(b.len());
    let mut bits = 0;
    for i in 0..n {
        bits += ((a[i] ^ b[i]) & 1).count_ones() + (((a[i] ^ b[i]) >> 1) & 1).count_ones();
    }
    bits as f64 / (2 * n) as f64
}

/// Align `recovered` against `reference` and return the best BER, searching
/// over both unknowns the receiver legitimately leaves behind:
///
/// * **delay** — differential detection and filter latency shift the stream by
///   an unknown constant;
/// * **rotation** — blind carrier acquisition resolves the differential-phase
///   bias only modulo π/2, so the dibits come out under one of four fixed
///   permutations. Downstream this is pinned by the Frame Sync Word; here we
///   search it, because the front end alone cannot know it.
fn best_ber(recovered: &[u8], reference: &[u8]) -> f64 {
    let n = recovered.len().min(300);
    let mut best = 1.0;
    if reference.len() < n {
        return best;
    }
    for k in 0..4u8 {
        let derot: Vec<u8> = recovered[..n]
            .iter()
            .map(|&d| hs_dsp::cqpsk::rotate_dibit(d, k))
            .collect();
        for delay in 0..=(reference.len() - n) {
            let e = ber(&derot, &reference[delay..delay + n]);
            if e < best {
                best = e;
            }
        }
    }
    best
}

#[test]
fn full_receiver_locks_and_recovers_on_offset_signal() {
    // Random dibit payload. Long enough to cover the receiver's blind
    // acquisition window (it emits nothing for the first ~465 symbols while it
    // settles timing and averages out the carrier bias) plus a settled tail.
    let mut s = 0x1234_5678u64;
    let dibits: Vec<u8> = (0..2400)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s & 3) as u8
        })
        .collect();

    let iq = modulate_iq(&dibits, SPS, BETA);
    // Carrier offset 0.01 rad/sample, phase 0.6, mild noise. No ISI here —
    // this isolates the carrier+timing recovery integration.
    let rx = impair(&iq, 0.01, 0.6, 0.05, 99);

    let mut recv = CqpskReceiver::new(SPS, BETA);
    let mut out = Vec::new();
    for &x in &rx {
        if let Some(d) = recv.push(x) {
            out.push(d);
        }
    }

    assert!(out.len() > 1200, "too few symbols: {}", out.len());
    let tail = &out[out.len() - 900..];
    let e = best_ber(tail, &dibits);
    eprintln!("CQPSK front end BER (carrier+timing offset) = {e:.4}");
    assert!(e < 0.05, "front end did not lock: BER {e:.4}");

    // The recovered carrier-frequency bias should match the injected offset:
    // 0.01 rad/sample × SPS = 0.1 rad/symbol of differential-phase bias.
    let expected_bias = 0.01 * SPS as f32;
    assert!(
        (recv.freq_bias() - expected_bias).abs() < 0.02,
        "carrier estimate {} vs expected {}",
        recv.freq_bias(),
        expected_bias
    );
}

/// A traffic channel is idle until a call keys up, so the equalized receiver
/// meets a long run of near-silence and then a sudden full-power onset. That
/// transient once drove the CMA equalizer's taps to NaN — the un-normalized
/// update overshot on samples the AGC had not yet levelled — and a single NaN
/// killed the receiver for the rest of the capture: every output NaN, and no
/// re-acquisition, because `NaN > threshold` is false. The NLMS-normalized
/// update is what keeps that transient bounded. This reproduces the exact
/// shape (idle → signal) and requires a clean decode of the part that carries
/// data.
#[test]
fn recovers_from_a_cold_idle_then_signal_onset() {
    // ~0.4 s of idle: low-level noise, no signal at all.
    let mut n = xorshift(0xC0FFEE);
    // Idle is the band's noise floor, not silence — comparable in level to
    // the signal that follows, as a channelized idle channel actually is.
    let idle: Vec<C32> = (0..4000).map(|_| C32::new(0.5 * n(), 0.5 * n())).collect();

    // Then a real transmission at an offset, exactly as a keyed-up call looks.
    let mut s = 0x9E37_79B9u64;
    let dibits: Vec<u8> = (0..4000)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s & 3) as u8
        })
        .collect();
    let sig = impair(&modulate_iq(&dibits, SPS, BETA), 0.01, 0.6, 0.05, 7);

    let mut rx = idle;
    rx.extend_from_slice(&sig);

    let mut recv = CqpskReceiver::new(SPS, BETA);
    let mut out = Vec::new();
    for &x in &rx {
        if let Some(d) = recv.push(x) {
            out.push(d);
        }
    }

    // Every emitted symbol must be finite — the NaN bug produced a stream that
    // was technically present but all-NaN downstream.
    assert!(
        recv.freq_bias().is_finite(),
        "carrier estimate went non-finite"
    );

    // The tail (well into the transmission) must decode cleanly. Before the
    // fix this was 100% errors, because the receiver never recovered from the
    // onset transient.
    assert!(
        out.len() > 1000,
        "too few symbols after onset: {}",
        out.len()
    );
    let tail = &out[out.len() - 800..];
    let e = best_ber(tail, &dibits);
    eprintln!("cold idle → onset BER = {e:.4}");
    assert!(
        e < 0.05,
        "receiver did not recover from the onset: BER {e:.4}"
    );
}

/// Resample `iq` at a slightly wrong clock (linear interpolation) to inject a
/// fractional, drifting timing offset — the timing recovery must track it.
fn clock_skew(iq: &[C32], ratio: f32) -> Vec<C32> {
    let mut out = Vec::new();
    let mut t = 0.0f32;
    while (t as usize) + 1 < iq.len() {
        let i = t as usize;
        let f = t - i as f32;
        out.push(iq[i] + (iq[i + 1] - iq[i]).scale(f));
        t += ratio;
    }
    out
}

#[test]
fn front_end_tracks_timing_clock_skew() {
    let mut s = 0xC0FFEEu64;
    let dibits: Vec<u8> = (0..1500)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s & 3) as u8
        })
        .collect();
    let iq = modulate_iq(&dibits, SPS, BETA);
    // 0.2% sample-clock error (tuner vs transmitter) + carrier offset + noise.
    let skewed = clock_skew(&iq, 1.002);
    let rx = impair(&skewed, 0.008, 1.1, 0.04, 7);

    let mut recv = CqpskReceiver::new(SPS, BETA);
    let mut out = Vec::new();
    for &x in &rx {
        if let Some(d) = recv.push(x) {
            out.push(d);
        }
    }
    let tail = &out[out.len().saturating_sub(800)..];
    let e = best_ber(tail, &dibits);
    eprintln!("CQPSK front end BER (clock skew + carrier + noise) = {e:.4}");
    assert!(
        e < 0.05,
        "timing tracking failed under clock skew: BER {e:.4}"
    );
}

/// Symbol-spaced complex two-ray channel applied to the oversampled IQ (echo
/// delayed by one symbol = SPS samples).
fn two_ray_iq(iq: &[C32], gain: f32, theta: f32, sps: usize) -> Vec<C32> {
    let echo = C32::new(gain * theta.cos(), gain * theta.sin());
    let mut out = vec![C32::ZERO; iq.len()];
    for i in 0..iq.len() {
        let mut y = iq[i];
        if i >= sps {
            y = y + echo * iq[i - sps];
        }
        out[i] = y;
    }
    out
}

/// Two-ray channel with a *fractional*-symbol echo delay (linearly interpolated
/// between the surrounding samples) — the simulcast regime the wired equalizers
/// were never tested against. Real simulcast delay spreads are 0.12–0.34 T, not
/// integer symbols, and a symbol-spaced equalizer sampling at the main path's
/// timing cannot cleanly invert a sub-symbol echo.
fn two_ray_iq_frac(iq: &[C32], gain: f32, theta: f32, delay_syms: f32) -> Vec<C32> {
    let echo = C32::new(gain * theta.cos(), gain * theta.sin());
    let d = delay_syms * SPS as f32; // delay in samples, possibly fractional
    let mut out = vec![C32::ZERO; iq.len()];
    for i in 0..iq.len() {
        let mut y = iq[i];
        let idx = i as f32 - d;
        if idx >= 0.0 {
            let i0 = idx.floor() as usize;
            let frac = idx - i0 as f32;
            let i1 = (i0 + 1).min(iq.len() - 1);
            let delayed = iq[i0] + (iq[i1] - iq[i0]).scale(frac);
            y = y + echo * delayed;
        }
        out[i] = y;
    }
    out
}

#[test]
fn thesis_on_live_iq_cma_beats_bare_on_isi() {
    // The whole story on one channel: ISI (two-ray) + carrier offset + timing
    // skew + noise. The CMA-equalized front end (equalizer before differential
    // detection) must beat the bare front end (differential detection first).
    let mut s = 0xBEEF_1234u64;
    let dibits: Vec<u8> = (0..4000)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s & 3) as u8
        })
        .collect();
    let iq = modulate_iq(&dibits, SPS, BETA);
    let isi = two_ray_iq(&iq, 0.6, std::f32::consts::FRAC_PI_4, SPS);
    let rx = impair(&isi, 0.006, 0.9, 0.04, 5);

    let run = |mut recv: CqpskReceiver| -> f64 {
        let mut out = Vec::new();
        for &x in &rx {
            if let Some(d) = recv.push(x) {
                out.push(d);
            }
        }
        // Use the settled tail (CMA needs time to converge).
        let tail = &out[out.len().saturating_sub(1500)..];
        best_ber(tail, &dibits)
    };

    let bare = run(CqpskReceiver::new_bare(SPS, BETA));
    let eq = run(CqpskReceiver::new(SPS, BETA));
    eprintln!("live IQ + ISI: bare BER = {bare:.4}, CMA-equalized BER = {eq:.4}");

    assert!(
        bare > 0.05,
        "channel too easy — bare should struggle (got {bare:.4})"
    );
    assert!(
        eq < bare * 0.5,
        "CMA did not beat bare: eq {eq:.4} vs bare {bare:.4}"
    );
}

/// A single NaN sample (front-end overflow, USB drop) must not permanently kill
/// the receiver. Before the fix the NaN poisoned the DC blocker + AGC IIR state,
/// every later symbol came out NaN, and the lock watchdog (`> LOCK_ERR_MAX`)
/// treats NaN as "locked" — so the receiver decoded garbage forever with no
/// recovery. The front-door guard drops the corrupt sample; decoding must
/// continue cleanly afterward.
#[test]
fn survives_a_nan_sample() {
    let mut s = 0xDEAD_BEEFu64;
    let dibits: Vec<u8> = (0..3000)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s & 3) as u8
        })
        .collect();
    let iq = modulate_iq(&dibits, SPS, BETA);
    let rx = impair(&iq, 0.006, 0.9, 0.04, 11);

    let mut recv = CqpskReceiver::new(SPS, BETA);
    let mut out = Vec::new();
    let nan_at = rx.len() / 2;
    for (i, &x) in rx.iter().enumerate() {
        if i == nan_at {
            assert!(recv.push(C32::new(f32::NAN, 0.0)).is_none());
        }
        if let Some(d) = recv.push(x) {
            out.push(d);
        }
    }

    // A poisoned receiver stops emitting (or emits garbage) after the NaN; the
    // guard must let it keep decoding the whole signal.
    assert!(
        out.len() > 2000,
        "receiver died after NaN: {} symbols",
        out.len()
    );
    let tail = &out[out.len() - 1000..];
    let e = best_ber(tail, &dibits);
    eprintln!("post-NaN tail BER = {e:.4}");
    assert!(
        e < 0.05,
        "receiver did not recover after a NaN sample: BER {e:.4}"
    );
}

/// A cold-started traffic channel can sit on idle noise for minutes before a
/// call keys up, and the equalizer adapts on that AGC-normalized unit-power
/// noise the whole time. The failed-acquisition-window tap reset (every
/// `ACQ_FAIL_LIMIT` windows) keeps it near identity so it never walks into the
/// degenerate CM-DFE minimum. This smoke-tests the reset path on both the CMA
/// and DFE front ends: after a long idle the receiver must still decode the
/// transmission that follows. (It does not prove the DFE-minimum fix — that
/// needs the low-level Airspy capture; it pins that the reset doesn't regress.)
#[test]
fn recovers_after_a_long_idle() {
    let mut n = xorshift(0xBAD_F00Du64);
    // ~5 acquisition windows of idle noise (2.4 s at 10 sps).
    let idle: Vec<C32> = (0..24_000)
        .map(|_| C32::new(0.5 * n(), 0.5 * n()))
        .collect();

    let mut s = 0x1234_CAFEu64;
    let dibits: Vec<u8> = (0..3000)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s & 3) as u8
        })
        .collect();
    let sig = impair(&modulate_iq(&dibits, SPS, BETA), 0.006, 0.9, 0.04, 3);

    let run = |mut recv: CqpskReceiver| -> f64 {
        let mut out = Vec::new();
        for &x in idle.iter().chain(sig.iter()) {
            if let Some(d) = recv.push(x) {
                out.push(d);
            }
        }
        assert!(
            out.len() > 1000,
            "too few symbols after long idle: {}",
            out.len()
        );
        let tail = &out[out.len() - 800..];
        best_ber(tail, &dibits)
    };

    let cma = run(CqpskReceiver::new(SPS, BETA));
    let dfe = run(CqpskReceiver::new_dfe(SPS, BETA));
    eprintln!("long-idle → onset: CMA BER = {cma:.4}, DFE BER = {dfe:.4}");
    assert!(
        cma < 0.05,
        "CMA did not decode after a long idle: BER {cma:.4}"
    );
    assert!(
        dfe < 0.05,
        "DFE did not decode after a long idle: BER {dfe:.4}"
    );
}

/// Quantify how the wired (symbol-spaced) equalizers handle a *fractional*-delay
/// echo — the actual simulcast regime (0.12–0.34 T), which the integer-symbol
/// tests above never exercised. Sweeps echo delay across the regime and prints
/// bare vs CMA vs DFE BER. The FSE work (M5) exists because a symbol-spaced
/// equalizer sampling at the main path's timing cannot cleanly invert a
/// sub-symbol echo; this test is the measurement that motivates and later gates
/// that work.
#[test]
fn fractional_delay_echo_measures_the_symbol_spaced_limit() {
    let mut s = 0xFACE_0FF0u64;
    let dibits: Vec<u8> = (0..4000)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s & 3) as u8
        })
        .collect();
    let iq = modulate_iq(&dibits, SPS, BETA);

    let run = |rx: &[C32], mut recv: CqpskReceiver| -> f64 {
        let mut out = Vec::new();
        for &x in rx {
            if let Some(d) = recv.push(x) {
                out.push(d);
            }
        }
        let tail = &out[out.len().saturating_sub(1500)..];
        best_ber(tail, &dibits)
    };

    eprintln!("fractional-delay echo (theta pi/4):");
    let (mut t_bare, mut t_cma) = (1.0f64, 1.0f64);
    for gain in [0.6f32, 0.9] {
        for delay_t in [0.0f32, 0.15, 0.25, 0.35, 0.5, 1.0] {
            let isi = two_ray_iq_frac(&iq, gain, std::f32::consts::FRAC_PI_4, delay_t);
            let rx = impair(&isi, 0.006, 0.9, 0.04, 5);
            let bare = run(&rx, CqpskReceiver::new_bare(SPS, BETA));
            let cma = run(&rx, CqpskReceiver::new(SPS, BETA));
            let dfe = run(&rx, CqpskReceiver::new_dfe(SPS, BETA));
            eprintln!(
                "  gain {gain:.1} delay {delay_t:.2} T: bare {bare:.4}  CMA {cma:.4}  DFE {dfe:.4}"
            );
            if gain == 0.6 && delay_t == 1.0 {
                t_bare = bare;
                t_cma = cma;
            }
        }
    }
    // The one-symbol echo is where the thesis bites: the equalized path recovers
    // what the detect-first path loses outright (measured: bare fails to acquire
    // at 1.0 T / 0.6, CMA decodes perfectly). Fractional delays below ~0.5 T are
    // absorbed by the RRC matched filter and do not distinguish the paths.
    assert!(
        t_cma < t_bare * 0.5,
        "CMA did not beat bare at 1.0 T / 0.6: {t_cma:.4} vs {t_bare:.4}"
    );
}

/// The DFE's slow feedforward step (0.001) cannot open the eye inside a short
/// voice burst, so the blind acquisition's coherence threshold is never met and
/// the receiver decodes nothing (out.len = 0, never acquires). The gear-shifted
/// step — fast feedforward during acquisition, slow after — must acquire and
/// decode a short burst on the same 1.0 T / 0.6 echo that defeats the
/// detect-first path.
#[test]
fn dfe_acquires_on_short_burst() {
    let mut s = 0xFACE_0FF0u64;
    let dibits: Vec<u8> = (0..4000)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s & 3) as u8
        })
        .collect();
    let iq = modulate_iq(&dibits, SPS, BETA);
    let isi = two_ray_iq(&iq, 0.6, std::f32::consts::FRAC_PI_4, SPS);
    let rx = impair(&isi, 0.006, 0.9, 0.04, 5);

    let mut recv = CqpskReceiver::new_dfe(SPS, BETA);
    let mut out = Vec::new();
    for &x in &rx {
        if let Some(d) = recv.push(x) {
            out.push(d);
        }
    }

    assert!(recv.acquired(), "DFE never acquired on the short burst");
    assert!(
        out.len() > 2000,
        "DFE decoded too few symbols: {}",
        out.len()
    );
    let tail = &out[out.len() - 1000..];
    let e = best_ber(tail, &dibits);
    eprintln!("DFE short-burst tail BER = {e:.4}");
    assert!(e < 0.05, "DFE did not decode the short burst: BER {e:.4}");
}
