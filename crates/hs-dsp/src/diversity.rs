//! Spatial diversity combining: maximal-ratio combine (MRC) of soft
//! differential-phase estimates from two (or more) antennas.
//!
//! # Why this exists
//!
//! A simulcast P25 site transmits the same CQPSK symbol from several towers.
//! A single antenna hears the sum, and the two (or more) copies arrive with
//! different delays, so multipath fades cancel at some frequencies — the
//! deep spectral nulls a linear equalizer cannot claw back. Two antennas
//! placed a fraction of a wavelength apart see **decorrelated** fades: a null
//! on one antenna is rarely a null on the other. Combining the two soft
//! decisions recovers most of the lost symbols, which is the single biggest
//! remaining lever on the residual "robotic" voice (see docs/DIVERSITY.md).
//!
//! # Where it plugs in
//!
//! Each antenna runs its own full CQPSK receiver — its own AGC, matched
//! filter, Gardner timing loop, and its own carrier-bias removal — so the two
//! `dphi` values are independent estimates of the *same* differential phase
//! (`dibit_to_dphase`), each with its own noise. `mrc_phase` averages them
//! weighted by SNR. The caller then soft-slices the result
//! (`hs_p25::soft::soft_slice_cqpsk`) and feeds the framer exactly as before.

use crate::C32;

/// Maximal-ratio combine several estimates of the same differential phase.
///
/// Each `(dphi, snr)` is a phase estimate in radians (already carrier-bias
/// corrected) and a non-negative SNR weight (larger = trust more; the MRC-
/// optimal weight is the branch's signal-to-noise ratio, often proxied by
/// `1 / lock_error` — see `CqpskReceiver::lock_error`).
///
/// The SNR-weighted sum is taken over the **complex phasors** `exp(j·dphi)`
/// rather than the raw angles, so estimates that straddle the ±π wrap point
/// average correctly instead of tearing to their linear midpoint. Returns the
/// combined phase in `(-π, π]`.
pub fn mrc_phase(branches: &[(f32, f32)]) -> f32 {
    let mut rx = 0.0f32;
    let mut ix = 0.0f32;
    let mut w = 0.0f32;
    for &(dphi, snr) in branches {
        if !snr.is_finite() || snr <= 0.0 {
            continue;
        }
        rx += dphi.cos() * snr;
        ix += dphi.sin() * snr;
        w += snr;
    }
    if w <= 0.0 {
        return 0.0;
    }
    C32::new(rx, ix).arg()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cqpsk::{dibit_to_dphase, dphase_to_dibit};
    use core::f32::consts::PI;

    /// Deterministic xorshift32 → uniform in [0, 1). Kept hand-rolled so the
    /// crate stays dependency-free.
    struct Rng(u32);
    impl Rng {
        fn next(&mut self) -> f32 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            self.0 = x;
            (x >> 8) as f32 / 16_777_216.0
        }
        /// Uniform in [−a, a].
        fn sym(&mut self, a: f32) -> f32 {
            (self.next() * 2.0 - 1.0) * a
        }
    }

    #[test]
    fn identical_branches_average_to_their_common_phase() {
        for &p in &[0.5f32, -0.5, 2.0, -2.5] {
            let c = mrc_phase(&[(p, 1.0), (p, 1.0)]);
            assert!((c - p).abs() < 1e-4, "phase {p} → {c}");
        }
    }

    #[test]
    fn a_strong_branch_dominates_a_weak_one() {
        // 100:1 SNR — the combined phase must sit almost exactly on the
        // strong branch, not the midpoint.
        let c = mrc_phase(&[(0.5, 100.0), (-1.5, 1.0)]);
        assert!((c - 0.5).abs() < 0.02, "got {c}");
    }

    #[test]
    fn wrap_straddling_estimates_combine_circularly() {
        // +2.9 and −2.9 are both ≈ ±π — the same physical angle. A naive
        // linear mean would tear to 0 (the wrong answer); the phasor mean must
        // land near ±π.
        let c = mrc_phase(&[(2.9, 1.0), (-2.9, 1.0)]);
        let mag = c.abs();
        assert!(mag > PI - 0.15, "expected near ±π, got {c}");
    }

    #[test]
    fn mrc_cuts_the_slice_error_versus_either_branch() {
        // Two equal-SNR branches of independent uniform phase noise. Averaging
        // shrinks the noise variance, so the combined phase must slice wrong
        // strictly less often than *either* branch alone.
        let mut rng = Rng(0x9E3779B9);
        // Noise amplitude above the π/4 decision-boundary distance so a single
        // branch errors ~20% of the time — a robust, non-cliff regime.
        let a = 1.0f32;
        let mut err_a = 0u32;
        let mut err_b = 0u32;
        let mut err_c = 0u32;
        for _ in 0..20_000 {
            let d = (rng.next() * 4.0) as u8 & 3;
            let ideal = dibit_to_dphase(d);
            let pa = ideal + rng.sym(a);
            let pb = ideal + rng.sym(a);
            let pc = mrc_phase(&[(pa, 1.0), (pb, 1.0)]);
            if dphase_to_dibit(pa) != d {
                err_a += 1;
            }
            if dphase_to_dibit(pb) != d {
                err_b += 1;
            }
            if dphase_to_dibit(pc) != d {
                err_c += 1;
            }
        }
        assert!(err_a > 0 && err_b > 0, "noise too small to fail (a={a})");
        assert!(
            err_c < err_a && err_c < err_b,
            "MRC must beat each branch: combined {err_c} vs {err_a}/{err_b}"
        );
    }
}