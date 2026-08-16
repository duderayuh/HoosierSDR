//! Quadrature FM discriminator.

use crate::C32;

/// Emits instantaneous frequency in radians/sample.
#[derive(Default)]
pub struct FmDemod {
    prev: C32,
}

impl FmDemod {
    pub fn new() -> Self {
        Self {
            prev: C32::new(1.0, 0.0),
        }
    }

    pub fn demod(&mut self, x: C32) -> f32 {
        let d = x * self.prev.conj();
        self.prev = x;
        d.arg()
    }
}
