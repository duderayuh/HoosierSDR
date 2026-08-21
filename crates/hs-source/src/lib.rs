//! SDR hardware abstraction. Backends: IQ file (first), Seify RTL-SDR and
//! Airspy (Phase 2, behind feature flags).

use std::io::Read;

#[cfg(feature = "airspy")]
pub mod airspy;
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

/// A gain setting for a live radio, in SDRTrunk's terms.
#[derive(Debug, Clone, PartialEq)]
pub enum GainSetting {
    /// Let the tuner's AGC run.
    Agc,
    /// Overall gain in dB (RTL-SDR: the tuner gain from its step list).
    Manual(f64),
    /// Airspy presets: one knob 0–21 that sets LNA/mixer/VGA together.
    AirspyLinearity(u8),
    AirspySensitivity(u8),
    /// Airspy stages set by hand (LNA 0–14, mixer 0–15, VGA 0–15) with the
    /// two front-end AGCs.
    AirspyManual {
        lna: u8,
        mixer: u8,
        vga: u8,
        lna_agc: bool,
        mixer_agc: bool,
    },
}

/// A handle for changing a streaming radio's gain from another thread. The
/// source applies the newest request from inside its own `read`, so the
/// device is only ever touched by the thread that owns it.
#[derive(Clone, Default)]
pub struct GainHandle(std::sync::Arc<std::sync::Mutex<Option<GainSetting>>>);

impl GainHandle {
    pub fn request(&self, g: GainSetting) {
        *self.0.lock().unwrap() = Some(g);
    }
    /// Take the pending request, if any.
    pub fn take(&self) -> Option<GainSetting> {
        self.0.lock().unwrap().take()
    }
}

/// The R820T/R828D tuner's gain steps in dB — what an RTL-SDR actually
/// offers; a value in between is rounded to one of these by the driver.
pub const RTL_TUNER_GAINS_DB: &[f64] = &[
    0.0, 0.9, 1.4, 2.7, 3.7, 7.7, 8.7, 12.5, 14.4, 15.7, 16.6, 19.7, 20.7, 22.9, 25.4, 28.0, 29.7,
    32.8, 33.8, 36.4, 37.2, 38.6, 40.2, 42.1, 43.4, 43.9, 44.5, 48.0, 49.6,
];

/// A stream of complex baseband samples at a known rate and center frequency.
pub trait SdrSource {
    fn sample_rate(&self) -> f64;
    fn center_freq(&self) -> f64;
    /// Fill `buf` with interleaved I/Q f32 samples. Returns samples written
    /// (pairs count as 2), or `Err(Eof)` when the stream ends.
    fn read(&mut self, buf: &mut [f32]) -> Result<usize, SourceError>;
    /// Samples or blocks lost so far on the way from the hardware (device-side
    /// USB starvation, or a consumer that fell behind). A file never drops.
    fn dropped(&self) -> u64 {
        0
    }
}

impl<T: SdrSource + ?Sized> SdrSource for Box<T> {
    fn sample_rate(&self) -> f64 {
        (**self).sample_rate()
    }
    fn center_freq(&self) -> f64 {
        (**self).center_freq()
    }
    fn read(&mut self, buf: &mut [f32]) -> Result<usize, SourceError> {
        (**self).read(buf)
    }
    fn dropped(&self) -> u64 {
        (**self).dropped()
    }
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
