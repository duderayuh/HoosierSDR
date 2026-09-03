# Equalizer + timing-recovery review (kimi-k3)

Provenance: OpenRouter `moonshotai/kimi-k3`, 2026-08-23. Prompt: review of
`hs-dsp` equalizer + timing/carrier recovery, following the silence-gate
dead-end investigation (gating equalizer adaptation on `acquired` deadlocks
the blind carrier acquisition on multipath). Findings below are kimi's,
lightly reformatted.

Landed so far: H1 (NaN front-door guard + watchdog predicate inversion), M4
(failed-acquisition-window tap reset), L6 (leaky-bucket `bad_run` — was a
hard reset to 0 on any good symbol, so a lock chattering across
`LOCK_ERR_MAX` could never accumulate 1000 *consecutive* bad symbols and
never trip the watchdog at all) and L11 (`reacquire()` now resets the
Gardner timing loop's integrator). Also added, beyond this review's scope:
an age-gated soft recovery on the first watchdog trip — an acquisition that
has held cleanly for ~1s gets one non-destructive recovery (reset only the
trip bookkeeping, keep the equalizer taps and carrier-bias estimate) before
the full `reacquire()`, addressing "throws away a working lock at moderate
SNR" without regressing a genuinely wrong acquisition (which still trips
within one `BAD_RUN_LIMIT` window, long before it looks established). See
`crates/hs-dsp/src/cqpsk.rs`'s `update_bad_run`/`watchdog_action` and their
unit tests. 2026-09-03 addendum below records a rejected attempt at a
related idea (throttling equalizer adaptation itself, not just watchdog
recovery, by lock quality). Not yet applied: M2/M3 (Gardner loop hardening +
TED normalization), M5 (fractional-spaced equalizer / FSE wiring), L7-L10,
L12, C13.

# Code Review: CQPSK pre-detection equalizer receiver

## Task 1 — Gating adaptation on idle noise without deadlocking acquisition

Your experiment confirmed a real structural constraint: **any presence gate measured downstream of the equalizer deadlocks**, because every such metric is conditioned on the equalizer having already converged:

- Blind coherence `|Σ exp(j(4Δφ−π))|/N` — deadlocks, as you measured (0.07 < 0.30 on the 0.6-echo channel).
- Decision-directed `err_ewma` — meaningless until acquisition, which needs the eq.
- The eq's own `error_var` (EWMA of g²) — high both on noise *and* on an unconverged eye, so gating on it is gating on convergence itself.

The only gates that cannot deadlock are **eq-independent** ones, measured upstream:

1. **AGC gain rail.** When the channel is idle, the AGC amplifies the noise floor to target and its gain sits at or near its maximum; a keyed-up carrier forces the gain to back off. `agc.gain() < 0.9 * gain_max` is a free, eq-independent presence flag available *during* acquisition. This requires the AGC to expose its gain and to have a max-gain clamp (neither visible in the source provided — verify).
2. **Pre-AGC input power with noise-floor tracking** (classic squelch: slow EWMA, track its minimum as the floor, hysteresis above it). Tuner-dependent threshold; more plumbing.
3. **Spectral concentration**: ratio of post-MF to pre-MF power. AGC-normalized noise is white; a real P25 signal concentrates in the RRC passband. Tuner-independent, but costs a second filter's worth of state.

That said, **the current design is fundamentally correct, and I'd argue it plus one addition beats all three gates.** Adapting on noise is *bounded and reversible*: NLMS normalization caps the step, the finite-tap guard caps catastrophe, and `reacquire()`'s tap reset discards the walk. The acquisition window slides until signal appears, and because adaptation never stops, the eq re-converges on the real signal within a window or two of key-up. The actual hole is elsewhere:

**`reacquire()` is only reachable from the tracking branch.** A receiver cold-started on an idle channel never acquires, so the tap reset in `reacquire()` never runs — the eq random-walks on noise indefinitely. It self-heals on key-up for the linear CMA, but for the DFE, AGC-normalized unit-power noise is exactly the condition your own `dfe.rs` docstring warns opens the degenerate CM-DFE minimum (feedback synthesizes modulus from past decisions). The deadlock-free fix that completes commit e193ebc: **reset the taps after K consecutive failed acquisition windows**, in the existing `else` branch of the coherence check in `push_phase`:

```rust
} else {
    self.acq = C32::ZERO;
    self.acq_n = 0;
    self.acq_failures += 1;
    if self.acq_failures >= 4 {   // ~1600 syms: taps are prima facie not opening an eye
        self.eq.reset();
        self.acq_failures = 0;
    }
}
```

Identity is the best possible cold start, so this can only help acquisition — no gate, no threshold, no tuner dependence. Do this and skip the AGC-gain gate unless field captures show the walk still biting.

---

## Sign/conjugation audit (requested explicitly)

I derived each update from J = E[(|y|²−R₂)²] with y = wᴴx. Wirtinger: ∂J/∂w*_k = 2(|y|²−R₂)·x_k·y*, so descent is **w += μ(R₂−|y|²)·x·y***.

- `CmaEqualizer::push`: `taps[k] + xk * ey.conj()` with `ey = y.scale(g*mu/energy)` → μg·x·y* ✓ correct.
- `CmaDfe::push` feedback: the fed-back regressor is −d (since `y = ff_out − fb_out`), so descent is fb −= μg·d·y*. The code's `self.fb[j] = self.fb[j] - dj * ey_fb;` **looks like a sign error and is not** — the comment's reasoning is right. ✓
- `LmsFse::train`: `taps[k] + (x * e.conj()).scale(mu)` ✓ textbook.
- `ComplexGardner` TED sign: late midpoint on a rising transition → y_mid > 0, (y_k − y_{k−1}) > 0 → e > 0 → `w` increases → next strobe sooner ✓ negative feedback. Interpolation convention verified: post-decrement `nco` is the overshoot, `mu = nco/w` samples back from `x`, `y = prev + (x−prev)(1−mu)` ✓. `timing.rs` uses the same convention (`frac = -count`) ✓.
- `CostasLoop` PD: err ≈ angular distance to nearest quadrant, `phase += alpha*err` with y = x·e^(−jφ) ✓.
- `rotate_dibit` tables are consistent with `dphase_to_dibit`'s quadrant assignment ✓. The blind estimator's aliasing (`wrap_pi(acq.arg())/4` ∈ (−π/4, π/4]) recovers the bias mod π/2 — which is precisely `rotate_dibit`'s contract, so pull-in range is effectively unlimited *modulo the quarter-turn* (the real limit is the MF passband, not the estimator). That property deserves a sentence in the docstring; as written, the "1 kHz → 1.3 rad" comment reads like a limitation when the structure actually absorbs it.

No sign or conjugation errors found anywhere.

---

## Findings, ranked

### HIGH — correctness / silent death

**H1. One NaN permanently kills the receiver; the watchdog is not NaN-safe.** `cqpsk.rs`, `push_phase`:

```rust
self.freq_bias += self.mu_freq * err;
...
self.err_ewma += 0.002 * (err.abs() - self.err_ewma);
if self.err_ewma > LOCK_ERR_MAX {
```

If any upstream stage emits a single NaN/inf sample (front-end overflow, USB drop, AGC divide), the Gardner's finite-guard resets `w` but still *outputs* the NaN interpolation that symbol; the eq's tap guard resets the taps but the NaN `y` has already been returned. Then `raw` → `err` → `err_ewma` and `freq_bias` are NaN, `NaN > LOCK_ERR_MAX` is **false**, `bad_run` resets to 0 every symbol, and every subsequent `corr` is NaN. The receiver decodes garbage forever with no recovery path. Your own `cma.rs` comment describes exactly this trap ("it never re-acquires, because NaN > threshold is false") — but the fix was applied to the equalizer taps, not to the carrier/lock state that actually owns re-acquisition. Fix both ends:

```rust
let raw = differential_detect(sym, prev);
if !raw.is_finite() { self.reacquire(); return None; }
```

and invert the watchdog predicate to `if !(self.err_ewma <= LOCK_ERR_MAX)` so NaN counts as bad.

### MEDIUM

**M2. `GardnerSync` (timing.rs) is a first-order loop with no clamping — the complex loop's hardening was never ported.** `self.count += self.sps / 2.0 - self.gain * e;` — (a) no integrator, so any tuner clock ppm locks with a steady-state timing offset ∝ offset/gain, and an offset exceeding `gain·max|e|` per half-symbol slips symbols outright; (b) a transient `e` can drive `count ≤ 0` → spurious double strobe, or NaN → `count <= 0.0` false forever → same silent-death class as H1; (c) `(1.0 - frac).clamp(0.0, 1.0)` masks frac > 1 instead of handling it; (d) `self.mu = frac; let _ = self.mu;` is a dead store. Fix: port the ComplexGardner PI + rate-clamp + finite-guard structure, or delete this loop outright.

**M3. Gardner TED gain is amplitude²-dependent in both loops.** `let e = ((y - self.y_prev_sym) * self.y_mid.conj()).re;` — e ∝ A², but kp/ki are computed from `loop_bw` assuming unit detector gain. Your own `dfe.rs` docstring documents post-MF levels of ~0.25 RMS (Airspy) vs ~1.0 (RTL) — a **16× loop-bandwidth swing** across tuners, from sluggish to bang-bang against the `w` clamp. Normalize: `e / (y_mid.norm_sq() + eps)`, or AGC after the matched filter.

**M4. Cold-start idle walk is never reset** — see Task 1. The `else` branch of the coherence check is the right place; this also closes the DFE degenerate-minimum exposure on AGC-normalized noise.

**M5. The wired equalizers are only tested against integer-symbol echoes; the field problem is fractional.** `cma.rs` test: `let rx = s + echo * prev;` (one-symbol echo). `dfe.rs` test: same. Only the *unwired* `lms.rs` test uses a T/2-spaced echo — while `lms.rs`'s own doc cites simulcast delay spreads of 0.12–0.34 T. A 9-tap **symbol-spaced** CMA placed after timing recovery is sampling-phase-sensitive and cannot synthesize fractional-delay inverse responses without aliased-null risk. This is the largest gap between the thesis claim and the implementation: the FSE exists but isn't wired. Recommend a T/2-spaced CMA ahead of the strobe decision (the Gardner already computes midpoints — the infrastructure is there) and fractional-delay test cases for the wired paths.

### LOW

**L6. `bad_run` hard-resets on one good symbol.** `} else { self.bad_run = 0; }` — a marginally-locked signal whose `err_ewma` chatters across 0.33 never accumulates 1000 *consecutive* bad symbols and never re-acquires. Use a leaky bucket (`saturating_sub(2)` on good) instead of zeroing.

**L7. Eq unfreeze at `settle > 32` ≪ Gardner settling time.** At `loop_bw = 0.004`, ζ = 0.707, the timing loop's time constant is ~1/(ζ·bw) ≈ 350 symbols; the comment claims the freeze lets timing settle "so it adapts on a real eye," but 32 symbols is ~10% of settling. CMA empirically converges through the transient (tests pass), but either say that in the comment or gate on a timing-lock indicator. Also make 32 a named constant next to `SETTLE_SYMS`.

**L8. `LmsFse`/`EqualizedCqpsk` are dead code with a broken health metric.** `error_var` is only updated inside `train()`; `EqualizedCqpsk::push` never trains, so the documented "retrain when error variance rises" can never fire — a frozen equalizer's `error_var` is frozen too. The struct doc ("wiring it behind full Gardner/Costas recovery … is the remaining Phase 1 integration") is stale: `CqpskReceiver` *is* the integration, and Costas is explicitly rejected. No NLMS or finite guard in `train()`. Wire it (as the M5 FSE) or delete it.

**L9. `CostasLoop` is dead code documenting the rejected architecture.** Its docstring says it de-rotates "so the equalizer downstream sees a de-rotated constellation" — the exact pipeline `cqpsk.rs`'s docstring explains is impossible (QPSK DD detector vs the 8-point π/4-DQPSK union). If ever instantiated on CQPSK it would fight the alternating π/4 offset every other symbol. Delete it. (Trivial: its phase wrap uses `< -PI` vs `wrap_pi`'s `<= -PI`.)

**L10. Acquisition latency stack-up eats the head of every traffic call.** The comment says "slide the window forward" but the code resets to zero — a block window, worst-case onset→acquisition ≈ 2 windows (~800 syms ≈ 167 ms), *plus* DFE convergence at production `mu_ff = 0.001` (thousands of symbols, i.e. several failed windows first). Two cheap wins: continuous EWMA coherence on exp(j(4Δφ−π)) thresholded every symbol instead of block windows, and gear-shifted mu (fast until first acquisition, then drop to production).

**L11. `reacquire()` doesn't reset the timing loop.** After 1000+ symbols of noise TED output, `gardner.integ` can be railed at ±0.5·w0; the `w` clamp bounds the damage to ±20% rate error, but discharge takes time while the new acquisition window runs. Reset the integrator (or the whole `ComplexGardner`) in `reacquire()`.

**L12. `CmaDfe::in_pwr` cold start.** `let a = (1.0 / self.seen as f32).max(0.01);` — the first sample sets `in_pwr = |x₁|²` outright; a key-up transient as sample one skews normalization for ~100 symbols. Floor `a` during warm-up or seed from the AGC target.

### COSMETIC

**C13.** `CmaEqualizer::new`/`CmaDfe::new` assert `>= 3` but not oddness — an even tap count puts the centre spike off by half a symbol (`RealLmsEq` does assert odd). Duplicated wrap logic in `dphase_to_dibit` vs `wrap_pi`. "~90 ms" comment vs 400/4800 = 83 ms.

---

## Summary

The central design — phase-blind CMA/DFE before differential detection, adapt-through-acquisition, reset-on-reacquire — is sound, and the update math is correct in every block (including the DFE feedback sign, which is easy to misread). The two things that will actually bite you in the field are **H1** (a single NaN is a permanent, silent kill — fix the watchdog predicate today) and **M5** (the wired equalizer has never been tested against the fractional-delay channel it exists to solve). Then close the cold-start idle walk with the failed-window tap reset (M4), which is the right answer to Task 1 — not a presence gate.

---

## Addendum, 2026-09-03: a rejected attempt at scaling adaptation by lock quality

A separate concern surfaced independently ("the equalizer adaptation is
wrong for a noise-limited channel") reads, on its face, like this review's
own Task 1 all over again: NLMS normalizes the step by *input* energy, which
the AGC pins to roughly unit power regardless of SNR, so a noise-limited
channel adapts at essentially the same effective step as a clean one. The
natural next idea — scale the *tracking* step down as the receiver's own
`err_ewma` rises, so it chases noise less as SNR drops — was implemented
(post-acquisition only, gated on lock age so it wouldn't touch the
pre-acquisition path Task 1 already settled) and rejected after a
discriminating experiment:

1. **First attempt (`err_ewma`-coupled step scale)** regressed
   `a_grant_outside_the_primary_band_decodes_on_an_extra_radio` (0 CQPSK
   syncs). Instrumented `err_ewma` per symbol: it climbed monotonically from
   acquisition (0.02 → 0.36 over ~2,400 symbols) instead of settling — a
   feedback loop, not noise. Coupling step size to the error the step is
   supposed to correct is a control-theory footgun: once error starts
   rising, the mechanism weakens exactly when it needs to work harder.
2. **Discriminating experiment**, per the reviewer's own advice: replace the
   error-coupled scale with a **fixed** low scale (0.15× nominal, no
   coupling at all) after a settle window, and trace `echo_frac` and
   `freq_bias` alongside `err_ewma`. Still failed, and the trace explains
   why: `echo_frac` opened at **0.67** (not near-zero — this "clean"
   synthetic channel is not actually flat) and `freq_bias` **drifted**
   continuously (0.52 → 0.78 over the run) rather than converging to a
   constant. The CMA was doing continuous, load-bearing compensation work —
   likely a standing timing/frequency residue this review's own **M3**
   already names (Gardner TED gain is amplitude²-dependent and un-normalized,
   so loop bandwidth swings with tuner/AGC level) — not idly re-adapting to
   noise. Throttling it, error-coupled or fixed, prevented it from keeping up
   and drove `err_ewma` up regardless of the scale law.

**Conclusion: rejected, not just mistuned.** If the equalizer is frequently
doing real, necessary tracking work even on synthetic "clean" test signals,
any scheme that slows it down under a lock-quality heuristic is unsafe in
general, not just under a bad coupling law. The kimi review's own Task 1
conclusion already covers the noise-limited case correctly: adaptation
during noise is *bounded and reversible* (NLMS caps the step, the finite-tap
guard in `cma.rs`/`dfe.rs` catches divergence, M4's failed-window reset
discards a walk on true idle noise) rather than something that needs slowing
down — and this review's own leaky-bucket fix above (L6) means a lock that's
*actually* bad now reliably reaches the `reacquire()` that resets the taps,
closing the loop. The real fix for the noise-limited/fractional-channel
regime this uncovered evidence for is still **M5** (the T/2 fractionally-
spaced equalizer) — large enough to be its own piece of work, not a
step-scaling heuristic bolted onto the symbol-spaced CMA.