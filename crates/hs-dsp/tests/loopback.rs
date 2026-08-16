//! Modulator → receiver loopback: the receiver must recover the transmitted
//! dibit stream exactly on a clean channel.

use hs_dsp::c4fm::slice;
use hs_dsp::modulator::C4fmModulator;
use hs_dsp::receiver::C4fmReceiver;

#[test]
fn clean_loopback_recovers_dibits() {
    let rate = 48000.0;
    let mut m = C4fmModulator::new(rate);
    let mut rx = C4fmReceiver::new(rate);

    // Pseudo-random dibit payload with a leading alternating preamble so
    // timing can lock before the data of interest.
    let mut dibits = vec![0u8; 0];
    for i in 0..200 {
        dibits.push(if i % 2 == 0 { 0b01 } else { 0b11 });
    }
    let mut lfsr = 0xACE1u16;
    for _ in 0..2000 {
        lfsr = (lfsr >> 1) ^ (if lfsr & 1 != 0 { 0xB400 } else { 0 });
        dibits.push((lfsr & 3) as u8);
    }

    let mut iq = Vec::new();
    for &d in &dibits {
        m.modulate(d, &mut iq);
    }
    // Flush filter tails.
    for _ in 0..200 {
        m.modulate(0b00, &mut iq);
    }

    let mut got = Vec::new();
    for &s in &iq {
        if let Some(sym) = rx.push(s) {
            got.push(slice(sym));
        }
    }

    // Find the payload inside the recovered stream (unknown alignment):
    // correlate against the last 1000 transmitted dibits.
    let needle = &dibits[dibits.len() - 1000..];
    let found = got
        .windows(needle.len())
        .any(|w| w.iter().zip(needle).filter(|(a, b)| a != b).count() == 0);
    assert!(
        found,
        "demodulated stream does not contain transmitted payload (got {} symbols)",
        got.len()
    );
}
