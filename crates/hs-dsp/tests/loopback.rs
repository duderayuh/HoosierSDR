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

/// A residual carrier/tuner frequency offset — an uncalibrated dongle, or
/// simply not having retuned exactly on frequency — shows up on an FM
/// discriminator as a constant bias added to every sample. Without tracking
/// and removing it (`c4fm::DcTracker`), that bias pushes the four ±1/±3
/// levels off-centre and confuses `slice`'s fixed thresholds; this is the
/// receiver-level proof that the fix actually recovers the payload.
#[test]
fn frequency_offset_still_recovers_dibits() {
    let rate = 48000.0;
    let mut m = C4fmModulator::new(rate);
    let mut rx = C4fmReceiver::new(rate);

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
    for _ in 0..200 {
        m.modulate(0b00, &mut iq);
    }

    // Rotate the whole IQ stream by a constant 600 Hz — a real, if generous,
    // tuner error (see `hs_core::follow`'s own notes: "an uncalibrated
    // dongle can sit 6 kHz off" the demodulator's much smaller ±1 kHz
    // tolerance; 600 Hz is a third of DEVIATION_MAX_HZ, biasing the inner
    // ±1 levels by a full unit — enough to land one of them exactly on a
    // slicer threshold if left uncorrected).
    let offset_hz = 600.0;
    let w = 2.0 * std::f64::consts::PI * offset_hz / rate;
    let (sin, cos) = (w.sin(), w.cos());
    let (mut nco_re, mut nco_im) = (1.0f64, 0.0f64);
    for s in iq.iter_mut() {
        let (re, im) = (s.re as f64, s.im as f64);
        *s = hs_dsp::C32::new(
            (re * nco_re - im * nco_im) as f32,
            (re * nco_im + im * nco_re) as f32,
        );
        let (nre, nim) = (nco_re * cos - nco_im * sin, nco_re * sin + nco_im * cos);
        nco_re = nre;
        nco_im = nim;
    }

    let mut got = Vec::new();
    for &s in &iq {
        if let Some(sym) = rx.push(s) {
            got.push(slice(sym));
        }
    }

    let needle = &dibits[dibits.len() - 1000..];
    let found = got
        .windows(needle.len())
        .any(|w| w.iter().zip(needle).filter(|(a, b)| a != b).count() == 0);
    assert!(
        found,
        "demodulated stream does not contain transmitted payload under a \
         600 Hz frequency offset (got {} symbols, tracked bias {})",
        got.len(),
        rx.freq_offset_bias()
    );
}
