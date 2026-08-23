//! End-to-end: a synthesized P25 control-channel grant drives the dual-SDR
//! scheduler to request a retune of the voice radio — through the real
//! modulator → demod → framer → FEC → trunking → grant-resolution chain.

use hs_core::decoder::{ChannelDecoder, EqMode};
use hs_core::dual::{DualSdrFollower, Retune};
use hs_core::priority::PriorityMap;
use hs_dsp::modulator::C4fmModulator;
use hs_dsp::C32;
use hs_p25::synth::build_tsdu;

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
fn control_grant_drives_voice_retune() {
    // IDEN_UP (plan 1: 851.0125 MHz base, 12.5 kHz spacing), then a group
    // voice grant for TG 0x2F93 on channel 10 → 851.1375 MHz.
    let iden: u64 =
        (1u64 << 60) | (100u64 << 51) | (1u64 << 50) | (100u64 << 32) | (851_012_500u64 / 5);
    let grant_channel = (1u64 << 12) | 10;
    let grant: u64 = (grant_channel << 40) | (0x2F93u64 << 24) | 0xBEEF1;
    let stream = build_tsdu(0x293, &[(0x3D, 0, iden), (0x00, 0, grant)]);
    let iq = modulate(&stream);

    let mut prio = PriorityMap::new();
    prio.set_base(0x2F93, 10);

    let control = ChannelDecoder::new(RATE, EqMode::Bypass);
    let mut f = DualSdrFollower::new(control, RATE, prio, RATE);

    let ev = f.process_control(&iq);
    assert!(ev.control_syncs >= 1, "control channel did not sync");
    assert_eq!(
        ev.retune,
        Some(Retune::Tune {
            freq_hz: 851_012_500 + 10 * 12_500,
            talkgroup: 0x2F93,
        }),
        "a clear voice grant should drive a retune of the voice radio"
    );
}
