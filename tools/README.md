# tools/

## scope.html — decode-session dashboard

A self-contained, dependency-free HTML dashboard for a HoosierSDR decode
session. Open it in any browser — no server, no build.

- Ships with a demo decode embedded, so it shows something immediately.
- Click **Load a real capture (run.json)** to view your own session, exported
  with `hoosier-sdr capture.cf32 --log run.json`.

Shows the soft-symbol **eye display** (with a density histogram), frame-sync
quality over time, symbol-level distribution, resolved voice grants with
clear/encrypted badges, NID/BCH decode stats, and the encryption-gate status.
Everything renders from the diagnostics JSON schema documented in
`docs/DIAGNOSTICS.md`.
