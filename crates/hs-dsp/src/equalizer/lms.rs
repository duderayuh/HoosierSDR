//! Sync-trained T/2 fractionally-spaced LMS equalizer.
//!
//! Implemented from Haykin, *Adaptive Filter Theory* (LMS update) and
//! Proakis, *Digital Communications* (fractionally-spaced structure).

use crate::C32;

/// T/2 fractionally-spaced feed-forward equalizer with complex LMS adaptation.
///
/// Operates on samples at 2× symbol rate; produces one output per symbol
/// (taking every second input as the decision instant). Train on the known
/// Frame Sync Word symbols, then freeze; retrain when error variance rises.
pub struct LmsFse {
    taps: Vec<C32>,
    delay: Vec<C32>,
    pos: usize,
    /// LMS step size. Small (1e-3..1e-2 normalized) for stability.
    pub mu: f32,
    /// Running error variance estimate (EWMA of |e|^2) for retrain decisions.
    pub error_var: f32,
}

impl LmsFse {
    /// `num_taps` at T/2 spacing — 12 taps spans 6 symbols, sufficient for
    /// simulcast delay spreads of 0.12–0.34 T.
    pub fn new(num_taps: usize, mu: f32) -> Self {
        assert!(num_taps >= 3);
        let mut taps = vec![C32::ZERO; num_taps];
        // Center-spike initialization: identity filter.
        taps[num_taps / 2] = C32::new(1.0, 0.0);
        Self {
            taps,
            delay: vec![C32::ZERO; num_taps],
            pos: 0,
            mu,
            error_var: 0.0,
        }
    }

    /// Push one T/2-spaced input sample into the delay line.
    pub fn push(&mut self, x: C32) {
        self.pos = (self.pos + 1) % self.delay.len();
        self.delay[self.pos] = x;
    }

    /// Current filter output y[n] = w^H x[n].
    pub fn output(&self) -> C32 {
        let n = self.taps.len();
        let mut acc = C32::ZERO;
        for k in 0..n {
            let x = self.delay[(self.pos + n - k) % n];
            acc = acc + self.taps[k].conj() * x;
        }
        acc
    }

    /// Training step against a known reference symbol (e.g. an FSW symbol).
    /// Returns the a-priori error. LMS update: w += mu * x * e^*.
    pub fn train(&mut self, desired: C32) -> C32 {
        let y = self.output();
        let e = desired - y;
        let n = self.taps.len();
        for k in 0..n {
            let x = self.delay[(self.pos + n - k) % n];
            self.taps[k] = self.taps[k] + (x * e.conj()).scale(self.mu);
        }
        self.error_var = 0.99 * self.error_var + 0.01 * e.norm_sq();
        e
    }

    pub fn taps(&self) -> &[C32] {
        &self.taps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The equalizer must invert a simple two-ray (ISI) channel when trained
    /// on a known sequence — the minimal version of the simulcast problem.
    #[test]
    fn converges_on_two_ray_channel() {
        let mut eq = LmsFse::new(11, 0.01);
        // Deterministic QPSK-ish training sequence.
        let syms: Vec<C32> = (0..4000)
            .map(|i| {
                let (a, b) = match (i * 7 + i / 3) % 4 {
                    0 => (1.0, 1.0),
                    1 => (-1.0, 1.0),
                    2 => (-1.0, -1.0),
                    _ => (1.0, -1.0),
                };
                C32::new(a, b).scale(core::f32::consts::FRAC_1_SQRT_2)
            })
            .collect();

        // Two-ray channel at T/2: main tap + 0.4 echo one half-symbol later.
        let mut prev = C32::ZERO;
        let mut last_err = 1.0f32;
        for (i, &s) in syms.iter().enumerate() {
            // Upsample 2×: symbol sample then zero-ish midpoint.
            for half in 0..2 {
                let clean = if half == 0 { s } else { C32::ZERO };
                let rx = clean + prev.scale(0.4);
                prev = clean;
                eq.push(rx);
            }
            let e = eq.train(s);
            if i > 3500 {
                last_err = last_err.min(e.norm_sq());
            }
        }
        assert!(
            eq.error_var < 0.05,
            "LMS FSE failed to converge: error_var={}",
            eq.error_var
        );
        assert!(last_err < 0.05);
    }
}
