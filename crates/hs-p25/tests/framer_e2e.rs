//! Framer integration: synthesized over-the-air dibit streams (status
//! symbols included) must decode back to the original TSBKs and voice frames.

use hs_p25::framer::{Framer, FramerEvent};
use hs_p25::sync_bit_errors;
use hs_p25::synth::{build_ldu1, build_tsdu, sync_dibits};
use hs_p25::tsbk::Tsbk;
use hs_p25::voice::ImbeFrame;

fn run(dibits: &[u8]) -> Vec<FramerEvent> {
    let mut f = Framer::new();
    let mut ev = Vec::new();
    // Leading garbage before the frame to exercise sync search.
    for g in [2u8, 0, 1, 3, 3, 0, 2, 1, 0, 0, 3] {
        f.push(g, &mut ev);
    }
    for &d in dibits {
        f.push(d, &mut ev);
    }
    ev
}

#[test]
fn tsdu_roundtrip_through_framer() {
    let args: u64 = (0x100Au64 << 40) | (0x2F93u64 << 24) | 0xBEEF1;
    let stream = build_tsdu(
        0x293,
        &[(0x00, 0, args), (0x3A, 0, 0x0001_0200_6400_u64 << 8)],
    );
    let ev = run(&stream);

    let mut saw_sync = false;
    let mut grants = 0;
    for e in &ev {
        match e {
            FramerEvent::Sync { bit_errors } => {
                assert_eq!(*bit_errors, 0);
                saw_sync = true;
            }
            FramerEvent::Nid { nid, .. } => {
                assert_eq!(nid.nac, 0x293);
            }
            FramerEvent::Tsdu { nac, blocks } => {
                assert_eq!(*nac, 0x293);
                assert_eq!(blocks.len(), 2);
                match &blocks[0].tsbk {
                    Tsbk::GroupVoiceGrant {
                        channel,
                        group,
                        source,
                        ..
                    } => {
                        assert_eq!(*channel, 0x100A);
                        assert_eq!(*group, 0x2F93);
                        assert_eq!(*source, 0xBEEF1);
                        grants += 1;
                    }
                    other => panic!("expected grant, got {:?}", other),
                }
                assert!(blocks[1].last_block);
            }
            _ => {}
        }
    }
    assert!(saw_sync);
    assert_eq!(grants, 1);
}

#[test]
fn ldu1_voice_frames_roundtrip_through_framer() {
    // Distinct bit patterns per frame across the valid codeword positions.
    let mut frames: [ImbeFrame; 9] = [[[0u8; 23]; 8]; 9];
    let widths = [23usize, 23, 23, 23, 15, 15, 15, 7];
    for (k, fr) in frames.iter_mut().enumerate() {
        for (w, row) in fr.iter_mut().enumerate() {
            for (x, cell) in row.iter_mut().enumerate().take(widths[w]) {
                *cell = (((k + 1) * (w + 2) * (x + 3)) % 2) as u8;
            }
        }
    }
    let stream = build_ldu1(0x293, &frames);
    let ev = run(&stream);
    let mut saw = false;
    for e in ev {
        if let FramerEvent::Ldu {
            nac,
            imbe,
            ess,
            inferred,
            ..
        } = e
        {
            assert_eq!(nac, 0x293);
            assert!(ess.is_none(), "an LDU1 carries no encryption sync");
            assert!(!inferred);
            assert_eq!(*imbe, frames);
            saw = true;
        }
    }
    assert!(saw, "no LDU event emitted");
}

/// The flywheel: a TSDU's frame sync word corrupted badly enough that a cold,
/// no-information search would reject it must still be recognized when it
/// lands exactly where the *previous* TSDU's declared length says it should
/// — the whole point of coasting on protocol timing instead of re-searching
/// blind. Uses a single-block first TSDU deliberately: its length (24 FSW +
/// 32 NID + 98 payload = 154 content dibits, 158 raw once its own interior
/// status dibits are counted) is exactly the case where a status dibit is
/// due right as the next FSW would start — see `arm_flywheel`'s docs — so
/// this exercises the ±1 adjustment end to end, not just the collision-free
/// common case.
#[test]
fn a_badly_corrupted_sync_word_is_still_caught_by_the_flywheel() {
    // Build each frame independently — `build_tsdu` calls `insert_status`
    // per frame, so each one's own NID/payload status-dibit positions are
    // correctly local to *its own* FSW, matching how `Framer` resets
    // `since_fs` to 24 on every sync (never a continuing global count).
    // Concatenating them directly would always land the next FSW exactly 24
    // raw dibits later, which is right *unless* the first frame's own
    // length happens to leave a status dibit due exactly there — the
    // 1-block case here — in which case one extra idle status dibit
    // (0b01, TIA-102's fixed idle pattern) is due *before* the second
    // frame's FSW starts, deferred rather than interrupting it (the FSW
    // itself is never interrupted — that part of the invariant holds
    // regardless of the gap length).
    let first = build_tsdu(
        0x293,
        &[(0x00, 0, (0x100Au64 << 40) | (0x2F93u64 << 24) | 0xBEEF1)],
    );
    let second = build_tsdu(
        0x293,
        &[(0x00, 0, (0x100Bu64 << 40) | (0x2F94u64 << 24) | 0xBEEF2)],
    );
    let mut stream = first;
    stream.push(0b01);
    stream.extend(second);

    let sync = sync_dibits();
    let second_at = stream
        .windows(sync.len())
        .rposition(|w| w == sync.as_slice())
        .expect("second frame's FSW not found");

    // Corrupt it: flip 10 scattered bits (of 48). Verified below to be well
    // past what an uninformed search would accept, and inside what the
    // flywheel's relaxed, position-predicted check tolerates.
    let flip_mask: u64 = (0..48).step_by(5).take(10).map(|b| 1u64 << b).sum();
    let mut window: u64 = 0;
    for &d in &sync {
        window = (window << 2) | d as u64;
    }
    let corrupted = window ^ flip_mask;
    let errs = sync_bit_errors(corrupted);
    assert_eq!(errs, 10, "test setup: expected exactly 10 flipped bits");
    // 10 errors in 48 bits is a firmly rejected window for an uninformed
    // search (SYNC_ERR_MAX=2 hard-accepts only up to 2, and the soft rule
    // needs the weighted-bad fraction under 0.16 — with no confidence
    // information at all here, that's a plain 10/48 = 0.208 ratio, which
    // fails it too). It is inside SYNC_ERR_MAX_COAST's soft fraction
    // (0.35 -> up to 16 errors), which only the flywheel's single
    // predicted-position check gets to use.
    for (i, d) in stream[second_at..second_at + sync.len()]
        .iter_mut()
        .enumerate()
    {
        *d = ((corrupted >> (2 * (sync.len() - 1 - i))) & 3) as u8;
    }

    let ev = run(&stream);

    let syncs: Vec<u32> = ev
        .iter()
        .filter_map(|e| match e {
            FramerEvent::Sync { bit_errors } => Some(*bit_errors),
            _ => None,
        })
        .collect();
    let grants: Vec<u16> = ev
        .iter()
        .filter_map(|e| match e {
            FramerEvent::Tsdu { blocks, .. } => blocks.iter().find_map(|b| match &b.tsbk {
                Tsbk::GroupVoiceGrant { group, .. } => Some(*group),
                _ => None,
            }),
            _ => None,
        })
        .collect();
    assert_eq!(
        syncs.len(),
        2,
        "expected two Sync events (clean first, flywheel-recovered second): got {syncs:?}"
    );
    assert_eq!(
        grants,
        vec![0x2F93, 0x2F94],
        "the second TSDU (past the corrupted sync) was not decoded — the \
         flywheel did not recover it"
    );
}

// ---------------------------------------------------------------------------
// Coasting: a lost sync or NID mid-voice must not cost the frame behind it.
// ---------------------------------------------------------------------------

mod coast {
    use hs_p25::framer::{Framer, FramerEvent, LDU_WIRE_DIBITS};
    use hs_p25::synth::{build_ldu1, build_ldu2, build_tdu, build_tsdu};
    use hs_p25::voice::ImbeFrame;
    use hs_p25::Duid;

    fn frames(seed: usize) -> [ImbeFrame; 9] {
        let mut frames: [ImbeFrame; 9] = [[[0u8; 23]; 8]; 9];
        let widths = [23usize, 23, 23, 23, 15, 15, 15, 7];
        for (k, fr) in frames.iter_mut().enumerate() {
            for (w, row) in fr.iter_mut().enumerate() {
                for (x, cell) in row.iter_mut().enumerate().take(widths[w]) {
                    *cell = (((k + seed) * (w + 2) * (x + 5)) % 2) as u8;
                }
            }
        }
        frames
    }

    /// Flip `n` of the first 32 content dibits after the sync (the NID).
    fn wreck_nid(frame: &mut [u8], n: usize) {
        // Status dibit at raw index 35 sits inside the NID span; skip it.
        let mut done = 0;
        let mut i = 24;
        while done < n {
            if i != 35 {
                frame[i] ^= 0b11;
                done += 1;
            }
            i += 1;
        }
    }

    /// Flip `n` dibits of the 24-dibit sync word.
    fn wreck_sync(frame: &mut [u8], n: usize) {
        for d in frame.iter_mut().take(n) {
            *d ^= 0b11;
        }
    }

    fn run(dibits: &[u8]) -> Vec<FramerEvent> {
        let mut f = Framer::new();
        let mut ev = Vec::new();
        for &d in dibits {
            f.push(d, &mut ev);
        }
        ev
    }

    fn ldus(ev: &[FramerEvent]) -> Vec<(Duid, bool, [ImbeFrame; 9])> {
        ev.iter()
            .filter_map(|e| match e {
                FramerEvent::Ldu {
                    duid,
                    inferred,
                    imbe,
                    ..
                } => Some((*duid, *inferred, **imbe)),
                _ => None,
            })
            .collect()
    }

    /// Three clean voice frames establish the NAC; the fourth arrives with a
    /// NID far beyond BCH repair. It is still decoded, as the LDU2 the
    /// cadence says it must be, with its voice frames intact.
    #[test]
    fn a_ruined_nid_mid_voice_still_yields_the_frame() {
        let mut stream = Vec::new();
        stream.extend(build_ldu1(0x293, &frames(1)));
        stream.extend(build_ldu2(0x293, &frames(2), None));
        stream.extend(build_ldu1(0x293, &frames(3)));
        let mut bad = build_ldu2(0x293, &frames(4), None);
        wreck_nid(&mut bad, 20);
        stream.extend(bad);
        stream.extend(build_ldu1(0x293, &frames(5)));
        let got = ldus(&run(&stream));
        assert_eq!(got.len(), 5, "{got:?}");
        assert_eq!(got[3].0, Duid::LogicalLinkDataUnit2);
        assert!(got[3].1, "the fourth frame should be marked inferred");
        assert_eq!(got[3].2, frames(4), "voice frames of the coasted LDU");
        assert!(!got[4].1, "a clean NID ends the coast");
    }

    /// The sync word itself destroyed: the flywheel presumes it, the NID
    /// behind it decodes cleanly, and the frame is a normal decode.
    #[test]
    fn a_ruined_sync_word_mid_voice_is_presumed() {
        let mut stream = Vec::new();
        stream.extend(build_ldu1(0x293, &frames(1)));
        stream.extend(build_ldu2(0x293, &frames(2), None));
        stream.extend(build_ldu1(0x293, &frames(3)));
        let mut bad = build_ldu2(0x293, &frames(4), None);
        wreck_sync(&mut bad, 12);
        stream.extend(bad);
        let got = ldus(&run(&stream));
        assert_eq!(got.len(), 4, "{got:?}");
        assert_eq!(got[3].0, Duid::LogicalLinkDataUnit2);
        assert!(!got[3].1, "a decoded NID is not an inference");
        assert_eq!(got[3].2, frames(4));
    }

    /// The budget: two frames may be inferred in a row, the third is not.
    #[test]
    fn coasting_stops_after_two_frames() {
        let mut stream = Vec::new();
        stream.extend(build_ldu1(0x293, &frames(1)));
        stream.extend(build_ldu2(0x293, &frames(2), None));
        stream.extend(build_ldu1(0x293, &frames(3)));
        for k in 4..7 {
            let mut bad = if k % 2 == 0 {
                build_ldu2(0x293, &frames(k), None)
            } else {
                build_ldu1(0x293, &frames(k))
            };
            wreck_nid(&mut bad, 20);
            stream.extend(bad);
        }
        let got = ldus(&run(&stream));
        assert_eq!(got.len(), 5, "{got:?}");
        assert!(got[3].1 && got[4].1);
    }

    /// Never on a control channel: a TSDU with a ruined NID goes back to the
    /// search, exactly as before coasting existed.
    #[test]
    fn a_control_channel_does_not_coast() {
        let iden = (1u64 << 60) | (100u64 << 51) | (1u64 << 50) | (100u64 << 32) | 170_202_500;
        let mut stream = Vec::new();
        for _ in 0..3 {
            stream.extend(build_tsdu(0x293, &[(0x3D, 0, iden)]));
        }
        let mut bad = build_tsdu(0x293, &[(0x3D, 0, iden)]);
        wreck_nid(&mut bad, 20);
        stream.extend(bad);
        let ev = run(&stream);
        let tsdus = ev
            .iter()
            .filter(|e| matches!(e, FramerEvent::Tsdu { .. }))
            .count();
        assert_eq!(tsdus, 3);
        assert!(ldus(&ev).is_empty());
    }

    /// A presumed frame is abandoned when a real sync word turns up inside
    /// it — here the transmission ended in a terminator right after the
    /// lost-NID frame. The terminator must be seen, and no phantom voice
    /// frame emitted for it.
    #[test]
    fn a_real_sync_inside_a_guessed_frame_wins() {
        let mut stream = Vec::new();
        stream.extend(build_ldu1(0x293, &frames(1)));
        stream.extend(build_ldu2(0x293, &frames(2), None));
        stream.extend(build_ldu1(0x293, &frames(3)));
        // The channel actually sends a terminator whose NID is ruined, so
        // the framer guesses an LDU2 …
        let mut tdu = build_tdu(0x293);
        wreck_nid(&mut tdu, 20);
        stream.extend(tdu);
        // … and then a clean terminator arrives well before the guessed
        // frame would have ended.
        stream.extend(build_tdu(0x293));
        let ev = run(&stream);
        let terminators = ev
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    FramerEvent::Skipped {
                        duid: Duid::TerminatorNoLc,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(terminators, 1, "{ev:?}");
        assert_eq!(ldus(&ev).len(), 3, "no phantom voice frame");
    }

    /// A whole frame's worth of air with no decode is reported as a gap
    /// once the next sync arrives, sized in dibits so a voice consumer can
    /// restore the missing time.
    #[test]
    fn unframed_air_is_reported_as_a_gap() {
        let mut stream = Vec::new();
        stream.extend(build_ldu1(0x293, &frames(1)));
        stream.extend(build_ldu2(0x293, &frames(2), None));
        stream.extend(build_ldu1(0x293, &frames(3)));
        // Exhaust the coast budget first so the loss is reported, not bridged.
        for k in 4..6 {
            let mut bad = if k % 2 == 0 {
                build_ldu2(0x293, &frames(k), None)
            } else {
                build_ldu1(0x293, &frames(k))
            };
            wreck_nid(&mut bad, 20);
            stream.extend(bad);
        }
        // Then one full frame of noise (a pattern no sync detector accepts).
        stream.extend((0..LDU_WIRE_DIBITS).map(|i| ((i * 7 + i / 5) % 4) as u8));
        stream.extend(build_ldu1(0x293, &frames(7)));
        let ev = run(&stream);
        let gap = ev.iter().find_map(|e| match e {
            FramerEvent::Gap { dibits } => Some(*dibits),
            _ => None,
        });
        let gap = gap.expect("no gap reported");
        assert!(
            (gap as i64 - LDU_WIRE_DIBITS as i64).abs() < 40,
            "gap {gap} dibits, expected about one frame ({LDU_WIRE_DIBITS})"
        );
        assert_eq!(ldus(&ev).len(), 6);
    }
}
