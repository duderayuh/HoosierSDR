//! P25 Phase I layer-1/2: frame sync, NID, FEC, TSBK/MBT parsing.
//!
//! Protocol constants are facts from the public TIA-102 specifications and
//! are not derived from any GPL implementation.

pub mod bch;
pub mod bits;
pub mod crc;
pub mod framer;
pub mod nid;
pub mod synth;
pub mod trellis;
pub mod tsbk;
pub mod voice;

/// P25 Frame Sync Word: 48 bits / 24 dibit symbols, transmitted before every
/// data unit. This is the free training sequence the equalizer trains on.
pub const FRAME_SYNC: u64 = 0x5575F5FF77FF;
pub const FRAME_SYNC_BITS: u32 = 48;
pub const FRAME_SYNC_SYMBOLS: usize = 24;

/// Data Unit ID from the NID (after BCH decode).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Duid {
    HeaderDataUnit,       // 0x0
    TerminatorNoLc,       // 0x3
    LogicalLinkDataUnit1, // 0x5
    TrunkSignalBlock,     // 0x7 (TSDU)
    LogicalLinkDataUnit2, // 0xA
    PacketDataUnit,       // 0xC
    TerminatorWithLc,     // 0xF
    Unknown(u8),
}

impl From<u8> for Duid {
    fn from(v: u8) -> Self {
        match v & 0xF {
            0x0 => Duid::HeaderDataUnit,
            0x3 => Duid::TerminatorNoLc,
            0x5 => Duid::LogicalLinkDataUnit1,
            0x7 => Duid::TrunkSignalBlock,
            0xA => Duid::LogicalLinkDataUnit2,
            0xC => Duid::PacketDataUnit,
            0xF => Duid::TerminatorWithLc,
            v => Duid::Unknown(v),
        }
    }
}

impl Duid {
    /// The 4-bit DUID code as transmitted.
    pub fn code(self) -> u8 {
        match self {
            Duid::HeaderDataUnit => 0x0,
            Duid::TerminatorNoLc => 0x3,
            Duid::LogicalLinkDataUnit1 => 0x5,
            Duid::TrunkSignalBlock => 0x7,
            Duid::LogicalLinkDataUnit2 => 0xA,
            Duid::PacketDataUnit => 0xC,
            Duid::TerminatorWithLc => 0xF,
            Duid::Unknown(v) => v & 0xF,
        }
    }
}

/// P25 encryption algorithm IDs. ALGID 0x80 is clear (unencrypted).
///
/// HoosierSDR **never decrypts**. Anything other than `Clear` is surfaced to
/// the UI as encrypted and the audio path is skipped — an architectural
/// refusal per 18 U.S.C. § 2511/2510. See README and CONTRIBUTING.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlgId {
    Clear,
    Encrypted(u8),
}

impl From<u8> for AlgId {
    fn from(v: u8) -> Self {
        if v == 0x80 {
            AlgId::Clear
        } else {
            AlgId::Encrypted(v)
        }
    }
}

impl AlgId {
    /// The single gate the audio pipeline consults. There is no override.
    pub fn is_decodable(self) -> bool {
        self == AlgId::Clear
    }
}

/// Count bit errors between a received 48-bit window and the frame sync word.
pub fn sync_bit_errors(window: u64) -> u32 {
    ((window ^ FRAME_SYNC) & ((1u64 << FRAME_SYNC_BITS) - 1)).count_ones()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_sync_matches() {
        assert_eq!(sync_bit_errors(FRAME_SYNC), 0);
        assert_eq!(sync_bit_errors(FRAME_SYNC ^ 0b101), 2);
    }

    #[test]
    fn encrypted_is_never_decodable() {
        assert!(AlgId::from(0x80).is_decodable());
        for v in 0u8..=0x7F {
            assert!(!AlgId::from(v).is_decodable());
        }
        assert!(!AlgId::from(0x81).is_decodable());
        assert!(!AlgId::from(0xAA).is_decodable()); // ADP
    }
}
