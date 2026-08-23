//! P25 Phase 2 AMBE+2 half-rate decoder (3600×2450).
//!
//! Wraps the ISC-licensed mbelib `ambe3600x2450.c` vendored under
//! `vendor/mbelib/`, exactly as the `imbe` module wraps the Phase I path.
//! Built only when the `imbe` feature (vendored mbelib C) is enabled; the
//! half-rate codec is available under ISC — no patent deadline (see
//! `docs/ARCHITECTURE.md §5`).

use crate::{Vocoder, VocoderError};
use hs_p25::p25p2::deinterleave::{deinterleave, VoiceFrame};

/// AMBE+2 half-rate emits 160 PCM samples per 20 ms voice frame at 8 kHz.
pub const SAMPLES_PER_FRAME: usize = 160;

/// Bits in a transmitted (still-interleaved) half-rate voice block.
pub const VOICE_BLOCK_BITS: usize = 72;

#[cfg(feature = "imbe")]
mod ffi {
    /// Mirror of mbelib's `mbe_parms`. Bit-identical to `imbe::ffi::MbeParms`
    /// (the same C struct); kept separate to avoid coupling the two decoders.
    #[repr(C)]
    pub struct MbeParms {
        pub w0: f32,
        pub l: i32,
        pub k: i32,
        pub vl: [i32; 57],
        pub ml: [f32; 57],
        pub log2ml: [f32; 57],
        pub phil: [f32; 57],
        pub psil: [f32; 57],
        pub gamma: f32,
        pub un: i32,
        pub repeat: i32,
    }

    impl MbeParms {
        pub fn zeroed() -> Self {
            // SAFETY: plain-old-data; zero is valid and mbe_initMbeParms
            // overwrites it before use.
            unsafe { core::mem::zeroed() }
        }
    }

    extern "C" {
        pub fn mbe_initMbeParms(cur: *mut MbeParms, prev: *mut MbeParms, prev_enh: *mut MbeParms);
        pub fn mbe_processAmbe3600x2450Frame(
            aout: *mut i16,
            errs: *mut i32,
            errs2: *mut i32,
            err_str: *mut u8,
            ambe_fr: *const [u8; 24],
            ambe_d: *mut u8,
            cur: *mut MbeParms,
            prev: *mut MbeParms,
            prev_enh: *mut MbeParms,
            uvquality: i32,
        );
    }
}

/// Unvoiced synthesis quality (1–64); mbelib's own default.
pub const DEFAULT_UV_QUALITY: i32 = 3;

#[cfg(feature = "imbe")]
pub struct Ambe2Decoder {
    cur: Box<ffi::MbeParms>,
    prev: Box<ffi::MbeParms>,
    prev_enh: Box<ffi::MbeParms>,
    /// Errors reported for the last decoded frame (post-FEC bit errors).
    pub last_errs: i32,
    uv_quality: i32,
}

#[cfg(feature = "imbe")]
impl Default for Ambe2Decoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "imbe")]
impl Ambe2Decoder {
    pub fn new() -> Self {
        let mut cur = Box::new(ffi::MbeParms::zeroed());
        let mut prev = Box::new(ffi::MbeParms::zeroed());
        let mut prev_enh = Box::new(ffi::MbeParms::zeroed());
        // SAFETY: three valid, distinct, properly-aligned MbeParms pointers.
        unsafe {
            ffi::mbe_initMbeParms(&mut *cur, &mut *prev, &mut *prev_enh);
        }
        Self {
            cur,
            prev,
            prev_enh,
            last_errs: 0,
            uv_quality: DEFAULT_UV_QUALITY,
        }
    }

    pub fn set_uv_quality(&mut self, q: i32) {
        self.uv_quality = q.clamp(1, 64);
    }

    pub fn uv_quality(&self) -> i32 {
        self.uv_quality
    }

    /// Decode one de-interleaved 4×24 AMBE+2 frame to 160 PCM samples.
    ///
    /// `frame` is the `VoiceFrame` from `hs_p25::p25p2::deinterleave::deinterleave`,
    /// row-major `[4][24]`, identical to the `char ambe_fr[4][24]` mbelib
    /// consumes. mbelib runs the Golay/Hamming FEC internally.
    pub fn decode(&mut self, frame: &VoiceFrame) -> [i16; SAMPLES_PER_FRAME] {
        let mut out = [0i16; SAMPLES_PER_FRAME];
        let mut errs = 0i32;
        let mut errs2 = 0i32;
        let mut err_str = [0u8; 64];
        let mut ambe_d = [0u8; 49];
        // SAFETY: buffers sized to mbelib's contract; ambe_fr is a pointer to
        // the first of four 24-byte rows, ambe_d is a 49-byte scratch.
        unsafe {
            ffi::mbe_processAmbe3600x2450Frame(
                out.as_mut_ptr(),
                &mut errs,
                &mut errs2,
                err_str.as_mut_ptr(),
                frame.as_ptr(),
                ambe_d.as_mut_ptr(),
                &mut *self.cur,
                &mut *self.prev,
                &mut *self.prev_enh,
                self.uv_quality,
            );
        }
        self.last_errs = errs2;
        out
    }
}

#[cfg(not(feature = "imbe"))]
pub struct Ambe2Decoder;

#[cfg(not(feature = "imbe"))]
impl Default for Ambe2Decoder {
    fn default() -> Self {
        Self
    }
}

#[cfg(not(feature = "imbe"))]
impl Ambe2Decoder {
    pub fn new() -> Self {
        Self
    }
    pub fn decode(&mut self, _frame: &VoiceFrame) -> [i16; SAMPLES_PER_FRAME] {
        [0; SAMPLES_PER_FRAME]
    }
}

impl Vocoder for Ambe2Decoder {
    fn name(&self) -> &'static str {
        if cfg!(feature = "imbe") {
            "AMBE+2 3600x2450"
        } else {
            "AMBE+2 3600x2450 (disabled: build with --features imbe)"
        }
    }

    fn decode_frame(
        &mut self,
        frame_bits: &[u8],
        pcm_out: &mut Vec<i16>,
    ) -> Result<(), VocoderError> {
        if frame_bits.len() != VOICE_BLOCK_BITS {
            return Err(VocoderError::BadFrame);
        }
        if !cfg!(feature = "imbe") {
            return Err(VocoderError::NotAvailable("build with --features imbe"));
        }
        let mut burst = [0u8; VOICE_BLOCK_BITS];
        burst.copy_from_slice(&frame_bits[..VOICE_BLOCK_BITS]);
        let frame = deinterleave(&burst);
        pcm_out.extend_from_slice(&self.decode(&frame));
        Ok(())
    }
}
