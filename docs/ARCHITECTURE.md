# HoosierSDR — Architecture & Roadmap
**Status:** design doc, v1 · **Date:** 2026-08-16
**Decisions locked:** Rust DSP core from scratch · desktop-first, macOS primary with Windows port · v1 gated on decode quality
**Not legal advice.** The legal section is research, not counsel.
---
## 0. Two corrections before anything else
**"Whisper for automatic text-to-speech" — you mean speech-to-text.** TTS synthesizes voice from text; Whisper transcribes voice into text. Small slip, but worth fixing now because it changes what you search for and what you'd ask a contributor to build.
**Hoosier SAFE-T is Project 25 Phase I, not Phase II.** I assumed Phase II in my first pass and I was wrong. Per the RadioReference database (SysID `6BD`, WACN `BEE00`, 84 sites as of the 2026-08-11 revision), SAFE-T is Phase I FDMA today. Phase II TDMA is in *pilot* at exactly two sites — Fort Wayne and Westville Correctional — with IPSC describing statewide rollout as "a multi-year project" if approved.
This correction is load-bearing, and in your favor twice over:
- **Legally.** The IMBE vocoder (Phase I) is out of patent — every DVSI patent covering it expired by ~2017-18. The AMBE+2 *half-rate* vocoder (Phase II) is covered by **US 8,359,197, active until 2028-05-20, with explicit decoding claims (42, 60, 72)**. So you can ship a complete, self-contained Phase I decoder in the US today. Phase II is the encumbered one.
- **Technically.** Phase I C4FM and Phase I LSM/CQPSK are where your local target system lives, so that's where your optimization effort belongs — and it's also where the open-source gap is widest.
---
## 1. The thesis
> **HoosierSDR is the first P25 decoder that equalizes the channel before differential detection.**
That sentence is the whole project. Everything else is scaffolding around it.
### Why it's a real gap, not a marketing line
I read the source of SDRTrunk, OP25 (boatbod), DSD-FME, trunk-recorder, and GopherTrunk. Findings:
| Technique | Who implements it for P25 |
|---|---|
| Coherent I/Q CQPSK + Gardner TED + Costas loop | OP25, trunk-recorder, SDRTrunk |
| Scalar "balance + gain" corrector re-fit at each sync | SDRTrunk (called `Equalizer`, but it's `(symbol + pll) * gain` — no taps, cannot invert ISI) |
| **CMA blind equalizer** | SDRTrunk has `CMAEqualizer.java` — **dead code, zero call sites**. GopherTrunk (Go) wires one in. |
| **Sync-trained LMS fractionally-spaced equalizer** | SDRTrunk added a real one on 2026-07-08 (`nxdn/layer1/C4FMEqualizer.java`) — **wired to NXDN only**, not P25, not DMR |
| **Decision feedback equalizer** | **Nobody** |
| **MLSE / Viterbi equalization** | **Nobody** |
| **Multi-symbol differential detection** | **Nobody** |
And the structural point that matters most: every open-source CQPSK chain is ordered
```
gardner_cc → diff_phasor_cc → costas_loop_cc → complex_to_arg → slicer
```
Differential detection happens *before* any place an equalizer could go. The π/4-DQPSK literature is unanimous that this is backwards — differential detection is a nonlinearity that scrambles ISI irrecoverably. There is no downstream fix. OP25 and trunk-recorder are both GNU Radio applications and GNU Radio 3.10 already ships `digital.decision_feedback_equalizer` with training-sequence support. Neither calls it.
### Why the physics favors you
| Quantity | Value |
|---|---|
| P25 symbol rate | 4800 baud → **208 µs** symbol period |
| C4FM simulcast delay-spread tolerance | ~25–40 µs |
| LSM/CQPSK delay-spread tolerance | ~55–70 µs |
| Delay spread as fraction of symbol | **~0.12 – 0.34 T** |
| Free training data | 24-symbol Frame Sync Word, **every 180 ms** |
This is about as friendly as equalization problems get. Short known ISI, periodic free training, and a 2026 CPU with cycles to burn. A T/2 fractionally-spaced equalizer spanning 2–3 symbols is theoretically sufficient — you are not solving an HF ionospheric channel. MLSE over this memory length is computationally trivial.
**Reality check that should keep you honest:** SDRTrunk is widely considered best-in-class on simulcast *without any equalizer at all*, purely on the strength of careful timing recovery and sync-anchored re-optimization. That tells you two things — the equalizer is remaining headroom, not the whole game; and if your timing recovery is sloppy, a beautiful equalizer won't save you.
### Prior art you must look at before writing a line: GopherTrunk
`github.com/MattCheramie/GopherTrunk` — Go, Apache-2.0, v0.7.1, actively developed as of 2026-08-15. It has `internal/dsp/equalizer/{cma,lms,fse}.go` and wires a T/2 fractionally-spaced CMA into `p25/phase1/receiver/cqpsk.go`. Its own docs articulate the same thesis you're chasing.
Caveats: it's an AI-agent-built project (~200k lines, ships `CLAUDE.md`), and its changelog is candid that several DSP features are "validated in unit tests but not yet confirmed against a live" radio. I found **no independent field benchmark** of it.
**Do not skip this.** Either it works — in which case your differentiator evaporates and you should reconsider — or it doesn't, in which case its architecture is a free head start and its failure modes tell you where the real difficulty is. Spend a day running it against a Lake County recording before committing months.
---
## 2. Stack
| Layer | Choice | Why |
|---|---|---|
| DSP + protocol core | **Rust**, `no_std`-friendly where practical | Your call, and correct. Memory safety in a codebase full of ring buffers and bit-packing, plus fearless parallelism across channels. |
| Hardware I/O | **Seify** (`seify`, `seify-rtlsdr`) with a `SdrSource` trait | Pure-Rust RTL-SDR driver. Avoids the libusb/JNI layer that is *currently breaking SDRTrunk on macOS Tahoe 26.x* — the README's top item is a `brew install libusb --HEAD` workaround. A pure-Rust USB path is a genuine, immediate Mac advantage. Keep a `soapysdr` feature flag for exotic hardware. |
| FFT / filters | `rustfft`, `futuredsp`, hand-written FIR with explicit SIMD | On Apple Silicon, `vDSP`/Accelerate via FFI is worth benchmarking against portable SIMD before assuming it wins. |
| UI | **Tauri v2** (Rust core + web frontend) | You said Windows is coming. Tauri gives you one Rust core and a UI that ports for free, in a ~10 MB binary rather than Electron's ~150 MB. Cost: waterfall/constellation rendering must go through WebGL/WebGPU on a canvas, not DOM. Budget real time for that — it's the one place Tauri is worse than egui. |
| Audio out | `cpal` | Cross-platform, Core Audio on macOS, WASAPI on Windows. |
| Transcription | `whisper-rs` (Metal/Vulkan) **and** a transducer path — see §6 | Two engines behind one trait. |
| Storage | SQLite via `rusqlite`, FTS5 for transcript search | Boring and correct. |
| Credentials | OS keyring (`keyring` crate → Keychain / DPAPI) | SDRTrunk stores RadioReference passwords in **plaintext** in the user profile. Beat that on day one; it's free. |
| License | **Apache-2.0** for your code | MIT is fine too, but Apache-2.0 §3 carries an express patent grant, which matters in a domain this patent-adjacent. **Not GPL** — see §5. |
### Hardware note you'll hit within a week
Lake County Simulcast (RR site 24666) spans **851.875 → 856.6625 MHz — roughly 4.8 MHz of occupied spectrum** across nine channels, three of them control channels.
An RTL-SDR gives you ~2.4 MHz of usable bandwidth. **It physically cannot see that whole site with one dongle.** You'd need two or three dongles with independent clocks, which complicates everything.
An **Airspy R2 (10 MHz) covers it in one shot**, with a much better ADC. If you have one, develop against it. If you don't, buy one before you start — it is the single highest-leverage $170 in this project. Keep RTL-SDR support (it's most users' hardware), but do not let it constrain your architecture.
HackRF's 8-bit ADC is genuinely poor for P25. Don't optimize for it.
---
## 3. Crate layout
```
hoosier-sdr/
├─ crates/
│  ├─ hs-source/          SdrSource trait; seify-rtlsdr, seify-airspy, iq-file backends
│  ├─ hs-dsp/             filters, resamplers, channelizer, AGC, TED, PLL
│  │                      equalizer/  ← lms.rs, fse.rs, dfe.rs, mlse.rs  (the moat)
│  ├─ hs-p25/             frame sync, NID, FEC (BCH/Golay/RS/trellis), TSBK/MBT parsing
│  ├─ hs-vocoder/         Vocoder trait
│  │   ├─ imbe/           Phase I IMBE — SHIPPED IN-TREE (patents expired)
│  │   └─ plugin/         dynamic-loaded Phase II AMBE+2 — NOT SHIPPED (see §5)
│  ├─ hs-trunk/           site/system state machine, control channel following, grants
│  ├─ hs-catalog/         RadioReference client, CSV import, FCC ULS, local SQLite
│  ├─ hs-transcribe/      VAD gate → ASR trait → hallucination filter → normalizer
│  ├─ hs-core/            orchestration, scan lists, call router, recording
│  └─ hs-bench/           BER harness, IQ corpus runner  ← build this FIRST
└─ app/                   Tauri v2 shell + web UI
```
### `hs-bench` comes first, before any decoder code
You cannot claim "better than anything else" without a number. Build the measuring instrument before the thing being measured.
- Capture 30–60 minutes of raw IQ from 3–4 SAFE-T simulcast sites at varying signal quality — Lake, Hendricks, Elkhart, plus one clean non-simulcast site as control.
- Store as a versioned corpus (Git LFS or a plain S3 bucket; the files are large).
- Harness reports, per recording: **sync-loss rate, BER pre-FEC, FEC correction rate, TSBK decode rate, voice frame error rate, calls successfully audio-decoded.**
- Run the same corpus through SDRTrunk (nightly), OP25, and GopherTrunk. Those are your baselines, in a checked-in results table.
- Wire it into CI. Every PR reports the delta.
This artifact is worth more to the project than the first three months of decoder work. It's also what turns "I think it sounds better" into a claim contributors and users will believe.
---
## 4. The receive chain
```
SDR ─► DC/IQ correction ─► polyphase channelizer (all site channels at once)
                                         │
                    ┌────────────────────┴────────────────────┐
                    ▼                                          ▼
            CONTROL CHANNEL                              VOICE CHANNELS (N)
                    │                                          │
        matched filter / RRC                        matched filter / RRC
                    │                                          │
        ┌───────────▼──────────┐                   ┌───────────▼──────────┐
        │  ADAPTIVE EQUALIZER  │  ◄── sync-trained │  ADAPTIVE EQUALIZER  │
        │  T/2 FSE, LMS→DFE    │      on 24-sym    │                      │
        └───────────┬──────────┘      FSW          └───────────┬──────────┘
                    │   ⚠ BEFORE differential detection         │
        timing recovery (Gardner) + carrier (Costas)            │
                    │                                          │
        differential detection ─► slicer            differential detection ─► slicer
                    │                                          │
        NID / FEC / TSBK parse                      voice frames ─► Vocoder trait
                    │                                          │
            trunking state machine ──── grants ───────────────► channel allocator
                                                               │
                                              PCM ─► audio out + recorder ─► transcriber
```
**Build order inside the equalizer module:**
1. **LMS T/2 FSE, sync-trained.** ~12 taps (6 symbols at T/2). Train on the FSW, freeze taps, retrain when error variance crosses threshold. This is exactly the design SDRTrunk already proved works — in NXDN. Port the *idea*, not the code (it's GPL; see §5).
2. **CMA fallback** for when sync is lost and you have no training reference.
3. **DFE** — feedforward + feedback taps. The literature on differentially-detected PSK in short ISI points here.
4. **MLSE** over 2–3 symbols of memory. Optimal detector for known-short ISI. Nobody in the P25 world has one. Expensive in CPU, trivial by 2026 standards.
5. **Delay-spread-adaptive equalizer length** — vary tap count with estimated spread as the user moves through the coverage footprint.
Ship 1 and measure. If step 1 doesn't beat SDRTrunk on your corpus, the thesis is in trouble and you should find out in month two, not month twelve.
---
## 5. Legal and licensing — the hard rules
These are constraints, not preferences. Violating them can kill the project outright; the one real casualty in this ecosystem (OpenEar, 2020) was taken down for **GPL contamination**, not patents.
### Rule 1 — Never decrypt. Not ever, not as an option, not "for research."
18 U.S.C. § 2511(2)(g)(ii)(II) makes monitoring unencrypted public-safety radio lawful. § 2510(16)(A) removes that protection the moment the traffic is "scrambled or encrypted." Decrypting P25 ADP/DES/AES you aren't authorized to receive is an ECPA violation, full stop. This is the brightest line in the entire domain.
Build the refusal into the architecture: detect the ALG ID, surface an encryption badge in the UI, skip the channel, move on. RadioReference's API exposes an `enc` attribute on frequencies and talkgroups (added in service v15) — use it to avoid even tuning channels that will never decode.
Relevant locally: IPSC policy encourages the statewide ADP key for encrypted talkgroups while discouraging encryption on dispatch and interop. But county LE is going dark fast — Wayne County went fully encrypted in Jan 2025, Hendricks in 2026. Expect the encrypted set to grow.
### Rule 2 — Vocoder split by phase
| | Patent status | Ship it? |
|---|---|---|
| **Phase I IMBE** | All DVSI patents expired (~2017–18) | **Yes, in-tree.** This is your local system. |
| **Phase II AMBE+2 half-rate** | **US 8,359,197 active to 2028-05-20**, decoding claims 42/60/72 | **No.** Plugin boundary, user-supplied. |
| Outside the US | EP counterpart expired 2024-03-26 | Phase II likely clear in EU |
The Phase II plugin pattern is exactly what SDRTrunk does with JMBE: define a `Vocoder` trait, load a dylib at runtime, ship a build helper, don't distribute the binary. It has a 10+ year track record of nobody getting sued.
One thing to verify yourself: Google Patents lists 8,359,197 as Active, but I could not reach USPTO Patent Center to confirm the 11.5-year maintenance fee was paid in 2024. **If it lapsed, the patent is already dead and Phase II opens up.** Ten minutes at patentcenter.uspto.gov answers this.
### Rule 3 — What you may and may not touch
| Source | License | Verdict |
|---|---|---|
| **mbelib** (`szechyjs/mbelib`, `lwvmobile/mbelib`) | **ISC** | ✅ Vendor or port into Apache-2.0 with attribution. Contains both IMBE and half-rate paths. The most valuable permissive asset in the ecosystem. |
| **DSD-FME's own code** | ISC | ✅ Same |
| `mbelib-neo` | **GPL-2.0+** | ❌ Relicensed. Do not touch. |
| OP25 (incl. Yazev IMBE) | GPLv3+ | ❌ |
| trunk-recorder | GPL-3.0 | ❌ |
| SDRTrunk, JMBE | GPL-3.0 | ❌ |
| dsd-neo | GPL-3.0+ | ❌ |
| TIA-102.BABA / BAAA specs | Copyrighted, purchasable | ✅ Implement from. ❌ Redistribute text. |
**Translating C or Java to Rust creates a derivative work.** Line-by-line transliteration carries the source license. So do copied constant tables and structure-preserving rewrites.
What you *can* freely take from anything: protocol facts. Frame layouts, bit orderings, Golay/Hamming/RS parameters, deinterleave patterns, superframe structure, slot timing. Facts aren't copyrightable. The specific code expressing them is.
**Practical discipline:** it is fine to read SDRTrunk's `C4FMEqualizer.java` to understand *that* a sync-trained T/2 LMS equalizer works and roughly what tap count and step size are sane. It is not fine to open it in one window and type Rust in the other. Write your equalizer from the adaptive-filtering literature (Haykin, Proakis) and your own measurements. Document that provenance in the repo.
### Rule 4 — Indiana law makes desktop-first the *legally* correct choice
IC 35-44.1-2-7, "Unlawful use of a police radio," Class B misdemeanor. Read the structure carefully, because most people get it wrong: the "while committing a crime" qualifier attaches only to clause (a)(3). Clause **(a)(1) — mere possession of a police radio — is a standalone offense**, and "police radio" is defined in (c) as one that "can be installed, maintained, or operated in a vehicle" or "can be operated while it is being carried by an individual."
Exemption **(b)(7)** covers "a person who uses a police radio only in the person's dwelling or place of business," and definition (c) explicitly excludes "a radio designed for use only in a dwelling."
**So: a desktop app on a Mac at home is squarely exempt. A laptop-plus-dongle in a car, or a phone app, is exposed** unless the user holds an FCC amateur license (b)(6) or written permission from a law enforcement chief executive (b)(5).
Your instinct to build a desktop app is right for a reason you probably didn't intend. Put a clear note in the README. If you ever add a mobile client, the receiver stays home and the phone is a remote *display* — which is a different legal posture and, conveniently, also the better architecture.
### Rule 5 — Don't commit RadioReference data
RR's terms prohibit reproducing "table data" without a license. Caching for the user who fetched it is universal practice and fine. **Committing real talkgroup dumps as test fixtures is the accidental violation that will get you a takedown.** Generate synthetic fixtures.
---
## 6. RadioReference integration
**The API:** SOAP/XML only, `https://api.radioreference.com/soap2/?wsdl&v=latest`, currently **version 18**. There is no REST API and RR's CEO has said there are no near-term plans for one. You'll need a SOAP client in Rust — likely hand-rolled XML over `reqwest` with `quick-xml`, since Rust's SOAP tooling is thin. Budget a week; it's tedious, not hard.
**Auth is three fields on every call:** `appKey` (yours, per-app, issued by RR) + `username` + `password` (the *end user's*, and they must hold a Premium subscription — $15/6mo, $30/yr).
**The open-source app-key question, resolved:** SDRTrunk publishes its key in plain sight in GPLv3 source (`RadioReference.java`: `public static final String SDRTRUNK_APP_KEY = "88969092";`) and has for years without apparent consequence. FreeSCAN took the opposite approach and stripped theirs, breaking RRDB import for anyone building from source.
The key is an app *identifier*, not a security boundary — it grants nothing without the user's premium credentials. But there is **no written RR policy** blessing publication, and a published key can be abused and revoked, breaking every install at once. **Apply at radioreference.com/account/api/apply (the form asks explicitly whether your app is open-source — it's an anticipated category, not a disqualifier) and get RR's position in writing by email before you decide.**
**Design:**
- **Three catalog sources behind one trait.** RR API (richest, needs subscription) · CSV import (trunk-recorder's approach — user downloads their own premium CSV, zero app key needed, always works) · on-air self-discovery (free, legal, gives you sites/NAC/WACN/SysID/control channels and observed talkgroup IDs — everything except the human-readable alias).
- **Ship the CSV path first.** It unblocks development entirely and it's the fallback for users who'd rather not hand your app their password.
- **Credentials in the OS keyring.** Never plaintext.
- **Read the `tdma_cc` site attribute** (added in API v18 specifically for this) so you know which control channels are Phase II TDMA — this is how you'll handle the Fort Wayne and Westville pilot sites.
- **Read the `enc` attribute** and grey out encrypted talkgroups in the UI.
- **Auto-select LSM demodulation** when the site is flagged simulcast, as SDRTrunk does.
- FCC ULS bulk downloads are free, public-domain, and give you licensed frequencies with no key — a genuinely open supplement, though no talkgroups or aliases.
---
## 7. Transcription — and why the naive version will disappoint you
**The number that should govern this whole subsystem:** off-the-shelf **Whisper large-v3 scores 50.8% WER on real police radio.** That's from the only rigorous study on this (BPC-CPD: 62,080 manually transcribed Chicago PD transmissions, 96.5% of them under 10 seconds — an almost exact match to your data). Every other word wrong.
For context in the same study: a fine-tuned NeMo Conformer hits **27.3%** — and **human inter-annotator agreement is 25.9–28.9%**. Roughly 13% of transmissions are unintelligible to *humans*. So 27% is the practical ceiling, and off-the-shelf Whisper is nowhere near it.
If you drop raw Whisper in and ship it, users will call it broken, and they'll be right.
### The pipeline, not the model
```
call audio (8 kHz, vocoded)
    → Silero VAD gate          ← highest-value single mitigation
    → resample to 16 kHz
    → ASR (trait: Whisper | Parakeet)
    → hallucination filter     ← blocklist + deloop
    → domain normalizer        ← fuzzy-match against talkgroup/street/unit rosters
    → SQLite FTS5
```
**Hallucination is the acute risk.** Whisper hallucinates on **40.3% of non-speech audio**, and the rate is duration-dependent in a way that's terrible for you: **52.1% at 1 second**, 11.6% at 10 seconds (the minimum), **62.3% at 30 seconds** (Whisper zero-pads everything to 30 s, which is both a compute waste and a hallucination trigger).
Broadcastify's own transcription preview has been caught rendering fire tone-outs as *"Please subscribe, click the bell icon, write a comment, share with friends."* That's your future bug report.
The saving grace: hallucinations are highly repetitive — 121,378 hallucinated outputs across only 1,270 unique strings, with the top phrases accounting for **67% of them**. A blocklist is unusually effective here.
**Mandatory settings:** `condition_on_previous_text = false` (each transmission is independent; carrying context is a documented repetition-loop driver). Note that tuning `no_speech_threshold` alone is documented as *insufficient* — don't rely on it.
### Strongly consider a transducer instead of Whisper
**NVIDIA Parakeet TDT 0.6B v3** (CC-BY-4.0): transducers don't hallucinate the way autoregressive decoders do, handle short utterances natively without 30-second padding, and on an M4 Mac benchmark ran **~6× faster than whisper.cpp large-v3-turbo**. whisper.cpp v1.9.0 (June 2026) added Parakeet support, so you can run both through one ggml runtime with one Metal/Vulkan backend.
**And this is the real argument:** `sherpa-onnx` (Apache-2.0) supports Parakeet with **native hotword biasing** — Aho-Corasick matching with per-word boost scores. That's deterministic vocabulary injection for "MEDIC 4," "signal 13," and 400 Indiana street names. Whisper's `initial_prompt` is a poor substitute: it shares the 224-token context budget (~100–150 proper nouns, nowhere near enough for a county), Whisper was never trained to follow biasing instructions, and the prompt vocabulary is *also* what it hallucinates from when it hears static.
**Recommendation:** ASR behind a trait, ship both, bake off on **your own hand-transcribed audio**. Every WER figure above comes from LibriSpeech, ATC, or Chicago PD — none of it is Indiana IMBE. Hand-transcribe 200–300 real SAFE-T transmissions early; that corpus is both your benchmark and the seed for eventual fine-tuning.
**Two things not to do:** don't run a denoiser front-end (a May 2026 study measured Parakeet at 97.0% word accuracy clean, 95.0% noisy, **87.2% best-enhanced** — enhancement cost 7.8 points versus doing nothing). And don't build one global prompt; build per-talkgroup vocabularies from that talkgroup's own transcript history.
**Platform acceleration:** macOS → Metal by default (Core ML is encoder-only, needs a per-model conversion step, and breaks on macOS betas — make it opt-in). Windows → Vulkan by default (vendor-neutral, supports integrated GPUs as of whisper.cpp 1.8.3, ~3–4× over CPU), CUDA as an optional download. CPU-only is a viable floor given your clips are seconds long and arrive at radio duty cycle.
---
## 8. Roadmap
Each phase has an exit gate. Don't advance without passing it.
### Phase 0 — Instrument (weeks 1–3)
Build `hs-bench`. Capture the IQ corpus. Run SDRTrunk, OP25, and GopherTrunk against it; record baselines.
**Gate:** a results table with real numbers for three existing decoders on your recordings.
### Phase 1 — Prove the thesis (months 1–4)
IQ file source → channelizer → C4FM and LSM demod → **sync-trained T/2 LMS FSE placed before differential detection** → frame sync → NID → FEC → TSBK parse. Offline only, no live radio, no UI.
**Gate: measurably lower BER and sync-loss than SDRTrunk nightly on at least two simulcast recordings.** If you can't clear this in four months, stop and reassess — the thesis was wrong, or the remaining headroom is smaller than it looked.
### Phase 2 — Hear it (months 4–6)
Live SDR source (Seify). Trunking state machine, control-channel following, grants, site failover. In-tree IMBE vocoder. Audio out via cpal. CLI only.
**Gate:** follow a full SAFE-T site for an hour, unattended, with clean audio and no crashes.
### Phase 3 — Make it an app (months 6–9)
Tauri shell. Scan lists, priority, lockout. WebGL waterfall and constellation. Call history with instant replay. Recording with metadata sidecars. CSV catalog import.
**Gate:** you use it as your daily driver instead of SDRTrunk for two weeks.
### Phase 4 — Catalog and transcription (months 9–12)
RadioReference SOAP client + keyring. Encryption and TDMA-CC attribute handling. Transcription pipeline with VAD gate, hallucination filter, hotword biasing. FTS5 search.
**Gate:** transcript quality good enough that search actually finds the call you remember.
### Phase 5 — Ship and port (year 2)
Windows build. Signed/notarized macOS release. Phase II TDMA + vocoder plugin boundary. Headless server + remote client. Broadcastify Calls / OpenMHz / rdio-scanner export. DFE and MLSE experiments.
**Note what's deliberately late:** DMR, NXDN, EDACS, LTR, map views, mobile. Every one is a feature other tools already have. None of them is why anyone would switch to yours.
---
## 9. Risk register
| Risk | Severity | Mitigation |
|---|---|---|
| **The equalizer doesn't beat SDRTrunk** | Fatal to the thesis | Phase 1 gate exists precisely to surface this in month 4, not month 18. Fallback positioning: "the native Mac scanner that actually works," which is real value given SDRTrunk's current Tahoe breakage. |
| **GopherTrunk already solved it** | High | Evaluate it in week 1. If it works, consider contributing to it instead — or compete on the app layer, where Go's story is weak. |
| **Solo-maintainer burnout** | High — this kills most projects like this | Scope v1 to P25 only. Resist every "could you also add DMR" request until Phase 5. The graveyard is full of scanner projects that tried to support everything. |
| RR revokes a published app key | Medium | CSV import path means the app degrades, not dies. Get RR's position in writing first. |
| 8,359,197 enforcement | Low probability, high impact | Phase I only in-tree; Phase II behind a plugin boundary until 2028-05-20. Zero enforcement history against open-source P25 in 14 years. |
| GPL contamination | Medium, and it's the one that has actually killed a project | Written provenance policy in CONTRIBUTING.md. Never port from GPL sources. This is what took down OpenEar. |
| Tauri WebGL waterfall performs badly | Medium | Prototype the spectrum view in week 2 of Phase 3, before committing the rest of the UI. |
| Transcription quality disappoints users | Medium | Set expectations in the UI. Show confidence. Ship the filter pipeline, not raw Whisper. Plan for fine-tuning. |
| SAFE-T goes encrypted | Low near-term, growing | Out of your control. Statewide/interop talkgroups are clear by IPSC policy; county LE is the erosion. |
---
## 10. Open questions
1. **Do you have an Airspy R2?** If not, buy one before Phase 0. RTL-SDR cannot cover a SAFE-T simulcast site's 4.8 MHz span in one device.
2. **Check 8,359,197's maintenance-fee status** at patentcenter.uspto.gov. If it lapsed, Phase II opens up years early.
3. **Run GopherTrunk against a Lake County recording** before writing DSP code.
4. **Email support@radioreference.com** when applying for an app key; get their position on publication in writing.
5. **Which sites can you actually receive** from your antenna location? That determines your corpus and therefore what you can prove.
6. **Are you soloing this, or recruiting?** It changes scope, and it changes whether the CI benchmark harness is nice-to-have or essential.
---
## Appendix — key sources
**Decoders:** [SDRTrunk](https://github.com/DSheirer/sdrtrunk) · [OP25 (boatbod)](https://github.com/boatbod/op25) · [DSD-FME](https://github.com/lwvmobile/dsd-fme) · [trunk-recorder](https://github.com/TrunkRecorder/trunk-recorder) · [GopherTrunk](https://github.com/MattCheramie/GopherTrunk)
**Simulcast physics:** [Tait, P25 Simulcast Coverage (PDF)](https://www.radioresource.com/downloads/tait/whitepapers/p25-simulcast-coverage-white-paper.pdf) · [EFJohnson, Simulcasting Project 25 (PDF)](https://www.efjohnson.com/resources/dyn/files/972772z218319c9/_fn/Simulcasting+Project+25.pdf) · [Wireless Pi: P25 CQPSK via C4FM receiver](https://wirelesspi.com/demodulating-p25-cqpsk-signals-using-a-c4fm-receiver/) · [GNU Radio DFE](https://wiki.gnuradio.org/index.php/Decision_Feedback_Equalizer)
**Rust SDR:** [Seify](https://github.com/FutureSDR/seify) · [FutureSDR](https://github.com/FutureSDR/FutureSDR) · [rustradio](https://github.com/ThomasHabets/rustradio) · [kchmck/p25rx](https://github.com/kchmck/p25rx) (dead since 2020, still the best Rust P25 reference)
**Legal:** [US8359197B2](https://patents.google.com/patent/US8359197B2/en) · [18 U.S.C. § 2510](https://www.law.cornell.edu/uscode/text/18/2510) · [§ 2511](https://www.law.cornell.edu/uscode/text/18/2511) · [IC 35-44.1-2-7](https://law.justia.com/codes/indiana/title-35/article-44-1/chapter-2/section-35-44-1-2-7/) · [mbelib (ISC)](https://github.com/szechyjs/mbelib) · [SDRTrunk JMBE wiki](https://github.com/dsheirer/sdrtrunk/wiki/JMBE)
**RadioReference:** [API wiki](https://wiki.radioreference.com/index.php/API) · [Web Service 3.1 reference](https://wiki.radioreference.com/index.php/RadioReference.com_Web_Service3.1) · [Database Web Service API policy](https://support.radioreference.com/hc/en-us/articles/18844460198932-Database-Web-Service-API) · [Hoosier SAFE-T (SID 8084)](https://www.radioreference.com/db/sid/8084) · [SAFE-T wiki](https://wiki.radioreference.com/index.php/Indiana_Project_Hoosier_SAFE-T) · [SAFE-T 2026 thread](https://forums.radioreference.com/threads/hoosier-safe-t-thread-2026.495989/)
**Transcription:** [Police radio ASR / BPC-CPD (arXiv 2409.10858)](https://arxiv.org/html/2409.10858v1) · [Whisper hallucination investigation (arXiv 2501.11378)](https://arxiv.org/html/2501.11378v1) · [Careless Whisper (FAccT '24)](https://arxiv.org/html/2402.08021v2) · [whisper.cpp](https://github.com/ggml-org/whisper.cpp) · [whisper-rs (Codeberg)](https://codeberg.org/tazz4843/whisper-rs) · [sherpa-onnx hotwords](https://k2-fsa.github.io/sherpa/onnx/hotwords/index.html) · [Parakeet TDT 0.6B v3](https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3) · [FluidAudio](https://github.com/FluidInference/FluidAudio)
