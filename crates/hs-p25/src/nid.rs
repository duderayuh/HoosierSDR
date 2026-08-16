//! Network ID word: NAC (12) + DUID (4), BCH(63,16)-protected, plus one
//! trailing parity bit to fill 64 bits / 32 dibits.

use crate::bch::Bch6316;
use crate::Duid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nid {
    pub nac: u16,
    pub duid: Duid,
}

pub struct NidCodec {
    bch: Bch6316,
}

impl Default for NidCodec {
    fn default() -> Self {
        Self::new()
    }
}

impl NidCodec {
    pub fn new() -> Self {
        Self {
            bch: Bch6316::new(),
        }
    }

    /// Encode to 64 bits (MSB-first): 63-bit codeword + even parity bit.
    pub fn encode(&self, nac: u16, duid: u8) -> u64 {
        let data = ((nac & 0xFFF) << 4) | (duid as u16 & 0xF);
        let cw = self.bch.encode(data);
        let parity = (cw.count_ones() & 1) as u64;
        (cw << 1) | parity
    }

    /// Decode 64 received bits; parity bit is advisory and ignored.
    pub fn decode(&self, word: u64) -> Option<(Nid, u32)> {
        let cw = word >> 1;
        let (data, errs) = self.bch.decode(cw)?;
        Some((
            Nid {
                nac: data >> 4,
                duid: Duid::from((data & 0xF) as u8),
            },
            errs,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nid_roundtrip_with_noise() {
        let c = NidCodec::new();
        let w = c.encode(0x293, 0x7);
        let (nid, e) = c.decode(w).unwrap();
        assert_eq!(nid.nac, 0x293);
        assert_eq!(nid.duid, Duid::TrunkSignalBlock);
        assert_eq!(e, 0);
        let (nid2, e2) = c.decode(w ^ 0b1010100 << 20).unwrap();
        assert_eq!(nid2.nac, 0x293);
        assert!(e2 > 0);
    }
}
