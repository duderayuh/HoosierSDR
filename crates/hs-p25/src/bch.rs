//! Binary BCH(63,16) encoder/decoder for the P25 Network ID word.
//!
//! Implemented from Lin & Costello, *Error Control Coding*: GF(2^6) with
//! primitive polynomial x^6 + x + 1, generator = LCM of minimal polynomials
//! of α^1..α^22 (designed distance 23, corrects up to 11 bit errors),
//! Berlekamp–Massey + Chien search decoding. Protocol fact per TIA-102.BAAA:
//! the NID is the 16 information bits (NAC 12 + DUID 4).

const M: usize = 6;
const N: usize = 63;
const K: usize = 16;
const T: usize = 11;

/// GF(64) log/antilog tables, primitive poly x^6+x+1 (0b1000011).
struct Gf {
    exp: [u8; 128],
    log: [u8; 64],
}

impl Gf {
    fn new() -> Self {
        let mut exp = [0u8; 128];
        let mut log = [0u8; 64];
        let mut x = 1u8;
        for i in 0..63 {
            exp[i] = x;
            log[x as usize] = i as u8;
            x <<= 1;
            if x & 0x40 != 0 {
                x = (x & 0x3F) ^ 0x03; // reduce by x^6 = x + 1
            }
        }
        for i in 63..128 {
            exp[i] = exp[i - 63];
        }
        Self { exp, log }
    }

    fn mul(&self, a: u8, b: u8) -> u8 {
        if a == 0 || b == 0 {
            0
        } else {
            self.exp[(self.log[a as usize] as usize + self.log[b as usize] as usize) % 63]
        }
    }

    fn inv(&self, a: u8) -> u8 {
        debug_assert!(a != 0);
        self.exp[(63 - self.log[a as usize] as usize) % 63]
    }
}

/// Generator polynomial bits (degree 47), computed once from the minimal
/// polynomials of α^1..α^22.
fn generator(gf: &Gf) -> u64 {
    // Collect distinct minimal polynomials via cyclotomic cosets of 1..22.
    let mut covered = [false; 63];
    let mut g: u128 = 1; // polynomial, bit i = coefficient of x^i
    for i in 1..=(2 * T) {
        if covered[i] {
            continue;
        }
        // Cyclotomic coset {i, 2i, 4i, ...} mod 63.
        let mut coset = Vec::new();
        let mut j = i;
        loop {
            if !covered[j] {
                covered[j] = true;
                coset.push(j);
            }
            j = (j * 2) % 63;
            if j == i {
                break;
            }
        }
        // Minimal polynomial = Π (x - α^j) over the coset, computed in GF(64)
        // then coefficients (0/1) folded into g.
        let mut mp: Vec<u8> = vec![1]; // coefficients, mp[k] of x^k, GF(64)
        for &r in &coset {
            let root = gf.exp[r];
            let mut next = vec![0u8; mp.len() + 1];
            for (k, &c) in mp.iter().enumerate() {
                next[k + 1] ^= c;
                next[k] ^= gf.mul(c, root);
            }
            mp = next;
        }
        // mp now has binary coefficients; multiply into g over GF(2).
        let mut ng: u128 = 0;
        for (k, &c) in mp.iter().enumerate() {
            debug_assert!(c <= 1);
            if c == 1 {
                ng ^= g << k;
            }
        }
        g = ng;
    }
    debug_assert_eq!(128 - g.leading_zeros() as usize - 1, N - K);
    g as u64
}

pub struct Bch6316 {
    gf: Gf,
    gen: u64,
}

impl Default for Bch6316 {
    fn default() -> Self {
        Self::new()
    }
}

impl Bch6316 {
    pub fn new() -> Self {
        let gf = Gf::new();
        let gen = generator(&gf);
        Self { gf, gen }
    }

    /// Encode 16 data bits into a 63-bit systematic codeword
    /// (bit 62 = first data bit, bit 0 = last parity bit).
    pub fn encode(&self, data: u16) -> u64 {
        let mut rem: u64 = (data as u64) << (N - K); // data * x^47
        for i in (N - K..N).rev() {
            if rem >> i & 1 == 1 {
                rem ^= self.gen << (i - (N - K));
            }
        }
        ((data as u64) << (N - K)) | rem
    }

    /// Decode a 63-bit received word; returns (data, bit_errors_corrected)
    /// or None if uncorrectable.
    pub fn decode(&self, mut rx: u64) -> Option<(u16, u32)> {
        rx &= (1u64 << N) - 1;
        // Syndromes S1..S22: S_j = r(α^j).
        let mut synd = [0u8; 2 * T + 1];
        let mut all_zero = true;
        for (j, s) in synd.iter_mut().enumerate().skip(1) {
            let mut acc = 0u8;
            for pos in 0..N {
                if rx >> (N - 1 - pos) & 1 == 1 {
                    // bit at descending power: exponent (N-1-pos)*j
                    acc ^= self.gf.exp[((N - 1 - pos) * j) % 63];
                }
            }
            *s = acc;
            if acc != 0 {
                all_zero = false;
            }
        }
        if all_zero {
            return Some(((rx >> (N - K)) as u16, 0));
        }
        // Berlekamp–Massey for the error locator polynomial.
        let mut c = vec![0u8; N]; // current locator
        let mut b = vec![0u8; N];
        c[0] = 1;
        b[0] = 1;
        let mut l = 0usize;
        let mut m = 1usize;
        let mut bb = 1u8;
        for n in 1..=(2 * T) {
            let mut d = synd[n];
            for i in 1..=l {
                d ^= self.gf.mul(c[i], synd[n - i]);
            }
            if d == 0 {
                m += 1;
            } else if 2 * l <= n - 1 {
                let t = c.clone();
                let coef = self.gf.mul(d, self.gf.inv(bb));
                for i in 0..N - m {
                    c[i + m] ^= self.gf.mul(coef, b[i]);
                }
                l = n - l;
                b = t;
                bb = d;
                m = 1;
            } else {
                let coef = self.gf.mul(d, self.gf.inv(bb));
                for i in 0..N - m {
                    c[i + m] ^= self.gf.mul(coef, b[i]);
                }
                m += 1;
            }
        }
        if l > T {
            return None;
        }
        // Chien search: roots α^{-i} ↔ error at position i (power N-1-pos).
        let mut errors = 0u32;
        let mut corrected = rx;
        for i in 0..N {
            let mut acc = 0u8;
            for (j, &cj) in c.iter().enumerate().take(l + 1) {
                acc ^= if cj == 0 {
                    0
                } else {
                    self.gf.exp[(self.gf.log[cj as usize] as usize + i * j) % 63]
                };
            }
            if acc == 0 {
                // Root at α^i → error locator X = α^{-i} → error power = 63-i.
                let power = (63 - i) % 63;
                corrected ^= 1u64 << power;
                errors += 1;
            }
        }
        if errors as usize != l {
            return None;
        }
        // Verify: recompute one syndrome.
        let mut acc = 0u8;
        for pos in 0..N {
            if corrected >> (N - 1 - pos) & 1 == 1 {
                acc ^= self.gf.exp[(N - 1 - pos) % 63];
            }
        }
        if acc != 0 {
            return None;
        }
        Some(((corrected >> (N - K)) as u16, errors))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_error_correction() {
        let bch = Bch6316::new();
        for data in [0x0000u16, 0xFFFF, 0x293F, 0x1234, 0xA5A5] {
            let cw = bch.encode(data);
            assert_eq!(bch.decode(cw), Some((data, 0)));
            // Flip up to 11 bits — must still decode.
            let mut corrupted = cw;
            for k in 0..11 {
                corrupted ^= 1u64 << ((k * 5 + 3) % 63);
            }
            let (d, e) = bch.decode(corrupted).expect("11 errors correctable");
            assert_eq!(d, data);
            assert_eq!(e, 11);
        }
    }
}
