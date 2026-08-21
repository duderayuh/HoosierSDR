//! P25 Phase 2 frame scrambler (TIA-102.BBAC figure 7.1).
//!
//! A 44-bit linear-feedback shift register whose state is seeded from the
//! network identity — WACN (20 bits), System ID (12 bits), and the NAC
//! (12 bits) — so every site produces a distinct scrambling sequence:
//!
//! ```text
//! seed = (WACN << 24) | (SysID << 12) | NAC
//! poly = x^44 + x^33 + x^19 + x^14 + x^8 + x^3 + 1
//! ```
//!
//! Scrambling is an XOR with a pseudorandom bit stream, so the same operation
//! both scrambles and descrambles. The sequence is applied to a superframe
//! starting at a slot-dependent offset (the 20-symbol sync is not scrambled).

/// The 44-bit LFSR state mask.
const LFSR_MASK: u64 = (1u64 << 44) - 1;

/// Derive the 44-bit scrambler seed from the network identity triple.
pub fn seed(wacn: u32, sysid: u32, nac: u32) -> u64 {
    ((wacn as u64) << 24) | ((sysid as u64) << 12) | (nac as u64)
}

/// Stateful 44-bit LFSR scrambler.
#[derive(Clone)]
pub struct P2Scrambler {
    state: u64,
}

impl P2Scrambler {
    /// Build a scrambler from the network identity triple.
    pub fn new(wacn: u32, sysid: u32, nac: u32) -> Self {
        Self::from_seed(seed(wacn, sysid, nac))
    }

    /// Build a scrambler from an explicit 44-bit seed (for tests / vectors).
    pub fn from_seed(seed: u64) -> Self {
        Self {
            state: seed & LFSR_MASK,
        }
    }

    /// Advance one step and return the output bit (bit 43 of the pre-shift
    /// state). The first 44 outputs are the seed itself, MSB-first.
    pub fn next_bit(&mut self) -> u8 {
        let out = ((self.state >> 43) & 1) as u8;
        let feedback = ((self.state >> 33)
            ^ (self.state >> 19)
            ^ (self.state >> 14)
            ^ (self.state >> 8)
            ^ (self.state >> 3)
            ^ (self.state >> 43))
            & 1;
        self.state = ((self.state << 1) | feedback) & LFSR_MASK;
        out
    }

    /// Produce the next `n` scramble bits.
    pub fn next_bits(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next_bit()).collect()
    }
}

/// Scramble (or descramble) a per-slot bit block in place.
///
/// `slot` is the timeslot index (0..12); the scramble stream is offset by the
/// 20-symbol sync plus `slot * SLOT_BITS` to align with the superframe, per
/// BBAC. The leading `SYNC_BITS` of the slot are left untouched (they are
/// transmitted unscrambled).
pub fn scramble_slot(wacn: u32, sysid: u32, nac: u32, slot: usize, bits: &mut [u8]) {
    let mut lfsr = P2Scrambler::new(wacn, sysid, nac);
    // Advance past the sync and any earlier slots.
    let skip = crate::p25p2::SYNC_BITS + slot * crate::p25p2::SLOT_BITS;
    lfsr.next_bits(skip);
    for bit in bits.iter_mut().skip(crate::p25p2::SYNC_BITS) {
        *bit ^= lfsr.next_bit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SAFE-T identity (WACN 0xBEE00, SysID 0x6BD, NAC 0x261), from
    /// docs/ARCHITECTURE.md. First 64 scramble bits, independently computed
    /// against the BBAC figure 7.1 LFSR.
    const SAFET_64_BITS: &str = "bee006bd26107e32";

    fn bits_to_hex(bits: &[u8]) -> String {
        bits.chunks(4)
            .map(|c| {
                let v = c.iter().fold(0u8, |a, &b| (a << 1) | b);
                char::from_digit(v as u32, 16).unwrap()
            })
            .collect()
    }

    #[test]
    fn first_64_bits_match_spec_vector() {
        let mut lfsr = P2Scrambler::new(0xBEE00, 0x6BD, 0x261);
        let bits = lfsr.next_bits(64);
        assert_eq!(bits_to_hex(&bits), SAFET_64_BITS);
    }

    #[test]
    fn first_44_bits_are_the_seed_msb_first() {
        // The LFSR reads its state out MSB-first before any feedback bit
        // reaches the output, so the first 44 bits must equal the seed.
        let wacn = 0xBEE00u32;
        let sysid = 0x6BDu32;
        let nac = 0x261u32;
        let mut lfsr = P2Scrambler::new(wacn, sysid, nac);
        let bits = lfsr.next_bits(44);
        let s = seed(wacn, sysid, nac);
        let expected = (0..44)
            .rev()
            .map(|i| ((s >> i) & 1) as u8)
            .collect::<Vec<_>>();
        assert_eq!(bits, expected);
    }

    #[test]
    fn scramble_is_an_involution() {
        let wacn = 0xBEE00u32;
        let sysid = 0x6BDu32;
        let nac = 0x261u32;
        let mut block = (0..super::super::super::p25p2::SLOT_BITS as u8)
            .map(|i| i & 1)
            .collect::<Vec<_>>();
        let original = block.clone();
        scramble_slot(wacn, sysid, nac, 0, &mut block);
        assert_ne!(block, original, "scrambling must change the block");
        scramble_slot(wacn, sysid, nac, 0, &mut block);
        assert_eq!(block, original, "scrambling twice must restore the block");
    }

    #[test]
    fn sync_is_not_scrambled() {
        let mut block = vec![1u8; crate::p25p2::SLOT_BITS];
        scramble_slot(0xBEE00, 0x6BD, 0x261, 0, &mut block);
        // First SYNC_BITS are untouched (still 1); rest are scrambled.
        assert!(block[..crate::p25p2::SYNC_BITS].iter().all(|&b| b == 1));
    }
}
