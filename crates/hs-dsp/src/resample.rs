//! Polyphase rational resampling: convert by a ratio `up/down` in one pass.
//!
//! The front-end decimator can bring any rate that is an integer multiple of
//! 4800 baud down to the working rate with plain integer decimation. Real
//! hardware is not always so obliging: an Airspy R2 samples at 10 or
//! 2.5 MSPS, neither of which divides by 4800. This stage closes that gap —
//! conceptually insert `up−1` zeros between samples, lowpass at the design
//! rate `input_rate × up`, and keep every `down`-th result. The polyphase
//! form never materializes the zeros: each output is one dot product over
//! `taps/up` history samples with the tap phase selected by the output's
//! position, so the cost is per *output* sample, not per upsampled tick.

use crate::fir::lowpass_taps;
use crate::C32;

pub struct RationalResampler {
    up: usize,
    down: usize,
    /// Lowpass at the design rate, scaled by `up` (zero-insertion loses a
    /// factor of `up` in level), zero-padded to a multiple of `up`.
    taps: Vec<f32>,
    /// Ring buffer of the most recent inputs, indexed by absolute count.
    hist: Vec<C32>,
    /// Taps per polyphase branch = history samples per output.
    per_phase: usize,
    /// Inputs consumed so far; the newest sample has index `n_in − 1`.
    n_in: u64,
    /// Index of the next output to produce.
    k_out: u64,
}

impl RationalResampler {
    /// Resample by `up/down` (`down ≥ up`: this stage only ever reduces the
    /// rate). `passband_hz` is preserved; everything above the output
    /// Nyquist is stopband so nothing folds in.
    pub fn new(input_rate: f64, up: usize, down: usize, passband_hz: f64) -> Self {
        assert!(up >= 1 && down > up, "resampler only reduces rate");
        let design_rate = input_rate * up as f64;
        let out_rate = design_rate / down as f64;
        let cutoff = passband_hz / design_rate;
        let stop = out_rate / 2.0 / design_rate;
        let transition = (stop - cutoff).max(1e-4);
        let mut n = (3.3 / transition).ceil() as usize;
        n = n.clamp(31, 4095);
        if n.is_multiple_of(2) {
            n += 1;
        }
        let design = cutoff + transition / 2.0;
        let mut taps: Vec<f32> = lowpass_taps(n, design)
            .into_iter()
            .map(|t| t * up as f32)
            .collect();
        while !taps.len().is_multiple_of(up) {
            taps.push(0.0);
        }
        let per_phase = taps.len() / up;
        Self {
            up,
            down,
            taps,
            // One slot of slack: an output's base sample can lag the newest
            // input by one, so `per_phase + 1` distinct samples are live.
            // Power-of-two length so indexing is a mask, not a division —
            // this runs per sample at 10 MSPS.
            hist: vec![C32::ZERO; (per_phase + 2).next_power_of_two()],
            per_phase,
            n_in: 0,
            k_out: 0,
        }
    }

    /// Push one input sample; returns a resampled output when one is due.
    /// With `down ≥ up` at most one output falls between consecutive inputs.
    pub fn push(&mut self, x: C32) -> Option<C32> {
        let mask = (self.hist.len() - 1) as u64;
        self.hist[(self.n_in & mask) as usize] = x;
        self.n_in += 1;
        let newest = self.n_in - 1;

        // Output k lives at upsampled tick k·down; it is computable once the
        // input covering that tick (index ⌊k·down/up⌋) has arrived.
        let tick = self.k_out * self.down as u64;
        if tick > newest * self.up as u64 {
            return None;
        }
        let phase = (tick % self.up as u64) as usize;
        let base = tick / self.up as u64;
        let mut acc = C32::ZERO;
        // Taps before the first sample read zeros; bound the loop instead of
        // checking inside it.
        let live = (base + 1).min(self.per_phase as u64) as usize;
        for i in 0..live {
            let t = self.taps[phase + i * self.up];
            if t == 0.0 {
                continue;
            }
            acc = acc + self.hist[((base - i as u64) & mask) as usize].scale(t);
        }
        self.k_out += 1;
        Some(acc)
    }
}

/// The `up/down` ratio and resulting rate that turns `rate` into the nearest
/// convenient multiple of the 4800-baud symbol rate — or `None` if `rate` is
/// already such a multiple and nothing downstream needs resampling.
///
/// Every rate the decoder handles natively (240 k, 2.4 M, 48 k …) divides
/// evenly by 4800. Real hardware sometimes doesn't: an Airspy R2 samples at
/// 10 or 2.5 MSPS, both of which are 25/24 of a clean rate (9.6 M, 2.4 M),
/// so the same gentle 24/25 resample normalizes either. Rates that are
/// already clean return `None`; oddball rates fall back to reducing toward
/// the nearest lower 4800-multiple.
pub fn normalize_ratio(rate: f64) -> Option<(usize, usize, f64)> {
    const BAUD: f64 = crate::P25_SYMBOL_RATE;
    if (rate / BAUD).fract().abs() < 1e-6 {
        return None;
    }
    // The Airspy R2's rates are exactly 25/24 of a 4800-multiple; recognize
    // that common case and normalize with the matching small ratio.
    for (native, up, down, out) in [
        (10_000_000.0, 24usize, 25usize, 9_600_000.0),
        (2_500_000.0, 24, 25, 2_400_000.0),
    ] {
        if (rate - native).abs() < 1.0 {
            return Some((up, down, out));
        }
    }
    // General fallback: reduce to the nearest lower multiple of 4800 with a
    // small rational approximation of the ratio.
    let target = (rate / BAUD).floor() * BAUD;
    let g = gcd(target.round() as u64, rate.round() as u64);
    let up = (target.round() as u64 / g) as usize;
    let down = (rate.round() as u64 / g) as usize;
    Some((up.max(1), down, target))
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 {
        a.max(1)
    } else {
        gcd(b, a % b)
    }
}

/// Resample an interleaved-f32 IQ buffer by `up/down`, preserving a passband
/// of `0.8 ×` the output Nyquist — wide enough to keep every channel in a
/// normalized wideband capture. One-shot, for normalizing a file at load.
pub fn resample_iq(iq: &[f32], up: usize, down: usize, input_rate: f64) -> Vec<f32> {
    let out_rate = input_rate * up as f64 / down as f64;
    let passband = 0.4 * out_rate; // 0.8 of the output Nyquist
    let mut rs = RationalResampler::new(input_rate, up, down, passband);
    let mut out = Vec::with_capacity(iq.len() * up / down + 4);
    for s in iq.chunks_exact(2) {
        if let Some(y) = rs.push(C32::new(s[0], s[1])) {
            out.push(y.re);
            out.push(y.im);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 80 kHz → 48 kHz by 3/5: an in-band tone survives at unit gain and the
    /// output rate is exactly right.
    #[test]
    fn tone_passes_at_unit_gain_and_exact_rate() {
        let fs = 80_000.0;
        let mut rs = RationalResampler::new(fs, 3, 5, 8_000.0);
        let n = 80_000; // 1 s
        let mut out = Vec::new();
        for i in 0..n {
            let t = i as f64 / fs;
            let p = 2.0 * std::f64::consts::PI * 3_000.0 * t;
            if let Some(y) = rs.push(C32::new(p.cos() as f32, p.sin() as f32)) {
                out.push(y);
            }
        }
        assert_eq!(out.len(), 48_000);
        let tail = &out[out.len() / 2..];
        let mean: f32 = tail.iter().map(|y| y.norm_sq()).sum::<f32>() / tail.len() as f32;
        assert!((mean - 1.0).abs() < 0.05, "passband gain {mean:.3}");
    }

    /// A tone above the output Nyquist must not fold into the passband.
    #[test]
    fn rejects_what_would_alias() {
        let fs = 80_000.0;
        let mut rs = RationalResampler::new(fs, 3, 5, 8_000.0);
        let mut pow = 0.0f64;
        let mut count = 0usize;
        let n = 80_000;
        for i in 0..n {
            let t = i as f64 / fs;
            // 30 kHz: above the 24 kHz output Nyquist, would alias to 18 kHz.
            let p = 2.0 * std::f64::consts::PI * 30_000.0 * t;
            if let Some(y) = rs.push(C32::new(p.cos() as f32, p.sin() as f32)) {
                pow += y.norm_sq() as f64;
                count += 1;
            }
        }
        let db = 10.0 * (pow / count as f64).log10();
        assert!(db < -40.0, "alias leaked at {db:.1} dB");
    }

    #[test]
    fn normalize_ratio_handles_airspy_and_clean_rates() {
        // Airspy R2's two rates both normalize with the gentle 24/25 ratio.
        assert_eq!(normalize_ratio(10_000_000.0), Some((24, 25, 9_600_000.0)));
        assert_eq!(normalize_ratio(2_500_000.0), Some((24, 25, 2_400_000.0)));
        // Native rates are already multiples of 4800 → no resampling.
        assert_eq!(normalize_ratio(2_400_000.0), None);
        assert_eq!(normalize_ratio(240_000.0), None);
        assert_eq!(normalize_ratio(48_000.0), None);
        // Fallback: any output rate stays an exact multiple of the baud.
        for r in [1_234_567.0, 3_000_000.0, 5_000_000.0] {
            if let Some((up, down, out)) = normalize_ratio(r) {
                assert!(up >= 1 && down > up, "rate {r}: bad ratio {up}/{down}");
                assert!(
                    (out / crate::P25_SYMBOL_RATE).fract().abs() < 1e-6,
                    "rate {r}: out {out} not a baud multiple"
                );
                assert!((out - r * up as f64 / down as f64).abs() < 1.0);
            }
        }
    }

    /// The resampled tone must keep its frequency: 3/5 of 80 kHz puts a
    /// 3 kHz tone at 3 kHz of 48 kHz — check the phase advance per sample.
    #[test]
    fn preserves_frequency() {
        let fs = 80_000.0;
        let mut rs = RationalResampler::new(fs, 3, 5, 8_000.0);
        let mut out = Vec::new();
        for i in 0..80_000 {
            let t = i as f64 / fs;
            let p = 2.0 * std::f64::consts::PI * 3_000.0 * t;
            if let Some(y) = rs.push(C32::new(p.cos() as f32, p.sin() as f32)) {
                out.push(y);
            }
        }
        let tail = &out[out.len() / 2..];
        let mut acc = 0.0f64;
        for w in tail.windows(2) {
            let d = w[1] * w[0].conj();
            acc += (d.im as f64).atan2(d.re as f64);
        }
        let hz = acc / (tail.len() - 1) as f64 * 48_000.0 / (2.0 * std::f64::consts::PI);
        assert!((hz - 3_000.0).abs() < 5.0, "tone moved to {hz:.1} Hz");
    }
}
