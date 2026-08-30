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
///
/// The outer-level estimate is gated — only symbols that scale beyond ±2 count
/// as outer ones — and that gate divides by the very estimate it updates. On
/// its own that deadlocks: if the estimate is ever driven too high, every
/// subsequent symbol scales below the gate, nothing updates it, and it stays
/// wrong forever. A traffic channel makes this routine rather than theoretical,
/// because it sits idle until a call arrives and the receiver spends that time
/// adapting to noise; a reference capture had fourteen seconds of it, and
/// the whole transmission that followed decoded as nothing.
///
/// So a second, ungated estimate of mean magnitude runs alongside and the outer
/// estimate leaks slowly toward what that mean implies. The gated path still
/// does the fine work when the eye is open; the leak only guarantees the
/// estimate can always find its way back.
pub struct SymbolScaler {
    avg_outer: f32,
    /// Ungated mean of |sym|, which no gate can lock out.
    mean_abs: f32,
}

impl Default for SymbolScaler {
    fn default() -> Self {
        Self {
            avg_outer: 3.0,
            mean_abs: 2.0,
        }
    }
}

impl SymbolScaler {
    pub fn scale(&mut self, sym: f32) -> f32 {
        let a = sym.abs();
        // Ungated: always follows the signal, so recovery is always possible.
        self.mean_abs += 0.001 * (a - self.mean_abs);

        // Only outer symbols (|s|>2 after scaling) update the estimate.
        let s = sym * 3.0 / self.avg_outer.max(1e-6);
        if s.abs() > 2.0 {
            self.avg_outer = 0.995 * self.avg_outer + 0.005 * a;
        }

        // Leak toward the outer level the ungated mean implies. For equally
        // likely 4-level symbols at ±1/±3 the mean magnitude is 2 and the outer
        // level is 3, so the outer level is 1.5× the mean. Slow enough not to
        // disturb a locked receiver, fast enough to break a deadlock.
        let implied = (self.mean_abs * 1.5).max(1e-6);
        self.avg_outer += 0.0005 * (implied - self.avg_outer);
        s
    }
}

#[cfg(test)]
mod scaler_tests {
    use super::*;

    /// Feed noise, then a real four-level signal, and require the scaler to
    /// recover. This is the traffic-channel case: idle until a call arrives.
    #[test]
    fn recovers_after_adapting_to_noise() {
        let mut sc = SymbolScaler::default();
        let mut seed = 0x1234_5678u64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            ((seed >> 40) as f32 / (1u64 << 23) as f32) - 1.0
        };
        // Idle: loud noise, which drives the gated estimate away.
        for _ in 0..20_000 {
            sc.scale(rnd() * 12.0);
        }
        // Signal arrives at a completely different level.
        let levels = [1.0f32, 3.0, -1.0, -3.0];
        let mut out = Vec::new();
        for i in 0..20_000 {
            let v = sc.scale(levels[i % 4] * 0.4);
            if i >= 19_000 {
                out.push(v);
            }
        }
        // Outer symbols must land near ±3 again, or the slicer sees nothing.
        let outer: Vec<f32> = out.iter().copied().filter(|v| v.abs() > 2.0).collect();
        assert!(
            !outer.is_empty(),
            "scaler never recovered: no symbol reached the outer level"
        );
        let mean: f32 = outer.iter().map(|v| v.abs()).sum::<f32>() / outer.len() as f32;
        assert!(
            (mean - 3.0).abs() < 1.0,
            "outer level settled at {mean:.2}, expected near 3"
        );
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
