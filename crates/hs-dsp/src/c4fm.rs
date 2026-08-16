//! C4FM symbol mapping and slicing.
//!
//! Protocol facts (TIA-102.BAAA): 4800 baud 4-level FSK, deviations
//! +1800/+600/−600/−1800 Hz map to dibits 01/00/10/11, i.e. symbol values
//! +3/+1/−1/−3.

/// A P25 dibit, 0..=3, in over-the-air bit order (MSB first).
pub type Dibit = u8;

pub const DEVIATION_MAX_HZ: f64 = 1800.0;

/// Dibit → symbol level in {+3,+1,-1,-3}.
pub fn dibit_to_level(d: Dibit) -> f32 {
    match d & 3 {
        0b01 => 3.0,
        0b00 => 1.0,
        0b10 => -1.0,
        _ => -3.0,
    }
}

/// Slice a normalized symbol (nominal levels ±1, ±3) to a dibit.
pub fn slice(sym: f32) -> Dibit {
    if sym > 2.0 {
        0b01
    } else if sym > 0.0 {
        0b00
    } else if sym > -2.0 {
        0b10
    } else {
        0b11
    }
}

/// Track signal deviation so slicing thresholds adapt to level drift.
/// EWMA of |sym| around the outer levels; scales input to nominal ±3.
pub struct SymbolScaler {
    avg_outer: f32,
}

impl Default for SymbolScaler {
    fn default() -> Self {
        Self { avg_outer: 3.0 }
    }
}

impl SymbolScaler {
    pub fn scale(&mut self, sym: f32) -> f32 {
        let a = sym.abs();
        // Only outer symbols (|s|>2 after scaling) update the estimate.
        let s = sym * 3.0 / self.avg_outer;
        if s.abs() > 2.0 {
            self.avg_outer = 0.995 * self.avg_outer + 0.005 * a;
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_levels() {
        for d in 0..4u8 {
            assert_eq!(slice(dibit_to_level(d)), d);
        }
    }
}
