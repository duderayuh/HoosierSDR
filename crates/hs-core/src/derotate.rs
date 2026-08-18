//! Resolve the π/2 rotation ambiguity a blindly-acquired CQPSK receiver
//! leaves in its dibit stream.
//!
//! `hs_dsp::cqpsk::CqpskReceiver` estimates the carrier-frequency bias without
//! any reference symbols, which it can only do modulo π/2 — the four ideal
//! differential phases of π/4-DQPSK map onto themselves under a quarter turn,
//! so no blind estimator can tell the four apart (see
//! `hs_dsp::cqpsk::rotate_dibit`). The detected dibits are therefore a fixed
//! permutation of the truth, and the permutation is constant for as long as
//! the receiver holds its bias.
//!
//! P25 hands us the answer for free: the 24-symbol Frame Sync Word is known
//! and arrives every 180 ms. We search the raw stream for the FSW under all
//! four rotations at once; whichever one matches names the permutation. Doing
//! it here — rather than running four framers, or teaching the framer about
//! modulation — keeps the cost at four 48-bit comparisons per dibit.

use hs_p25::FRAME_SYNC_BITS;

/// Bit errors tolerated when matching a rotated Frame Sync Word. Kept tighter
/// than the framer's own threshold: a false lock here mis-rotates the entire
/// stream, which is far more damaging than missing one sync opportunity.
const LOCK_ERR_MAX: u32 = 1;

/// Number of dibits replayed into the framer once the rotation is found, so
/// the framer still sees the sync word that revealed it.
const FSW_DIBITS: usize = (FRAME_SYNC_BITS / 2) as usize;

pub struct Derotator {
    /// The four rotations of the Frame Sync Word, indexed by quarter turn.
    patterns: [u64; 4],
    /// Rolling window of raw (still-rotated) dibits.
    shift: u64,
    /// Ring of the most recent raw dibits, replayed on lock.
    recent: [u8; FSW_DIBITS],
    at: usize,
    seen: usize,
    /// Resolved rotation, once found.
    rot: Option<u8>,
}

impl Default for Derotator {
    fn default() -> Self {
        Self::new()
    }
}

impl Derotator {
    pub fn new() -> Self {
        let mut patterns = [0u64; 4];
        for (k, p) in patterns.iter_mut().enumerate() {
            let mut w = 0u64;
            for d in hs_p25::synth::sync_dibits() {
                w = (w << 2) | rotate_u64(d, k as u8);
            }
            *p = w;
        }
        Self {
            patterns,
            shift: 0,
            recent: [0; FSW_DIBITS],
            at: 0,
            seen: 0,
            rot: None,
        }
    }

    /// The resolved quarter-turn rotation, if the sync word has been found.
    pub fn rotation(&self) -> Option<u8> {
        self.rot
    }

    /// Feed one raw dibit from the CQPSK receiver, appending the dibits the
    /// framer should see to `out`: nothing while the rotation is still
    /// unknown, the whole derotated sync word on the push that resolves it,
    /// and one derotated dibit per push thereafter.
    pub fn push(&mut self, raw: u8, out: &mut Vec<u8>) {
        if let Some(k) = self.rot {
            // Detected = rotate(true, k), so true = rotate(detected, −k).
            out.push(rotate_dibit(raw, (4 - k) & 3));
            return;
        }

        self.recent[self.at] = raw;
        self.at = (self.at + 1) % FSW_DIBITS;
        self.seen += 1;
        self.shift = (self.shift << 2) | raw as u64;
        let window = self.shift & ((1u64 << FRAME_SYNC_BITS) - 1);

        if self.seen < FSW_DIBITS {
            return;
        }
        for k in 0..4u8 {
            if (window ^ self.patterns[k as usize]).count_ones() <= LOCK_ERR_MAX {
                self.rot = Some(k);
                // Replay the sync word, derotated and in order, so the framer
                // locks on the very frame that revealed the rotation instead
                // of waiting another 180 ms for the next one.
                for i in 0..FSW_DIBITS {
                    let raw = self.recent[(self.at + i) % FSW_DIBITS];
                    out.push(rotate_dibit(raw, (4 - k) & 3));
                }
                return;
            }
        }
    }
}

fn rotate_dibit(d: u8, k: u8) -> u8 {
    hs_dsp::cqpsk::rotate_dibit(d, k)
}

fn rotate_u64(d: u8, k: u8) -> u64 {
    rotate_dibit(d, k) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_sync_word_under_every_rotation() {
        for k in 0..4u8 {
            let mut d = Derotator::new();
            let mut recovered = Vec::new();
            // Preamble of arbitrary dibits, then the rotated sync word.
            let stream: Vec<u8> = (0..40)
                .map(|i| ((i * 3 + 1) % 4) as u8)
                .chain(hs_p25::synth::sync_dibits())
                .collect();
            for raw in stream.iter().map(|&x| rotate_dibit(x, k)) {
                d.push(raw, &mut recovered);
            }
            assert_eq!(d.rotation(), Some(k), "rotation {k} not found");
            // The replayed dibits must reconstruct the true sync word.
            let mut w = 0u64;
            for &x in recovered.iter().rev().take(FSW_DIBITS).rev() {
                w = (w << 2) | x as u64;
            }
            assert_eq!(w, hs_p25::FRAME_SYNC, "rotation {k} replay mismatch");
        }
    }

    #[test]
    fn passes_later_dibits_through_derotated() {
        let k = 2u8;
        let mut d = Derotator::new();
        let mut sink = Vec::new();
        for raw in hs_p25::synth::sync_dibits()
            .into_iter()
            .map(|x| rotate_dibit(x, k))
        {
            d.push(raw, &mut sink);
        }
        assert_eq!(d.rotation(), Some(k));
        for truth in [0u8, 1, 2, 3, 3, 1] {
            sink.clear();
            d.push(rotate_dibit(truth, k), &mut sink);
            assert_eq!(sink, vec![truth], "post-lock derotation");
        }
    }

    #[test]
    fn rotation_is_a_group_of_order_four() {
        for d in 0..4u8 {
            assert_eq!(rotate_dibit(d, 0), d);
            assert_eq!(rotate_dibit(rotate_dibit(d, 1), 3), d);
            assert_eq!(rotate_dibit(rotate_dibit(d, 2), 2), d);
        }
    }
}
