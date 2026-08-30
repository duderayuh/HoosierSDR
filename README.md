# HoosierSDR

**P25 Phase I trunked-radio receiver in Rust.** Equalizes the channel *before* differential detection — the difference that matters on simulcast systems.

- Decodes P25 Phase I trunked radio (any network) to audio
- Desktop app (macOS, Windows) + CLI
- Pre-alpha, but decodes real off-air P25 end to end

## Install (macOS)

```sh
curl -fsSL https://raw.githubusercontent.com/duderayuh/HoosierSDR/main/tools/install-mac.sh | bash
```

Or double-click `install-mac.command`.

Installs Xcode CLT, Homebrew, Rust, the SDR libraries, and `tauri-cli`; clones to `~/HoosierSDR`; builds the CLI. Re-run any time to repair or update.

## Quick start

```sh
cargo run -p hs-cli -- --demo      # synthesized decode, no hardware needed
```

## Features

- **P25 Phase I** — C4FM + CQPSK modem, frame sync, BCH/Golay/trellis FEC, TSBK/MBT parsing
- **Trunking** — control-channel following, grant tracking, alternate-channel hunting
- **IMBE voice** — vendored ISC-licensed mbelib to 8 kHz PCM
- **Encryption gate** — encrypted traffic flagged and skipped, never decoded
- **Equalizer-first CQPSK** — adaptive equalizer before differential detection (the core idea)
- **Radios** — Airspy R2 (10 MSPS, whole-site), RTL-SDR (2.4 MSPS)
- **Other protocols** — AM, NBFM, DCS; more on the roadmap
- **Desktop app** — live decode, spectrum waterfall, record IQ (Tauri v2)

## Usage

```sh
# decode a recording
cargo run -p hs-cli -- --rate 240000 --offset 50k --cqpsk capture.cf32

# find P25 channels in a recording
hoosier-sdr --rate 240000 --freq 858.9375M --scan capture.cf32

# follow a whole site live (Airspy R2)
cargo run --release -p hs-cli --features airspy -- --sdr --source airspy \
    --rate 10000000 --freq 855M --follow --control 851.5375M

# decode one channel live (RTL-SDR)
cargo run --release -p hs-cli --features rtlsdr -- --sdr --freq 851.0125M --cqpsk
```

## Build

```sh
cargo build --workspace
cargo test --workspace
cd app && cargo tauri dev      # desktop app
```

The IMBE vocoder compiles vendored C (ISC mbelib), so a C compiler is required.

## Documentation

- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — design and roadmap
- [`docs/RECEPTION.md`](docs/RECEPTION.md) — improving simulcast reception with the app's meters
- [`results/baselines.md`](results/baselines.md) — measured decode quality
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — code provenance policy

## Legal

- **No decryption.** Encrypted traffic is detected and skipped. Architectural, not a setting. (18 U.S.C. § 2511)
- **No RadioReference data committed.** Fixtures are synthetic.
- **No GPL-derived code.** Apache-2.0. See `CONTRIBUTING.md`.

## License

Apache-2.0 — see `LICENSE`.
