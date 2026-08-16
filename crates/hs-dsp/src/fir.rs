//! FIR filtering for real and complex streams.

use crate::C32;

/// Real-tapped FIR over f32 samples.
pub struct Fir {
    taps: Vec<f32>,
    delay: Vec<f32>,
    pos: usize,
}

impl Fir {
    pub fn new(taps: Vec<f32>) -> Self {
        let n = taps.len();
        assert!(n > 0);
        Self {
            taps,
            delay: vec![0.0; n],
            pos: 0,
        }
    }

    pub fn filter(&mut self, x: f32) -> f32 {
        self.pos = (self.pos + 1) % self.delay.len();
        self.delay[self.pos] = x;
        let n = self.taps.len();
        let mut acc = 0.0f32;
        for k in 0..n {
            acc += self.taps[k] * self.delay[(self.pos + n - k) % n];
        }
        acc
    }
}

/// Real-tapped FIR over complex samples, with integer decimation.
pub struct FirC {
    taps: Vec<f32>,
    delay: Vec<C32>,
    pos: usize,
    decim: usize,
    phase: usize,
}

impl FirC {
    pub fn new(taps: Vec<f32>, decim: usize) -> Self {
        let n = taps.len();
        assert!(n > 0 && decim > 0);
        Self {
            taps,
            delay: vec![C32::ZERO; n],
            pos: 0,
            decim,
            phase: 0,
        }
    }

    /// Push one sample; returns Some(filtered) on decimation instants.
    pub fn push(&mut self, x: C32) -> Option<C32> {
        self.pos = (self.pos + 1) % self.delay.len();
        self.delay[self.pos] = x;
        self.phase += 1;
        if self.phase < self.decim {
            return None;
        }
        self.phase = 0;
        let n = self.taps.len();
        let mut acc = C32::ZERO;
        for k in 0..n {
            let s = self.delay[(self.pos + n - k) % n];
            acc = acc + s.scale(self.taps[k]);
        }
        Some(acc)
    }
}

/// Windowed-sinc lowpass taps (Hamming window). `cutoff` is normalized to
/// sample rate (0..0.5).
pub fn lowpass_taps(num_taps: usize, cutoff: f64) -> Vec<f32> {
    assert!(
        num_taps % 2 == 1,
        "odd tap count keeps linear phase symmetric"
    );
    let m = (num_taps - 1) as f64;
    let mut taps = Vec::with_capacity(num_taps);
    let mut sum = 0.0;
    for i in 0..num_taps {
        let n = i as f64 - m / 2.0;
        let sinc = if n == 0.0 {
            2.0 * cutoff
        } else {
            (2.0 * core::f64::consts::PI * cutoff * n).sin() / (core::f64::consts::PI * n)
        };
        let w = 0.54 - 0.46 * (2.0 * core::f64::consts::PI * i as f64 / m).cos();
        let t = sinc * w;
        sum += t;
        taps.push(t);
    }
    taps.into_iter().map(|t| (t / sum) as f32).collect()
}
