//! Front-end decimation: bring a native SDR capture rate down to the
//! decoder's working rate.
//!
//! An RTL-SDR cannot sample at 48 kHz — its practical floor is ~230 kHz, and
//! the usual P25 capture rate is 240 kHz (50 samples/symbol). The demodulators
//! downstream are tuned for ~10 samples/symbol: their matched filters, Gardner
//! loop bandwidths and equalizer step sizes are all expressed per symbol, so
//! feeding them a 5× oversampled stream detunes every one of those loops at
//! once. This stage sits at the very front and resamples by an integer factor
//! chosen so the working rate lands near 10 samples/symbol, with a
//! windowed-sinc anti-alias filter that also serves as the channel filter —
//! it rejects the adjacent P25 channels that would otherwise fold into the
//! passband.

use crate::fir::{lowpass_taps, FirC};
use crate::{C32, P25_SYMBOL_RATE};

/// Passband half-width to preserve, in Hz. A 12.5 kHz P25 channel is
/// comfortably inside ±8 kHz for both C4FM (frequency deviation ±1.8 kHz plus
/// shaping skirts) and CQPSK (β=0.2 → ±2.88 kHz).
const PASSBAND_HZ: f64 = 8_000.0;

/// Samples/symbol the demodulators are tuned for. Decimation targets the
/// largest integer factor that keeps the working rate at or above this.
pub const TARGET_SPS: usize = 10;

/// How a capture rate is reduced to the decoder's working rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DecimationPlan {
    /// Integer decimation factor (1 = pass through untouched).
    pub factor: usize,
    /// Rate the demodulator actually sees.
    pub working_rate: f64,
    /// Samples per symbol at the working rate.
    pub sps: usize,
}

impl DecimationPlan {
    /// Choose a decimation factor for `sample_rate`, keeping at least
    /// `min_sps` samples/symbol at the output.
    ///
    /// `sample_rate` must be an integer multiple of the 4800-baud symbol rate
    /// (240 kHz, 48 kHz, 960 kHz … all are). The factor is the largest
    /// divisor of the total oversampling that still leaves `min_sps`, so the
    /// working rate stays an exact multiple of the symbol rate and no
    /// fractional resampler is needed.
    pub fn for_rate(sample_rate: f64, min_sps: usize) -> Self {
        let total = sample_rate / P25_SYMBOL_RATE;
        assert!(
            (total.fract()).abs() < 1e-9,
            "sample rate {sample_rate} is not a multiple of {P25_SYMBOL_RATE} baud"
        );
        let total = total.round() as usize;
        assert!(
            total >= min_sps.min(4),
            "sample rate {sample_rate} gives only {total} samples/symbol"
        );
        let mut factor = 1;
        for d in 1..=total {
            if total.is_multiple_of(d) && total / d >= min_sps {
                factor = factor.max(d);
            }
        }
        Self {
            factor,
            working_rate: sample_rate / factor as f64,
            sps: total / factor,
        }
    }
}

/// Integer-factor complex decimator with an anti-alias / channel filter, and
/// an optional digital downconverter in front of it.
///
/// The downconverter is what makes one wideband capture useful for more than
/// one channel. An RTL-SDR at 240 kHz sees nineteen 12.5 kHz P25 channels at
/// once; mixing the wanted one to DC before the channel filter selects it,
/// and means a capture that was tuned slightly off — a mis-identified control
/// channel, a neighbouring site — is still decodable without re-recording.
pub struct Decimator {
    /// Per-sample NCO rotation, or None when tuned to DC.
    step: Option<C32>,
    nco: C32,
    n: u32,
    fir: Option<FirC>,
    plan: DecimationPlan,
}

impl Decimator {
    /// Decimate only, keeping the channel already at DC.
    pub fn new(sample_rate: f64, min_sps: usize) -> Self {
        Self::with_offset(sample_rate, min_sps, 0.0)
    }

    /// Mix the channel `offset_hz` away from the capture centre down to DC,
    /// then decimate.
    pub fn with_offset(sample_rate: f64, min_sps: usize, offset_hz: f64) -> Self {
        let plan = DecimationPlan::for_rate(sample_rate, min_sps);
        let mut d = Self::from_plan(sample_rate, plan);
        if offset_hz != 0.0 {
            let w = -2.0 * core::f64::consts::PI * offset_hz / sample_rate;
            d.step = Some(C32::new(w.cos() as f32, w.sin() as f32));
        }
        d
    }

    fn from_plan(sample_rate: f64, plan: DecimationPlan) -> Self {
        let fir = if plan.factor == 1 {
            None
        } else {
            // Passband to PASSBAND_HZ, stopband from the output Nyquist so
            // nothing can fold in. Hamming needs ~3.3/Δf normalized taps to
            // resolve that transition.
            let cutoff = PASSBAND_HZ / sample_rate;
            let stop = plan.working_rate / 2.0 / sample_rate;
            let transition = (stop - cutoff).max(1e-3);
            let mut n = (3.3 / transition).ceil() as usize;
            n = n.clamp(31, 4095);
            if n.is_multiple_of(2) {
                n += 1;
            }
            // Design at the passband edge plus half the transition, which puts
            // the −6 dB point in the middle of the band we don't care about.
            let design = cutoff + transition / 2.0;
            Some(FirC::new(lowpass_taps(n, design), plan.factor))
        };
        Self {
            step: None,
            nco: C32::new(1.0, 0.0),
            n: 0,
            fir,
            plan,
        }
    }

    pub fn plan(&self) -> DecimationPlan {
        self.plan
    }

    /// Push one input sample; returns a sample at the working rate on
    /// decimation instants.
    pub fn push(&mut self, x: C32) -> Option<C32> {
        let x = match self.step {
            Some(step) => {
                let y = x * self.nco;
                self.nco = self.nco * step;
                // Repeated complex multiplication bleeds magnitude, which over
                // a multi-minute capture would show up as a slow gain ramp.
                // Renormalize periodically rather than calling sin/cos per
                // sample.
                self.n += 1;
                if self.n >= 1024 {
                    self.n = 0;
                    let m = self.nco.norm_sq().sqrt();
                    if m > 1e-6 {
                        self.nco = self.nco.scale(1.0 / m);
                    }
                }
                y
            }
            None => x,
        };
        match self.fir.as_mut() {
            Some(f) => f.push(x),
            None => Some(x),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_a_sane_factor_for_common_capture_rates() {
        // The RTL-SDR rate this project captures at.
        let p = DecimationPlan::for_rate(240_000.0, TARGET_SPS);
        assert_eq!(p.factor, 5);
        assert_eq!(p.working_rate, 48_000.0);
        assert_eq!(p.sps, 10);

        // Already at the working rate → pass through.
        let p = DecimationPlan::for_rate(48_000.0, TARGET_SPS);
        assert_eq!(p.factor, 1);
        assert_eq!(p.sps, 10);

        // A high rate still lands at or above the target.
        let p = DecimationPlan::for_rate(960_000.0, TARGET_SPS);
        assert!(p.sps >= TARGET_SPS, "sps {}", p.sps);
        assert_eq!(p.working_rate * p.factor as f64, 960_000.0);
    }

    #[test]
    fn rejects_an_adjacent_channel_that_would_fold_in() {
        // A tone 50 kHz off center (the neighbouring active channel in the
        // Marion County capture) aliases to +2 kHz — dead centre of the
        // passband — if it is not filtered before decimating by 5.
        let fs = 240_000.0;
        let mut dec = Decimator::new(fs, TARGET_SPS);
        let mut wanted = Decimator::new(fs, TARGET_SPS);
        let (mut alias_pow, mut want_pow) = (0.0f64, 0.0f64);
        let n = 40_000;
        for i in 0..n {
            let t = i as f64 / fs;
            let a = 2.0 * std::f64::consts::PI * 50_000.0 * t;
            let w = 2.0 * std::f64::consts::PI * 2_000.0 * t;
            if let Some(y) = dec.push(C32::new(a.cos() as f32, a.sin() as f32)) {
                alias_pow += y.norm_sq() as f64;
            }
            if let Some(y) = wanted.push(C32::new(w.cos() as f32, w.sin() as f32)) {
                want_pow += y.norm_sq() as f64;
            }
        }
        let rejection_db = 10.0 * (want_pow / alias_pow.max(1e-30)).log10();
        assert!(
            rejection_db > 40.0,
            "adjacent-channel rejection only {rejection_db:.1} dB"
        );
    }

    #[test]
    fn downconverter_selects_an_offset_channel() {
        // Two tones: an unwanted one at DC and the wanted one 50 kHz up (the
        // situation in the Marion County capture, which was tuned one channel
        // low). Tuning to +50 kHz must keep the latter and reject the former.
        let fs = 240_000.0;
        let mut dec = Decimator::with_offset(fs, TARGET_SPS, 50_000.0);
        let mut wanted = 0.0f64;
        let mut rejected = 0.0f64;
        let n = 60_000;
        for i in 0..n {
            let t = i as f64 / fs;
            let w = 2.0 * std::f64::consts::PI * 50_000.0 * t;
            if let Some(y) = dec.push(C32::new(w.cos() as f32, w.sin() as f32)) {
                wanted += y.norm_sq() as f64;
            }
        }
        let mut dec = Decimator::with_offset(fs, TARGET_SPS, 50_000.0);
        for _ in 0..n {
            if let Some(y) = dec.push(C32::new(1.0, 0.0)) {
                rejected += y.norm_sq() as f64;
            }
        }
        let db = 10.0 * (wanted / rejected.max(1e-30)).log10();
        assert!(db > 40.0, "offset tuning rejection only {db:.1} dB");
    }

    #[test]
    fn passes_an_in_band_signal_through_intact() {
        let fs = 240_000.0;
        let mut dec = Decimator::new(fs, TARGET_SPS);
        let mut out = Vec::new();
        let n = 20_000;
        for i in 0..n {
            let t = i as f64 / fs;
            let p = 2.0 * std::f64::consts::PI * 3_000.0 * t;
            if let Some(y) = dec.push(C32::new(p.cos() as f32, p.sin() as f32)) {
                out.push(y);
            }
        }
        assert_eq!(out.len(), n / 5);
        // Skip the filter's fill-in transient, then check unit gain.
        let tail = &out[out.len() / 2..];
        let mean: f32 = tail.iter().map(|y| y.norm_sq()).sum::<f32>() / tail.len() as f32;
        assert!((mean - 1.0).abs() < 0.05, "passband gain {mean:.3}");
    }
}
