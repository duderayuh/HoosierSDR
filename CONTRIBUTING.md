# Contributing to HoosierSDR

## Code provenance policy (non-negotiable)

This project is **Apache-2.0**. GPL contamination has killed projects in this ecosystem before (OpenEar, 2020). These rules protect the project's existence:

1. **Never port, translate, or transliterate code from GPL sources.** That includes SDRTrunk, JMBE, OP25, trunk-recorder, dsd-neo, and mbelib-neo. Translating Java/C/Python to Rust creates a derivative work and carries the source license. Copied constant tables and structure-preserving rewrites count too.
2. **Reading GPL code to learn *that* a technique works is fine.** Opening it in one window while typing Rust in the other is not. Implement from the literature (Haykin, Proakis), from the TIA-102 specs, and from your own measurements.
3. **Permissively licensed sources you may port from, with attribution:** mbelib (ISC — `szechyjs/mbelib`, `lwvmobile/mbelib`), DSD-FME's own ISC code, GopherTrunk (Apache-2.0).
4. **Protocol facts are free.** Frame layouts, bit orderings, FEC parameters, deinterleave patterns, slot timing — facts aren't copyrightable. The code expressing them is.
5. **State provenance in your PR.** Nontrivial DSP or protocol code must say where it came from: a spec section, a paper, a permissively-licensed project (named), or original work.

## Other hard rules

- **No decryption code.** PRs adding P25 decryption of any kind (ADP/DES/AES) will be closed without discussion.
- **No RadioReference data in the repo.** Test fixtures must be synthetic. Committing real talkgroup dumps violates RR's terms.
- **Phase II vocoder from ISC mbelib.** The AMBE+2 half-rate decoder is available in the ISC-licensed `mbelib` (`ambe3600x2450.c` → `mbe_processAmbe3600x2450Frame` plus ECC helpers); HoosierSDR vendors only the IMBE subset today, so vendoring that `.c` file (same ISC licence as the IMBE already vendored) is the path — the same code SDRTrunk, OP25 and GopherSDR ship Phase II on. Do not port any GPL vocoder (OP25, mbelib-neo, JMBE). The `hs-vocoder` plugin boundary is an optional escape hatch, not a licence requirement.
- **Benchmarks over vibes.** DSP changes should come with `hs-bench` numbers against the IQ corpus. "Sounds better" is not a metric.

## Scope

v1 is **P25 Phase I only**. DMR/NXDN/EDACS/LTR/mobile requests are deferred to Phase 5 by design — see the roadmap in `docs/ARCHITECTURE.md` §8.
