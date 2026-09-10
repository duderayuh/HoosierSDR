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

/// AGC target power for voice PCM: 0.015 (RMS ≈ 0.12 of full scale) rather
/// than `AudioAgc::new`'s default 0.0625 (RMS 0.25), which measurably
/// clipped real decoded speech (see the commit that added this constant —
/// ~2.6% of samples on a real live call). At 0.015, a peak needs to reach
/// ~8x RMS before clipping, comfortably past real speech's crest factor.
const VOICE_AGC_TARGET: f32 = 0.015;

impl Concealer {
    pub fn new() -> Self {
        Self {
            held: Vec::new(),
            held_repeats: 0,
            agc: AudioAgc::with_target(VOICE_AGC_TARGET),
        }
    }

    /// Process one decoded voice frame's PCM in place: level-normalize it,
    /// then conceal it against the held (already-leveled) buffer when
    /// `quality` is poor. `pcm` should be the exact samples `quality` scores
    /// (one IMBE frame, 160 samples at 8 kHz for P25 Phase I) — concealment
    /// blends sample-for-sample against the previous frame of the same
    /// length, so mismatched lengths would blend the wrong content together.
    pub fn process(&mut self, pcm: &mut [i16], quality: VoiceQuality) {
        // Level-normalize the freshly decoded audio *first*, before any
        // concealment blending or fading touches it. Doing this after
        // concealment (an earlier version of this function did) let the
        // AGC's power estimate see whatever concealment had already faded
        // toward — including near-silence during a bad stretch — so it
        // drifted to think the channel had gone quiet. The next frame with
        // real content then looked, to the AGC, like a huge jump from
        // near-zero, and it overshot exactly like the fresh-instance
        // startup pop this crate already fixed once, except recurring
        // *mid-call* every time a marginal stretch resolved back to a good
        // one — measured on a real archived call as an audible click/pop at
        // several points through a single transmission (see git history).
        // Running the AGC on the true decode first means it only ever
        // tracks the actual signal's level, never a concealment artifact,
        // and both `held` and the blend below already start from
        // consistently-leveled audio.
        //
        // Whole-frame leveling, not sample-by-sample: the AGC sees the
        // frame's peak before applying any gain and caps the gain so the
        // peak stays under `hs_decoders::frontend::PEAK_CEILING`. With
        // sample-wise leveling, a frame arriving several times hotter than
        // the 125 ms running level was hard-clipped for its first 20–40 ms
        // — measured at ~0.5 full-scale bursts per second of speech on real
        // calls, the "electronic" crackle mixed into decoded voice.
        self.agc.frame(pcm);

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
    }

    /// Produce one frame for a slot the channel was on the air for but no
    /// voice frame was decoded from: the held (last good) audio, fading out
    /// over the same budget a run of bad frames gets. This is what keeps
    /// the audio timeline honest — a lost frame becomes 20 ms of held
    /// sound, not 20 ms deleted from the call.
    ///
    /// The AGC is deliberately not run here: it has no new signal to
    /// measure, and feeding it the fade would starve its estimate so the
    /// next real frame spiked (the same failure `process` guards against).
    pub fn conceal_missing(&mut self, frame_len: usize, out: &mut Vec<i16>) {
        self.held_repeats += 1;
        let fade = (1.0 - self.held_repeats as f32 / MAX_HELD_REPEATS as f32).clamp(0.0, 1.0);
        out.extend(
            (0..frame_len).map(|i| (self.held.get(i).copied().unwrap_or(0) as f32 * fade) as i16),
        );
    }

    /// The transmission ended: forget the held audio so the next one never
    /// conceals with a stranger's voice. The level estimate is kept — the
    /// channel's loudness is a property of the system, not the talker.
    pub fn end_of_transmission(&mut self) {
        self.held.clear();
        self.held_repeats = 0;
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
        // Establish a held buffer at a distinctive positive level. `held`
        // now stores AGC'd (already leveled), not raw, samples — see
        // `Concealer::process`'s doc — so its exact values aren't asserted,
        // only that they came out positive, matching the positive input.
        let mut good_pcm = [20_000i16; 160];
        c.process(&mut good_pcm, good());
        assert!(c.held.iter().all(|&s| s > 0), "held: {:?}", &c.held[..4]);

        // A garbled frame at the opposite extreme, scored fully bad.
        let mut bad_pcm = [-20_000i16; 160];
        c.process(&mut bad_pcm, bad());
        // Fully bad (score 0.0, below CONCEAL_FLOOR) means decoded_weight
        // clamps to 0: the output should be *entirely* the held level — its
        // sign must have flipped positive, proof the held buffer dominated,
        // not the (negative) decoded frame it was blended against.
        assert!(
            bad_pcm.iter().all(|&s| s > 0),
            "a fully-bad frame should be replaced by the held buffer, not the garbled decode: {:?}",
            &bad_pcm[..4]
        );
    }

    #[test]
    fn a_missing_frame_is_the_held_audio_fading_and_leaves_the_agc_alone() {
        let mut c = Concealer::new();
        let mut good_pcm = [8_000i16; 160];
        for _ in 0..50 {
            good_pcm = [8_000i16; 160];
            c.process(&mut good_pcm, good());
        }
        let level = good_pcm[0];
        let mut out = Vec::new();
        c.conceal_missing(160, &mut out);
        assert_eq!(out.len(), 160);
        assert!(out[0] > 0 && (out[0] as i32 - level as i32).abs() < level as i32 / 10);
        for _ in 0..(MAX_HELD_REPEATS + 5) {
            out.clear();
            c.conceal_missing(160, &mut out);
        }
        assert_eq!(out[0], 0, "faded to silence");
        // A real frame afterwards lands at the settled level, not a spike.
        let mut pcm = [8_000i16; 160];
        c.process(&mut pcm, good());
        assert!(
            (pcm[0] as i32 - level as i32).abs() < level as i32 / 5,
            "{} vs {level}",
            pcm[0]
        );
        c.end_of_transmission();
        out.clear();
        c.conceal_missing(160, &mut out);
        assert!(
            out.iter().all(|&s| s == 0),
            "nothing held after a transmission ends"
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

    /// Regression test for a real click/pop found in an archived live call
    /// (talkgroup 49F-DISPATCH on a simulcast site — see git history): a
    /// stretch of marginal-quality frames whose *raw decode* still carried
    /// real energy (not literal silence) got attenuated by concealment on
    /// their way out, and — when the AGC ran on that already-faded output —
    /// its power estimate drifted down to match the faded audio rather than
    /// the true decoded signal. The next good, normally-loud frame then
    /// looked like a huge jump from near-silence and spiked toward full
    /// scale, an audible click every time a marginal stretch resolved back
    /// to a good one.
    #[test]
    fn a_marginal_stretch_does_not_starve_the_agc_for_the_frame_after_it() {
        let mut c = Concealer::new();
        // Settle the AGC on ordinary, good-quality, moderately loud audio.
        let mut settled_level = 0i16;
        for _ in 0..500 {
            let mut pcm = [8_000i16; 160];
            c.process(&mut pcm, good());
            settled_level = pcm[0];
        }

        // A long stretch of fully-bad (concealed, held-dominated) frames
        // whose raw decode still has real energy — not silence — so a
        // correctly-behaving AGC should barely move its estimate even
        // though the concealed *output* fades toward near-zero late in the
        // stretch (fade approaches 0 as held_repeats approaches
        // MAX_HELD_REPEATS).
        let marginal = VoiceQuality {
            confidence: 0.0,
            fec_errors: 10,
            lock: None,
        };
        assert!(
            marginal.score() < CONCEAL_FLOOR,
            "test setup: must be fully held-dominated (decoded_weight 0)"
        );
        for _ in 0..(MAX_HELD_REPEATS - 5) {
            let mut pcm = [8_000i16; 160];
            c.process(&mut pcm, marginal);
        }

        // A normal frame right after the marginal stretch: must land back
        // near the pre-stretch settled level, not spike to several times it
        // the way a starved AGC would (found on a real call: a 25311 spike
        // right after a stretch that had faded to ~1500 — a ~17x jump, well
        // past anything a real speech onset produces).
        let mut pcm = [8_000i16; 160];
        c.process(&mut pcm, good());
        assert!(
            pcm.iter().all(|&s| (s as f32) < settled_level as f32 * 2.0),
            "a normal frame right after a marginal stretch should return near the settled \
             level ({settled_level}), not spike: {:?}",
            &pcm[..4]
        );
    }

    /// Regression test for real clipping measured on a live-decoded call:
    /// a synthetic signal with a speech-like crest factor (~5x, periodic
    /// loud bursts over a quiet baseline) fed through many "good" frames —
    /// enough for the slow (alpha=0.001) AGC to settle before measuring —
    /// A frame far hotter than the speech before it — a stressed syllable, or
    /// a corrupted IMBE gain field — must come out loud but under the peak
    /// ceiling, never pinned at full scale: exactly the bursts measured on
    /// real calls before the AGC leveled whole frames.
    #[test]
    fn a_hot_frame_after_quiet_speech_is_limited_not_clipped() {
        let mut c = Concealer::new();
        for k in 0..400 {
            let mut pcm = [0i16; 160];
            for (i, s) in pcm.iter_mut().enumerate() {
                *s = (500.0 * ((k * 160 + i) as f32 * 0.12).sin()) as i16;
            }
            c.process(&mut pcm, good());
        }
        let mut hot = [0i16; 160];
        for (i, s) in hot.iter_mut().enumerate() {
            *s = (15_000.0 * (i as f32 * 0.12).sin()) as i16;
        }
        c.process(&mut hot, good());
        let peak = hot.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!(peak < 32_767, "hot frame must not be pinned at full scale");
        assert!(
            peak as f32 <= hs_decoders::frontend::PEAK_CEILING * 32_767.0 + 1.0,
            "hot frame should be held under the ceiling, peaked at {peak}"
        );
        assert!(peak > 20_000, "the frame should still be loud: {peak}");
    }

    /// must clip only rarely once settled.
    #[test]
    fn agc_target_leaves_headroom_for_a_speech_like_crest_factor() {
        let mut c = Concealer::new();
        let settle_frames = 300;
        let measure_frames = 300;
        let mut clipped = 0usize;
        let mut total = 0usize;
        for frame_idx in 0..(settle_frames + measure_frames) {
            let mut pcm = [0i16; 160];
            for (i, s) in pcm.iter_mut().enumerate() {
                let n = (frame_idx * 160 + i) as f32;
                let base = 3000.0 * (n * 0.05).sin();
                // A short, ~5x-amplitude burst every 50 samples — a crude
                // stand-in for a syllable peak over quieter speech.
                let in_burst = (n as i64) % 50 < 5;
                let burst = if in_burst {
                    15_000.0 * (n * 0.3).sin()
                } else {
                    0.0
                };
                *s = (base + burst) as i16;
            }
            c.process(&mut pcm, good());
            if frame_idx >= settle_frames {
                for &s in &pcm {
                    total += 1;
                    if s.unsigned_abs() >= 32_767 {
                        clipped += 1;
                    }
                }
            }
        }
        let frac = clipped as f32 / total as f32;
        assert!(
            frac < 0.005,
            "clipped fraction too high for a speech-like crest factor: {frac:.4} ({clipped}/{total})"
        );
    }

    #[test]
    fn a_frame_right_at_the_threshold_passes_through_almost_unmodified() {
        let mut c = Concealer::new();
        let mut good_pcm = [20_000i16; 160];
        c.process(&mut good_pcm, good());
        let held_after_first_frame = c.held[0];

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
        // `held` stores AGC'd, not raw, samples (see `Concealer::process`'s
        // doc), so the exact value isn't asserted — only that it changed
        // (this frame's distinct content replaced the previous held buffer,
        // proving this frame did *not* go through the concealment branch)
        // and kept the right sign (input was positive).
        assert_ne!(c.held[0], held_after_first_frame);
        assert!(c.held[0] > 0);
    }
}
