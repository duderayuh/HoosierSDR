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

Installs Xcode CLT, Homebrew, Rust, the SDR libraries (`airspy libusb soapysdr soapyrtlsdr librtlsdr pkg-config ffmpeg`), and `tauri-cli` (prebuilt via `cargo-binstall`, not a source compile); clones to `~/HoosierSDR`; builds the CLI; verifies it with a no-hardware demo decode. Idempotent and self-healing — re-run any time to repair or update.

### Known gotchas

- **The installer exits right after "Xcode command-line tools".** That's expected on a fresh Mac — it pops Apple's dialog, you click *Install* and wait for it to finish, then **re-run the installer**. It picks up where it left off.
- **`cargo tauri --version` → `no such command: tauri`.** `tauri-cli` isn't installed yet. Don't reach for `cargo install tauri-cli` (it compiles ~500 crates and can OOM an 8 GB machine). Use the prebuilt route instead:

  ```sh
  brew install cargo-binstall
  cargo binstall tauri-cli -y
  ```

- **The desktop app is a *separate* workspace under `app/`.** `cargo tauri dev` from `~/HoosierSDR` fails with `Couldn't recognize the current folder as a Tauri project`. Run it from inside the app folder:

  ```sh
  cd ~/HoosierSDR/app && cargo tauri dev
  ```

- **8 GB machines (MacBook Neo, A18 Pro).** The first `cargo tauri dev` is the big compile — it can beach-ball or OOM on 8 GB. Let it churn; if it OOMs, retry with `CARGO_BUILD_JOBS=2 cargo tauri dev`.
- **`brew install …` changes only take effect in a new terminal.** After the installer finishes, open a fresh window before running `hoosier-sdr` or `cargo tauri`.

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
