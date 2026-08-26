//! Live RTL-SDR capture via SoapySDR (the C library + librtlsdr). Unlike
//! Seify's pure-Rust `rtlsdr` driver, this supports *every* RTL tuner — the
//! E4000 (Nooelec Smartee XTR), R828D (RTL-SDR Blog V4), R820T2, FC0012/13,
//! etc. — because librtlsdr does the tuner work. Behind the `soapy` feature.
//!
//! This is the path for a Nooelec Smartee XTR: its E4000 tuner panics the
//! pure-Rust driver with "Failed to find tuner, aborting". SoapySDR selects the
//! RTL module with `soapy_driver=rtlsdr` and hands everything to librtlsdr.
//!
//! Requires `brew install soapysdr soapyrtlsdr` (and `librtlsdr`).

use crate::{FreqHandle, GainHandle, GainSetting, SdrSource, SourceError};
use num_complex::Complex32;
use seify::{DynDevice, DynRxStreamer, RxStreamer};

/// A live RTL-SDR source opened through SoapySDR. Owns the device and an active
/// RX streamer; the `SdrSource` impl is the same contract the pure-Rust RTL-SDR
/// and Airspy paths satisfy, so it drops into the decode chain unchanged.
pub struct SoapyRtlSource {
    _dev: DynDevice,
    rx: DynRxStreamer,
    sample_rate: f64,
    center_freq: f64,
    scratch: Vec<Complex32>,
    gain: GainHandle,
    freq: FreqHandle,
}

/// Clamp a gain request into the union of a Seify range (min/max across all
/// items). Unlike the pure-Rust path, the E4000 and R820T2 report *different*
/// gain ranges and step lists, so the R820T2-specific `clamp_rtl_gain` table is
/// wrong here — clamp to whatever the device actually reports instead.
fn clamp_to_range(db: f64, r: &seify::Range) -> f64 {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for item in &r.items {
        match item {
            seify::RangeItem::Interval(a, b) | seify::RangeItem::Step(a, b, _) => {
                min = min.min(*a);
                max = max.max(*b);
            }
            seify::RangeItem::Value(v) => {
                min = min.min(*v);
                max = max.max(*v);
            }
        }
    }
    if min.is_finite() && max.is_finite() {
        db.clamp(min, max)
    } else {
        db
    }
}

impl SoapyRtlSource {
    /// The RTL-SDRs SoapySDR can see, as (reopen args, human label).
    ///
    /// Enumerates the `rtlsdr` SoapySDR module directly. seify 0.23's own Soapy
    /// probe is unusable for enumeration: it forwards the seify-level
    /// `driver=soapy` key into `SoapySDRDevice_enumerate`, which filters on a
    /// module literally named "soapy" (none exists) and returns nothing. The
    /// reopen args below carry `soapy_driver=rtlsdr`, which `open()` translates
    /// back into SoapySDR's `driver` correctly.
    pub fn list() -> Vec<(String, String, String)> {
        let devs = match soapysdr::enumerate("driver=rtlsdr") {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        devs.into_iter()
            .map(|a| {
                let label = a
                    .get("label")
                    .filter(|s| !s.is_empty())
                    .or_else(|| a.get("product"))
                    .unwrap_or("RTL-SDR (Soapy)")
                    .to_string();
                let serial = a.get("serial").unwrap_or("");
                let tuner = a.get("tuner").map(|s| s.to_string()).unwrap_or_default();
                let args = if serial.is_empty() {
                    "driver=soapy,soapy_driver=rtlsdr".to_string()
                } else {
                    format!("driver=soapy,soapy_driver=rtlsdr,serial={serial}")
                };
                (args, label, tuner)
            })
            .collect()
    }

    /// Open the RTL-SDR selected by `args` (from [`Self::list`]) at
    /// `center_freq` Hz / `sample_rate` S/s, with manual `gain` dB or tuner
    /// AGC when `gain` is `None`.
    pub fn open(
        args: &str,
        center_freq: f64,
        sample_rate: f64,
        gain: Option<f64>,
    ) -> Result<Self, SourceError> {
        let map = |e: seify::Error| SourceError::Unsupported(format!("soapy: {e:?}"));
        let dev = DynDevice::from_args(args).map_err(map)?;
        let rx0 = dev.rx(0).map_err(map)?;
        rx0.sample_rate().set(sample_rate).map_err(map)?;
        rx0.frequency().set(center_freq).map_err(map)?;
        match gain {
            Some(g) => {
                let _ = rx0.agc().disable();
                let g = match rx0.gain().range() {
                    Ok(r) => clamp_to_range(g, &r),
                    Err(_) => g,
                };
                rx0.gain().set(g).map_err(map)?;
            }
            None => {
                let _ = rx0.agc().enable();
            }
        }
        let mut rx = dev.rx_streamer(&[0]).map_err(map)?;
        rx.activate().map_err(map)?;
        Ok(Self {
            _dev: dev,
            rx,
            sample_rate,
            center_freq,
            scratch: Vec::new(),
            gain: GainHandle::default(),
            freq: FreqHandle::default(),
        })
    }

    /// A handle that changes this radio's gain while it streams.
    pub fn gain_handle(&self) -> GainHandle {
        self.gain.clone()
    }

    /// A handle that retunes this radio while it streams (dual-SDR hopper).
    pub fn freq_handle(&self) -> FreqHandle {
        self.freq.clone()
    }

    /// The tuner's gain range (min, max, step) in dB, if the device reports one.
    pub fn gain_range(&self) -> Option<(f64, f64, f64)> {
        let rx0 = self._dev.rx(0).ok()?;
        let r = rx0.gain().range().ok()?;
        match r.items.first()? {
            seify::RangeItem::Interval(a, b) => Some((*a, *b, 0.0)),
            seify::RangeItem::Step(a, b, st) => Some((*a, *b, *st)),
            _ => None,
        }
    }

    fn apply_gain(&mut self, g: &GainSetting) -> Result<(), seify::Error> {
        let rx0 = self._dev.rx(0)?;
        match g {
            GainSetting::Agc => {
                rx0.agc().enable()?;
            }
            GainSetting::Manual(db) => {
                let _ = rx0.agc().disable();
                let db = match rx0.gain().range() {
                    Ok(r) => clamp_to_range(*db, &r),
                    Err(_) => *db,
                };
                rx0.gain().set(db)?;
            }
            _ => {} // Airspy-only settings mean nothing to an RTL-SDR.
        }
        Ok(())
    }

    fn apply_freq(&mut self, hz: f64) -> Result<(), seify::Error> {
        let rx0 = self._dev.rx(0)?;
        rx0.frequency().set(hz)?;
        self.center_freq = hz;
        Ok(())
    }
}

impl SdrSource for SoapyRtlSource {
    fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    fn center_freq(&self) -> f64 {
        self.center_freq
    }

    fn freq_handle(&self) -> FreqHandle {
        self.freq.clone()
    }

    fn read(&mut self, buf: &mut [f32]) -> Result<usize, SourceError> {
        if let Some(g) = self.gain.take() {
            if let Err(e) = self.apply_gain(&g) {
                eprintln!("rtl-sdr gain {g:?}: {e:?}");
            }
        }
        if let Some(hz) = self.freq.take() {
            if let Err(e) = self.apply_freq(hz) {
                eprintln!("rtl-sdr retune to {hz}: {e:?}");
            }
        }
        let pairs = buf.len() / 2;
        if self.scratch.len() < pairs {
            self.scratch.resize(pairs, Complex32::new(0.0, 0.0));
        }
        let n = self
            .rx
            .read(&mut [&mut self.scratch[..pairs]], 1_000_000)
            .map_err(|e| SourceError::Unsupported(format!("soapy read: {e:?}")))?;
        for (i, c) in self.scratch[..n].iter().enumerate() {
            buf[i * 2] = c.re;
            buf[i * 2 + 1] = c.im;
        }
        Ok(n * 2)
    }
}

impl Drop for SoapyRtlSource {
    /// Stop the async read before the device closes (same crash-avoidance order
    /// as the pure-Rust path).
    fn drop(&mut self) {
        let _ = self.rx.deactivate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `list()` returns well-formed seify reopen args, and an empty list (not a
    /// panic) when no RTL-SDR is attached (e.g. CI).
    #[test]
    fn list_returns_seify_reopen_args() {
        for (args, label, _tuner) in SoapyRtlSource::list() {
            assert!(args.contains("driver=soapy"), "bad args: {args}");
            assert!(args.contains("soapy_driver=rtlsdr"), "bad args: {args}");
            assert!(!label.is_empty());
        }
    }
}
