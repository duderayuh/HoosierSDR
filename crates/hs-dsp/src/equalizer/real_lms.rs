//! Symbol-spaced real LMS equalizer for the C4FM (discriminator) path,
//! trained on the known Frame Sync Word symbol levels.
//!
//! This is the v1 realization of the project thesis for C4FM: adapt on the
//! 24 FSW symbols every frame, then filter the soft symbols *before* the
//! slicer. T/2 fractional spacing is the planned upgrade (see `LmsFse` for
//! the complex CQPSK variant).

pub struct RealLmsEq {
    taps: Vec<f32>,
    delay: Vec<f32>,
    pos: usize,
    pub mu: f32,
    pub error_var: f32,
    pub trained: bool,
}

impl RealLmsEq {
    pub fn new(num_taps: usize, mu: f32) -> Self {
        assert!(num_taps >= 3 && num_taps % 2 == 1);
        let mut taps = vec![0.0; num_taps];
        taps[num_taps / 2] = 1.0;
        Self {
            taps,
            delay: vec![0.0; num_taps],
            pos: 0,
            mu,
            error_var: 1.0,
            trained: false,
        }
    }

    /// Group delay in symbols (center-tap reference).
    pub fn delay_syms(&self) -> usize {
        self.taps.len() / 2
    }

    pub fn push(&mut self, x: f32) -> f32 {
        self.pos = (self.pos + 1) % self.delay.len();
        self.delay[self.pos] = x;
        self.output()
    }

    fn output(&self) -> f32 {
        let n = self.taps.len();
        let mut acc = 0.0;
        for k in 0..n {
            acc += self.taps[k] * self.delay[(self.pos + n - k) % n];
        }
        acc
    }

    /// One LMS training step against a known symbol level (±1, ±3).
    pub fn train(&mut self, desired: f32) -> f32 {
        let y = self.output();
        let e = desired - y;
        let n = self.taps.len();
        for k in 0..n {
            self.taps[k] += self.mu * e * self.delay[(self.pos + n - k) % n];
        }
        self.error_var = 0.95 * self.error_var + 0.05 * e * e;
        self.trained = true;
        e
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_symbol_isi() {
        // Channel: s[n] + 0.35 s[n-1] (post-discriminator simulcast echo).
        let mut eq = RealLmsEq::new(7, 0.01);
        let seq: Vec<f32> = (0..5000)
            .map(|i| [3.0, 1.0, -1.0, -3.0][(i * 5 + i / 7) % 4])
            .collect();
        let mut prev = 0.0f32;
        for &s in &seq {
            let rx = s + 0.35 * prev;
            prev = s;
            eq.push(rx);
        }
        // Re-run with training, offset by group delay.
        let d = eq.delay_syms();
        let mut prev = 0.0f32;
        for (i, &s) in seq.iter().enumerate() {
            let rx = s + 0.35 * prev;
            prev = s;
            eq.push(rx);
            if i >= d {
                eq.train(seq[i - d]);
            }
        }
        assert!(eq.error_var < 0.1, "error_var={}", eq.error_var);
    }
}
