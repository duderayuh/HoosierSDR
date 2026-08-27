//! Digital-Coded Squelch (DCS / DPL) decode over an NBFM channel.
//!
//! DCS transmits a continuous 23-bit word at 134.4 bit/s as sub-audible data
//! below the 300 Hz voice band. Each word is a binary Golay(23,12) codeword
//! whose 12 data bits carry a 9-bit octal code plus a fixed 3-bit field; the
//! remaining 11 bits are Golay parity, giving 3-bit error correction.
//!
//! This decoder runs the ordinary NBFM audio path (so the channel is still
//! listenable) and, in parallel, extracts the sub-audible bitstream, recovers
//! word alignment, Golay-corrects each word, and reports the octal code.
//!
//! Framing convention (documented, and used by this crate's encoder so the
//! round-trip test is self-consistent): bits are transmitted LSB first; the
//! 12 Golay data bits are `(fixed3 << 9) | code9` with `fixed3 = 0b100`; the
//! codeword is `(data12 << 11) | parity`. This matches the most commonly
//! published DCS description. Polarity is auto-detected: a channel whose fixed
//! field decodes to the complement is the inverted ("N") variant.
//!
//! Field validation against off-air DCS is still pending — no captures are
//! committed to this repository (synthetic fixtures only), so the round-trip
//! test is the current correctness evidence.

use hs_dsp::fir::{lowpass_taps, Fir};
use hs_dsp::fm::FmDemod;
use hs_dsp::C32;

use crate::frontend::{AudioAgc, AudioResampler, Ddc, NoiseSquelch};
use crate::{DecoderEvent, DecoderKind, DecoderOutput, SignalDecoder};

pub mod golay;

/// DCS symbol rate.
const DCS_BAUD: f64 = 134.4;
/// The fixed 3-bit field in every DCS data word (upper 3 of the 12 data bits).
const FIXED3: u32 = 0b100;

/// Encode a 9-bit DCS octal code into its 23-bit transmitted word (bit 0 =
/// first transmitted). Shared with the decoder's round-trip test and usable by
/// a future DCS modulator.
pub fn encode_word(code9: u32) -> u32 {
    let data12 = (FIXED3 << 9) | (code9 & 0x1FF);
    golay::encode(data12)
}

pub struct DcsDecoder {
    // --- audio path (identical to NBFM) ---
    ddc: Ddc,
    fm: FmDemod,
    squelch: NoiseSquelch,
    audio: AudioResampler,
    agc: AudioAgc,
    was_open: bool,
    // --- sub-audible DCS path ---
    /// Isolates the <300 Hz DCS band from the discriminator output.
    sub_lp: Fir,
    /// Removes residual carrier/DC offset so the slicer centres on zero.
    dc: f32,
    dc_a: f32,
    /// Fractional bit-phase accumulator (advances by baud/working per sample).
    mu: f32,
    mu_step: f32,
    /// Integrate-and-dump accumulator over the current bit.
    acc: f32,
    acc_n: u32,
    prev_sub: f32,
    /// Rolling window of recovered bits (newest pushed at the back).
    bits: std::collections::VecDeque<u8>,
    /// Running count of recovered bits, for word-phase tracking.
    bit_idx: u32,
    /// Per-phase (0..23) exact-codeword hit tally: the last code seen at that
    /// phase and how many times in a row it has recurred. The true word
    /// boundary is the phase that yields an exact Golay codeword (zero
    /// syndrome) with the fixed field intact, period after period.
    phase_hits: [(Option<(u16, bool)>, u32); 23],
    /// Last code reported, to emit only on change.
    last_code: Option<(u16, bool)>,
}

impl DcsDecoder {
    pub fn new(capture_rate: f64, offset_hz: f64, squelch_level: f32) -> Self {
        let ddc = Ddc::new(capture_rate, offset_hz, 6_000.0);
        let working = ddc.working_rate();
        let audio = AudioResampler::new(working, ddc.audio_decim());
        // Isolate the DCS band: pass to ~250 Hz, well below the 300 Hz voice
        // floor. A wide transition keeps the tap count bounded.
        let cutoff = 250.0 / working;
        let transition = (450.0 / working - cutoff).max(2e-3);
        let mut n = (3.3 / transition).ceil() as usize;
        n = n.clamp(31, 1023);
        if n.is_multiple_of(2) {
            n += 1;
        }
        Self {
            ddc,
            fm: FmDemod::new(),
            squelch: NoiseSquelch::new(working, squelch_level),
            audio,
            agc: AudioAgc::new(),
            was_open: false,
            sub_lp: Fir::new(lowpass_taps(n, cutoff)),
            dc: 0.0,
            dc_a: 0.001,
            mu: 0.0,
            mu_step: (DCS_BAUD / working) as f32,
            acc: 0.0,
            acc_n: 0,
            prev_sub: 0.0,
            bits: std::collections::VecDeque::with_capacity(64),
            bit_idx: 0,
            phase_hits: [(None, 0); 23],
            last_code: None,
        }
    }

    /// Feed one recovered sub-audible bit and try to lock a DCS code.
    fn on_bit(&mut self, bit: u8, out: &mut DecoderOutput) {
        if self.bits.len() == 23 {
            self.bits.pop_front();
        }
        self.bits.push_back(bit);
        let phase = (self.bit_idx % 23) as usize;
        self.bit_idx = self.bit_idx.wrapping_add(1);
        if self.bits.len() < 23 {
            return;
        }
        // Assemble the current 23-bit window (oldest bit = bit 0 = first
        // transmitted).
        let mut cw: u32 = 0;
        for (i, &b) in self.bits.iter().enumerate() {
            cw |= (b as u32) << i;
        }
        // Only an exact Golay codeword (zero corrections) with the DCS fixed
        // field marks a true word boundary; wrong alignments need corrections.
        let Some((code, inverted, errs)) = decode_word(cw) else {
            self.phase_hits[phase] = (None, 0);
            return;
        };
        if errs != 0 {
            self.phase_hits[phase] = (None, 0);
            return;
        }
        let key = (code, inverted);
        let slot = &mut self.phase_hits[phase];
        if slot.0 == Some(key) {
            slot.1 += 1;
        } else {
            *slot = (Some(key), 1);
        }
        // Two clean periods at the same phase is a confident lock.
        if slot.1 >= 2 && self.last_code != Some(key) {
            self.last_code = Some(key);
            out.events.push(DecoderEvent::Dcs { code, inverted });
        }
    }
}

impl SignalDecoder for DcsDecoder {
    fn process(&mut self, iq: &[f32]) -> DecoderOutput {
        let mut out = DecoderOutput::default();
        let mut i = 0;
        while i + 1 < iq.len() {
            let s = C32::new(iq[i], iq[i + 1]);
            i += 2;
            let Some(w) = self.ddc.push(s) else { continue };
            let disc = self.fm.demod(w);

            // --- audio path ---
            let open = self.squelch.update(disc);
            if open != self.was_open {
                out.events.push(if open {
                    DecoderEvent::SquelchOpen
                } else {
                    DecoderEvent::SquelchClose
                });
                self.was_open = open;
            }
            if let Some(a) = self.audio.push(disc) {
                if open {
                    out.audio.push(self.agc.sample(a));
                }
            }

            // --- sub-audible DCS path ---
            let sub = self.sub_lp.filter(disc);
            self.dc += self.dc_a * (sub - self.dc);
            let centred = sub - self.dc;
            // Integrate over the bit; transition-based clock correction nudges
            // the phase so the dump boundary sits between symbols.
            self.acc += centred;
            self.acc_n += 1;
            if (centred >= 0.0) != (self.prev_sub >= 0.0) {
                // A zero crossing should fall at a bit boundary (mu≈0); pull the
                // phase gently toward it.
                self.mu -= 0.05 * self.mu;
            }
            self.prev_sub = centred;
            self.mu += self.mu_step;
            if self.mu >= 1.0 {
                self.mu -= 1.0;
                if self.acc_n > 0 {
                    let bit = (self.acc >= 0.0) as u8;
                    self.on_bit(bit, &mut out);
                }
                self.acc = 0.0;
                self.acc_n = 0;
            }
        }
        out
    }

    fn kind(&self) -> DecoderKind {
        DecoderKind::Dcs
    }
}

/// Decode a 23-bit candidate (bit 0 = first transmitted) into `(octal code,
/// inverted, bit errors)` if the fixed field checks out, trying both
/// polarities. Returns `None` when neither polarity yields the fixed field.
fn decode_word(cw: u32) -> Option<(u16, bool, u32)> {
    for inverted in [false, true] {
        let c = if inverted { (!cw) & 0x7F_FFFF } else { cw };
        let (data12, errs) = golay::decode(c);
        let fixed = (data12 >> 9) & 0b111;
        if fixed == FIXED3 {
            let code9 = data12 & 0x1FF;
            return Some((code9 as u16, inverted, errs));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_round_trips_through_golay() {
        for code in [0o023u32, 0o754, 0o131, 0o000, 0o777] {
            let cw = encode_word(code);
            let (dc, inv, errs) = decode_word(cw).expect("decodes");
            assert_eq!(dc as u32, code, "code {code:o}");
            assert!(!inv);
            assert_eq!(errs, 0);
            // Flip 3 bits — Golay must still correct.
            let corrupted = cw ^ 0b0000_0000_0000_0000_0010_1001;
            let (dc, _, errs) = decode_word(corrupted).expect("corrects 3 errors");
            assert_eq!(dc as u32, code, "3-error code {code:o}");
            assert!(errs <= 3);
        }
    }

    /// FM-modulate a continuously-repeated DCS word as sub-audible data and
    /// confirm the decoder recovers the code end to end.
    #[test]
    fn recovers_code_from_modulated_signal() {
        let fs = 240_000.0;
        let code = 0o754u32;
        let cw = encode_word(code);
        let bits: Vec<u8> = (0..23).map(|i| ((cw >> i) & 1) as u8).collect();

        // Build a sub-audible NRZ message at 134.4 baud and FM-modulate it with
        // a small deviation, carrier 25 kHz off centre.
        let carrier = 25_000.0;
        let dev = 500.0; // Hz peak, typical DCS deviation
        let n = 600_000;
        let mut iq = Vec::with_capacity(n * 2);
        let mut phase = 0.0f64;
        for k in 0..n {
            let bit_idx = ((k as f64 / fs) * DCS_BAUD) as usize;
            let b = bits[bit_idx % bits.len()];
            let msg = if b == 1 { 1.0 } else { -1.0 };
            phase += 2.0 * std::f64::consts::PI * (carrier + dev * msg) / fs;
            iq.push(phase.cos() as f32);
            iq.push(phase.sin() as f32);
        }

        let mut dec = DcsDecoder::new(fs, carrier, 0.3);
        let out = dec.process(&iq);
        let found = out.events.iter().any(|e| {
            matches!(e, DecoderEvent::Dcs { code: c, inverted: false } if *c as u32 == code)
        });
        assert!(found, "DCS code {code:o} not decoded; events: {:?}", out.events);
    }
}
