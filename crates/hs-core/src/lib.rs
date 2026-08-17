//! Orchestration: wires source → channelizer → demod/equalizer → P25 →
//! trunking → vocoder → audio/recording/transcription. Real pipeline lands
//! across Phases 1–2; this crate currently pins the dependency graph.

pub mod decoder;
pub mod derotate;
pub mod diag;
pub mod scan;
pub mod stream;

pub use hs_catalog as catalog;
pub use hs_dsp as dsp;
pub use hs_p25 as p25;
pub use hs_source as source;
pub use hs_transcribe as transcribe;
pub use hs_trunk as trunk;
pub use hs_vocoder as vocoder;
