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
| Service types / category filter | groups | tags | — | — | ✅ | ✅ live chips (playlist ∩ categories) |
| Quick keys / number tags | ❌ | ❌ | ❌ | presets | ✅ | ❌ → P3 |
| Auto-start on launch | ✅ | — | — | service | ✅ | ✅ opt-in, last playlist |
| Multiple systems at once | ✅ | ✅ | ✅ | ✅ | — | ❌ → P3 (extra radios now pool on one site; a second system needs a second follower) |
| Skip encrypted grants | ❌ | — | ✅ | ✅ | — | ✅ |
| Discovery (log unknown TGs/freqs) | events | history | auto files | dump | ✅ | ✅ Discovery tab: every grant, unnamed filter, name-it, CSV export |

## 2. Talkgroup / unit management

| Feature | SDRTrunk | Unitrunker | DSDPlus | OP25 | HoosierSDR |
|---|:-:|:-:|:-:|:-:|:-:|
| Talkgroup aliases | ✅ | ✅ | ✅ | ✅ | ✅ sortable columns, filter by source (system / CSV) |
| RadioReference import (API) | ✅ | ✅ | ❌ | ❌ | ✅ (+ ZIP/state/county browse) |
| CSV import | ❌ | ❌ | files | tsv | ✅ |
| Unit/radio ID aliases | ✅ | ✅ | ✅ | ✅ | ✅ local table + CSV import (RR API has no roster) |
| Talker alias (OTA) | ✅ | ❌ | ✅ | ❌ | 🟡 Motorola words assembled, printable text confirmed by repetition; parser-tested, **not yet seen live** (no capture carries alias words); learned into radio IDs on request |
| Affiliations / registrations view | ✅ | history | ✅ | ✅ | ✅ (0x28/0x2B/0x2C/0x2F parsed; Discovery tab) |
| Patch / regroup tracking | ✅ | ✅ | — | ✅ | ✅ shown on calls and site panel |
| Per-alias color / icon | ✅ | ✅ | — | colors | ✅ colour per talkgroup or range; calls and now-playing tinted |
| Ranges / wildcards | ✅ | ❌ | ❌ | ✅ | ✅ talkgroup range rules (lock/priority native in the engine, colour, alert); radio-ID regex rules with `$1` |

## 3. Audio & recording

| Feature | SDRTrunk | DSDPlus | Trunk Recorder | ProScan | HoosierSDR |
|---|:-:|:-:|:-:|:-:|:-:|
| Live playback of calls | ✅ | ✅ | plugins | ✅ | ✅ (on completion) |
| Per-call recording | MP3/WAV | WAV/MP3 | WAV/M4A | MP3/WAV | WAV/MP3/M4A/Opus, quality, CBR/VBR; per-talkgroup record / stream / upload policy (default + exceptions, bulk from the alias table) |
| Metadata sidecar (JSON) | ❌ | SRT | ✅ | ID3 tags | ✅ trunk-recorder shape |
| Filename formatter | fixed | aliases | fixed | ✅ | ✅ template with 15 tokens, sub-folders, sanitised |
| Instant replay / replay last | ❌ | ❌ | — | grid | ✅ replay-last + library playback, listen mode |
| Priority preemption of audio | ✅ | ✅ | — | — | ✅ queue order (no mid-call interrupt) |
| Mute / volume | ✅ | ✅ | — | ✅ | ✅ slider + mute (live, replay, library) |
| Start/drop tones, alert tones | ✅ | ✅ | — | — | ✅ emergency + per-TG 🔔 |
| Duplicate-call suppression (multi-site) | ✅ | — | ✅ | — | ❌ → P3 |
| Auto-prune recordings | ❌ | — | ✅ | ✅ | ✅ by age, starred kept |
| Baseband IQ recording | ✅ | survey | debug | — | ✅ (CLI) |

## 4. Monitor / UI

| Feature | SDRTrunk | OP25 web | rdio-scanner | HoosierSDR |
|---|:-:|:-:|:-:|:-:|
| Active calls ("now playing") | ✅ | ✅ | queue | ✅ with timers |
| Call history with filter | ✅ events | ✅ | ✅ search | ✅ |
| Spectrum waterfall | ✅ | plots | — | ✅ |
| Constellation / eye | ✅ | ✅ | — | ✅ control-channel I/Q (CQPSK) or level-transition (C4FM) |
| Decoder-state panel (NAC, WACN, site, neighbours, bandplan) | ✅ | ✅ | — | ✅ NAC, WACN, system, RFSS/site, alternates, band plans, patches, neighbours (0x3C) |
| Event log (moves, affiliations, patches, denials) | ✅ | ✅ | — | ✅ moves, patches, busy, out-of-band, emergency, aliases, positions; affiliations in their own table |
| Emergency indication + alert | ✅ | ✅ | LED | ✅ (parser-tested; not yet seen live) |
| Per-TG alert sound / LED color | actions | — | ✅ | ✅ sound; **Alerts tab**: keyword / emergency / talkgroup / radio triggers → Telegram message + MP3 (with earlier calls combined), optional Ollama AI gate, per-talkgroup cooldown, firing log |
| Themes | nightly | settings | — | ✅ light/dark |
| Multi-window / detachable | ✅ | — | — | ❌ → P3 |

## 5. Streaming, sharing, export

| Feature | SDRTrunk | Trunk Recorder | OP25 | HoosierSDR |
|---|:-:|:-:|:-:|:-:|
| Call event CSV log | ✅ | JSON | SQL | ✅ SQLite library + JSON sidecars (CSV export via cart) |
| rdio-scanner upload | ✅ | ✅ | ❌ | ✅ |
| OpenMHz upload | ✅ | ✅ | ❌ | ✅ |
| Broadcastify Calls | ✅ | ✅ | ❌ | ✅ |
| Icecast / Broadcastify feed (live) | ✅ | via liquidsoap | ✅ | ✅ via ffmpeg |
| Status API / WebSocket / MQTT | ❌ | ✅ | HTTP | ❌ → P3 |
| Script hook on call | actions | ✅ | — | ✅ env + JSON on stdin, off-thread, timeout |

## 6. Radio / hardware

| Feature | SDRTrunk | Unitrunker | HoosierSDR |
|---|:-:|:-:|:-:|
| RTL-SDR, Airspy R2/Mini | ✅ | ✅ | ✅ R2 / RTL (Mini: untested) |
| HackRF, SDRplay, BladeRF | ✅ | HackRF | ➖ |
| Multiple tuners pooled | ✅ | ✅ | ✅ one radio on control, the others parked over the rest of the span (auto plan in Devices) |
| Gain / PPM controls | ✅ | ✅ | ✅ gain (RTL), PPM (both; "use measured" from the control channel) |
| Auto-PPM from decoder | ✅ | — | ✅ (per run) |
| Airspy gain control | ✅ | ✅ | ❌ R2 firmware hangs (documented) |

## 7. Protocols & location

| Feature | SDRTrunk | DSDPlus | HoosierSDR |
|---|:-:|:-:|:-:|
| P25 Phase I | ✅ | ✅ | ✅ (pre-detection equalizer — unique) |
| P25 Phase II TDMA | ✅ | ✅ | ➖ (patent window, ARCHITECTURE §5) |
| DMR / NXDN / EDACS / LTR | ✅ / — | ✅ | ➖ |
| LRRP / GPS map | ✅ | ✅ | ✅ positions on an OSM tile map + table (parser unverified on air, see lrrp.rs) |
| Transcription | ❌ | ❌ | ✅ faster-whisper / openai-whisper, model picker, editable, searchable |

## Beyond the matrix (2026-08-20/21)
Call library with capture-time SHA-256, full-text search (names + transcripts), listen/archive mode, cart → export with a chain-of-custody manifest (`manifest.json` + `manifest.sha256`), transcript editing kept separate from machine text, whisper model pre-download, Aliases tab (SDRTrunk alias-list model) with "is this talkgroup loaded?".

## Priority plan

**P1 — scanner must-haves — done 2026-08-20 (PR #15):** priority talkgroups with audio preemption; hold / skip / timed avoid; hang time exposed; auto-start last playlist; unit-ID aliases (RR import + editable); emergency + per-TG alert tones; patches shown; event log panel; decoder-state panel (NAC / WACN / system / site / neighbours); call JSON sidecar + CSV call log; replay-last.

**P2 — done 2026-08-21:** rdio-scanner / OpenMHz / Broadcastify Calls uploaders; script hook; volume; filename formatter; auto-prune; talker alias (plumbing + conservative decoder — awaiting a live capture with alias words); affiliations view; alias colors; ranges / wildcards; PPM; real constellation; LRRP map; discovery; service-type filter. Engine side: TSBK 0x28/0x2B/0x2C/0x2F/0x3C; RFSS/site and WACN now stored (NetworkStatus was parsed but never kept, and its WACN/system fields were split wrong — caught by a new test over the NAC 0x260 capture, which now decodes WACN 0xBEE00 / system 0x262 with a neighbour naming the same system); `AffiliationTable`, native lockout/priority ranges, symbol ring for the constellation.

**P3:** multiple tuners / systems; Icecast live feed; status API; quick keys; multi-window; duplicate suppression.

**Out of scope for v1:** Phase II, DMR/NXDN/EDACS/LTR, HackRF/SDRplay — per the roadmap; everything else above is on the list.

## Alerts (2026-08-21)

`app/src/alerts.rs`. Trigger → optional AI gate → actions. Keyword triggers run when the transcript lands (whole-word, case-insensitive, any of the phrases, on the chosen talkgroups); emergency / talkgroup / radio triggers run when the call completes. Telegram: `sendMessage`, or `sendAudio` with an MP3 made by ffmpeg (WAV as `sendDocument` without it); the triggering call can be concatenated with the previous N calls on the same talkgroup within a window. The AI gate posts the transcript and the alert's prompt to a local Ollama `/api/generate` in JSON mode with `think: false` (a thinking model otherwise returns an empty response — measured with qwen3.6 and gemma4 locally) and expects `{"fire", "summary"}`; when Ollama is unreachable the alert **fails open** by default and says so in the message. Bot token in the Keychain; all HTTP from Rust. The webview has no native `alert`/`confirm` (Tauri v2), which is why playlist deletion and cart export looked stuck; both now use in-app dialogs.

## Listen groups, queue limits, accordions (2026-08-21)

Groups: named sets of talkgroups made from ticked rows in Aliases; a chip on the Monitor mutes/unmutes the whole group (mute = added to the engine lockout). Scanning settings: *calls decoded at once* (1–6, `TrunkFollower::set_max_calls`) and *drop queued audio older than N s* (player prunes stale clips at dequeue) — the fixes for garbled audio and a playback queue minutes behind the radio on a busy site. Call history on the Monitor shows transcripts as they land and filters on them. The left-column panels are collapsible. Fixed: each run now has its own stop flag (a Stop→Start on another radio no longer resurrects the old loop), the RTL-SDR streamer deactivates before close, and switching the radio selector while live stops first.

## Busy-site decoding (2026-08-21)

Traffic channels are now sliced out of one FFT of the band per block (`hs_dsp::channelizer`, overlap-save, 48 kHz out) instead of each running its own decimator from the wideband stream; the control channel keeps its direct path. The channelizer slice is tapered (raised cosine 8→22 kHz, a real lowpass instead of a brick wall, so slice-edge energy cannot wrap around in overlap-save) and channel-filtered again at 48 kHz before its decoders. The first live run of the channelizer garbled audio badly; the synthetic CQPSK chain is too marginal to adjudicate neighbour behaviour (four equal-level neighbours defeat the classic path too), so Settings → Scanning → **Channel extraction** keeps the classic per-channel decimator as an on-air A/B switch. Both modulations are still decoded per call by default (a C4FM discriminator on CQPSK syncs and emits garbled audio, so a wrong site measurement would otherwise go uncaught); `set_single_modulation` is available but off. Each call's decoders run on their own thread per block. Measured on 2 s of 9.6 MSPS with 12 calls up (dual decoders, `twelve_calls_at_once_realtime_factor`, release): **before 0.33× real time** — the follower could not keep up, which is what garbled every call at once on a busy site — **after 1.95×** on one laptop, ~3× once the site modulation is confirmed. Field-capture tests and the synthetic grant→audio test are unchanged in outcome; `marion.cu8` (2.4 MSPS, real control channel) decodes identically before and after. No second tuner is involved; extra radios remain the P3 multi-system item. Calls-at-once ceiling raised 6 → 24 (default 12).

## Devices, forced modulation, Discovery playback (2026-08-21)

**Devices tab**: Airspys (`airspy_list_devices`) and RTL-SDRs (Seify enumerate) are detected on start and on Rescan; the top-bar Radio picker lists exactly those, defaulting to the saved radio if attached, else the first Airspy, else the first RTL-SDR. Per-radio settings (nickname, ppm, gain, preferred rate) are keyed by serial in `devices.json` and used when that radio starts; `open_device` opens a specific serial / Seify args. **Traffic modulation** (Tuning panel): auto (both decoders, cleaner wins), or CQPSK / C4FM only — for a simulcast site that RadioReference says is CQPSK, forcing it removes the chance of a C4FM decode winning the arbitration and playing garbled audio. **Discovery ▶** plays the newest recorded call on a talkgroup so it can be named from what was actually said.

## Conversations (2026-08-21)

`app/src/conversations.rs`. A rule names talkgroups and the fixed party's radio IDs (listed, and/or learned: a radio heard in ≥3 and ≥60 % of conversations on the talkgroup is proposed in the editor). Transmissions group into incidents keyed by (talkgroup, mobile unit); a fixed party's transmission joins the most recently active incident; a different mobile unit is a different incident. After `end_gap_secs` (90 s default) of quiet — waiting up to 90 s more for whisper — the transcripts are stitched with UNIT/FIXED labels, Ollama writes a summary from the rule's prompt (free-text completion, `think:false`), every transmission's audio is combined into one MP3, and it goes to the rule's Telegram chat (default: the alerts' chat). A transmission within `late_window_secs` reopens the incident; the revised summary deletes the earlier Telegram messages (ids kept) and sends again, marked "revised ×n". "Test on recent calls" runs the whole path on the newest run of calls on the rule's talkgroups. Alerts tab → Conversations.

## Band coverage with several radios (2026-08-21)

`TrunkFollower` now owns a primary `Band` (control channel + the calls inside its span) and any number of extra `Band`s (`add_band`, fed by `process_band`). A grant is decoded in whichever band covers its frequency; calls are counted, prioritised and contended across all bands; only a grant no band covers is "out of band". The app opens the extra radios alongside the primary, each with its own reader thread, and drains them between the primary's blocks (`Buffered::try_read`). Devices → **Band coverage** plans it: the site span comes from the playlist (now saved with it) or is typed; each other attached radio marked *cover* is parked, widest first, on the largest uncovered stretch; the plan and any uncovered remainder are shown. Verified synthetically (`a_grant_outside_the_primary_band_decodes_on_an_extra_radio`): the extra band's call decodes while the primary's still does. Tuner error is measured on the control channel and applied to every band's channel offsets.

## Audio quality (2026-08-21)

The speaker path upsampled 8 kHz audio to the device rate by **linear interpolation with no anti-imaging filter**: a 3 kHz formant's image sat at 5 kHz only 6.6 dB down, a 1 kHz tone's at 7 kHz 17 dB down — the thin, metallic sound listeners reported. `player::SincInterp` (32-tap Blackman-windowed sinc, 128 phases) replaces it; `sinc_interpolation_suppresses_images` measures the 7 and 9 kHz images of a 1 kHz tone at −88 / −91 dB. The stored calls themselves have a normal speech spectrum (no clipping; energy 300–3000 Hz), so the decode was not the main culprit. Vocoder: mbelib renders unvoiced bands from `uvquality` sine components (its default 3); now a Settings control (default 16) since smoother unvoiced synthesis is the other half of "metallic". Channel extraction remains switchable (channelizer / classic) for an on-air A/B.
