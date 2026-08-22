//! Framer integration: synthesized over-the-air dibit streams (status
//! symbols included) must decode back to the original TSBKs and voice frames.

use hs_p25::framer::{Framer, FramerEvent};
use hs_p25::synth::{build_ldu1, build_tsdu};
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
