//! P25 1/2-rate trellis codec for TSDU/TSBK blocks: 96 data bits + 2-bit
//! flush → 49 constellation nibbles → interleave → 98 dibits.
//!
//! The interleave schedule and dibit-pair transition matrix are protocol
//! facts; tables taken from DSD-FME (`p25_12.c`, ISC license, Copyright (C)
//! 2010 DSD Author — see NOTICE). The Viterbi decoder itself is written
//! from the standard algorithm.

/// Interleave: transmitted dibit i carries deinterleaved position INTERLEAVE[i].
const INTERLEAVE: [usize; 98] = [
    0, 1, 8, 9, 16, 17, 24, 25, 32, 33, 40, 41, 48, 49, 56, 57, 64, 65, 72, 73, 80, 81, 88, 89, 96,
    97, 2, 3, 10, 11, 18, 19, 26, 27, 34, 35, 42, 43, 50, 51, 58, 59, 66, 67, 74, 75, 82, 83, 90,
    91, 4, 5, 12, 13, 20, 21, 28, 29, 36, 37, 44, 45, 52, 53, 60, 61, 68, 69, 76, 77, 84, 85, 92,
    93, 6, 7, 14, 15, 22, 23, 30, 31, 38, 39, 46, 47, 54, 55, 62, 63, 70, 71, 78, 79, 86, 87, 94,
    95,
];

/// Dibit-pair transition matrix: expected transmitted nibble for
/// (previous_dibit * 4 + current_dibit).
const DTM: [u8; 16] = [2, 12, 1, 15, 14, 0, 13, 3, 9, 7, 10, 4, 5, 11, 6, 8];

/// Encode 12 data bytes (96 bits) into 98 transmitted dibits.
pub fn encode(data: &[u8; 12]) -> [u8; 98] {
    // 48 data dibits + 1 flush dibit (0).
    let mut din = [0u8; 49];
    for i in 0..48 {
        let byte = data[i / 4];
        din[i] = (byte >> (6 - 2 * (i % 4))) & 3;
    }
    let mut deint = [0u8; 98];
    let mut state = 0u8;
    for (i, &d) in din.iter().enumerate() {
        let nib = DTM[(state * 4 + d) as usize];
        deint[i * 2] = nib >> 2;
        deint[i * 2 + 1] = nib & 3;
        state = d;
    }
    let mut out = [0u8; 98];
    for (i, o) in out.iter_mut().enumerate() {
        *o = deint[INTERLEAVE[i]];
    }
    out
}

/// Viterbi-decode 98 received dibits into 12 data bytes.
/// Returns (data, path_bit_errors) or None if hopeless.
pub fn decode(rx: &[u8; 98]) -> Option<([u8; 12], u32)> {
    let mut deint = [0u8; 98];
    for i in 0..98 {
        deint[INTERLEAVE[i]] = rx[i] & 3;
    }
    let mut nibs = [0u8; 49];
    for (i, n) in nibs.iter_mut().enumerate() {
        *n = (deint[i * 2] << 2) | deint[i * 2 + 1];
    }

    const INF: u32 = u32::MAX / 2;
    let mut metric = [INF; 4];
    metric[0] = 0; // encoder starts in state 0
    let mut paths: Vec<[u8; 4]> = Vec::with_capacity(49);
    for &rx_nib in nibs.iter() {
        let mut next = [INF; 4];
        let mut back = [0u8; 4];
        for s in 0..4usize {
            if metric[s] == INF {
                continue;
            }
            for d in 0..4usize {
                let expect = DTM[s * 4 + d];
                let cost = (expect ^ rx_nib).count_ones();
                let m = metric[s] + cost;
                if m < next[d] {
                    next[d] = m;
                    back[d] = s as u8;
                }
            }
        }
        paths.push(back);
        metric = next;
    }
    // Final state should be 0 (flush dibit); take best regardless but note it.
    let mut state = (0..4).min_by_key(|&s| metric[s])? as u8;
    let total_cost = metric[state as usize];
    if total_cost == INF {
        return None;
    }
    let mut dibits = [0u8; 49];
    for t in (0..49).rev() {
        dibits[t] = state;
        state = paths[t][state as usize];
    }
    let mut data = [0u8; 12];
    for i in 0..48 {
        data[i / 4] |= dibits[i] << (6 - 2 * (i % 4));
    }
    Some((data, total_cost))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_clean_and_with_errors() {
        let data: [u8; 12] = [
            0x00, 0x2F, 0x93, 0xAB, 0x00, 0x01, 0x00, 0x64, 0x00, 0x00, 0xBE, 0xEF,
        ];
        let tx = encode(&data);
        let (rx, cost) = decode(&tx).unwrap();
        assert_eq!(rx, data);
        assert_eq!(cost, 0);

        // Two single-bit errors in well-separated trellis stages —
        // Viterbi must recover. (Dense adjacent-stage errors can exceed the
        // 4-state code's correction ability; that's expected, not a bug.)
        let mut bad = tx;
        bad[5] ^= 1; // → stage 8 after deinterleave
        bad[40] ^= 1; // → stage 29
        let (rx2, cost2) = decode(&bad).unwrap();
        assert_eq!(rx2, data);
        assert!(cost2 > 0);
    }
}
