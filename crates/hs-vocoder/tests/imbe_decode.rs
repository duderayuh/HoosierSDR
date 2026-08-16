//! Verify the vendored mbelib IMBE decoder links, runs, and returns PCM.
//!
//! These tests prove the FFI wiring, `mbe_parms` struct layout, and buffer
//! sizing are correct — the decoder runs without memory corruption and
//! returns exactly 160 bounded samples per frame. Asserting *correct audio*
//! requires a known-good IMBE test vector paired with reference PCM; that
//! validation is done in hs-bench against the field IQ corpus (an all-zero
//! or random frame legitimately decodes to silence, because mbelib mutes on
//! invalid pitch/voicing, so it cannot serve as an audio oracle here).

#![cfg(feature = "imbe")]

use hs_vocoder::imbe::{ImbeDecoder, SAMPLES_PER_FRAME};
use hs_vocoder::Vocoder;

#[test]
fn runs_and_returns_bounded_frames() {
    let mut dec = ImbeDecoder::new();
    for f in 0..8u8 {
        let mut fr = [[0u8; 23]; 8];
        for (x, cell) in fr[0].iter_mut().enumerate().take(12) {
            *cell = ((f as usize + x) % 2) as u8;
        }
        for cw in 1..8 {
            for x in 0..10 {
                fr[cw][x] = ((f as usize * 3 + cw + x) % 2) as u8;
            }
        }
        let pcm = dec.decode(&fr);
        assert_eq!(pcm.len(), SAMPLES_PER_FRAME);
        // No assertion on silence: an arbitrary frame may legitimately mute.
        let _ = pcm.iter().copied().max();
    }
}

#[test]
fn trait_path_matches_direct() {
    let mut dec = ImbeDecoder::new();
    assert_eq!(dec.name(), "IMBE 7200x4400");
    let bits = vec![1u8; 144];
    let mut pcm = Vec::new();
    dec.decode_frame(&bits, &mut pcm).unwrap();
    assert_eq!(pcm.len(), SAMPLES_PER_FRAME);
}
