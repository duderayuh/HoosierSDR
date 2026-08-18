//! End-to-end: a P25 signal placed at an offset inside a wideband stream must
//! survive the channelizer and decode. This is the path trunk-following takes,
//! so a silent failure here would mean calls that never appear.

use hs_core::decoder::{ChannelDecoder, EqMode, Modulation};
use hs_dsp::channelizer::Channelizer;
use hs_dsp::cqpsk::modulate_iq;
use hs_p25::synth::build_tsdu;

const WIDE: f64 = 240_000.0;
const OFFSET: f64 = 50_000.0;

fn control_dibits() -> Vec<u8> {
    let iden_args: u64 = {
        let iden = 1u64 << 60;
        let bw = 100u64 << 51;
        let sign = 1u64 << 50;
        let spacing = 100u64 << 32;
        iden | bw | sign | spacing | (851_012_500u64 / 5)
    };
    let grant_args: u64 = (((1u64 << 12) | 10) << 40) | (0x2F93u64 << 24) | 0xBEEF1;
    let mut d: Vec<u8> = (0..1200).map(|i| ((i * 5 + i / 3) % 4) as u8).collect();
    for _ in 0..8 {
        d.extend(build_tsdu(
            0x293,
            &[(0x3D, 0, iden_args), (0x00, 0, grant_args)],
        ));
        d.extend((0..40).map(|i| ((i * 5 + i / 3) % 4) as u8));
    }
    d
}

/// Modulate at the wideband rate and shift up to `OFFSET`.
fn wideband_signal() -> Vec<f32> {
    let sps = (WIDE / 4800.0) as usize;
    let base = modulate_iq(&control_dibits(), sps, 0.2);
    let mut iq = Vec::with_capacity(base.len() * 2);
    for (n, s) in base.iter().enumerate() {
        let w = 2.0 * std::f64::consts::PI * OFFSET * n as f64 / WIDE;
        let (sin, cos) = (w.sin() as f32, w.cos() as f32);
        iq.push(s.re * cos - s.im * sin);
        iq.push(s.re * sin + s.im * cos);
    }
    iq
}

#[test]
fn channelized_signal_still_decodes() {
    let iq = wideband_signal();

    let mut ch = Channelizer::new(WIDE, &[OFFSET]);
    assert_eq!(ch.output_rate(), 48_000.0);
    let chan = ch.process(&iq).remove(0);
    assert!(!chan.is_empty(), "channelizer produced no output");

    let mut dec =
        ChannelDecoder::with_offset(ch.output_rate(), Modulation::Cqpsk, EqMode::Enabled, 0.0);
    let out = dec.process(&chan);

    assert!(
        out.syncs >= 1,
        "no frame sync through the channelizer (got {} syncs from {} samples)",
        out.syncs,
        chan.len() / 2
    );
    assert!(
        out.grants.iter().any(|g| g.talkgroup == 0x2F93),
        "grant lost through the channelizer: {:?}",
        out.grants
    );
}

/// The same signal decoded by tuning directly, as a control: if this fails the
/// test signal is at fault, not the channelizer.
#[test]
fn the_same_signal_decodes_without_the_channelizer() {
    let iq = wideband_signal();
    let mut dec = ChannelDecoder::with_offset(WIDE, Modulation::Cqpsk, EqMode::Enabled, OFFSET);
    let out = dec.process(&iq);
    assert!(
        out.syncs >= 1,
        "direct decode failed: the test signal is bad"
    );
}
