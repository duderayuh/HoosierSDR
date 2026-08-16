# Diagnostics export — the refine-from-real-data loop

When you run HoosierSDR against a **real** capture and something decodes
poorly, export the diagnostics and (optionally) the IQ, and share both. That
lets the exact decode be reproduced and the DSP tuned against your signal
rather than a synthetic one.

## How to export

```sh
hoosier-sdr capture.cf32 --rate 48000 --log run.json --save-iq run.cf32
```

- `--log run.json` — a small JSON diagnostics report (schema below).
- `--save-iq run.cf32` — the exact interleaved-f32 IQ that was decoded, so the
  run can be replayed bit-for-bit:
  ```sh
  hoosier-sdr run.cf32 --rate 48000
  ```

Share `run.json` always; share `run.cf32` when the signal itself is needed to
reproduce a problem (it is larger — 8 bytes per IQ sample).

### Capturing IQ in the first place

Until live SDR capture lands (`hs-source` + Seify), capture with any SDR tool
that writes raw complex float32, e.g. GQRX/SDR++ "record baseband" set to
CF32, or `rtl_sdr`/`airspy_rx` piped through a converter. The sample rate you
pass to `--rate` must match the capture and be a multiple of 4800.

## JSON schema (`hoosier-sdr/diagnostics/1`)

| Field | Meaning |
|-------|---------|
| `sample_rate` | IQ sample rate used |
| `equalizer` | whether the experimental equalizer was enabled |
| `symbols_processed` | total C4FM symbols recovered |
| `voice_frames` / `pcm_samples` | decoded IMBE frames and audio samples |
| `sync_count` | frame-sync detections |
| `mean_sync_bit_errors` | avg bit errors in the 48-bit sync correlation (↓ better) |
| `symbol_health.level_counts` | histogram of sliced dibits `[+3,+1,-1,-3]` |
| `symbol_health.soft_mean` | mean soft-symbol value (DC bias indicator) |
| `symbol_health.eye_error` | mean |soft − nearest nominal level| (↓ = open eye) |
| `syncs[]` | `{at: symbol index, err: bit errors}` per detection |
| `nids[]` | `{nac, duid, bch_err}` per NID decode |
| `grants[]` | `{tg, src, freq_hz, enc}` resolved voice grants |
| `encrypted_talkgroups[]` | talkgroups skipped because encrypted |

### What the numbers tell us

- **High `mean_sync_bit_errors` or few `syncs`** → timing/carrier recovery is
  struggling, or the signal isn't C4FM at this rate/offset.
- **`eye_error` large (≳0.3)** → closed eye: ISI (simulcast!), gain, or timing.
  This is the metric the pre-detection equalizer is meant to drive down.
- **`level_counts` badly skewed** → DC offset or deviation-scaling error.
- **`bch_err` frequently nonzero** → NIDs are marginal; the demod is on the
  edge of working.
- **`encrypted_talkgroups` populated** → those calls are AES/DES/ADP and will
  never decode by design.

Nothing in the log contains audio content or personal data — it is decode
telemetry only.
