# hoosier-sdr (CLI)

The HoosierSDR command-line application. Decodes a P25 Phase I transmission
from a raw IQ recording, prints trunking activity and encryption status, and
writes decoded voice to a WAV file.

## Run it now (no hardware, no recording)

```sh
cargo run -p hs-cli -- --demo
```

This synthesizes a control-channel + clear-voice transmission, runs it through
the full decode pipeline (C4FM demod → framer → BCH/trellis/CRC → trunking →
IMBE vocoder), resolves the voice grant to a downlink frequency, and writes
`hoosier_out.wav`.

## Decode a real recording

```sh
cargo run -p hs-cli -- --rate 48000 capture.cf32 --wav call.wav
```

`capture.cf32` is raw interleaved little-endian f32 IQ. The sample rate must be
an integer multiple of 4800 (the P25 symbol rate); 48000 is a good default.

## Live audio playback

WAV output works everywhere with no audio backend. For live playback add the
`audio` feature (uses CoreAudio on macOS, WASAPI on Windows, ALSA on Linux):

```sh
cargo run -p hs-cli --features audio -- --demo --play
```

On Linux this requires ALSA development headers (`libasound2-dev`); macOS and
Windows need no extra system packages.

## Options

| Flag | Meaning |
|------|---------|
| `--demo` | Decode a synthesized transmission (no input needed) |
| `--rate <HZ>` | IQ sample rate (default 48000) |
| `--wav <PATH>` | Voice output WAV path (default `hoosier_out.wav`) |
| `--no-wav` | Skip WAV output |
| `--equalizer` | Enable the experimental FSW-trained equalizer (see `results/baselines.md`) |
| `--play` | Live playback (needs `--features audio`) |

## Legal

Decodes **unencrypted** P25 only. Encrypted talkgroups are detected and
skipped by design — HoosierSDR never decrypts. Use only where authorized
under applicable law. See the root README.
