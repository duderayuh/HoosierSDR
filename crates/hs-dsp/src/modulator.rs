//! C4FM modulator — used by hs-bench to synthesize P25 baseband for
//! end-to-end loopback tests, and eventually for corpus augmentation.

use crate::c4fm::{dibit_to_level, Dibit, DEVIATION_MAX_HZ};
use crate::fir::Fir;
use crate::rrc::rrc_taps;
use crate::C32;

pub struct C4fmModulator {
    shaper: Fir,
    sps: usize,
    phase: f64,
    rad_per_unit: f64,
}

impl C4fmModulator {
    pub fn new(sample_rate: f64) -> Self {
        let sps = (sample_rate / crate::P25_SYMBOL_RATE) as usize;
        assert!(sps >= 4);
        // Impulse-train input: scale taps by sps to keep symbol amplitude.
        let taps = rrc_taps(sps, 6, 0.2)
            .into_iter()
            .map(|t| t * sps as f32)
            .collect();
        Self {
            shaper: Fir::new(taps),
            sps,
            phase: 0.0,
            rad_per_unit: 2.0 * core::f64::consts::PI * (DEVIATION_MAX_HZ / 3.0) / sample_rate,
        }
    }

    /// Modulate one dibit into `sps` IQ samples appended to `out`.
    pub fn modulate(&mut self, d: Dibit, out: &mut Vec<C32>) {
        let level = dibit_to_level(d);
        for i in 0..self.sps {
            let x = if i == 0 { level } else { 0.0 };
            let freq = self.shaper.filter(x) as f64 * self.rad_per_unit;
            self.phase += freq;
            out.push(C32::new(self.phase.cos() as f32, self.phase.sin() as f32));
        }
    }
}
