//! SDR hardware abstraction. Backends: IQ file (first), Seify RTL-SDR and
//! Airspy (Phase 2, behind feature flags).

use std::io::Read;

#[cfg(feature = "rtlsdr")]
pub mod rtlsdr;

/// Complex IQ sample pair as delivered by a source, interleaved f32.
pub type IqBlock = Vec<f32>;

#[derive(Debug)]
pub enum SourceError {
    Io(std::io::Error),
    Eof,
    Unsupported(String),
}

impl From<std::io::Error> for SourceError {
    fn from(e: std::io::Error) -> Self {
        SourceError::Io(e)
    }
}

/// A stream of complex baseband samples at a known rate and center frequency.
pub trait SdrSource {
    fn sample_rate(&self) -> f64;
    fn center_freq(&self) -> f64;
    /// Fill `buf` with interleaved I/Q f32 samples. Returns samples written
    /// (pairs count as 2), or `Err(Eof)` when the stream ends.
    fn read(&mut self, buf: &mut [f32]) -> Result<usize, SourceError>;
}

/// Raw IQ file playback (interleaved f32 little-endian, `.cf32`).
/// This is the Phase 0/1 workhorse: everything is developed offline against
/// the benchmark corpus before any live hardware is wired in.
pub struct IqFileSource<R: Read> {
    reader: R,
    sample_rate: f64,
    center_freq: f64,
}

impl<R: Read> IqFileSource<R> {
    pub fn new(reader: R, sample_rate: f64, center_freq: f64) -> Self {
        Self {
            reader,
            sample_rate,
            center_freq,
        }
    }
}

impl<R: Read> SdrSource for IqFileSource<R> {
    fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    fn center_freq(&self) -> f64 {
        self.center_freq
    }

    fn read(&mut self, buf: &mut [f32]) -> Result<usize, SourceError> {
        let mut bytes = vec![0u8; buf.len() * 4];
        let mut filled = 0;
        while filled < bytes.len() {
            match self.reader.read(&mut bytes[filled..])? {
                0 => break,
                n => filled += n,
            }
        }
        let samples = filled / 4;
        if samples == 0 {
            return Err(SourceError::Eof);
        }
        for i in 0..samples {
            buf[i] = f32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap());
        }
        Ok(samples)
    }
}
