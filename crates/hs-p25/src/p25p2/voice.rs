//! P25 Phase 2 voice-channel slot extraction.
//!
//! A descrambled 360-bit TDMA slot carries the voice-bearing block(s) at fixed
//! offsets, selected by the slot's DUID. This module maps a slot to its voice
//! blocks so the caller can hand each one to [`super::deinterleave`] and then
//! to the vocoder.
//!
//! # Slot layout (360 bits, descrambled; bit 0 is first on air)
//! ```text
//!   0..  2   DUID (part 1)                [duid.rs]
//!   2.. 74   voice block 0   (72 bits)
//!  74.. 76   DUID (part 2)                [duid.rs]
//!  76..148   voice block 1   (72 bits)
//! 148..172   ESS-B / ESS-A    (24 bits, further split by voice mode)
//! 172..244   voice block 2   (72 bits, full-rate only)
//! 244..246   DUID (part 3)                [duid.rs]
//! 246..318   voice block 3   (72 bits, full-rate only; else ESS-A part 2)
//! 318..320   DUID (part 4)                [duid.rs]
//! 320..360   ISCH            (40 bits)    [isch.rs]
//! ```
//! The two voice modes consume the same interleaver but different block counts:
//! * DUID 0 — **4V**, full rate (IMBE 7200): four 72-bit voice blocks.
//! * DUID 6 — **2V**, half rate (AMBE+2 3600): two 72-bit voice blocks.
//!
//! # Provenance
//! Offsets and the DUID→mode mapping are protocol **facts** from TIA-102.BBAB
//! / BBAC, cross-checked against ISC `dsd-fme` (`p25p2_frame.c`). No GPL code.

/// DUID type for full-rate voice (4V, four voice blocks: IMBE).
pub const DUID_4V_FULLRATE: u8 = 0;
/// DUID type for half-rate voice (2V, two voice blocks: AMBE+2).
pub const DUID_2V_HALFRATE: u8 = 6;

/// Bits in a single deinterleaved voice block.
pub const VOICE_BLOCK_BITS: usize = 72;

/// Slot bit offsets of the four voice blocks in full-rate (4V) mode.
pub const VOICE_OFFSETS_4V: [usize; 4] = [2, 76, 172, 246];

/// Slot bit offsets of the two voice blocks in half-rate (2V) mode.
pub const VOICE_OFFSETS_2V: [usize; 2] = [2, 76];

/// The voice blocks carried by a slot for a given DUID type (empty for non-voice).
pub fn voice_block_offsets(duid: u8) -> &'static [usize] {
    match duid {
        DUID_4V_FULLRATE => &VOICE_OFFSETS_4V,
        DUID_2V_HALFRATE => &VOICE_OFFSETS_2V,
        _ => &[],
    }
}

/// Extract the 72-bit voice block starting at `offset` within a 360-bit
/// descrambled slot, low-bits-as-received order (`block[0]` is `slot[offset]`).
pub fn extract_voice_block(slot: &[u8], offset: usize) -> [u8; VOICE_BLOCK_BITS] {
    let mut block = [0u8; VOICE_BLOCK_BITS];
    for (i, b) in block.iter_mut().enumerate() {
        *b = slot[offset + i] & 1;
    }
    block
}

/// All voice blocks for a slot, in signal order. Empty when `duid` is not a
/// voice-carrying slot. Blocks are still interleaved — run each through
/// [`super::deinterleave::deinterleave`] before the vocoder.
pub fn voice_blocks(slot: &[u8], duid: u8) -> Vec<[u8; VOICE_BLOCK_BITS]> {
    voice_block_offsets(duid)
        .iter()
        .map(|&o| extract_voice_block(slot, o))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::p25p2::duid::DUID_BIT_POSITIONS;
    use crate::p25p2::isch::ISCH_BIT_OFFSET;

    #[test]
    fn voice_blocks_do_not_overlap_duid_or_isch() {
        // Guard the slot layout: no voice bit may collide with a DUID bit, the
        // ISCH field, or fall outside the 360-bit slot.
        let duid_set: std::collections::HashSet<usize> =
            DUID_BIT_POSITIONS.iter().copied().collect();
        let mut voice_seen = std::collections::HashSet::new();
        for &off in &VOICE_OFFSETS_4V {
            for i in 0..VOICE_BLOCK_BITS {
                let pos = off + i;
                assert!(pos < 360, "voice bit {pos} out of range");
                assert!(
                    !duid_set.contains(&pos),
                    "voice bit {pos} collides with DUID"
                );
                assert!(
                    pos < ISCH_BIT_OFFSET,
                    "voice bit {pos} collides with ISCH field"
                );
                assert!(
                    voice_seen.insert(pos),
                    "voice bit {pos} duplicated across blocks"
                );
            }
        }
        assert_eq!(voice_seen.len(), 4 * VOICE_BLOCK_BITS);
    }

    #[test]
    fn half_rate_offsets_are_a_prefix_of_full_rate() {
        assert_eq!(&VOICE_OFFSETS_2V[..], &VOICE_OFFSETS_4V[..2]);
    }

    #[test]
    fn block_extraction_is_positional() {
        let mut slot = [0u8; 360];
        // Write a distinctive ramp into block 1 (offset 76).
        for i in 0..72 {
            slot[76 + i] = (i % 2) as u8;
        }
        let b = extract_voice_block(&slot, 76);
        for i in 0..72 {
            assert_eq!(b[i], (i % 2) as u8, "bit {i} mismatched");
        }
    }

    #[test]
    fn duid_selects_the_right_block_count() {
        let slot = [0u8; 360];
        assert_eq!(voice_blocks(&slot, DUID_4V_FULLRATE).len(), 4);
        assert_eq!(voice_blocks(&slot, DUID_2V_HALFRATE).len(), 2);
        assert_eq!(voice_blocks(&slot, 3).len(), 0); // SACCH, not voice
        assert_eq!(voice_blocks(&slot, 13).len(), 0); // MAC_SIGNAL / LCCH
    }
}
