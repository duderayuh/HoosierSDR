//! Reed–Solomon codes over GF(64) as P25 uses them for the Link Control and
//! Encryption Sync fields.
//!
//! Implemented from Lin & Costello, *Error Control Coding* (Berlekamp–Massey
//! locator synthesis, Chien search, Forney magnitudes). Protocol facts per
//! TIA-102.BAAA: the field is GF(2^6) with primitive polynomial x^6 + x + 1,
//! the generator polynomial is g(x) = Π (x + α^i) for i = 1..2t, and the
//! codes are shortened from the (63, 63−2t) parent: coefficient positions 0
//! to 2t−1 hold parity, positions 2t.. hold the data hexbits, and the
//! positions above the shortened length are implicitly zero.

/// GF(64) arithmetic via log/antilog tables, primitive polynomial x^6+x+1.
struct Gf {
    exp: [u8; 128],
    log: [u8; 64],
}

impl Gf {
    fn new() -> Self {
        let mut exp = [0u8; 128];
        let mut log = [0u8; 64];
        let mut x = 1u8;
        for (i, e) in exp.iter_mut().take(63).enumerate() {
            *e = x;
            log[x as usize] = i as u8;
            x <<= 1;
            if x & 0x40 != 0 {
                x = (x & 0x3F) ^ 0x03;
            }
        }
        for i in 63..128 {
            exp[i] = exp[i - 63];
        }
        Self { exp, log }
    }

    #[inline]
    fn mul(&self, a: u8, b: u8) -> u8 {
        if a == 0 || b == 0 {
            0
        } else {
            self.exp[self.log[a as usize] as usize + self.log[b as usize] as usize]
        }
    }

    #[inline]
    fn inv(&self, a: u8) -> u8 {
        debug_assert!(a != 0);
        self.exp[(63 - self.log[a as usize] as usize) % 63]
    }

    /// α^k for any integer k.
    #[inline]
    fn alpha(&self, k: i32) -> u8 {
        self.exp[k.rem_euclid(63) as usize]
    }
}

/// A shortened RS(n, n−2t) code over GF(64).
pub struct ReedSolomon {
    gf: Gf,
    /// Shortened codeword length in hexbits.
    n: usize,
    /// Parity symbols (2t).
    parity: usize,
    /// Generator polynomial coefficients, low degree first, `parity + 1` long.
    gen: Vec<u8>,
}

/// Outcome of a decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RsResult {
    /// The word was (or was corrected to) a codeword; this many symbols
    /// were changed.
    Corrected(usize),
    /// More errors than the code can repair.
    Uncorrectable,
}

impl ReedSolomon {
    /// The (24, 16, 9) code protecting the LDU2 Encryption Sync field:
    /// corrects up to 4 hexbit errors.
    pub fn new_24_16() -> Self {
        Self::new(24, 8)
    }

    /// The (24, 12, 13) code protecting the LDU1 Link Control field:
    /// corrects up to 6 hexbit errors.
    pub fn new_24_12() -> Self {
        Self::new(24, 12)
    }

    fn new(n: usize, parity: usize) -> Self {
        assert!(n <= 63 && parity < n);
        let gf = Gf::new();
        // g(x) = Π_{i=1}^{parity} (x + α^i), built up one root at a time.
        let mut gen = vec![1u8];
        for i in 1..=parity as i32 {
            let root = gf.alpha(i);
            let mut next = vec![0u8; gen.len() + 1];
            for (k, &c) in gen.iter().enumerate() {
                // (x + root) · c x^k = c x^{k+1} + root·c x^k
                next[k + 1] ^= c;
                next[k] ^= gf.mul(root, c);
            }
            gen = next;
        }
        Self { gf, n, parity, gen }
    }

    /// Shortened codeword length in hexbits.
    pub fn codeword_len(&self) -> usize {
        self.n
    }

    /// Number of parity hexbits (2t).
    pub fn parity_len(&self) -> usize {
        self.parity
    }

    /// Systematic encode: `word[parity..n]` holds the data hexbits on entry;
    /// on return `word[0..parity]` holds the parity. (Used by tests and the
    /// synthesizer — the receiver only decodes.)
    pub fn encode(&self, word: &mut [u8]) {
        assert_eq!(word.len(), self.n);
        // Remainder of m(x)·x^parity divided by g(x), by long division from
        // the top coefficient down.
        let mut rem = vec![0u8; self.parity];
        for j in (self.parity..self.n).rev() {
            let feedback = word[j] ^ rem[self.parity - 1];
            for k in (1..self.parity).rev() {
                rem[k] = rem[k - 1] ^ self.gf.mul(feedback, self.gen[k]);
            }
            rem[0] = self.gf.mul(feedback, self.gen[0]);
        }
        word[..self.parity].copy_from_slice(&rem);
    }

    /// Correct `word` in place. Position `i` of `word` is the coefficient of
    /// x^i, so the parity hexbits come first.
    pub fn decode(&self, word: &mut [u8]) -> RsResult {
        assert_eq!(word.len(), self.n);
        let gf = &self.gf;
        let t2 = self.parity;

        // Syndromes S_i = r(α^i), i = 1..2t, stored at s[i-1].
        let mut s = vec![0u8; t2];
        let mut all_zero = true;
        for (i, si) in s.iter_mut().enumerate() {
            let a = gf.alpha(i as i32 + 1);
            let mut acc = 0u8;
            // Horner from the top coefficient.
            for &c in word.iter().rev() {
                acc = gf.mul(acc, a) ^ c;
            }
            *si = acc;
            all_zero &= acc == 0;
        }
        if all_zero {
            return RsResult::Corrected(0);
        }

        // Berlekamp–Massey: error-locator Λ(x), low degree first.
        let mut lambda = vec![0u8; t2 + 1];
        let mut b = vec![0u8; t2 + 1];
        lambda[0] = 1;
        b[0] = 1;
        let mut l = 0usize;
        let mut m = 1usize;
        let mut bb = 1u8;
        for n in 0..t2 {
            // Discrepancy d = S_{n+1} + Σ_{i=1}^{L} Λ_i S_{n+1-i}.
            let mut d = s[n];
            for i in 1..=l {
                d ^= gf.mul(lambda[i], s[n - i]);
            }
            if d == 0 {
                m += 1;
                continue;
            }
            let coef = gf.mul(d, gf.inv(bb));
            if 2 * l <= n {
                let old = lambda.clone();
                for k in 0..=t2 {
                    if k >= m {
                        lambda[k] ^= gf.mul(coef, b[k - m]);
                    }
                }
                l = n + 1 - l;
                b = old;
                bb = d;
                m = 1;
            } else {
                for k in 0..=t2 {
                    if k >= m {
                        lambda[k] ^= gf.mul(coef, b[k - m]);
                    }
                }
                m += 1;
            }
        }
        if l > t2 / 2 {
            return RsResult::Uncorrectable;
        }

        // Chien search over the parent code's positions: error at position j
        // when Λ(α^{-j}) = 0. A root beyond the shortened length is a
        // position that was never transmitted, so the word is uncorrectable.
        let mut positions = Vec::with_capacity(l);
        for j in 0..63i32 {
            let x = gf.alpha(-j);
            let mut acc = 0u8;
            for &c in lambda[..=l].iter().rev() {
                acc = gf.mul(acc, x) ^ c;
            }
            if acc == 0 {
                if j as usize >= self.n {
                    return RsResult::Uncorrectable;
                }
                positions.push(j as usize);
            }
        }
        if positions.len() != l {
            return RsResult::Uncorrectable;
        }

        // Forney: Ω(x) = S(x)·Λ(x) mod x^{2t} with S(x) = Σ S_{i+1} x^i, and
        // for first root α^1 the magnitude is e_j = Ω(X_j^{-1}) / Λ'(X_j^{-1}).
        let mut omega = vec![0u8; t2];
        for i in 0..t2 {
            for k in 0..=l {
                if i + k < t2 {
                    omega[i + k] ^= gf.mul(s[i], lambda[k]);
                }
            }
        }
        for &j in &positions {
            let xinv = gf.alpha(-(j as i32));
            let mut num = 0u8;
            for &c in omega.iter().rev() {
                num = gf.mul(num, xinv) ^ c;
            }
            // Formal derivative in characteristic 2: only odd-power terms.
            let mut den = 0u8;
            let mut pow = 1u8;
            for (i, &li) in lambda.iter().enumerate().take(l + 1).skip(1) {
                if i % 2 == 1 {
                    den ^= gf.mul(li, pow);
                }
                pow = gf.mul(pow, xinv);
            }
            if den == 0 {
                return RsResult::Uncorrectable;
            }
            word[j] ^= gf.mul(num, gf.inv(den));
        }

        // The corrected word must be a codeword; anything else was a
        // miscorrection of a pattern beyond the code's radius.
        for i in 0..t2 {
            let a = gf.alpha(i as i32 + 1);
            let mut acc = 0u8;
            for &c in word.iter().rev() {
                acc = gf.mul(acc, a) ^ c;
            }
            if acc != 0 {
                return RsResult::Uncorrectable;
            }
        }
        RsResult::Corrected(positions.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg(seed: &mut u64) -> u64 {
        *seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *seed >> 33
    }

    #[test]
    fn generator_polynomial_has_the_expected_degree_and_roots() {
        let rs = ReedSolomon::new_24_16();
        assert_eq!(rs.gen.len(), 9);
        assert_eq!(rs.gen[8], 1, "monic");
        // Every α^i for i = 1..8 is a root of g.
        for i in 1..=8 {
            let a = rs.gf.alpha(i);
            let mut acc = 0u8;
            for &c in rs.gen.iter().rev() {
                acc = rs.gf.mul(acc, a) ^ c;
            }
            assert_eq!(acc, 0, "α^{i} is not a root");
        }
    }

    #[test]
    fn encoded_words_have_zero_syndromes_and_decode_untouched() {
        let rs = ReedSolomon::new_24_16();
        let mut seed = 7u64;
        for _ in 0..200 {
            let mut w = [0u8; 24];
            for x in w.iter_mut().skip(8) {
                *x = (lcg(&mut seed) & 0x3F) as u8;
            }
            rs.encode(&mut w);
            let mut copy = w;
            assert_eq!(rs.decode(&mut copy), RsResult::Corrected(0));
            assert_eq!(copy, w);
        }
    }

    #[test]
    fn corrects_up_to_four_symbol_errors_anywhere() {
        let rs = ReedSolomon::new_24_16();
        let mut seed = 99u64;
        for trial in 0..500 {
            let mut w = [0u8; 24];
            for x in w.iter_mut().skip(8) {
                *x = (lcg(&mut seed) & 0x3F) as u8;
            }
            rs.encode(&mut w);
            let errs = (trial % 4) + 1;
            let mut rx = w;
            let mut hit = Vec::new();
            while hit.len() < errs {
                let p = (lcg(&mut seed) % 24) as usize;
                if hit.contains(&p) {
                    continue;
                }
                hit.push(p);
                let e = 1 + (lcg(&mut seed) % 63) as u8;
                rx[p] ^= e;
            }
            let res = rs.decode(&mut rx);
            assert_eq!(res, RsResult::Corrected(errs), "trial {trial}: {res:?}");
            assert_eq!(rx, w, "trial {trial} data not recovered");
        }
    }

    #[test]
    fn refuses_words_beyond_its_radius_rather_than_guessing() {
        // Five errors exceed t = 4. A decoder may occasionally land on
        // another codeword, but it must never return a non-codeword as
        // "corrected" — and with random errors it should refuse most.
        let rs = ReedSolomon::new_24_16();
        let mut seed = 5u64;
        let mut refused = 0;
        let trials = 300;
        for _ in 0..trials {
            let mut w = [0u8; 24];
            for x in w.iter_mut().skip(8) {
                *x = (lcg(&mut seed) & 0x3F) as u8;
            }
            rs.encode(&mut w);
            let mut rx = w;
            let mut hit = Vec::new();
            while hit.len() < 6 {
                let p = (lcg(&mut seed) % 24) as usize;
                if hit.contains(&p) {
                    continue;
                }
                hit.push(p);
                rx[p] ^= 1 + (lcg(&mut seed) % 63) as u8;
            }
            match rs.decode(&mut rx) {
                RsResult::Uncorrectable => refused += 1,
                RsResult::Corrected(_) => {
                    // Whatever it returned must at least be a codeword.
                    let mut check = rx;
                    assert_eq!(rs.decode(&mut check), RsResult::Corrected(0));
                }
            }
        }
        assert!(
            refused > trials * 9 / 10,
            "only {refused}/{trials} six-error words were refused"
        );
    }

    #[test]
    fn the_link_control_code_corrects_six() {
        let rs = ReedSolomon::new_24_12();
        let mut w = [0u8; 24];
        for (i, x) in w.iter_mut().enumerate().skip(12) {
            *x = (i * 5 % 64) as u8;
        }
        rs.encode(&mut w);
        let mut rx = w;
        for p in [0usize, 3, 9, 13, 17, 23] {
            rx[p] ^= 0x2A;
        }
        assert_eq!(rs.decode(&mut rx), RsResult::Corrected(6));
        assert_eq!(rx, w);
    }
}
