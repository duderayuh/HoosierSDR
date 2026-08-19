# Benchmark baselines

Per the design doc (§3), decode quality is a measured number, not a vibe.
This file records what the current code actually does. Regenerate the
synthetic table with `cargo run -p hs-bench`.

## Status of the Phase 1 gate

**Not yet passed on real recordings** — but the core mechanism is proven in a
controlled experiment (see "Thesis experiment" below), and the receiver now
decodes real off-air P25 (see "First field decode"). The full gate is
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

## First field decode — Marion County, 2026-08

The first real off-air capture decodes. An RTL-SDR recording made in Marion
County, Indiana (`rtl_sdr -f 858937500 -s 240000 -g 40`, 27.3 s, cu8) was
decoded end to end by `hoosier-sdr` with no offline preprocessing:

```sh
hoosier-sdr --rate 240000 --offset 50k --cqpsk capture.cf32
```

| Measure | Value |
|---|---|
| Modulation | **CQPSK / LSM** (simulcast) |
| NAC | **0x261**, on 149/149 decoded NIDs |
| Frame syncs | 151 in 27.3 s |
| Mean sync bit errors (of 48) | **0.07** — 143/151 with zero |
| NID BCH errors | mean 0.39; 130/149 with zero |
| DUIDs seen | TDULC 88, LDU2 30, LDU1 29, TDU 1, HDU 1 |
| Voice | 531 IMBE frames → 10.6 s of 8 kHz PCM |
| Grants | 0 — this is a **traffic channel**, not the control channel |

Two things this capture established, both now fixed in code:

1. **The tuned frequency was not the P25 channel.** 858.9375 MHz was picked
   from an `rtl_power` sweep on received power alone; the actual P25 carrier
   is 50 kHz up, at **858.9875 MHz**. Sweeping the capture across every
   12.5 kHz grid offset found it — 4th-power differential-phase concentration
   peaks sharply at +50.0 kHz (0.79, versus <0.10 everywhere else), which is
   an unambiguous π/4-DQPSK signature. The `--offset` downconverter exists so
   a wideband capture can be re-tuned in software rather than re-recorded.
2. **Native SDR rates were unusable.** The demodulators are tuned per symbol
   at ~10 samples/symbol; a 240 kHz capture is 50, which detunes the matched
   filter, timing loop and equalizer simultaneously. `hs-dsp::decimate` now
   resamples at the front of the chain.

### Equalizer A/B on this capture

| Path | Syncs | Mean sync bit errors | Voice frames |
|---|:--:|:--:|:--:|
| CMA equalizer before differential detection | 151 | **0.073** | 531 |
| Conventional detect-first (`--no-equalizer`) | 150 | 0.153 | 522 |

Read this honestly: the equalizer roughly halves the residual sync bit-error
rate and recovers nine more voice frames, but on a signal this strong
(~40 dB SNR, near-perfect decode either way) there is very little ISI to
remove and therefore very little to win. This capture does **not** test the
thesis — it validates the receiver. The thesis targets the degraded simulcast
regime, and confirming it needs captures where the conventional path actually
fails: weak signal, deep multipath, or a site with overlapping transmitters at
comparable strength.

### Equalizer A/B across a wideband capture — 8 simulcast channels, 2026-08-18

A second Marion County capture (`marion.cu8`, 2.4 Msps, ~5.7 s) widens the
sample from one channel to a whole band slice: `--scan` found **9 P25
channels across three NACs** (0x260, 0x261, 0x6B6) — 8 CQPSK/LSM simulcast
channels plus one marginal C4FM. Every CQPSK channel was decoded twice, with
the CMA equalizer in and bypassed:

```sh
hoosier-sdr --rate 2400000 --offset <off> --cqpsk [--no-equalizer] --log out.json marion.cu8
```

| Offset | NAC | Type | Syncs eq/no-eq | Sync bit-err eq/no-eq | Voice frames eq/no-eq |
|---:|:--:|:--|:--:|:--:|:--:|
| +125.0 kHz | 0x6B6 | voice | 85 / 88 | **0.65** / 1.07 | 72 / 72 |
| +550.0 kHz | 0x261 | control | 69 / 67 | **0.42** / 0.45 | — |
| +100.0 kHz | 0x261 | voice | 68 / 70 | **0.10** / 0.24 | 135 / 135 |
| −1175.0 kHz | 0x260 | voice | 63 / 63 | **0.17** / 0.57 | 126 / 126 |
| −675.0 kHz | 0x261 | voice | 40 / 40 | **0.10** / 0.17 | 135 / 135 |
| +250.0 kHz | 0x260 | voice | 32 / 32 | 0.31 / 0.28 | 270 / 279 |
| +275.0 kHz | 0x260 | voice | 32 / 32 | **0.00** / 0.19 | 279 / 279 |
| −125.0 kHz | 0x261 | voice | 32 / 32 | **0.03** / 0.09 | 279 / 279 |

(Offsets are relative to the capture centre; a separate control-channel
capture the same night, `live261.cu8`, gave the same picture: 74 vs 73 syncs,
identical 10 grants either way.)

The pattern is consistent with the single-channel A/B above, now across eight
independent simulcast channels: **the equalizer lowers residual sync bit
errors on 6 of 8 channels — often by 2–4×, twice to near zero — but decode
outcomes (syncs, NIDs, voice frames) are unchanged.** At this receive
location every channel is strong enough that the conventional path already
survives its errors; the equalizer is demonstrably cleaning the symbols, but
FEC was already absorbing the difference. The thesis-deciding capture is
still the one from a degraded location — deep simulcast overlap between
towers, where equal-strength multipath makes the detect-first path actually
drop frames.

## Soft-decision decoding — measured on the field capture

Hard slicing discards the demodulator's confidence: a C4FM symbol at +2.9 and
one at +2.05 both become the same bits, and every stage downstream treats them
as equal evidence. Carrying per-bit confidence into the frame-sync correlator
and the trellis Viterbi decoder recovers most of what the hard path was losing.

Same 27.3 s Marion County capture, same everything else:

| | hard decision | soft decision |
|---|:--:|:--:|
| Frame syncs | 151 | **161** |
| Voice frames | 531 | **585** |
| Decoded audio | 10.62 s | **11.70 s** |
| Missed LDU frames (gap analysis) | 7 | **1** |

**+1.08 s of recovered audio, and 6 of 7 dropped voice frames recovered.**

The new syncs are real, not false locks — three independent checks agree:
all 158 NIDs still decode to NAC 0x261 (a false sync yields a garbage NAC),
the NID BCH error rate is unchanged (0.39 → 0.41 mean, ~87% clean either way),
and LDU1/LDU2 gained *equally* (+3 each), which is the signature of genuine
voice frames since the two alternate through a call.

Mean sync bit-errors rises (0.073 → 0.385) and that is the mechanism working,
not a regression: the correlator is now *accepting* windows with more hard bit
errors, because the confidence pattern says those errors sit on bits the
demodulator never trusted.

### Coding gain in isolation

`cargo test -p hs-p25 --lib soft_decoding` runs 300 TSBK frames through a C4FM
symbol channel with Gaussian noise and decodes each twice from the *same*
received symbols:

| noise σ | hard | soft |
|---|:--:|:--:|
| 0.9 | 299/300 | 300/300 |
| 1.1 | 290/300 | 300/300 |
| 1.3 | 258/300 | 298/300 |
| 1.5 | 175/300 | **291/300** |

Soft decoding does not rescue a trellis stage that noise destroyed outright —
no decoder does. It wins on the many marginal frames where the confidence
pattern says which way to lean, which is exactly how coding gain works.

Voice FEC (Golay/Hamming on the IMBE frames) and the NID's BCH decoder are
still hard-decision; algebraic decoders need Chase-style soft decoding, which
is the next increment.

## Link Control and its Hamming code — derived from the air

Each LDU1 embeds a 72-bit Link Control Word naming the talkgroup and the
transmitting radio, so a traffic channel identifies its own call with no
control channel present. Cross-validated on the Marion County captures: the
control channel issued 12 grants onto 857.7625 MHz for talkgroup 10255, and
that traffic channel — decoded separately, at a different frequency, in a
different modulation — reports talkgroup 10255 in its own Link Control.

Each hexbit of that word is protected by Hamming(10,6,3). **The parity
equations were derived from real traffic rather than taken from a
specification.** Parity is a linear function of the data bits, so for each of
the four parity positions the 6-bit mask best predicting it was found by
exhaustive search over 744 hexbits captured off air:

| parity bit | data bits | fits |
|---|---|---:|
| 0 | d0 ⊕ d1 ⊕ d2 ⊕ d5 | 92.9% |
| 1 | d0 ⊕ d1 ⊕ d3 ⊕ d5 | 94.8% |
| 2 | d0 ⊕ d2 ⊕ d3 ⊕ d4 | 91.9% |
| 3 | d1 ⊕ d2 ⊕ d3 ⊕ d4 | 93.0% |

A wrong mask would fit near 50%; the residual few percent are the channel
errors the code exists to correct. The derivation is then checked rather than
trusted: the 64 codewords these masks generate have a minimum Hamming distance
of exactly **3**, which is what Hamming(10,6,3) must have and what a mistaken
derivation would not produce. That property is asserted in a test.

Measured on 744 hexbits from 31 link-control words:

| | count | share |
|---|---:|---:|
| already correct | 655 | 88.0% |
| corrected by Hamming | 75 | 10.1% |
| beyond correction | 14 | 1.9% |

| Words with all 12 data hexbits sound | |
|---|---:|
| without correction | 14 / 31 |
| **with Hamming** | **24 / 31** |

Words containing a hexbit beyond correction are refused. Allowing two such
hexbits through was tried, on the reasoning that the repetition check would
sort them out; it recovered no additional call and let corrupted words leaking
through as bogus vendor messages rise from 6 to 9, so the strict rule stands.

The remaining 7-in-31 need the outer RS(24,12,13) layer, which is not decoded
yet — and unlike the Hamming code its generator cannot be recovered by a linear
fit, so it needs the specification rather than more captures.

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
- **Equalizer placement.** Wiring the equalizer into this front end ahead of
  the detector needs a **phase-blind (CMA)** equalizer, because the coherent
  FSW-trained FSE wants an absolute phase reference this differential front end
  deliberately never establishes. That integration now exists — see below.

## Thesis on live-style IQ (ISI + carrier + timing) — PASSES

`cargo test -p hs-dsp --test cqpsk_frontend` (`thesis_on_live_iq_*`) runs the
whole story on one channel: a two-ray (simulcast) echo **plus** carrier
frequency offset, phase offset, sample-clock skew, and AWGN — then decodes it
two ways through the full `CqpskReceiver`.

| Front end | Recovered BER |
|-----------|:-------------:|
| Bare — differential detection first (OP25 / trunk-recorder / SDRTrunk) | **0.070** |
| CMA-equalized — `CmaEqualizer` **before** differential detection (HoosierSDR) | **0.000** |

This is the thesis and the front end combined: the phase-blind constant-modulus
equalizer (`hs-dsp::equalizer::CmaEqualizer`) opens the eye with no reference
symbol and no carrier lock — which is exactly what lets it sit ahead of the
differential detector on π/4-DQPSK — while the Gardner loop recovers timing and
the differential-domain tracker removes the carrier frequency offset. The
equalizer takes a 7% error rate to zero on a channel that carries the full set
of real-tuner impairments, not just clean symbols.

### CQPSK path decodes full P25 frames

The front end is now wired all the way through the protocol stack.
`ChannelDecoder::new_cqpsk()` runs IQ → carrier + timing recovery → CMA
equalizer → differential detection → **`hs-p25` framer → trunking → IMBE
voice**, the same back end the C4FM path uses. `cargo test -p hs-core --test
cqpsk_pipeline` synthesizes a P25 transmission as π/4-DQPSK IQ and decodes it:

- control channel → frame sync, NID (BCH, 0 errors), TSBK trellis+CRC →
  **voice grant resolved to 851.1375 MHz**
- LDU1 → **1440 PCM samples of IMBE audio**

`hoosier-sdr --cqpsk --demo` exercises the whole path from the CLI, and
`--cqpsk <capture.cf32>` decodes a real simulcast recording once you have one.

### What remains

This is validated on **synthetic** IQ end to end (both modulations now decode
real frames). The open items are live I/O and field validation, not DSP
theory: run live SDR capture (`hs-source` + Seify) into the decoder, and re-run
the whole thing on a captured SAFE-T corpus against SDRTrunk / OP25 to fill in
the external-baseline table below.

## External-decoder baselines — first numbers, Marion County control channel

First head-to-head, 2026-08-19. Input: the Marion County wideband capture
(`marion.cu8`, 2.4 Msps u8, 5.83 s), control channel at +550 kHz — NAC 0x261,
CQPSK/LSM simulcast (WACN BEE00, SYS 262, RFSS 1, Site 10, per GopherTrunk's
own site decode). Every decoder saw the same signal; where a decoder needed a
single channel, all got the *identical* file: the channel mixed to DC and
decimated to 48 ksps (windowed-sinc FIR, then fine-centered by a 4th-power
carrier estimate to −198 Hz residual). The metric is **CRC-passed TSBKs**,
the one number all three report with the same semantics (OP25 dumps TSBKs
only after `crc16` passes; HoosierSDR counts blocks after trellis+CRC;
GopherTrunk reports `tsbk decoded` net of `crc_failed`).

| Decoder | Input | Frame syncs | TSBKs (CRC-passed) | Voice grants |
|---|---|:--:|:--:|:--:|
| **SDRTrunk 0.6.1** (P25P1 CQPSK/LSM, recording-tuner playback) | 2.4M s16 wav | n/r | **~205** | n/r |
| **HoosierSDR** (native front end, wideband) | 2.4M cu8 | 69 | 152 | 21 |
| **HoosierSDR** | 48k channelized | 69 | 147 | 19 |
| OP25 boatbod `28f2c40` (2026-08-13), `-D cqpsk` | 48k channelized | n/r | 21 | 1 |
| GopherTrunk `09b0014` (2026-08-18), `-demod cqpsk` | 48k channelized | 8 | 17 | 7 |
| GopherTrunk, same, wideband `-auto-tune` | 2.4M cu8 | 4 | 12 | — |

**SDRTrunk wins this recording, and the loss is localized.** Its playback
looped the file (~2.4 passes, 484 TSBKs over 13.5 s, steady 35–36/s); per
5.83 s pass that is ~205, and its first cold pass was already ~200. Loop
playback was verified real, not a live dongle: the same grants repeat with
the file's 5.8 s period, and sync-loss events cluster at the loop seams.
The channel carries 69 TSDU frames × 3 blocks ≈ 207 TSBKs, so SDRTrunk
decodes essentially every block. HoosierSDR syncs on **all 69 frames** but
recovers only 152 blocks (147 channelized) — the deficit is not sync, not
timing, not the front end: **~25% of TSBK blocks die in trellis/CRC after a
good sync.** That is a specific, findable bug-or-gap (block 2/3 handling,
status-symbol stripping, or soft-decision quality late in the frame), not a
thesis problem. Two more honest notes: `--no-equalizer` scored 160, so the
CMA equalizer slightly *hurts* TSBK decode on this strong channel; and this
channel's distortion is mild (mean sync bit errors 0.42/48), so the
equalizer-attribution test still needs the degraded capture described above.

Against the detect-first CLI decoders the picture inverts: OP25 was given a
parameter sweep (`-C`, `-G`, `-b`, `-X` around defaults; defaults won, and
`-D fsk4` decoded nothing) and still managed 21; GopherTrunk's best was 17
on the pre-centered single channel (its `-auto-tune` found the carrier
within 150 Hz of our estimate but decoded fewer frames). HoosierSDR decodes
7–9× either of them on the same bits.

The Phase 1 gate ("measurably lower BER and sync-loss than SDRTrunk
nightly") is therefore **not yet met on this recording**: sync is at parity
(69/69 frames vs SDRTrunk's ~100% block recovery implies it missed no
frames either) and block FEC is behind. The next unit of work is the TSBK
block-decode gap — rerun this table after it closes.

Reproduce: `docker build` boatbod op25 (gr3.10, Ubuntu 22.04), then
`rx.py -F <48k.cf32> -S 48000 -D cqpsk -T trunk.tsv -v 10` and count
`TSBK: op=` lines; `gophertrunk replay -in marion.cu8 -format u8
-sample-rate 2400000 -protocol p25p1 -demod cqpsk -tune-hz 550000`;
`hoosier-sdr --rate 2400000 --offset 550k --cqpsk marion.cu8` ("TSBKs
decoded" line in the summary); SDRTrunk 0.6.1 → Add Recording Tuner on a
16-bit stereo IQ wav of the capture (center 851.000 MHz), a P25P1 channel
at 851.550 MHz with modulation CQPSK, preferred tuner = the recording
tuner (else a live dongle covering the frequency is silently chosen), and
count `TSBK` lines in the decoded-messages event log, normalized per
5.83 s file pass (playback loops).

| Decoder | Recording | Sync-loss | Pre-FEC BER | TSBK rate | Voice FER |
|---------|-----------|-----------|-------------|-----------|-----------|
| _voice-channel comparison pending — needs per-decoder FER instrumentation_ | | | | | |
