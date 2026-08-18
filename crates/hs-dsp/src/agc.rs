//! Receiver conditioning: DC removal and automatic gain control.
//!
//! Both exist because real tuner output is not the clean unit-power baseband
//! the bench signals are. An RTL-SDR parks a DC spur at the tuned frequency
//! (I/Q offset in the quadrature mixer), which lands exactly on the P25
//! carrier; and its absolute output level depends on gain setting, antenna and
//! path loss. The CMA equalizer downstream drives its output onto a circle of
//! *fixed* radius, so an input scaled by an unknown constant makes the
//! constant-modulus error meaningless — CMA must be fed normalized samples.

use crate::C32;

/// Single-pole complex DC blocker: y[n] = x[n] − x[n−1] + a·y[n−1].
///
/// `a` near 1 puts the notch tight around DC, so it removes the tuner's DC
/// spur without eating the modulation, which for P25 has no DC content of its
/// own worth preserving.
pub struct DcBlocker {
    a: f32,
    x1: C32,
    y1: C32,
}

impl DcBlocker {
    pub fn new(a: f32) -> Self {
        Self {
            a,
            x1: C32::ZERO,
            y1: C32::ZERO,
        }
    }

    pub fn push(&mut self, x: C32) -> C32 {
        let y = x - self.x1 + self.y1.scale(self.a);
        self.x1 = x;
        self.y1 = y;
        y
    }
}

impl Default for DcBlocker {
    fn default() -> Self {
        Self::new(0.999)
    }
}

/// Complex AGC tracking mean power to a target, with a slow single-pole
/// estimator so it follows path loss without flattening the modulation.
pub struct Agc {
    /// EWMA of |x|².
    power: f32,
    alpha: f32,
    target: f32,
    gain: f32,
    /// Samples seen, used to widen the estimator's effective window during
    /// warm-up (see `push`).
    n: u64,
}

impl Agc {
    /// `alpha` is the power-estimator smoothing per sample (smaller = slower);
    /// `target` is the wanted mean |y|².
    pub fn new(alpha: f32, target: f32) -> Self {
        Self {
            power: 0.0,
            alpha,
            target,
            gain: 1.0,
            n: 0,
        }
    }

    pub fn push(&mut self, x: C32) -> C32 {
        // Warm-up: average over a growing window (1/n) until that becomes
        // slower than the steady-state EWMA. Seeding the estimator from one
        // sample instead would let a near-zero first sample — the ordinary
        // case, since a pulse-shaping filter starts filled with zeros — ask
        // for a gain of thousands, and that step propagates into the timing
        // loop as a huge error term.
        self.n = self.n.saturating_add(1);
        let a = self.alpha.max(1.0 / self.n as f32);
        self.power += a * (x.norm_sq() - self.power);
        if self.power > 1e-12 {
            // Clamp so a dead-air gap cannot wind the gain up into noise.
            self.gain = (self.target / self.power).sqrt().clamp(1e-3, 1e3);
        }
        x.scale(self.gain)
    }

    /// Current linear gain (diagnostic).
    pub fn gain(&self) -> f32 {
        self.gain
    }
}

impl Default for Agc {
    fn default() -> Self {
        Self::new(1e-3, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dc_blocker_removes_a_constant_offset() {
        let mut dc = DcBlocker::default();
        let offset = C32::new(0.3, -0.2);
        let mut last = C32::ZERO;
        for i in 0..20_000 {
            let p = 2.0 * std::f32::consts::PI * 0.05 * i as f32;
            last = dc.push(C32::new(p.cos(), p.sin()) + offset);
        }
        // Run a further window and confirm the mean has collapsed to ~0.
        let mut sum = C32::ZERO;
        let n = 4_000;
        for i in 20_000..20_000 + n {
            let p = 2.0 * std::f32::consts::PI * 0.05 * i as f32;
            sum = sum + dc.push(C32::new(p.cos(), p.sin()) + offset);
        }
        let mean = sum.scale(1.0 / n as f32);
        assert!(
            mean.norm_sq() < 1e-3,
            "residual DC {mean:?} (last {last:?})"
        );
    }

    #[test]
    fn agc_normalizes_an_arbitrary_input_level() {
        for level in [0.01f32, 0.6, 40.0] {
            let mut agc = Agc::default();
            let mut mean = 0.0f32;
            let n = 60_000;
            for i in 0..n {
                let p = 2.0 * std::f32::consts::PI * 0.05 * i as f32;
                let y = agc.push(C32::new(p.cos() * level, p.sin() * level));
                if i >= n / 2 {
                    mean += y.norm_sq();
                }
            }
            mean /= (n / 2) as f32;
            assert!((mean - 1.0).abs() < 0.05, "level {level} → power {mean:.3}");
        }
    }
}
