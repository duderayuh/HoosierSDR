//! Continuous streaming decode: pull IQ from any [`SdrSource`] in blocks and
//! feed the stateful [`ChannelDecoder`], emitting decode output as it arrives.
//!
//! This is the "live" path — the same loop drives a file replay or a live SDR
//! (an RTL-SDR via `hs-source`'s `rtlsdr` feature). The decoder is stateful
//! across blocks, so block boundaries are invisible to the demodulator.

use crate::decoder::{ChannelDecoder, DecodeOutput};
use hs_dsp::resample::{normalize_ratio, RationalResampler};
use hs_dsp::C32;
use hs_source::{SdrSource, SourceError};

/// A source whose rate has been normalized to a multiple of the 4800-baud
/// symbol rate, streaming. An Airspy R2 samples at 10 or 2.5 MSPS — neither
/// divides by 4800 — so this wraps it in the same gentle 24/25 polyphase
/// resample the offline path applies to a recording, and reports the clean
/// rate (9.6 / 2.4 MSPS) as its own. Everything downstream — decoder, trunk
/// follower, recordings — then sees a native-rate source. A source whose rate
/// is already clean (an RTL-SDR at 2.4 MSPS) passes straight through.
pub struct Normalized<S: SdrSource> {
    inner: S,
    rs: Option<RationalResampler>,
    out_rate: f64,
    /// Raw samples from the inner source, pulled in chunks.
    raw: Vec<f32>,
    raw_pos: usize,
}

impl<S: SdrSource> Normalized<S> {
    pub fn new(inner: S) -> Self {
        let in_rate = inner.sample_rate();
        let (rs, out_rate) = match normalize_ratio(in_rate) {
            Some((up, down, out)) => {
                // Preserve 0.8 of the output Nyquist, as `resample_iq` does.
                let rs = RationalResampler::new(in_rate, up, down, 0.4 * out);
                (Some(rs), out)
            }
            None => (None, in_rate),
        };
        Self {
            inner,
            rs,
            out_rate,
            raw: vec![0.0; 65536 * 2],
            raw_pos: usize::MAX,
        }
    }

    /// The wrapped source.
    pub fn inner(&self) -> &S {
        &self.inner
    }

    /// True if a resample is actually happening.
    pub fn is_resampling(&self) -> bool {
        self.rs.is_some()
    }
}

impl<S: SdrSource> SdrSource for Normalized<S> {
    fn sample_rate(&self) -> f64 {
        self.out_rate
    }

    fn center_freq(&self) -> f64 {
        self.inner.center_freq()
    }

    fn dropped(&self) -> u64 {
        self.inner.dropped()
    }

    fn read(&mut self, buf: &mut [f32]) -> Result<usize, SourceError> {
        let Some(rs) = self.rs.as_mut() else {
            return self.inner.read(buf);
        };
        let want = buf.len() & !1;
        let mut n = 0;
        while n < want {
            if self.raw_pos >= self.raw.len() {
                // Refill. `raw` is trimmed to what was read so `raw_pos`
                // comparisons stay exact.
                let mut chunk = std::mem::take(&mut self.raw);
                if chunk.len() < 65536 * 2 {
                    chunk.resize(65536 * 2, 0.0);
                }
                let got = self.inner.read(&mut chunk)?;
                chunk.truncate(got);
                self.raw = chunk;
                self.raw_pos = 0;
                if got == 0 {
                    if n > 0 {
                        break;
                    }
                    continue;
                }
            }
            let x = C32::new(self.raw[self.raw_pos], self.raw[self.raw_pos + 1]);
            self.raw_pos += 2;
            if let Some(y) = rs.push(x) {
                buf[n] = y.re;
                buf[n + 1] = y.im;
                n += 2;
            }
        }
        Ok(n)
    }
}

/// Cumulative stats over a streaming session.
#[derive(Debug, Default, Clone)]
pub struct StreamStats {
    pub blocks: u64,
    pub iq_samples: u64,
    pub syncs: u32,
    pub grants: usize,
    pub pcm_samples: usize,
    pub encrypted_skips: usize,
}

/// A source drained continuously by its own thread into a bounded queue.
///
/// A radio read synchronously from the decode loop loses whatever arrives
/// while the decoder is busy: librtlsdr only buffers *inside* a read call, so
/// every block boundary is a gap. Measured live on an RTL-SDR control channel,
/// that kept ~73% of the frame syncs and essentially no TSBKs — the 48-bit
/// sync survives a gap far more often than a full trellis-coded block does —
/// while the same dongle's `rtl_sdr` recording decoded 1191 TSBKs offline.
/// Draining on a dedicated thread keeps the radio's own buffer empty; if the
/// decoder ever falls behind, the *newest* block is dropped and counted
/// (the queue already holds ~2 s of the freshest contiguous audio), rather
/// than the stream silently corrupted. `read` waits at most 250 ms so a
/// caller polling a stop flag is never stuck.
pub struct Buffered {
    rx: std::sync::mpsc::Receiver<Result<Vec<f32>, SourceError>>,
    reader: Option<std::thread::JoinHandle<()>>,
    sample_rate: f64,
    center_freq: f64,
    queue_drops: std::sync::Arc<std::sync::atomic::AtomicU64>,
    inner_drops: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pending: Vec<f32>,
    pending_pos: usize,
}

impl Buffered {
    /// Wrap a live radio: blocks the consumer can't keep up with are dropped
    /// and counted (the radio's own buffer must never overflow).
    pub fn new<S: SdrSource + Send + 'static>(inner: S, block_pairs: usize) -> Self {
        Self::with_policy(inner, block_pairs, false)
    }

    /// Wrap a recording (or anything that can wait): the reader blocks when
    /// the queue is full, so every sample reaches the consumer.
    pub fn lossless<S: SdrSource + Send + 'static>(inner: S, block_pairs: usize) -> Self {
        Self::with_policy(inner, block_pairs, true)
    }

    fn with_policy<S: SdrSource + Send + 'static>(
        mut inner: S,
        block_pairs: usize,
        lossless: bool,
    ) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::mpsc::{sync_channel, TrySendError};
        use std::sync::Arc;

        let sample_rate = inner.sample_rate();
        let center_freq = inner.center_freq();
        // ~2 s of queue whatever the rate.
        let depth = ((sample_rate * 2.0) / block_pairs as f64)
            .ceil()
            .clamp(16.0, 1024.0) as usize;
        let (tx, rx) = sync_channel::<Result<Vec<f32>, SourceError>>(depth);
        let queue_drops = Arc::new(AtomicU64::new(0));
        let inner_drops = Arc::new(AtomicU64::new(0));
        let (qd, id) = (Arc::clone(&queue_drops), Arc::clone(&inner_drops));
        let reader = std::thread::spawn(move || {
            let mut buf = vec![0.0f32; block_pairs * 2];
            loop {
                match inner.read(&mut buf) {
                    Ok(0) => continue,
                    Ok(n) => {
                        id.store(inner.dropped(), Ordering::Relaxed);
                        let block = Ok(buf[..n].to_vec());
                        if lossless {
                            if tx.send(block).is_err() {
                                return;
                            }
                        } else {
                            match tx.try_send(block) {
                                Ok(()) => {}
                                Err(TrySendError::Full(_)) => {
                                    qd.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(TrySendError::Disconnected(_)) => return,
                            }
                        }
                    }
                    // The radio's own error is the thing the user needs to
                    // see; pass it through (blocking send: it's the last one)
                    // instead of turning it into a silent end-of-stream.
                    Err(SourceError::Eof) => return,
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        return;
                    }
                }
            }
        });
        Self {
            rx,
            reader: Some(reader),
            sample_rate,
            center_freq,
            queue_drops,
            inner_drops,
            pending: Vec::new(),
            pending_pos: 0,
        }
    }
}

impl Buffered {
    /// Read without waiting: whatever is buffered or already queued, else
    /// `Ok(0)`. For a loop that blocks on one radio and drains the others.
    pub fn try_read(&mut self, buf: &mut [f32]) -> Result<usize, SourceError> {
        if self.pending_pos >= self.pending.len() {
            self.pending = match self.rx.try_recv() {
                Ok(r) => r?,
                Err(std::sync::mpsc::TryRecvError::Empty) => return Ok(0),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return Err(SourceError::Eof),
            };
            self.pending_pos = 0;
        }
        let avail = &self.pending[self.pending_pos..];
        let n = avail.len().min(buf.len() & !1);
        buf[..n].copy_from_slice(&avail[..n]);
        self.pending_pos += n;
        Ok(n)
    }
}

impl Buffered {
    /// Throw away whatever is queued right now — the blocks that piled up
    /// while a live caller was busy measuring — and return how many.
    pub fn discard_queued(&mut self) -> usize {
        let mut n = 0;
        while self.rx.try_recv().is_ok() {
            n += 1;
        }
        self.pending.clear();
        self.pending_pos = 0;
        n
    }
}

impl Drop for Buffered {
    /// Close the channel, then wait for the reader so the radio is actually
    /// closed (and free for the next open) when this goes away.
    fn drop(&mut self) {
        // Dropping `rx` first makes the reader's next send fail and return.
        let (_tx, rx) = std::sync::mpsc::sync_channel::<Result<Vec<f32>, SourceError>>(1);
        let old = std::mem::replace(&mut self.rx, rx);
        drop(old);
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
    }
}

impl SdrSource for Buffered {
    fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    fn center_freq(&self) -> f64 {
        self.center_freq
    }

    fn dropped(&self) -> u64 {
        use std::sync::atomic::Ordering;
        self.queue_drops.load(Ordering::Relaxed) + self.inner_drops.load(Ordering::Relaxed)
    }

    fn read(&mut self, buf: &mut [f32]) -> Result<usize, SourceError> {
        if self.pending_pos >= self.pending.len() {
            // A bounded wait so a caller polling a stop flag gets control
            // back even when the radio has gone quiet.
            self.pending = match self.rx.recv_timeout(std::time::Duration::from_millis(250)) {
                Ok(r) => r?,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => return Ok(0),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(SourceError::Eof)
                }
            };
            self.pending_pos = 0;
        }
        let avail = &self.pending[self.pending_pos..];
        let n = avail.len().min(buf.len() & !1);
        buf[..n].copy_from_slice(&avail[..n]);
        self.pending_pos += n;
        Ok(n)
    }
}

/// Read from `src` in blocks of `block_pairs` IQ pairs, decode continuously,
/// and invoke `on_output` for each non-empty block result (a resolved grant,
/// decoded audio, or an encryption skip). Returns when the source reports EOF.
///
/// `on_output` is where a caller routes audio to a device, appends to a WAV,
/// or updates a UI — it runs in the capture loop, so keep it cheap.
pub fn run<S: SdrSource>(
    src: &mut S,
    dec: &mut ChannelDecoder,
    block_pairs: usize,
    mut on_output: impl FnMut(&DecodeOutput),
) -> Result<StreamStats, SourceError> {
    let mut buf = vec![0.0f32; block_pairs * 2];
    let mut stats = StreamStats::default();
    loop {
        match src.read(&mut buf) {
            Ok(0) => continue,
            Ok(n) => {
                let out = dec.process(&buf[..n]);
                stats.blocks += 1;
                stats.iq_samples += (n / 2) as u64;
                stats.syncs += out.syncs;
                stats.grants += out.grants.len();
                stats.pcm_samples += out.pcm.len();
                stats.encrypted_skips += out.encrypted_skips.len();
                if !out.grants.is_empty() || !out.pcm.is_empty() || !out.encrypted_skips.is_empty()
                {
                    on_output(&out);
                }
            }
            Err(SourceError::Eof) => break,
            Err(e) => return Err(e),
        }
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::ChannelDecoder;
    use hs_source::IqFileSource;

    /// Build a C4FM demo transmission as interleaved-f32 bytes.
    fn demo_bytes() -> Vec<u8> {
        use hs_dsp::modulator::C4fmModulator;
        use hs_dsp::C32;
        use hs_p25::synth::{build_ldu1, build_tsdu};
        use hs_p25::voice::ImbeFrame;

        let iden =
            (1u64 << 60) | (100u64 << 51) | (1u64 << 50) | (100u64 << 32) | (851_012_500u64 / 5);
        let channel = (1u64 << 12) | 10;
        let grant = (channel << 40) | (0x2F93u64 << 24) | 0xBEEF1;
        let mut stream = build_tsdu(0x293, &[(0x3D, 0, iden), (0x00, 0, grant)]);
        let mut frames: [ImbeFrame; 9] = [[[0u8; 23]; 8]; 9];
        let widths = [23usize, 23, 23, 23, 15, 15, 15, 7];
        for (k, fr) in frames.iter_mut().enumerate() {
            for (w, row) in fr.iter_mut().enumerate() {
                for (x, cell) in row.iter_mut().enumerate().take(widths[w]) {
                    *cell = (((k + 1) * (w + 2) * (x + 5)) % 2) as u8;
                }
            }
        }
        stream.extend(build_ldu1(0x293, &frames));

        let mut m = C4fmModulator::new(48000.0);
        let mut iq: Vec<C32> = Vec::new();
        for i in 0..400 {
            m.modulate(if i % 2 == 0 { 0b01 } else { 0b11 }, &mut iq);
        }
        for &d in &stream {
            m.modulate(d, &mut iq);
        }
        for _ in 0..200 {
            m.modulate(0b00, &mut iq);
        }
        let mut bytes = Vec::with_capacity(iq.len() * 8);
        for c in iq {
            bytes.extend_from_slice(&c.re.to_le_bytes());
            bytes.extend_from_slice(&c.im.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn streams_blocks_and_decodes_across_boundaries() {
        // Feed the demo through a file source in SMALL blocks, so the frame is
        // split across many block boundaries — the decoder's cross-block state
        // must still recover the grant and the voice.
        let bytes = demo_bytes();
        let cursor = std::io::Cursor::new(bytes);
        let mut src = IqFileSource::new(cursor, 48000.0, 851_000_000.0);
        let mut dec = ChannelDecoder::new(48000.0, crate::decoder::EqMode::Bypass);

        let mut grants_seen = 0;
        let stats = run(&mut src, &mut dec, 512, |out| {
            grants_seen += out.grants.len();
        })
        .expect("stream ok");

        assert!(
            stats.blocks > 1,
            "expected multiple blocks, got {}",
            stats.blocks
        );
        assert!(stats.syncs >= 1, "no sync across streamed blocks");
        assert!(grants_seen >= 1, "grant not recovered from streamed blocks");
        assert_eq!(stats.pcm_samples, 9 * 160, "voice not decoded from stream");
    }
}

#[cfg(test)]
mod wrapper_tests {
    use super::*;
    use hs_source::IqFileSource;

    fn tone_source(rate: f64, hz: f64, secs: f64) -> IqFileSource<std::io::Cursor<Vec<u8>>> {
        let n = (rate * secs) as usize;
        let mut bytes = Vec::with_capacity(n * 8);
        for i in 0..n {
            let ph = 2.0 * std::f64::consts::PI * hz * i as f64 / rate;
            bytes.extend_from_slice(&(ph.cos() as f32).to_le_bytes());
            bytes.extend_from_slice(&(ph.sin() as f32).to_le_bytes());
        }
        IqFileSource::new(std::io::Cursor::new(bytes), rate, 851e6)
    }

    fn drain<S: SdrSource>(src: &mut S) -> Vec<f32> {
        let mut out = Vec::new();
        let mut buf = vec![0.0f32; 4096];
        loop {
            match src.read(&mut buf) {
                Ok(0) => continue,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(SourceError::Eof) => return out,
                Err(e) => panic!("{e:?}"),
            }
        }
    }

    /// An Airspy's 2.5 MSPS comes out as 2.4 MSPS with an in-band tone intact
    /// and the right number of samples; a native 2.4 MSPS source is untouched.
    #[test]
    fn normalizes_an_airspy_rate_and_passes_a_clean_one() {
        let mut n = Normalized::new(tone_source(2_500_000.0, 100_000.0, 0.2));
        assert!(n.is_resampling());
        assert_eq!(n.sample_rate(), 2_400_000.0);
        let out = drain(&mut n);
        let pairs = out.len() / 2;
        // Short by the filter's group delay (not flushed at EOF), nothing more.
        assert!(
            (480_000.0 - pairs as f64) < 1000.0 && pairs <= 480_000,
            "got {pairs} pairs"
        );
        // Tone power (skip the filter's start-up) should be ~unit.
        let tail = &out[out.len() / 2..];
        let p: f32 = tail.iter().map(|v| v * v).sum::<f32>() / (tail.len() / 2) as f32;
        assert!((p - 1.0).abs() < 0.05, "tone power {p:.3}");

        let mut c = Normalized::new(tone_source(2_400_000.0, 100_000.0, 0.05));
        assert!(!c.is_resampling());
        assert_eq!(drain(&mut c).len(), 2 * 120_000);
    }

    /// The reader-thread wrapper delivers every sample in order and ends
    /// cleanly at the source's EOF.
    #[test]
    fn buffered_delivers_everything_in_order() {
        let rate = 240_000.0;
        let mut direct = drain(&mut tone_source(rate, 1000.0, 0.1));
        let mut b = Buffered::new(tone_source(rate, 1000.0, 0.1), 4096);
        assert_eq!(b.sample_rate(), rate);
        let got = drain(&mut b);
        assert_eq!(got.len(), direct.len());
        assert!(got.iter().zip(direct.iter_mut()).all(|(a, b)| a == b));
        assert_eq!(b.dropped(), 0);
    }
}
