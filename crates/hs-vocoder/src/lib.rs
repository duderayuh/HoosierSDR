//! Voice decoding behind a single trait.
//!
//! - `imbe`: P25 Phase I IMBE, shipped in-tree (DVSI patents expired ~2017-18).
//!   To be ported from ISC-licensed mbelib with attribution — never from GPL
//!   forks (mbelib-neo is GPL; do not touch it).
//! - `plugin`: P25 Phase II AMBE+2 half-rate via a user-supplied dynamic
//!   library. NOT distributed with this project while US 8,359,197 is active
//!   (to 2028-05-20). See docs/ARCHITECTURE.md §5.

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
