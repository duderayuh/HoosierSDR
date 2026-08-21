# HoosierSDR desktop app (Tauri v2)

The macOS/Windows desktop shell. A thin Tauri layer over the `hs-core` decode
engine: tune an RTL-SDR, watch calls decode live (with talkgroup names from a
RadioReference CSV), see a spectrum waterfall, and **record IQ + diagnostics to
disk** so a capture can be shared and replayed.

This crate is its **own Cargo workspace** — it is deliberately excluded from
the main workspace so `cargo build --workspace` and CI (which run on Linux
without WebKit) never try to build it. Build it here, on macOS or Windows.

## Prerequisites (macOS)

```sh
# Rust (if not already): https://rustup.rs
brew install libusb                       # RTL-SDR USB access (Seify backend)
cargo install tauri-cli --version '^2.0'  # the `cargo tauri` command
```

macOS ships the WebKit webview, so nothing else is needed. On Windows, install
the WebView2 runtime (usually already present) and the MSVC toolchain.

## Run it

```sh
cd app
cargo tauri dev
```

First build compiles the whole decode stack plus Tauri and Seify, so it takes a
few minutes. After that the window opens with:

- **Tuning** — frequency (`851.0125M`), sample rate, gain (blank = hardware
  AGC), modulation (C4FM or CQPSK/LSM for simulcast).
- **Catalog** — path to your RadioReference talkgroup CSV; grants then show
  names instead of numbers.
- **Record IQ / log** — paths to write a `.cf32` capture and a `.json`
  diagnostics log. Fill these in before capturing to save data to share.
- **Start / Stop** — live capture; calls stream into the table as they decode.
- **Decode file** — point at a `.cf32` recording to decode without hardware.

## Capturing data to share

1. Set **Record IQ to** = `lake.cf32` and **Record log to** = `lake.json`.
2. Tune to a SAFE-T control channel, pick **CQPSK** for a simulcast site,
   **Start**, let it run through some traffic, **Stop**.
3. Send `lake.cf32` + `lake.json`. The `.cf32` replays the exact signal;
   the `.json` carries decode telemetry (sync quality, eye health, grants).

You can also open `lake.json` in `tools/scope.html` (repo root) for a visual
breakdown of that session.

## Build a bundle (optional)

```sh
cargo tauri build
```

produces a signed-ready `.app` / `.dmg` (macOS) or `.msi` (Windows). For a
distributable build you'll want to replace `icons/icon.png` with a full icon
set and enable `bundle.active` in `tauri.conf.json`.

## Legal

Decodes unencrypted P25 only; encrypted talkgroups are detected and skipped by
design. Indiana users: run at your dwelling or place of business (IC
35-44.1-2-7). See the repository root README.

**Front-end load check:** `cd app && npm i --no-save jsdom@24 && node scripts/pagecheck.js` boots `dist/` under jsdom with a fake Tauri bridge and fails on any load-time JavaScript error (which otherwise shows up as an app with every list empty).
