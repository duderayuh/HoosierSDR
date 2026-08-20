//! Constant-Modulus Decision-Feedback Equalizer — phase-blind, for the
//! simulcast deep-null channel the linear CMA cannot open.
//!
//! ## Why a DFE, and why here
//!
//! A simulcast site transmits the same waveform from several towers. At a
//! receiver between them the paths sum with a relative delay, so the channel
//! is two-ray: `rx[n] = s[n] + a·e^{jφ}·s[n−d]`. As the towers' relative
//! phase φ drifts, that echo rotates; when it swings toward destructive
//! alignment the channel develops a **deep spectral null**. Field captures
//! show exactly this as short bursts (5–11 symbols) where the differential
//! phase is dragged around while amplitude stays high — the residual that
//! survives the linear CMA and kills ~13 TSBK blocks per control-channel pass
//! (see `results/baselines.md`).
//!
//! A linear equalizer (the [`CmaEqualizer`](super::cma::CmaEqualizer)) opens a
//! null by inverting it, which amplifies the noise sitting in that null — the
//! textbook failure of linear equalization on a nulled channel. A
//! decision-feedback equalizer instead **cancels** the post-cursor echo using
//! past decisions, with no noise enhancement, which is precisely the case DFE
//! exists for.
//!
//! ## Staying phase-blind
//!
//! P25 CQPSK is π/4-DQPSK: non-coherent, no absolute phase reference (that is
//! why the equalizer sits before differential detection at all). A classic
//! decision-directed DFE slices to the constellation and feeds back hard
//! symbols — impossible here without a phase reference, and hard slices
//! propagate errors. Instead, both the feedforward and feedback sections are
//! adapted by the **constant-modulus** criterion, and the fed-back "decision"
//! is the unit-circle projection `y/|y|`: amplitude normalization only, phase
//! preserved. The true symbols have unit magnitude, so a past output driven
//! onto the unit circle approximates the past symbol up to the same constant
//! rotation the feedback taps absorb — no reference, no hard slicing, no
//! constellation dependence, and the differential detector downstream removes
//! the residual rotation exactly as it does for the linear path.
//!
//! Implemented as a single constant-modulus update over the concatenated
//! regressor `[feedforward inputs ; −(past decisions)]`, so the feedback taps
//! get the correct gradient sign for free (the fed-back term is subtracted).

use crate::C32;

/// Phase-blind CMA decision-feedback equalizer.
pub struct CmaDfe {
    /// Feedforward taps (precursor + main tap), conj convention as in `output`.
    ff: Vec<C32>,
    /// Feedback taps, one per past decision they cancel.
    fb: Vec<C32>,
    /// Feedforward input delay line (ring buffer).
    xin: Vec<C32>,
    xpos: usize,
    /// Past unit-circle decisions, newest at `dpos` (ring buffer).
    dec: Vec<C32>,
    dpos: usize,
    mu_ff: f32,
    mu_fb: f32,
    r2: f32,
    /// EWMA of the CMA error magnitude — a convergence/health signal, matching
    /// [`CmaEqualizer::error_var`](super::cma::CmaEqualizer).
    pub error_var: f32,
    /// EWMA of input symbol power, used to hold the equalizer's input at unit
    /// power. The receiver's AGC normalizes *sample*-rate power ahead of the
    /// matched filter, so after the filter strips out-of-channel energy the
    /// symbols can land well below the constant-modulus radius (measured:
    /// ~0.25 RMS on an Airspy capture vs ~1.0 on RTL-SDR captures). A DFE
    /// started with a persistent modulus error of that size has a cheaper
    /// route to unit modulus than the slow feedforward gain: let the feedback
    /// taps synthesize it from past decisions — the degenerate CM-DFE minimum
    /// that decouples the output from the input entirely. Normalizing here
    /// makes the centre-spike init a genuine unit-modulus passthrough so that
    /// route never opens.
    in_pwr: f32,
    seen: u32,
}

impl CmaDfe {
    /// `ff_taps` feedforward taps (≥3, centre-spike initialized), `fb_taps`
    /// feedback taps (≥1; enough to span the simulcast echo delay).
    ///
    /// The feedforward and feedback sections take **separate** step sizes.
    /// The feedforward part is an ordinary constant-modulus filter and
    /// converges at the linear CMA's rate; the feedback loop is recursive —
    /// each output re-enters through the next symbol's decisions — so it needs
    /// a far gentler step or it rings and injects noise on a near-clean
    /// channel (measured: a shared aggressive step collapsed the decode, a
    /// ~100× slower feedback step recovered it and then beat the linear CMA).
    pub fn new(ff_taps: usize, fb_taps: usize, mu_ff: f32, mu_fb: f32) -> Self {
        assert!(ff_taps >= 3 && fb_taps >= 1);
        let mut ff = vec![C32::ZERO; ff_taps];
        ff[ff_taps / 2] = C32::new(1.0, 0.0); // identity: a clean signal passes
        Self {
            ff,
            fb: vec![C32::ZERO; fb_taps],
            xin: vec![C32::ZERO; ff_taps],
            xpos: 0,
            dec: vec![C32::ZERO; fb_taps],
            dpos: 0,
            mu_ff,
            mu_fb,
            r2: 1.0,
            error_var: 0.0,
            in_pwr: 1.0,
            seen: 0,
        }
    }

    fn reset(&mut self) {
        self.ff.iter_mut().for_each(|t| *t = C32::ZERO);
        let mid = self.ff.len() / 2;
        self.ff[mid] = C32::new(1.0, 0.0);
        self.fb.iter_mut().for_each(|t| *t = C32::ZERO);
        self.xin.iter_mut().for_each(|d| *d = C32::ZERO);
        self.dec.iter_mut().for_each(|d| *d = C32::ZERO);
        self.error_var = 0.0;
        self.in_pwr = 1.0;
        self.seen = 0;
    }

    /// Feedforward part: Σ ff[k]* · x[n−k].
    fn ff_out(&self) -> C32 {
        let n = self.ff.len();
        let mut acc = C32::ZERO;
        for k in 0..n {
            let x = self.xin[(self.xpos + n - k) % n];
            acc = acc + self.ff[k].conj() * x;
        }
        acc
    }

    /// Feedback part: Σ fb[j]* · dec[n−1−j] — strictly past decisions.
    fn fb_out(&self) -> C32 {
        let n = self.fb.len();
        let mut acc = C32::ZERO;
        for j in 0..n {
            // dpos holds the newest decision (already the previous symbol's,
            // since decisions are pushed after each output), so index back.
            let d = self.dec[(self.dpos + n - j) % n];
            acc = acc + self.fb[j].conj() * d;
        }
        acc
    }

    /// Push one symbol-rate sample, adapt, and return the equalized output.
    /// `adapt` gates tap updates so the caller can freeze the equalizer until
    /// the timing loop has settled (as the linear CMA path does).
    pub fn push(&mut self, x: C32, adapt: bool) -> C32 {
        // Symbol-rate power normalization (see `in_pwr`). Growing-window mean
        // during warm-up, then a ~100-symbol EWMA — slow next to the symbol
        // rate but fast next to the tap adaptation it protects.
        self.seen = self.seen.saturating_add(1);
        let a = (1.0 / self.seen as f32).max(0.01);
        self.in_pwr += a * (x.norm_sq() - self.in_pwr);
        let x = x.scale(1.0 / self.in_pwr.max(1e-12).sqrt());
        self.xpos = (self.xpos + 1) % self.xin.len();
        self.xin[self.xpos] = x;

        // y = feedforward − feedback (post-cursor echo cancelled by decisions).
        let y = self.ff_out() - self.fb_out();

        if adapt {
            // Constant-modulus gradient, NLMS-normalized by the *combined*
            // regressor energy (feedforward inputs plus fed-back decisions) so
            // the ke*-up transient cannot make the step diverge — the same
            // failure the linear CMA guards against, here spanning both
            // sections. `1e-6` keeps idle noise from dividing by zero.
            let ff_energy: f32 = self.xin.iter().map(|c| c.norm_sq()).sum();
            let fb_energy: f32 = self.dec.iter().map(|c| c.norm_sq()).sum();
            let energy = ff_energy + fb_energy + 1e-6;
            let g = self.r2 - y.norm_sq();
            // Shared CMA error, normalized; the per-section step is applied
            // when the tap is updated so feedforward and feedback can adapt at
            // their own rates.
            let e = y.scale(g / energy);
            let ey_ff = e.scale(self.mu_ff).conj();
            let ey_fb = e.scale(self.mu_fb).conj();

            let nf = self.ff.len();
            for k in 0..nf {
                let xk = self.xin[(self.xpos + nf - k) % nf];
                self.ff[k] = self.ff[k] + xk * ey_ff;
            }
            let nb = self.fb.len();
            for j in 0..nb {
                let dj = self.dec[(self.dpos + nb - j) % nb];
                // The fed-back term is subtracted in `y`, so its regressor is
                // −dj; the update carries that sign, cancelling the echo rather
                // than reinforcing it.
                self.fb[j] = self.fb[j] - dj * ey_fb;
            }
            self.error_var = 0.99 * self.error_var + 0.01 * (g * g);

            if !self.finite() {
                self.reset();
            }
        }

        // Record this output's unit-circle projection as the decision the next
        // symbols feed back. Amplitude-normalize only (phase preserved); a dead
        // sample projects to nothing.
        let mag = y.norm_sq().sqrt();
        let decision = if mag > 1e-6 {
            y.scale(1.0 / mag)
        } else {
            C32::ZERO
        };
        self.dpos = (self.dpos + 1) % self.dec.len();
        self.dec[self.dpos] = decision;

        y
    }

    fn finite(&self) -> bool {
        self.ff
            .iter()
            .chain(self.fb.iter())
            .all(|t| t.re.is_finite() && t.im.is_finite())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cqpsk::modulate_symbols;

    /// A two-ray channel whose echo is deep enough to null the linear CMA:
    /// the DFE must still drive the output to unit modulus, blindly.
    #[test]
    fn opens_a_deep_null_two_ray_eye_blindly() {
        let dibits: Vec<u8> = (0..8000).map(|i| ((i * 7 + i / 3) % 4) as u8).collect();
        let syms = modulate_symbols(&dibits);
        // 0.9 echo one symbol back: near-destructive, a deep spectral null —
        // the regime where a linear equalizer enhances noise and a DFE wins.
        let echo = C32::new(0.9, 0.0);
        let mut eq = CmaDfe::new(7, 3, 0.05, 0.005);

        let mut prev = C32::ZERO;
        let (mut early, mut late) = (0.0f32, 0.0f32);
        for (i, &s) in syms.iter().enumerate() {
            let rx = s + echo * prev;
            prev = s;
            let y = eq.push(rx, true);
            let e = (y.norm_sq() - 1.0).abs();
            if i < 800 {
                early += e;
            }
            if i >= syms.len() - 800 {
                late += e;
            }
        }
        assert!(
            late < early * 0.3,
            "CMA-DFE did not open the deep-null eye: early {early:.1} late {late:.1}"
        );
    }

    /// A clean channel arriving well below the constant-modulus radius (as the
    /// sample-rate AGC delivers on an Airspy capture, ~0.25 RMS) must not be
    /// "fixed" by the feedback section. Without input normalization the slow
    /// production step sizes let the feedback taps grow to synthesize unit
    /// modulus from past decisions — the degenerate CM-DFE minimum, which
    /// off-air collapsed a 209-TSBK control channel to 46 — while the CM error
    /// fell, so nothing downstream noticed. Pin both symptoms.
    #[test]
    fn low_level_input_does_not_feed_the_degenerate_minimum() {
        let dibits: Vec<u8> = (0..40_000).map(|i| ((i * 7 + i / 3) % 4) as u8).collect();
        let syms = modulate_symbols(&dibits);
        // Production step sizes (see `CqpskReceiver::new_dfe`).
        let mut eq = CmaDfe::new(9, 6, 0.001, 0.0005);
        let mut late = 0.0f32;
        for (i, &s) in syms.iter().enumerate() {
            let y = eq.push(s.scale(0.25), true);
            if i >= syms.len() - 800 {
                late += (y.norm_sq() - 1.0).abs();
            }
        }
        let fb_norm = eq.fb.iter().map(|c| c.norm_sq()).sum::<f32>().sqrt();
        assert!(
            fb_norm < 0.05,
            "feedback taps grew to {fb_norm:.3} on a clean low-level channel"
        );
        assert!(
            late < 1.0,
            "DFE did not pass a low-level clean channel: {late:.3}"
        );
    }

    /// On an already-clean channel the centre-spike init means the DFE passes
    /// the signal through and stays converged (it must not manufacture ISI).
    #[test]
    fn passes_a_clean_channel_without_harm() {
        let dibits: Vec<u8> = (0..4000).map(|i| ((i * 5) % 4) as u8).collect();
        let syms = modulate_symbols(&dibits);
        let mut eq = CmaDfe::new(7, 3, 0.05, 0.005);
        let mut late = 0.0f32;
        for (i, &s) in syms.iter().enumerate() {
            let y = eq.push(s, true);
            if i >= syms.len() - 800 {
                late += (y.norm_sq() - 1.0).abs();
            }
        }
        assert!(late < 1.0, "DFE distorted a clean channel: {late:.3}");
    }
}
