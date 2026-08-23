//! P25 Phase 2 (TDMA) layer-2 framing — slot/superframe geometry, the frame
//! scrambler, and (in later increments) the DUID, ISCH, and voice-channel
//! frame extraction.
//!
//! # Provenance
//! Everything in this module is implemented from **protocol facts** in the
//! public TIA-102 specifications (BBAB *Time Division Multiple Access*,
//! BBAC *MAC/Logical Channel*) and the ISC-licensed `dsd-fme` reference.
//! No code is derived from GPL projects (OP25, mbelib-neo) — see
//! `CONTRIBUTING.md` and `docs/ARCHITECTURE.md §5`.
//!
//! # Scope note
//! Phase 2 voice is IMBE (full rate, 7200 bps — already in-tree) and
//! AMBE+2 (half rate, 3600 bps — available in the ISC-licensed mbelib
//! `ambe3600x2450.c`, `mbe_processAmbe3600x2450Frame`;
//! HoosierSDR vendors only the IMBE subset today). This module extracts and
//! de-FECs the **frame bits**; vendoring the half-rate `.c` and feeding those
//! bits to it is the remaining wiring.

pub mod deinterleave;
pub mod duid;
pub mod isch;
pub mod modulation;
pub mod scramble;
pub mod voice;

/// P25 Phase 2 symbol rate (symbols/second).
pub const SYMBOL_RATE: f64 = 6000.0;

/// Bits per symbol: Phase 2 is a 2-bit-per-symbol (quaternary) CPM.
pub const BITS_PER_SYMBOL: u32 = 2;

/// One TDMA timeslot: 180 symbols / 360 bits at 30 ms.
pub const SLOT_SYMBOLS: usize = 180;
pub const SLOT_BITS: usize = SLOT_SYMBOLS * BITS_PER_SYMBOL as usize;

/// A superframe is 12 timeslots (360 ms).
pub const SLOTS_PER_SUPERFRAME: usize = 12;
pub const SUPERFRAME_SYMBOLS: usize = SLOTS_PER_SUPERFRAME * SLOT_SYMBOLS;
pub const SUPERFRAME_BITS: usize = SLOTS_PER_SUPERFRAME * SLOT_BITS;

/// Frame-sync word length, in symbols (transmitted at the head of each slot).
pub const SYNC_SYMBOLS: usize = 20;
pub const SYNC_BITS: usize = SYNC_SYMBOLS * BITS_PER_SYMBOL as usize;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_holds_together() {
        assert_eq!(SLOT_BITS, 360);
        assert_eq!(SLOT_SYMBOLS, 180);
        assert_eq!(SUPERFRAME_BITS, 4320);
        assert_eq!(SUPERFRAME_SYMBOLS, 2160);
        assert_eq!(SYNC_BITS, 40);
        // 12 slots x 30 ms = 360 ms superframe at 6000 sym/s
        assert_eq!(SUPERFRAME_SYMBOLS as f64 / SYMBOL_RATE, 0.360);
    }
}
