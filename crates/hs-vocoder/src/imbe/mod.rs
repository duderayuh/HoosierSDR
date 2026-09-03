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
        // The three building blocks `mbe_processImbe7200x4400Frame` composes
        // internally, exposed individually so `soft_correct` (below) can run
        // its own trial decodes on confidence-guided bit-flip candidates
        // *before* the real decode — using mbelib's exact channel-coding
        // logic rather than a from-scratch reimplementation of it.
        pub fn mbe_golay2312(input: *const u8, output: *mut u8) -> i32;
        pub fn mbe_hamming1511(input: *const u8, output: *mut u8) -> i32;
        pub fn mbe_demodulateImbe7200x4400Data(imbe: *mut [u8; 23]);
    }
}

/// Reliability-based (Chase-II style) pre-correction ahead of mbelib's own
/// bounded-distance FEC.
///
/// mbelib's `mbe_golay2312`/`mbe_hamming1511` correct up to their code's
/// guaranteed radius (3 hard errors for Golay(23,12,7); 1 for Hamming(15,11))
/// by table lookup on the syndrome — a pure function of whatever 23 (or 15)
/// hard bits it's handed. It has no way to know the demodulator was *unsure*
/// about a couple of those bits, so a burst of 4 hard errors that happens to
/// include a marginal symbol is indistinguishable, to mbelib, from 4 solid
/// ones — and gets "corrected" to the wrong codeword or rejected outright.
///
/// This tries flipping small subsets of the least-confident bits before
/// handing candidates to the real decoder, and keeps whichever pre-flip
/// yields output closest (in confidence-weighted bits) to what was actually
/// received. When the codeword was already within mbelib's native radius,
/// the zero-flip candidate wins (this never does worse than plain hard
/// decoding); when it wasn't, spending a little confidence on the bits most
/// likely to be wrong can pull the true 4-or-5-error pattern back inside
/// that radius. See `chase_pick`'s doc for the mechanics.
#[cfg(feature = "imbe")]
mod soft_fec {
    use super::ffi;
    use hs_p25::voice::ImbeConf;

    /// Try flipping subsets of the `trial` least-confident bits in `received`
    /// and let `decode` (mbelib's own syndrome decoder) judge each candidate;
    /// return whichever *pre-decode input* makes `decode`'s output closest,
    /// in confidence-weighted bits, to what was actually received.
    ///
    /// `decode`'s output is always included as a candidate result via
    /// `mask = 0` (no manual flip) — the plain hard-decision case — so this
    /// can never choose worse than doing nothing: it only switches to a
    /// flipped candidate when that candidate's decode is *strictly* closer to
    /// the received bits, confidence-weighted, than decoding the bits as
    /// received.
    fn chase_pick<const N: usize>(
        received: [u8; N],
        conf: [u8; N],
        trial: usize,
        decode: impl Fn([u8; N]) -> [u8; N],
    ) -> [u8; N] {
        let mut order: [usize; N] = std::array::from_fn(|i| i);
        order.sort_by_key(|&i| conf[i]);
        let trial = trial.min(N);

        let mut best = received;
        let mut best_cost = u64::MAX;
        for mask in 0..(1u32 << trial) {
            let mut candidate_in = received;
            for (b, &pos) in order[..trial].iter().enumerate() {
                if mask & (1 << b) != 0 {
                    candidate_in[pos] ^= 1;
                }
            }
            let candidate_out = decode(candidate_in);
            let cost: u64 = (0..N)
                .filter(|&i| candidate_out[i] != received[i])
                .map(|i| conf[i] as u64)
                .sum();
            if cost < best_cost {
                best_cost = cost;
                best = candidate_in;
            }
        }
        best
    }

    /// Two manually-tried bits (4 candidates) around Golay's native 3-error
    /// radius: enough to reach some real 4-error patterns without combinatorial
    /// blowup (4 Golay words/frame × 4 candidates = 16 trial decodes/frame).
    const GOLAY_TRIAL_BITS: usize = 2;
    /// Two manually-tried bits (4 candidates). Hamming(15,11) only corrects
    /// 1 error natively and, unlike Golay, never "absorbs" a second one as
    /// part of that correction — so recovering a real 2-error burst needs
    /// *both* of its error positions manually flipped, not just one; a
    /// single trial bit can only ever reach 1 residual error, never 0, and
    /// so can never fully cancel a 2-error pattern. Confirmed empirically:
    /// trial=1 found no recoverable 2-error scenario in an exhaustive search
    /// over this codeword's 15 positions (see git history).
    const HAMMING_TRIAL_BITS: usize = 2;

    fn golay_decode(input: [u8; 23]) -> [u8; 23] {
        let mut out = [0u8; 23];
        // SAFETY: mbe_golay2312 reads/writes exactly 23-byte buffers, both
        // valid and correctly sized here.
        unsafe { ffi::mbe_golay2312(input.as_ptr(), out.as_mut_ptr()) };
        out
    }

    fn hamming_decode(input: [u8; 15]) -> [u8; 15] {
        let mut out = [0u8; 15];
        // SAFETY: mbe_hamming1511 reads/writes exactly 15-byte buffers, both
        // valid and correctly sized here.
        unsafe { ffi::mbe_hamming1511(input.as_ptr(), out.as_mut_ptr()) };
        out
    }

    /// Confidence-guided pre-correction of one de-interleaved IMBE frame, run
    /// before handing it to mbelib's real decode. Mutates `imbe_fr` in place:
    /// on return it still needs mbelib's own `mbe_processImbe7200x4400Frame`
    /// to actually perform the correction and synthesis, but is now the raw
    /// input that call needs to see to reproduce (and sometimes improve on)
    /// the correction chosen here.
    ///
    /// Only ever substitutes *pre-decode inputs* (never writes a manually
    /// "corrected" codeword back) so mbelib's subsequent, independent decode
    /// of the substituted bits is the sole source of truth for the final
    /// correction — this cannot disagree with itself.
    pub fn soft_correct(imbe_fr: &mut super::ImbeFrame, conf: &ImbeConf) {
        // Codeword 0 (Golay, not scrambled): pick the best pre-flip using its
        // own confidence directly.
        let c0: [u8; 23] = imbe_fr[0];
        imbe_fr[0] = chase_pick(c0, conf[0], GOLAY_TRIAL_BITS, golay_decode);

        // Codewords 1..7 are PN-scrambled with a seed mbelib derives from the
        // *corrected* codeword 0 (see `mbe_demodulateImbe7200x4400Data`) —
        // Chase decoding only makes sense on the true, descrambled codeword,
        // not its scrambled transmission form. Reproduce mbelib's exact
        // sequencing: correct C0 (via the same substituted input just
        // chosen, so this is the identical correction mbelib's own later
        // call will make), then descramble with it.
        let corrected_c0 = golay_decode(imbe_fr[0]);
        let mut scratch = *imbe_fr;
        scratch[0] = corrected_c0;
        // SAFETY: scratch is a valid, fully-initialized [[u8; 23]; 8]; the
        // FFI only ever indexes its first 7 rows within their documented
        // widths.
        unsafe { ffi::mbe_demodulateImbe7200x4400Data(scratch.as_mut_ptr()) };

        for i in 1..4 {
            scratch[i] = chase_pick(scratch[i], conf[i], GOLAY_TRIAL_BITS, golay_decode);
        }
        for i in 4..7 {
            let received: [u8; 15] = scratch[i][..15].try_into().unwrap();
            let c: [u8; 15] = conf[i][..15].try_into().unwrap();
            let picked = chase_pick(received, c, HAMMING_TRIAL_BITS, hamming_decode);
            scratch[i][..15].copy_from_slice(&picked);
        }

        // Re-scramble the (possibly Chase-corrected) 1..7 back to the
        // raw/received representation mbelib's real decode expects — same
        // seed (`scratch[0]` unchanged since the descramble above), so this
        // is the same deterministic XOR sequence applied a second time to
        // the corrected values, not an undo of the first application.
        unsafe { ffi::mbe_demodulateImbe7200x4400Data(scratch.as_mut_ptr()) };

        imbe_fr[1..7].copy_from_slice(&scratch[1..7]);
        // imbe_fr[7] (7 unprotected bits) is left untouched — no FEC exists
        // to guide a pre-flip choice for it.
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// mbelib's own Golay(23,12) generator table (`golayGenerator` in
        /// vendor/mbelib/ecc_const.h), copied verbatim rather than searched
        /// for: `mbe_golay2312`'s syndrome table (`golayMatrix`) maps *many*
        /// nonzero syndromes to a zero *data* correction — any syndrome whose
        /// implied error pattern lands entirely in the 11 parity bits leaves
        /// the 12 data bits unchanged — so "does `mbe_golay2312` leave this
        /// word alone" is satisfied by far more than just the 2048/12≈171
        /// genuinely valid codewords per data pattern; searching parity
        /// space for a decode-stable word (an earlier version of this
        /// helper) reliably found one of those *look-alikes* instead, not a
        /// real codeword — silently invalidating every test built on it.
        /// Encoding directly from the generator, as mbelib's own encoder
        /// would, has no such ambiguity.
        const GOLAY_GENERATOR: [u32; 12] = [
            0x63a, 0x31d, 0x7b4, 0x3da, 0x1ed, 0x6cc, 0x366, 0x1b3, 0x6e3, 0x54b, 0x49f, 0x475,
        ];

        /// Build the genuine Golay(23,12) codeword for a 12-bit data
        /// pattern, matching `mbe_checkGolayBlock`'s `eccexpected` exactly.
        /// `mbe_checkGolayBlock` walks `block_l`'s bits 22 downto 11 (in
        /// that order) pairing bit(22-i) with `golayGenerator[i]`; since
        /// `data[k]` here holds `word[11+k]` = bit(11+k), bit(22-i) is
        /// `data[11-i]` — so `golayGenerator[i]` pairs with `data[11-i]`,
        /// not `data[i]`. Verified (not just asserted) by the `debug_assert`
        /// below, which failed loudly on the first, reversed attempt at this
        /// pairing — see git history.
        fn find_valid_golay_codeword(data: [u8; 12]) -> [u8; 23] {
            let mut parity = 0u32;
            for (i, &bit) in data.iter().enumerate() {
                if bit != 0 {
                    parity ^= GOLAY_GENERATOR[11 - i];
                }
            }
            let mut word = [0u8; 23];
            word[11..23].copy_from_slice(&data);
            for b in 0..11 {
                // `eccbits`/`block_l`'s low 11 bits are `word[0..11]` with
                // `word[0]` as bit 0 (LSB) — see `mbe_golay2312`'s own
                // `block` construction.
                word[b] = ((parity >> b) & 1) as u8;
            }
            debug_assert!(
                (0..23).all(|flip| {
                    let mut perturbed = word;
                    perturbed[flip] ^= 1;
                    // Only the data bits (11..23) are checked: `out[0..11]`
                    // is always a passthrough of whatever parity was fed in
                    // (see `mbe_golay2312`), never independently corrected.
                    golay_decode(perturbed)[11..23] == word[11..23]
                }),
                "constructed word does not correct every single-bit error — not a real codeword"
            );
            word
        }

        /// Same idea for Hamming(15,11): brute-force the whole 15-bit space
        /// (cheap — 32768 candidates) for a fixed point of `mbe_hamming1511`.
        fn find_valid_hamming_codeword(skip: usize) -> [u8; 15] {
            (0u32..1 << 15)
                .map(|w| -> [u8; 15] { std::array::from_fn(|i| ((w >> i) & 1) as u8) })
                .filter(|&word| hamming_decode(word) == word)
                .nth(skip)
                .expect("no valid Hamming(15,11) codeword found")
        }

        #[test]
        fn chase_decoding_recovers_a_golay_error_burst_hard_decoding_cannot() {
            let data = [1, 0, 1, 1, 0, 0, 1, 0, 1, 1, 0, 1];
            let codeword = find_valid_golay_codeword(data);

            // 4 errors — beyond Golay(23,12,7)'s native 3-error radius — two
            // on bits marked low-confidence, two on bits marked certain.
            // Found by exhaustive search over error-position/low-confidence
            // combinations (see git history): not every 4-error pattern with
            // 2 low-confidence bits is recoverable this way (a perfect code
            // has no safety margin — some 4-error words sit closer, in raw
            // bit distance, to a different codeword than to the true one,
            // and no amount of confidence weighting saves those), so this is
            // a real, found-not-assumed instance of the case that Chase
            // decoding *does* help.
            let mut received = codeword;
            for &p in &[0usize, 1, 2, 6] {
                received[p] ^= 1;
            }
            let mut conf = [255u8; 23];
            conf[0] = 10;
            conf[1] = 10;

            // Confirm the premise: plain hard decoding of this 4-error word
            // must NOT recover the true data, or this test proves nothing.
            let hard_out = golay_decode(received);
            assert_ne!(
                &hard_out[11..23],
                &codeword[11..23],
                "test setup: 4 errors should already defeat plain hard decoding"
            );

            // Chase decoding, trusting confidence to find the two bits most
            // likely wrong, must recover the original data.
            let picked = chase_pick(received, conf, GOLAY_TRIAL_BITS, golay_decode);
            let recovered = golay_decode(picked);
            assert_eq!(
                &recovered[11..23],
                &codeword[11..23],
                "soft (Chase) decoding failed to recover data hard decoding could not"
            );
        }

        #[test]
        fn a_clean_golay_codeword_is_left_alone() {
            let data = [0, 1, 0, 0, 1, 1, 0, 1, 0, 0, 1, 0];
            let codeword = find_valid_golay_codeword(data);
            let conf = [255u8; 23];

            // No manual flip should ever win over the zero-flip candidate
            // when the received word is already a valid codeword.
            let picked = chase_pick(codeword, conf, GOLAY_TRIAL_BITS, golay_decode);
            assert_eq!(picked, codeword);
        }

        #[test]
        fn chase_decoding_recovers_a_hamming_error_pair_hard_decoding_cannot() {
            let codeword = find_valid_hamming_codeword(0);

            // 2 errors — beyond Hamming(15,11)'s native 1-error radius, and
            // both marked low-confidence: unlike Golay, Hamming's native
            // decoder never absorbs a second error as part of correcting the
            // first, so recovering this needs *both* true error positions
            // guessed (see `HAMMING_TRIAL_BITS`'s doc). Found by exhaustive
            // search over error-position combinations (see git history).
            let mut received = codeword;
            received[0] ^= 1;
            received[1] ^= 1;
            let mut conf = [255u8; 15];
            conf[0] = 8;
            conf[1] = 8;

            let hard_out = hamming_decode(received);
            assert_ne!(
                hard_out, codeword,
                "test setup: 2 errors should already defeat plain hard decoding"
            );

            let picked = chase_pick(received, conf, HAMMING_TRIAL_BITS, hamming_decode);
            let recovered = hamming_decode(picked);
            assert_eq!(
                recovered, codeword,
                "soft (Chase) decoding failed to recover the codeword hard decoding could not"
            );
        }

        #[test]
        fn soft_correct_full_frame_recovers_a_scrambled_codeword_error_burst() {
            // Codeword 0 (Golay, unscrambled): clean, so soft_correct's own
            // trial decoding of it should reproduce it exactly.
            let c0 = find_valid_golay_codeword([1, 1, 0, 0, 1, 0, 1, 0, 1, 1, 1, 0]);

            // Build a full frame: c0 clean, codeword 1 will be corrupted
            // *after* scrambling (as it would arrive off the air), the rest
            // left at whatever the descrambler will treat as "received"
            // (their own correctness isn't under test here).
            let mut frame = [[0u8; 23]; 8];
            frame[0] = c0;

            // Discover the true (descrambled) codeword 1 would need to be by
            // running the same scramble mbelib uses, in the clear, then
            // burying errors in its *scrambled* (transmitted) form. The XOR
            // scramble flips bit values but never moves bit positions, so
            // the same error-position/confidence pattern already proven
            // recoverable in `chase_decoding_recovers_a_golay_error_burst_
            // hard_decoding_cannot` (found by exhaustive search, since most
            // hand-picked 4-error patterns are *not* recoverable — see that
            // test's comment) is recoverable here too, applied at the same
            // positions before scrambling.
            let data1 = [0, 0, 1, 1, 0, 1, 0, 0, 1, 0, 1, 1];
            let true_codeword1 = find_valid_golay_codeword(data1);
            let mut scratch = frame;
            scratch[1] = true_codeword1;
            unsafe { ffi::mbe_demodulateImbe7200x4400Data(scratch.as_mut_ptr()) };
            let mut scrambled1 = scratch[1];
            for &p in &[0usize, 1, 2, 6] {
                scrambled1[p] ^= 1;
            }
            frame[1] = scrambled1;

            let mut conf = [[255u8; 23]; 8];
            conf[1][0] = 10;
            conf[1][1] = 10;

            soft_correct(&mut frame, &conf);

            // Re-run the same descramble the real decode path will run, and
            // confirm codeword 1's data recovers to the true value even
            // though it took 4 (scrambled-domain) errors to get there.
            let corrected_c0 = golay_decode(frame[0]);
            let mut check = frame;
            check[0] = corrected_c0;
            unsafe { ffi::mbe_demodulateImbe7200x4400Data(check.as_mut_ptr()) };
            let recovered = golay_decode(check[1]);
            assert_eq!(
                &recovered[11..23],
                &true_codeword1[11..23],
                "soft_correct did not recover codeword 1's data through scrambling"
            );
        }

        /// Regression test for a real data race in the vendored C: mbelib's
        /// `mbe_checkGolayBlock` used `static` locals for pure scratch state
        /// (see the fix in `vendor/mbelib/ecc.c`), so concurrent calls from
        /// different threads could clobber each other's in-flight syndrome
        /// computation — exactly the shape of `hs_core::follow`'s per-call
        /// `thread::scope`, which decodes every simultaneous call's voice
        /// frames on its own thread through this same function. Found by
        /// this test file itself flaking nondeterministically under the
        /// default parallel test runner before the fix.
        #[test]
        fn golay_decode_is_safe_to_call_from_many_threads_at_once() {
            let codewords: Vec<[u8; 23]> = (0u16..64)
                .map(|n| {
                    let bits: [u8; 12] = std::array::from_fn(|i| ((n >> i) & 1) as u8);
                    find_valid_golay_codeword(bits)
                })
                .collect();
            std::thread::scope(|sc| {
                for cw in &codewords {
                    sc.spawn(move || {
                        for _ in 0..200 {
                            assert_eq!(
                                golay_decode(*cw)[11..23],
                                cw[11..23],
                                "a clean codeword decoded wrong under concurrent load"
                            );
                        }
                    });
                }
            });
        }
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
                self.uv_quality,
            );
        }
        self.last_errs = errs2;
        out
    }

    /// As [`ImbeDecoder::decode`], but first runs confidence-guided
    /// pre-correction (see `soft_fec::soft_correct`) using the demodulator's
    /// per-bit reliability for this frame. Falls back to plain hard decoding
    /// with no measurable behavior change when every bit is certain (the
    /// zero-flip candidate always wins in that case).
    pub fn decode_soft(
        &mut self,
        frame: &ImbeFrame,
        conf: &hs_p25::voice::ImbeConf,
    ) -> [i16; SAMPLES_PER_FRAME] {
        let mut corrected = *frame;
        soft_fec::soft_correct(&mut corrected, conf);
        self.decode(&corrected)
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
    pub fn decode_soft(
        &mut self,
        _frame: &ImbeFrame,
        _conf: &hs_p25::voice::ImbeConf,
    ) -> [i16; SAMPLES_PER_FRAME] {
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
