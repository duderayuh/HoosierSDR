//! FFT channelizer: extract many narrow channels from one wideband stream,
//! sharing the expensive work between them.
//!
//! Filtering each channel separately repeats the same job over the same
//! samples. Selecting one 12.5 kHz channel from a 2.4 MHz capture costs a
//! ~500-tap filter over every sample, and a band holds hundreds of channels.
//! An FFT already separates frequencies, so transforming once and then
//! *slicing* the spectrum gives each channel for the price of a small inverse
//! transform.
//!
//! This is what makes trunk-following possible on one radio. A control channel
//! grants calls onto traffic channels scattered across the band; with a
//! channelizer the control channel and every traffic channel inside the
//! capture are decoded **at the same time**, so no call is missed while the
//! receiver retunes — because it never retunes.
//!
//! ## Method
//!
//! Overlap-save with brick-wall bin selection. Each block of `N` input samples
//! is transformed, the `W` bins covering a wanted channel are taken, and those
//! are inverse-transformed to give `W` samples at `sample_rate × W / N` —
//! slicing bins *is* the decimation. Blocks overlap by half and only the
//! middle half of each output block is kept, which discards the edges where
//! the transform's circular wrap-around corrupts the result.
//!
//! An earlier attempt windowed each block and overlap-*added* instead. That is
//! a valid filter-bank structure but not with these sizes: folding `N` bins
//! down to `W` makes the analysis window itself the channel filter, and a Hann
//! window spanning 4000 samples is a lowpass barely a kilohertz wide. It
//! removed almost all of a 12.5 kHz P25 signal while still looking plausible —
//! right frequency band, right rough power — and decoded nothing. Selecting
//! bins from an unwindowed transform gives a brick-wall channel filter
//! instead, which is what the channel actually needs.
//!
//! Sizes are chosen so the output lands exactly on the decoder's working rate:
//! `W = 800` and `N = sample_rate / 60` give 48 kHz out, which is 10 samples
//! per symbol. That needs a transform length like 4000 = 2⁵·5³, hence the
//! mixed-radix [`FftPlan`](crate::fft::FftPlan) — the factor of five is not
//! reachable with a radix-2 transform. The plan is built once and held, not
//! rebuilt per block: at 2.4 MHz this runs a 40000-point transform 120 times a
//! second, and recomputing its twiddle factors every time was the difference
//! between following a system in real time and falling behind the air.

use crate::fft::FftPlan;
use crate::C32;

/// Output samples per symbol, and hence the working rate (4800 × this).
const OUT_SPS: usize = 10;
/// Bins taken per channel, and hence the output block size.
///
/// This is a correctness parameter, not a tuning knob. Selecting bins gives a
/// brick-wall channel filter, whose impulse response is a long sinc, so each
/// output block is corrupted at its edges by the transform's circular
/// wrap-around and only the middle half is usable. The guard that discards is
/// a *fraction* of the block, so it must be long enough in absolute terms to
/// contain the filter's ringing: at 80 bins the guard was 20 samples and
/// nothing decoded, while at 800 it is 200 samples and everything does.
const CHANNEL_BINS: usize = 800;

/// One wideband stream in, many narrowband channels out.
pub struct Channelizer {
    n: usize,
    hop_in: usize,
    /// Output samples kept per block (the middle half).
    keep_out: usize,
    /// Centre bin of each requested channel.
    bins: Vec<isize>,
    /// Actual offset each channel ended up at, after snapping to a bin.
    actual_hz: Vec<f64>,
    /// Unconsumed input.
    pending: Vec<C32>,
    /// Transforms held across blocks: the twiddle tables and scratch are the
    /// same every block, and rebuilding them each time cost more than the
    /// arithmetic did.
    fwd: FftPlan,
    inv: FftPlan,
    out_rate: f64,
    sample_rate: f64,
    /// Per-bin gain across the slice: flat over the channel, raised-cosine
    /// to zero toward the slice edges. A brick-wall slice has a sinc impulse
    /// response far longer than the overlap-save guard, so energy near the
    /// slice edge — the channel two over, on a busy band — wraps around and
    /// smears across the block. The taper makes the slice a real lowpass
    /// with a short response, which is what the guard assumes.
    taper: Vec<f32>,
}

/// Flat passband of the slice taper, Hz each side of the channel centre.
const TAPER_PASS_HZ: f64 = 8_000.0;
/// Where the taper reaches zero, Hz each side (inside the ±24 kHz slice).
const TAPER_STOP_HZ: f64 = 22_000.0;

fn make_taper(out_rate: f64) -> Vec<f32> {
    let bin_hz = out_rate / CHANNEL_BINS as f64;
    (0..CHANNEL_BINS)
        .map(|j| {
            // Bin j sits at (j - W/2) × bin_hz after the rotate below puts
            // the channel centre at index W/2 before inversion; the taper is
            // symmetric so the indexing convention does not matter.
            let f = ((j as f64 - CHANNEL_BINS as f64 / 2.0) * bin_hz).abs();
            if f <= TAPER_PASS_HZ {
                1.0
            } else if f >= TAPER_STOP_HZ {
                0.0
            } else {
                let x = (f - TAPER_PASS_HZ) / (TAPER_STOP_HZ - TAPER_PASS_HZ);
                (0.5 * (1.0 + (core::f64::consts::PI * x).cos())) as f32
            }
        })
        .collect()
}

impl Channelizer {
    /// Build a channelizer for `sample_rate`, extracting the channels at the
    /// given offsets from the capture centre.
    ///
    /// `sample_rate` must be a multiple of 4800 (as every P25 capture rate is),
    /// which also guarantees the block length divides evenly.
    pub fn new(sample_rate: f64, offsets_hz: &[f64]) -> Self {
        let out_rate = (4800 * OUT_SPS) as f64;
        assert!(
            sample_rate >= out_rate,
            "capture rate {sample_rate} is below the working rate {out_rate}"
        );
        let n_f = sample_rate * CHANNEL_BINS as f64 / out_rate;
        let n = n_f.round() as usize;
        assert!(
            (n_f - n as f64).abs() < 1e-6 && n.is_multiple_of(2),
            "sample rate {sample_rate} does not divide into whole blocks"
        );

        // An offset beyond the captured band has no signal to extract; left
        // unchecked the bin index wraps and the channel silently returns a
        // *different* frequency's traffic, which reads as a successful decode
        // of the wrong channel. Refuse instead.
        let nyquist = sample_rate / 2.0;
        for &o in offsets_hz {
            assert!(
                o.abs() < nyquist,
                "offset {o} Hz is outside the captured band (+/-{nyquist} Hz)"
            );
        }

        let bin_hz = sample_rate / n as f64;
        let bins: Vec<isize> = offsets_hz
            .iter()
            .map(|&o| (o / bin_hz).round() as isize)
            .collect();
        // A channel is snapped to the nearest bin, leaving at most half a bin
        // of residual offset. At 2.4 MHz that is 300 Hz, inside the receiver's
        // tolerance, so no further correction is needed.
        let actual_hz = bins.iter().map(|&b| b as f64 * bin_hz).collect();

        Self {
            n,
            hop_in: n / 2,
            keep_out: CHANNEL_BINS / 2,
            bins,
            actual_hz,
            pending: Vec::with_capacity(n * 2),
            fwd: FftPlan::new(n),
            inv: FftPlan::new(CHANNEL_BINS),
            out_rate,
            sample_rate,
            taper: make_taper(out_rate),
        }
    }

    /// Replace the set of channels being extracted, keeping any buffered
    /// input.
    ///
    /// A trunked system grants calls onto channels at runtime, so the set has
    /// to change while the stream keeps flowing. Only the bin list changes —
    /// the transform, which is the expensive part, is unaffected.
    pub fn set_channels(&mut self, offsets_hz: &[f64]) {
        let nyquist = self.sample_rate / 2.0;
        for &o in offsets_hz {
            assert!(
                o.abs() < nyquist,
                "offset {o} Hz is outside the captured band (+/-{nyquist} Hz)"
            );
        }
        let bin_hz = self.sample_rate / self.n as f64;
        self.bins = offsets_hz
            .iter()
            .map(|&o| (o / bin_hz).round() as isize)
            .collect();
        self.actual_hz = self.bins.iter().map(|&b| b as f64 * bin_hz).collect();
    }

    /// Forget buffered input — for a stream that paused while no channel was
    /// wanted, so the next block is not prefixed with stale samples.
    pub fn reset(&mut self) {
        self.pending.clear();
    }

    /// Rate of every output channel.
    pub fn output_rate(&self) -> f64 {
        self.out_rate
    }

    /// Rate of the wideband input.
    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// Offsets actually delivered, after snapping each to the nearest bin.
    pub fn actual_offsets_hz(&self) -> &[f64] {
        &self.actual_hz
    }

    pub fn channel_count(&self) -> usize {
        self.bins.len()
    }

    /// Push interleaved-IQ input; returns per-channel interleaved-IQ output,
    /// one buffer per requested channel, at [`Channelizer::output_rate`].
    ///
    /// Output is produced a block at a time, so a short push may return
    /// nothing; state is retained between calls.
    pub fn process(&mut self, iq: &[f32]) -> Vec<Vec<f32>> {
        let mut i = 0;
        while i + 1 < iq.len() {
            self.pending.push(C32::new(iq[i], iq[i + 1]));
            i += 2;
        }

        let mut out: Vec<Vec<f32>> = vec![Vec::new(); self.bins.len()];
        let mut spectrum = vec![C32::ZERO; self.n];
        let mut slice = vec![C32::ZERO; CHANNEL_BINS];
        let scale = CHANNEL_BINS as f32 / self.n as f32;

        let mut consumed = 0usize;
        while consumed + self.n <= self.pending.len() {
            spectrum.copy_from_slice(&self.pending[consumed..consumed + self.n]);
            self.fwd.forward(&mut spectrum);

            for (ch, &centre) in self.bins.iter().enumerate() {
                // Take the bins around this channel, arranged so its centre
                // lands at DC in the inverse transform.
                for (j, s) in slice.iter_mut().enumerate() {
                    let off = j as isize - (CHANNEL_BINS as isize) / 2;
                    let idx = (centre + off).rem_euclid(self.n as isize) as usize;
                    *s = spectrum[idx].scale(self.taper[j]);
                }
                slice.rotate_left(CHANNEL_BINS / 2);
                self.inv.inverse(&mut slice);

                // Overlap-save: keep the middle half, where the block is free
                // of the transform's circular wrap-around. Consecutive blocks
                // overlap by half the input, so these middles join up.
                let start = CHANNEL_BINS / 4;
                let o = &mut out[ch];
                for v in &slice[start..start + self.keep_out] {
                    let v = v.scale(scale);
                    o.push(v.re);
                    o.push(v.im);
                }
            }
            consumed += self.hop_in;
        }

        if consumed > 0 {
            self.pending.drain(..consumed);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two tones at different offsets in one wideband stream must come out in
    /// their own channels and nowhere else.
    #[test]
    fn separates_two_channels_and_rejects_the_other() {
        let fs = 240_000.0;
        let (a_hz, b_hz) = (-50_000.0, 25_000.0);
        let mut ch = Channelizer::new(fs, &[a_hz, b_hz]);
        assert_eq!(ch.output_rate(), 48_000.0);

        let mut iq = Vec::new();
        for i in 0..120_000 {
            let t = i as f64 / fs;
            let a = 2.0 * std::f64::consts::PI * a_hz * t;
            let b = 2.0 * std::f64::consts::PI * b_hz * t;
            // Both tones present at once, equal amplitude.
            iq.push((a.cos() + b.cos()) as f32);
            iq.push((a.sin() + b.sin()) as f32);
        }
        let out = ch.process(&iq);
        assert_eq!(out.len(), 2);

        // Each channel should hold a strong DC-ish tone (its own, mixed down)
        // and the other should not leak in. Compare mean power.
        for (k, o) in out.iter().enumerate() {
            let tail = &o[o.len() / 2..];
            let p: f32 = tail
                .chunks(2)
                .map(|c| c[0] * c[0] + c[1] * c[1])
                .sum::<f32>()
                / (tail.len() / 2) as f32;
            assert!(p > 0.25, "channel {k} lost its tone (power {p:.4})");
        }

        // A channel tuned to empty spectrum must stay quiet.
        let mut empty = Channelizer::new(fs, &[100_000.0]);
        let q = empty.process(&iq);
        let tail = &q[0][q[0].len() / 2..];
        let p: f32 = tail
            .chunks(2)
            .map(|c| c[0] * c[0] + c[1] * c[1])
            .sum::<f32>()
            / (tail.len() / 2) as f32;
        assert!(p < 0.01, "empty channel picked up {p:.4} of power");
    }

    /// A tone inside the channel passes; a tone near the slice edge — the
    /// channel two over — is removed by the taper rather than wrapped around.
    #[test]
    fn taper_keeps_the_channel_and_rejects_the_slice_edge() {
        let fs = 240_000.0;
        let power = |hz: f64| {
            let mut ch = Channelizer::new(fs, &[0.0]);
            let mut iq = Vec::new();
            for i in 0..120_000 {
                let p = 2.0 * std::f64::consts::PI * hz * i as f64 / fs;
                iq.push(p.cos() as f32);
                iq.push(p.sin() as f32);
            }
            let o = ch.process(&iq).remove(0);
            let tail = &o[o.len() / 2..];
            tail.chunks(2)
                .map(|c| c[0] * c[0] + c[1] * c[1])
                .sum::<f32>()
                / (tail.len() / 2) as f32
        };
        assert!(power(4_000.0) > 0.25, "in-channel tone lost");
        assert!(power(23_000.0) < 0.01, "slice-edge tone not removed");
    }

    #[test]
    #[should_panic(expected = "outside the captured band")]
    fn refuses_an_offset_beyond_the_captured_band() {
        // Without this the bin index wraps and the channel quietly delivers a
        // different frequency — which decodes fine and is completely wrong.
        Channelizer::new(2_400_000.0, &[1_400_000.0]);
    }

    #[test]
    fn reports_the_offset_it_actually_tuned() {
        // Offsets snap to the nearest bin; the caller needs to know where the
        // channel really is rather than where it asked for.
        let ch = Channelizer::new(2_400_000.0, &[50_000.0, 6_250.0]);
        for (want, got) in [50_000.0, 6_250.0].iter().zip(ch.actual_offsets_hz()) {
            assert!(
                (want - got).abs() <= 300.0,
                "asked {want}, tuned {got} — beyond the receiver's tolerance"
            );
        }
    }

    #[test]
    fn streams_across_push_boundaries() {
        // A block split across two pushes must produce the same samples as one
        // push, or a live capture would decode differently from a file.
        let fs = 240_000.0;
        let mut iq = Vec::new();
        for i in 0..60_000 {
            let p = 2.0 * std::f64::consts::PI * 12_500.0 * i as f64 / fs;
            iq.push(p.cos() as f32);
            iq.push(p.sin() as f32);
        }
        let whole = Channelizer::new(fs, &[12_500.0]).process(&iq).remove(0);

        let mut split = Channelizer::new(fs, &[12_500.0]);
        let mid = (iq.len() / 2) & !1;
        let mut got = split.process(&iq[..mid]);
        let rest = split.process(&iq[mid..]);
        got[0].extend_from_slice(&rest[0]);

        assert_eq!(got[0].len(), whole.len(), "sample count differs");
        for (i, (a, b)) in got[0].iter().zip(whole.iter()).enumerate() {
            assert!((a - b).abs() < 1e-4, "sample {i}: {a} vs {b}");
        }
    }
}
