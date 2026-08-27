//! AM envelope demodulator.
//!
//! Chain: DDC to the working rate → magnitude (envelope) detection → DC block
//! to strip the carrier term → audio-rate resample → level normalize. AM here
//! runs open (no squelch): AM users typically monitor with the squelch off,
//! and there is no sub-audible signalling to key it.

use hs_dsp::C32;

use crate::frontend::{AudioAgc, AudioResampler, Ddc};
use crate::{DecoderEvent, DecoderKind, DecoderOutput, SignalDecoder};

pub struct AmDecoder {
    ddc: Ddc,
    /// One-pole DC blocker on the real envelope removes the carrier's constant
    /// term, leaving the audio.
    dc: DcBlockerF,
    audio: AudioResampler,
    agc: AudioAgc,
    announced: bool,
}

impl AmDecoder {
    pub fn new(capture_rate: f64, offset_hz: f64) -> Self {
        // AM voice channels are typically 8–10 kHz wide (aviation is ~8 kHz).
        let ddc = Ddc::new(capture_rate, offset_hz, 5_000.0);
        let audio = AudioResampler::new(ddc.working_rate(), ddc.audio_decim());
        Self {
            ddc,
            dc: DcBlockerF::new(0.995),
            audio,
            agc: AudioAgc::new(),
            announced: false,
        }
    }
}

impl SignalDecoder for AmDecoder {
    fn process(&mut self, iq: &[f32]) -> DecoderOutput {
        let mut out = DecoderOutput::default();
        if !self.announced {
            out.events.push(DecoderEvent::SquelchOpen);
            self.announced = true;
        }
        let mut i = 0;
        while i + 1 < iq.len() {
            let s = C32::new(iq[i], iq[i + 1]);
            i += 2;
            let Some(w) = self.ddc.push(s) else { continue };
            let env = w.norm_sq().sqrt();
            let audio = self.dc.push(env);
            if let Some(a) = self.audio.push(audio) {
                out.audio.push(self.agc.sample(a));
            }
        }
        out
    }

    fn kind(&self) -> DecoderKind {
        DecoderKind::Am
    }
}

/// Single-pole DC blocker for a real stream: y[n] = x[n] − x[n−1] + a·y[n−1].
struct DcBlockerF {
    a: f32,
    x1: f32,
    y1: f32,
}

impl DcBlockerF {
    fn new(a: f32) -> Self {
        Self { a, x1: 0.0, y1: 0.0 }
    }
    fn push(&mut self, x: f32) -> f32 {
        let y = x - self.x1 + self.a * self.y1;
        self.x1 = x;
        self.y1 = y;
        y
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthesize an AM signal: carrier at `carrier_hz` from centre, amplitude
    /// modulated by a `tone_hz` sinusoid at modulation depth `m`.
    fn am_iq(sample_rate: f64, carrier_hz: f64, tone_hz: f64, m: f64, n: usize) -> Vec<f32> {
        let mut iq = Vec::with_capacity(n * 2);
        for k in 0..n {
            let t = k as f64 / sample_rate;
            let env = 1.0 + m * (2.0 * std::f64::consts::PI * tone_hz * t).sin();
            let ph = 2.0 * std::f64::consts::PI * carrier_hz * t;
            iq.push((env * ph.cos()) as f32);
            iq.push((env * ph.sin()) as f32);
        }
        iq
    }

    /// Dominant audio frequency via zero-crossing rate on the decoded PCM.
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
    fn recovers_a_1khz_tone() {
        let fs = 240_000.0;
        // Carrier 30 kHz off centre; decode with a matching offset.
        let iq = am_iq(fs, 30_000.0, 1_000.0, 0.6, 240_000);
        let mut dec = AmDecoder::new(fs, 30_000.0);
        let out = dec.process(&iq);
        assert!(out.audio.len() > 1_000, "got {} samples", out.audio.len());
        // Skip the AGC/DC warm-up transient.
        let tail = &out.audio[out.audio.len() / 3..];
        let f = dominant_hz(tail, crate::AUDIO_RATE as f64);
        assert!((f - 1_000.0).abs() < 60.0, "dominant {f:.0} Hz");
    }
}
