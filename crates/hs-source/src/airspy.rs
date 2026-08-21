//! Live Airspy R2 capture over `libairspy` directly. Behind the `airspy`
//! feature so the core build stays pure-Rust and libusb-free.
//!
//! Seify has no Airspy backend, and SoapySDR would add a plugin layer for
//! one device — so this is a thin, deliberately minimal FFI over the handful
//! of `libairspy` calls the receiver needs. The Airspy is the device that
//! matters for the thesis: an RTL-SDR cannot span a SAFE-T simulcast site's
//! 4.8 MHz in one capture; an R2 at 10 MSPS can.
//!
//! ## Firmware limits (R2, NOS v1.0.0-rc10, 2016)
//!
//! Proven off-air (see `tools/install-mac.sh` and `results/baselines.md`):
//! * **INT16_IQ is the sample type that streams reliably.** Asking this
//!   firmware for float32 hangs it. Samples are 12-bit data in int16, scaled
//!   here by 1/32768 exactly as the `.cs16` file loader does, so a live run
//!   and a replayed `airspy_rx -t 2` capture are numerically identical.
//! * **Setting any gain wedges USB streaming** until the board is replugged.
//!   This source therefore makes *no* gain or AGC calls and runs the
//!   firmware's defaults — which decoded a control channel at its full TSBK
//!   rate. A requested gain is reported back as ignored rather than applied.
//! * Supported rates are exactly those the board advertises (10 and 2.5 MSPS
//!   on an R2). Neither divides by 4800; the caller normalizes downstream
//!   (see `hs_core::stream::Normalized`).
//!
//! ## Threading
//!
//! `libairspy` delivers blocks on its own USB thread through a C callback.
//! The callback converts and `try_send`s each block into a bounded channel
//! and never blocks: if the decoder falls behind, the block is dropped and
//! counted rather than letting the device's own buffer overflow (the same
//! policy as the trunk follower's reader thread). Device-side drops the
//! firmware reports are counted separately.

use crate::{SdrSource, SourceError};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;

#[repr(C)]
struct AirspyDevice {
    _private: [u8; 0],
}

/// `airspy_transfer` from `airspy.h`, field-for-field.
#[repr(C)]
struct AirspyTransfer {
    device: *mut AirspyDevice,
    ctx: *mut c_void,
    samples: *mut c_void,
    sample_count: i32,
    dropped_samples: u64,
    sample_type: i32,
}

const AIRSPY_SUCCESS: i32 = 0;
const AIRSPY_SAMPLE_INT16_IQ: i32 = 2;

type SampleBlockCb = unsafe extern "C" fn(*mut AirspyTransfer) -> i32;

extern "C" {
    fn airspy_list_devices(serials: *mut u64, count: i32) -> i32;
    fn airspy_open(device: *mut *mut AirspyDevice) -> i32;
    fn airspy_open_sn(device: *mut *mut AirspyDevice, serial: u64) -> i32;
    fn airspy_close(device: *mut AirspyDevice) -> i32;
    fn airspy_get_samplerates(device: *mut AirspyDevice, buffer: *mut u32, len: u32) -> i32;
    fn airspy_set_samplerate(device: *mut AirspyDevice, samplerate: u32) -> i32;
    fn airspy_set_sample_type(device: *mut AirspyDevice, sample_type: i32) -> i32;
    fn airspy_set_freq(device: *mut AirspyDevice, freq_hz: u32) -> i32;
    fn airspy_start_rx(device: *mut AirspyDevice, cb: SampleBlockCb, ctx: *mut c_void) -> i32;
    fn airspy_stop_rx(device: *mut AirspyDevice) -> i32;
}

/// Shared between the USB callback and the reader.
struct Shared {
    tx: SyncSender<Vec<f32>>,
    /// Blocks this side dropped because the decoder was behind.
    queue_drops: AtomicU64,
    /// Samples the firmware reported dropping (USB starvation on its side).
    device_drops: AtomicU64,
}

unsafe extern "C" fn on_block(t: *mut AirspyTransfer) -> i32 {
    // SAFETY: libairspy hands us a valid transfer whose `ctx` is the
    // `Arc<Shared>` pointer we passed to `airspy_start_rx`, which outlives
    // streaming (see `Drop`).
    let t = &*t;
    let shared = &*(t.ctx as *const Shared);
    if t.dropped_samples > 0 {
        shared
            .device_drops
            .fetch_add(t.dropped_samples, Ordering::Relaxed);
    }
    if t.sample_type != AIRSPY_SAMPLE_INT16_IQ || t.sample_count <= 0 {
        return 0;
    }
    // `sample_count` counts complex samples; INT16_IQ lays them out as
    // interleaved I,Q int16 pairs.
    let n = t.sample_count as usize;
    let raw = std::slice::from_raw_parts(t.samples as *const i16, n * 2);
    let block: Vec<f32> = raw.iter().map(|&s| s as f32 / 32768.0).collect();
    match shared.tx.try_send(block) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            shared.queue_drops.fetch_add(1, Ordering::Relaxed);
        }
        // Reader gone: returning non-zero stops streaming.
        Err(TrySendError::Disconnected(_)) => return 1,
    }
    0
}

/// A live Airspy source. Owns the device and its RX stream.
pub struct AirspySource {
    dev: *mut AirspyDevice,
    rx: Receiver<Vec<f32>>,
    shared: Arc<Shared>,
    sample_rate: f64,
    center_freq: f64,
    /// Leftover from a block the caller's buffer couldn't hold.
    pending: Vec<f32>,
    pending_pos: usize,
    gain_ignored: Option<f64>,
}

// SAFETY: the device pointer is only used from the owning thread for
// stop/close; libairspy's own thread drives the callback, which touches only
// the `Shared` (Sync) state.
unsafe impl Send for AirspySource {}

impl AirspySource {
    /// Serial numbers of the attached Airspy boards.
    pub fn list() -> Vec<u64> {
        let mut serials = [0u64; 16];
        // SAFETY: buffer and count match.
        let n = unsafe { airspy_list_devices(serials.as_mut_ptr(), serials.len() as i32) };
        if n <= 0 {
            return Vec::new();
        }
        serials[..n as usize].to_vec()
    }

    /// Open an Airspy (the one with `serial`, or the first found), tune to
    /// `center_freq` Hz and stream INT16 IQ at `sample_rate` — which must be
    /// one of the rates the board advertises (10 or 2.5 MSPS on an R2).
    /// `gain` is accepted for interface parity but **not applied** (see the
    /// module notes); `gain_ignored()` reports it so the caller can say so.
    pub fn open(
        serial: Option<u64>,
        center_freq: f64,
        sample_rate: f64,
        gain: Option<f64>,
    ) -> Result<Self, SourceError> {
        let fail = |what: &str, code: i32| {
            SourceError::Unsupported(format!("libairspy {what} failed ({code})"))
        };
        let mut dev: *mut AirspyDevice = std::ptr::null_mut();
        // SAFETY: plain FFI calls with valid out-pointers; every failure path
        // closes the device again.
        unsafe {
            let r = match serial {
                Some(sn) => airspy_open_sn(&mut dev, sn),
                None => airspy_open(&mut dev),
            };
            if r != AIRSPY_SUCCESS || dev.is_null() {
                return Err(SourceError::Unsupported(format!(
                    "no Airspy found ({r}) — is it plugged in, and not held by another program?"
                )));
            }
            let close = |dev: *mut AirspyDevice| {
                airspy_close(dev);
            };

            // Validate the rate against what the board offers, so a wrong
            // --rate is a clear error instead of a silent firmware default.
            let mut count = [0u32; 1];
            if airspy_get_samplerates(dev, count.as_mut_ptr(), 0) == AIRSPY_SUCCESS && count[0] > 0
            {
                let mut rates = vec![0u32; count[0] as usize];
                if airspy_get_samplerates(dev, rates.as_mut_ptr(), count[0]) == AIRSPY_SUCCESS
                    && !rates.iter().any(|&r| r as f64 == sample_rate)
                {
                    close(dev);
                    return Err(SourceError::Unsupported(format!(
                        "Airspy does not support {} Hz; it offers {}",
                        sample_rate as u64,
                        rates
                            .iter()
                            .map(|r| format!("{r}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                }
            }
            let r = airspy_set_sample_type(dev, AIRSPY_SAMPLE_INT16_IQ);
            if r != AIRSPY_SUCCESS {
                close(dev);
                return Err(fail("set_sample_type", r));
            }
            let r = airspy_set_samplerate(dev, sample_rate as u32);
            if r != AIRSPY_SUCCESS {
                close(dev);
                return Err(fail("set_samplerate", r));
            }
            let r = airspy_set_freq(dev, center_freq as u32);
            if r != AIRSPY_SUCCESS {
                close(dev);
                return Err(fail("set_freq", r));
            }
            // Deliberately no gain / AGC calls — see the module notes.

            // ~2 s of queue at 2.5 MSPS (blocks are 65536 complex samples).
            let (tx, rx) = sync_channel::<Vec<f32>>(96);
            let shared = Arc::new(Shared {
                tx,
                queue_drops: AtomicU64::new(0),
                device_drops: AtomicU64::new(0),
            });
            let ctx = Arc::as_ptr(&shared) as *mut c_void;
            let r = airspy_start_rx(dev, on_block, ctx);
            if r != AIRSPY_SUCCESS {
                close(dev);
                return Err(fail("start_rx", r));
            }
            Ok(Self {
                dev,
                rx,
                shared,
                sample_rate,
                center_freq,
                pending: Vec::new(),
                pending_pos: 0,
                gain_ignored: gain,
            })
        }
    }

    /// The gain the caller asked for, which this firmware cannot be given.
    pub fn gain_ignored(&self) -> Option<f64> {
        self.gain_ignored
    }

    /// Blocks dropped here because the consumer fell behind.
    pub fn queue_drops(&self) -> u64 {
        self.shared.queue_drops.load(Ordering::Relaxed)
    }

    /// Samples the device reported dropping on its side of the USB link.
    pub fn device_drops(&self) -> u64 {
        self.shared.device_drops.load(Ordering::Relaxed)
    }
}

impl SdrSource for AirspySource {
    fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    fn center_freq(&self) -> f64 {
        self.center_freq
    }

    fn dropped(&self) -> u64 {
        self.queue_drops() + self.device_drops()
    }

    fn read(&mut self, buf: &mut [f32]) -> Result<usize, SourceError> {
        if self.pending_pos >= self.pending.len() {
            self.pending = match self.rx.recv() {
                Ok(b) => b,
                Err(_) => return Err(SourceError::Eof),
            };
            self.pending_pos = 0;
        }
        let avail = &self.pending[self.pending_pos..];
        // Keep I/Q pairs intact.
        let n = avail.len().min(buf.len() & !1);
        buf[..n].copy_from_slice(&avail[..n]);
        self.pending_pos += n;
        Ok(n)
    }
}

impl Drop for AirspySource {
    fn drop(&mut self) {
        // SAFETY: stop streaming first so the callback can no longer run,
        // then close; `shared` (the callback's ctx) is dropped only after
        // both return, when the struct's fields are dropped.
        unsafe {
            airspy_stop_rx(self.dev);
            airspy_close(self.dev);
        }
    }
}
