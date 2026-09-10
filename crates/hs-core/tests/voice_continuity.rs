//! Voice continuity through the whole single-channel decoder: what reaches
//! the audio buffer when the air is imperfect. Each test synthesizes a C4FM
//! transmission frame by frame, damages it in one specific way, and checks
//! the audio that comes out — in *time*, not just in frames, because a lost
//! 180 ms that is simply deleted from a call is the stutter a listener hears.

use hs_core::decoder::{ChannelDecoder, EncState, EqMode};
use hs_dsp::modulator::C4fmModulator;
use hs_dsp::C32;
use hs_p25::ess::{Ess, ALGID_CLEAR};
use hs_p25::synth::{build_ldu1, build_ldu2, build_tdu};
use hs_p25::voice::ImbeFrame;

const RATE: f64 = 48_000.0;
const LDU_PCM: usize = 9 * 160;

/// Nine voice frames of pseudo-random bits. Random, not patterned: real
/// voice is, and a periodic pattern is structure the receiver's DC and
/// level trackers would follow instead of the signal.
fn frames(seed: usize) -> [ImbeFrame; 9] {
    let mut frames: [ImbeFrame; 9] = [[[0u8; 23]; 8]; 9];
    let widths = [23usize, 23, 23, 23, 15, 15, 15, 7];
    let mut state = 0x9E37_79B9u64.wrapping_mul(seed as u64 + 1);
    let mut bit = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 40 & 1) as u8
    };
    for fr in frames.iter_mut() {
        for (w, row) in fr.iter_mut().enumerate() {
            for cell in row.iter_mut().take(widths[w]) {
                *cell = bit();
            }
        }
    }
    frames
}

/// Flip `n` content dibits of the NID (skipping the status dibit at 35).
fn wreck_nid(frame: &mut [u8], n: usize) {
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

/// One frame's worth of noise: a dibit pattern no sync detector accepts.
fn noise_frame(seed: u32) -> Vec<u8> {
    (0..hs_p25::framer::LDU_WIRE_DIBITS)
        .map(|i| ((i * 7 + i / 5 + seed) % 4) as u8)
        .collect()
}

struct Run {
    /// PCM samples produced per input frame, plus a final flush entry.
    per_frame: Vec<usize>,
    total: usize,
    inferred: u64,
    concealed: u64,
    ess_valid: u64,
    ess_invalid: u64,
    enc: EncState,
}

fn run(frames_on_air: &[Vec<u8>], grant_clear: bool) -> Run {
    let mut m = C4fmModulator::new(RATE);
    let mut dec = ChannelDecoder::new(RATE, EqMode::Bypass);
    if grant_clear {
        dec.set_grant_clear(true);
    }
    let feed = |dec: &mut ChannelDecoder, iq: &mut Vec<C32>| -> usize {
        let mut f = Vec::with_capacity(iq.len() * 2);
        for c in iq.drain(..) {
            f.push(c.re);
            f.push(c.im);
        }
        dec.process(&f).pcm.len()
    };
    let mut iq: Vec<C32> = Vec::new();
    for i in 0..400 {
        m.modulate(if i % 2 == 0 { 0b01 } else { 0b11 }, &mut iq);
    }
    feed(&mut dec, &mut iq);
    let mut per_frame = Vec::new();
    for f in frames_on_air {
        for &d in f {
            m.modulate(d, &mut iq);
        }
        per_frame.push(feed(&mut dec, &mut iq));
    }
    for i in 0..200 {
        m.modulate(if i % 2 == 0 { 0b01 } else { 0b11 }, &mut iq);
    }
    per_frame.push(feed(&mut dec, &mut iq));
    let d = dec.diagnostics();
    Run {
        total: per_frame.iter().sum(),
        per_frame,
        inferred: d.voice_frames_inferred,
        concealed: d.voice_frames_concealed,
        ess_valid: d.ess_valid,
        ess_invalid: d.ess_invalid,
        enc: dec.encryption(),
    }
}

fn voice(n: usize) -> Vec<Vec<u8>> {
    (1..=n)
        .map(|k| {
            if k % 2 == 1 {
                build_ldu1(0x293, &frames(k))
            } else {
                build_ldu2(0x293, &frames(k), None)
            }
        })
        .collect()
}

/// The bug that started this: one flipped bit in the ALGID of one LDU2 used
/// to read as "encrypted" and mute that frame and every LDU1 after it —
/// half the transmission gone, in 180 ms pieces. Now the Hamming layer
/// repairs the bit and every frame plays.
#[test]
fn one_algid_bit_error_costs_no_audio() {
    let clean = run(&voice(8), true);
    assert_eq!(clean.total, 8 * LDU_PCM, "clean: {:?}", clean.per_frame);
    assert_eq!(clean.inferred, 0, "clean air needed no coasting");

    let mut air = voice(8);
    // Hexbit 12 of the ESS starts at payload bit 840: its MSB is ALGID's
    // 0x80 bit. Payload dibit index = 28 (sync+NID) + bit/2, plus the
    // status dibits inserted before it (one per 36 raw).
    let content_dibit = 28 + 840 / 2;
    let raw = content_dibit + content_dibit / 35;
    air[3][raw] ^= 0b10;
    let hit = run(&air, true);
    assert_eq!(
        hit.total,
        8 * LDU_PCM,
        "with one ALGID bit error: {:?}",
        hit.per_frame
    );
    assert_eq!(hit.enc, EncState::Clear);
    assert!(hit.ess_valid >= 3, "ESS validated {} of 4", hit.ess_valid);
}

/// The gate itself is intact: an Encryption Sync that validates and names an
/// algorithm mutes the transmission, and one that validates clear restores
/// it. Without a grant the first LDU1 plays, since nothing has said
/// otherwise yet — exactly what the decoder always did.
#[test]
fn a_validated_encryption_sync_still_mutes() {
    let aes = Ess {
        mi: [1, 2, 3, 4, 5, 6, 7, 8, 9],
        algid: 0x84,
        kid: 7,
    };
    let clear = Ess {
        mi: [0; 9],
        algid: ALGID_CLEAR,
        kid: 0,
    };
    let air = vec![
        build_ldu1(0x293, &frames(1)),
        build_ldu2(0x293, &frames(2), Some(aes)),
        build_ldu1(0x293, &frames(3)),
        build_ldu2(0x293, &frames(4), Some(aes)),
        build_ldu1(0x293, &frames(5)),
        build_ldu2(0x293, &frames(6), Some(clear)),
        build_ldu1(0x293, &frames(7)),
    ];
    let r = run(&air, false);
    assert_eq!(r.ess_valid, 3, "{r:?}", r = r.per_frame);
    // Frame 1 plays (unknown yet), 2–5 are muted, 6 and 7 play.
    assert_eq!(r.total, 3 * LDU_PCM, "{:?}", r.per_frame);
    assert_eq!(r.enc, EncState::Clear);

    // A terminator ends the transmission and the verdict with it.
    let mut air2 = air.clone();
    air2.truncate(4);
    air2.push(build_tdu(0x293));
    air2.push(build_ldu1(0x293, &frames(9)));
    let r2 = run(&air2, false);
    assert_eq!(r2.total, 2 * LDU_PCM, "{:?}", r2.per_frame);
}

/// A frame whose NID was destroyed is still decoded, on the LDU1/LDU2
/// cadence, so the audio is complete.
#[test]
fn a_lost_nid_mid_voice_costs_no_audio() {
    let mut air = voice(6);
    wreck_nid(&mut air[3], 20);
    let r = run(&air, true);
    assert_eq!(r.total, 6 * LDU_PCM, "{:?}", r.per_frame);
    assert_eq!(r.inferred, 9, "{:?}", r.per_frame);
}

/// A whole frame of air with nothing decodable, once the coast budget is
/// spent, comes back as 180 ms of concealment audio rather than being cut
/// out of the call: the timeline is honest.
#[test]
fn unframed_air_becomes_concealment_not_deleted_time() {
    let mut air = voice(5);
    wreck_nid(&mut air[3], 20);
    wreck_nid(&mut air[4], 20);
    air.push(noise_frame(3));
    air.push(build_ldu2(0x293, &frames(8), None));
    air.push(build_ldu1(0x293, &frames(9)));
    let r = run(&air, true);
    assert_eq!(r.inferred, 18, "two coasted frames");
    assert_eq!(r.concealed, 9, "one frame of concealment for the noise");
    assert_eq!(r.total, 7 * LDU_PCM + 9 * 160, "{:?}", r.per_frame);
    assert!(r.ess_invalid <= 2);
}
