//! P25 CQPSK / LSM (π/4-DQPSK) primitives and the pre-detection equalized
//! receiver — the core of the project thesis.
//!
//! P25 Phase I has two compatible modulations at 4800 baud: C4FM (frequency,
//! handled in `receiver`) and CQPSK/LSM (linear, used on simulcast sites).
//! Both carry the same dibits. CQPSK maps each dibit to a **differential**
//! phase change:
//!
//! | dibit | Δφ    | (matches C4FM level) |
//! |-------|-------|----------------------|
//! | 01    | +3π/4 | +3                   |
//! | 00    | +π/4  | +1                   |
//! | 10    | −π/4  | −1                   |
//! | 11    | −3π/4 | −3                   |
//!
//! The receiver structure that distinguishes HoosierSDR: the adaptive
//! equalizer runs on the complex symbol stream **before** differential
//! detection. Every other open-source P25 decoder differentially detects
//! first, which is a nonlinearity that makes inter-symbol interference
//! unrecoverable (docs/ARCHITECTURE.md §1). See `EqualizedCqpsk`.

use crate::equalizer::{CmaDfe, CmaEqualizer, LmsFse};
use crate::C32;
use core::f32::consts::PI;

/// Which pre-detection equalizer the CQPSK receiver runs.
///
/// The thesis A/B is `Cma` vs `None`; `Dfe` adds decision feedback to cancel
/// the deep-null simulcast echo the linear CMA leaves behind (see
/// [`CmaDfe`](crate::equalizer::CmaDfe)).
enum FrontEq {
    None,
    Cma(CmaEqualizer),
    Dfe(CmaDfe),
}

impl FrontEq {
    fn push(&mut self, x: C32, adapt: bool) -> C32 {
        match self {
            FrontEq::None => x,
            FrontEq::Cma(eq) => eq.push(x, adapt),
            FrontEq::Dfe(eq) => eq.push(x, adapt),
        }
    }

    /// Echo profile read off the current taps; `None` when no equalizer runs.
    fn profile(&self) -> Option<EchoProfile> {
        match self {
            FrontEq::None => None,
            FrontEq::Cma(eq) => Some(echo_profile_from_taps(eq.taps(), &[])),
            FrontEq::Dfe(eq) => Some(echo_profile_from_taps(eq.ff_taps(), eq.fb_taps())),
        }
    }

    /// Reset the taps to the centre-spike identity, discarding any solution
    /// walked onto noise. Called on re-acquisition so a fresh transmission
    /// never inherits taps converged on an idle channel.
    fn reset(&mut self) {
        match self {
            FrontEq::None => {}
            FrontEq::Cma(eq) => eq.reset(),
            FrontEq::Dfe(eq) => eq.reset(),
        }
    }

    /// Gear-shift the equalizer's step sizes. Only the DFE reacts; the CMA's
    /// single step and the `None` baseline ignore this (the CMA needs no
    /// gear-shift — its one step serves both acquisition and tracking).
    fn set_step(&mut self, mu_ff: f32, mu_fb: f32) {
        match self {
            FrontEq::None => {}
            FrontEq::Cma(_) => {}
            FrontEq::Dfe(eq) => eq.set_step(mu_ff, mu_fb),
        }
    }
}

/// The echo structure a converged equalizer has learned, read from its taps.
///
/// This is the receiver's own live measure of simulcast distortion: on a
/// simulcast site the copies from multiple towers arrive at different delays,
/// and the equalizer's off-cursor tap energy is what it took to undo them. A
/// linear equalizer's taps approximate the *inverse* channel rather than the
/// channel itself, so treat these as a severity proxy, not a channel sounding
/// — clean air reads near zero, and it grows monotonically with echo strength
/// and delay, which is exactly what a "move / aim the antenna" feedback meter
/// needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EchoProfile {
    /// Fraction (0..1) of total tap energy away from the cursor tap. ~0 on a
    /// clean single-tower channel; grows with the echo the equalizer had to
    /// cancel.
    pub echo_frac: f32,
    /// Energy-weighted RMS spread of the tap delays around their mean, in
    /// symbols (1 symbol = 208.3 µs at 4800 baud).
    pub rms_spread_syms: f32,
}

impl EchoProfile {
    /// The RMS spread converted to microseconds at the P25 4800-baud rate.
    pub fn rms_spread_us(&self) -> f32 {
        self.rms_spread_syms * (1.0e6 / 4800.0)
    }
}

/// Compute an [`EchoProfile`] from equalizer taps: symbol-spaced feedforward
/// taps `ff` (the cursor is the strongest tap) plus optional decision-feedback
/// taps `fb`, where `fb[j]` cancels the echo `j + 1` symbols after the cursor.
pub fn echo_profile_from_taps(ff: &[C32], fb: &[C32]) -> EchoProfile {
    // Cursor = strongest feedforward tap. CMA can walk the peak off the
    // centre-spike start, so find it rather than assume the middle.
    let cursor = ff
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.norm_sq().total_cmp(&b.1.norm_sq()))
        .map(|(i, _)| i as f32)
        .unwrap_or(0.0);
    // (delay in symbols relative to the cursor, tap energy) for every tap.
    let terms = ff
        .iter()
        .enumerate()
        .map(|(k, t)| (k as f32 - cursor, t.norm_sq()))
        .chain(
            fb.iter()
                .enumerate()
                .map(|(j, t)| (j as f32 + 1.0, t.norm_sq())),
        );
    let mut total = 0.0f32;
    let mut cursor_e = 0.0f32;
    let mut m1 = 0.0f32;
    let mut m2 = 0.0f32;
    for (d, e) in terms {
        total += e;
        if d == 0.0 {
            cursor_e += e;
        }
        m1 += e * d;
        m2 += e * d * d;
    }
    // NaN-poisoned taps also land here (comparisons with NaN are false), so
    // a diverged equalizer reads as "no echo" rather than propagating NaN.
    if !total.is_finite() || total <= 1e-12 {
        return EchoProfile {
            echo_frac: 0.0,
            rms_spread_syms: 0.0,
        };
    }
    let mean = m1 / total;
    EchoProfile {
        echo_frac: (1.0 - cursor_e / total).clamp(0.0, 1.0),
        rms_spread_syms: (m2 / total - mean * mean).max(0.0).sqrt(),
    }
}

/// Differential phase increment for a dibit (radians).
pub fn dibit_to_dphase(d: u8) -> f32 {
    match d & 3 {
        0b01 => 3.0 * PI / 4.0,
        0b00 => PI / 4.0,
        0b10 => -PI / 4.0,
        _ => -3.0 * PI / 4.0,
    }
}

/// Slice a detected differential phase to a dibit.
pub fn dphase_to_dibit(dphi: f32) -> u8 {
    // Wrap to (-π, π].
    let mut p = dphi;
    while p > PI {
        p -= 2.0 * PI;
    }
    while p <= -PI {
        p += 2.0 * PI;
    }
    if p > PI / 2.0 {
        0b01 // +3π/4
    } else if p > 0.0 {
        0b00 // +π/4
    } else if p > -PI / 2.0 {
        0b10 // −π/4
    } else {
        0b11 // −3π/4
    }
}

/// Rotate a dibit by `k` quarter turns of the differential phase.
///
/// A residual carrier-frequency error of exactly a multiple of π/2 per symbol
/// is invisible to any blind estimator of π/4-DQPSK — the four ideal
/// differential phases (±π/4, ±3π/4) map onto themselves under a π/2 rotation.
/// The estimator therefore recovers the bias only *modulo* π/2, leaving the
/// detected dibit stream a fixed permutation of the truth. This function is
/// that permutation; the ambiguity is resolved downstream by finding which
/// rotation makes the known Frame Sync Word appear (see
/// `hs_core::decoder`).
pub fn rotate_dibit(d: u8, k: u8) -> u8 {
    // Quadrants ordered by increasing differential phase, matching
    // `dphase_to_dibit`: (−π,−π/2]→11, (−π/2,0]→10, (0,π/2]→00, (π/2,π]→01.
    const QUAD_TO_DIBIT: [u8; 4] = [0b11, 0b10, 0b00, 0b01];
    const DIBIT_TO_QUAD: [u8; 4] = [2, 3, 1, 0];
    QUAD_TO_DIBIT[((DIBIT_TO_QUAD[(d & 3) as usize] + k) & 3) as usize]
}

/// Differentially modulate dibits into absolute complex symbols (unit
/// magnitude), starting from phase 0.
pub fn modulate_symbols(dibits: &[u8]) -> Vec<C32> {
    let mut phase = 0.0f32;
    let mut out = Vec::with_capacity(dibits.len());
    for &d in dibits {
        phase += dibit_to_dphase(d);
        out.push(C32::new(phase.cos(), phase.sin()));
    }
    out
}

/// Differential detection: Δφ between consecutive symbols. `prev` is the
/// previously received symbol; returns (dphase, this_symbol) so the caller
/// can chain.
pub fn differential_detect(cur: C32, prev: C32) -> f32 {
    (cur * prev.conj()).arg()
}

/// Oversampled RRC-shaped CQPSK IQ: differentially modulate `dibits`, then
/// pulse-shape with a root-raised-cosine filter at `sps` samples/symbol.
/// Produces the kind of continuous complex baseband a real transmitter emits
/// (before channel and receiver front end).
pub fn modulate_iq(dibits: &[u8], sps: usize, beta: f64) -> Vec<C32> {
    use crate::fir::FirC;
    use crate::rrc::rrc_taps;

    let syms = modulate_symbols(dibits);
    // Upsample: place each symbol as an impulse, zero-fill between, then RRC.
    let taps: Vec<f32> = rrc_taps(sps, 6, beta)
        .into_iter()
        .map(|t| t * sps as f32)
        .collect();
    let mut filt = FirC::new(taps, 1);
    let mut out = Vec::with_capacity(syms.len() * sps);
    for &s in &syms {
        for k in 0..sps {
            let imp = if k == 0 { s } else { C32::ZERO };
            // Real RRC taps shape I and Q together (complex convolution with
            // real taps).
            out.push(filt.push(imp).unwrap_or(C32::ZERO));
        }
    }
    out
}

/// A CQPSK receiver with a sync-trained fractionally-spaced equalizer placed
/// **before** differential detection — the thesis realized at the symbol
/// level. Fed T/2-spaced complex samples; emits detected dibits.
///
/// Timing and carrier recovery are assumed upstream here (this operates on
/// symbol-synchronous complex samples); wiring it behind full Gardner/Costas
/// recovery on live IQ is the remaining Phase 1 integration.
pub struct EqualizedCqpsk {
    eq: LmsFse,
    /// Previous equalized symbol, for differential detection.
    prev: Option<C32>,
    /// True once the equalizer has been trained on a reference.
    trained: bool,
}

impl EqualizedCqpsk {
    pub fn new(num_taps: usize, mu: f32) -> Self {
        Self {
            eq: LmsFse::new(num_taps, mu),
            prev: None,
            trained: false,
        }
    }

    /// Train the equalizer on a known symbol sequence (e.g. the FSW), fed as
    /// T/2-spaced samples with the corresponding known absolute symbols. The
    /// two slices align by index at the decision instants; intermediate T/2
    /// samples get `None` desired.
    pub fn train(&mut self, samples_t2: &[C32], desired: &[Option<C32>], passes: usize) {
        assert_eq!(samples_t2.len(), desired.len());
        for _ in 0..passes {
            for (&x, &d) in samples_t2.iter().zip(desired) {
                self.eq.push(x);
                if let Some(target) = d {
                    self.eq.train(target);
                }
            }
        }
        self.trained = true;
    }

    /// Push one T/2-spaced sample; on decision instants (every other sample)
    /// returns the detected dibit. `is_decision` marks the on-symbol phase.
    pub fn push(&mut self, x: C32, is_decision: bool) -> Option<u8> {
        self.eq.push(x);
        if !is_decision {
            return None;
        }
        let sym = self.eq.output();
        let out = self
            .prev
            .map(|p| dphase_to_dibit(differential_detect(sym, p)));
        self.prev = Some(sym);
        out
    }

    pub fn error_var(&self) -> f32 {
        self.eq.error_var
    }
}

/// Full CQPSK receiver for real off-air IQ: RRC matched filter → complex
/// Gardner timing recovery → differential detection with carrier-frequency
/// tracking → dibit.
///
/// This is the carrier + timing front end that takes the symbol-level thesis
/// off the bench and onto a continuous, oversampled signal with the frequency
/// and timing offsets a real tuner delivers.
///
/// Carrier handling note: P25 CQPSK is π/4-DQPSK, whose constellation is an
/// 8-point union of two QPSK grids — a plain QPSK Costas loop corrupts it. A
/// static carrier *phase* offset is removed for free by differential
/// detection; a carrier *frequency* offset survives as a constant bias on
/// every differential phase, so we estimate and remove that bias in the
/// differential domain (decision-directed, tracking to zero steady-state
/// error). This receiver realizes the **thesis on live-style IQ**: a
/// phase-blind CMA equalizer (`equalizer::CmaEqualizer`) sits ahead of the
/// differential detector, removing inter-symbol interference before the
/// nonlinearity — with no carrier lock, which π/4-DQPSK does not permit. Build
/// with [`CqpskReceiver::new`] for the equalized path, or
/// [`CqpskReceiver::new_bare`] to bypass the equalizer for A/B comparison.
pub struct CqpskReceiver {
    dc: crate::agc::DcBlocker,
    pub agc: crate::agc::Agc,
    mf: crate::fir::FirC,
    gardner: crate::timing_complex::ComplexGardner,
    eq: FrontEq,
    prev_sym: Option<C32>,
    /// Tracked differential-phase bias from the carrier frequency offset.
    freq_bias: f32,
    mu_freq: f32,
    settle: u32,
    /// Blind (non-data-aided) acquisition accumulator: Σ exp(j·(4Δφ − π)).
    acq: C32,
    acq_n: u32,
    /// Consecutive failed acquisition windows; drives the idle-channel tap reset.
    acq_failures: u32,
    acquired: bool,
    /// Smoothed magnitude of the decision-directed phase error, the receiver's
    /// own measure of whether it is locked.
    err_ewma: f32,
    /// Consecutive symbols spent above the lock threshold.
    bad_run: u32,
}

/// Symbols to let the timing loop settle before blind acquisition starts.
const SETTLE_SYMS: u32 = 64;
/// Symbols averaged by the blind carrier-bias estimator (~90 ms at 4800 baud).
const ACQ_SYMS: u32 = 400;
/// Consecutive failed acquisition windows before the equalizer taps are reset
/// to identity. A cold-started traffic channel sits on idle noise (which the
/// AGC normalizes to unit power) until a call keys up; over minutes the DFE in
/// particular can settle into the degenerate CM-DFE minimum from that input.
/// Resetting after a few windows keeps the receiver at the best possible cold
/// start. No gate or threshold — identity only helps acquisition.
const ACQ_FAIL_LIMIT: u32 = 4;
/// DFE feedforward step during acquisition — fast enough to open the eye within
/// the acquisition window on a short voice burst, so the blind carrier
/// acquisition's coherence threshold is met. Gear-shifted to [`DFE_FF_TRACK`]
/// on acquisition.
const DFE_FF_ACQ: f32 = 0.05;
/// DFE feedforward step during tracking — slow, for low steady-state
/// misadjustment. A fast feedforward left running in steady-state costs ~6
/// TSBKs on the reference control channel (187 vs 193). Reached by the gear-shift
/// on acquisition.
const DFE_FF_TRACK: f32 = 0.001;
/// DFE feedback step — always slow. The recursive feedback section rings and
/// injects noise (and once collapsed a control channel to 6 syncs) at an
/// aggressive step, so it never gear-shifts.
const DFE_FB: f32 = 0.0005;
/// Minimum coherence |Σ exp(j·(4Δφ−π))| / N for the acquisition to be believed.
/// Real CQPSK approaches 1; noise sits near 1/√N ≈ 0.05 at N = 400. A threshold
/// well above the noise floor but far below a real lock cleanly separates them.
const ACQ_COHERENCE_MIN: f32 = 0.30;

/// Decision-directed phase error, in radians, above which the receiver is
/// judged unlocked.
///
/// The error is bounded by π/4 ≈ 0.785 by construction, since it is measured
/// against whichever symbol was decided. Noise therefore averages around 0.39,
/// half the bound, while a locked receiver sits far below it. The threshold
/// splits those two populations.
const LOCK_ERR_MAX: f32 = 0.33;

/// Consecutive bad symbols before re-acquiring (~0.2 s). Long enough that a
/// brief fade does not trigger a re-acquisition, short enough to catch the
/// start of a transmission.
const BAD_RUN_LIMIT: u32 = 1000;

impl CqpskReceiver {
    /// Equalized front end: CMA equalizer before differential detection.
    /// `sps` samples/symbol; `beta` RRC rolloff.
    pub fn new(sps: usize, beta: f64) -> Self {
        Self::build(sps, beta, FrontEq::Cma(CmaEqualizer::new(9, 0.05)))
    }

    /// Baseline front end with no equalizer (for A/B measurement).
    pub fn new_bare(sps: usize, beta: f64) -> Self {
        Self::build(sps, beta, FrontEq::None)
    }

    /// Decision-feedback front end: cancels the post-cursor simulcast echo the
    /// linear CMA leaves in a deep spectral null. Same phase-blind placement
    /// before differential detection (see [`CmaDfe`](crate::equalizer::CmaDfe)).
    pub fn new_dfe(sps: usize, beta: f64) -> Self {
        // 9 feedforward + 6 feedback taps. The feedforward starts at the fast
        // acquisition step so the eye opens during blind acquisition — a slow
        // feedforward never converges inside a short voice burst, and the
        // acquisition coherence then never clears its threshold — and
        // gear-shifts to the slow tracking step on acquisition in `push_phase`.
        // The feedback stays slow throughout (see [`DFE_FB`]). This lifts the
        // reference control channel 192 → 202 TSBKs without sacrificing
        // short-burst acquisition.
        Self::build(
            sps,
            beta,
            FrontEq::Dfe(CmaDfe::new(9, 6, DFE_FF_ACQ, DFE_FB)),
        )
    }

    fn build(sps: usize, beta: f64, eq: FrontEq) -> Self {
        use crate::rrc::rrc_taps;
        Self {
            dc: crate::agc::DcBlocker::default(),
            agc: crate::agc::Agc::new(1e-3, 1.0),
            mf: crate::fir::FirC::new(rrc_taps(sps, 6, beta), 1),
            gardner: crate::timing_complex::ComplexGardner::new(sps as f32, 0.004),
            eq,
            prev_sym: None,
            freq_bias: 0.0,
            mu_freq: 0.02,
            settle: 0,
            acq: C32::ZERO,
            acq_n: 0,
            acq_failures: 0,
            acquired: false,
            err_ewma: 0.0,
            bad_run: 0,
        }
    }

    /// Push one IQ sample. Returns Some(dibit) when a symbol decision is made.
    /// Use [`CqpskReceiver::push_phase`] to get the differential phase too,
    /// which downstream soft-decision decoding needs.
    ///
    /// Cold-start order matters here. On real tuner output the receiver must
    /// first strip the DC spur and normalize level (so the constant-modulus
    /// equalizer has a meaningful modulus target), then let timing settle,
    /// then acquire the carrier bias *blindly* — a decision-directed loop
    /// cannot pull in from cold, because a 1 kHz tuner offset is 1.3 rad of
    /// differential phase per symbol, far outside the ±π/4 decision well, so
    /// every decision it would steer on is already wrong.
    pub fn push(&mut self, iq: C32) -> Option<u8> {
        self.push_phase(iq).map(|(d, _)| d)
    }

    /// As [`CqpskReceiver::push`], but also returns the carrier-corrected
    /// differential phase the dibit was sliced from. How far that phase sits
    /// from a decision boundary is precisely how much the decision can be
    /// trusted, and discarding it is what forces every stage downstream into
    /// hard decisions.
    pub fn push_phase(&mut self, iq: C32) -> Option<(u8, f32)> {
        // Front-door guard: a single non-finite sample (front-end overflow, USB
        // drop, tuner glitch) would otherwise poison the IIR state of the DC
        // blocker and AGC permanently — their EWMA/feedback state carries no
        // finite guard, and a NaN there can never recover, because `reacquire()`
        // does not reset those stages. Dropping the corrupt sample before it
        // reaches any filter state is the only place recovery is free. The
        // `raw` guard below is defense-in-depth for NaN arising internally.
        if !iq.re.is_finite() || !iq.im.is_finite() {
            return None;
        }
        let cleaned = self.agc.push(self.dc.push(iq));
        let filtered = self.mf.push(cleaned)?;
        let sym = self.gardner.push(filtered)?;
        self.settle = self.settle.saturating_add(1);
        // Equalize before differential detection (the thesis). Freeze the
        // taps until the timing loop has settled so it adapts on a real eye.
        //
        // Do NOT gate this on `acquired`. The blind carrier acquisition depends
        // on the equalizer having already opened the eye on multipath — freezing
        // the taps until acquisition completes drops the acquisition coherence
        // below its threshold and deadlocks (observed on the 0.6-echo ISI test:
        // coherence ~0.07 < ACQ_COHERENCE_MIN 0.30, so acquisition never fires).
        // "Adapting to idle noise" is instead handled by the lock-loss
        // re-acquisition (the err_ewma watch below) plus the tap reset in
        // `reacquire()`.
        let sym = self.eq.push(sym, self.settle > 32);
        let prev = match self.prev_sym {
            Some(p) => p,
            None => {
                self.prev_sym = Some(sym);
                return None;
            }
        };
        let raw = differential_detect(sym, prev);
        // A single non-finite sample upstream (front-end overflow, USB drop,
        // AGC divide) poisons `raw` and, through `err`/`err_ewma`/`freq_bias`,
        // the whole receiver. The lock watchdog below is `> LOCK_ERR_MAX`,
        // which is false for NaN, so without this guard a NaN would make the
        // receiver decode garbage forever with no recovery path. Recover before
        // the poison can spread.
        if !raw.is_finite() {
            self.reacquire();
            return None;
        }
        self.prev_sym = Some(sym);

        // Phase 1 — blind carrier acquisition. The four ideal differential
        // phases are the odd multiples of π/4, so 4·Δφ is always ≡ π (mod 2π)
        // regardless of the data. Averaging exp(j·(4Δφ − π)) therefore cancels
        // the modulation and leaves 4× the carrier bias, with no decisions
        // involved. The estimate is only unique modulo π/2 (see
        // `rotate_dibit`); the residual quarter-turn is resolved against the
        // Frame Sync Word downstream.
        if !self.acquired {
            if self.settle > SETTLE_SYMS {
                let a = 4.0 * raw - PI;
                self.acq = self.acq + C32::new(a.cos(), a.sin());
                self.acq_n += 1;
                if self.acq_n >= ACQ_SYMS {
                    // Only accept the estimate if the accumulator is coherent.
                    // Σ exp(j·(4Δφ − π)) has magnitude ≈ acq_n on real CQPSK
                    // (the modulation cancels, leaving the carrier bias in
                    // phase) but only √acq_n on noise (a random walk). A
                    // traffic channel is idle until a call keys up, so blind
                    // acquisition would otherwise *complete on noise*, latch a
                    // meaningless bias, and never revisit it — decoding the
                    // real transmission that follows at chance. Requiring
                    // coherence makes acquisition wait for signal instead.
                    let coherence = self.acq.norm_sq().sqrt() / self.acq_n as f32;
                    if coherence > ACQ_COHERENCE_MIN {
                        self.freq_bias = wrap_pi(self.acq.arg()) / 4.0;
                        self.acquired = true;
                        // The eye is open: gear-shift the DFE feedforward from
                        // the fast acquisition step to the slow tracking step.
                        self.eq.set_step(DFE_FF_TRACK, DFE_FB);
                    } else {
                        // Not signal yet. Slide the window forward rather than
                        // restart cold, so onset is caught within one window.
                        self.acq = C32::ZERO;
                        self.acq_n = 0;
                        // Idle-channel recovery. This branch is the ONLY place
                        // reachable before acquisition completes — `reacquire()`
                        // cannot fire here because there is no lock to lose.
                        // After a few failed windows the taps are prima facie
                        // not opening an eye (or there is no signal yet), so
                        // reset them to identity; identity is the best possible
                        // cold start and the reset cannot hurt acquisition.
                        self.acq_failures += 1;
                        if self.acq_failures >= ACQ_FAIL_LIMIT {
                            self.eq.reset();
                            self.acq_failures = 0;
                        }
                    }
                }
            }
            return None;
        }

        // Phase 2 — track. Differential phase, minus the carrier bias.
        let corr = wrap_pi(raw - self.freq_bias);
        let dibit = dphase_to_dibit(corr);
        // Decision-directed refinement: now that the bias is inside the
        // decision well, pull it toward the ideal phase of the decided dibit.
        let ideal = dibit_to_dphase(dibit);
        let err = wrap_pi(corr - ideal);
        self.freq_bias += self.mu_freq * err;

        // Watch the lock, and re-acquire if it is not real.
        //
        // Blind acquisition happens once, on whatever the receiver hears
        // first. On a traffic channel that is silence: the channel is idle
        // until a call is granted onto it, so the estimate is made from noise
        // and is meaningless, and without this the receiver would keep that
        // estimate through the entire transmission that follows. A control
        // channel never exposes this because it transmits continuously.
        self.err_ewma += 0.002 * (err.abs() - self.err_ewma);
        // `!(<=)` rather than `>` so a NaN err_ewma (poisoned upstream) counts
        // as unlocked and re-acquires, instead of comparing false forever.
        if !(self.err_ewma <= LOCK_ERR_MAX) {
            self.bad_run += 1;
            if self.bad_run >= BAD_RUN_LIMIT {
                self.reacquire();
                return None;
            }
        } else {
            self.bad_run = 0;
        }
        Some((dibit, corr))
    }

    /// True once blind carrier acquisition has completed.
    pub fn acquired(&self) -> bool {
        self.acquired
    }

    /// The most recent equalized symbol (the point a constellation display
    /// plots), once one has been decided.
    pub fn last_symbol(&self) -> Option<C32> {
        self.prev_sym
    }

    /// Restart blind acquisition — call after a prolonged loss of sync, when
    /// the tracked bias may have walked onto a neighbouring quarter turn.
    pub fn reacquire(&mut self) {
        self.acq = C32::ZERO;
        self.acq_n = 0;
        self.acquired = false;
        self.settle = 0;
        self.err_ewma = 0.0;
        self.bad_run = 0;
        self.freq_bias = 0.0;
        self.prev_sym = None;
        self.acq_failures = 0;
        // Discard any taps the equalizer walked onto the noise in the window
        // between the transmission ending and this re-acquisition. The next
        // transmission starts from the centre-spike identity, not a stale
        // solution converged on an idle channel.
        self.eq.reset();
        // Back to the fast acquisition step for the next acquisition.
        self.eq.set_step(DFE_FF_ACQ, DFE_FB);
    }

    /// Smoothed decision-directed phase error: low when locked, near 0.39 on
    /// noise. Exposed for diagnostics.
    pub fn lock_error(&self) -> f32 {
        self.err_ewma
    }

    /// The echo structure the equalizer has learned — the live simulcast-
    /// distortion readout. `None` when no equalizer runs or before blind
    /// acquisition completes (taps adapted on idle noise mean nothing).
    pub fn echo_profile(&self) -> Option<EchoProfile> {
        if !self.acquired {
            return None;
        }
        self.eq.profile()
    }

    /// Current carrier-frequency estimate as a per-symbol differential-phase
    /// bias (radians).
    pub fn freq_bias(&self) -> f32 {
        self.freq_bias
    }
}

/// Wrap a phase to (−π, π].
pub fn wrap_pi(mut p: f32) -> f32 {
    use core::f32::consts::PI;
    while p > PI {
        p -= 2.0 * PI;
    }
    while p <= -PI {
        p += 2.0 * PI;
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echo_profile_identity_taps_read_clean() {
        let ff = [C32::ZERO, C32::new(1.0, 0.0), C32::ZERO];
        let p = echo_profile_from_taps(&ff, &[]);
        assert!(p.echo_frac < 1e-6, "echo_frac {}", p.echo_frac);
        assert!(p.rms_spread_syms < 1e-6, "spread {}", p.rms_spread_syms);
    }

    #[test]
    fn echo_profile_known_taps_compute_exactly() {
        // Cursor of unit energy at delay 0, one feedback tap of amplitude 0.5
        // at delay +1: energies 1.0 and 0.25. echo_frac = 0.25/1.25 = 0.2;
        // mean delay 0.2, E[d²] = 0.2, spread = √(0.2 − 0.04) = 0.4.
        let ff = [C32::ZERO, C32::new(1.0, 0.0), C32::ZERO];
        let fb = [C32::new(0.0, 0.5)];
        let p = echo_profile_from_taps(&ff, &fb);
        assert!(
            (p.echo_frac - 0.2).abs() < 1e-6,
            "echo_frac {}",
            p.echo_frac
        );
        assert!(
            (p.rms_spread_syms - 0.4).abs() < 1e-6,
            "spread {}",
            p.rms_spread_syms
        );
        // At 4800 baud, 0.4 symbols is 83.3 µs.
        assert!((p.rms_spread_us() - 83.33).abs() < 0.1);
    }

    #[test]
    fn echo_profile_separates_clean_from_two_ray() {
        // Converge the CMA on a clean channel and on a two-ray channel; the
        // echo fraction read off the taps must clearly separate the two.
        let dibits: Vec<u8> = (0..20000).map(|i| ((i * 7 + i / 3) % 4) as u8).collect();
        let syms = modulate_symbols(&dibits);
        // The production receiver's CMA parameters (see `CqpskReceiver::new`).
        let run = |echo: Option<C32>| {
            let mut eq = crate::equalizer::CmaEqualizer::new(9, 0.05);
            let mut prev = C32::ZERO;
            for &s in &syms {
                let rx = match echo {
                    Some(e) => s + e * prev,
                    None => s,
                };
                prev = s;
                eq.push(rx, true);
            }
            echo_profile_from_taps(eq.taps(), &[])
        };
        // Measured steady states (CMA 9 taps, µ 0.05): clean 0.000, echo 0.20
        // → 0.009, echo 0.45 → 0.033, echo 0.70 → 0.064 — small in absolute
        // terms (the CMA's converged solution under-inverts the channel) but
        // cleanly monotonic in echo strength, which is what a meter needs.
        let clean = run(None);
        let mild = run(Some(C32::new(0.2 * 0.7, 0.2 * 0.7)));
        let strong = run(Some(C32::new(0.45 * 0.7, 0.45 * 0.7)));
        assert!(clean.echo_frac < 0.005, "clean read {}", clean.echo_frac);
        assert!(
            strong.echo_frac > mild.echo_frac && mild.echo_frac > 0.005,
            "strong {} mild {} clean {}",
            strong.echo_frac,
            mild.echo_frac,
            clean.echo_frac
        );
        assert!(
            strong.rms_spread_syms > 0.3,
            "spread {}",
            strong.rms_spread_syms
        );
    }

    #[test]
    fn diff_modulation_roundtrips_clean() {
        let dibits = [0b01u8, 0b00, 0b11, 0b10, 0b10, 0b01, 0b11, 0b00];
        let syms = modulate_symbols(&dibits);
        // Differential detection of the clean symbols must recover the dibits
        // (first symbol has no predecessor → skip).
        let mut prev = C32::new(1.0, 0.0); // implicit phase-0 start
        for (i, &s) in syms.iter().enumerate() {
            let d = dphase_to_dibit(differential_detect(s, prev));
            assert_eq!(d, dibits[i], "dibit {i}");
            prev = s;
        }
    }
}
