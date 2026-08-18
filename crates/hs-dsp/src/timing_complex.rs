//! Complex Gardner symbol-timing recovery with an interpolating NCO.
//!
//! Recovers the symbol sampling instant from oversampled complex baseband
//! (post matched filter) without needing carrier lock first — Gardner's TED
//! e = Re{ conj(y_mid)·(y_k − y_{k−1}) } is carrier-phase independent, so it
//! pairs naturally ahead of the Costas loop. A modulo-1 NCO strobes twice per
//! symbol (symbol instant + midpoint); a PI loop filter steers the strobe to
//! the pulse peak. Structure from Rice, *Digital Communications: A
//! Discrete-Time Approach*, ch. 8.

use crate::C32;

pub struct ComplexGardner {
    /// Nominal samples per symbol.
    sps: f32,
    /// NCO phase accumulator; a strobe fires when it underflows past 0.
    nco: f32,
    /// Loop-controlled increment per input sample (~1/(sps/2) for half-symbol).
    w: f32,
    w0: f32,
    kp: f32,
    ki: f32,
    integ: f32,
    /// Previous input sample, for linear interpolation.
    prev: C32,
    have_prev: bool,
    /// true → next strobe is a symbol instant; false → a midpoint.
    on_symbol: bool,
    y_prev_sym: C32,
    y_mid: C32,
    seen_sym: bool,
}

impl ComplexGardner {
    pub fn new(samples_per_symbol: f32, loop_bw: f32) -> Self {
        // Half-symbol strobing: NCO increments by 1/(sps/2) per input sample.
        let w0 = 1.0 / (samples_per_symbol / 2.0);
        let zeta = 0.707f32;
        let denom = 1.0 + 2.0 * zeta * loop_bw + loop_bw * loop_bw;
        let kp = 4.0 * zeta * loop_bw / denom;
        let ki = 4.0 * loop_bw * loop_bw / denom;
        Self {
            sps: samples_per_symbol,
            nco: 0.0,
            w: w0,
            w0,
            kp,
            ki,
            integ: 0.0,
            prev: C32::ZERO,
            have_prev: false,
            on_symbol: false,
            y_prev_sym: C32::ZERO,
            y_mid: C32::ZERO,
            seen_sym: false,
        }
    }

    /// Push one oversampled complex sample; returns Some(symbol) at symbol
    /// strobes (once per symbol period, at the recovered instant).
    pub fn push(&mut self, x: C32) -> Option<C32> {
        if !self.have_prev {
            self.prev = x;
            self.have_prev = true;
            self.nco += self.w;
            return None;
        }
        let mut out = None;
        self.nco += self.w;
        if self.nco >= 1.0 {
            self.nco -= 1.0;
            // Fractional strobe instant between prev and x: mu in [0,1),
            // where the strobe sits mu of the way from prev toward x.
            let mu = self.nco / self.w;
            let y = self.prev + (x - self.prev).scale(1.0 - mu);
            if self.on_symbol {
                // Gardner TED using the midpoint between the two symbols. Skip
                // the very first symbol strobe (no valid previous symbol yet).
                if self.seen_sym {
                    let e = ((y - self.y_prev_sym) * self.y_mid.conj()).re;
                    self.integ += self.ki * e;
                    // Clamp the integrator so a transient can't run the rate away.
                    self.integ = self.integ.clamp(-0.5 * self.w0, 0.5 * self.w0);
                    self.w = self.w0 + self.kp * e + self.integ;
                    // Hard-limit the NCO rate. The proportional term is
                    // unbounded in the TED output, so one transient — an AGC
                    // step, a dropout, a saturated front end — can drive `w`
                    // negative or non-finite, after which the accumulator
                    // never reaches the strobe threshold again and the
                    // receiver goes silent for the rest of the capture with no
                    // error reported. Real tuner clock error is well under 1%,
                    // so ±20% of nominal is generous and makes that failure
                    // unreachable.
                    if !self.w.is_finite() {
                        self.integ = 0.0;
                        self.w = self.w0;
                    }
                    self.w = self.w.clamp(0.8 * self.w0, 1.2 * self.w0);
                }
                self.seen_sym = true;
                self.y_prev_sym = y;
                out = Some(y);
            } else {
                self.y_mid = y;
            }
            self.on_symbol = !self.on_symbol;
        }
        self.prev = x;
        out
    }

    /// Recovered samples-per-symbol estimate (diagnostic).
    pub fn est_sps(&self) -> f32 {
        2.0 / self.w
    }

    /// Nominal samples per symbol this loop was built for.
    pub fn nominal_sps(&self) -> f32 {
        self.sps
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cqpsk::{differential_detect, dphase_to_dibit, modulate_iq};
    use crate::fir::FirC;
    use crate::rrc::rrc_taps;

    #[test]
    fn recovers_symbols_at_pulse_peak() {
        // Full chain: RRC-shaped CQPSK → matched filter → Gardner. Recovered
        // symbols, differentially detected, must match the transmitted dibits.
        let sps = 10usize;
        let beta = 0.2;
        let dibits: Vec<u8> = (0..600).map(|i| ((i * 7 + i / 5) % 4) as u8).collect();
        let iq = modulate_iq(&dibits, sps, beta);

        let mut mf = FirC::new(rrc_taps(sps, 6, beta), 1);
        let mut g = ComplexGardner::new(sps as f32, 0.004);
        let mut prev = None;
        let mut out = Vec::new();
        for &x in &iq {
            if let Some(f) = mf.push(x) {
                if let Some(sym) = g.push(f) {
                    if let Some(p) = prev {
                        out.push(dphase_to_dibit(differential_detect(sym, p)));
                    }
                    prev = Some(sym);
                }
            }
        }
        // Compare the settled tail against the transmitted dibits at the best
        // alignment (loop latency introduces an unknown constant offset).
        let tail = &out[out.len().saturating_sub(400)..];
        let mut best = 1.0f64;
        for delay in 0..dibits.len().saturating_sub(tail.len()) {
            let n = tail.len();
            let errs = tail
                .iter()
                .zip(&dibits[delay..delay + n])
                .filter(|(a, b)| a != b)
                .count();
            best = best.min(errs as f64 / n as f64);
        }
        assert!(
            best < 0.02,
            "timing recovery failed, symbol error {best:.3}"
        );
    }
}
