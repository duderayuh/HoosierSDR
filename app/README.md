# HoosierSDR desktop app (Tauri v2)

macOS/Windows desktop shell over the `hs-core` decode engine: tune a radio, watch
calls decode live, see a spectrum waterfall, record IQ to disk.

Own Cargo workspace — excluded from the main workspace so `cargo build
--workspace` and CI (Linux, no WebKit) never build it.

## Prerequisites (macOS)

```sh
brew install airspy soapysdr soapyrtlsdr librtlsdr libusb pkg-config ffmpeg
cargo install tauri-cli --version '^2.0'
```

The app links all three SDR backends (`rtlsdr`, `airspy`, `soapy`), so every
SDR formula above is required — a missing one fails at link time. `ffmpeg`
enables mp3/m4a/opus call storage (without it, calls stay WAV). macOS ships
WebKit; on Windows install WebView2 + MSVC.

## Run

```sh
cd app
cargo tauri dev
```

First build compiles the whole decode stack plus Tauri; takes a few minutes.

## Controls

- **Tuning** — frequency, sample rate, gain, modulation (C4FM / CQPSK)
- **Catalog** — RadioReference talkgroup CSV for names
- **Record IQ / log** — paths for a `.cf32` capture + `.json` diagnostics
- **Start / Stop** — live capture
- **Decode file** — decode a `.cf32` recording without hardware

## Transcription (optional)

Needs a native arm64 Python 3.10+ with `faster-whisper`. Apple's CommandLineTools
Python 3.9 won't work (no ctranslate2 wheel), and a Rosetta x86_64 interpreter
mismatches the arm64 `av` wheel.

```sh
brew install python@3.12
/opt/homebrew/bin/python3.12 -m pip install faster-whisper
```

The app probes `python3.13` / `3.12` / `3.11` in both Homebrew prefixes
automatically; set `TRANSCRIBE_PYTHON` to pin a specific interpreter.

## Bundle

```sh
cargo tauri build
```

## Legal

Decodes unencrypted P25 only. Indiana users: run at your dwelling or place of
business (IC 35-44.1-2-7).
