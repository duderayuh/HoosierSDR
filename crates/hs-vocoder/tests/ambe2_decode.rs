//! Verify the vendored mbelib AMBE+2 half-rate decoder links, runs, and
//! returns PCM — and that the 72-bit voice-block → deinterleave → vocoder
//! wiring round-trips.
//!
//! As with the IMBE test, arbitrary frames may legitimately decode to silence
//! (mbelib mutes on invalid pitch/voicing), so no audio-oracle assertions here;
//! the point is the FFI, struct layout, buffer sizing, and the deinterleave
//! wiring are correct.

#![cfg(feature = "imbe")]

use hs_p25::p25p2::deinterleave::{interleave, is_transmitted, VoiceFrame};
use hs_vocoder::ambe2::{Ambe2Decoder, SAMPLES_PER_FRAME, VOICE_BLOCK_BITS};
use hs_vocoder::Vocoder;

#[test]
fn runs_and_returns_bounded_frames() {
    let mut dec = Ambe2Decoder::new();
    for f in 0..8u8 {
        let mut frame: VoiceFrame = [[0u8; 24]; 4];
        for (r, row) in frame.iter_mut().enumerate() {
            for (c, cell) in row.iter_mut().enumerate() {
                *cell = ((f as usize * 3 + r * 7 + c) % 2) as u8;
            }
        }
        let pcm = dec.decode(&frame);
        assert_eq!(pcm.len(), SAMPLES_PER_FRAME);
        let _ = pcm.iter().copied().max();
    }
}

#[test]
fn trait_path_matches_direct() {
    let mut dec = Ambe2Decoder::new();
    assert_eq!(dec.name(), "AMBE+2 3600x2450");
    let bits = vec![1u8; VOICE_BLOCK_BITS];
    let mut pcm = Vec::new();
    dec.decode_frame(&bits, &mut pcm).unwrap();
    assert_eq!(pcm.len(), SAMPLES_PER_FRAME);
}

#[test]
fn interleaved_burst_round_trips_through_the_vocoder() {
    // A 72-bit burst built by the interleaver must, when handed to
    // decode_frame, deinterleave back to the source frame and produce the same
    // PCM as decoding the frame directly.
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
    assert!(
        burst.iter().any(|&b| b != 0),
        "burst should not be all zeros"
    );

    let mut direct = Ambe2Decoder::new();
    let expected = direct.decode(&frame);

    let mut dec = Ambe2Decoder::new();
    let mut pcm = Vec::new();
    dec.decode_frame(&burst, &mut pcm).unwrap();
    assert_eq!(pcm.len(), SAMPLES_PER_FRAME);
    assert_eq!(pcm, expected);
}

#[test]
fn rejects_wrong_length() {
    let mut dec = Ambe2Decoder::new();
    let mut pcm = Vec::new();
    assert!(dec.decode_frame(&[0u8; 71], &mut pcm).is_err());
}
