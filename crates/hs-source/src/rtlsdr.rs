//! Live RTL-SDR capture via Seify. Behind the `rtlsdr` feature so the core
//! and CI stay pure-Rust and libusb-free; enable it to capture off-air.
//!
//! Seify's RTL-SDR backend delivers `num_complex::Complex32` samples; this
//! source converts them to the interleaved-f32 blocks the decode chain
//! consumes. Tune, sample rate, and gain are set at construction.

use crate::{SdrSource, SourceError};
use num_complex::Complex32;
use seify::{DynDevice, DynRxStreamer, RxStreamer};

/// A live RTL-SDR source. Owns the device and an active RX streamer.
pub struct RtlSdrSource {
    _dev: DynDevice,
    rx: DynRxStreamer,
    sample_rate: f64,
    center_freq: f64,
    scratch: Vec<Complex32>,
}

impl RtlSdrSource {
    /// The RTL-SDRs Seify can see: (args that reopen exactly this device,
    /// human label).
    pub fn list() -> Vec<(String, String)> {
        seify::enumerate_with_args("driver=rtlsdr")
            .unwrap_or_default()
            .into_iter()
            .map(|a| {
                let label = a
                    .get::<String>("label")
                    .or_else(|_| a.get::<String>("product"))
                    .unwrap_or_else(|_| "RTL-SDR".into());
                let serial = a.get::<String>("serial").unwrap_or_default();
                (
                    a.to_string(),
                    if serial.is_empty() { label } else { format!("{label} · {serial}") },
                )
            })
            .collect()
    }

    /// Open the first RTL-SDR (or the device matching `args`, e.g.
    /// `"driver=rtlsdr,rtl=0"`), tune to `center_freq` Hz at `sample_rate`
    /// samples/sec, and set `gain` dB (or enable AGC when `gain` is None).
    pub fn open(
        args: &str,
        center_freq: f64,
        sample_rate: f64,
        gain: Option<f64>,
    ) -> Result<Self, SourceError> {
        let map = |e: seify::Error| SourceError::Unsupported(format!("seify: {e:?}"));
        let dev = DynDevice::from_args(args).map_err(map)?;
        let rx0 = dev.rx(0).map_err(map)?;
        rx0.sample_rate().set(sample_rate).map_err(map)?;
        rx0.frequency().set(center_freq).map_err(map)?;
        match gain {
            Some(g) => {
                // Best-effort disable AGC, then set manual gain.
                let _ = rx0.agc().disable();
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
        })
    }
}

impl SdrSource for RtlSdrSource {
    fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    fn center_freq(&self) -> f64 {
        self.center_freq
    }

    fn read(&mut self, buf: &mut [f32]) -> Result<usize, SourceError> {
        let pairs = buf.len() / 2;
        if self.scratch.len() < pairs {
            self.scratch.resize(pairs, Complex32::new(0.0, 0.0));
        }
        let n = self
            .rx
            .read(&mut [&mut self.scratch[..pairs]], 1_000_000)
            .map_err(|e| SourceError::Unsupported(format!("seify read: {e:?}")))?;
        for (i, c) in self.scratch[..n].iter().enumerate() {
            buf[i * 2] = c.re;
            buf[i * 2 + 1] = c.im;
        }
        Ok(n * 2)
    }
}

impl Drop for RtlSdrSource {
    /// Stop the async read before the device closes: librtlsdr closed while
    /// a transfer is in flight is a known crash, and this is exactly the
    /// order a radio switch (RTL-SDR → Airspy) exercises.
    fn drop(&mut self) {
        let _ = self.rx.deactivate();
    }
}
