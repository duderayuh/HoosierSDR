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
}

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
        }
    }

    /// Normalize one audio sample and quantize to i16.
    pub fn sample(&mut self, x: f32) -> i16 {
        self.power += self.alpha * (x * x - self.power);
        if self.power > 1e-9 {
            self.gain = (self.target / self.power).sqrt().clamp(1e-3, 1e4);
        }
        let y = (x * self.gain).clamp(-1.0, 1.0);
        (y * 32_767.0) as i16
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
        let clipped = first_20.iter().filter(|&&s| s.unsigned_abs() >= 32_767).count();
        assert_eq!(
            clipped, 0,
            "the first 20 samples of a loud onset should not clip: {first_20:?}"
        );
    }
}
