//! Synthetic channel impairments for the benchmark: additive noise and a
//! two-ray simulcast echo. Used to build reproducible test IQ when no field
//! corpus is present, and to stress the equalizer's ISI headroom.

use hs_dsp::C32;

/// Simple xorshift RNG — deterministic, no external deps, so bench runs are
/// reproducible across machines and CI.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    /// Standard-normal-ish via sum of uniforms (Irwin–Hall, n=6).
    pub fn gaussian(&mut self) -> f32 {
        let mut s = 0.0f32;
        for _ in 0..6 {
            s += (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32;
        }
        (s - 3.0) / 1.732
    }
}

/// Apply a two-ray channel (main + delayed echo) then add complex AWGN at
/// the given Es/N0 in dB. `echo_delay` is in samples, `echo_gain` linear.
pub fn impair(iq: &[f32], echo_delay: usize, echo_gain: f32, esno_db: f32, seed: u64) -> Vec<f32> {
    let n = iq.len() / 2;
    let mut samples: Vec<C32> = (0..n).map(|i| C32::new(iq[2 * i], iq[2 * i + 1])).collect();

    if echo_gain != 0.0 && echo_delay > 0 {
        let orig = samples.clone();
        for i in echo_delay..n {
            samples[i] = samples[i] + orig[i - echo_delay].scale(echo_gain);
        }
    }

    // Noise power from Es/N0, assuming unit signal power.
    let noise_sigma = 10f32.powf(-esno_db / 20.0);
    let mut rng = Rng::new(seed);
    let mut out = Vec::with_capacity(iq.len());
    for s in samples {
        let ns = C32::new(
            s.re + noise_sigma * rng.gaussian(),
            s.im + noise_sigma * rng.gaussian(),
        );
        out.push(ns.re);
        out.push(ns.im);
    }
    out
}
