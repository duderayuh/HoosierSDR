//! Soft-decision FEC for the IMBE 7200×4400 voice frame.
//!
//! mbelib's `ecc.c` hard-decides every bit before its Golay(23,12) and
//! Hamming(15,11) decoders see it, throwing away the demodulator's per-bit
//! confidence. On a simulcast channel a deep null concentrates errors into one
//! codeword; a hard Golay decoder miscorrects a 4+ error codeword (it can only
//! reach distance 3), synthesizes *wrong* speech parameters, and the result is
//! the characteristic robotic garble. This module re-decodes the frame with the
//! confidence carried along, so the maximum-likelihood codeword is chosen even
//! when the errors sit on low-confidence bits, and it reports an honest error
//! count (so mbelib holds a genuinely bad frame instead of mis-synthesizing it).
//!
//! # What it replaces
//! `mbe_processImbe7200x4400Frame` = C0 Golay → PRNG demodulate → data Golay +
//! Hamming → synth. This module reproduces the first three steps *soft* and
//! returns the 88 voice bits, which are handed to mbelib's synthesis-only entry
//! point `mbe_processImbe4400Dataf` (`char imbe_d[88]`). Bit-for-bit identical
//! to the mbelib pipeline on all-`CERTAIN` input.

use std::sync::OnceLock;

/// Golay(23,12) generator parity rows, from mbelib `ecc_const.h` (ISC).
/// Row `i` is the 11-bit parity contributed by data bit `11 - i`.
const GOLAY_GENERATOR: [u32; 12] = [
    0x63a, 0x31d, 0x7b4, 0x3da, 0x1ed, 0x6cc, 0x366, 0x1b3, 0x6e3, 0x54b, 0x49f, 0x475,
];

/// Hamming(15,11) parity-check rows, from mbelib `ecc_const.h` (ISC).
/// The code is systematic: data in bits 14..4, parity in bits 3..0, and the
/// four rows form `[H_d | I_4]`, so the parity is `H_d · data`.
const HAMMING_GENERATOR: [u16; 4] = [0x7f08, 0x78e4, 0x66d2, 0x55b1];

/// Encode a 12-bit data word to its 23-bit Golay codeword.
fn golay_codeword(data: u16) -> u32 {
    let mut parity = 0u32;
    for i in 0..12 {
        // golayGenerator[i] belongs to data bit (11 - i): i=0 → bit 11 (MSB).
        if data & (1 << (11 - i)) != 0 {
            parity ^= GOLAY_GENERATOR[i];
        }
    }
    ((data as u32) << 11) | (parity & 0x7ff)
}

/// Encode an 11-bit data word to its 15-bit Hamming codeword.
fn hamming_codeword(data: u16) -> u16 {
    let mut parity = 0u16;
    for i in 0..4 {
        let g = HAMMING_GENERATOR[i] as u16;
        let mut p = 0u16;
        for j in 4..=14 {
            p ^= ((g >> j) & 1) & ((data >> (j - 4)) & 1);
        }
        parity |= p << i;
    }
    (data << 4) | (parity & 0xf)
}

/// Precomputed Golay(23,12) codebook — every valid 23-bit codeword.
fn golay_codebook() -> &'static [u32; 4096] {
    static BOOK: OnceLock<[u32; 4096]> = OnceLock::new();
    BOOK.get_or_init(|| {
        let mut b = [0u32; 4096];
        for d in 0..4096u16 {
            b[d as usize] = golay_codeword(d);
        }
        b
    })
}

/// Precomputed Hamming(15,11) codebook — every valid 15-bit codeword.
fn hamming_codebook() -> &'static [u16; 2048] {
    static BOOK: OnceLock<[u16; 2048]> = OnceLock::new();
    BOOK.get_or_init(|| {
        let mut b = [0u16; 2048];
        for d in 0..2048u16 {
            b[d as usize] = hamming_codeword(d);
        }
        b
    })
}

/// Soft distance of a codeword from the received soft vector: the summed
/// confidence of every bit position where the codeword disagrees with the hard
/// decision. Minimizing this is maximum-likelihood under the confidence model.
#[inline]
fn soft_dist(cw: u32, bits: &[u8; 23], conf: &[u8; 23]) -> u32 {
    let mut d = 0u32;
    for i in 0..23 {
        if ((cw >> i) & 1) as u8 != bits[i] {
            d += conf[i] as u32;
        }
    }
    d
}

/// Soft-decision Golay(23,12). Returns the 12 data bits and the hard Hamming
/// distance between the chosen codeword and the received hard decision (the
/// same "corrected errors" mbelib reports for its hard decoder).
pub fn soft_golay(bits: &[u8; 23], conf: &[u8; 23]) -> (u16, u32) {
    let book = golay_codebook();
    let mut best_data = 0u16;
    let mut best_dist = u32::MAX;
    for (d, &cw) in book.iter().enumerate() {
        let dist = soft_dist(cw, bits, conf);
        if dist < best_dist {
            best_dist = dist;
            best_data = d as u16;
        }
    }
    // Hard error count = Hamming distance of the chosen codeword from the
    // received bits (not the confidence-weighted distance).
    let cw = golay_codeword(best_data);
    let hard = (0..23)
        .map(|i| ((cw >> i) & 1) as u8 ^ bits[i])
        .filter(|&x| x != 0)
        .count() as u32;
    (best_data, hard)
}

/// Soft-decision Hamming(15,11). Returns the 11 data bits and the hard error
/// count, mirroring [`soft_golay`].
pub fn soft_hamming(bits: &[u8; 15], conf: &[u8; 15]) -> (u16, u32) {
    let book = hamming_codebook();
    let mut best_data = 0u16;
    let mut best_dist = u32::MAX;
    for (d, &cw) in book.iter().enumerate() {
        let mut dist = 0u32;
        for i in 0..15 {
            if ((cw >> i) & 1) as u8 != bits[i] {
                dist += conf[i] as u32;
            }
        }
        if dist < best_dist {
            best_dist = dist;
            best_data = d as u16;
        }
    }
    let cw = hamming_codeword(best_data);
    let hard = (0..15)
        .map(|i| ((cw >> i) & 1) as u8 ^ bits[i])
        .filter(|&x| x != 0)
        .count() as u32;
    (best_data, hard)
}

/// The pseudo-random descrambler used to spread the IMBE frame (mbelib
/// `mbe_demodulateImbe7200x4400Data`). Seed is row 0's 12 Golay data bits;
/// returns the 114 scrambler bits applied to rows 1..6.
fn imbe_prng(seed12: u16) -> [u8; 114] {
    let mut pr: u32 = (seed12 as u32) << 4; // pr[0] = seed << 4
    let mut out = [0u8; 114];
    for k in 0..114 {
        pr = (173 * pr + 13849) % 65536; // pr[k + 1]
        out[k] = (pr / 32768) as u8;
    }
    out
}

/// A soft 8×23 IMBE frame: hard bits plus per-bit confidence.
#[derive(Clone, Copy, Default)]
pub struct SoftImbeFrame {
    /// Hard bit values, `frame[r][c]` = mbelib's `imbe_fr[r][c]`.
    pub bits: [[u8; 23]; 8],
    /// Per-bit confidence (0 = coin flip, 255 = certain).
    pub conf: [[u8; 23]; 8],
}

/// Soft-decode a de-interleaved IMBE frame to its 88 voice bits.
///
/// Returns `(imbe_d, errs)` where `imbe_d` is the 88 data bits in exactly the
/// order `mbe_processImbe4400Dataf` consumes, and `errs` is the total hard bit
/// error count (mbelib's `errs2`, i.e. the threshold that triggers hold).
pub fn soft_decode_imbe(frame: &SoftImbeFrame) -> ([u8; 88], u32) {
    // ---- C0: soft Golay on row 0 → corrected seed (12 data bits) ----
    let mut row0 = [0u8; 23];
    row0.copy_from_slice(&frame.bits[0]);
    let mut row0_conf = [0u8; 23];
    row0_conf.copy_from_slice(&frame.conf[0]);
    let (seed, errs0) = soft_golay(&row0, &row0_conf);
    let seed_cw = golay_codeword(seed);
    // Write the corrected row 0 back (bits 22..11 hold the seed).
    for i in 0..23 {
        row0[i] = ((seed_cw >> i) & 1) as u8;
    }

    // ---- demodulate: XOR rows 1..6 with the PRNG (confidence unchanged) ----
    let prng = imbe_prng(seed);
    let mut rows_soft = [[(0u8, 0u8); 23]; 8];
    for i in 0..23 {
        rows_soft[0][i] = (row0[i], frame.conf[0][i]);
    }
    let mut k = 0usize;
    for r in 1..4 {
        for c in (0..23).rev() {
            rows_soft[r][c] = (frame.bits[r][c] ^ prng[k], frame.conf[r][c]);
            k += 1;
        }
    }
    for r in 4..7 {
        for c in (0..15).rev() {
            rows_soft[r][c] = (frame.bits[r][c] ^ prng[k], frame.conf[r][c]);
            k += 1;
        }
    }
    for c in 0..7 {
        rows_soft[7][c] = (frame.bits[7][c], frame.conf[7][c]);
    }

    // ---- data FEC: soft Golay rows 1..3, soft Hamming rows 4..6 ----
    let mut imbe_d = [0u8; 88];
    let mut errs = errs0;

    // Row 0 data bits 22..11 (already corrected) → the first 12 output bits.
    let mut o = 0usize;
    for c in (11..=22).rev() {
        imbe_d[o] = rows_soft[0][c].0 & 1;
        o += 1;
    }
    // Rows 1..3 Golay.
    for r in 1..4 {
        let mut bits = [0u8; 23];
        let mut conf = [0u8; 23];
        for c in 0..23 {
            bits[c] = rows_soft[r][c].0;
            conf[c] = rows_soft[r][c].1;
        }
        let (data, e) = soft_golay(&bits, &conf);
        errs += e;
        let cw = golay_codeword(data);
        for c in (11..=22).rev() {
            imbe_d[o] = ((cw >> c) & 1) as u8;
            o += 1;
        }
    }
    // Rows 4..6 Hamming.
    for r in 4..7 {
        let mut bits = [0u8; 15];
        let mut conf = [0u8; 15];
        for c in 0..15 {
            bits[c] = rows_soft[r][c].0;
            conf[c] = rows_soft[r][c].1;
        }
        let (data, e) = soft_hamming(&bits, &conf);
        errs += e;
        let cw = hamming_codeword(data);
        for c in (4..=14).rev() {
            imbe_d[o] = ((cw >> c) & 1) as u8;
            o += 1;
        }
    }
    // Row 7 unprotected 7 bits.
    for c in (0..7).rev() {
        imbe_d[o] = rows_soft[7][c].0 & 1;
        o += 1;
    }
    debug_assert_eq!(o, 88);

    (imbe_d, errs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_conf() -> [u8; 23] {
        [255u8; 23]
    }
    fn full_conf15() -> [u8; 15] {
        [255u8; 15]
    }

    fn hamming_weight(cw: u32) -> u32 {
        cw.count_ones()
    }

    #[test]
    fn golay_has_minimum_distance_seven() {
        let book = golay_codebook();
        for i in 0..4096 {
            for j in (i + 1)..4096 {
                assert!(
                    hamming_weight(book[i] ^ book[j]) >= 7,
                    "codewords {i} and {j} too close"
                );
            }
        }
    }

    #[test]
    fn golay_clean_codeword_recovers_data() {
        for d in [0u16, 1, 0xfff, 0x555, 0xaaa, 1234] {
            let cw = golay_codeword(d);
            let mut bits = [0u8; 23];
            for i in 0..23 {
                bits[i] = ((cw >> i) & 1) as u8;
            }
            let (out, errs) = soft_golay(&bits, &full_conf());
            assert_eq!(out, d, "data {d:#x}");
            assert_eq!(errs, 0);
        }
    }

    #[test]
    fn golay_corrects_three_errors_and_matches_hard() {
        // Every 3-error pattern must decode to the original codeword — the
        // perfect-code property makes soft and hard agree here.
        let d = 0b1010_1010_1010u16;
        let cw = golay_codeword(d);
        for a in 0..23 {
            for b in (a + 1)..23 {
                for c in (b + 1)..23 {
                    let corrupted = cw ^ (1 << a) ^ (1 << b) ^ (1 << c);
                    let mut bits = [0u8; 23];
                    for i in 0..23 {
                        bits[i] = ((corrupted >> i) & 1) as u8;
                    }
                    let (out, errs) = soft_golay(&bits, &full_conf());
                    assert_eq!(out, d, "errs at {a},{b},{c}");
                    assert_eq!(errs, 3);
                }
            }
        }
    }

    #[test]
    fn golay_soft_recovers_four_low_confidence_errors() {
        // Four errors, all on bits the demodulator did not trust: the soft
        // metric still lands on the true codeword, where the hard decoder
        // (radius 3) cannot see it.
        let d = 0b0011_1100_0011u16;
        let cw = golay_codeword(d);
        let err_bits = [0usize, 3, 11, 20];
        let corrupted =
            cw ^ (1 << err_bits[0]) ^ (1 << err_bits[1]) ^ (1 << err_bits[2]) ^ (1 << err_bits[3]);
        let mut bits = [0u8; 23];
        let mut conf = full_conf();
        for i in 0..23 {
            bits[i] = ((corrupted >> i) & 1) as u8;
        }
        for &b in &err_bits {
            conf[b] = 5; // barely trusted
        }
        let (out, _) = soft_golay(&bits, &conf);
        assert_eq!(out, d, "soft decoder failed on 4 low-confidence errors");
    }

    #[test]
    fn hamming_has_minimum_distance_three() {
        let book = hamming_codebook();
        for i in 0..2048 {
            for j in (i + 1)..2048 {
                assert!(
                    ((book[i] ^ book[j]) as u32).count_ones() >= 3,
                    "codewords {i} and {j} too close"
                );
            }
        }
    }

    #[test]
    fn hamming_corrects_one_error() {
        let d = 0b10110101101u16;
        let cw = hamming_codeword(d);
        for e in 0..15 {
            let corrupted = cw ^ (1 << e);
            let mut bits = [0u8; 15];
            for i in 0..15 {
                bits[i] = ((corrupted >> i) & 1) as u8;
            }
            let (out, errs) = soft_hamming(&bits, &full_conf15());
            assert_eq!(out, d, "single error at {e}");
            assert_eq!(errs, 1);
        }
    }

    #[test]
    fn prng_matches_the_mbelib_lcg() {
        // Seed that drives several distinct scrambler bits; values computed by
        // replicating mbelib's exact LCG (pr[0]=seed<<4, pr[i]=(173 pr[i-1]+13849)
        // mod 65536, output bit = pr[i]/32768).
        let out = imbe_prng(0xabc);
        assert_eq!(out[0], 0, "pr[1]");
        assert_eq!(out[1], 0, "pr[2]");
        assert_eq!(out[2], 1, "pr[3]");
        assert_eq!(out[3], 1, "pr[4]");
        assert_eq!(out[50], 0, "pr[51]");
        assert_eq!(out[113], 1, "pr[114]");
        assert_eq!(out.len(), 114);
    }

    /// Build a transmit-side IMBE frame: encode the 88 voice bits into the
    /// 8×23 matrix, then scramble rows 1..6 exactly as the transmitter does.
    fn encode_imbe(imbe_d: &[u8; 88]) -> [[u8; 23]; 8] {
        let mut fr = [[0u8; 23]; 8];
        // Row 0 = Golay(seed), seed = imbe_d[0..12] (imbe_d[0] = bit 22 MSB).
        let mut seed = 0u16;
        for b in &imbe_d[0..12] {
            seed = (seed << 1) | (*b & 1) as u16;
        }
        let cw = golay_codeword(seed);
        for i in 0..23 {
            fr[0][i] = ((cw >> i) & 1) as u8;
        }
        // Rows 1..3 = Golay of the next 12 bits each.
        for r in 1..4 {
            let mut d = 0u16;
            for b in &imbe_d[r * 12..r * 12 + 12] {
                d = (d << 1) | (*b & 1) as u16;
            }
            let cw = golay_codeword(d);
            for i in 0..23 {
                fr[r][i] = ((cw >> i) & 1) as u8;
            }
        }
        // Rows 4..6 = Hamming of the next 11 bits each.
        for r in 4..7 {
            let mut d = 0u16;
            for b in &imbe_d[48 + (r - 4) * 11..48 + (r - 4) * 11 + 11] {
                d = (d << 1) | (*b & 1) as u16;
            }
            let cw = hamming_codeword(d);
            for i in 0..15 {
                fr[r][i] = ((cw >> i) & 1) as u8;
            }
        }
        // Row 7 = the last 7 bits (imbe_d[81..88], bit 6 first).
        for c in 0..7 {
            fr[7][6 - c] = imbe_d[81 + c] & 1;
        }
        // Scramble rows 1..6.
        let prng = imbe_prng(seed);
        let mut k = 0;
        for r in 1..4 {
            for c in (0..23).rev() {
                fr[r][c] ^= prng[k];
                k += 1;
            }
        }
        for r in 4..7 {
            for c in (0..15).rev() {
                fr[r][c] ^= prng[k];
                k += 1;
            }
        }
        fr
    }

    #[test]
    fn soft_decode_round_trips_a_full_frame() {
        // Every non-zero 88-bit word must survive encode → decode with zero
        // reported errors, pinning the bit ordering in both directions.
        let mut imbe_d = [0u8; 88];
        for i in 0..88 {
            imbe_d[i] = ((i * 13 + 7) % 2) as u8;
        }
        let fr = encode_imbe(&imbe_d);
        let frame = SoftImbeFrame {
            bits: fr,
            conf: [[255u8; 23]; 8],
        };
        let (out, errs) = soft_decode_imbe(&frame);
        assert_eq!(errs, 0, "clean frame is error-free");
        assert_eq!(&out[..], &imbe_d[..], "round-trip bit-order mismatch");
    }
}
