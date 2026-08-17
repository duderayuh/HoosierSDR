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
