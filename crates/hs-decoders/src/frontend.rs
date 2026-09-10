//! Shared analog front-end: digital downconversion + channel-filtered
//! decimation to a working rate, audio-rate resampling, and an FM noise
//! squelch. Every analog decoder in this crate is built from these three
//! pieces plus a demodulator, so the physical-layer work lives here once.

use hs_dsp::fir::{lowpass_taps, Fir, FirC};
use hs_dsp::C32;

use crate::AUDIO_RATE;

/// Target working rate the demodulators run at, in Hz. A comfortable ~48 kHz
/// for a 12.5/25 kHz channel; the actual value is the nearest multiple of
/// [`AUDIO_RATE`] the capture rate divides to.
const TARGET_WORKING_HZ: f64 = 48_000.0;

/// Digital downconverter: mix the channel `offset_hz` from the capture centre
/// down to DC, channel-filter, and integer-decimate to the working rate.
///
/// Unlike `hs_dsp::decimate::Decimator`, this places no P25 symbol-rate
/// constraint on the capture rate — it only requires the rate to be a multiple
/// of the 8 kHz audio rate, which every SDR capture rate this project uses
/// (240 kHz, 2.4 MHz, 9.6 MHz, 48 kHz …) already satisfies. That keeps the
/// working rate an exact integer multiple of the audio rate, so the downstream
/// audio resampler is plain integer decimation.
pub struct Ddc {
    step: Option<C32>,
    nco: C32,
    renorm: u32,
    fir: FirC,
    working_rate: f64,
    /// working_rate / AUDIO_RATE, the integer audio decimation factor.
    audio_decim: usize,
}

impl Ddc {
    /// Build a downconverter for `capture_rate` selecting the channel
    /// `offset_hz` away, keeping `passband_hz` of bandwidth. `capture_rate`
    /// must be a positive multiple of [`AUDIO_RATE`].
    pub fn new(capture_rate: f64, offset_hz: f64, passband_hz: f64) -> Self {
        let total = capture_rate / AUDIO_RATE as f64;
        assert!(
            capture_rate > 0.0 && (total.fract()).abs() < 1e-6,
            "capture rate {capture_rate} must be a multiple of {AUDIO_RATE} Hz"
        );
        let total = total.round() as usize;
        // Choose the audio-decimation factor `d` (a divisor of `total`, so the
        // decimation stays integer end to end) whose working rate 8000·d lands
        // nearest the 48 kHz target.
        let want = (TARGET_WORKING_HZ / AUDIO_RATE as f64).round() as usize; // 6
        let mut audio_decim = 1;
        let mut best = usize::MAX;
        for d in 1..=total {
            if total.is_multiple_of(d) {
                let err = want.abs_diff(d);
                if err < best {
                    best = err;
                    audio_decim = d;
                }
            }
        }
        let factor = total / audio_decim;
        let working_rate = capture_rate / factor as f64;

        let fir = if factor == 1 {
            // No decimation needed; a single-tap identity filter keeps the type
            // uniform.
            FirC::new(vec![1.0], 1)
        } else {
            let cutoff = passband_hz / capture_rate;
            let stop = working_rate / 2.0 / capture_rate;
            let transition = (stop - cutoff).max(1e-3);
            let mut n = (3.3 / transition).ceil() as usize;
            n = n.clamp(31, 4095);
            if n.is_multiple_of(2) {
                n += 1;
            }
            let design = cutoff + transition / 2.0;
            FirC::new(lowpass_taps(n, design), factor)
        };

        let step = if offset_hz != 0.0 {
            let w = -2.0 * std::f64::consts::PI * offset_hz / capture_rate;
            Some(C32::new(w.cos() as f32, w.sin() as f32))
        } else {
            None
        };

        Self {
            step,
            nco: C32::new(1.0, 0.0),
            renorm: 0,
            fir,
            working_rate,
            audio_decim,
        }
    }

    pub fn working_rate(&self) -> f64 {
        self.working_rate
    }

    /// Integer factor from the working rate down to [`AUDIO_RATE`].
    pub fn audio_decim(&self) -> usize {
        self.audio_decim
    }

    /// Push one capture-rate complex sample; returns a working-rate sample on
    /// decimation instants.
    pub fn push(&mut self, x: C32) -> Option<C32> {
        let x = match self.step {
            Some(step) => {
                let y = x * self.nco;
                self.nco = self.nco * step;
                self.renorm += 1;
                if self.renorm >= 1024 {
                    self.renorm = 0;
                    let m = self.nco.norm_sq().sqrt();
                    if m > 1e-6 {
                        self.nco = self.nco.scale(1.0 / m);
                    }
                }
                y
            }
            None => x,
        };
        self.fir.push(x)
    }
}

/// Linear-phase highpass taps: spectral inversion of a lowpass (δ − lowpass).
/// `cutoff` is normalized to the sample rate (0..0.5).
pub fn highpass_taps(num_taps: usize, cutoff: f64) -> Vec<f32> {
    let mut taps = lowpass_taps(num_taps, cutoff);
    for t in taps.iter_mut() {
        *t = -*t;
    }
    taps[num_taps / 2] += 1.0;
    taps
}

/// Anti-aliased integer decimation of a real audio stream from the working
/// rate down to [`AUDIO_RATE`]. Fixed low-pass at the voice band keeps this a
/// one-tap-per-sample cost.
pub struct AudioResampler {
    lp: Fir,
    decim: usize,
    phase: usize,
}

impl AudioResampler {
    pub fn new(working_rate: f64, decim: usize) -> Self {
        // Cut just above the 3 kHz voice band; the 8 kHz output Nyquist is
        // 4 kHz, so this leaves a clean guard before downsampling.
        let cutoff = 3_200.0 / working_rate;
        let transition = (4_000.0 / working_rate - cutoff).max(2e-3);
        let mut n = (3.3 / transition).ceil() as usize;
        n = n.clamp(31, 1023);
        if n.is_multiple_of(2) {
            n += 1;
        }
        Self {
            lp: Fir::new(lowpass_taps(n, cutoff)),
            decim: decim.max(1),
            phase: 0,
        }
    }

    /// Push one working-rate audio sample; returns an 8 kHz sample on
    /// decimation instants.
    pub fn push(&mut self, x: f32) -> Option<f32> {
        let y = self.lp.filter(x);
        self.phase += 1;
        if self.phase < self.decim {
            return None;
        }
        self.phase = 0;
        Some(y)
    }
}

/// Slow RMS normalizer for demodulated audio, so decoded output lands at a
/// consistent level regardless of deviation, modulation depth or path loss.
/// Converts a normalized float to a clipped 16-bit sample.
pub struct AudioAgc {
    /// EWMA of the audio power.
    power: f32,
    alpha: f32,
    target: f32,
    gain: f32,
    /// Gain ceiling left behind by [`AudioAgc::frame`]'s peak limiter,
    /// releasing back toward [`GAIN_MAX`] frame by frame.
    limit: f32,
}

/// Largest gain the RMS loop may apply.
const GAIN_MAX: f32 = 1e4;
/// Largest normalized magnitude a frame leaves [`AudioAgc::frame`] with. The
/// gain is capped per frame so this is never exceeded — short of the
/// per-sample safety clamp, which only fires when a peak lands inside the
/// few-millisecond ramp below.
pub const PEAK_CEILING: f32 = 0.9;
/// Per-frame release of the peak limiter: the ceiling may rise by this factor
/// (~1 dB) each frame, so a single hot frame does not hold the level down for
/// long, but it also does not snap back up and pump.
const LIMIT_RELEASE: f32 = 1.122;
/// Samples over which a *lowered* ceiling is eased in at the start of a frame,
/// so the gain does not step at the frame boundary (a step multiplies a
/// non-zero boundary sample by a different factor than its neighbour — a
/// click). 4 ms at 8 kHz.
const LIMIT_RAMP: usize = 32;

impl AudioAgc {
    pub fn new() -> Self {
        Self::with_target(0.0625)
    }

    /// As [`AudioAgc::new`], but targeting `target` mean-square power instead
    /// of the default 0.0625 (~0.25 full-scale RMS). A caller whose audio has
    /// a higher crest factor than the analog paths this default was tuned
    /// against — e.g. vocoded speech — needs a lower target for the same
    /// clipping margin: at 0.0625 (RMS 0.25), any peak past 4x RMS clips,
    /// which real speech's crest factor exceeds often enough to be audible.
    pub fn with_target(target: f32) -> Self {
        Self {
            // Seeded at the target itself, not 0.0: starting "blind" (as if
            // silent) makes the very first loud sample look like an
            // enormous power jump relative to the tiny EWMA so far, so the
            // gain formula overshoots hugely before the slow (alpha=0.001)
            // average catches up — an abrupt, audible pop right when audio
            // starts, worst for a caller (like a per-call decoder) that
            // constructs a fresh AGC right where loud audio is about to
            // begin, with no gradual ramp-up to let it adapt gently.
            // Assuming "already at the target level" instead means a
            // correctly-leveled signal starts with gain ~1.0 and barely
            // moves; the AGC still adapts (up or down) to whatever the
            // signal actually is, just without the false-start spike.
            power: target,
            alpha: 0.001,
            target,
            gain: 1.0,
            limit: GAIN_MAX,
        }
    }

    /// Normalize one audio sample and quantize to i16.
    pub fn sample(&mut self, x: f32) -> i16 {
        let g = self.track(x);
        let y = (x * g).clamp(-1.0, 1.0);
        (y * 32_767.0) as i16
    }

    /// Advance the power estimate by one sample and return the RMS-loop gain.
    fn track(&mut self, x: f32) -> f32 {
        self.power += self.alpha * (x * x - self.power);
        if self.power > 1e-9 {
            self.gain = (self.target / self.power).sqrt().clamp(1e-3, GAIN_MAX);
        }
        self.gain
    }

    /// Normalize one whole frame of i16 audio in place, with lookahead peak
    /// limiting.
    ///
    /// The slow RMS loop is the same one [`AudioAgc::sample`] runs — the
    /// power estimate advances sample by sample from the *input*, so release
    /// and level matching behave exactly as before. What changes is that the
    /// frame's peak is known before any gain is applied, so the gain is
    /// capped at whatever keeps that peak under [`PEAK_CEILING`]. The RMS
    /// loop's 125 ms time constant otherwise lets a frame that arrives
    /// several times hotter than the running level hard-clip for 20–40 ms
    /// before the estimate catches up — measured on real calls as ~0.5
    /// full-scale bursts per second of speech, the "electronic" crackle in
    /// decoded voice. The cap only ever lowers gain; on audio the RMS loop
    /// already keeps under the ceiling this is a no-op.
    pub fn frame(&mut self, pcm: &mut [i16]) {
        let peak = pcm
            .iter()
            .map(|&s| (s as f32 / 32_768.0).abs())
            .fold(0.0f32, f32::max);
        let cap = if peak > 0.0 {
            PEAK_CEILING / peak
        } else {
            GAIN_MAX
        };
        let start = self.limit;
        let limit = (self.limit * LIMIT_RELEASE).min(cap).min(GAIN_MAX);
        for (i, s) in pcm.iter_mut().enumerate() {
            let x = *s as f32 / 32_768.0;
            let rms_gain = self.track(x);
            // Ease a lowered ceiling in over the first few ms; a raised one
            // applies at once (it is at most ~1 dB above the last frame's).
            let ceiling = if limit < start {
                let t = ((i + 1) as f32 / LIMIT_RAMP as f32).min(1.0);
                start + (limit - start) * t
            } else {
                limit
            };
            let mut g = rms_gain.min(ceiling);
            // Hard guarantee for a peak that lands inside the ramp.
            if x.abs() * g > PEAK_CEILING {
                g = PEAK_CEILING / x.abs();
            }
            *s = (x * g * 32_767.0) as i16;
        }
        self.limit = limit;
    }
}

impl Default for AudioAgc {
    fn default() -> Self {
        Self::new()
    }
}

/// FM noise squelch. When no carrier is present the discriminator output is
/// full of high-frequency noise; a real signal quiets that band. This tracks
/// energy above the voice band and gates on it, with hysteresis so a marginal
/// signal does not chatter open and closed.
pub struct NoiseSquelch {
    hp: Fir,
    /// EWMA of the high-band energy (rad²/sample of discriminator output).
    noise: f32,
    alpha: f32,
    open: bool,
    open_thresh: f32,
    close_thresh: f32,
    /// Samples processed, to hold the gate closed during warm-up.
    warm: u32,
}

impl NoiseSquelch {
    /// `level` in 0..1 sets sensitivity: 0 opens easily (tight squelch off),
    /// 1 demands a very clean signal. `working_rate` is the discriminator
    /// output rate.
    pub fn new(working_rate: f64, level: f32) -> Self {
        // Measure the octave above the voice band. At 48 kHz working that is
        // ~5–10 kHz, empty of voice but full of FM noise on an open channel.
        let cutoff = (5_000.0 / working_rate).min(0.45);
        let n = 63;
        // Map level→thresholds. Discriminator output is in radians/sample; on
        // noise the high-band energy is large, on a locked carrier it collapses
        // toward zero. These bounds were chosen against the synthetic FM in the
        // tests and give clean open/close on ~20 dB SNR and up.
        let open_thresh = 0.02 + 0.20 * (1.0 - level).clamp(0.0, 1.0);
        Self {
            hp: Fir::new(highpass_taps(n, cutoff)),
            noise: 1.0,
            alpha: 0.002,
            open: false,
            open_thresh,
            close_thresh: open_thresh * 1.8,
            warm: 0,
        }
    }

    /// Update with one discriminator-output sample; returns whether the
    /// squelch is currently open.
    pub fn update(&mut self, disc: f32) -> bool {
        let h = self.hp.filter(disc);
        self.noise += self.alpha * (h * h - self.noise);
        self.warm = self.warm.saturating_add(1);
        if self.warm < 2_000 {
            return false;
        }
        if self.open {
            if self.noise > self.close_thresh {
                self.open = false;
            }
        } else if self.noise < self.open_thresh {
            self.open = true;
        }
        self.open
    }

    pub fn is_open(&self) -> bool {
        self.open
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddc_picks_working_rate_multiple_of_audio() {
        for &rate in &[48_000.0f64, 240_000.0, 2_400_000.0, 9_600_000.0] {
            let d = Ddc::new(rate, 0.0, 6_000.0);
            let w = d.working_rate();
            assert!(
                (w / AUDIO_RATE as f64).fract().abs() < 1e-9,
                "working {w} not a multiple of {AUDIO_RATE}"
            );
            assert!((32_000.0..=64_000.0).contains(&w), "working rate {w}");
            assert_eq!(d.audio_decim(), (w / AUDIO_RATE as f64).round() as usize);
        }
    }

    #[test]
    fn highpass_rejects_dc_passes_high() {
        let taps = highpass_taps(63, 0.2);
        // DC gain (sum of taps) ~ 0.
        let dc: f32 = taps.iter().sum();
        assert!(dc.abs() < 1e-3, "hp DC gain {dc}");
    }

    /// A fresh `AudioAgc` seeing full-volume audio right from its first
    /// sample (no gradual ramp-up, e.g. a P25 call decoder allocated fresh
    /// per call, not per session) must not clip the very first samples: the
    /// power estimate starting at 0.0 makes the gain formula see almost no
    /// signal yet, so it overshoots wildly before the slow (alpha=0.001)
    /// EWMA catches up — an abrupt, audible pop right at the start of every
    /// clip, distinct from (and worse than) ordinary steady-state clipping.
    #[test]
    fn a_fresh_agc_does_not_slam_the_first_loud_samples() {
        let mut agc = AudioAgc::with_target(0.015);
        let loud = 0.5f32; // a typical loud speech sample, normalized
        let first_20: Vec<i16> = (0..20).map(|_| agc.sample(loud)).collect();
        let clipped = first_20
            .iter()
            .filter(|&&s| s.unsigned_abs() >= 32_767)
            .count();
        assert_eq!(
            clipped, 0,
            "the first 20 samples of a loud onset should not clip: {first_20:?}"
        );
    }

    /// One frame of 8 kHz audio: a sine at `amp` (i16 scale), `cycles` per
    /// frame.
    fn sine_frame(amp: f32, cycles: f32, frame_idx: usize) -> [i16; 160] {
        let mut f = [0i16; 160];
        for (i, s) in f.iter_mut().enumerate() {
            let n = (frame_idx * 160 + i) as f32;
            *s = (amp * (n * cycles * std::f32::consts::TAU / 160.0).sin()) as i16;
        }
        f
    }

    /// The failure `frame` exists for: a long quiet stretch lets the RMS
    /// loop's gain climb, then a frame arrives ~10x hotter. Sample-by-sample
    /// leveling multiplies its first 20–40 ms by the stale gain and clips
    /// hard; frame leveling sees the peak coming and caps the gain instead.
    #[test]
    fn a_hot_frame_after_quiet_audio_is_limited_not_clipped() {
        let mut agc = AudioAgc::with_target(0.015);
        for k in 0..400 {
            let mut f = sine_frame(600.0, 3.0, k);
            agc.frame(&mut f);
        }
        assert!(
            agc.gain > 3.0,
            "gain should have climbed on quiet audio: {}",
            agc.gain
        );
        let mut hot = sine_frame(12_000.0, 3.0, 400);
        agc.frame(&mut hot);
        let peak = hot.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!(
            peak as f32 <= PEAK_CEILING * 32_767.0 + 1.0,
            "hot frame should be held under the ceiling, peaked at {peak}"
        );
        assert!(
            peak > 20_000,
            "the frame should still be loud, not squashed: {peak}"
        );
    }

    /// The limiter is a ceiling, never a change of level: on audio the RMS
    /// loop already keeps under `PEAK_CEILING`, `frame` must produce exactly
    /// what `sample` did.
    #[test]
    fn frame_leveling_matches_sample_leveling_when_nothing_peaks() {
        let mut by_sample = AudioAgc::with_target(0.015);
        let mut by_frame = AudioAgc::with_target(0.015);
        for k in 0..300 {
            let input = sine_frame(4_000.0, 5.0, k);
            let expect: Vec<i16> = input
                .iter()
                .map(|&s| by_sample.sample(s as f32 / 32_768.0))
                .collect();
            let mut got = input;
            by_frame.frame(&mut got);
            assert_eq!(
                got.to_vec(),
                expect,
                "frame {k} diverged from sample-wise leveling"
            );
        }
    }

    /// After a hot frame the ceiling releases gradually (~1 dB a frame), not
    /// in one jump back to the RMS gain — a jump would pump audibly.
    #[test]
    fn the_peak_ceiling_releases_gradually() {
        let mut agc = AudioAgc::with_target(0.015);
        for k in 0..400 {
            agc.frame(&mut sine_frame(600.0, 3.0, k));
        }
        let gain_before_hot = agc.gain;
        agc.frame(&mut sine_frame(12_000.0, 3.0, 400));
        let after_hot = agc.limit;
        assert!(
            after_hot < gain_before_hot,
            "the hot frame should have lowered the ceiling below the stale gain ({gain_before_hot}): {after_hot}"
        );
        agc.frame(&mut sine_frame(600.0, 3.0, 401));
        let ratio = agc.limit / after_hot;
        assert!(
            (ratio - LIMIT_RELEASE).abs() < 1e-3,
            "ceiling should release by LIMIT_RELEASE per frame, got x{ratio}"
        );
    }
}
