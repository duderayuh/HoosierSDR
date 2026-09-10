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
| `voice_frame_errors` | cumulative post-FEC bit-error count across all voice frames (mbelib's `errs2`) |
| `voice_frames_holding` | voice frames whose `errs2` exceeded 5 — mbelib rejected them as too corrupt and concealed by holding the previous frame |
| `voice_error_max` | worst single-frame `errs2` seen |
| `derotator_slips` | (CQPSK) times the quarter-turn derotation was re-pinned after first lock — a carrier-bias slip or a re-acquisition on another turn. Each one used to silence the framer until the next hard re-acquire (an 11 s hole on a clean transmission, 2026-09-09); now costs at most one frame |
| `mean_voice_quality` | mean composite `VoiceQuality` score (0..1) across all voice frames — combines FEC error count, pre-FEC demodulator confidence, and (CQPSK) carrier lock; a fuller picture than `voice_frame_errors` alone, which can miss a frame that passed FEC clean while every symbol sat on a decision boundary the whole time |
| `voice_frames_low_quality` | voice frames whose composite score fell under 0.5 — a superset of `voice_frames_holding` that also catches the low-confidence-but-FEC-clean case above |
| `voice_frames_inferred` | voice frames from LDUs whose sync word or NID was lost and that were decoded on the LDU1/LDU2 cadence instead (the framer *coasts* up to two frames mid-voice; see below) |
| `voice_frames_concealed` | 20 ms slots the channel was on the air for with nothing decodable, filled with held-and-faded audio so the call's timeline stays honest |
| `ess_valid` / `ess_invalid` | LDU2 Encryption Sync fields that passed / failed their Reed–Solomon check. An invalid field says nothing about encryption; only a valid one can mute a call |
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

## `talker_aliases` (added 2026-08-21)

`[{"tg": 20308, "alias": "ENG 21"}]` — over-the-air aliases confirmed on this
channel by `hs_p25::talker_alias`: the longest printable run in the Motorola
alias Link Control words (MFID 0x90, LCO 0x15/0x17), accepted only after the
same text repeats. The field layout of those words is deliberately *not*
assumed; `vendor_lc_samples` keeps their raw arguments so a real capture can
turn this into a proper parser. Empty on every capture in the corpus so far.

## Receiver event trace (`HS_CQPSK_TRACE`, added 2026-09-09)

Set `HS_CQPSK_TRACE=1` in the environment and the CQPSK receiver prints its
acquisition and recovery events to stderr, each stamped with its symbol
count (≈ 4800/s): every blind-acquisition window with its coherence
(`ACQUIRED` or `fail`), tap resets to identity, watchdog trips with the
smoothed decision error and which recovery they chose, and non-finite-sample
re-acquires. It is how the 858.9875 MHz dropout was diagnosed: the receiver
acquired at 5.8 s and reported nothing for the next 24 s while the framer saw
no sync words for 11 of them — a silent false lock, not a lost one.

```sh
HS_CQPSK_TRACE=1 hoosier-sdr --cqpsk --offset 1325k --rate 9600000 --log out.json capture.cs16 2> trace.txt
```

## Voice continuity (added 2026-09-10)

Three policies decide whether a marginal signal sounds choppy, and the fields
above are how to see them working:

- **Encryption is decided from validated evidence only.** The LDU2
  Encryption Sync is decoded through its Hamming(10,6) and RS(24,16) layers
  (`hs_p25::ess`). A field that validates sets the verdict for the
  transmission in either direction; one that fails changes nothing. (The
  first version read the raw ALGID and latched "encrypted" on any bit error,
  which muted the rest of most transmissions on a marginal simulcast
  channel.) A call started from a clear grant starts out clear. On a busy
  site expect `ess_valid` to dominate; a large `ess_invalid` share means the
  channel is barely decodable at all.
- **A lost sync or NID does not cost the frame.** Mid-voice, on a NAC seen
  on three clean NIDs, the framer presumes the next frame is there at the
  protocol cadence and infers LDU1/LDU2 from the alternation, for up to two
  frames in a row. The nine IMBE frames carry their own FEC, so the vocoder
  judges them individually. `voice_frames_inferred` counts them.
- **Missing time is filled, not deleted.** When a real sync arrives after
  unframed air, the framer reports the gap and the decoder emits one
  concealment frame per 20 ms slot (up to 0.9 s), so a lost stretch is a
  fade rather than a jump. `voice_frames_concealed` counts those.

A useful single number for a capture is *slot coverage*:
`voice_frames / (voice_frames + voice_frames_concealed)` — the share of the
transmission's 20 ms slots that carried decoded audio.
