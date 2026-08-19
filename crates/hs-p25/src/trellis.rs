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

/// Viterbi-decode 98 received hard dibits into 12 data bytes.
/// Returns (data, path_bit_errors) or None if hopeless.
pub fn decode(rx: &[u8; 98]) -> Option<([u8; 12], u32)> {
    let mut soft = [crate::soft::SoftDibit::default(); 98];
    for (o, &d) in soft.iter_mut().zip(rx.iter()) {
        *o = crate::soft::SoftDibit::hard(d);
    }
    // Path cost comes back scaled by CERTAIN; report it in bit errors so the
    // hard-decision contract is unchanged.
    decode_soft(&soft).map(|(d, c)| (d, c / crate::soft::CERTAIN as u32))
}

/// Viterbi-decode 98 received dibits **with per-bit confidence**.
///
/// The only change from the hard decoder is the branch metric: instead of
/// counting how many bits differ from the expected constellation nibble, each
/// disagreement is charged by how much the demodulator trusted that bit. A
/// path that contradicts four barely-decided bits then costs less than one
/// contradicting two confident bits — which is the right ordering, and the one
/// hard decoding cannot express. This is where most of the coding gain in a
/// soft-decision receiver comes from.
///
/// Returns (data, path_cost) with the cost in confidence units; with
/// all-certain inputs it is exactly `CERTAIN ×` the hard-decision bit errors.
pub fn decode_soft(rx: &[crate::soft::SoftDibit; 98]) -> Option<([u8; 12], u32)> {
    use crate::soft::SoftDibit;

    let mut deint = [SoftDibit::default(); 98];
    for i in 0..98 {
        deint[INTERLEAVE[i]] = rx[i];
    }

    const INF: u32 = u32::MAX / 2;
    let mut metric = [INF; 4];
    metric[0] = 0; // encoder starts in state 0
    let mut paths: Vec<[u8; 4]> = Vec::with_capacity(49);
    for t in 0..49usize {
        // A constellation nibble is carried by two consecutive dibits, each
        // with its own confidence.
        let (hi, lo) = (deint[t * 2], deint[t * 2 + 1]);
        let mut next = [INF; 4];
        let mut back = [0u8; 4];
        for s in 0..4usize {
            if metric[s] == INF {
                continue;
            }
            for d in 0..4usize {
                let expect = DTM[s * 4 + d];
                let cost = hi.cost_against(expect >> 2) + lo.cost_against(expect & 3);
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

/// K-best list Viterbi: like [`decode_soft`], but returns up to `list`
/// lowest-cost decodes, best first (the first entry is the ML path).
///
/// The point is CRC-guided recovery: when the maximum-likelihood path fails
/// the TSBK CRC, the correct codeword is usually one of the next few paths —
/// a couple of low-confidence dibits decided the other way. Trying list
/// candidates against the CRC turns those near-misses into decodes, and the
/// CRC arbitrates: a wrong candidate passes with probability ~2⁻¹⁶ per try.
pub fn decode_list_soft(
    rx: &[crate::soft::SoftDibit; 98],
    list: usize,
) -> Vec<([u8; 12], u32)> {
    use crate::soft::SoftDibit;

    let k = list.max(1);
    let mut deint = [SoftDibit::default(); 98];
    for i in 0..98 {
        deint[INTERLEAVE[i]] = rx[i];
    }

    // survivors[s]: up to k paths ending in state s, as (cost, prev_state,
    // prev_rank). back[t][s] mirrors them for traceback.
    let mut survivors: [Vec<(u32, u8, u8)>; 4] = Default::default();
    survivors[0].push((0, 0, 0)); // encoder starts in state 0
    let mut back: Vec<[Vec<(u32, u8, u8)>; 4]> = Vec::with_capacity(49);

    for t in 0..49usize {
        let (hi, lo) = (deint[t * 2], deint[t * 2 + 1]);
        let mut next: [Vec<(u32, u8, u8)>; 4] = Default::default();
        for d in 0..4usize {
            let mut cands: Vec<(u32, u8, u8)> = Vec::with_capacity(4 * k);
            for s in 0..4usize {
                let expect = DTM[s * 4 + d];
                let branch = hi.cost_against(expect >> 2) + lo.cost_against(expect & 3);
                for (r, &(cost, _, _)) in survivors[s].iter().enumerate() {
                    cands.push((cost + branch, s as u8, r as u8));
                }
            }
            cands.sort_unstable();
            cands.truncate(k);
            next[d] = cands;
        }
        back.push(next.clone());
        survivors = next;
    }

    // Rank all finishing paths, best first, and trace each back.
    let mut finals: Vec<(u32, u8, u8)> = Vec::new();
    for s in 0..4usize {
        for (r, &(cost, _, _)) in survivors[s].iter().enumerate() {
            finals.push((cost, s as u8, r as u8));
        }
    }
    finals.sort_unstable();
    finals.truncate(list);

    let mut out = Vec::with_capacity(finals.len());
    for &(cost, fs, fr) in &finals {
        let (mut state, mut rank) = (fs, fr);
        let mut dibits = [0u8; 49];
        for t in (0..49).rev() {
            dibits[t] = state;
            let (_, ps, pr) = back[t][state as usize][rank as usize];
            state = ps;
            rank = pr;
        }
        let mut data = [0u8; 12];
        for i in 0..48 {
            data[i / 4] |= dibits[i] << (6 - 2 * (i % 4));
        }
        out.push((data, cost));
    }
    out
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

    #[test]
    fn list_decoding_recovers_blocks_the_ml_path_gets_wrong() {
        use crate::soft::SoftDibit;

        let data: [u8; 12] = [
            0x00, 0x2F, 0x93, 0xAB, 0x00, 0x01, 0x00, 0x64, 0x00, 0x00, 0xBE, 0xEF,
        ];
        let tx = encode(&data);

        // The list must lead with the ML decode and never lower a cost.
        let clean: Vec<SoftDibit> = tx.iter().map(|&d| SoftDibit::hard(d)).collect();
        let clean: [SoftDibit; 98] = clean.try_into().unwrap();
        let list = decode_list_soft(&clean, 8);
        assert_eq!(list[0].0, data);
        assert_eq!(list[0].1, decode_soft(&clean).unwrap().1);
        assert!(list.windows(2).all(|w| w[0].1 <= w[1].1));

        // Find a corruption dense enough that the ML path decodes to the
        // wrong data (adjacent-stage errors exceed the 4-state code), then
        // show the true codeword still appears in the list — that is the
        // candidate the TSBK CRC picks out. Deterministic search, no RNG.
        let mut found = false;
        'outer: for a in 0..96usize {
            let mut bad = tx;
            bad[a] ^= 3;
            bad[a + 1] ^= 3;
            bad[a + 2] ^= 1;
            let soft: Vec<SoftDibit> = bad.iter().map(|&d| SoftDibit::hard(d)).collect();
            let soft: [SoftDibit; 98] = soft.try_into().unwrap();
            let (ml, _) = decode_soft(&soft).unwrap();
            if ml == data {
                continue; // ML already right; not the case under test
            }
            for (cand, _) in decode_list_soft(&soft, 64) {
                if cand == data {
                    found = true;
                    break 'outer;
                }
            }
        }
        assert!(
            found,
            "no corruption produced an ML miss that the list recovered"
        );
    }

    #[test]
    fn soft_decoding_beats_hard_decoding_on_a_noisy_channel() {
        use crate::soft::{soft_slice_c4fm, SoftDibit};

        // Coding gain is statistical, not per-frame: soft decoding does not
        // rescue a stage that noise destroyed outright, it wins on the many
        // marginal frames where the confidence pattern says which way to lean.
        // So measure it the way it is actually claimed -- as a success rate
        // over many frames through a realistic symbol channel.
        //
        // The channel here is the real one: dibits become C4FM levels
        // (nominal +/-1, +/-3), pick up Gaussian noise, and are then either
        // hard-sliced (throwing the confidence away) or soft-sliced (keeping
        // it). Both decoders see exactly the same received symbols.
        // Chosen so hard decoding is clearly stressed but not collapsing; the
        // gap widens further as noise rises (at 1.5: hard 175/300, soft 291/300).
        const TRIALS: u32 = 300;
        const NOISE: f32 = 1.3;

        let mut rng = 0x243F_6A88_85A3_08D3u64;
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        // Sum of uniforms -> approximately Gaussian.
        let mut noise = move || {
            let mut acc = 0.0f32;
            for _ in 0..4 {
                acc += (next() >> 40) as f32 / (1u64 << 24) as f32;
            }
            (acc - 2.0) / 1.15
        };

        let level = |d: u8| match d & 3 {
            0b01 => 3.0f32,
            0b00 => 1.0,
            0b10 => -1.0,
            _ => -3.0,
        };

        let (mut hard_ok, mut soft_ok) = (0u32, 0u32);
        for t in 0..TRIALS {
            let mut data = [0u8; 12];
            for (i, b) in data.iter_mut().enumerate() {
                *b = ((t as usize * 7 + i * 31) % 251) as u8;
            }
            let tx = encode(&data);

            let mut hard = [0u8; 98];
            let mut soft = [SoftDibit::default(); 98];
            for i in 0..98 {
                let rx = level(tx[i]) + NOISE * noise();
                let sd = soft_slice_c4fm(rx);
                hard[i] = sd.bits;
                soft[i] = sd;
            }

            if decode(&hard).map(|(d, _)| d) == Some(data) {
                hard_ok += 1;
            }
            if decode_soft(&soft).map(|(d, _)| d) == Some(data) {
                soft_ok += 1;
            }
        }

        eprintln!("trellis @ noise {NOISE}: hard {hard_ok}/{TRIALS}, soft {soft_ok}/{TRIALS}");
        // The channel must be hard enough to distinguish the two.
        assert!(
            hard_ok < TRIALS,
            "channel too easy: hard decoding already perfect"
        );
        assert!(
            soft_ok > hard_ok,
            "soft decoding did not beat hard: soft {soft_ok} vs hard {hard_ok} of {TRIALS}"
        );
    }

    #[test]
    fn soft_decoding_matches_hard_when_every_bit_is_certain() {
        use crate::soft::SoftDibit;
        let data: [u8; 12] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let tx = encode(&data);
        let mut soft = [SoftDibit::default(); 98];
        for (o, &d) in soft.iter_mut().zip(tx.iter()) {
            *o = SoftDibit::hard(d);
        }
        assert_eq!(decode_soft(&soft).map(|(d, _)| d), Some(data));
    }
}
