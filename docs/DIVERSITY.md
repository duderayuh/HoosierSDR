# Spatial diversity (two-Airspy MRC) — design

Status: **in progress** (`#3`). First increment live: `hs_dsp::diversity::mrc_phase`.

## Problem

A metro-county dispatch site is a single-frequency simulcast P25 Phase I
(CQPSK) site. One antenna hears the sum of several tower copies; multipath
fades produce deep spectral nulls that the single-tap / CMA / DFE equalizers
cannot fully undo. The residual shows up as ~2.5 corrected data-errors per
IMBE frame and ~3% held voice frames — the "robotic" error-concealment blips.

Soft-decision FEC (PR #24) already makes those errors *honest* (no more silent
miscorrections). It does not *reduce* them. Two antennas with decorrelated
multipath do.

## Approach: maximal-ratio combining on soft symbols

Two Airspy R2s, each on its own antenna a fraction of a wavelength apart, tuned
to the same centre and rate. Each runs a **full independent CQPSK receiver**
(its own AGC, matched filter, Gardner timing loop, and carrier-bias removal).
Both emit a differential-phase estimate `dphi` per symbol; MRC combines them
weighted by per-branch SNR, then a single soft-slice feeds one framer/vocoder.

- The MRC point is `hs_core::decoder::ChannelDecoder::process`, CQPSK branch:
  today `push_phase` → `(dibit, dphi)` → `soft_slice_cqpsk(dphi)` → framer.
  Diversity replaces the single `dphi` with `mrc_phase(&[branch_a, branch_b])`.
- Combining in the **differential-phase domain** (after each branch removes its
  own carrier bias) is what makes two independent-LO Airspys combinable at all:
  the data is `Δφ`, identical on both, while each LO's absolute phase is not.

## Stage plan

1. **MRC core** (this increment): `hs_dsp::diversity::mrc_phase` — SNR-weighted
   phasor mean of any number of `(dphi, snr)`. Unit-tested (identical,
   strong-dominance, circular wrap, and an end-to-end slice-error reduction).
2. **SNR weight estimation**: per-branch weight ∝ `1 / CqpskReceiver::lock_error`
   (bounded, low when locked). Expose a stable `snr()` on the receiver.
3. **Time alignment**: two Airspys have independent clocks (≈±50 ppm) and start
   at different USB instants. Their symbol streams drift relative to each other,
   so the two `dphi` values must be aligned before combining. Plan: cross-
   correlate the two symbol streams on the 48-bit Frame Sync Word (or a long
   silence/idle edge) to find the sample offset, then resample one branch with a
   fractional-delay / polyphase tap to track the drift in a closed loop.
4. **Dual-source plumbing**: a `DualSource` (or two `Buffered` + a combining
   reader) opens both Airspys by serial (`AirspySource::open(serial)` /
   `AirspySource::list()`). Both feed `Normalized` → `ChannelDecoder`
   front-ends; the combining stage sits at stage-3's MRC point.
5. **Measure** against `captures/reference.cu8` (replayed
   twice with decorrelated offsets) and a live two-antenna capture.

## Hardware

Two Airspy R2s already connected (serials `0x637862DC2E43BCD7`,
`0x637862DC2F891FD7`, firmware rc10). Both open independently today; the
`libairspy` FFI (`hs_source::airspy`) is serial-addressable and thread-owns one
device per source — two concurrent `AirspySource`s are expected to coexist
(each drives its own USB thread).

## Open questions / risks

- **Alignment robustness**: clock drift means the offset is time-varying; a
  once-only cross-correlation is not enough. Needs a tracking loop (early-late
  on the combined symbol timing, or periodic re-correlation on sync words).
- **Sample-rate mismatch**: `10 MSPS` vs `2.5 MSPS` on the two boards would need
  both at the same rate (2.5 MSPS spans the single 12.5 kHz channel; 10 MSPS is
  for wideband trunk following). Diversity is per-channel, so 2.5 MSPS.
- **Phase ambiguity**: each branch independently resolves its own π/2 quarter-
  turn (via the derotator). MRC must happen *before* derotation, or carry the
  same rotation on both branches — combine `dphi`, then derotate once.