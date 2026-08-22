//! P25 Phase 2 ISCH (Inter-slot Signaling Channel) — the 40-bit word at the
//! tail of every slot that carries slot-type status and superframe position.
//!
//! # Provenance
//! The ISCH is a (40,7,16) code: 7 data bits → 40 code bits, minimum distance
//! 16, so the decoder corrects up to 7 bit errors. The 128 codewords are
//! protocol **facts** from TIA-102.BBAB, cross-checked against the ISC-licensed
//! `dsd-fme` reference — not copied from any GPL source.

/// Bit offset of the 40-bit ISCH within a 360-bit slot: the last 40 bits.
pub const ISCH_BIT_OFFSET: usize = 320;
pub const ISCH_BITS: usize = 40;

/// The S-ISCH idle word — a slot carrying no voice or data. Not a codeword;
/// it is the special "idle" marker the frame synchronizer recognizes first.
pub const ISCH_IDLE: u64 = 0x575D57F7FF;

/// Decoded ISCH fields (7 data bits).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Isch {
    /// Unfinished-frame count (0..=3).
    pub uf_count: u8,
    /// Free / idle flag.
    pub free: bool,
    /// ISCH location (0..=3) — position of this slot's ISCH in the cycle.
    pub isch_loc: u8,
    /// Channel number (0..=3) this slot's traffic belongs to.
    pub chan_num: u8,
}

/// The 128 codewords of the (40,7,16) ISCH code, indexed by the packed 7-bit
/// data value `(chan_num << 5) | (isch_loc << 3) | (free << 2) | uf_count`.
pub const ISCH_CODEWORDS: [u64; 128] = [
    0x184229D461,
    0x18761451F6,
    0x181AE27E2F,
    0x182EDFFBB8,
    0x18DF8A7510,
    0x18EBB7F087,
    0x188741DF5E,
    0x18B37C5AC9,
    0x1146A44F13,
    0x117299CA84,
    0x111E6FE55D,
    0x112A5260CA,
    0x11DB07EE62,
    0x11EF3A6BF5,
    0x1183CC442C,
    0x11B7F1C1BB,
    0x1A4A2E239E,
    0x1A7E13A609,
    0x1A12E589D0,
    0x1A26D80C47,
    0x1AD78D82EF,
    0x1AE3B00778,
    0x1A8F4628A1,
    0x1ABB7BAD36,
    0x134EA3B8EC,
    0x137A9E3D7B,
    0x13166812A2,
    0x1322559735,
    0x13D300199D,
    0x13E73D9C0A,
    0x138BCBB3D3,
    0x13BFF63644,
    0x1442F705EF,
    0x1476CA8078,
    0x141A3CAFA1,
    0x142E012A36,
    0x14DF54A49E,
    0x14EB692109,
    0x14879F0ED0,
    0x14B3A28B47,
    0x1D467A9E9D,
    0x1D72471B0A,
    0x1D1EB134D3,
    0x1D2A8CB144,
    0x1DDBD93FEC,
    0x1DEFE4BA7B,
    0x1D831295A2,
    0x1DB72F1035,
    0x164AF0F210,
    0x167ECD7787,
    0x16123B585E,
    0x162606DDC9,
    0x16D7535361,
    0x16E36ED6F6,
    0x168F98F92F,
    0x16BBA57CB8,
    0x1F4E7D6962,
    0x1F7A40ECF5,
    0x1F16B6C32C,
    0x1F228B46BB,
    0x1FD3DEC813,
    0x1FE7E34D84,
    0x1F8B15625D,
    0x1FBF28E7CA,
    0x084D62C339,
    0x08795F46AE,
    0x0815A96977,
    0x082194ECE0,
    0x08D0C16248,
    0x08E4FCE7DF,
    0x08880AC806,
    0x08BC374D91,
    0x0149EF584B,
    0x017DD2DDDC,
    0x011124F205,
    0x0125197792,
    0x01D44CF93A,
    0x01E0717CAD,
    0x018C875374,
    0x01B8BAD6E3,
    0x0A456534C6,
    0x0A7158B151,
    0x0A1DAE9E88,
    0x0A29931B1F,
    0x0AD8C695B7,
    0x0AECFB1020,
    0x0A800D3FF9,
    0x0AB430BA6E,
    0x0341E8AFB4,
    0x0375D52A23,
    0x03192305FA,
    0x032D1E806D,
    0x03DC4B0EC5,
    0x03E8768B52,
    0x038480A48B,
    0x03B0BD211C,
    0x044DBC12B7,
    0x0479819720,
    0x041577B8F9,
    0x04214A3D6E,
    0x04D01FB3C6,
    0x04E4223651,
    0x0488D41988,
    0x04BCE99C1F,
    0x0D493189C5,
    0x0D7D0C0C52,
    0x0D11FA238B,
    0x0D25C7A61C,
    0x0DD49228B4,
    0x0DE0AFAD23,
    0x0D8C5982FA,
    0x0DB864076D,
    0x0645BBE548,
    0x06718660DF,
    0x061D704F06,
    0x06294DCA91,
    0x06D8184439,
    0x06EC25C1AE,
    0x0680D3EE77,
    0x06B4EE6BE0,
    0x0F41367E3A,
    0x0F750BFBAD,
    0x0F19FDD474,
    0x0F2DC051E3,
    0x0FDC95DF4B,
    0x0FE8A85ADC,
    0x0F845E7505,
    0x0FB063F092,
];

/// Extract the 40-bit ISCH from a 360-bit slot as an MSB-first `u64`.
pub fn isch_word(slot: &[u8]) -> u64 {
    let mut w = 0u64;
    for i in 0..ISCH_BITS {
        w = (w << 1) | (slot[ISCH_BIT_OFFSET + i] & 1) as u64;
    }
    w
}

/// Decode a 40-bit ISCH word, correcting up to 7 bit errors. Returns `None`
/// for the idle word or a word too corrupted to place within that radius.
pub fn decode_isch(word: u64) -> Option<Isch> {
    let mut best: Option<usize> = None;
    let mut best_dist = u32::MAX;
    for (i, &cw) in ISCH_CODEWORDS.iter().enumerate() {
        let d = (word ^ cw).count_ones();
        if d < best_dist {
            best_dist = d;
            best = Some(i);
        }
    }
    // Minimum distance 16 corrects up to 7 errors.
    if best_dist <= 7 {
        let v = best.unwrap();
        Some(Isch {
            uf_count: (v & 0x3) as u8,
            free: ((v >> 2) & 0x1) != 0,
            isch_loc: ((v >> 3) & 0x3) as u8,
            chan_num: ((v >> 5) & 0x3) as u8,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codewords_decode_to_their_fields() {
        for (v, &cw) in ISCH_CODEWORDS.iter().enumerate() {
            let isch = decode_isch(cw).expect("valid codeword");
            assert_eq!(isch.uf_count as usize, v & 0x3, "value {v} uf_count");
            assert_eq!(isch.free, ((v >> 2) & 0x1) != 0, "value {v} free");
            assert_eq!(isch.isch_loc as usize, (v >> 3) & 0x3, "value {v} loc");
            assert_eq!(isch.chan_num as usize, (v >> 5) & 0x3, "value {v} chan");
        }
    }

    #[test]
    fn seven_bit_errors_are_corrected() {
        // Flip seven bits of codeword 0; it must still decode to value 0.
        let mut w = ISCH_CODEWORDS[0];
        for b in [0u32, 5, 11, 17, 23, 29, 35] {
            w ^= 1u64 << b;
        }
        let isch = decode_isch(w).expect("7 errors correctable");
        assert_eq!(isch.uf_count, 0);
        assert!(!isch.free);
    }

    #[test]
    fn idle_word_is_not_a_codeword() {
        assert_eq!(decode_isch(ISCH_IDLE), None);
    }

    #[test]
    fn codewords_have_minimum_distance_sixteen() {
        for i in 0..128 {
            for j in 0..128 {
                if i != j {
                    assert!(
                        (ISCH_CODEWORDS[i] ^ ISCH_CODEWORDS[j]).count_ones() >= 16,
                        "codewords {i} and {j} too close"
                    );
                }
            }
        }
    }
}
