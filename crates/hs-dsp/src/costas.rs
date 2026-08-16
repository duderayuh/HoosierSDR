//! Decision-directed Costas loop for QPSK carrier recovery.
//!
//! Real off-air IQ arrives with a carrier frequency and phase offset the
//! tuner/oscillator couldn't remove. Differential detection tolerates a
//! static *phase* offset but not a *frequency* offset — a constant frequency
//! error biases every differential phase and rotates the whole constellation
//! into the wrong decision regions. The Costas loop estimates and removes
//! that rotation so the equalizer downstream sees a de-rotated constellation.
//!
//! Second-order loop (proportional + integral) so it tracks a frequency
//! offset to zero steady-state error. Implemented from Rice, *Digital
//! Communications: A Discrete-Time Approach*.

use crate::C32;
use core::f32::consts::PI;

pub struct CostasLoop {
    /// Current NCO phase (radians).
    phase: f32,
    /// Integrated frequency estimate (radians/sample).
    freq: f32,
    alpha: f32, // proportional gain
    beta: f32,  // integral gain
    max_freq: f32,
}

impl CostasLoop {
    /// `loop_bw` is the normalized loop bandwidth (e.g. 0.01). `max_freq` caps
    /// the frequency estimate (radians/sample) to keep the loop from running
    /// away on noise.
    pub fn new(loop_bw: f32, max_freq: f32) -> Self {
        // Standard 2nd-order gains for a given normalized bandwidth and
        // critical damping (zeta = 1/sqrt(2)).
        let zeta = 0.707f32;
        let denom = 1.0 + 2.0 * zeta * loop_bw + loop_bw * loop_bw;
        let alpha = 4.0 * zeta * loop_bw / denom;
        let beta = 4.0 * loop_bw * loop_bw / denom;
        Self {
            phase: 0.0,
            freq: 0.0,
            alpha,
            beta,
            max_freq,
        }
    }

    /// De-rotate one sample by the current NCO phase and advance the loop.
    /// Returns the corrected sample.
    pub fn process(&mut self, x: C32) -> C32 {
        // Rotate input by -phase.
        let nco = C32::new(self.phase.cos(), -self.phase.sin());
        let y = x * nco;

        // QPSK decision-directed phase error: the error is the imaginary part
        // of y times sign(re) minus real part times sign(im) — equivalently
        // the angle to the nearest π/2 constellation point.
        let err = phase_error_qpsk(y);

        // Second-order loop update.
        self.freq += self.beta * err;
        self.freq = self.freq.clamp(-self.max_freq, self.max_freq);
        self.phase += self.freq + self.alpha * err;
        // Wrap phase.
        while self.phase > PI {
            self.phase -= 2.0 * PI;
        }
        while self.phase < -PI {
            self.phase += 2.0 * PI;
        }
        y
    }

    /// Current frequency estimate (radians/sample).
    pub fn freq(&self) -> f32 {
        self.freq
    }
}

/// Decision-directed phase-error detector for QPSK: distance in angle from the
/// nearest π/2 grid point (the ±1±j constellation), sign-corrected.
fn phase_error_qpsk(y: C32) -> f32 {
    // Hard decision to nearest quadrant point (±1, ±1)/√2.
    let di = if y.re >= 0.0 { 1.0 } else { -1.0 };
    let dq = if y.im >= 0.0 { 1.0 } else { -1.0 };
    // Error = Im(y · conj(decision)), normalized. This is the classic
    // decision-directed QPSK detector.
    (y.im * di - y.re * dq) * 0.5
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locks_onto_frequency_offset() {
        // A rotating QPSK point should be de-rotated to a stationary
        // constellation once the loop locks.
        let f_off = 0.03f32; // radians/sample
        let mut loopf = CostasLoop::new(0.02, 0.2);
        let mut phase = 0.7f32;
        let sym = C32::new(1.0, 1.0).scale(core::f32::consts::FRAC_1_SQRT_2);
        let mut last = C32::ZERO;
        for _ in 0..3000 {
            let rot = C32::new(phase.cos(), phase.sin());
            last = loopf.process(sym * rot);
            phase += f_off;
        }
        // After lock, the recovered frequency should match the offset and the
        // output should sit near a constellation point (|im|≈|re|).
        assert!(
            (loopf.freq() - f_off).abs() < 0.005,
            "freq est {} vs {}",
            loopf.freq(),
            f_off
        );
        let mag = last.norm_sq().sqrt();
        assert!(mag > 0.5, "output collapsed");
    }
}
