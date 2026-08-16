//! Full CQPSK pipeline: synthesize a P25 transmission as π/4-DQPSK IQ, run it
//! through the CQPSK ChannelDecoder (carrier + timing recovery → CMA equalizer
//! → differential detection → framer → trunking → IMBE voice), and assert real
//! frames decode — the CQPSK counterpart of the C4FM pipeline test.

use hs_core::decoder::{ChannelDecoder, Modulation};
use hs_dsp::cqpsk::modulate_iq;
use hs_dsp::C32;
use hs_p25::synth::{build_ldu1, build_tsdu};
use hs_p25::voice::ImbeFrame;

const RATE: f64 = 48000.0;
const SPS: usize = 10;
const BETA: f64 = 0.2;

/// Interleave a complex IQ stream to the f32 buffer ChannelDecoder consumes.
fn interleave(iq: &[C32]) -> Vec<f32> {
    let mut out = Vec::with_capacity(iq.len() * 2);
    for c in iq {
        out.push(c.re);
        out.push(c.im);
    }
    out
}

/// A preamble of well-spread dibits so the timing loop and CMA equalizer
/// converge before the real frame arrives.
fn preamble(n: usize) -> Vec<u8> {
    (0..n).map(|i| ((i * 5 + i / 3) % 4) as u8).collect()
}

#[test]
fn cqpsk_control_channel_grant_decodes() {
    let iden_args: u64 = {
        let iden = 1u64 << 60;
        let bw = 100u64 << 51;
        let sign = 1u64 << 50;
        let spacing = 100u64 << 32;
        let base = 851_012_500u64 / 5;
        iden | bw | sign | spacing | base
    };
    let grant_channel = (1u64 << 12) | 10;
    let grant_args: u64 = (grant_channel << 40) | (0x2F93u64 << 24) | 0xBEEF1;

    let mut dibits = preamble(300);
    dibits.extend(build_tsdu(
        0x293,
        &[(0x3D, 0, iden_args), (0x00, 0, grant_args)],
    ));
    dibits.extend(preamble(80)); // flush the receiver pipeline latency

    let iq = modulate_iq(&dibits, SPS, BETA);
    let samples = interleave(&iq);

    let mut dec = ChannelDecoder::new_cqpsk(RATE);
    assert_eq!(dec.modulation(), Modulation::Cqpsk);
    let out = dec.process(&samples);

    assert!(out.syncs >= 1, "no frame sync on CQPSK control channel");
    assert!(
        out.grants
            .iter()
            .any(|g| g.talkgroup == 0x2F93 && g.freq_hz == 851_012_500 + 10 * 12_500),
        "CQPSK grant did not resolve, got {:?}",
        out.grants
    );
}

#[test]
fn cqpsk_voice_frame_decodes_to_audio() {
    let mut frames: [ImbeFrame; 9] = [[[0u8; 23]; 8]; 9];
    let widths = [23usize, 23, 23, 23, 15, 15, 15, 7];
    for (k, fr) in frames.iter_mut().enumerate() {
        for (w, row) in fr.iter_mut().enumerate() {
            for (x, cell) in row.iter_mut().enumerate().take(widths[w]) {
                *cell = (((k + 1) * (w + 2) * (x + 5)) % 2) as u8;
            }
        }
    }
    let mut dibits = preamble(300);
    dibits.extend(build_ldu1(0x293, &frames));
    dibits.extend(preamble(80)); // flush the receiver pipeline latency

    let iq = modulate_iq(&dibits, SPS, BETA);
    let samples = interleave(&iq);

    let mut dec = ChannelDecoder::new_cqpsk(RATE);
    let out = dec.process(&samples);

    assert!(out.syncs >= 1, "no sync on CQPSK voice frame");
    assert_eq!(
        out.pcm.len(),
        9 * 160,
        "expected 1440 PCM samples from CQPSK LDU"
    );
}
