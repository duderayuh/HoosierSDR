//! Phase I IMBE 7200×4400 decoder.
//!
//! DVSI's IMBE patents expired ~2017-18, so this decoder ships in-tree. It
//! wraps the ISC-licensed mbelib C implementation vendored under
//! `vendor/mbelib/` (see NOTICE). Built only when the `imbe` feature is on.

use crate::{Vocoder, VocoderError};
use hs_p25::voice::ImbeFrame;

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
        pub fn mbe_processImbe7200x4400Frame(
            aout: *mut i16,
            errs: *mut i32,
            errs2: *mut i32,
            err_str: *mut u8,
            imbe_fr: *const [u8; 23],
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
}

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
        }
    }

    /// Decode one de-interleaved IMBE frame to 160 PCM samples.
    pub fn decode(&mut self, frame: &ImbeFrame) -> [i16; SAMPLES_PER_FRAME] {
        let mut out = [0i16; SAMPLES_PER_FRAME];
        let mut errs = 0i32;
        let mut errs2 = 0i32;
        let mut err_str = [0u8; 64];
        let mut imbe_d = [0u8; 88];
        // SAFETY: mbelib reads imbe_fr as char[8][23] and writes exactly 160
        // shorts into aout; all buffers are sized to match. err_str is a
        // C string scratch buffer of ample size.
        unsafe {
            ffi::mbe_processImbe7200x4400Frame(
                out.as_mut_ptr(),
                &mut errs,
                &mut errs2,
                err_str.as_mut_ptr(),
                frame.as_ptr(),
                imbe_d.as_mut_ptr(),
                &mut *self.cur,
                &mut *self.prev,
                &mut *self.prev_enh,
                3,
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
    pub fn decode(&mut self, _frame: &ImbeFrame) -> [i16; SAMPLES_PER_FRAME] {
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
        let frame = hs_p25::voice::deinterleave_imbe(frame_bits);
        pcm_out.extend_from_slice(&self.decode(&frame));
        Ok(())
    }
}
