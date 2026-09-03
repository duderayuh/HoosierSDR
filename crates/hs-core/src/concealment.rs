//! Voice-frame concealment and level normalization: Rust-side audio
//! post-processing sitting between the vocoder's raw PCM output and whatever
//! plays it, driven by [`crate::decoder::VoiceQuality`].
//!
//! mbelib's own concealment (hold-and-repeat, then mute) lives entirely
//! inside the vendored C, operating on its internal spectral parameters
//! using only its own post-FEC error count — invisible and untunable from
//! here (see the vendored mbelib README; there is no hook to change it
//! short of patching the C). This is a second, independent stage on the PCM
//! mbelib already produced, using the fuller `VoiceQuality` signal — which
//! also sees pre-FEC demodulator confidence and, on CQPSK, carrier lock — to
//! catch a frame mbelib's narrower, FEC-only view judged clean enough to
//! pass through untouched but that a listener would hear as marginal.
//!
//! De-emphasis is deliberately not implemented here: IMBE/AMBE's analysis-
//! synthesis model works on spectral parameters, not a pre-emphasized
//! waveform, so vocoded PCM was never pre-emphasized in the first place —
//! there is nothing to undo.

use crate::decoder::VoiceQuality;
use hs_decoders::frontend::AudioAgc;

/// Below this composite score a frame is blended toward the held (last
/// good) buffer instead of being played as decoded.
const CONCEAL_BELOW: f32 = 0.5;
/// At or below this score a frame is treated as fully unusable: blended in
/// almost entirely from the held buffer rather than partially.
const CONCEAL_FLOOR: f32 = 0.15;
/// After this many consecutive concealed frames (1.6 s at 20 ms/frame), stop
/// repeating the held buffer and fade its contribution toward silence
/// instead — looping the same 20 ms of audio indefinitely reads to a
/// listener as a stuck, robotic artifact, worse than fading out.
const MAX_HELD_REPEATS: u32 = 80;

/// Per-channel concealment and leveling state. One per audio output stream
/// (e.g. one per active call), not shared — the held buffer and AGC gain are
/// specific to one continuous stream of speech.
pub struct Concealer {
    held: Vec<i16>,
    held_repeats: u32,
    agc: AudioAgc,
}

impl Concealer {
    pub fn new() -> Self {
        Self {
            held: Vec::new(),
            held_repeats: 0,
            agc: AudioAgc::new(),
        }
    }

    /// Process one decoded voice frame's PCM in place: conceal it against
    /// the held buffer when `quality` is poor, then apply level
    /// normalization. `pcm` should be the exact samples `quality` scores
    /// (one IMBE frame, 160 samples at 8 kHz for P25 Phase I) — concealment
    /// blends sample-for-sample against the previous frame of the same
    /// length, so mismatched lengths would blend the wrong content together.
    pub fn process(&mut self, pcm: &mut [i16], quality: VoiceQuality) {
        let score = quality.score();
        if score < CONCEAL_BELOW {
            self.held_repeats += 1;
            // 1.0 while still within the repeat budget, decaying linearly to
            // 0.0 at MAX_HELD_REPEATS — an unboundedly repeated buffer would
            // otherwise loop forever on a long-dead channel.
            let fade = (1.0 - self.held_repeats as f32 / MAX_HELD_REPEATS as f32).clamp(0.0, 1.0);
            // How much of the newly *decoded* (poor) frame to still let
            // through: 0 at or below the floor (fully replaced by the held
            // buffer), rising to ~1 right at the concealment threshold, so
            // there's no audible seam where concealment switches on.
            let decoded_weight =
                ((score - CONCEAL_FLOOR) / (CONCEAL_BELOW - CONCEAL_FLOOR)).clamp(0.0, 1.0);
            for (i, s) in pcm.iter_mut().enumerate() {
                // Missing history (nothing held yet, or this frame is longer
                // than what was held) falls back to silence, not garbage —
                // an attenuated bad decode, never a stale unrelated buffer.
                let held = self.held.get(i).copied().unwrap_or(0) as f32 * fade;
                let decoded = *s as f32;
                *s = (held * (1.0 - decoded_weight) + decoded * decoded_weight) as i16;
            }
        } else {
            self.held_repeats = 0;
            self.held.clear();
            self.held.extend_from_slice(pcm);
        }
        for s in pcm.iter_mut() {
            *s = self.agc.sample(*s as f32 / 32_768.0);
        }
    }
}

impl Default for Concealer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quality(score_inputs: (f32, u32, Option<f32>)) -> VoiceQuality {
        VoiceQuality {
            confidence: score_inputs.0,
            fec_errors: score_inputs.1,
            lock: score_inputs.2,
        }
    }

    fn good() -> VoiceQuality {
        quality((1.0, 0, None)) // score 1.0
    }

    fn bad() -> VoiceQuality {
        quality((0.0, 20, None)) // score 0.0, well under CONCEAL_FLOOR
    }

    #[test]
    fn a_good_frame_updates_the_held_buffer_and_resets_the_repeat_count() {
        let mut c = Concealer::new();
        let mut pcm = [1000i16; 160];
        c.process(&mut pcm, good());
        assert_eq!(c.held_repeats, 0);
        assert_eq!(c.held.len(), 160);
    }

    #[test]
    fn a_bad_frame_right_after_a_good_one_is_pulled_toward_the_held_buffer() {
        let mut c = Concealer::new();
        // Establish a held buffer of a distinctive constant level. AGC needs
        // a moment to settle, so read `held` directly rather than asserting
        // on this frame's own (still-normalizing) output.
        let mut good_pcm = [20_000i16; 160];
        c.process(&mut good_pcm, good());
        assert!(c.held.iter().all(|&s| s == 20_000));

        // A garbled frame at the opposite extreme, scored fully bad.
        let mut bad_pcm = [-20_000i16; 160];
        c.process(&mut bad_pcm, bad());
        // Fully bad (score 0.0, below CONCEAL_FLOOR) means decoded_weight
        // clamps to 0: the output should be *entirely* the held level (pre-
        // AGC it would be exactly +20000; AGC may rescale it, but the sign
        // must have flipped positive — proof the held buffer dominated,
        // not the decoded frame it was blended against).
        assert!(
            bad_pcm.iter().all(|&s| s > 0),
            "a fully-bad frame should be replaced by the held buffer, not the garbled decode: {:?}",
            &bad_pcm[..4]
        );
    }

    #[test]
    fn concealment_fades_out_after_many_consecutive_bad_frames() {
        let mut c = Concealer::new();
        let mut good_pcm = [20_000i16; 160];
        c.process(&mut good_pcm, good());

        // Drive well past MAX_HELD_REPEATS with fully-bad frames.
        let mut last_conceal_magnitude = i32::MAX;
        for _ in 0..(MAX_HELD_REPEATS + 20) {
            let mut bad_pcm = [-20_000i16; 160];
            c.process(&mut bad_pcm, bad());
            last_conceal_magnitude = bad_pcm[0] as i32;
        }
        // Long after the repeat budget, concealment should have faded to
        // (near) silence rather than still confidently outputting the
        // original held level.
        assert!(
            last_conceal_magnitude.abs() < 5000,
            "concealment should fade toward silence, not repeat the held buffer forever: got {last_conceal_magnitude}"
        );
    }

    #[test]
    fn a_frame_right_at_the_threshold_passes_through_almost_unmodified() {
        let mut c = Concealer::new();
        let mut good_pcm = [20_000i16; 160];
        c.process(&mut good_pcm, good());

        // confidence=0.0, fec_errors=0, lock=None scores exactly
        // 0.5*0.0 + 0.5*1.0 = CONCEAL_BELOW under the documented no-lock
        // formula. The concealment check is strictly `<`, so a frame scoring
        // exactly at the threshold is out of it entirely: its own content
        // becomes the new held buffer, not the old one.
        let at_threshold = VoiceQuality {
            confidence: 0.0,
            fec_errors: 0,
            lock: None,
        };
        assert_eq!(at_threshold.score(), CONCEAL_BELOW);
        let mut distinct_pcm = [5_000i16; 160];
        c.process(&mut distinct_pcm, at_threshold);
        assert_eq!(c.held_repeats, 0);
        assert_eq!(c.held[0], 5_000);
    }
}
