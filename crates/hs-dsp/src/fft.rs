//! A small iterative radix-2 FFT, and an averaged power spectrum built on it.
//!
//! Added for one concrete reason: scanning a wideband capture by decoding every
//! channel position is far too slow. At a 2.4 MHz capture rate there are ~950
//! channel positions, and decoding each one costs a 495-tap channel filter over
//! four seconds of samples — about 180 G multiply-accumulates for a single
//! file. But most of a public-safety band is empty air at any instant, and a
//! spectrum tells us that in one pass over the data. Screening first, then
//! decoding only where there is actually a signal, turns an intractable sweep
//! into a quick one without weakening the result: the decoder still has the
//! final say on every candidate, the spectrum just decides who gets asked.
//!
//! Hand-rolled to keep the crate dependency-free, in the same spirit as the
//! CSV and XML readers.

use crate::C32;

/// In-place forward FFT. `buf.len()` must be a power of two.
pub fn fft(buf: &mut [C32]) {
    let n = buf.len();
    assert!(n.is_power_of_two(), "FFT length must be a power of two");
    if n < 2 {
        return;
    }

    // Bit-reversal permutation.
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            buf.swap(i, j);
        }
    }

    // Danielson–Lanczos butterflies.
    let mut len = 2usize;
    while len <= n {
        let ang = -2.0 * core::f32::consts::PI / len as f32;
        let (ws, wc) = (ang.sin(), ang.cos());
        let wlen = C32::new(wc, ws);
        for start in (0..n).step_by(len) {
            let mut w = C32::new(1.0, 0.0);
            for k in 0..len / 2 {
                let u = buf[start + k];
                let v = buf[start + k + len / 2] * w;
                buf[start + k] = u + v;
                buf[start + k + len / 2] = u - v;
                w = w * wlen;
            }
        }
        len <<= 1;
    }
}

/// Averaged periodogram of an interleaved-IQ buffer, in dB, ordered from the
/// most negative frequency to the most positive (i.e. already fft-shifted).
///
/// `size` is the FFT length and sets the frequency resolution: bin width is
/// `sample_rate / size`.
pub fn power_spectrum_db(iq: &[f32], size: usize) -> Vec<f32> {
    assert!(size.is_power_of_two());
    let mut acc = vec![0.0f64; size];
    let mut segments = 0u32;

    // Hann window, to stop a strong carrier smearing across the whole span and
    // masking the weak channel next to it.
    let win: Vec<f32> = (0..size)
        .map(|i| {
            let x = 2.0 * core::f32::consts::PI * i as f32 / size as f32;
            0.5 - 0.5 * x.cos()
        })
        .collect();

    let mut buf = vec![C32::ZERO; size];
    let total_pairs = iq.len() / 2;
    let mut off = 0usize;
    while off + size <= total_pairs {
        for k in 0..size {
            let i = (off + k) * 2;
            buf[k] = C32::new(iq[i] * win[k], iq[i + 1] * win[k]);
        }
        fft(&mut buf);
        for (a, b) in acc.iter_mut().zip(buf.iter()) {
            *a += b.norm_sq() as f64;
        }
        segments += 1;
        // Hop by a whole segment; overlapping buys smoothness we do not need.
        off += size;
    }

    if segments == 0 {
        return vec![-200.0; size];
    }
    let scale = 1.0 / segments as f64;
    // fft-shift while converting: bin i of the FFT holds frequency
    // i/size for i < size/2, and (i-size)/size above that.
    let mut out = vec![0.0f32; size];
    for (i, o) in out.iter_mut().enumerate() {
        let src = (i + size / 2) % size;
        let p = acc[src] * scale;
        *o = 10.0 * (p.max(1e-30)).log10() as f32;
    }
    out
}

/// Median of a slice, used as a robust noise-floor estimate: a band that is
/// mostly empty has a median sitting in the noise, and a handful of strong
/// carriers cannot drag it up the way a mean would.
pub fn median(values: &[f32]) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v: Vec<f32> = values.iter().copied().filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
    v[v.len() / 2]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transforms_a_tone_into_a_single_bin() {
        let n = 64usize;
        let bin = 7usize;
        let mut buf: Vec<C32> = (0..n)
            .map(|i| {
                let p = 2.0 * core::f32::consts::PI * bin as f32 * i as f32 / n as f32;
                C32::new(p.cos(), p.sin())
            })
            .collect();
        fft(&mut buf);
        let peak = (0..n)
            .max_by(|&a, &b| buf[a].norm_sq().partial_cmp(&buf[b].norm_sq()).unwrap())
            .unwrap();
        assert_eq!(peak, bin);
        // Energy concentrates: the peak dwarfs everything else.
        let others: f32 = (0..n).filter(|&i| i != bin).map(|i| buf[i].norm_sq()).sum();
        assert!(buf[bin].norm_sq() > 100.0 * others, "leakage too high");
    }

    #[test]
    fn spectrum_locates_an_offset_carrier() {
        // A tone a quarter of the way up the band must land a quarter of the
        // way up the (fft-shifted) spectrum, i.e. 3/4 across the array.
        let size = 256usize;
        let rate = 240_000.0f32;
        let tone = 60_000.0f32; // +quarter of the sample rate
        let mut iq = Vec::new();
        for i in 0..size * 8 {
            let p = 2.0 * core::f32::consts::PI * tone * i as f32 / rate;
            iq.push(p.cos());
            iq.push(p.sin());
        }
        let psd = power_spectrum_db(&iq, size);
        let peak = (0..size)
            .max_by(|&a, &b| psd[a].partial_cmp(&psd[b]).unwrap())
            .unwrap();
        // Bin index → frequency: (peak - size/2) * rate / size.
        let freq = (peak as f32 - size as f32 / 2.0) * rate / size as f32;
        assert!((freq - tone).abs() < rate / size as f32, "found {freq} Hz");
    }

    #[test]
    fn median_ignores_a_few_strong_outliers() {
        let mut v = vec![1.0f32; 100];
        for x in v.iter_mut().take(5) {
            *x = 1000.0;
        }
        assert_eq!(median(&v), 1.0);
    }
}

/// A reusable transform for one length: twiddle factors computed once,
/// scratch allocated once.
///
/// [`fft_any`] is fine for a one-off, but the channelizer runs the same
/// transform over and over — 120 times a second at 2.4 MHz — and doing it the
/// straightforward way is what made trunk-following slower than real time.
/// Two things dominated, and neither is inherent to the algorithm:
///
/// * **Trig in the innermost loop.** Every butterfly called `sin` and `cos`
///   to rebuild a root of unity that the previous block had already computed.
///   The whole set fits in one table of `n` entries — 320 kB at the size the
///   channelizer uses — and every root any recursion level needs is an entry
///   of that one table, because `W_m^k = W_n^{k·n/m}` whenever `m` divides `n`.
/// * **Allocation per recursion node.** Splitting into sub-transforms built a
///   fresh `Vec` for each, thousands of them per block. Writing the
///   sub-transforms into disjoint slices of the output removes every one.
///
/// What is left is the same Cooley–Tukey decomposition, doing arithmetic
/// instead of bookkeeping.
pub struct FftPlan {
    n: usize,
    /// `W_n^j = exp(-2πij/n)` for every `j`, shared by all recursion levels.
    tw: Vec<C32>,
    scratch: Vec<C32>,
}

/// Largest radix the fixed-size butterfly buffer handles. Sizes with a larger
/// smallest prime factor fall back to a direct DFT — correct, just slower, and
/// not a shape any capture rate produces.
const MAX_RADIX: usize = 32;

impl FftPlan {
    pub fn new(n: usize) -> Self {
        let n = n.max(1);
        let tw = (0..n)
            .map(|j| {
                let a = -2.0 * core::f64::consts::PI * j as f64 / n as f64;
                C32::new(a.cos() as f32, a.sin() as f32)
            })
            .collect();
        Self {
            n,
            tw,
            scratch: vec![C32::ZERO; n],
        }
    }

    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// In-place forward transform. `buf.len()` must equal [`FftPlan::len`].
    pub fn forward(&mut self, buf: &mut [C32]) {
        assert_eq!(
            buf.len(),
            self.n,
            "buffer does not match the planned length"
        );
        // The recursion reads its input with a stride while writing the output
        // linearly, so the two cannot be the same buffer.
        self.scratch.copy_from_slice(buf);
        transform(buf, &self.scratch, 1, &self.tw, self.n);
    }

    /// In-place inverse transform, scaled so `inverse(forward(x)) == x`.
    pub fn inverse(&mut self, buf: &mut [C32]) {
        for v in buf.iter_mut() {
            *v = v.conj();
        }
        self.forward(buf);
        let s = 1.0 / self.n as f32;
        for v in buf.iter_mut() {
            *v = v.conj().scale(s);
        }
    }
}

/// Transform `inp` (read with `stride`) into `out`, using roots drawn from the
/// single table `tw` of length `n_total`.
fn transform(out: &mut [C32], inp: &[C32], stride: usize, tw: &[C32], n_total: usize) {
    let n = out.len();
    let r = smallest_factor(n);
    let step = n_total / n;

    if r == n || r > MAX_RADIX {
        // Prime length (or an awkward one): evaluate the DFT directly.
        for (k, o) in out.iter_mut().enumerate() {
            let mut acc = C32::ZERO;
            for j in 0..n {
                acc = acc + inp[j * stride] * tw[(j * k % n) * step];
            }
            *o = acc;
        }
        return;
    }

    let m = n / r;
    for q in 0..r {
        let (lo, hi) = (q * m, (q + 1) * m);
        transform(
            &mut out[lo..hi],
            &inp[q * stride..],
            stride * r,
            tw,
            n_total,
        );
    }

    if r == 2 {
        // The common case, worth not paying the general path for.
        for k in 0..m {
            let a = out[k];
            let b = out[m + k] * tw[k * step];
            out[k] = a + b;
            out[m + k] = a - b;
        }
        return;
    }

    let mut t = [C32::ZERO; MAX_RADIX];
    for k in 0..m {
        // Twiddle each sub-transform's contribution: W_n^{qk}.
        for (q, tq) in t.iter_mut().enumerate().take(r) {
            *tq = out[q * m + k] * tw[(q * k * step) % n_total];
        }
        // Then an r-point DFT across them: W_r^{pq} = W_n_total^{pq·m·step}.
        for p in 0..r {
            let mut acc = C32::ZERO;
            for (q, &tq) in t.iter().enumerate().take(r) {
                acc = acc + tq * tw[(p * q * m * step) % n_total];
            }
            out[p * m + k] = acc;
        }
    }
}

/// Forward FFT for **any** length, not just powers of two.
///
/// A channelizer needs this. Extracting a 48 kHz channel from a 2.4 MHz
/// capture is a decimation by 50, and 50 has a factor of 5 that no power-of-two
/// transform can express: with a radix-2 FFT the only achievable ratios from
/// 2.4 MHz that stay an integer number of samples per symbol are 1, 2 and 4,
/// which is nowhere near enough. Supporting sizes like 4000 = 2⁵·5³ makes the
/// exact rate reachable in one step.
///
/// Recursive mixed-radix Cooley–Tukey: split off the smallest prime factor,
/// transform the interleaved sub-sequences, and combine them with a direct
/// DFT over that factor. Powers of two take the specialized path above.
pub fn fft_any(buf: &mut [C32]) {
    let n = buf.len();
    if n <= 1 {
        return;
    }
    if n.is_power_of_two() {
        fft(buf);
        return;
    }
    let r = smallest_factor(n);
    let m = n / r;

    // Decimation in time: r interleaved sub-sequences of length m.
    let mut sub: Vec<Vec<C32>> = (0..r)
        .map(|j| (0..m).map(|k| buf[k * r + j]).collect())
        .collect();
    for s in sub.iter_mut() {
        fft_any(s);
    }

    let two_pi = 2.0 * core::f32::consts::PI;
    for k in 0..m {
        for q in 0..r {
            let idx = k + q * m;
            let mut acc = C32::ZERO;
            for (j, s) in sub.iter().enumerate() {
                let ang = -two_pi * (idx as f32) * (j as f32) / (n as f32);
                acc = acc + s[k] * C32::new(ang.cos(), ang.sin());
            }
            buf[idx] = acc;
        }
    }
}

/// Inverse FFT for any length, scaled so `ifft_any(fft_any(x)) == x`.
pub fn ifft_any(buf: &mut [C32]) {
    for v in buf.iter_mut() {
        *v = v.conj();
    }
    fft_any(buf);
    let s = 1.0 / buf.len() as f32;
    for v in buf.iter_mut() {
        *v = v.conj().scale(s);
    }
}

fn smallest_factor(n: usize) -> usize {
    let mut d = 2;
    while d * d <= n {
        if n.is_multiple_of(d) {
            return d;
        }
        d += 1;
    }
    n
}

#[cfg(test)]
mod any_tests {
    use super::*;

    fn naive_dft(x: &[C32]) -> Vec<C32> {
        let n = x.len();
        (0..n)
            .map(|k| {
                let mut acc = C32::ZERO;
                for (j, &v) in x.iter().enumerate() {
                    let a = -2.0 * core::f32::consts::PI * (k * j) as f32 / n as f32;
                    acc = acc + v * C32::new(a.cos(), a.sin());
                }
                acc
            })
            .collect()
    }

    #[test]
    fn matches_a_direct_dft_at_awkward_lengths() {
        // 100 = 2²·5², 60 = 2²·3·5, 7 prime: none are powers of two.
        for n in [7usize, 12, 60, 100] {
            let x: Vec<C32> = (0..n)
                .map(|i| C32::new((i as f32 * 0.37).sin(), (i as f32 * 0.11).cos()))
                .collect();
            let want = naive_dft(&x);
            let mut got = x.clone();
            fft_any(&mut got);
            for (i, (a, b)) in got.iter().zip(want.iter()).enumerate() {
                let err = (*a - *b).norm_sq().sqrt();
                assert!(err < 1e-2, "n={n} bin {i}: {a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn plan_agrees_with_the_recursive_transform() {
        // The plan is an optimisation, so the bar is that it changes nothing.
        // Sizes cover a prime, a power of two, the channelizer's output block
        // (800 = 2⁵·5²) and a mixed length.
        for n in [7usize, 12, 64, 100, 800] {
            let x: Vec<C32> = (0..n)
                .map(|i| C32::new((i as f32 * 0.37).sin(), (i as f32 * 0.11).cos()))
                .collect();
            let mut want = x.clone();
            fft_any(&mut want);
            let mut got = x.clone();
            FftPlan::new(n).forward(&mut got);
            for (i, (a, b)) in got.iter().zip(want.iter()).enumerate() {
                assert!(
                    (*a - *b).norm_sq().sqrt() < 1e-2,
                    "n={n} bin {i}: {a:?} vs {b:?}"
                );
            }
        }
    }

    #[test]
    fn plan_round_trips_at_the_channelizer_block_size() {
        // 40000 = 2⁶·5⁴ is the forward transform a 2.4 MHz capture runs 120
        // times a second, and the one the whole optimisation is about.
        let n = 40000usize;
        let x: Vec<C32> = (0..n)
            .map(|i| C32::new((i as f32 * 0.013).cos(), (i as f32 * 0.007).sin()))
            .collect();
        let mut plan = FftPlan::new(n);
        let mut y = x.clone();
        plan.forward(&mut y);
        plan.inverse(&mut y);
        for (a, b) in x.iter().zip(y.iter()) {
            assert!((*a - *b).norm_sq().sqrt() < 1e-3, "{a:?} vs {b:?}");
        }
    }

    #[test]
    fn plan_puts_a_tone_in_one_bin() {
        // A round trip cannot catch an error that the inverse undoes, so pin
        // the forward transform against something known independently.
        let n = 4000usize;
        let bin = 137usize;
        let mut buf: Vec<C32> = (0..n)
            .map(|i| {
                let p = 2.0 * core::f32::consts::PI * bin as f32 * i as f32 / n as f32;
                C32::new(p.cos(), p.sin())
            })
            .collect();
        FftPlan::new(n).forward(&mut buf);
        let peak = (0..n)
            .max_by(|&a, &b| buf[a].norm_sq().partial_cmp(&buf[b].norm_sq()).unwrap())
            .unwrap();
        assert_eq!(peak, bin);
    }

    #[test]
    fn inverse_round_trips_at_the_channelizer_size() {
        // 4000 = 2⁵·5³ is the block size a 2.4 MHz capture uses.
        let n = 4000usize;
        let x: Vec<C32> = (0..n)
            .map(|i| C32::new((i as f32 * 0.013).cos(), (i as f32 * 0.007).sin()))
            .collect();
        let mut y = x.clone();
        fft_any(&mut y);
        ifft_any(&mut y);
        for (a, b) in x.iter().zip(y.iter()) {
            assert!((*a - *b).norm_sq().sqrt() < 1e-3, "{a:?} vs {b:?}");
        }
    }
}
