//! Narrow-band FM voice demodulator with a noise squelch.
//!
//! Chain: DDC to the working rate → FM discriminator → noise squelch →
//! audio-rate resample → level normalize. Squelch open/close transitions are
//! reported as events; audio only flows while the squelch is open.

use hs_dsp::fm::FmDemod;
use hs_dsp::C32;

use crate::frontend::{AudioAgc, AudioResampler, Ddc, NoiseSquelch};
use crate::{DecoderEvent, DecoderKind, DecoderOutput, SignalDecoder};

pub struct NbfmDecoder {
    ddc: Ddc,
    fm: FmDemod,
    squelch: NoiseSquelch,
    audio: AudioResampler,
    agc: AudioAgc,
    was_open: bool,
}

impl NbfmDecoder {
    /// `squelch_level` in 0..1: 0 opens easily, 1 demands a clean signal.
    pub fn new(capture_rate: f64, offset_hz: f64, squelch_level: f32) -> Self {
        // A 12.5 kHz NBFM channel occupies roughly ±6 kHz (±5 kHz deviation
        // plus audio); keep that and reject the neighbours.
        let ddc = Ddc::new(capture_rate, offset_hz, 6_000.0);
        let working = ddc.working_rate();
        let audio = AudioResampler::new(working, ddc.audio_decim());
        Self {
            ddc,
            fm: FmDemod::new(),
            squelch: NoiseSquelch::new(working, squelch_level),
            audio,
            agc: AudioAgc::new(),
            was_open: false,
        }
    }
}

impl SignalDecoder for NbfmDecoder {
    fn process(&mut self, iq: &[f32]) -> DecoderOutput {
        let mut out = DecoderOutput::default();
        let mut i = 0;
        while i + 1 < iq.len() {
            let s = C32::new(iq[i], iq[i + 1]);
            i += 2;
            let Some(w) = self.ddc.push(s) else { continue };
            let disc = self.fm.demod(w);
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
        }
        out
    }

    fn kind(&self) -> DecoderKind {
        DecoderKind::Nbfm
    }
}

#[cfg(test)]
pub(crate) mod test_signals {
    /// Synthesize an NBFM signal: a carrier `carrier_hz` off centre, FM'd by a
    /// `tone_hz` sinusoid with peak deviation `dev_hz`, at amplitude `amp`,
    /// with white noise of standard deviation `noise`. Returns interleaved IQ.
    ///
    /// A tiny deterministic LCG makes the noise reproducible without pulling in
    /// an RNG crate.
    pub fn nbfm_iq(
        sample_rate: f64,
        carrier_hz: f64,
        tone_hz: f64,
        dev_hz: f64,
        amp: f64,
        noise: f64,
        n: usize,
    ) -> Vec<f32> {
        let mut iq = Vec::with_capacity(n * 2);
        let mut phase = 0.0f64;
        let mut rng: u64 = 0x1234_5678_9abc_def0;
        let mut gauss = || {
            // Two uniform draws → one approximately-normal via the central
            // limit of 4 samples; enough for a squelch-threshold test.
            let mut s = 0.0f64;
            for _ in 0..4 {
                rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                s += ((rng >> 33) as f64 / (1u64 << 31) as f64) - 1.0;
            }
            s * 0.5
        };
        for k in 0..n {
            let t = k as f64 / sample_rate;
            let msg = (2.0 * std::f64::consts::PI * tone_hz * t).sin();
            // Integrate the message for FM phase.
            phase += 2.0 * std::f64::consts::PI * (carrier_hz + dev_hz * msg) / sample_rate;
            let i = amp * phase.cos() + noise * gauss();
            let q = amp * phase.sin() + noise * gauss();
            iq.push(i as f32);
            iq.push(q as f32);
        }
        iq
    }
}

#[cfg(test)]
mod tests {
    use super::test_signals::nbfm_iq;
    use super::*;

    fn dominant_hz(pcm: &[i16], rate: f64) -> f64 {
        let mut crossings = 0;
        for w in pcm.windows(2) {
            if (w[0] < 0) != (w[1] < 0) {
                crossings += 1;
            }
        }
        crossings as f64 / 2.0 / (pcm.len() as f64 / rate)
    }

    #[test]
    fn recovers_tone_and_opens_squelch() {
        let fs = 240_000.0;
        // Strong clean signal, 1 kHz tone, ±3 kHz deviation, 25 kHz off centre.
        let iq = nbfm_iq(fs, 25_000.0, 1_000.0, 3_000.0, 1.0, 0.0, 300_000);
        let mut dec = NbfmDecoder::new(fs, 25_000.0, 0.3);
        let out = dec.process(&iq);
        assert!(
            out.events.contains(&DecoderEvent::SquelchOpen),
            "squelch never opened"
        );
        assert!(out.audio.len() > 2_000, "audio {}", out.audio.len());
        let tail = &out.audio[out.audio.len() / 3..];
        let f = dominant_hz(tail, crate::AUDIO_RATE as f64);
        assert!((f - 1_000.0).abs() < 80.0, "dominant {f:.0} Hz");
    }

    #[test]
    fn squelch_stays_closed_on_noise() {
        let fs = 240_000.0;
        // No carrier — pure noise. Squelch must not open.
        let iq = nbfm_iq(fs, 25_000.0, 1_000.0, 3_000.0, 0.0, 1.0, 300_000);
        let mut dec = NbfmDecoder::new(fs, 25_000.0, 0.3);
        let out = dec.process(&iq);
        assert!(
            !out.events.contains(&DecoderEvent::SquelchOpen),
            "squelch opened on noise"
        );
        assert!(out.audio.is_empty(), "leaked {} noise samples", out.audio.len());
    }
}
