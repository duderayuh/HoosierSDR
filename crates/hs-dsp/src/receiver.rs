//! Complete C4FM receiver: complex baseband in, soft symbols out.
//!
//! Chain: FM discriminator → RRC matched filter → deviation scaling →
//! Gardner timing recovery. Slicing and FSW-trained equalization happen
//! downstream (hs-core) so the equalizer can be trained from frame sync
//! feedback — before the decision device, per the project thesis.

use crate::c4fm::SymbolScaler;
use crate::fir::Fir;
use crate::fm::FmDemod;
use crate::rrc::rrc_taps;
use crate::timing::GardnerSync;
use crate::C32;

pub struct C4fmReceiver {
    fm: FmDemod,
    mf: Fir,
    scaler: SymbolScaler,
    timing: GardnerSync,
    /// rad/sample at max deviation → normalizes discriminator out to ±3.
    freq_scale: f32,
}

impl C4fmReceiver {
    /// `sample_rate` must be an integer multiple of 4800 (e.g. 48000).
    pub fn new(sample_rate: f64) -> Self {
        let sps = sample_rate / crate::P25_SYMBOL_RATE;
        assert!(
            (sps.fract()).abs() < 1e-9 && sps >= 4.0,
            "need integer sps >= 4"
        );
        let max_dev_rad = 2.0 * core::f64::consts::PI * crate::c4fm::DEVIATION_MAX_HZ / sample_rate;
        Self {
            fm: FmDemod::new(),
            mf: Fir::new(rrc_taps(sps as usize, 6, 0.2)),
            scaler: SymbolScaler::default(),
            timing: GardnerSync::new(sps as f32, 0.05),
            freq_scale: (3.0 / max_dev_rad) as f32,
        }
    }

    /// Push one IQ sample; returns a soft symbol (nominal ±1/±3) at strobes.
    pub fn push(&mut self, iq: C32) -> Option<f32> {
        let disc = self.fm.demod(iq) * self.freq_scale;
        let shaped = self.mf.filter(disc);
        self.timing.push(shaped).map(|s| self.scaler.scale(s))
    }
}
