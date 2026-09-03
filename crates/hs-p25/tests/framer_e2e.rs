//! Framer integration: synthesized over-the-air dibit streams (status
//! symbols included) must decode back to the original TSBKs and voice frames.

use hs_p25::framer::{Framer, FramerEvent};
use hs_p25::synth::{build_ldu1, build_tsdu, sync_dibits};
use hs_p25::tsbk::Tsbk;
use hs_p25::voice::ImbeFrame;
use hs_p25::sync_bit_errors;

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
            nac, imbe, algid, ..
        } = e
        {
            assert_eq!(nac, 0x293);
            assert_eq!(algid, None);
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
    let first = build_tsdu(0x293, &[(0x00, 0, (0x100Au64 << 40) | (0x2F93u64 << 24) | 0xBEEF1)]);
    let second = build_tsdu(0x293, &[(0x00, 0, (0x100Bu64 << 40) | (0x2F94u64 << 24) | 0xBEEF2)]);
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
    for (i, d) in stream[second_at..second_at + sync.len()].iter_mut().enumerate() {
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
