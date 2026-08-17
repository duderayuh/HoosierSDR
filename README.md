# HoosierSDR

**A P25 trunked-radio receiver in Rust — the first P25 decoder that equalizes the channel before differential detection.**

Desktop-first (macOS primary, Windows port planned). Built for Indiana's Hoosier SAFE-T system (P25 Phase I), useful for any P25 Phase I network.

> Status: **pre-alpha, and it decodes real off-air P25.** A 27-second RTL-SDR
> capture from Marion County, Indiana decodes end to end — NAC 0x261, 151 frame
> syncs at a mean 0.07 bit errors, 10.6 s of IMBE voice. See
> [`results/baselines.md`](results/baselines.md#first-field-decode--marion-county-2026-08). A complete offline P25 Phase I decode chain works end to end today — control-channel trunking, voice grants, and IMBE audio. See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the full design doc and roadmap, and [`results/baselines.md`](results/baselines.md) for measured decode quality.

## Try it now

```sh
cargo run -p hs-cli -- --demo
```

Synthesizes a P25 control-channel + clear-voice transmission, runs it through the whole pipeline (C4FM demod → framer → BCH/trellis/CRC FEC → trunking state machine → IMBE vocoder), resolves the voice grant to its downlink frequency, and writes decoded audio to `hoosier_out.wav`. To decode a real recording at an RTL-SDR's native rate: `cargo run -p hs-cli --
--rate 240000 --offset 50k --cqpsk capture.cf32`. The front end decimates to the
demodulators' working rate and `--offset` tunes to any 12.5 kHz channel inside
the captured band, so one wideband recording covers a whole slice of spectrum.

## What works today

- **Full C4FM modem** — FM discriminator, RRC matched filter, Gardner timing recovery, 4-level slicer, with a clean-channel modulator↔demodulator loopback test.
- **P25 Phase I layer 2** — 48-bit frame sync, BCH(63,16) NID decode (Berlekamp–Massey + Chien, corrects 11 errors), 1/2-rate trellis Viterbi for TSBK, CRC-CCITT16, status-symbol handling.
- **Trunking** — TSBK parsing (grants, IDEN_UP channel plans, network/RFSS status), channel→downlink-frequency resolution, grant tracking.
- **IMBE voice** — Phase I vocoder via vendored ISC-licensed mbelib (FFI), producing 8 kHz PCM.
- **Encryption gate** — ALGID detection wired through; encrypted grants/voice are flagged and never decoded, by architecture.
- **Benchmark harness** — `hs-bench` runs synthetic or field IQ and reports decode metrics with the equalizer A/B.

## Live capture

Streaming decode works end to end. `hs-core::stream::run` pulls IQ from any `SdrSource` in blocks and feeds the stateful decoder, so a frame split across block boundaries still decodes (tested). An RTL-SDR backend (Seify) lives behind the off-by-default `rtlsdr` feature, keeping the core build pure-Rust and libusb-free:

```sh
cargo run -p hs-cli --features rtlsdr -- --sdr --freq 851.0125M --cqpsk
```

(That pulls Seify + libusb; on macOS `brew install libusb`. Without the feature, `--sdr` prints setup guidance.)

## Desktop app

A Tauri v2 desktop app lives in [`app/`](app/) (its own workspace, built on
macOS/Windows — see [`app/README.md`](app/README.md)): tune an RTL-SDR, watch
calls decode live with talkgroup names, a spectrum waterfall, and one-click
**record IQ + diagnostics to disk**. Talkgroup names come from a RadioReference
CSV via `hs-catalog` (also available on the CLI: `--catalog talkgroups.csv`).

```sh
cd app && cargo tauri dev      # macOS: brew install libusb; cargo install tauri-cli
```

## What's not done yet

The thesis is still unproven *in the field*. The Marion County capture decodes
cleanly either way — at ~40 dB SNR there is almost no ISI to remove — so it
validates the receiver, not the equalizer. Confirming the thesis needs captures
where the conventional detect-first path actually fails: weak signal, deep
multipath, or overlapping simulcast transmitters at comparable strength.
Also outstanding: control-channel capture (the one recorded so far is a traffic
channel), the RadioReference SOAP API (CSV import works today), and
transcription. These are the Phase 4–5 roadmap.

## The thesis

Every open-source P25 CQPSK receiver (OP25, trunk-recorder, SDRTrunk) performs differential detection *before* any point where an equalizer could act. Differential detection is a nonlinearity that scrambles inter-symbol interference irrecoverably. HoosierSDR places a sync-trained T/2 fractionally-spaced adaptive equalizer **before** differential detection, trained on the 24-symbol Frame Sync Word that arrives free every 180 ms — targeting the simulcast-distortion regime where existing decoders degrade.

**Status of the thesis — now demonstrated in a controlled experiment.** `cargo test -p hs-dsp --test thesis_cqpsk` runs a CQPSK stream through a complex two-ray (simulcast-like) channel and decodes it two ways:

| Decode path | Symbol error rate |
|-------------|:-----------------:|
| Differential detection first (OP25 / trunk-recorder / SDRTrunk) | **0.259** |
| Sync-trained equalizer **before** differential detection (this project) | **0.000** |

That is the whole claim in one number — a categorical win, because differential detection is a nonlinearity that makes ISI unrecoverable, so removing it first is not a marginal gain. What remains is *integration*: wiring this proven complex equalizer (`hs-dsp::cqpsk::EqualizedCqpsk` / `LmsFse`) behind live Costas carrier + Gardner timing recovery on real CQPSK IQ, then re-running the gate on a field corpus. The C4FM voice/control path above already decodes end to end today; the CQPSK front end is the next integration. `results/baselines.md` reports everything without spin.

## Workspace layout

| Crate | Purpose |
|---|---|
| `hs-source` | `SdrSource` trait; RTL-SDR / Airspy (via Seify) and IQ-file backends |
| `hs-dsp` | Filters, resamplers, channelizer, AGC, timing/carrier recovery, and `equalizer/` (LMS FSE, CMA, DFE, MLSE) |
| `hs-p25` | Frame sync, NID, FEC (BCH/Golay/RS/trellis), TSBK/MBT parsing |
| `hs-vocoder` | `Vocoder` trait; Phase I IMBE in-tree, Phase II behind a plugin boundary |
| `hs-trunk` | Trunking state machine, control-channel following, grants |
| `hs-catalog` | Talkgroup/site catalog: CSV import, RadioReference SOAP, FCC ULS |
| `hs-transcribe` | VAD → ASR trait (Whisper / Parakeet) → hallucination filter → normalizer |
| `hs-core` | Orchestration, scan lists, call routing, recording |
| `hs-bench` | BER/decode-quality benchmark harness — built first, wired into CI |
| `hs-cli` | `hoosier-sdr` command-line app: decode a recording or `--demo`, write WAV |
| `app/` | Tauri v2 desktop app: live tune/decode, spectrum, record IQ (own workspace) |

## Building

```sh
cargo build --workspace
cargo test --workspace
cargo run -p hs-bench          # synthetic decode-quality A/B
cargo run -p hs-cli -- --demo  # decode a synthesized transmission
```

The IMBE vocoder compiles vendored C (ISC-licensed mbelib) via `cc`, so a C
compiler is required for the default build. `cargo build -p hs-vocoder
--no-default-features` gives a pure-Rust build with the vocoder stubbed out.

## Legal posture (read this)

- **No decryption. Ever.** Encrypted traffic (any P25 ALG ID other than clear) is detected, badged, and skipped. This is an architectural refusal, not a setting. See 18 U.S.C. § 2511/2510.
- **Phase I IMBE only in-tree.** The IMBE vocoder patents have expired. The Phase II AMBE+2 half-rate vocoder remains patent-encumbered in the US (US 8,359,197, to 2028-05-20) and is supported only via a user-supplied runtime plugin that this project does not distribute.
- **Indiana users:** IC 35-44.1-2-7 restricts *mobile/portable* police radio receivers. Use of this software at your dwelling or place of business falls under exemption (b)(7). Do not run it in a vehicle or carry it on your person unless you hold an FCC amateur license or written LE permission. This is not legal advice.
- **No RadioReference data is committed to this repository.** Test fixtures are synthetic.

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md) — especially the **code provenance policy**. This project is Apache-2.0 and must stay clean of GPL-derived code.

## License

Apache-2.0. See [`LICENSE`](LICENSE).
