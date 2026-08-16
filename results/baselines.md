# Benchmark baselines

Per the design doc (§3), decode quality is a measured number, not a vibe.
This file records what the current code actually does. Regenerate the
synthetic table with `cargo run -p hs-bench`.

## Status of the Phase 1 gate

**Not passed** — and expected not to be, at this stage. The gate is
"measurably lower BER and sync-loss than SDRTrunk on real simulcast
recordings." That requires (a) the field-IQ corpus, which is not yet
captured, and (b) the complex fractionally-spaced equalizer before
differential detection. Both are the project's core remaining work.

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
the two-ray case, not yet wired into the CQPSK front end). That front end —
a coherent LSM/CQPSK demodulator with the equalizer ahead of the diff
detector — is the next major DSP milestone.

## External-decoder baselines (to be filled during Phase 0)

Once the SAFE-T IQ corpus is captured, run the same recordings through
SDRTrunk (nightly), OP25, and GopherTrunk and record their
sync-loss / BER / TSBK-decode / voice-FER numbers here as the comparison
baseline. No numbers yet — no corpus yet.

| Decoder | Recording | Sync-loss | Pre-FEC BER | TSBK rate | Voice FER |
|---------|-----------|-----------|-------------|-----------|-----------|
| _pending_ | | | | | |
