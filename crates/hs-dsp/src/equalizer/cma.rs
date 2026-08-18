//! Constant-Modulus (Godard) adaptive equalizer — phase-blind.
//!
//! P25 CQPSK symbols all have unit magnitude, so the constant-modulus
//! criterion J = E[(|y|² − R₂)²] applies: drive the equalizer output onto the
//! unit circle without any reference symbol or carrier lock. That phase-blind
//! property is exactly what lets the equalizer sit **before** differential
//! detection on the differential (non-coherent) CQPSK front end — the coherent
//! FSW-trained FSE (`LmsFse`) would need an absolute phase reference the
//! differential receiver never establishes, but CMA needs none. Any residual
//! constant rotation CMA leaves is removed by the differential detector that
//! follows.
//!
//! Implemented from Godard (1980) / Haykin, *Adaptive Filter Theory*.

use crate::C32;

/// Symbol-spaced complex CMA equalizer. `R2` is the constant-modulus target
/// E[|s|⁴]/E[|s|²]; for unit-magnitude CQPSK symbols it is 1.0.
pub struct CmaEqualizer {
    taps: Vec<C32>,
    delay: Vec<C32>,
    pos: usize,
    mu: f32,
    r2: f32,
    /// EWMA of the CMA error magnitude, a convergence/health signal.
    pub error_var: f32,
}

impl CmaEqualizer {
    pub fn new(num_taps: usize, mu: f32) -> Self {
        assert!(num_taps >= 3);
        let mut taps = vec![C32::ZERO; num_taps];
        // Center-spike init: identity, so an already-clean signal passes.
        taps[num_taps / 2] = C32::new(1.0, 0.0);
        Self {
            taps,
            delay: vec![C32::ZERO; num_taps],
            pos: 0,
            mu,
            r2: 1.0,
            error_var: 0.0,
        }
    }

    /// Reset to the center-spike identity, discarding any diverged taps.
    fn reset(&mut self) {
        for t in self.taps.iter_mut() {
            *t = C32::ZERO;
        }
        let mid = self.taps.len() / 2;
        self.taps[mid] = C32::new(1.0, 0.0);
        for d in self.delay.iter_mut() {
            *d = C32::ZERO;
        }
        self.error_var = 0.0;
    }

    fn output(&self) -> C32 {
        let n = self.taps.len();
        let mut acc = C32::ZERO;
        for k in 0..n {
            let x = self.delay[(self.pos + n - k) % n];
            acc = acc + self.taps[k].conj() * x;
        }
        acc
    }

    /// Push one sample, adapt the taps by the CMA rule, and return the
    /// equalized output. `adapt` gates the tap update so the caller can freeze
    /// the equalizer (e.g. before the timing loop has settled).
    pub fn push(&mut self, x: C32, adapt: bool) -> C32 {
        self.pos = (self.pos + 1) % self.delay.len();
        self.delay[self.pos] = x;
        let y = self.output();
        if adapt {
            // CMA (Godard p=2): w[k] += mu · (R₂ − |y|²) · y · conj(x[k]).
            //
            // Normalize the step by the delay-line energy (NLMS). The plain
            // rule diverges on the transient at the *start* of a transmission:
            // a traffic channel is idle until a call keys up, and when it does,
            // full-power samples arrive a few symbols before the AGC has
            // caught up. Fed those, |y|² briefly explodes, g = R₂ − |y|² swings
            // hugely negative, and the un-normalized tap update overshoots into
            // a runaway that lands on NaN — after which every output is NaN and
            // the whole receiver is dead for the rest of the capture (it never
            // re-acquires, because NaN > threshold is false). Dividing the step
            // by the input energy bounds it regardless of that transient, which
            // is exactly the case NLMS exists for. `1e-6` keeps idle noise from
            // dividing by zero.
            let energy: f32 = self.delay.iter().map(|c| c.norm_sq()).sum::<f32>() + 1e-6;
            let g = self.r2 - y.norm_sq();
            let ey = y.scale(g * self.mu / energy);
            let n = self.taps.len();
            for k in 0..n {
                let xk = self.delay[(self.pos + n - k) % n];
                // Gradient step in the same (conj-tap) convention as output().
                self.taps[k] = self.taps[k] + xk * ey.conj();
            }
            self.error_var = 0.99 * self.error_var + 0.01 * (g * g);

            // Belt and braces: if a pathological input still drove the taps
            // non-finite, reset to the identity spike rather than stay dead.
            if !self
                .taps
                .iter()
                .all(|t| t.re.is_finite() && t.im.is_finite())
            {
                self.reset();
            }
        }
        y
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cqpsk::modulate_symbols;

    #[test]
    fn opens_a_two_ray_eye_blindly() {
        // CQPSK symbols through a symbol-spaced complex two-ray channel. CMA
        // must open the eye (drive outputs to unit modulus) with NO reference
        // and NO carrier lock.
        let dibits: Vec<u8> = (0..6000).map(|i| ((i * 7 + i / 3) % 4) as u8).collect();
        let syms = modulate_symbols(&dibits);
        let echo = C32::new(0.45 * 0.7, 0.45 * 0.7); // 0.45∠45°
        let mut eq = CmaEqualizer::new(11, 0.003);

        let mut prev = C32::ZERO;
        let mut mod_err_early = 0.0f32;
        let mut mod_err_late = 0.0f32;
        for (i, &s) in syms.iter().enumerate() {
            let rx = s + echo * prev;
            prev = s;
            let y = eq.push(rx, true);
            let e = (y.norm_sq() - 1.0).abs();
            if i < 500 {
                mod_err_early += e;
            }
            if i >= syms.len() - 500 {
                mod_err_late += e;
            }
        }
        // Modulus error must shrink dramatically as CMA converges.
        assert!(
            mod_err_late < mod_err_early * 0.3,
            "CMA did not converge: early {mod_err_early:.1} late {mod_err_late:.1}"
        );
    }
}
