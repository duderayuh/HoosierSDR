# HoosierSDR — Architecture & Roadmap

Design doc for the P25 Phase I trunked-radio receiver.

## Thesis

Equalize the channel **before** differential detection. Every other open-source
P25 decoder does differential detection first — a nonlinearity that scrambles
inter-symbol interference (ISI) irrecoverably. A sync-trained T/2
fractionally-spaced equalizer, trained on the 24-symbol Frame Sync Word that
arrives every 180 ms, targets the simulcast-distortion regime where existing
decoders degrade.

Measured (`hs-dsp::cqpsk` two-ray experiment): **0.259 → 0.000** symbol error
rate. See `results/baselines.md`.

## Stack

| Layer | Choice |
|---|---|
| DSP + protocol core | Rust |
| Hardware I/O | Seify (`seify`, `seify-rtlsdr`), `SdrSource` trait |
| FFT / filters | `rustfft`, hand-written FIR |
| UI | Tauri v2 |
| Audio out | `cpal` |
| Transcription | `whisper-rs` (Metal/Vulkan) |
| Storage | SQLite (`rusqlite`, FTS5) |
| License | Apache-2.0 (no GPL) |

## Crate layout

```
hoosier-sdr/
├─ crates/
│  ├─ hs-source/          SdrSource trait; seify-rtlsdr, seify-airspy, iq-file
│  ├─ hs-dsp/             filters, resamplers, channelizer, AGC, TED, PLL, equalizer/
│  ├─ hs-p25/             frame sync, NID, FEC (BCH/Golay/RS/trellis), TSBK/MBT
│  ├─ hs-vocoder/         Vocoder trait; vendored ISC mbelib (IMBE)
│  ├─ hs-trunk/           site/system state machine, control-channel following
│  ├─ hs-catalog/         RadioReference client, CSV import
│  ├─ hs-transcribe/      VAD gate → ASR → hallucination filter → normalizer
│  ├─ hs-core/            orchestration, scan lists, call router, recording
│  └─ hs-bench/           BER harness, IQ corpus runner
└─ app/                   Tauri v2 shell + web UI
```

## Receive chain

```
SDR → DC/IQ correction → channelizer
  ├─ control channel → RRC → equalizer → timing/carrier → diff detect → slicer → NID/FEC/TSBK → trunking
  └─ voice channels  → RRC → equalizer → timing/carrier → diff detect → slicer → IMBE → PCM
```

The equalizer sits **before** differential detection on both paths.

## Legal (hard rules)

- **No decryption.** Encrypted traffic is detected and skipped. 18 U.S.C. § 2511.
- **Vocoder from ISC mbelib.** IMBE (Phase I) in-tree; AMBE+2 half-rate (Phase II) vendored from the same ISC code (`ambe3600x2450.c`).
- **No GPL-derived code.** Never port from OP25, SDRTrunk, trunk-recorder, mbelib-neo, dsd-neo, or JMBE. See `CONTRIBUTING.md`.
- **No RadioReference data committed.** Synthetic fixtures only.
- **Desktop-first is legally correct in Indiana** (IC 35-44.1-2-7): a desktop app at a dwelling is exempt; a vehicle/portable rig is not.

## RadioReference

- SOAP/XML only (`api.radioreference.com/soap2`, version 18). No REST API.
- Auth: `appKey` + end-user `username`/`password` (Premium subscription required).
- **Ship CSV import first** — no app key, no credentials, always works.
- Credentials in the OS keyring, never plaintext.
- Read the `enc` and `tdma_cc` attributes (grey out encrypted talkgroups; handle Phase II pilot sites).

## Transcription

Off-the-shelf Whisper scores ~50% WER on real police radio; the practical ceiling
(human inter-annotator agreement) is ~27%. Ship the pipeline, not raw Whisper:

```
call audio → Silero VAD gate → resample → ASR (Whisper | Parakeet) → hallucination filter → normalizer → SQLite FTS5
```

- **Hallucination blocklist** — hallucinations are highly repetitive; a blocklist is unusually effective.
- `condition_on_previous_text = false` (independent transmissions).
- Prefer a transducer (Parakeet) over Whisper; `sherpa-onnx` adds hotword biasing.

## Roadmap

- **Phase 0 — Instrument.** Build `hs-bench`, capture an IQ corpus, baseline SDRTrunk/OP25/GopherTrunk.
- **Phase 1 — Prove the thesis.** Offline decode; equalizer before differential detection. Gate: lower BER than SDRTrunk on two simulcast captures.
- **Phase 2 — Hear it.** Live SDR, trunking, IMBE audio. Gate: follow a site for an hour, clean audio, no crashes.
- **Phase 3 — App.** Tauri shell, waterfall, recording. Gate: daily driver for two weeks.
- **Phase 4 — Catalog + transcription.** RadioReference, keyring, transcription pipeline. Gate: search finds the call.
- **Phase 5 — Ship.** Windows, signed release, Phase II TDMA, headless + remote client.

## Risk register

| Risk | Severity | Mitigation |
|---|---|---|
| Equalizer doesn't beat SDRTrunk | Fatal | Phase 1 gate surfaces it early |
| GopherTrunk already solved it | High | Evaluate week 1; compete on the app layer |
| Solo-maintainer burnout | High | Scope v1 to P25 only |
| GPL contamination | Medium | Provenance policy in `CONTRIBUTING.md` |
| Transcription quality disappoints | Medium | Set expectations; ship the filter pipeline |
| SAFE-T goes encrypted | Low, growing | Interop/dispatch talkgroups clear by IPSC policy |

## Sources

- **Decoders:** [SDRTrunk](https://github.com/DSheirer/sdrtrunk) · [OP25](https://github.com/boatbod/op25) · [DSD-FME](https://github.com/lwvmobile/dsd-fme) · [trunk-recorder](https://github.com/TrunkRecorder/trunk-recorder) · [GopherTrunk](https://github.com/MattCheramie/GopherTrunk)
- **Simulcast:** [Tait P25 Simulcast (PDF)](https://www.radioresource.com/downloads/tait/whitepapers/p25-simulcast-coverage-white-paper.pdf) · [EFJohnson Simulcasting (PDF)](https://www.efjohnson.com/resources/dyn/files/972772z218319c9/_fn/Simulcasting+Project+25.pdf)
- **Rust SDR:** [Seify](https://github.com/FutureSDR/seify) · [p25rx](https://github.com/kchmck/p25rx)
- **Legal:** [18 U.S.C. § 2511](https://www.law.cornell.edu/uscode/text/18/2511) · [IC 35-44.1-2-7](https://law.justia.com/codes/indiana/title-35/article-44-1/chapter-2/section-35-44-1-2-7/) · [mbelib (ISC)](https://github.com/szechyjs/mbelib)
- **RadioReference:** [API wiki](https://wiki.radioreference.com/index.php/API) · [Hoosier SAFE-T](https://www.radioreference.com/db/sid/8084)
- **Transcription:** [Police radio ASR / BPC-CPD (arXiv 2409.10858)](https://arxiv.org/html/2409.10858v1) · [Whisper hallucination (arXiv 2501.11378)](https://arxiv.org/html/2501.11378v1) · [sherpa-onnx hotwords](https://k2-fsa.github.io/sherpa/onnx/hotwords/index.html)
