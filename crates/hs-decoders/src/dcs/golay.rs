//! Binary Golay(23,12,7) code: 12 data bits, 11 parity bits, corrects up to 3
//! bit errors. Used by the DCS decoder; implemented from the code's standard
//! generator polynomial, not derived from any GPL source.
//!
//! The code is *perfect*: every one of the 2^11 syndromes corresponds to a
//! unique error pattern of weight ≤ 3, so a 2048-entry syndrome table decodes
//! any correctable word in one lookup.

use std::sync::OnceLock;

/// Generator polynomial g(x) = x^11+x^10+x^6+x^5+x^4+x^2+1 (0xC75), degree 11.
const POLY: u32 = 0xC75;
const MASK23: u32 = 0x7F_FFFF;

/// Reduce a 23-bit word modulo the generator, yielding the 11-bit syndrome.
pub fn syndrome(cw: u32) -> u32 {
    let mut rem = cw & MASK23;
    for i in (11..23).rev() {
        if (rem >> i) & 1 == 1 {
            rem ^= POLY << (i - 11);
        }
    }
    rem & 0x7FF
}

/// Systematically encode 12 data bits into a 23-bit codeword: data in the high
/// 12 bits, parity in the low 11.
pub fn encode(data12: u32) -> u32 {
    let data = data12 & 0xFFF;
    let parity = syndrome(data << 11);
    (data << 11) | parity
}

/// Syndrome → minimum-weight error pattern (23-bit), built once.
fn error_table() -> &'static [u32; 2048] {
    static T: OnceLock<[u32; 2048]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [u32::MAX; 2048];
        t[0] = 0;
        // Enumerate error patterns in increasing weight; keep the first (hence
        // minimum-weight) pattern seen for each syndrome.
        for a in 0..23u32 {
            let e = 1 << a;
            let s = syndrome(e) as usize;
            if t[s] == u32::MAX {
                t[s] = e;
            }
            for b in (a + 1)..23 {
                let e = e | (1 << b);
                let s = syndrome(e) as usize;
                if t[s] == u32::MAX {
                    t[s] = e;
                }
                for c in (b + 1)..23 {
                    let e = e | (1 << c);
                    let s = syndrome(e) as usize;
                    if t[s] == u32::MAX {
                        t[s] = e;
                    }
                }
            }
        }
        t
    })
}

/// Decode a 23-bit word, correcting up to 3 errors. Returns the 12 data bits
/// and how many bit errors were corrected.
pub fn decode(cw: u32) -> (u32, u32) {
    let s = syndrome(cw) as usize;
    let e = error_table()[s];
    let corrected = (cw ^ e) & MASK23;
    ((corrected >> 11) & 0xFFF, e.count_ones())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_produces_zero_syndrome() {
        for data in [0u32, 0xFFF, 0xABC, 0x123, 0x800] {
            let cw = encode(data);
            assert_eq!(syndrome(cw), 0, "data {data:#x}");
            let (d, e) = decode(cw);
            assert_eq!(d, data & 0xFFF);
            assert_eq!(e, 0);
        }
    }

    #[test]
    fn corrects_up_to_three_errors() {
        let data = 0xABC;
        let cw = encode(data);
        // Every 1-, 2- and 3-bit error must decode back to the same data.
        for a in 0..23 {
            for b in a..23 {
                for c in b..23 {
                    let err = (1 << a) | (1 << b) | (1 << c);
                    let (d, ne) = decode(cw ^ err);
                    assert_eq!(d, data, "err bits {a},{b},{c}");
                    assert!(ne <= 3);
                }
            }
        }
    }

    #[test]
    fn table_covers_every_syndrome() {
        let t = error_table();
        assert!(t.iter().all(|&e| e != u32::MAX), "a syndrome was uncovered");
    }
}
