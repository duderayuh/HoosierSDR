//! Pluggable single-channel decoders beyond P25: analog voice (AM, NBFM),
//! sub-audible squelch (DCS), and — in later phases — the FSK ANI/data and
//! trunked-signaling families (MDC-1200, Fleetsync II, Tait, LTR, MPT-1327).
//!
//! Every decoder here consumes interleaved-IQ `f32` at the capture rate and
//! implements [`SignalDecoder`], producing a uniform [`DecoderOutput`] of
//! decoded audio plus typed [`DecoderEvent`]s. The P25 stack keeps its own
//! richer pipeline in `hs-core`; a thin adapter there lets it share this trait
//! so the CLI and app can dispatch every protocol through one interface.
//!
//! Provenance: all DSP and protocol logic here is implemented from published
//! specifications and the digital-communications literature. No code is derived
//! from GPL-licensed decoders (SDRTrunk, DSD, OP25). See CONTRIBUTING.md.

pub mod frontend;

pub mod am;
pub mod dcs;
pub mod nbfm;

pub use am::AmDecoder;
pub use dcs::DcsDecoder;
pub use nbfm::NbfmDecoder;

/// Sample rate of the mono PCM every analog decoder emits, matching the P25
/// voice path so the CLI/app audio sinks are shared.
pub const AUDIO_RATE: u32 = 8_000;

/// Which decoder is running on a channel. The string forms are the CLI
/// `--decoder` names and the desktop-app picker values.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DecoderKind {
    /// P25 Phase I (built natively in `hs-core`, adapted to this trait).
    P25,
    /// AM envelope demodulator (aviation, some utility voice).
    Am,
    /// Narrow-band FM voice with noise squelch.
    Nbfm,
    /// NBFM plus Digital Coded Squelch decode.
    Dcs,
    /// MDC-1200 ANI/status bursts over FM. (Phase 2)
    Mdc1200,
    /// Fleetsync II ANI / status / GPS bursts over FM. (Phase 2)
    FleetsyncII,
    /// Tait CCDI GPS location bursts over FM. (Phase 2)
    Tait1200,
    /// LoJack LJ1200 data bursts. (Phase 2, provenance-gated)
    Lj1200,
    /// LTR-Standard trunked signaling. (Phase 3)
    LtrStandard,
    /// LTR-Net trunked signaling. (Phase 3)
    LtrNet,
    /// Passport trunked signaling. (Phase 4, provenance-gated)
    Passport,
    /// MPT-1327 trunked signaling with channel following. (Phase 4)
    Mpt1327,
}

impl DecoderKind {
    /// Parse a CLI/app decoder name. Accepts a few common aliases.
    pub fn from_name(s: &str) -> Option<Self> {
        let k = match s.trim().to_ascii_lowercase().as_str() {
            "p25" | "p25p1" => Self::P25,
            "am" => Self::Am,
            "nbfm" | "fm" => Self::Nbfm,
            "dcs" => Self::Dcs,
            "mdc" | "mdc1200" | "mdc-1200" => Self::Mdc1200,
            "fleetsync" | "fleetsync2" | "fleetsyncii" | "fleetsync-ii" => Self::FleetsyncII,
            "tait" | "tait1200" | "tait-1200" => Self::Tait1200,
            "lj1200" | "lojack" => Self::Lj1200,
            "ltr" | "ltr-standard" | "ltr-std" => Self::LtrStandard,
            "ltr-net" | "ltrnet" => Self::LtrNet,
            "passport" => Self::Passport,
            "mpt1327" | "mpt-1327" | "mpt" => Self::Mpt1327,
            _ => return None,
        };
        Some(k)
    }

    /// Stable lowercase identifier (the canonical CLI/app name).
    pub fn name(self) -> &'static str {
        match self {
            Self::P25 => "p25",
            Self::Am => "am",
            Self::Nbfm => "nbfm",
            Self::Dcs => "dcs",
            Self::Mdc1200 => "mdc1200",
            Self::FleetsyncII => "fleetsync",
            Self::Tait1200 => "tait1200",
            Self::Lj1200 => "lj1200",
            Self::LtrStandard => "ltr-standard",
            Self::LtrNet => "ltr-net",
            Self::Passport => "passport",
            Self::Mpt1327 => "mpt1327",
        }
    }

    /// Human-readable label for UIs and logs.
    pub fn label(self) -> &'static str {
        match self {
            Self::P25 => "P25 Phase I",
            Self::Am => "AM",
            Self::Nbfm => "FM / NBFM",
            Self::Dcs => "DCS (NBFM)",
            Self::Mdc1200 => "MDC-1200",
            Self::FleetsyncII => "Fleetsync II",
            Self::Tait1200 => "Tait 1200",
            Self::Lj1200 => "LJ1200",
            Self::LtrStandard => "LTR-Standard",
            Self::LtrNet => "LTR-Net",
            Self::Passport => "Passport",
            Self::Mpt1327 => "MPT-1327",
        }
    }

    /// Whether a working decoder for this kind exists in the project today
    /// (P25 natively in `hs-core`; AM/NBFM/DCS in this crate). The remaining
    /// kinds are declared for the roadmap and rejected by the factory until
    /// their phase lands.
    pub fn is_implemented(self) -> bool {
        matches!(self, Self::P25 | Self::Am | Self::Nbfm | Self::Dcs)
    }

    /// Whether this kind's decoder is one of the analog family in this crate
    /// (i.e. constructible by the `hs-core` analog factory). P25 is excluded —
    /// it is built through its native pipeline.
    pub fn is_analog(self) -> bool {
        matches!(self, Self::Am | Self::Nbfm | Self::Dcs)
    }
}

/// A typed thing a decoder recovered from the channel. Non-exhaustive so later
/// phases can add variants without breaking match arms downstream.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq)]
pub enum DecoderEvent {
    /// Squelch opened — audio is now flowing.
    SquelchOpen,
    /// Squelch closed — the channel went quiet.
    SquelchClose,
    /// A Digital Coded Squelch code was identified. `code` is the standard
    /// three-digit octal DCS number; `inverted` marks the reverse-polarity
    /// (a.k.a. "N"/"I") variant some radios transmit.
    Dcs { code: u16, inverted: bool },
    /// An ANI (automatic number identification) burst: a unit announcing its
    /// ID, optionally with an operation/opcode label. (FSK families, Phase 2)
    Ani { id: u32, op: Option<String> },
    /// A status/message-code burst from a unit. (Phase 2)
    Status { id: u32, status: u16 },
    /// A GPS position report decoded from a data burst. (Phase 2)
    Gps { id: u32, lat: f64, lon: f64 },
    /// A trunked voice-channel grant. (Phase 3–4)
    Grant {
        talkgroup: u32,
        freq_hz: Option<f64>,
        source: Option<u32>,
    },
    /// A decoder-specific human-readable line for logs when no typed variant
    /// fits yet.
    Message(String),
}

/// One block's worth of decoder output.
#[derive(Default, Clone)]
pub struct DecoderOutput {
    /// Mono PCM at [`AUDIO_RATE`], gated by squelch (empty when squelched or on
    /// a data-only decoder).
    pub audio: Vec<i16>,
    /// Typed events recovered this block.
    pub events: Vec<DecoderEvent>,
}

impl DecoderOutput {
    pub fn is_empty(&self) -> bool {
        self.audio.is_empty() && self.events.is_empty()
    }
}

/// A single-channel decoder: push interleaved-IQ `f32` at the capture rate,
/// get audio and events back.
pub trait SignalDecoder {
    /// Process a slice of interleaved (I, Q) `f32` samples at the capture rate
    /// the decoder was built for.
    fn process(&mut self, iq: &[f32]) -> DecoderOutput;

    /// Which decoder this is.
    fn kind(&self) -> DecoderKind;

    /// Sample rate of the PCM in [`DecoderOutput::audio`]. Always
    /// [`AUDIO_RATE`] for the built-in analog decoders.
    fn audio_rate(&self) -> u32 {
        AUDIO_RATE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_names_round_trip() {
        for k in [
            DecoderKind::Am,
            DecoderKind::Nbfm,
            DecoderKind::Dcs,
            DecoderKind::Mdc1200,
            DecoderKind::Mpt1327,
        ] {
            assert_eq!(DecoderKind::from_name(k.name()), Some(k));
        }
        assert_eq!(DecoderKind::from_name("FM"), Some(DecoderKind::Nbfm));
        assert_eq!(DecoderKind::from_name("nonsense"), None);
    }
}
