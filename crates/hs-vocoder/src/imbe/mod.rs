//! Phase I IMBE decoder (in-tree; patents expired).
//!
//! Implementation planned for Phase 2: port from ISC-licensed mbelib
//! (`szechyjs/mbelib`) with attribution. Provenance must be documented
//! per CONTRIBUTING.md.

use crate::{Vocoder, VocoderError};

/// Placeholder until the Phase 2 port lands.
pub struct ImbeDecoder;

impl Vocoder for ImbeDecoder {
    fn name(&self) -> &'static str {
        "IMBE 7200x4400 (stub)"
    }

    fn decode_frame(
        &mut self,
        _frame_bits: &[u8],
        _pcm_out: &mut Vec<i16>,
    ) -> Result<(), VocoderError> {
        Err(VocoderError::NotAvailable("IMBE decoder lands in Phase 2"))
    }
}
