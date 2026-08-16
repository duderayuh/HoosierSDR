//! Gardner symbol-timing recovery over a real-valued (post-discriminator)
//! sample stream, with linear interpolation.
//!
//! Implemented from Gardner (1986) / Proakis; zero-crossing-based error is
//! usable for 4-level C4FM because level transitions dominate.

pub struct GardnerSync {
    sps: f32,
    mu: f32,    // fractional interpolation offset
    count: f32, // samples until next strobe
    gain: f32,
    prev_sample: f32,
    last_sym: f32,
    mid_sym: f32,
    want_mid: bool,
}

impl GardnerSync {
    pub fn new(samples_per_symbol: f32, loop_gain: f32) -> Self {
        Self {
            sps: samples_per_symbol,
            mu: 0.0,
            count: samples_per_symbol / 2.0,
            gain: loop_gain,
            prev_sample: 0.0,
            last_sym: 0.0,
            mid_sym: 0.0,
            want_mid: true,
        }
    }

    /// Push one input sample; returns Some(symbol_value) at symbol strobes.
    pub fn push(&mut self, x: f32) -> Option<f32> {
        self.count -= 1.0;
        let mut out = None;
        if self.count <= 0.0 {
            // Linear interpolation at fractional offset.
            let frac = -self.count; // how far past the ideal instant we are
            let y = self.prev_sample + (x - self.prev_sample) * (1.0 - frac).clamp(0.0, 1.0);
            if self.want_mid {
                self.mid_sym = y;
                self.count += self.sps / 2.0;
            } else {
                // Gardner TED: e = (y[n] - y[n-1]) * y[n-1/2]
                let e = (y - self.last_sym) * self.mid_sym;
                self.last_sym = y;
                // Positive error → strobe late → shorten next period.
                self.count += self.sps / 2.0 - self.gain * e;
                self.mu = frac;
                let _ = self.mu;
                out = Some(y);
            }
            self.want_mid = !self.want_mid;
        }
        self.prev_sample = x;
        out
    }
}
