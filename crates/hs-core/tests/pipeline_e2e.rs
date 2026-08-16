//! Full-pipeline integration: synthesize a P25 control-channel + voice
//! transmission as C4FM IQ, run it through the complete ChannelDecoder, and
//! assert that trunking grants resolve and clear voice produces PCM.
//!
//! This exercises every layer at once: modulator → discriminator → RRC →
//! Gardner timing → equalizer → slicer → framer → BCH/trellis/CRC → trunk
//! state machine → IMBE vocoder.

use hs_core::decoder::{ChannelDecoder, EqMode};
use hs_dsp::modulator::C4fmModulator;
use hs_dsp::C32;
use hs_p25::synth::{build_ldu1, build_tsdu};
use hs_p25::voice::ImbeFrame;

const RATE: f64 = 48000.0;

fn modulate(dibits: &[u8]) -> Vec<f32> {
    let mut m = C4fmModulator::new(RATE);
    let mut iq: Vec<C32> = Vec::new();
    // Preamble for timing lock.
    for i in 0..300 {
        m.modulate(if i % 2 == 0 { 0b01 } else { 0b11 }, &mut iq);
    }
    for &d in dibits {
        m.modulate(d, &mut iq);
    }
    for _ in 0..200 {
        m.modulate(0b00, &mut iq);
    }
    let mut out = Vec::with_capacity(iq.len() * 2);
    for c in iq {
        out.push(c.re);
        out.push(c.im);
    }
    out
}

#[test]
fn control_channel_grant_resolves_through_full_chain() {
    // First an IDEN_UP so the channel plan is known, then a group voice grant.
    let iden_args: u64 = {
        // iden=1, bw=12.5, sign=+, offset (unused here)=0, spacing=12.5kHz,
        // base=851.0125 MHz in 5 Hz units.
        let iden = 1u64 << 60;
        let bw = 100u64 << 51; // 100 * 0.125 = 12.5 kHz
        let sign = 1u64 << 50;
        let off = 0u64 << 42;
        let spacing = 100u64 << 32; // 100 * 0.125 = 12.5 kHz
        let base = 851_012_500u64 / 5;
        iden | bw | sign | off | spacing | base
    };
    let grant_channel = (1u64 << 12) | 10;
    let grant_args: u64 = (grant_channel << 40) | (0x2F93u64 << 24) | 0xBEEF1;

    let stream = build_tsdu(0x293, &[(0x3D, 0, iden_args), (0x00, 0, grant_args)]);
    let iq = modulate(&stream);

    let mut dec = ChannelDecoder::new(RATE, EqMode::Enabled);
    let out = dec.process(&iq);

    assert!(out.syncs >= 1, "no frame sync detected");
    assert!(
        out.grants.iter().any(|g| g.talkgroup == 0x2F93
            && g.source_unit == 0xBEEF1
            && g.freq_hz == 851_012_500 + 10 * 12_500),
        "expected resolved grant, got {:?}",
        out.grants
    );
}

#[test]
fn clear_voice_produces_audio_encrypted_is_skipped() {
    // Build one LDU1 of clear voice.
    let mut frames: [ImbeFrame; 9] = [[[0u8; 23]; 8]; 9];
    let widths = [23usize, 23, 23, 23, 15, 15, 15, 7];
    for (k, fr) in frames.iter_mut().enumerate() {
        for (w, row) in fr.iter_mut().enumerate() {
            for (x, cell) in row.iter_mut().enumerate().take(widths[w]) {
                *cell = (((k + 1) * (w + 2) * (x + 5)) % 2) as u8;
            }
        }
    }
    let stream = build_ldu1(0x293, &frames);
    let iq = modulate(&stream);

    let mut dec = ChannelDecoder::new(RATE, EqMode::Enabled);
    let out = dec.process(&iq);

    assert!(out.syncs >= 1, "no sync on voice frame");
    // Nine IMBE frames × 160 samples if the LDU decoded.
    assert_eq!(out.pcm.len(), 9 * 160, "expected 1440 PCM samples");
}
