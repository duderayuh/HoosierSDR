//! DSP primitives for HoosierSDR: complex baseband types, filters,
//! timing/carrier recovery, and the adaptive equalizer family.
//!
//! Provenance: all algorithms here are implemented from the adaptive-filtering
//! and digital-communications literature (Haykin, *Adaptive Filter Theory*;
//! Proakis, *Digital Communications*) and TIA-102 protocol facts. No code in
//! this crate is derived from GPL-licensed projects. See CONTRIBUTING.md.

pub mod equalizer;

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
