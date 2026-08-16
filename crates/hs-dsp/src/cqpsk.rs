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

use crate::equalizer::LmsFse;
use crate::C32;
use core::f32::consts::PI;

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
/// error). Placing an adaptive equalizer ahead of the detector — the thesis —
/// requires a phase-blind (CMA) equalizer on this modulation and is the next
/// integration step; see `EqualizedCqpsk` for the proven symbol-level result.
pub struct CqpskReceiver {
    mf: crate::fir::FirC,
    gardner: crate::timing_complex::ComplexGardner,
    prev_sym: Option<C32>,
    /// Tracked differential-phase bias from the carrier frequency offset.
    freq_bias: f32,
    mu_freq: f32,
    settle: u32,
}

impl CqpskReceiver {
    /// `sps` samples/symbol of the incoming IQ; `beta` RRC rolloff.
    pub fn new(sps: usize, beta: f64) -> Self {
        use crate::rrc::rrc_taps;
        Self {
            mf: crate::fir::FirC::new(rrc_taps(sps, 6, beta), 1),
            gardner: crate::timing_complex::ComplexGardner::new(sps as f32, 0.004),
            prev_sym: None,
            freq_bias: 0.0,
            mu_freq: 0.02,
            settle: 0,
        }
    }

    /// Push one IQ sample. Returns Some(dibit) when a symbol decision is made.
    pub fn push(&mut self, iq: C32) -> Option<u8> {
        let filtered = self.mf.push(iq)?;
        let sym = self.gardner.push(filtered)?;
        let prev = match self.prev_sym {
            Some(p) => p,
            None => {
                self.prev_sym = Some(sym);
                return None;
            }
        };
        // Differential phase, minus the tracked carrier-frequency bias.
        let raw = differential_detect(sym, prev);
        let corr = wrap_pi(raw - self.freq_bias);
        let dibit = dphase_to_dibit(corr);
        // Decision-directed bias update: pull toward the ideal differential
        // phase of the decided dibit. Let timing/interp settle first.
        self.settle = self.settle.saturating_add(1);
        if self.settle > 16 {
            let ideal = dibit_to_dphase(dibit);
            self.freq_bias += self.mu_freq * wrap_pi(corr - ideal);
        }
        self.prev_sym = Some(sym);
        Some(dibit)
    }

    /// Current carrier-frequency estimate as a per-symbol differential-phase
    /// bias (radians).
    pub fn freq_bias(&self) -> f32 {
        self.freq_bias
    }
}

/// Wrap a phase to (−π, π].
fn wrap_pi(mut p: f32) -> f32 {
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
