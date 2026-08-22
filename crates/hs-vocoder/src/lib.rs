//! Voice decoding behind a single trait.
//!
//! - `imbe`: P25 Phase I IMBE, shipped in-tree (DVSI patents expired ~2017-18).
//!   To be ported from ISC-licensed mbelib with attribution — never from GPL
//!   forks (mbelib-neo is GPL; do not touch it).
//! - `plugin`: P25 Phase II AMBE+2 half-rate via a user-supplied dynamic
//!   library — an optional escape hatch, not a licence requirement. The
//!   half-rate decoder is available in ISC mbelib (`ambe3600x2450.c`).

pub mod imbe;
pub mod plugin;

#[derive(Debug)]
pub enum VocoderError {
    BadFrame,
    NotAvailable(&'static str),
}

/// One voice frame in, PCM out. 8 kHz mono i16.
pub trait Vocoder {
    /// Human-readable codec name, e.g. "IMBE 7200x4400".
    fn name(&self) -> &'static str;
    /// Decode one vocoder frame (post-FEC bits) into PCM samples.
    fn decode_frame(
        &mut self,
        frame_bits: &[u8],
        pcm_out: &mut Vec<i16>,
    ) -> Result<(), VocoderError>;
}
