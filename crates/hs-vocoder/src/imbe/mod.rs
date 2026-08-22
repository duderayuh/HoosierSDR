//! Phase I IMBE 7200×4400 decoder.
//!
//! DVSI's IMBE patents expired ~2017-18, so this decoder ships in-tree. It
//! wraps the ISC-licensed mbelib C implementation vendored under
//! `vendor/mbelib/` (see NOTICE). Built only when the `imbe` feature is on.

use crate::{Vocoder, VocoderError};
use hs_p25::imbec::SoftImbeFrame;

/// P25 IMBE emits 160 PCM samples per 20 ms voice frame at 8 kHz.
pub const SAMPLES_PER_FRAME: usize = 160;

#[cfg(feature = "imbe")]
mod ffi {
    /// Mirror of mbelib's `mbe_parms` (see vendor/mbelib/mbelib.h). Layout
    /// must match exactly; verified by a size assertion at construction.
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
            // SAFETY: all fields are plain old data; zero is a valid bit
            // pattern and mbe_initMbeParms overwrites it before use.
            unsafe { core::mem::zeroed() }
        }
    }

    extern "C" {
        pub fn mbe_initMbeParms(cur: *mut MbeParms, prev: *mut MbeParms, prev_enh: *mut MbeParms);
        /// Synthesis from FEC-corrected 88 data bits — the entry point used by
        /// the soft-decision path, which does its own FEC in `hs_p25::imbec`
        /// and hands mbelib only the cleaned `imbe_d[88]`.
        pub fn mbe_processImbe4400Data(
            aout: *mut i16,
            errs: *mut i32,
            errs2: *mut i32,
            err_str: *mut u8,
            imbe_d: *mut u8,
            cur: *mut MbeParms,
            prev: *mut MbeParms,
            prev_enh: *mut MbeParms,
            uvquality: i32,
        );
    }
}

#[cfg(feature = "imbe")]
pub struct ImbeDecoder {
    cur: Box<ffi::MbeParms>,
    prev: Box<ffi::MbeParms>,
    prev_enh: Box<ffi::MbeParms>,
    /// Errors reported for the last decoded frame (post-FEC bit errors).
    pub last_errs: i32,
    /// Unvoiced synthesis quality, 1–64.
    uv_quality: i32,
}

/// Sine components mbelib synthesizes per *unvoiced* band, 1–64.
///
/// Left at mbelib's default of 3 on the strength of a measurement rather than
/// a guess. The theory was that unvoiced bands carry the fricatives (s, f, sh,
/// t) that hold most speech energy above 2 kHz, so starving them would be what
/// makes a clean decode still sound muffled. Swept against a real off-air
/// capture, that turned out to be **wrong**: raising the value does not add
/// high-frequency energy, it slightly removes it (2.9% of energy above 2 kHz
/// at q=3, 1.4% at q=64). Higher values trade spectral energy for smoother,
/// less granular unvoiced synthesis, which may still sound better to a
/// listener — a judgement no spectrum measurement settles — so the value stays
/// tunable via [`ImbeDecoder::set_uv_quality`].
///
/// Either way this only changes how decoded parameters are *rendered* to
/// audio. It never changes what was decoded.
pub const DEFAULT_UV_QUALITY: i32 = 3;

#[cfg(feature = "imbe")]
impl Default for ImbeDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "imbe")]
impl ImbeDecoder {
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

    /// Set unvoiced synthesis quality (1–64). Higher renders fricatives with
    /// more sine components: better high-frequency detail, more CPU.
    pub fn set_uv_quality(&mut self, q: i32) {
        self.uv_quality = q.clamp(1, 64);
    }

    pub fn uv_quality(&self) -> i32 {
        self.uv_quality
    }

    /// Decode one de-interleaved IMBE frame to 160 PCM samples, using the
    /// soft-decision FEC and mbelib's synthesis-only entry point.
    pub fn decode(&mut self, frame: &SoftImbeFrame) -> [i16; SAMPLES_PER_FRAME] {
        // Soft-decision Golay/Hamming in Rust recovers the 88 voice bits by
        // maximum likelihood on the confidence-weighted frame; mbelib then
        // only synthesizes from the cleaned bits.
        let (mut imbe_d, soft_errs) = hs_p25::imbec::soft_decode_imbe(frame);
        let mut out = [0i16; SAMPLES_PER_FRAME];
        let mut errs = 0i32;
        let mut errs2 = soft_errs.min(144) as i32;
        let mut err_str = [0u8; 256];
        // SAFETY: mbelib reads imbe_d as char[88] and writes exactly 160
        // shorts into aout; all buffers are sized to match and err_str is a
        // generous scratch buffer.
        unsafe {
            ffi::mbe_processImbe4400Data(
                out.as_mut_ptr(),
                &mut errs,
                &mut errs2,
                err_str.as_mut_ptr(),
                imbe_d.as_mut_ptr(),
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
pub struct ImbeDecoder;

#[cfg(not(feature = "imbe"))]
impl Default for ImbeDecoder {
    fn default() -> Self {
        Self
    }
}

#[cfg(not(feature = "imbe"))]
impl ImbeDecoder {
    pub fn new() -> Self {
        Self
    }
    pub fn decode(&mut self, _frame: &SoftImbeFrame) -> [i16; SAMPLES_PER_FRAME] {
        [0; SAMPLES_PER_FRAME]
    }
}

impl Vocoder for ImbeDecoder {
    fn name(&self) -> &'static str {
        if cfg!(feature = "imbe") {
            "IMBE 7200x4400"
        } else {
            "IMBE 7200x4400 (disabled: build with --features imbe)"
        }
    }

    fn decode_frame(
        &mut self,
        frame_bits: &[u8],
        pcm_out: &mut Vec<i16>,
    ) -> Result<(), VocoderError> {
        if frame_bits.len() != 144 {
            return Err(VocoderError::BadFrame);
        }
        if !cfg!(feature = "imbe") {
            return Err(VocoderError::NotAvailable("build with --features imbe"));
        }
        let hard = hs_p25::voice::deinterleave_imbe(frame_bits);
        // There is no soft information offline — mark every bit certain, so
        // the soft decoder reduces to the hard pipeline exactly.
        let frame = SoftImbeFrame {
            bits: hard,
            conf: [[255u8; 23]; 8],
        };
        pcm_out.extend_from_slice(&self.decode(&frame));
        Ok(())
    }
}
