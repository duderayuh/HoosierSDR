//! Root-raised-cosine pulse shaping taps (Proakis, *Digital Communications*).
//!
//! Note: real P25 C4FM transmit shaping is raised-cosine plus an inverse-sinc
//! compensator. RRC at both ends composes to the same Nyquist RC response and
//! is what we use for the internal modulator/demodulator pair; matched-filter
//! refinement against real transmitters is Phase 1 tuning work.

/// RRC taps: `sps` samples/symbol, `span` symbols each side, rolloff `beta`.
pub fn rrc_taps(sps: usize, span: usize, beta: f64) -> Vec<f32> {
    let n = 2 * span * sps + 1;
    let mut taps = Vec::with_capacity(n);
    for i in 0..n {
        let t = (i as f64 - (n - 1) as f64 / 2.0) / sps as f64; // in symbols
        taps.push(rrc_impulse(t, beta));
    }
    // Normalize to unit DC gain.
    let sum: f64 = taps.iter().sum();
    taps.into_iter().map(|t| (t / sum) as f32).collect()
}

fn rrc_impulse(t: f64, beta: f64) -> f64 {
    use core::f64::consts::PI;
    if t == 0.0 {
        return 1.0 - beta + 4.0 * beta / PI;
    }
    let denom_zero = (4.0 * beta * t).abs() - 1.0;
    if denom_zero.abs() < 1e-9 {
        // Singularity at t = ±1/(4β)
        return beta / 2f64.sqrt()
            * ((1.0 + 2.0 / PI) * (PI / (4.0 * beta)).sin()
                + (1.0 - 2.0 / PI) * (PI / (4.0 * beta)).cos());
    }
    ((PI * t * (1.0 - beta)).sin() + 4.0 * beta * t * (PI * t * (1.0 + beta)).cos())
        / (PI * t * (1.0 - (4.0 * beta * t).powi(2)))
}
