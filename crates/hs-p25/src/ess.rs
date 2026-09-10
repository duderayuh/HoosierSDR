//! Encryption Sync from an LDU2: the field that says whether a transmission
//! is encrypted, decoded through its full protection.
//!
//! Protocol facts (TIA-102.BAAA): the 96-bit Encryption Sync — Message
//! Indicator (72), ALGID (8), KID (16) — is 16 data hexbits followed by 8
//! Reed–Solomon parity hexbits, an RS(24,16,9) codeword over GF(64). Each of
//! the 24 hexbits is transmitted as 10 bits, 6 of data and a Hamming(10,6,3)
//! tail, in the six 40-bit slots between IMBE frames 2..8 of the LDU2. The
//! first transmitted hexbit is the highest-degree coefficient of the RS
//! codeword and the last parity hexbit is the constant term.
//!
//! Reading ALGID raw — as this project did at first — turned every bit
//! error in an unprotected 8-bit field into a muted transmission: one flip
//! read as "encrypted" and, because that verdict was sticky, silenced every
//! following LDU1 of the call. With the Hamming and Reed–Solomon layers
//! decoded, an ESS either validates (up to 4 hexbits wrong after the Hamming
//! pass) and is trusted, or fails and says nothing — a corrupt field carries
//! no information and must not be mistaken for a verdict.

use crate::lc::hamming;
use crate::rs::{ReedSolomon, RsResult};
use crate::voice::LDU_PAYLOAD_BITS;

/// Bit offset of each of the 24 ESS hexbits in an LDU2 payload, in
/// transmission order: four 10-bit hexbits per 40-bit slot.
pub const ESS_HEXBIT_OFFSETS: [usize; 24] = [
    288, 298, 308, 318, 472, 482, 492, 502, 656, 666, 676, 686, 840, 850, 860, 870, 1024, 1034,
    1044, 1054, 1208, 1218, 1228, 1238,
];

/// Length of the ESS in hexbits, and how many of them are data.
const ESS_HEXBITS: usize = 24;
const ESS_DATA_HEXBITS: usize = 16;

/// ALGID for an unencrypted transmission.
pub const ALGID_CLEAR: u8 = 0x80;

/// A validated Encryption Sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ess {
    /// Message Indicator (crypto sync), 72 bits.
    pub mi: [u8; 9],
    /// Encryption algorithm; [`ALGID_CLEAR`] means none.
    pub algid: u8,
    /// Key ID.
    pub kid: u16,
}

impl Ess {
    pub fn is_clear(&self) -> bool {
        self.algid == ALGID_CLEAR
    }
}

/// What one LDU2's ESS field yielded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EssDecode {
    /// The field, when the Reed–Solomon check passed. `None` means the
    /// field was too damaged to trust — *not* that the call is encrypted.
    pub ess: Option<Ess>,
    /// ALGID as read after Hamming correction alone, for callers with no
    /// better evidence that want to confirm it by repetition.
    pub algid_hamming: u8,
    /// Hexbits whose Hamming code could not settle them (distance > 1).
    pub hamming_doubtful: u32,
    /// Hexbits the Reed–Solomon layer changed, when it validated.
    pub rs_corrected: u32,
}

/// Decode the Encryption Sync of an LDU2 payload (status symbols removed).
/// `None` only when the payload is too short to hold one.
pub fn decode_ess(payload_bits: &[u8]) -> Option<EssDecode> {
    if payload_bits.len() < LDU_PAYLOAD_BITS {
        return None;
    }
    // Inner code: settle each hexbit with its Hamming tail. A hexbit the
    // Hamming code cannot repair is kept as its nearest codeword and left for
    // the outer code, which is what the outer code is for.
    let mut hexbits = [0u8; ESS_HEXBITS];
    let mut doubtful = 0u32;
    for (h, &off) in ESS_HEXBIT_OFFSETS.iter().enumerate() {
        let mut cw = 0u16;
        for b in 0..10 {
            cw = (cw << 1) | payload_bits[off + b] as u16;
        }
        let (data, dist) = hamming::decode_best(cw);
        if dist > 1 {
            doubtful += 1;
        }
        hexbits[h] = data;
    }
    let algid_hamming = (hexbits[12] << 2) | (hexbits[13] >> 4);

    // Outer code: transmission order is highest degree first, so hexbit h is
    // the coefficient of x^(23−h) and the eight parity hexbits land in
    // positions 0..8 as the code expects.
    let mut word = [0u8; ESS_HEXBITS];
    for (h, &v) in hexbits.iter().enumerate() {
        word[ESS_HEXBITS - 1 - h] = v;
    }
    let rs = rs_24_16();
    let ess = match rs.decode(&mut word) {
        RsResult::Uncorrectable => None,
        RsResult::Corrected(n) => {
            let mut bits = [0u8; 96];
            for h in 0..ESS_DATA_HEXBITS {
                let v = word[ESS_HEXBITS - 1 - h];
                for b in 0..6 {
                    bits[h * 6 + b] = (v >> (5 - b)) & 1;
                }
            }
            let mut mi = [0u8; 9];
            for (i, m) in mi.iter_mut().enumerate() {
                *m = crate::bits::read_bits(&bits, i * 8, 8) as u8;
            }
            let algid = crate::bits::read_bits(&bits, 72, 8) as u8;
            let kid = crate::bits::read_bits(&bits, 80, 16) as u16;
            Some((Ess { mi, algid, kid }, n as u32))
        }
    };
    Some(EssDecode {
        ess: ess.map(|(e, _)| e),
        algid_hamming,
        hamming_doubtful: doubtful,
        rs_corrected: ess.map(|(_, n)| n).unwrap_or(0),
    })
}

fn rs_24_16() -> &'static ReedSolomon {
    static RS: std::sync::OnceLock<ReedSolomon> = std::sync::OnceLock::new();
    RS.get_or_init(ReedSolomon::new_24_16)
}

/// Write a complete, correctly coded Encryption Sync into an LDU2 payload
/// (for the synthesizer and tests): RS parity from the 16 data hexbits, then
/// a Hamming tail on every hexbit.
pub fn write_ess(payload_bits: &mut [u8], ess: &Ess) {
    assert!(payload_bits.len() >= LDU_PAYLOAD_BITS);
    let mut bits = [0u8; 96];
    for (i, &m) in ess.mi.iter().enumerate() {
        crate::bits::write_bits(&mut bits, i * 8, 8, m as u64);
    }
    crate::bits::write_bits(&mut bits, 72, 8, ess.algid as u64);
    crate::bits::write_bits(&mut bits, 80, 16, ess.kid as u64);
    let mut word = [0u8; ESS_HEXBITS];
    for h in 0..ESS_DATA_HEXBITS {
        let mut v = 0u8;
        for b in 0..6 {
            v = (v << 1) | bits[h * 6 + b];
        }
        word[ESS_HEXBITS - 1 - h] = v;
    }
    rs_24_16().encode(&mut word);
    for (h, &off) in ESS_HEXBIT_OFFSETS.iter().enumerate() {
        let cw = hamming::encode(word[ESS_HEXBITS - 1 - h]);
        for b in 0..10 {
            payload_bits[off + b] = ((cw >> (9 - b)) & 1) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Ess {
        Ess {
            mi: [0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 0x11],
            algid: 0x84, // AES-256
            kid: 0x0BEF,
        }
    }

    #[test]
    fn round_trips_a_clean_field() {
        let mut p = vec![0u8; LDU_PAYLOAD_BITS];
        write_ess(&mut p, &sample());
        let d = decode_ess(&p).unwrap();
        assert_eq!(d.ess, Some(sample()));
        assert_eq!(d.algid_hamming, 0x84);
        assert_eq!(d.rs_corrected, 0);
        assert_eq!(d.hamming_doubtful, 0);
    }

    #[test]
    fn a_clear_field_reads_clear() {
        let clear = Ess {
            mi: [0; 9],
            algid: ALGID_CLEAR,
            kid: 0,
        };
        let mut p = vec![0u8; LDU_PAYLOAD_BITS];
        write_ess(&mut p, &clear);
        let d = decode_ess(&p).unwrap();
        assert!(d.ess.unwrap().is_clear());
    }

    /// The failure that motivated this module: one flipped ALGID bit used to
    /// read as "encrypted". Now the Hamming layer repairs it outright.
    #[test]
    fn one_algid_bit_error_is_corrected_not_believed() {
        let clear = Ess {
            mi: [0; 9],
            algid: ALGID_CLEAR,
            kid: 0,
        };
        let mut p = vec![0u8; LDU_PAYLOAD_BITS];
        write_ess(&mut p, &clear);
        // ALGID's most significant bit is the first bit of hexbit 12.
        p[ESS_HEXBIT_OFFSETS[12]] ^= 1;
        let d = decode_ess(&p).unwrap();
        assert_eq!(d.ess.map(|e| e.algid), Some(ALGID_CLEAR));
        assert_eq!(d.algid_hamming, ALGID_CLEAR);
    }

    /// Four whole hexbits destroyed (beyond any Hamming repair) still
    /// validate through the outer code; five do not, and the result says
    /// "unknown" rather than picking a verdict.
    #[test]
    fn the_outer_code_repairs_four_ruined_hexbits_and_refuses_five() {
        let mut p = vec![0u8; LDU_PAYLOAD_BITS];
        write_ess(&mut p, &sample());
        let ruin = |p: &mut [u8], h: usize| {
            for b in 0..10 {
                p[ESS_HEXBIT_OFFSETS[h] + b] ^= (b % 3 == 0) as u8;
            }
        };
        for h in [1usize, 12, 13, 22] {
            ruin(&mut p, h);
        }
        let d = decode_ess(&p).unwrap();
        assert_eq!(d.ess, Some(sample()), "four ruined hexbits");
        assert!(d.rs_corrected >= 1 && d.rs_corrected <= 4);

        ruin(&mut p, 7);
        let d = decode_ess(&p).unwrap();
        assert_eq!(d.ess, None, "five ruined hexbits must not validate");
    }

    #[test]
    fn the_hexbit_offsets_are_the_link_control_slots() {
        // Same six slots as Link Control, four hexbits each.
        for (s, &slot) in crate::lc::LC_SLOTS.iter().enumerate() {
            for h in 0..4 {
                assert_eq!(ESS_HEXBIT_OFFSETS[s * 4 + h], slot + h * 10);
            }
        }
    }
}
