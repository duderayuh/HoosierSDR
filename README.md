# HoosierSDR

**A P25 trunked-radio receiver in Rust — the first P25 decoder that equalizes the channel before differential detection.**

Desktop-first (macOS and Windows). Built for Indiana's Hoosier SAFE-T system (P25 Phase I), useful for any P25 Phase I network.

> Status: **pre-alpha, and it decodes real off-air P25.** A 27-second RTL-SDR
> capture from Marion County, Indiana decodes end to end — NAC 0x261, 151 frame
> syncs at a mean 0.07 bit errors, 10.6 s of IMBE voice. See
> [`results/baselines.md`](results/baselines.md#first-field-decode--marion-county-2026-08). A complete offline P25 Phase I decode chain works end to end today — control-channel trunking, voice grants, and IMBE audio. See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the full design doc and roadmap, and [`results/baselines.md`](results/baselines.md) for measured decode quality.

## Install (macOS, one command)

```sh
curl -fsSL https://raw.githubusercontent.com/duderayuh/HoosierSDR/main/tools/install-mac.sh | bash
```

Installs every dependency (Xcode CLT, Homebrew, Rust, airspy + libusb), clones
to `~/HoosierSDR`, builds the release CLI, and verifies it with a no-hardware
decode. Idempotent — safe to re-run. Linux/Windows: see [Building](#building)
below.

## Try it now

```sh
cargo run -p hs-cli -- --demo
```

Synthesizes a P25 control-channel transmission plus a voice grant and runs it through the whole pipeline (C4FM demod → framer → BCH/trellis/CRC FEC → trunking state machine → IMBE vocoder), resolving the grant to its downlink frequency. It's a **no-hardware smoke test**: it proves framing, trunking and grant resolution, but the synthesized IMBE voice frames are not valid FEC codewords, so the vocoder outputs silence and `hoosier_out.wav` is a silent placeholder. To decode a real recording at an RTL-SDR's native rate: `cargo run -p hs-cli --
--rate 240000 --offset 50k --cqpsk capture.cf32`. The front end decimates to the
demodulators' working rate and `--offset` tunes to any 12.5 kHz channel inside
the captured band, so one wideband recording covers a whole slice of spectrum.

## What works today

- **Full C4FM modem** — FM discriminator, RRC matched filter, Gardner timing recovery, 4-level slicer, with a clean-channel modulator↔demodulator loopback test.
- **P25 Phase I layer 2** — 48-bit frame sync, BCH(63,16) NID decode (Berlekamp–Massey + Chien, corrects 11 errors), 1/2-rate trellis Viterbi for TSBK, CRC-CCITT16, status-symbol handling.
- **Trunking** — TSBK parsing (grants, IDEN_UP channel plans, network/RFSS status), channel→downlink-frequency resolution, grant tracking.
- **Trunk following** — `--follow` watches a control channel and decodes the calls it grants, with per-call modulation detection and tuner-error correction; calls end on the channel's own terminator (TDU) so back-to-back grants on one frequency stay separate calls; when the control channel goes quiet it hunts the alternates the site announced (SCCB), carrying the channel plan across so no grant is lost.
- **IMBE voice** — Phase I vocoder via vendored ISC-licensed mbelib (FFI), producing 8 kHz PCM.
- **Encryption gate** — ALGID detection wired through; encrypted grants/voice are flagged and never decoded, by architecture.
- **Benchmark harness** — `hs-bench` runs synthetic or field IQ and reports decode metrics with the equalizer A/B.

## Live capture

Streaming decode works end to end. `hs-core::stream::run` pulls IQ from any `SdrSource` in blocks and feeds the stateful decoder, so a frame split across block boundaries still decodes (tested). Two radio backends live behind off-by-default features, keeping the core build pure-Rust and libusb-free:

- **Airspy R2** (`airspy`, direct `libairspy`) — the device that matters: at 10 MSPS it spans a whole SAFE-T site, so `--follow` can track the control channel and every voice channel it grants from one radio. Its 10/2.5 MSPS are normalized to 9.6/2.4 MSPS on the fly. Proven live: a site followed at real time (9.61/9.60 Msps, 0 dropped) with calls decoded to audio across 851–858 MHz.
- **RTL-SDR** (`rtlsdr`, via Seify) — 2.4 MSPS, one channel or a ±1 MHz slice.

```sh
# whole site, live, from an Airspy R2 (centre the band on the site's span)
cargo run --release -p hs-cli --features airspy -- --sdr --source airspy \
    --rate 10000000 --freq 855M --follow --control 851.5375M

# one channel from an RTL-SDR
cargo run --release -p hs-cli --features rtlsdr -- --sdr --freq 851.0125M --cqpsk
```

Builds compile for the host CPU by default (`.cargo/config.toml`); at 10 MSPS that is the difference between following a site drop-free and losing a third of the samples. `--secs N` ends a live run after N seconds with the summary printed; `--serial <hex>` picks one of several Airspys. The Airspy R2's 2016 firmware takes no gain setting (it hangs), so it runs at its defaults — which decode fine. macOS: `brew install airspy libusb`. Without the feature, `--sdr` prints setup guidance. The normalizer preserves 0.8 of the output Nyquist: ±960 kHz around the centre at 2.5 MSPS, ±3.84 MHz at 10 MSPS — centre the band so every channel you need sits inside that.

## Desktop app

**Config** holds the RadioReference login (premium account; secrets in the Keychain), a state → county → system browser (or a ZIP code), a talkgroup picker, and **playlists** — a saved system + site + talkgroup set that tunes the receiver and filters the feed in one click. The RadioReference *app key* is compiled in at build time from `HS_RR_APP_KEY` or a git-ignored `app/.rr_app_key` file and lightly masked in the binary; it is never committed. A build without it shows an App-key field instead. Note what masking is and isn't: a key inside a desktop binary can always be extracted — RadioReference's revocable per-app key and each user's own login are the real controls.

The app follows a site the same way the CLI does — **Follow site** mode takes a band centre and a control channel, measures where the control channel really is and which modulation it uses, then decodes every call it grants, listing each in the dispatch feed with its duration, playing completed calls through the default audio device, and (optionally) saving one WAV per call. **One channel** mode is the single-channel decoder with the equalizer selector. The follow loop is shared with the CLI (`hs_core::follow`) and is verified headless over a recorded Airspy capture in the app's tests.


## Finding the control channel

A power sweep won't find it. That method put the first field capture 50 kHz off
the real carrier, locked onto a strong signal that wasn't P25 at all — a
spectrum plot can't tell a control channel from an analog repeater. There are
two reliable ways instead.

**Scan by decoding.** `--scan` sweeps every channel position in a wideband
capture, runs the real decoder at each, and reports what actually carries P25:

```sh
hoosier-sdr --rate 240000 --freq 858.9375M --scan capture.cf32
```

```text
Found 1 P25 channel(s):

  voice    858.9875 MHz  CQPSK  NAC 0x261    20 syncs  err 0.30
```

One 240 kHz recording covers ~19 channels, and each hit is labelled control vs
voice, with its modulation and NAC. Frequencies are snapped to the P25 channel
raster, so they're ready to paste into `--freq`.

**Or ask RadioReference**, which already knows every site's control and
alternate channels:

```sh
export RR_APP_KEY=... RR_USERNAME=... RR_PASSWORD=...
cargo run -p hs-cli --features radioreference -- --rr-system 7804
```

prints each site with its NAC, RFSS and control channels (primary first, ready
to paste into `--freq`), and writes a talkgroup CSV that `--catalog` reads
back. The NAC it prints is the same one the decoder reports off the air, so a
capture can be matched to the exact site it was hearing.

This needs an **application key** — register the app once at
[radioreference.com/apps/account](https://www.radioreference.com/apps/account/?tab=api)
— and each user supplies their own RadioReference login with an active premium
subscription; the service authenticates the end user on every call. HoosierSDR
deliberately ships no key of its own. Without a subscription, export the
talkgroup CSV from the website by hand and use `--catalog` — that path needs no
credentials and always works.

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
channel) and transcription. The RadioReference client is written but has not yet
run against the live service — the response field mapping is built from
published documentation and may need one correction pass against a real payload
(`--rr-dump` captures it). These are the Phase 4–5 roadmap.

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
| `hs-source` | `SdrSource` trait; RTL-SDR (Seify), Airspy R2 (libairspy) and IQ-file backends |
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
- **No RadioReference data is committed to this repository.** Test fixtures are synthetic.

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md) — especially the **code provenance policy**. This project is Apache-2.0 and must stay clean of GPL-derived code.

## License

Apache-2.0. See [`LICENSE`](LICENSE).
