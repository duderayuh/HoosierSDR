//! Runtime-loaded vocoder plugin boundary (Phase II AMBE+2).
//!
//! HoosierSDR does not ship a Phase II vocoder while US 8,359,197 is active
//! (expires 2028-05-20). Users may supply their own decoder as a dynamic
//! library implementing this C ABI; a build-helper tool will be provided
//! (the SDRTrunk/JMBE pattern). Loading lands in Phase 5.

/// Expected symbol names in a user-supplied plugin dylib.
pub const PLUGIN_INIT_SYMBOL: &str = "hs_vocoder_init";
pub const PLUGIN_DECODE_SYMBOL: &str = "hs_vocoder_decode_frame";
