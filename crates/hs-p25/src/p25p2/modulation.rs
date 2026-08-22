//! P25 Phase 2 air-interface modulation and symbol mapping.
//!
//! # Modulation (fact-corrected)
//! Phase 2 uses **two** modulations on the same 12.5 kHz, 6000-symbol/s channel:
//!
//! * **Downlink** (base station → subscriber, what a receiver actually hears) is
//!   **H-DQPSK** — *Harmonized Differential QPSK*, a *linear* π/4-differential
//!   modulation. It is received with the same differential-QPSK receiver as
//!   Phase I CQPSK (see `hs-dsp::cqpsk`), just at 6000 sym/s instead of 4800.
//! * **Uplink** (subscriber → base station) is **H-CPM** — *Harmonized CPM*, a
//!   quaternary *continuous-phase* modulation with two alternating modulation
//!   indices (multi-h). A scanner never receives it, only the downlink H-DQPSK.
//!
//! (Source: TIA-102.BBAB; the Freedom R8000 application note, which documents
//! H-CPM inbound / H-DQPSK outbound.) This corrects the earlier "H-CPM demod"
//! framing — the demod target on the downlink is the *linear* H-DQPSK.
//!
//! # Symbol alphabet
//! Both modulations carry a quaternary symbol per 2-bit dibit, with the four
//! ideal frequency-deviation levels `{ −3, −1, +1, +3 }` (normalized). The
//! level ↔ dibit mapping below is cross-checked against ISC `dsd-fme`
//! (`digitize`, non-inverted `+p25p2` path), not GPL code.

/// Ideal quaternary frequency-deviation levels, ascending.
pub const SYMBOL_LEVELS: [f32; 4] = [-3.0, -1.0, 1.0, 3.0];

/// Convert a level index (0..=3, ascending) to its dibit (2 bits).
///
/// Verified against `dsd-fme`: `+3→1, +1→0, −1→2, −3→3`.
pub const LEVEL_TO_DIBIT: [u8; 4] = [0b11, 0b10, 0b00, 0b01];

/// Convert a dibit (0..=3) to its level index (0..=3, ascending).
pub const DIBIT_TO_LEVEL_INDEX: [u8; 4] = [2, 3, 1, 0];

/// Convert a dibit to its ideal (normalized) frequency-deviation level.
pub fn dibit_to_level(dibit: u8) -> f32 {
    SYMBOL_LEVELS[DIBIT_TO_LEVEL_INDEX[(dibit & 3) as usize] as usize]
}

/// The downlink modulation (what a receiver demodulates).
pub const DOWNLINK_MODULATION: &str = "H-DQPSK (Harmonized Differential QPSK, linear)";
/// The uplink modulation (subscriber transmit only — not received by a scanner).
pub const UPLINK_MODULATION: &str = "H-CPM (Harmonized CPM, quaternary multi-h)";

/// Maximum normalized deviation of the quaternary levels (for AFK/frequency
/// scaling). In Hz at the `SYMBOL_RATE`, the peak deviation is 3·h·symbol_rate;
/// the R8000 lists the peak **symbol deviation** as ~2845–3310 Hz.
pub const NORMALIZED_PEAK_DEVIATION: f32 = 3.0;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping_is_a_bijection() {
        for d in 0u8..4 {
            let idx = DIBIT_TO_LEVEL_INDEX[d as usize];
            assert_eq!(LEVEL_TO_DIBIT[idx as usize], d, "dibit {d} round-trips");
        }
    }

    #[test]
    fn dibit_levels_match_dsd_fme() {
        // dsd-fme digitize (non-inverted +p25p2): +3→1, +1→0, −1→2, −3→3.
        assert_eq!(dibit_to_level(1), 3.0); // +3
        assert_eq!(dibit_to_level(0), 1.0); // +1
        assert_eq!(dibit_to_level(2), -1.0); // −1
        assert_eq!(dibit_to_level(3), -3.0); // −3
    }

    #[test]
    fn levels_are_ascending_and_symmetric() {
        assert_eq!(SYMBOL_LEVELS, [-3.0, -1.0, 1.0, 3.0]);
        for &l in &SYMBOL_LEVELS[..2] {
            assert!(SYMBOL_LEVELS.iter().any(|&x| x == -l), "level {l} lacks −l");
        }
    }
}
