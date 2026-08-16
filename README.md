# HoosierSDR

**A P25 trunked-radio receiver in Rust — the first P25 decoder that equalizes the channel before differential detection.**

Desktop-first (macOS primary, Windows port planned). Built for Indiana's Hoosier SAFE-T system (P25 Phase I), useful for any P25 Phase I network.

> Status: **pre-alpha scaffolding.** See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the full design doc and roadmap.

## The thesis

Every open-source P25 CQPSK receiver (OP25, trunk-recorder, SDRTrunk) performs differential detection *before* any point where an equalizer could act. Differential detection is a nonlinearity that scrambles inter-symbol interference irrecoverably. HoosierSDR places a sync-trained T/2 fractionally-spaced adaptive equalizer **before** differential detection, trained on the 24-symbol Frame Sync Word that arrives free every 180 ms — targeting the simulcast-distortion regime where existing decoders degrade.

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
| `app/` | Tauri v2 desktop shell (Phase 3) |

## Building

```sh
cargo build --workspace
cargo test --workspace
```

## Legal posture (read this)

- **No decryption. Ever.** Encrypted traffic (any P25 ALG ID other than clear) is detected, badged, and skipped. This is an architectural refusal, not a setting. See 18 U.S.C. § 2511/2510.
- **Phase I IMBE only in-tree.** The IMBE vocoder patents have expired. The Phase II AMBE+2 half-rate vocoder remains patent-encumbered in the US (US 8,359,197, to 2028-05-20) and is supported only via a user-supplied runtime plugin that this project does not distribute.
- **Indiana users:** IC 35-44.1-2-7 restricts *mobile/portable* police radio receivers. Use of this software at your dwelling or place of business falls under exemption (b)(7). Do not run it in a vehicle or carry it on your person unless you hold an FCC amateur license or written LE permission. This is not legal advice.
- **No RadioReference data is committed to this repository.** Test fixtures are synthetic.

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md) — especially the **code provenance policy**. This project is Apache-2.0 and must stay clean of GPL-derived code.

## License

Apache-2.0. See [`LICENSE`](LICENSE).
