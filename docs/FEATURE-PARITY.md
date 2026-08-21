# Feature parity: HoosierSDR vs. the established trunked scanners

Compared against SDRTrunk 0.6.1 (+0.7 nightlies), Unitrunker 2.1, DSDPlus 2.547,
OP25 (boatbod/osmocom), Trunk Recorder, rdio-scanner / OpenMHz, ProScan, and the
Uniden SDS100 / BCD536HP feature set that defines what scanner users expect.
Inventories were gathered 2026-08-20 from each project's docs and source.

Legend: ✅ have · 🟡 partial · ❌ missing · ➖ deliberately out of scope for v1
(P25 Phase I only — see ARCHITECTURE.md §8 "what's deliberately late").

## 1. Scanning model

| Feature | SDRTrunk | Unitrunker | DSDPlus | OP25 | Uniden | HoosierSDR |
|---|:-:|:-:|:-:|:-:|:-:|:-:|
| Follow a trunked site from one wideband radio | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ (10 MSPS Airspy, ±3.84 MHz) |
| Control-channel hunt / alternates | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| Auto-detect modulation (C4FM/LSM) | manual | ✅ | ✅ | manual | ✅ | ✅ measured |
| Auto tuner-error correction | ✅ | warp | — | FLL | — | ✅ measured |
| Playlists / favorites (system+site+TG set) | ✅ | ✅ | files | tsv | ✅ | ✅ |
| Lockout / avoid (permanent) | 🟡 mute only | ✅ | ✅ | ✅ | ✅ | ✅ |
| Temporary avoid (timed) | ❌ | ❌ | ❌ | ❌ | ✅ | ✅ 30/60/120 min |
| Priority talkgroups (preempt) | ✅ 1–99 | ✅ rank | ✅ | ✅ | ✅ | ✅ high/normal/low; decoder contention + playback order |
| Hold on talkgroup / site | ❌ | ✅ | ✅ | ✅ | ✅ | ✅ talkgroup |
| Skip current call | ❌ | — | — | ✅ | ✅ | ✅ |
| Delay / hang time | fade timers | ✅ | — | 2 s | ✅ | ✅ setting |
| Whitelist/allowlist | ❌ | tags | threshold | ✅ | ID scan | ✅ |
| Service types / category filter | groups | tags | — | — | ✅ | 🟡 picker only → P2 |
| Quick keys / number tags | ❌ | ❌ | ❌ | presets | ✅ | ❌ → P3 |
| Auto-start on launch | ✅ | — | — | service | ✅ | ✅ opt-in, last playlist |
| Multiple systems at once | ✅ | ✅ | ✅ | ✅ | — | ❌ → P3 (second radio) |
| Skip encrypted grants | ❌ | — | ✅ | ✅ | — | ✅ |
| Discovery (log unknown TGs/freqs) | events | history | auto files | dump | ✅ | 🟡 → P2 |

## 2. Talkgroup / unit management

| Feature | SDRTrunk | Unitrunker | DSDPlus | OP25 | HoosierSDR |
|---|:-:|:-:|:-:|:-:|:-:|
| Talkgroup aliases | ✅ | ✅ | ✅ | ✅ | ✅ |
| RadioReference import (API) | ✅ | ✅ | ❌ | ❌ | ✅ (+ ZIP/state/county browse) |
| CSV import | ❌ | ❌ | files | tsv | ✅ |
| Unit/radio ID aliases | ✅ | ✅ | ✅ | ✅ | ✅ local table + CSV import (RR API has no roster) |
| Talker alias (OTA) | ✅ | ❌ | ✅ | ❌ | ❌ → P2 (engine) |
| Affiliations / registrations view | ✅ | history | ✅ | ✅ | ❌ → P2 |
| Patch / regroup tracking | ✅ | ✅ | — | ✅ | ✅ shown on calls and site panel |
| Per-alias color / icon | ✅ | ✅ | — | colors | ❌ → P2 |
| Ranges / wildcards | ✅ | ❌ | ❌ | ✅ | ❌ → P2 |

## 3. Audio & recording

| Feature | SDRTrunk | DSDPlus | Trunk Recorder | ProScan | HoosierSDR |
|---|:-:|:-:|:-:|:-:|:-:|
| Live playback of calls | ✅ | ✅ | plugins | ✅ | ✅ (on completion) |
| Per-call recording | MP3/WAV | WAV/MP3 | WAV/M4A | MP3/WAV | WAV |
| Metadata sidecar (JSON) | ❌ | SRT | ✅ | ID3 tags | ✅ trunk-recorder shape |
| Filename formatter | fixed | aliases | fixed | ✅ | 🟡 fixed → P2 |
| Instant replay / replay last | ❌ | ❌ | — | grid | ✅ replay-last (90 s buffer) + per-row |
| Priority preemption of audio | ✅ | ✅ | — | — | ✅ queue order (no mid-call interrupt) |
| Mute / volume | ✅ | ✅ | — | ✅ | 🟡 play on/off → P2 volume |
| Start/drop tones, alert tones | ✅ | ✅ | — | — | ✅ emergency + per-TG 🔔 |
| Duplicate-call suppression (multi-site) | ✅ | — | ✅ | — | ❌ → P3 |
| Auto-prune recordings | ❌ | — | ✅ | ✅ | ❌ → P2 |
| Baseband IQ recording | ✅ | survey | debug | — | ✅ (CLI) |

## 4. Monitor / UI

| Feature | SDRTrunk | OP25 web | rdio-scanner | HoosierSDR |
|---|:-:|:-:|:-:|:-:|
| Active calls ("now playing") | ✅ | ✅ | queue | ✅ with timers |
| Call history with filter | ✅ events | ✅ | ✅ search | ✅ |
| Spectrum waterfall | ✅ | plots | — | ✅ |
| Constellation / eye | ✅ | ✅ | — | ❌ → P2 (real symbols) |
| Decoder-state panel (NAC, WACN, site, neighbours, bandplan) | ✅ | ✅ | — | 🟡 NAC, alternates, band plans, patches (no WACN/neighbours yet) |
| Event log (moves, affiliations, patches, denials) | ✅ | ✅ | — | 🟡 moves, patches, busy, out-of-band, emergency (no affiliations yet) |
| Emergency indication + alert | ✅ | ✅ | LED | ✅ (parser-tested; not yet seen live) |
| Per-TG alert sound / LED color | actions | — | ✅ | ✅ sound |
| Themes | nightly | settings | — | ✅ light/dark |
| Multi-window / detachable | ✅ | — | — | ❌ → P3 |

## 5. Streaming, sharing, export

| Feature | SDRTrunk | Trunk Recorder | OP25 | HoosierSDR |
|---|:-:|:-:|:-:|:-:|
| Call event CSV log | ✅ | JSON | SQL | ✅ calls.csv |
| rdio-scanner upload | ✅ | ✅ | ❌ | ❌ → P2 |
| OpenMHz upload | ✅ | ✅ | ❌ | ❌ → P2 |
| Broadcastify Calls | ✅ | ✅ | ❌ | ❌ → P2 |
| Icecast / Broadcastify feed (live) | ✅ | via liquidsoap | ✅ | ❌ → P3 (needs MP3/Opus encoder) |
| Status API / WebSocket / MQTT | ❌ | ✅ | HTTP | ❌ → P3 |
| Script hook on call | actions | ✅ | — | ❌ → P2 |

## 6. Radio / hardware

| Feature | SDRTrunk | Unitrunker | HoosierSDR |
|---|:-:|:-:|:-:|
| RTL-SDR, Airspy R2/Mini | ✅ | ✅ | ✅ R2 / RTL (Mini: untested) |
| HackRF, SDRplay, BladeRF | ✅ | HackRF | ➖ |
| Multiple tuners pooled | ✅ | ✅ | ❌ → P3 |
| Gain / PPM controls | ✅ | ✅ | 🟡 gain (RTL) → P2 PPM |
| Auto-PPM from decoder | ✅ | — | ✅ (per run) |
| Airspy gain control | ✅ | ✅ | ❌ R2 firmware hangs (documented) |

## 7. Protocols & location

| Feature | SDRTrunk | DSDPlus | HoosierSDR |
|---|:-:|:-:|:-:|
| P25 Phase I | ✅ | ✅ | ✅ (pre-detection equalizer — unique) |
| P25 Phase II TDMA | ✅ | ✅ | ➖ (patent window, ARCHITECTURE §5) |
| DMR / NXDN / EDACS / LTR | ✅ / — | ✅ | ➖ |
| LRRP / GPS map | ✅ | ✅ | 🟡 decoded, not shown → P2 map |
| Transcription | ❌ | ❌ | 🟡 crate scaffold → Phase 4 |

## Priority plan

**P1 — scanner must-haves — done 2026-08-20 (PR #15):** priority talkgroups with audio preemption; hold / skip / timed avoid; hang time exposed; auto-start last playlist; unit-ID aliases (RR import + editable); emergency + per-TG alert tones; patches shown; event log panel; decoder-state panel (NAC / WACN / system / site / neighbours); call JSON sidecar + CSV call log; replay-last.

**P2:** rdio-scanner / OpenMHz / Broadcastify Calls uploaders; script hook; volume; filename formatter; auto-prune; talker alias; affiliations view; alias colors; ranges; PPM; real constellation; LRRP map; discovery.

**P3:** multiple tuners / systems; Icecast live feed; status API; quick keys; multi-window; duplicate suppression.

**Out of scope for v1:** Phase II, DMR/NXDN/EDACS/LTR, HackRF/SDRplay — per the roadmap; everything else above is on the list.
