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
//!
//! The permutation is constant only *for as long as the receiver holds its
//! bias*. It does not always. The decision-directed carrier loop is blind to
//! a quarter-turn slip (the constellation maps onto itself), so a fade or a
//! glitch can walk the bias onto the neighbouring quarter turn with the
//! receiver's own lock metric never flinching — and a re-acquisition lands
//! on any of the four turns with equal odds. Measured on a survey capture:
//! the equalized receiver went silent for 11.4 s in the middle of a clean
//! transmission that the bypassed receiver decoded straight through, with
//! no watchdog trip; a frozen derotation was permuting every dibit. So the
//! search never stops: after lock, every dibit still updates the window, and
//! a sync word found under a *different* rotation re-pins it. The dibits
//! are handed to the framer through a 24-dibit delay line so the frame that
//! reveals a slip is itself delivered correctly — the framer locks on that
//! very frame instead of the next one.
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
    /// Ring of the most recent raw dibits with their soft confidences: the
    /// delay line the framer is fed from once locked, and the sync word
    /// replayed on lock. The confidence rides along so the soft-decision
    /// framer sees each dibit with its own symbol's confidence, not a later
    /// one's.
    recent: [(u8, [u8; 2]); FSW_DIBITS],
    at: usize,
    seen: usize,
    /// Ring entries not yet handed to the framer (≤ `FSW_DIBITS`).
    unflushed: usize,
    /// Resolved rotation, once found.
    rot: Option<u8>,
    /// Times the rotation changed after the first lock (a bias slip or a
    /// re-acquisition on another quarter turn) — diagnostics.
    slips: u32,
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
            recent: [(0, [0, 0]); FSW_DIBITS],
            at: 0,
            seen: 0,
            unflushed: 0,
            rot: None,
            slips: 0,
        }
    }

    /// The resolved quarter-turn rotation, if the sync word has been found.
    pub fn rotation(&self) -> Option<u8> {
        self.rot
    }

    /// Rotation changes since the first lock.
    pub fn slips(&self) -> u32 {
        self.slips
    }

    /// Feed one raw dibit from the CQPSK receiver, appending the dibits the
    /// framer should see to `out` (each with its confidence): nothing while
    /// the rotation is still unknown, the whole derotated sync word on the
    /// push that resolves it, and thereafter one derotated dibit per push,
    /// 24 dibits behind the input (so a rotation slip revealed by a sync
    /// word is corrected before that sync word reaches the framer).
    pub fn push(&mut self, raw: u8, conf: [u8; 2], out: &mut Vec<(u8, [u8; 2])>) {
        self.shift = (self.shift << 2) | raw as u64;
        self.seen += 1;
        let window = self.shift & ((1u64 << FRAME_SYNC_BITS) - 1);
        let found = if self.seen >= FSW_DIBITS {
            (0..4u8).find(|&k| (window ^ self.patterns[k as usize]).count_ones() <= LOCK_ERR_MAX)
        } else {
            None
        };
        match (self.rot, found) {
            (None, Some(k)) => {
                self.rot = Some(k);
                // Replay the sync word, derotated and in order, so the framer
                // locks on the very frame that revealed the rotation instead
                // of waiting another 180 ms for the next one. The ring holds
                // the 23 dibits before this one.
                for i in 0..FSW_DIBITS - 1 {
                    let (r, c) = self.recent[(self.at + 1 + i) % FSW_DIBITS];
                    out.push((rotate_dibit(r, (4 - k) & 3), c));
                }
                out.push((rotate_dibit(raw, (4 - k) & 3), conf));
                // Everything in the ring, this dibit included, has now gone
                // out; the delay line starts empty from the next push.
                self.unflushed = 0;
                self.recent[self.at] = (raw, conf);
                self.at = (self.at + 1) % FSW_DIBITS;
                return;
            }
            (Some(k0), Some(k)) if k != k0 => {
                // The bias slipped a quarter turn (or a re-acquisition landed
                // on another one). Re-pin; the sync word that showed it is
                // still in the delay line and goes out under the new turn.
                self.rot = Some(k);
                self.slips += 1;
            }
            _ => {}
        }
        if let Some(k) = self.rot {
            // Delay line: hand over the dibit leaving the ring.
            if self.unflushed == FSW_DIBITS {
                let (r, c) = self.recent[self.at];
                out.push((rotate_dibit(r, (4 - k) & 3), c));
            } else {
                self.unflushed += 1;
            }
        }
        self.recent[self.at] = (raw, conf);
        self.at = (self.at + 1) % FSW_DIBITS;
    }

    /// `push` without confidences, for callers and tests that only care
    /// about the dibits.
    #[cfg(test)]
    fn push_dibit(&mut self, raw: u8, out: &mut Vec<u8>) {
        let mut buf = Vec::new();
        self.push(raw, [0, 0], &mut buf);
        out.extend(buf.into_iter().map(|(d, _)| d));
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
                d.push_dibit(raw, &mut recovered);
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
            d.push_dibit(raw, &mut sink);
        }
        assert_eq!(d.rotation(), Some(k));
        // Post-lock dibits come out derotated, FSW_DIBITS pushes later (the
        // delay line that lets a later slip be corrected in time).
        let later = [0u8, 1, 2, 3, 3, 1];
        sink.clear();
        for &truth in &later {
            d.push_dibit(rotate_dibit(truth, k), &mut sink);
        }
        assert!(sink.is_empty(), "nothing leaves the delay line early");
        for _ in 0..FSW_DIBITS {
            d.push_dibit(rotate_dibit(2, k), &mut sink);
        }
        assert_eq!(&sink[..later.len()], &later, "post-lock derotation");
    }

    #[test]
    fn rotation_is_a_group_of_order_four() {
        for d in 0..4u8 {
            assert_eq!(rotate_dibit(d, 0), d);
            assert_eq!(rotate_dibit(rotate_dibit(d, 1), 3), d);
            assert_eq!(rotate_dibit(rotate_dibit(d, 2), 2), d);
        }
    }

    /// Everything after the preamble must reach the framer in order, exactly
    /// once, through the delay line.
    #[test]
    fn the_delay_line_preserves_the_stream() {
        let mut d = Derotator::new();
        let mut recovered = Vec::new();
        let sync: Vec<u8> = hs_p25::synth::sync_dibits();
        let payload: Vec<u8> = (0..300).map(|i| ((i * 7 + 2) % 4) as u8).collect();
        let mut truth: Vec<u8> = Vec::new();
        truth.extend((0..40).map(|i| ((i * 3 + 1) % 4) as u8));
        let start = truth.len();
        truth.extend(&sync);
        truth.extend(&payload);
        for &x in &truth {
            d.push_dibit(rotate_dibit(x, 2), &mut recovered);
        }
        // Flush the delay line with dibits we do not check.
        for _ in 0..FSW_DIBITS {
            d.push_dibit(0, &mut recovered);
        }
        assert_eq!(&recovered[..truth.len() - start], &truth[start..]);
    }

    /// A quarter-turn slip mid-stream is re-pinned by the next sync word, and
    /// that sync word itself arrives derotated correctly.
    #[test]
    fn a_rotation_slip_is_repinned_on_the_next_sync_word() {
        let mut d = Derotator::new();
        let mut recovered = Vec::new();
        let sync: Vec<u8> = hs_p25::synth::sync_dibits();
        let frame: Vec<u8> = sync
            .iter()
            .copied()
            .chain((0..200).map(|i| ((i * 5 + 3) % 4) as u8))
            .collect();
        // Frame 1 under rotation 1, frames 2 and 3 under rotation 3.
        for &x in &frame {
            d.push_dibit(rotate_dibit(x, 1), &mut recovered);
        }
        assert_eq!(d.rotation(), Some(1));
        for &x in frame.iter().chain(frame.iter()) {
            d.push_dibit(rotate_dibit(x, 3), &mut recovered);
        }
        for _ in 0..FSW_DIBITS {
            d.push_dibit(0, &mut recovered);
        }
        assert_eq!(d.rotation(), Some(3));
        assert_eq!(d.slips(), 1);
        // Frame 1 intact up to its last dibit — the one leaving the delay
        // line on the push that revealed the slip goes out under the new
        // rotation, and in real traffic everything between the true slip
        // and the sync word is wrong anyway. Then frames 2 and 3 exact: the
        // slipped frame's own sync word was delivered under the new
        // rotation, so the framer locks on it, not the one after.
        let n = frame.len();
        assert_eq!(&recovered[..n - 1], &frame[..n - 1]);
        assert_eq!(&recovered[n..2 * n], &frame[..], "frame after the slip");
        assert_eq!(&recovered[2 * n..3 * n], &frame[..]);
    }

    /// Each dibit leaves the delay line with its own confidence.
    #[test]
    fn confidences_ride_the_delay_line() {
        let mut d = Derotator::new();
        let mut out = Vec::new();
        for x in hs_p25::synth::sync_dibits() {
            d.push(x, [7, 7], &mut out);
        }
        assert_eq!(d.rotation(), Some(0));
        assert!(out.iter().all(|&(_, c)| c == [7, 7]));
        out.clear();
        for i in 0..FSW_DIBITS + 3 {
            d.push((i % 4) as u8, [i as u8, i as u8], &mut out);
        }
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], (0, [0, 0]));
        assert_eq!(out[1], (1, [1, 1]));
        assert_eq!(out[2], (2, [2, 2]));
    }
}
