# HoosierSDR desktop app (Tauri v2)

macOS/Windows desktop shell over the `hs-core` decode engine: tune a radio, watch
calls decode live, see a spectrum waterfall, record IQ to disk.

Own Cargo workspace — excluded from the main workspace so `cargo build
--workspace` and CI (Linux, no WebKit) never build it.

## Prerequisites (macOS)

```sh
brew install airspy soapysdr soapyrtlsdr librtlsdr libusb pkg-config
cargo install tauri-cli --version '^2.0'
```

The app links all three SDR backends (`rtlsdr`, `airspy`, `soapy`), so every
formula above is required — a missing one fails at link time. macOS ships
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

```sh
python3 -m pip install --user faster-whisper
```

The app probes `/usr/local/bin/python3` and `/opt/homebrew/bin/python3` first;
set `TRANSCRIBE_PYTHON` to pin a specific interpreter.

## Bundle

```sh
cargo tauri build
```

## Legal

Decodes unencrypted P25 only. Indiana users: run at your dwelling or place of
business (IC 35-44.1-2-7).
