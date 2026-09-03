//! DSP primitives for HoosierSDR: complex baseband types, filters,
//! timing/carrier recovery, and the adaptive equalizer family.
//!
//! Provenance: all algorithms here are implemented from the adaptive-filtering
//! and digital-communications literature (Haykin, *Adaptive Filter Theory*;
//! Proakis, *Digital Communications*) and TIA-102 protocol facts. No code in
//! this crate is derived from GPL-licensed projects. See CONTRIBUTING.md.

pub mod agc;
pub mod c4fm;
pub mod channelizer;
pub mod costas;
pub mod cqpsk;
pub mod decimate;
pub mod diversity;
pub mod equalizer;
pub mod fft;
pub mod fir;
pub mod fm;
pub mod modulator;
pub mod receiver;
pub mod resample;
pub mod rrc;
pub mod timing;
pub mod timing_complex;

/// Complex baseband sample. Deliberately minimal; swap for `num_complex`
/// once external dependencies are introduced.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct C32 {
    pub re: f32,
    pub im: f32,
}

impl C32 {
    pub const ZERO: Self = Self { re: 0.0, im: 0.0 };

    pub fn new(re: f32, im: f32) -> Self {
        Self { re, im }
    }

    pub fn conj(self) -> Self {
        Self::new(self.re, -self.im)
    }

    pub fn norm_sq(self) -> f32 {
        self.re * self.re + self.im * self.im
    }

    pub fn arg(self) -> f32 {
        self.im.atan2(self.re)
    }

    pub fn scale(self, k: f32) -> Self {
        Self::new(self.re * k, self.im * k)
    }
}

impl core::ops::Add for C32 {
    type Output = Self;
    fn add(self, r: Self) -> Self {
        Self::new(self.re + r.re, self.im + r.im)
    }
}

impl core::ops::Sub for C32 {
    type Output = Self;
    fn sub(self, r: Self) -> Self {
        Self::new(self.re - r.re, self.im - r.im)
    }
}

impl core::ops::Mul for C32 {
    type Output = Self;
    fn mul(self, r: Self) -> Self {
        Self::new(
            self.re * r.re - self.im * r.im,
            self.re * r.im + self.im * r.re,
        )
    }
}

/// P25 symbol rate, symbols/second.
pub const P25_SYMBOL_RATE: f64 = 4800.0;

/// P25 channel spacing, Hz — adjacent channels on a site sit this far apart.
pub const P25_CHANNEL_SPACING_HZ: f64 = 12_500.0;

/// Half-width, Hz, to preserve when filtering a single P25 channel out of a
/// wideband capture. Covers both modulations' occupied bandwidth — CQPSK RRC
/// (β=0.2, 4800 baud) needs ±2,880 Hz; C4FM's Carson's-rule bandwidth is
/// wider, ±(1,800 Hz deviation + 2,400 Hz half-symbol-rate) = ±4,200 Hz —
/// with real margin (not just past the theoretical edge) for practical,
/// finite-length pulse shaping, timing jitter and residual tuner error, while
/// still leaving a guard band (750 Hz) before the midpoint to the next
/// channel at half the spacing (±6,250 Hz). A half-width anywhere near that
/// midpoint (the once-used ±8,000 Hz was past it) lets the neighbouring
/// channel straight into the demodulator; cutting all the way down to the
/// ±2,880 Hz CQPSK number with no margin measurably hurt acquisition in
/// practice (see `hs-core/tests/follow_trunk.rs`) — it clips enough of the
/// RRC pulse's own tails to compound with the receiver's matched filter and
/// raise ISI. 6 kHz was the narrowest value that still acquired reliably in
/// that regression.
pub const P25_CHANNEL_HALF_BW_HZ: f64 = 6_000.0;
