# Benchmark baselines

Per the design doc (§3), decode quality is a measured number, not a vibe.
This file records what the current code actually does. Regenerate the
synthetic table with `cargo run -p hs-bench`.

## Status of the Phase 1 gate

**Not yet passed on real recordings** — but the core mechanism is now proven
in a controlled experiment (see "Thesis experiment" below). The full gate is
"measurably lower BER and sync-loss than SDRTrunk on real simulcast
recordings," which still requires (a) the field-IQ corpus, not yet captured,
and (b) wiring the proven complex equalizer behind live carrier/timing
recovery on the CQPSK front end. Those are the remaining integration steps.

## Thesis experiment (complex two-ray channel) — PASSES

`cargo test -p hs-dsp --test thesis_cqpsk` runs the project's central claim as
a controlled experiment: a CQPSK symbol stream through a complex two-ray
(simulcast-like) channel, decoded two ways.

| Decode path | Symbol error rate |
|-------------|:-----------------:|
| Differential detection first (what OP25 / trunk-recorder / SDRTrunk do) | **0.259** |
| Sync-trained equalizer **before** differential detection (HoosierSDR) | **0.000** |

Echo: 55% amplitude, π/4 phase offset. The equalizer is the complex T/2
`LmsFse`, trained on a 24-symbol known sync sequence, then frozen. This is the
whole thesis in one number: differential detection is a nonlinearity that
makes ISI unrecoverable, so removing it *before* that step is a categorical
win, not a marginal one. The complex echo here is exactly the class of
distortion the real post-discriminator equalizer *cannot* touch (next
section) — which is why the CQPSK front end is the path that matters.

## Synthetic self-benchmark (no external corpus)

`hs-bench` synthesizes a P25 control-channel + voice transmission
(IDEN_UP → group voice grant → one clear LDU), passes it through a two-ray
echo + AWGN channel, and decodes it twice: equalizer BYPASSED vs ENABLED.
Metrics are `syncs / grants / pcm-samples` (higher is better).

Latest run (echo = 20 samples @ 48 kHz ≈ 2 symbols, gain 0.45):

| Es/N0 | Bypass (sync/grant/pcm) | Equalized (sync/grant/pcm) |
|------:|:-----------------------:|:--------------------------:|
| 30 dB | 2 / 1 / 1440 | 2 / 0 / 1440 |
| 18 dB | 2 / 1 / 1440 | 2 / 0 / 1440 |
| 12 dB | 2 / 1 / 1440 | 2 / 0 / 1440 |
|  9 dB | 2 / 1 / 1440 | 2 / 0 / 1440 |
|  6 dB | 2 / 0 / 1440 | 2 / 0 / 1440 |

### How to read this

The echo is applied at **complex baseband, before the FM discriminator.**
The equalizer currently in the pipeline (`RealLmsEq`) is a **real,
symbol-domain LMS placed after the discriminator.** It therefore *cannot*
invert this class of distortion — the discriminator is a nonlinearity
between the multipath and the equalizer. The table confirms exactly that:

- Equalized decode is **non-harmful**: it preserves sync and voice
  (pcm = 1440 at every SNR), verified by a regression test.
- Equalized does **not beat** bypass: the trellis-coded grant, the most
  distortion-sensitive metric here, is lost under the equalizer's residual.

This is the design doc's thesis demonstrating itself. Passing the gate
requires the **complex fractionally-spaced equalizer before differential
detection** (`hs-dsp::equalizer::LmsFse`, implemented and unit-tested for
the two-ray case). The CQPSK front end it plugs into now exists — see below.

## CQPSK carrier + timing front end — WORKS on realistic IQ

`hs-dsp::cqpsk::CqpskReceiver` takes the symbol-level thesis off the bench and
onto a continuous, oversampled signal with the impairments a real tuner
delivers. `cargo test -p hs-dsp --test cqpsk_frontend` feeds it RRC-shaped
CQPSK through:

| Impairment | Recovered BER |
|------------|:-------------:|
| Carrier frequency offset (0.01 rad/sample) + phase offset + AWGN | **0.000** |
| 0.2% sample-clock skew + carrier offset + AWGN | **0.000** |

The chain is: RRC matched filter → complex Gardner timing recovery (NCO +
interpolator + PI loop) → differential detection with carrier-frequency
tracking. Two design facts worth recording:

- **No Costas loop.** P25 CQPSK is π/4-DQPSK, whose constellation is an
  8-point union of two QPSK grids; a plain QPSK Costas loop corrupts it. A
  static carrier *phase* offset is removed for free by differential detection,
  and the residual carrier *frequency* offset is tracked as a constant bias in
  the differential-phase domain (decision-directed, zero steady-state error).
  The recovered bias matches the injected offset to within 0.02 rad.
- **Equalizer placement next.** Wiring `LmsFse` into this front end ahead of
  the detector needs a **phase-blind (CMA)** equalizer, because the coherent
  FSW-trained FSE wants an absolute phase reference this differential front end
  deliberately never establishes. That CMA integration — combining the proven
  thesis with the working front end on one ISI+carrier+timing channel — is the
  next DSP milestone.

## External-decoder baselines (to be filled during Phase 0)

Once the SAFE-T IQ corpus is captured, run the same recordings through
SDRTrunk (nightly), OP25, and GopherTrunk and record their
sync-loss / BER / TSBK-decode / voice-FER numbers here as the comparison
baseline. No numbers yet — no corpus yet.

| Decoder | Recording | Sync-loss | Pre-FEC BER | TSBK rate | Voice FER |
|---------|-----------|-----------|-------------|-----------|-----------|
| _pending_ | | | | | |
