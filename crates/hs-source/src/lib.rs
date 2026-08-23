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

/// A handle for retuning a streaming radio from another thread. Like
/// [`GainHandle`], the source applies the newest frequency from inside its
/// own `read`, so the tuner is only ever touched by the thread that owns it.
/// This is what a dual-SDR hopper uses to move the voice radio between
/// channels without sharing the device across threads.
#[derive(Clone, Default)]
pub struct FreqHandle(std::sync::Arc<std::sync::Mutex<Option<f64>>>);

impl FreqHandle {
    /// Request a retune to `hz` (the new centre frequency).
    pub fn request(&self, hz: f64) {
        *self.0.lock().unwrap() = Some(hz);
    }
    /// Take the pending retune, if any.
    pub fn take(&self) -> Option<f64> {
        self.0.lock().unwrap().take()
    }
}

/// The R820T/R828D tuner's gain steps in dB — what an RTL-SDR actually
/// offers; a value in between is rounded to one of these by the driver.
pub const RTL_TUNER_GAINS_DB: &[f64] = &[
    0.0, 0.9, 1.4, 2.7, 3.7, 7.7, 8.7, 12.5, 14.4, 15.7, 16.6, 19.7, 20.7, 22.9, 25.4, 28.0, 29.7,
    32.8, 33.8, 36.4, 37.2, 38.6, 40.2, 42.1, 43.4, 43.9, 44.5, 48.0, 49.6,
];

/// Sensible default tuner gain for an RTL-SDR: 40 dB, the same value the
/// known-good field capture was made at (`rtl_sdr -g 40`). The R820T's
/// hardware AGC is unreliable and produced garbled voice at low/floor gain,
/// so a fresh device defaults to a fixed manual gain rather than AGC.
pub const RTL_DEFAULT_GAIN_DB: f64 = 40.0;

/// Clamp an RTL-SDR gain into the tuner's valid step list. Out-of-range values
/// (e.g. a stale negative reading) are snapped to the nearest real step rather
/// than passed to the driver, which would either reject them or pick a garbage
/// floor — the failure mode behind "-24 dB" garbage voice.
pub fn clamp_rtl_gain(db: f64) -> f64 {
    let min = RTL_TUNER_GAINS_DB[0];
    let max = RTL_TUNER_GAINS_DB[RTL_TUNER_GAINS_DB.len() - 1];
    let clamped = db.clamp(min, max);
    RTL_TUNER_GAINS_DB
        .iter()
        .copied()
        .min_by(|a, b| (a - clamped).abs().total_cmp(&(b - clamped).abs()))
        .unwrap_or(clamped)
}

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
    /// A handle that retunes this radio while it streams. Radios that support
    /// retuning return their handle; others (files, wrappers) return a no-op.
    /// The dual-SDR hopper writes the next voice-channel frequency here, and
    /// the radio applies it inside its own `read`.
    fn freq_handle(&self) -> FreqHandle {
        FreqHandle::default()
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
    fn freq_handle(&self) -> FreqHandle {
        (**self).freq_handle()
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
