//! P25 Phase 2 Data Unit ID (DUID) — the (8,4,4) code labelling each slot.
//!
//! # Provenance
//! The DUID is protected by an (8,4,4) extended-Hamming-style code: 4 data
//! bits + 4 parity bits, minimum distance 4, so the decoder corrects a single
//! bit error. The 16 codewords below are protocol **facts** from TIA-102.BBAB
//! (Phase 2 *Time Division Multiple Access*); they were derived from the code
//! structure, not copied from any GPL project's lookup table.

/// Bit positions of the 8 DUID bits within a 360-bit timeslot (MSB first).
pub const DUID_BIT_POSITIONS: [usize; 8] = [0, 1, 74, 75, 244, 245, 318, 319];

/// The 16 codewords of the DUID (8,4,4) code, indexed by DUID type (0..=15).
/// Bit 7 is the first transmitted DUID bit (slot bit 0); bit 0 is slot bit 319.
pub const DUID_CODEWORDS: [u8; 16] = [
    0b00000000, 0b00010111, 0b00101110, 0b00111001, 0b01001011, 0b01011100, 0b01100101, 0b01110010,
    0b10001101, 0b10011010, 0b10100011, 0b10110100, 0b11000110, 0b11010001, 0b11101000, 0b11111111,
];

/// Extract the 8 DUID bits from a 360-bit slot as an MSB-first word.
pub fn duid_word(slot: &[u8]) -> u8 {
    let mut w = 0u8;
    for &pos in &DUID_BIT_POSITIONS {
        w = (w << 1) | (slot[pos] & 1);
    }
    w
}

/// Decode a DUID word to its type (0..=15), correcting a single bit error.
/// Returns `None` when the word is more than one error from every codeword
/// (ambiguous or corrupt).
pub fn decode_duid(word: u8) -> Option<u8> {
    let mut best: Option<u8> = None;
    let mut best_dist = u32::MAX;
    for (ty, &cw) in DUID_CODEWORDS.iter().enumerate() {
        let d = (word ^ cw).count_ones();
        if d < best_dist {
            best_dist = d;
            best = Some(ty as u8);
        }
    }
    // Minimum distance 4 ⇒ radius-1 spheres are disjoint, so distance ≤ 1 has
    // a unique nearest codeword; anything further is ambiguous.
    if best_dist <= 1 {
        best
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_codewords_decode_to_their_type() {
        for (ty, &cw) in DUID_CODEWORDS.iter().enumerate() {
            assert_eq!(decode_duid(cw), Some(ty as u8), "codeword {ty}");
        }
    }

    #[test]
    fn single_bit_error_is_corrected() {
        for (ty, &cw) in DUID_CODEWORDS.iter().enumerate() {
            for b in 0..8 {
                assert_eq!(
                    decode_duid(cw ^ (1 << b)),
                    Some(ty as u8),
                    "type {ty}, bit {b}"
                );
            }
        }
    }

    #[test]
    fn two_bit_error_is_refused() {
        // Distance 2 from a codeword is ambiguous in a min-distance-4 code.
        let w = DUID_CODEWORDS[0] ^ 0b00000011;
        assert_eq!(decode_duid(w), None);
    }

    #[test]
    fn codewords_have_minimum_distance_four() {
        for (i, &a) in DUID_CODEWORDS.iter().enumerate() {
            for (j, &b) in DUID_CODEWORDS.iter().enumerate() {
                if i != j {
                    assert!(
                        (a ^ b).count_ones() >= 4,
                        "codewords {i} ({a:08b}) and {j} ({b:08b}) too close"
                    );
                }
            }
        }
    }
}
