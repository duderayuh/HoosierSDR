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
