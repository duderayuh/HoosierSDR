//! P25 Phase 2 TDMA voice deinterleaver.
//!
//! The transmitter spreads a 96-bit (4×24) AMBE+2 / IMBE voice frame across a
//! 72-bit burst inside a slot using a diagonal interleaver. 24 of the 96 bits
//! are punctured (never transmitted). This module reverses that: it maps 72
//! received bits back onto the 4×24 frame grid that `mbelib` consumes.
//!
//! # Provenance
//! The permutation tables (`C0`…`C3`, `CSUBSET`) are protocol **facts** from
//! TIA-102.BBAB §7 (the TDMA voice/interleave schedule), cross-checked against
//! the ISC-licensed `dsd-fme` reference (`p25p2_frame.c`, "4V and 2V
//! deinterleave schedule"). They are a fixed numeric permutation, not creative
//! expression, and they were transcribed directly — not copied from any GPL
//! codebase. See `CONTRIBUTING.md` and `docs/ARCHITECTURE.md §5`.
//!
//! # Wire format
//! `deinterleave` consumes `input[0..72]` in *slot order* (the first received
//! bit is `input[0]`) and writes a row-major 4×24 frame, indexed `frame[row][col]`,
//! identical to `char ambe_fr[4][24]` consumed by `mbe_processAmbe3600x2400Frame`.
//!
//! The output grid is *punctured*: 24 positions are never filled by the RX
//! path (they carry no transmitted bit). Those are the bits mbelib reconstructs
//! from its own FEC. Specifically, row 1 drops column 23, row 2 drops columns
//! 11–23, and row 3 drops columns 14–23.

/// Per-slot deinterleave schedule row 0 (24 columns, all of 0..=23).
const C0: [u8; 24] = [
    23, 5, 22, 4, 21, 3, 20, 2, 19, 1, 18, 0, 17, 16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6,
];

/// Per-slot deinterleave schedule row 1 (23 columns; col 23 is punctured).
const C1: [u8; 23] = [
    10, 9, 8, 7, 6, 5, 22, 4, 21, 3, 20, 2, 19, 1, 18, 0, 17, 16, 15, 14, 13, 12, 11,
];

/// Per-slot deinterleave schedule row 2 (11 columns; cols 11–23 punctured).
const C2: [u8; 11] = [3, 2, 1, 0, 10, 9, 8, 7, 6, 5, 4];

/// Per-slot deinterleave schedule row 3 (14 columns; cols 14–23 punctured).
const C3: [u8; 14] = [13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0];

/// Which schedule row (0..=3) consumes each of the 72 received bit positions.
const CSUBSET: [u8; 72] = [
    0, 0, 1, 2, 0, 0, 1, 2, 0, 0, 1, 2, 0, 0, 1, 2, //
    0, 0, 1, 3, 0, 0, 1, 3, 0, 1, 1, 3, 0, 1, 1, 3, //
    0, 1, 1, 3, 0, 1, 1, 3, 0, 1, 1, 3, 0, 1, 2, 3, //
    0, 1, 2, 3, 0, 1, 2, 3, 0, 1, 2, 3, 0, 1, 2, 3, //
    0, 1, 2, 3, 0, 1, 2, 3,
];

/// Transmitted bits per schedule row — the interleaver's column consumption.
/// These must equal the actual `C0…C3` lengths (24/23/11/14).
#[cfg(test)]
const ROW_BITS: [usize; 4] = [C0.len(), C1.len(), C2.len(), C3.len()];

/// A deinterleaved 4×24 voice frame (96-bit container, 72 transmitted bits).
pub type VoiceFrame = [[u8; 24]; 4];

/// Deinterleave a 72-bit received burst into a 4×24 frame grid.
///
/// This is the RX direction: `input[x]` is the `x`-th bit received in the
/// slot's voice block (slot order), and the result places each bit at its
/// original `frame[row][col]` position before the transmitter's interleaver.
pub fn deinterleave(input: &[u8; 72]) -> VoiceFrame {
    let mut frame = [[0u8; 24]; 4];
    // Per-row consumption counters (q/r/s/t in the reference implementation).
    let mut consumed = [0usize; 4];

    for (x, &row) in CSUBSET.iter().enumerate() {
        let col = schedule_col(row as usize, &mut consumed);
        frame[row as usize][col as usize] = input[x] & 1;
    }

    frame
}

/// Interleave a 4×24 frame grid back into a 72-bit burst (TX direction).
///
/// This is the exact inverse of [`deinterleave`] on the transmitted positions.
/// It exists to let tests prove round-trip identity, and for any future encode
/// path. Punctured positions in `frame` are ignored by construction (the
/// schedule never reads them).
pub fn interleave(frame: &VoiceFrame) -> [u8; 72] {
    let mut out = [0u8; 72];
    let mut consumed = [0usize; 4];

    for (x, &row) in CSUBSET.iter().enumerate() {
        let col = schedule_col(row as usize, &mut consumed);
        out[x] = frame[row as usize][col as usize] & 1;
    }

    out
}

/// Return the next column to write for `row`, advancing that row's counter.
///
/// This mirrors the reference's cascade of `if ww == 0 { b = c0[q++]; } …`.
#[inline]
fn schedule_col(row: usize, consumed: &mut [usize; 4]) -> u8 {
    let col = match row {
        0 => C0[consumed[0]],
        1 => C1[consumed[1]],
        2 => C2[consumed[2]],
        3 => C3[consumed[3]],
        _ => unreachable!("CSUBSET values are 0..=3"),
    };
    consumed[row] += 1;
    col
}

/// Whether a `(row, col)` position of the 4×24 grid carries a transmitted bit
/// (i.e. is reachable by the deinterleaver). The 24 unreachable cells are the
/// punctures mbelib's FEC must fill in.
pub fn is_transmitted(row: usize, col: usize) -> bool {
    match row {
        0 => C0.contains(&(col as u8)),
        1 => C1.contains(&(col as u8)),
        2 => C2.contains(&(col as u8)),
        3 => C3.contains(&(col as u8)),
        _ => false,
    }
}

/// Number of bits actually transmitted of the 96-position frame (always 72).
pub const TRANSMITTED_BITS: usize = CSUBSET.len();
/// Number of punctured frame positions (always 24).
pub const PUNCTURED_BITS: usize = 96 - TRANSMITTED_BITS;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csubset_consumption_matches_schedule_lengths() {
        // The selector must demand exactly len(C0)…len(C3) of each row, so the
        // per-row counters never run off the end.
        let mut seen = [0usize; 4];
        for &row in &CSUBSET {
            seen[row as usize] += 1;
        }
        assert_eq!(
            seen, ROW_BITS,
            "CSUBSET composition must match C0..C3 lengths"
        );
        assert_eq!(seen.iter().sum::<usize>(), TRANSMITTED_BITS);
        assert_eq!(ROW_BITS.iter().sum::<usize>(), TRANSMITTED_BITS);
    }

    #[test]
    fn schedule_rows_are_distinct_columns() {
        // Each schedule row visits a set of distinct columns, so no frame cell
        // is written twice and the deinterleaver is a true permutation.
        for (row, tbl) in [C0.as_slice(), &C1, &C2, &C3].iter().enumerate() {
            let mut cols = tbl.to_vec();
            cols.sort_unstable();
            cols.dedup();
            assert_eq!(cols.len(), tbl.len(), "row {row} has duplicate columns");
        }
    }

    #[test]
    fn puncture_count_is_24() {
        assert_eq!(PUNCTURED_BITS, 24);
        let mut transmitted = 0;
        for r in 0..4 {
            for c in 0..24 {
                if is_transmitted(r, c) {
                    transmitted += 1;
                }
            }
        }
        assert_eq!(transmitted, 72);
        assert_eq!(transmitted + PUNCTURED_BITS, 96);
    }

    #[test]
    fn interleave_deinterleave_round_trip() {
        // Fill every transmitted position with a unique value, leave punctures
        // at 0, and prove interleave → deinterleave recovers the exact frame.
        let mut frame: VoiceFrame = [[0u8; 24]; 4];
        let mut bit = 0u8;
        for r in 0..4 {
            for c in 0..24 {
                if is_transmitted(r, c) {
                    frame[r][c] = bit & 1;
                    bit = bit.wrapping_add(1);
                }
            }
        }
        let burst = interleave(&frame);
        let recovered = deinterleave(&burst);
        assert_eq!(
            recovered, frame,
            "round-trip must be identity on transmitted bits"
        );
    }

    #[test]
    fn deinterleave_is_deterministic_and_total() {
        // A single bit set at input position p must land at exactly one frame
        // cell (the permutation is total and injective over 72 positions).
        let mut seen = std::collections::HashSet::new();
        for p in 0..72 {
            let mut burst = [0u8; 72];
            burst[p] = 1;
            let frame = deinterleave(&burst);
            let mut set_cells = Vec::new();
            for r in 0..4 {
                for c in 0..24 {
                    if frame[r][c] == 1 {
                        set_cells.push((r, c));
                    }
                }
            }
            assert_eq!(set_cells.len(), 1, "position {p} must map to one cell");
            assert!(
                seen.insert(set_cells[0]),
                "position {p} collided at {set_cells:?}"
            );
        }
        assert_eq!(seen.len(), 72);
    }
}
