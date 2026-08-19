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
///
/// The delay line is stored doubled — every sample written at both `pos` and
/// `pos + n` — so the convolution reads a single contiguous window with no
/// per-tap modulo. On a wideband capture this filter is the follower's hottest
/// loop (a ~440-tap dot product per output sample, per channel), and the
/// modulo the ring buffer used to need in that inner loop both cost cycles and
/// blocked the compiler from vectorizing it. Taps are stored reversed so the
/// window and the taps run in the same direction.
pub struct FirC {
    /// Taps in reverse order (oldest-sample-first), for a straight dot product.
    taps_rev: Vec<f32>,
    /// Doubled delay line: index i and i+n always hold the same sample.
    delay: Vec<C32>,
    pos: usize,
    decim: usize,
    phase: usize,
}

impl FirC {
    pub fn new(taps: Vec<f32>, decim: usize) -> Self {
        let n = taps.len();
        assert!(n > 0 && decim > 0);
        let mut taps_rev = taps;
        taps_rev.reverse();
        Self {
            taps_rev,
            delay: vec![C32::ZERO; 2 * n],
            pos: 0,
            decim,
            phase: 0,
        }
    }

    /// Push one sample; returns Some(filtered) on decimation instants.
    pub fn push(&mut self, x: C32) -> Option<C32> {
        let n = self.taps_rev.len();
        self.pos = (self.pos + 1) % n;
        // Write to both halves so a window of length n starting anywhere in
        // [0, n) stays contiguous.
        self.delay[self.pos] = x;
        self.delay[self.pos + n] = x;
        self.phase += 1;
        if self.phase < self.decim {
            return None;
        }
        self.phase = 0;
        // Window holds the last n samples oldest-first: index pos+1 is the
        // oldest, pos+n the newest — the same order as taps_rev.
        let window = &self.delay[self.pos + 1..self.pos + 1 + n];
        let mut acc = C32::ZERO;
        for (s, &t) in window.iter().zip(self.taps_rev.iter()) {
            acc = acc + s.scale(t);
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
