//! Bridge the pluggable `hs-decoders` family (AM, NBFM, DCS, …) into the CLI
//! and desktop app, and adapt the P25 [`ChannelDecoder`] to the same
//! [`SignalDecoder`] trait so every protocol can be driven through one
//! interface.
//!
//! The analog decoders are self-contained in `hs-decoders`; this module only
//! adds the factory that turns a [`DecoderKind`] into a boxed decoder and the
//! thin P25 adapter (P25 keeps its richer native pipeline elsewhere — this
//! adapter surfaces its audio and grants through the unified output).

use hs_decoders::{
    AmDecoder, DcsDecoder, DecoderEvent, DecoderKind, DecoderOutput, NbfmDecoder, SignalDecoder,
};

pub use hs_decoders::{DecoderEvent as Event, DecoderKind as Kind, AUDIO_RATE};

use crate::decoder::{ChannelDecoder, EqMode, Modulation};

/// Build a decoder for `kind` on a capture at `capture_rate`, selecting the
/// channel `offset_hz` from the capture centre. `squelch_level` (0..1) applies
/// to the FM-based analog decoders. Returns an error for kinds whose phase has
/// not landed yet.
pub fn build(
    kind: DecoderKind,
    capture_rate: f64,
    offset_hz: f64,
    squelch_level: f32,
) -> Result<Box<dyn SignalDecoder>, String> {
    if kind.is_analog() {
        // The analog front-end decimates by an integer to the 8 kHz audio
        // rate, so it needs a capture rate that is a multiple of it. Every SDR
        // capture rate this project uses already is; reject anything else here
        // instead of panicking deep in the DDC.
        let ratio = capture_rate / AUDIO_RATE as f64;
        if !(capture_rate > 0.0 && (ratio.fract()).abs() < 1e-6) {
            return Err(format!(
                "{} needs a capture rate that is a multiple of {} Hz (got {:.0})",
                kind.label(),
                AUDIO_RATE,
                capture_rate
            ));
        }
    }
    match kind {
        DecoderKind::Am => Ok(Box::new(AmDecoder::new(capture_rate, offset_hz))),
        DecoderKind::Nbfm => Ok(Box::new(NbfmDecoder::new(
            capture_rate,
            offset_hz,
            squelch_level,
        ))),
        DecoderKind::Dcs => Ok(Box::new(DcsDecoder::new(
            capture_rate,
            offset_hz,
            squelch_level,
        ))),
        other => Err(format!(
            "{} decoder is on the roadmap but not implemented yet",
            other.label()
        )),
    }
}

/// Adapts the native P25 [`ChannelDecoder`] to [`SignalDecoder`], mapping its
/// rich output down to the unified audio + events form. Voice PCM becomes
/// audio; resolved grants become [`DecoderEvent::Grant`]s.
pub struct P25SignalDecoder {
    inner: ChannelDecoder,
}

impl P25SignalDecoder {
    pub fn new(capture_rate: f64, modulation: Modulation, eq: EqMode, offset_hz: f64) -> Self {
        Self {
            inner: ChannelDecoder::with_offset(capture_rate, modulation, eq, offset_hz),
        }
    }

    /// Access the underlying P25 decoder for its native (richer) output.
    pub fn inner_mut(&mut self) -> &mut ChannelDecoder {
        &mut self.inner
    }
}

impl SignalDecoder for P25SignalDecoder {
    fn process(&mut self, iq: &[f32]) -> DecoderOutput {
        let native = self.inner.process(iq);
        let mut out = DecoderOutput {
            audio: native.pcm,
            events: Vec::new(),
        };
        for g in native.grants {
            out.events.push(DecoderEvent::Grant {
                talkgroup: g.talkgroup as u32,
                freq_hz: Some(g.freq_hz as f64),
                source: Some(g.source_unit),
            });
        }
        out
    }

    fn kind(&self) -> DecoderKind {
        DecoderKind::P25
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factory_builds_implemented_kinds() {
        for k in [DecoderKind::Am, DecoderKind::Nbfm, DecoderKind::Dcs] {
            assert!(build(k, 240_000.0, 0.0, 0.3).is_ok(), "{k:?}");
        }
    }

    #[test]
    fn factory_rejects_unimplemented_kinds() {
        for k in [DecoderKind::Mpt1327, DecoderKind::LtrNet, DecoderKind::Lj1200] {
            assert!(build(k, 240_000.0, 0.0, 0.3).is_err(), "{k:?}");
        }
    }
}
