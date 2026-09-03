//! Soft-decision dibits: a symbol decision carried together with how much the
//! demodulator trusts it.
//!
//! Hard slicing throws away the most useful thing the demodulator knows. A
//! C4FM symbol at +2.9 and one at +2.05 both slice to the outer level, but the
//! first is unambiguous and the second sits almost on the decision boundary —
//! and once both are the bit `1`, every stage downstream treats them
//! identically. The frame-sync correlator counts them as equal evidence; the
//! Viterbi decoder charges them the same branch cost.
//!
//! Keeping the confidence is worth roughly 2–3 dB through a soft-input
//! decoder, which is the difference between recovering a marginal frame and
//! dropping it. Measured against the first field capture, the missing frames
//! were not corrupted-and-uncorrectable — they were frames whose sync word was
//! never detected at all, which is exactly the failure a soft correlator fixes.

/// A dibit plus per-bit confidence.
///
/// `conf[0]` belongs to the most significant bit, `conf[1]` to the least.
/// 0 means "no information, this bit is a coin flip"; 255 means "certain".
/// Confidence is deliberately a byte rather than a float: it keeps the type
/// cheap to copy through the framer's buffers, and the decoders only need a
/// monotone reliability ordering, not calibrated log-likelihoods.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SoftDibit {
    pub bits: u8,
    pub conf: [u8; 2],
}

/// Confidence value meaning "certain", used when a hard dibit is lifted into
/// the soft path.
pub const CERTAIN: u8 = 255;

impl SoftDibit {
    pub fn new(bits: u8, conf: [u8; 2]) -> Self {
        Self {
            bits: bits & 3,
            conf,
        }
    }

    /// Lift a hard dibit into the soft path, asserting full confidence.
    ///
    /// This keeps every existing hard-decision caller working unchanged and
    /// makes the soft decoders behave exactly like the hard ones when no
    /// confidence is available — so adopting the soft path cannot regress a
    /// path that has not been converted yet.
    pub fn hard(bits: u8) -> Self {
        Self::new(bits, [CERTAIN; 2])
    }

    /// The most significant bit.
    pub fn msb(self) -> u8 {
        (self.bits >> 1) & 1
    }

    /// The least significant bit.
    pub fn lsb(self) -> u8 {
        self.bits & 1
    }

    /// Confidence that bit `i` (0 = MSB) is what `bits` says.
    pub fn conf_of(self, i: usize) -> u8 {
        self.conf[i & 1]
    }

    /// Cost of claiming this dibit was `expected`: the summed confidence of
    /// every bit that would have to be wrong.
    ///
    /// This is the soft replacement for Hamming distance. A disagreement on a
    /// bit the demodulator was unsure of is cheap; a disagreement on a
    /// confident bit is expensive. With all-`CERTAIN` inputs it reduces to
    /// Hamming distance scaled by 255, so soft and hard decoding agree exactly
    /// on the hard case.
    pub fn cost_against(self, expected: u8) -> u32 {
        let diff = self.bits ^ (expected & 3);
        let mut c = 0u32;
        if diff & 0b10 != 0 {
            c += self.conf[0] as u32;
        }
        if diff & 0b01 != 0 {
            c += self.conf[1] as u32;
        }
        c
    }
}

/// Expand soft dibits to per-bit confidence (MSB of each dibit first),
/// mirroring `bits::dibits_to_bits`'s bit ordering exactly so the two arrays
/// stay index-aligned — `soft_dibits_to_bit_conf(d)[i]` is the confidence of
/// `dibits_to_bits(&d.iter().map(|s| s.bits).collect())[i]`.
pub fn soft_dibits_to_bit_conf(dibits: &[SoftDibit]) -> Vec<u8> {
    let mut out = Vec::with_capacity(dibits.len() * 2);
    for d in dibits {
        out.push(d.conf[0]);
        out.push(d.conf[1]);
    }
    out
}

/// Soft-slice a C4FM symbol (nominal ±1 / ±3) into a dibit with confidence.
///
/// The two bits are decided by two independent thresholds, so each carries its
/// own reliability:
///
/// * the **MSB** is the sign, so its confidence grows with `|sym|` — a symbol
///   near zero could be either inner level;
/// * the **LSB** selects inner versus outer, so its confidence grows with the
///   distance from the ±2 boundary between them.
pub fn soft_slice_c4fm(sym: f32) -> SoftDibit {
    // Level → dibit: +3 → 01, +1 → 00, −1 → 10, −3 → 11.
    let bits = if sym > 2.0 {
        0b01
    } else if sym > 0.0 {
        0b00
    } else if sym > -2.0 {
        0b10
    } else {
        0b11
    };
    // Scale so a full level of margin (1.0) reads as certain; the eye is
    // nominally ±1/±3, so 1.0 away from a threshold is as good as it gets.
    let msb = confidence(sym.abs());
    let lsb = confidence((sym.abs() - 2.0).abs());
    SoftDibit::new(bits, [msb, lsb])
}

/// Soft-slice a CQPSK differential phase into a dibit with confidence.
///
/// Mirrors the C4FM mapping on the phase circle: the MSB is decided by the
/// sign of the phase (boundary at 0 and ±π) and the LSB by whether the phase
/// is beyond ±π/2, so each bit's confidence is its distance from its own
/// decision boundary.
pub fn soft_slice_cqpsk(dphi: f32) -> SoftDibit {
    use core::f32::consts::PI;
    let mut p = dphi;
    while p > PI {
        p -= 2.0 * PI;
    }
    while p <= -PI {
        p += 2.0 * PI;
    }
    let bits = if p > PI / 2.0 {
        0b01
    } else if p > 0.0 {
        0b00
    } else if p > -PI / 2.0 {
        0b10
    } else {
        0b11
    };
    // Ideal phases sit π/4 from every boundary, so normalize by that.
    let quarter = PI / 4.0;
    let msb = confidence(p.abs().min(PI - p.abs()) / quarter);
    let lsb = confidence((p.abs() - PI / 2.0).abs() / quarter);
    SoftDibit::new(bits, [msb, lsb])
}

/// Map a normalized margin (1.0 = a full symbol of headroom) to a confidence
/// byte, saturating rather than wrapping.
fn confidence(margin: f32) -> u8 {
    if !margin.is_finite() || margin <= 0.0 {
        return 0;
    }
    (margin.min(1.0) * CERTAIN as f32).round() as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hard_dibits_reduce_to_hamming_distance() {
        // The soft cost must agree with hard decoding when confidence is full,
        // so converting a decoder to the soft metric cannot change its
        // behaviour on hard input.
        for bits in 0..4u8 {
            for expected in 0..4u8 {
                let hamming = (bits ^ expected).count_ones();
                assert_eq!(
                    SoftDibit::hard(bits).cost_against(expected),
                    hamming * CERTAIN as u32
                );
            }
        }
    }

    #[test]
    fn c4fm_confidence_tracks_distance_from_the_decision_boundary() {
        // Dead-centre symbols are trusted.
        let clean = soft_slice_c4fm(3.0);
        assert_eq!(clean.bits, 0b01);
        assert_eq!(clean.conf, [CERTAIN, CERTAIN]);

        // A symbol sitting on the inner/outer boundary keeps its sign but has
        // no idea which of the two positive levels it is.
        let edge = soft_slice_c4fm(2.0);
        assert_eq!(edge.conf[1], 0, "LSB on the boundary must be untrusted");
        assert!(edge.conf[0] > 200, "sign is still obvious: {edge:?}");

        // A symbol near zero has no trustworthy sign.
        let zero = soft_slice_c4fm(0.05);
        assert!(
            zero.conf[0] < 20,
            "sign near zero must be untrusted: {zero:?}"
        );
    }

    #[test]
    fn c4fm_soft_slicing_agrees_with_hard_slicing() {
        // Whatever the confidence, the decision itself must not change.
        for i in -400..=400 {
            let s = i as f32 / 100.0;
            assert_eq!(
                soft_slice_c4fm(s).bits,
                hs_dsp_slice(s),
                "disagreement at {s}"
            );
        }
    }

    /// Mirror of `hs_dsp::c4fm::slice`, duplicated here because hs-p25 does not
    /// depend on hs-dsp; the test above pins the two to the same thresholds.
    fn hs_dsp_slice(sym: f32) -> u8 {
        if sym > 2.0 {
            0b01
        } else if sym > 0.0 {
            0b00
        } else if sym > -2.0 {
            0b10
        } else {
            0b11
        }
    }

    #[test]
    fn cqpsk_confidence_is_highest_at_the_ideal_phases() {
        use core::f32::consts::PI;
        for (phase, bits) in [
            (PI * 0.75, 0b01u8),
            (PI * 0.25, 0b00),
            (-PI * 0.25, 0b10),
            (-PI * 0.75, 0b11),
        ] {
            let d = soft_slice_cqpsk(phase);
            assert_eq!(d.bits, bits, "phase {phase}");
            assert_eq!(d.conf, [CERTAIN, CERTAIN], "ideal phase {phase}");
        }
        // Halfway between two ideal phases, the bit that separates them is a
        // coin flip.
        let boundary = soft_slice_cqpsk(PI / 2.0);
        assert_eq!(boundary.conf[1], 0, "{boundary:?}");
    }
}
