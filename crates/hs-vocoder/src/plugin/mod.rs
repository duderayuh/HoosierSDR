//! Optional runtime-loaded vocoder plugin boundary (Phase II AMBE+2).
//!
//! The half-rate AMBE+2 decoder is available in the ISC-licensed mbelib
//! (`ambe3600x2450.c` → `mbe_processAmbe3600x2450Frame`);
//! HoosierSDR vendors only the IMBE subset today. This boundary is an escape
//! hatch, not a licence requirement — a user who prefers their own decoder can
//! supply a dynamic library implementing this C ABI.

/// Expected symbol names in a user-supplied plugin dylib.
pub const PLUGIN_INIT_SYMBOL: &str = "hs_vocoder_init";
pub const PLUGIN_DECODE_SYMBOL: &str = "hs_vocoder_decode_frame";
