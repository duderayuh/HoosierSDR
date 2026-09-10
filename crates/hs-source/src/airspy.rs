//! Live Airspy R2 capture over `libairspy` directly. Behind the `airspy`
//! feature so the core build stays pure-Rust and libusb-free.
//!
//! Seify has no Airspy backend, and SoapySDR would add a plugin layer for
//! one device — so this is a thin, deliberately minimal FFI over the handful
//! of `libairspy` calls the receiver needs. The Airspy is the device that
//! matters for the thesis: an RTL-SDR cannot span a statewide simulcast site's
//! 4.8 MHz in one capture; an R2 at 10 MSPS can.
//!
//! ## Firmware limits (R2, NOS v1.0.0-rc10, 2016)
//!
//! Proven off-air (see `tools/install-mac.sh` and `results/baselines.md`):
//! * **INT16_IQ is the sample type that streams reliably.** Asking this
//!   firmware for float32 hangs it. Samples are 12-bit data in int16, scaled
//!   here by 1/32768 exactly as the `.cs16` file loader does, so a live run
//!   and a replayed `airspy_rx -t 2` capture are numerically identical.
//! * **Gain is applied at open, before `airspy_start_rx`** — the same order
//!   `airspy_rx` itself uses. This module used to claim "setting any gain
//!   wedges USB streaming until the board is replugged" and made no gain
//!   calls at all, running whatever the firmware defaults to (measured
//!   2026-09-03 on this exact board/firmware/serial at idle: RMS ≈ 0.0008,
//!   too low to detect the local control channel at all — see
//!   `results/baselines.md`). That claim does not reproduce on today's
//!   libairspy (1.0.10): `airspy_rx -l 14 -m 12 -v 12` captured cleanly (RMS
//!   0.0008 → 0.122, ~43 dB more) and `airspy_info` still saw the device
//!   immediately afterward, no replug needed. Gain-at-open now defaults to
//!   [`DEFAULT_SENSITIVITY_GAIN`], tuned for weak-signal reception, and is
//!   still fully overridable via [`GainSetting`]. A *mid-stream* gain change
//!   (the [`GainHandle`] path `read()` applies on the fly) was not
//!   specifically re-tested and stays exactly as before — opt-in, used only
//!   when a caller asks for a live gain change after the radio is already
//!   streaming.
//! * Supported rates are exactly those the board advertises (10 and 2.5 MSPS
//!   on an R2). Neither divides by 4800; the caller normalizes downstream
//!   (see `hs_core::stream::Normalized`).
//!
//! ## Threading
//!
//! `libairspy` delivers blocks on its own consumer thread through a C
//! callback. The callback converts and `try_send`s each block into a bounded
//! channel and never blocks: if the reader falls behind, the block is dropped
//! and counted rather than letting the device's own buffer overflow (the same
//! policy as the trunk follower's reader thread).
//!
//! That consumer thread is the fragile link in the capture path. `libairspy`
//! keeps only 8 USB buffers (65536 complex samples each — ~52 ms of air at
//! 10 MSPS) between its USB transfer callback and this callback; when the
//! thread is late collecting them, the USB side discards whole buffers and
//! reports them on the next transfer as `dropped_samples`. Everything
//! downstream has seconds of queue; this stage has tens of milliseconds, so
//! a busy machine (transcription, a browser) starves it first. Two
//! mitigations: the first callback raises its own thread to real-time
//! round-robin priority (best effort — `libairspy` does this itself on
//! Windows only), and the drops it reports are counted in *blocks*, the unit
//! every other drop counter in the capture path uses, so a lost buffer reads
//! as 1, not 65536.

use crate::{FreqHandle, GainHandle, GainSetting, SdrSource, SourceError};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    fn airspy_set_lna_gain(device: *mut AirspyDevice, value: u8) -> i32;
    fn airspy_set_mixer_gain(device: *mut AirspyDevice, value: u8) -> i32;
    fn airspy_set_vga_gain(device: *mut AirspyDevice, value: u8) -> i32;
    fn airspy_set_lna_agc(device: *mut AirspyDevice, value: u8) -> i32;
    fn airspy_set_mixer_agc(device: *mut AirspyDevice, value: u8) -> i32;
    fn airspy_set_linearity_gain(device: *mut AirspyDevice, value: u8) -> i32;
    fn airspy_set_sensitivity_gain(device: *mut AirspyDevice, value: u8) -> i32;
}

/// Sensitivity-gain level (0–21) applied by default when the caller doesn't
/// request a specific gain. Sensitivity favours front-end (LNA) gain over
/// linearity, which is the right tradeoff for a weak, already-band-limited
/// P25 signal rather than a strong nearby blocker (this is what SDRTrunk's
/// Airspy "sensitivity" control does too). Picked from a real off-air
/// capture of the local site (see `results/baselines.md`); raise it further
/// if a site is still too weak to detect, or drop to `AirspyLinearity`/
/// `AirspyManual` if a strong nearby signal starts clipping the ADC.
pub const DEFAULT_SENSITIVITY_GAIN: u8 = 15;

/// Apply one gain setting to an open device. Called from `open()` before
/// `airspy_start_rx` (see the module notes — this firmware takes gain calls
/// fine at that point) and from the live [`GainHandle`] path while already
/// streaming.
///
/// # Safety
/// `dev` must be an open device.
unsafe fn apply_airspy_gain(dev: *mut AirspyDevice, g: &GainSetting) -> Result<(), SourceError> {
    let check = |what: &str, r: i32| {
        if r == AIRSPY_SUCCESS {
            Ok(())
        } else {
            Err(SourceError::Unsupported(format!(
                "airspy {what} failed ({r})"
            )))
        }
    };
    match g {
        GainSetting::AirspyLinearity(v) => check(
            "linearity gain",
            airspy_set_linearity_gain(dev, (*v).min(21)),
        ),
        GainSetting::AirspySensitivity(v) => check(
            "sensitivity gain",
            airspy_set_sensitivity_gain(dev, (*v).min(21)),
        ),
        GainSetting::AirspyManual {
            lna,
            mixer,
            vga,
            lna_agc,
            mixer_agc,
        } => {
            check("lna agc", airspy_set_lna_agc(dev, u8::from(*lna_agc)))?;
            check("mixer agc", airspy_set_mixer_agc(dev, u8::from(*mixer_agc)))?;
            if !lna_agc {
                check("lna gain", airspy_set_lna_gain(dev, (*lna).min(14)))?;
            }
            if !mixer_agc {
                check("mixer gain", airspy_set_mixer_gain(dev, (*mixer).min(15)))?;
            }
            check("vga gain", airspy_set_vga_gain(dev, (*vga).min(15)))
        }
        GainSetting::Agc => {
            check("lna agc", airspy_set_lna_agc(dev, 1))?;
            check("mixer agc", airspy_set_mixer_agc(dev, 1))
        }
        GainSetting::Manual(_) => Ok(()),
    }
}

/// Shared between the USB callback and the reader.
struct Shared {
    tx: SyncSender<Vec<f32>>,
    /// Blocks this side dropped because the reader was behind.
    queue_drops: AtomicU64,
    /// USB buffers `libairspy` discarded because the callback thread was late
    /// collecting them (see the module notes), counted in blocks of one
    /// buffer — the same 65536-sample unit as `queue_drops`.
    device_drops: AtomicU64,
    /// Whether the callback thread's priority has been raised yet (done once,
    /// from the first callback, since only that thread can do it).
    boosted: AtomicBool,
}

/// `libairspy` reports a drop as `dropped_buffers * sample_count` — the
/// whole USB buffers it discarded, in samples — so the buffer count is that
/// quotient. Counted this way the figure is in the same unit as every other
/// drop counter in the capture path (one block ≈ 6.5 ms at 10 MSPS) instead
/// of 65536 times larger.
fn device_drop_blocks(dropped_samples: u64, sample_count: i32) -> u64 {
    if sample_count <= 0 {
        return 0;
    }
    dropped_samples.div_ceil(sample_count as u64)
}

/// Raise the calling thread to real-time round-robin priority, returning the
/// priority set. Best effort: on macOS this is allowed for any process (it
/// is how audio I/O threads keep up); on Linux it needs `CAP_SYS_NICE` or an
/// rtprio limit and otherwise fails with `EPERM`, which is fine — the
/// capture just stays as sensitive to load as it always was.
#[cfg(unix)]
fn boost_current_thread() -> Result<i32, i32> {
    // Hand-declared rather than via the `libc` crate: `build.rs` promises
    // this feature adds no crate dependencies. Layouts checked against
    // glibc/musl and macOS `<sched.h>`; `SCHED_RR` is 2 on all three.
    const SCHED_RR: i32 = 2;
    #[cfg(target_os = "macos")]
    type PthreadT = *mut c_void;
    #[cfg(not(target_os = "macos"))]
    type PthreadT = usize;
    #[repr(C)]
    struct SchedParam {
        sched_priority: i32,
        #[cfg(target_os = "macos")]
        _opaque: [u8; 4],
    }
    extern "C" {
        fn pthread_self() -> PthreadT;
        fn pthread_setschedparam(thread: PthreadT, policy: i32, param: *const SchedParam) -> i32;
        fn sched_get_priority_max(policy: i32) -> i32;
        fn sched_get_priority_min(policy: i32) -> i32;
    }
    // SAFETY: plain POSIX calls on the calling thread with a fully
    // initialised, correctly laid-out parameter block.
    unsafe {
        let (min, max) = (
            sched_get_priority_min(SCHED_RR),
            sched_get_priority_max(SCHED_RR),
        );
        if max < 0 || min < 0 {
            return Err(max.min(min));
        }
        // macOS: the top of the round-robin band (47) sits above every normal
        // thread (31) and below the kernel. Linux: the band runs to 99, where
        // the kernel's own real-time threads live; stay modest.
        let want = if cfg!(target_os = "macos") {
            max
        } else {
            50.clamp(min, max)
        };
        let param = SchedParam {
            sched_priority: want,
            #[cfg(target_os = "macos")]
            _opaque: [0; 4],
        };
        match pthread_setschedparam(pthread_self(), SCHED_RR, &param) {
            0 => Ok(want),
            e => Err(e),
        }
    }
}

#[cfg(not(unix))]
fn boost_current_thread() -> Result<i32, i32> {
    Err(-1)
}

unsafe extern "C" fn on_block(t: *mut AirspyTransfer) -> i32 {
    // SAFETY: libairspy hands us a valid transfer whose `ctx` is the
    // `Arc<Shared>` pointer we passed to `airspy_start_rx`, which outlives
    // streaming (see `Drop`).
    let t = &*t;
    let shared = &*(t.ctx as *const Shared);
    if !shared.boosted.swap(true, Ordering::Relaxed) {
        if let Err(e) = boost_current_thread() {
            eprintln!(
                "airspy: could not raise the USB thread's priority (error {e}); \
                 stream drops under CPU load are more likely"
            );
        }
    }
    if t.dropped_samples > 0 {
        shared.device_drops.fetch_add(
            device_drop_blocks(t.dropped_samples, t.sample_count),
            Ordering::Relaxed,
        );
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
    applied_gain: GainSetting,
    gain: GainHandle,
    freq: FreqHandle,
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
    /// `gain` sets the sensitivity-gain level (0–21, clamped) at open, before
    /// streaming starts; `None` defaults to [`DEFAULT_SENSITIVITY_GAIN`].
    /// `applied_gain()` reports what was actually set. For finer control
    /// (linearity gain, or hand-set LNA/mixer/VGA) use [`Self::set_gain`] or
    /// a live [`GainHandle`] with the richer [`GainSetting`] variants.
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
            // Gain at open, before streaming starts — see the module notes on
            // why this is safe on this firmware despite the historical claim
            // to the contrary.
            let level = gain
                .map(|g| g.clamp(0.0, 21.0).round() as u8)
                .unwrap_or(DEFAULT_SENSITIVITY_GAIN);
            let applied_gain = GainSetting::AirspySensitivity(level);
            if let Err(e) = apply_airspy_gain(dev, &applied_gain) {
                close(dev);
                return Err(e);
            }

            // ~2.5 s of queue at 2.5 MSPS, ~0.6 s at 10 (blocks are 65536
            // complex samples).
            let (tx, rx) = sync_channel::<Vec<f32>>(96);
            let shared = Arc::new(Shared {
                tx,
                queue_drops: AtomicU64::new(0),
                device_drops: AtomicU64::new(0),
                boosted: AtomicBool::new(false),
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
                applied_gain,
                gain: GainHandle::default(),
                freq: FreqHandle::default(),
            })
        }
    }

    /// The gain setting actually applied at open (see `open()`).
    pub fn applied_gain(&self) -> &GainSetting {
        &self.applied_gain
    }

    /// Apply a gain setting now (the radio may be streaming). Opt-in: see
    /// the module notes on firmware that hangs.
    pub fn set_gain(&mut self, g: &GainSetting) -> Result<(), SourceError> {
        // SAFETY: `dev` is open for the life of `self`.
        unsafe { apply_airspy_gain(self.dev, g) }
    }

    /// A handle that changes this radio's gain from another thread; the
    /// request is applied on the next `read`.
    pub fn gain_handle(&self) -> GainHandle {
        self.gain.clone()
    }

    /// A handle that retunes this radio from another thread; the request is
    /// applied on the next `read` (on the reader's own thread). The dual-SDR
    /// hopper writes the next voice-channel frequency here.
    pub fn freq_handle(&self) -> FreqHandle {
        self.freq.clone()
    }

    /// Blocks dropped here because the consumer fell behind.
    pub fn queue_drops(&self) -> u64 {
        self.shared.queue_drops.load(Ordering::Relaxed)
    }

    /// USB buffers `libairspy` discarded because its callback thread was late
    /// collecting them, in blocks (one buffer = one block of 65536 complex
    /// samples, the same unit as [`Self::queue_drops`]).
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

    fn freq_handle(&self) -> FreqHandle {
        self.freq.clone()
    }

    fn dropped(&self) -> u64 {
        self.queue_drops() + self.device_drops()
    }

    fn read(&mut self, buf: &mut [f32]) -> Result<usize, SourceError> {
        if let Some(g) = self.gain.take() {
            if let Err(e) = self.set_gain(&g) {
                eprintln!("airspy gain {g:?}: {e:?}");
            }
        }
        if let Some(hz) = self.freq.take() {
            // SAFETY: `dev` is open for the life of `self`; this runs on the
            // reader's own thread, same as the gain path.
            let r = unsafe { airspy_set_freq(self.dev, hz as u32) };
            if r == AIRSPY_SUCCESS {
                self.center_freq = hz;
            } else {
                eprintln!("airspy retune to {hz} failed ({r})");
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The two figures a user saw as "dropped 393216 block(s)" and "dropped
    /// 2162688 block(s)": libairspy's `dropped_samples` for 6 and 33 lost
    /// USB buffers of 65536 samples. Counted in blocks they are 6 and 33.
    #[test]
    fn device_drops_count_buffers_not_samples() {
        assert_eq!(device_drop_blocks(393_216, 65_536), 6);
        assert_eq!(device_drop_blocks(2_162_688, 65_536), 33);
        assert_eq!(device_drop_blocks(0, 65_536), 0);
        // Defensive: a partial figure still counts as a lost buffer, and a
        // transfer with no samples cannot divide by zero.
        assert_eq!(device_drop_blocks(1, 65_536), 1);
        assert_eq!(device_drop_blocks(65_536, 0), 0);
    }

    /// The callback itself, driven with a fake transfer: device drops land
    /// in blocks, the samples still reach the queue, and a full queue counts
    /// one block per lost transfer. No radio needed.
    #[test]
    fn callback_counts_in_blocks() {
        let (tx, rx) = sync_channel::<Vec<f32>>(1);
        let shared = Arc::new(Shared {
            tx,
            queue_drops: AtomicU64::new(0),
            device_drops: AtomicU64::new(0),
            // Skip the priority change: this is the test runner's thread.
            boosted: AtomicBool::new(true),
        });
        let mut samples = vec![0i16; 65_536 * 2];
        samples[0] = 16_384;
        let mut t = AirspyTransfer {
            device: std::ptr::null_mut(),
            ctx: Arc::as_ptr(&shared) as *mut c_void,
            samples: samples.as_mut_ptr() as *mut c_void,
            sample_count: 65_536,
            dropped_samples: 2 * 65_536,
            sample_type: AIRSPY_SAMPLE_INT16_IQ,
        };
        // SAFETY: `t` is a fully initialised transfer whose `ctx` is a live
        // `Shared`, exactly what libairspy would hand over.
        assert_eq!(unsafe { on_block(&mut t) }, 0);
        assert_eq!(shared.device_drops.load(Ordering::Relaxed), 2);
        assert_eq!(shared.queue_drops.load(Ordering::Relaxed), 0);
        // Queue holds one block; the next one has nowhere to go.
        t.dropped_samples = 0;
        assert_eq!(unsafe { on_block(&mut t) }, 0);
        assert_eq!(shared.queue_drops.load(Ordering::Relaxed), 1);
        assert_eq!(shared.device_drops.load(Ordering::Relaxed), 2);
        let block = rx.recv().unwrap();
        assert_eq!(block.len(), 65_536 * 2);
        assert!((block[0] - 0.5).abs() < 1e-6);
        drop(rx);
        // Reader gone: the callback asks libairspy to stop.
        assert_eq!(unsafe { on_block(&mut t) }, 1);
    }
}
