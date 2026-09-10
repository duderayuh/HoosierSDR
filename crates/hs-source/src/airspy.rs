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
//! `libairspy` delivers blocks on its own conversion thread through a C
//! callback. The callback converts and `try_send`s each block into a bounded
//! channel and never blocks: if the decoder falls behind, the block is
//! dropped and counted rather than letting the device's own buffer overflow
//! (the same policy as the trunk follower's reader thread).
//!
//! Upstream of that channel, `libairspy` keeps its own ring of only
//! `RAW_BUFFER_COUNT` (8) raw USB transfers between the libusb event thread
//! and the conversion thread that runs the 12-bit → int16 IQ converter and
//! then this callback. At 10 MSPS a transfer is 65536 complex samples
//! (~6.5 ms), so that ring holds ~52 ms: if the conversion thread is
//! descheduled longer than that — a whisper burst, a WebKit paint, anything
//! with more priority — libusb overwrites the oldest transfer and reports it
//! as `dropped_samples` on the next callback (a per-transfer delta, in
//! samples). Those are the "65536 stream drop(s)" a lightly loaded machine
//! still shows: one transfer, not 65536 blocks, and nothing to do with the
//! decoder's own queue. They are counted here in *blocks* as
//! [`AirspySource::device_drops`]. Since the pthread `libairspy` spawns runs
//! at default QoS, the first callback promotes it (macOS): user-interactive
//! QoS, then the Mach time-constraint (real-time) policy with one transfer
//! period as the deadline, which runs ahead of every timeshare thread
//! (user-interactive alone still lost 21 transfers in one ~190 ms gap). The
//! callback also fills recycled blocks rather than allocating 512 KB per
//! transfer, and when a transfer is lost it prints how long since the
//! callback last ran and how long its own body took, so a stall inside the
//! callback and a starved thread can be told apart.

use crate::{FreqHandle, GainHandle, GainSetting, SdrSource, SourceError};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Instant;

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
    /// Blocks this side dropped because the decoder was behind.
    queue_drops: AtomicU64,
    /// Blocks (USB transfers) `libairspy`'s own ring dropped because its
    /// conversion thread — the one that calls `on_block` — was starved.
    device_drops: AtomicU64,
    /// Whether `on_block` has already raised its thread's scheduling class.
    promoted: AtomicBool,
    /// Sample rate, for turning a transfer into a time.
    rate: f64,
    /// Blocks the reader has finished with, reused by `on_block` so the
    /// callback never asks the allocator for another 512 KB (a `vm_allocate`
    /// per transfer, contended with everything else in the process that maps
    /// or frees memory).
    pool: Mutex<Vec<Vec<f32>>>,
    /// Diagnostics for a starved callback: when it last ran (ns since
    /// `epoch`), how long the last body took and the worst body so far (µs).
    epoch: Instant,
    last_call_ns: AtomicU64,
    last_body_us: AtomicU64,
    max_body_us: AtomicU64,
}

/// Blocks the pool keeps at most (≈ the channel depth plus what the reader
/// holds); anything beyond is simply freed.
const POOL_MAX: usize = 128;

#[cfg(target_os = "macos")]
extern "C" {
    fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
}

/// `QOS_CLASS_USER_INTERACTIVE` from `<sys/qos.h>`.
#[cfg(target_os = "macos")]
const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;

#[cfg(target_os = "macos")]
#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

/// `thread_time_constraint_policy` from `<mach/thread_policy.h>`.
#[cfg(target_os = "macos")]
#[repr(C)]
struct ThreadTimeConstraintPolicy {
    period: u32,
    computation: u32,
    constraint: u32,
    preemptible: u32,
}

#[cfg(target_os = "macos")]
extern "C" {
    fn mach_thread_self() -> u32;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
    fn thread_policy_set(thread: u32, flavor: u32, policy: *const u32, count: u32) -> i32;
    fn mach_port_deallocate(task: u32, name: u32) -> i32;
    static mach_task_self_: u32;
}

#[cfg(target_os = "macos")]
const THREAD_TIME_CONSTRAINT_POLICY: u32 = 2;

/// Promote the calling thread (libairspy's conversion thread) so a
/// scheduling gap under ordinary desktop load stops costing a USB transfer.
/// Two steps, each best effort: user-interactive QoS (a better timeshare
/// class, P-core placement), then the Mach time-constraint policy CoreAudio
/// uses — a real-time band that runs ahead of every timeshare thread, with a
/// deadline of one transfer period. `period_ms` is how often a transfer
/// arrives.
fn promote_current_thread(period_ms: f64) {
    #[cfg(target_os = "macos")]
    // SAFETY: plain Mach/libc calls on the current thread; the policy struct
    // is passed by pointer with its exact word count, and the thread port
    // from `mach_thread_self` is released again.
    unsafe {
        let r = pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0);
        if r != 0 {
            eprintln!("airspy: could not raise conversion thread QoS ({r})");
        }
        let mut tb = MachTimebaseInfo { numer: 1, denom: 1 };
        mach_timebase_info(&mut tb);
        let abs = |ms: f64| (ms * 1e6 * tb.denom as f64 / tb.numer as f64) as u32;
        // Budget half the period for the converter plus this callback (they
        // use well under a fifth), and require the work to finish within the
        // period so the ring can never fill.
        let policy = ThreadTimeConstraintPolicy {
            period: abs(period_ms),
            computation: abs(period_ms * 0.5),
            constraint: abs(period_ms * 0.9),
            preemptible: 1,
        };
        let port = mach_thread_self();
        let r = thread_policy_set(
            port,
            THREAD_TIME_CONSTRAINT_POLICY,
            &policy as *const ThreadTimeConstraintPolicy as *const u32,
            (std::mem::size_of::<ThreadTimeConstraintPolicy>() / 4) as u32,
        );
        mach_port_deallocate(mach_task_self_, port);
        if r == 0 {
            eprintln!(
                "airspy: conversion thread promoted (user-interactive QoS + real-time policy, period {period_ms:.1} ms)"
            );
        } else {
            eprintln!("airspy: real-time policy refused ({r}); conversion thread stays at user-interactive QoS");
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = period_ms;
}

unsafe extern "C" fn on_block(t: *mut AirspyTransfer) -> i32 {
    // SAFETY: libairspy hands us a valid transfer whose `ctx` is the
    // `Arc<Shared>` pointer we passed to `airspy_start_rx`, which outlives
    // streaming (see `Drop`).
    let t = &*t;
    let shared = &*(t.ctx as *const Shared);
    let t0 = Instant::now();
    let block_ms = if t.sample_count > 0 && shared.rate > 0.0 {
        t.sample_count as f64 * 1e3 / shared.rate
    } else {
        6.5
    };
    if !shared.promoted.swap(true, Ordering::Relaxed) {
        promote_current_thread(block_ms);
    }
    // How long since we last ran: a gap longer than the driver's ring is the
    // starvation that loses transfers, and whether *our* body was the slow
    // part is the question the diagnostic answers.
    let now_ns = t0.duration_since(shared.epoch).as_nanos() as u64;
    let prev_ns = shared.last_call_ns.swap(now_ns, Ordering::Relaxed);
    // `dropped_samples` is transfers lost since the previous callback, times
    // this transfer's `sample_count`; count them as blocks, the unit every
    // other drop counter uses.
    if t.dropped_samples > 0 && t.sample_count > 0 {
        let lost = t.dropped_samples.div_ceil(t.sample_count as u64);
        shared.device_drops.fetch_add(lost, Ordering::Relaxed);
        let gap_ms = if prev_ns == 0 {
            0.0
        } else {
            (now_ns - prev_ns) as f64 / 1e6
        };
        eprintln!(
            "airspy: driver dropped {lost} transfer(s): {gap_ms:.1} ms since our callback last ran (ring holds ~{:.0} ms); our previous callback body took {} µs, worst so far {} µs",
            block_ms * 8.0,
            shared.last_body_us.load(Ordering::Relaxed),
            shared.max_body_us.load(Ordering::Relaxed)
        );
    }
    if t.sample_type != AIRSPY_SAMPLE_INT16_IQ || t.sample_count <= 0 {
        return 0;
    }
    // `sample_count` counts complex samples; INT16_IQ lays them out as
    // interleaved I,Q int16 pairs.
    let n = t.sample_count as usize;
    let raw = std::slice::from_raw_parts(t.samples as *const i16, n * 2);
    let mut block = shared
        .pool
        .lock()
        .map(|mut p| p.pop().unwrap_or_default())
        .unwrap_or_default();
    block.clear();
    block.reserve(n * 2);
    block.extend(raw.iter().map(|&s| s as f32 / 32768.0));
    let ret = match shared.tx.try_send(block) {
        Ok(()) => 0,
        Err(TrySendError::Full(block)) => {
            shared.queue_drops.fetch_add(1, Ordering::Relaxed);
            recycle(shared, block);
            0
        }
        // Reader gone: returning non-zero stops streaming.
        Err(TrySendError::Disconnected(_)) => 1,
    };
    let body_us = t0.elapsed().as_micros() as u64;
    shared.last_body_us.store(body_us, Ordering::Relaxed);
    shared.max_body_us.fetch_max(body_us, Ordering::Relaxed);
    ret
}

/// Hand a finished block back for `on_block` to fill again.
fn recycle(shared: &Shared, block: Vec<f32>) {
    if block.capacity() == 0 {
        return;
    }
    if let Ok(mut p) = shared.pool.lock() {
        if p.len() < POOL_MAX {
            p.push(block);
        }
    }
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

            // ~2 s of queue at 2.5 MSPS (blocks are 65536 complex samples).
            let (tx, rx) = sync_channel::<Vec<f32>>(96);
            let shared = Arc::new(Shared {
                tx,
                queue_drops: AtomicU64::new(0),
                device_drops: AtomicU64::new(0),
                promoted: AtomicBool::new(false),
                rate: sample_rate,
                pool: Mutex::new(Vec::new()),
                epoch: Instant::now(),
                last_call_ns: AtomicU64::new(0),
                last_body_us: AtomicU64::new(0),
                max_body_us: AtomicU64::new(0),
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

    /// Blocks `libairspy` dropped inside its own USB ring because its
    /// conversion thread was starved (see the module notes on threading).
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

    fn driver_dropped(&self) -> u64 {
        self.device_drops()
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
            let next = match self.rx.recv() {
                Ok(b) => b,
                Err(_) => return Err(SourceError::Eof),
            };
            recycle(&self.shared, std::mem::replace(&mut self.pending, next));
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
