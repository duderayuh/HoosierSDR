//! Voice LDU parsing: IMBE frame extraction and de-interleave into the
//! 8×23 codeword matrix consumed by the vocoder.
//!
//! Frame layout offsets are protocol facts (TIA-102.BAAA). The IMBE dibit
//! interleave schedule tables are taken from DSD-FME (`p25p1_const.h`, ISC
//! license, Copyright (C) 2010 DSD Author — see NOTICE).

use crate::bits::read_bits;
use crate::imbec::SoftImbeFrame;
use crate::soft::SoftDibit;

/// IMBE interleave schedule: transmitted dibit i places its MSB at
/// frame[IW[i]][IX[i]] and LSB at frame[IY[i]][IZ[i]].
#[rustfmt::skip]
const IW: [usize; 72] = [
    0, 2, 4, 1, 3, 5,  0, 2, 4, 1, 3, 6,  0, 2, 4, 1, 3, 6,
    0, 2, 4, 1, 3, 6,  0, 2, 4, 1, 3, 6,  0, 2, 4, 1, 3, 6,
    0, 2, 5, 1, 3, 6,  0, 2, 5, 1, 3, 6,  0, 2, 5, 1, 3, 7,
    0, 2, 5, 1, 3, 7,  0, 2, 5, 1, 4, 7,  0, 3, 5, 2, 4, 7,
];
#[rustfmt::skip]
const IX: [usize; 72] = [
    22, 20, 10, 20, 18, 0,  20, 18, 8, 18, 16, 13,  18, 16, 6, 16, 14, 11,
    16, 14, 4, 14, 12, 9,   14, 12, 2, 12, 10, 7,   12, 10, 0, 10, 8, 5,
    10, 8, 13, 8, 6, 3,     8, 6, 11, 6, 4, 1,      6, 4, 9, 4, 2, 6,
    4, 2, 7, 2, 0, 4,       2, 0, 5, 0, 13, 2,      0, 21, 3, 21, 11, 0,
];
#[rustfmt::skip]
const IY: [usize; 72] = [
    1, 3, 5, 0, 2, 4,  1, 3, 6, 0, 2, 4,  1, 3, 6, 0, 2, 4,
    1, 3, 6, 0, 2, 4,  1, 3, 6, 0, 2, 4,  1, 3, 6, 0, 2, 5,
    1, 3, 6, 0, 2, 5,  1, 3, 6, 0, 2, 5,  1, 3, 6, 0, 2, 5,
    1, 3, 7, 0, 2, 5,  1, 4, 7, 0, 3, 5,  2, 4, 7, 1, 3, 5,
];
#[rustfmt::skip]
const IZ: [usize; 72] = [
    21, 19, 1, 21, 19, 9,  19, 17, 14, 19, 17, 7,  17, 15, 12, 17, 15, 5,
    15, 13, 10, 15, 13, 3, 13, 11, 8, 13, 11, 1,   11, 9, 6, 11, 9, 14,
    9, 7, 4, 9, 7, 12,     7, 5, 2, 7, 5, 10,      5, 3, 0, 5, 3, 8,
    3, 1, 5, 3, 1, 6,      1, 14, 3, 1, 22, 4,     22, 12, 1, 22, 20, 2,
];

/// One de-interleaved IMBE voice codeword: 8 codewords × up to 23 bits,
/// exactly the `char imbe_fr[8][23]` layout mbelib consumes.
pub type ImbeFrame = [[u8; 23]; 8];

/// De-interleave 144 channel bits into the 8×23 codeword matrix.
pub fn deinterleave_imbe(bits144: &[u8]) -> ImbeFrame {
    assert_eq!(bits144.len(), 144);
    let mut fr = [[0u8; 23]; 8];
    for i in 0..72 {
        let msb = bits144[i * 2];
        let lsb = bits144[i * 2 + 1];
        fr[IW[i]][IX[i]] = msb;
        fr[IY[i]][IZ[i]] = lsb;
    }
    fr
}

/// Interleave an 8×23 matrix back to 144 channel bits (tests / synth).
pub fn interleave_imbe(fr: &ImbeFrame) -> [u8; 144] {
    let mut bits = [0u8; 144];
    for i in 0..72 {
        bits[i * 2] = fr[IW[i]][IX[i]];
        bits[i * 2 + 1] = fr[IY[i]][IZ[i]];
    }
    bits
}

/// Bit offsets of the nine 144-bit IMBE codewords within an LDU payload
/// (payload = frame bits after FS and NID, status symbols already removed;
/// total payload = 1568 bits).
pub const LDU_PAYLOAD_BITS: usize = 1568;
pub const IMBE_OFFSETS: [usize; 9] = [0, 144, 328, 512, 696, 880, 1064, 1248, 1424];

/// Extract the nine de-interleaved IMBE frames from an LDU payload.
pub fn extract_imbe_frames(payload_bits: &[u8]) -> Option<[ImbeFrame; 9]> {
    if payload_bits.len() < LDU_PAYLOAD_BITS {
        return None;
    }
    let mut out = [[[0u8; 23]; 8]; 9];
    for (k, &off) in IMBE_OFFSETS.iter().enumerate() {
        out[k] = deinterleave_imbe(&payload_bits[off..off + 144]);
    }
    Some(out)
}

/// De-interleave 72 soft dibits into a soft 8×23 frame, carrying per-bit
/// confidence through the exact schedule [`deinterleave_imbe`] uses.
pub fn soft_deinterleave_imbe(dibits: &[SoftDibit]) -> SoftImbeFrame {
    assert_eq!(dibits.len(), 72);
    let mut fr = SoftImbeFrame::default();
    for (i, d) in dibits.iter().enumerate() {
        fr.bits[IW[i]][IX[i]] = d.msb();
        fr.conf[IW[i]][IX[i]] = d.conf[0];
        fr.bits[IY[i]][IZ[i]] = d.lsb();
        fr.conf[IY[i]][IZ[i]] = d.conf[1];
    }
    fr
}

/// Extract the nine soft (bit + confidence) IMBE frames from an LDU payload
/// of soft dibits (status symbols already removed). The offset table is in bit
/// positions, so each frame is the 72 dibits at `IMBE_OFFSETS[k] / 2`.
pub fn soft_extract_imbe_frames(payload_dibits: &[SoftDibit]) -> Option<[SoftImbeFrame; 9]> {
    if payload_dibits.len() < LDU_PAYLOAD_BITS / 2 {
        return None;
    }
    let mut out = [SoftImbeFrame::default(); 9];
    for (k, &off) in IMBE_OFFSETS.iter().enumerate() {
        out[k] = soft_deinterleave_imbe(&payload_dibits[off / 2..off / 2 + 72]);
    }
    Some(out)
}

/// LDU2 Encryption Sync offsets: ALGID is the 72..80 bit range of the ES
/// hexbit payload. v1 extracts it without Hamming/RS correction (clear
/// channel assumption); robust ES decode is Phase 2 work. The gate treats
/// anything other than 0x80 as encrypted → skip audio.
pub fn ldu2_algid_raw(payload_bits: &[u8]) -> Option<u8> {
    // ES hexbits ride in the six 40-bit link-control slots between IMBE
    // frames 2..8 (offsets 288,472,656,840,1024,1208), 24 hexbits of 10 bits
    // each (Hamming(10,6)); data hexbit h occupies slot bits h*10..h*10+6.
    if payload_bits.len() < LDU_PAYLOAD_BITS {
        return None;
    }
    const SLOTS: [usize; 6] = [288, 472, 656, 840, 1024, 1208];
    let mut hexbits = Vec::with_capacity(24);
    for &s in &SLOTS {
        for j in 0..4 {
            let code = read_bits(payload_bits, s + j * 10, 10);
            hexbits.push(((code >> 4) & 0x3F) as u8);
        }
    }
    // ALGID = hexbits 12..13 → bits 72..80 of the 96-bit ES payload.
    Some((hexbits[12] << 2) | (hexbits[13] >> 4))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleave_roundtrip() {
        let mut fr: ImbeFrame = [[0u8; 23]; 8];
        // Fill valid bit positions with a pattern. Codewords 0-3 are 23 bits,
        // 4-6 are 15 bits, 7 is 7 bits — only touch positions the schedule uses.
        for i in 0..72 {
            fr[IW[i]][IX[i]] = ((i * 7) % 2) as u8;
            fr[IY[i]][IZ[i]] = ((i * 3 + 1) % 2) as u8;
        }
        let bits = interleave_imbe(&fr);
        assert_eq!(deinterleave_imbe(&bits), fr);
    }

    #[test]
    fn schedule_covers_all_144_positions_exactly_once() {
        let mut seen = std::collections::HashSet::new();
        for i in 0..72 {
            assert!(seen.insert((IW[i], IX[i])), "dup MSB at {}", i);
            assert!(seen.insert((IY[i], IZ[i])), "dup LSB at {}", i);
        }
        assert_eq!(seen.len(), 144);
    }
}
