#!/bin/bash
# HoosierSDR one-command macOS installer.
#
#   curl -fsSL https://raw.githubusercontent.com/duderayuh/HoosierSDR/main/tools/install-mac.sh | bash
#
# Installs every dependency (Xcode CLT, Homebrew, Rust, the SDR capture libs
# — airspy, libusb, soapysdr, soapyrtlsdr, librtlsdr — plus tauri-cli), clones
# the repo to ~/HoosierSDR (or repairs an existing checkout), builds the release
# CLI, and verifies it with a no-hardware demo decode.
#
# Idempotent AND self-healing: re-running only does what is missing, and it
# repairs a stale or broken checkout instead of leaving you with a half-broken
# install. Works on Apple Silicon and Intel.
set -euo pipefail

REPO_URL="https://github.com/duderayuh/HoosierSDR.git"
DEST="${HOOSIER_DIR:-$HOME/HoosierSDR}"

say()  { printf '\n\033[1;36m▶ %s\033[0m\n' "$1"; }
ok()   { printf '\033[1;32m  ✓ %s\033[0m\n' "$1"; }
warn() { printf '\033[1;33m  ! %s\033[0m\n' "$1"; }
die()  { printf '\n\033[1;31m✗ %s\033[0m\n' "$1" >&2; exit 1; }

[ "$(uname -s)" = "Darwin" ] || die "This installer is for macOS. See the README for Linux/Windows."

# ── 1. Xcode command-line tools (C compiler for the vocoder, plus git) ──
say "Xcode command-line tools"
if xcode-select -p >/dev/null 2>&1; then
  ok "already installed"
else
  warn "opening the installer dialog — click Install, let it finish, then re-run this script"
  xcode-select --install || true
  die "waiting on Xcode command-line tools; re-run once they finish installing"
fi

# ── 2. Homebrew ──
say "Homebrew"
if ! command -v brew >/dev/null 2>&1; then
  # Load an existing install that just isn't on PATH yet.
  for p in /opt/homebrew/bin/brew /usr/local/bin/brew; do
    [ -x "$p" ] && eval "$("$p" shellenv)" && break
  done
fi
if command -v brew >/dev/null 2>&1; then
  ok "already installed"
else
  warn "installing Homebrew (may prompt for your password)"
  NONINTERACTIVE=1 /bin/bash -c \
    "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
  for p in /opt/homebrew/bin/brew /usr/local/bin/brew; do
    [ -x "$p" ] && eval "$("$p" shellenv)" && break
  done
  command -v brew >/dev/null 2>&1 || die "Homebrew installed but not on PATH; open a new terminal and re-run"
fi

# ── 3. Rust ──
say "Rust toolchain"
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"
if command -v cargo >/dev/null 2>&1; then
  ok "already installed ($(rustc --version))"
else
  warn "installing Rust via rustup"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  source "$HOME/.cargo/env"
  command -v cargo >/dev/null 2>&1 || die "Rust installed but cargo not on PATH; open a new terminal and re-run"
fi

# ── 4. Clone or repair the repo (done EARLY so the source is always present) ──
say "HoosierSDR source → $DEST"
if [ -d "$DEST/.git" ]; then
  # Existing checkout: fast-forward to origin/main, and self-heal a stale,
  # diverged, or detached checkout — but never clobber uncommitted work.
  if git -C "$DEST" fetch --all --prune 2>/dev/null; then
    git -C "$DEST" checkout main 2>/dev/null || git -C "$DEST" checkout -B main origin/main 2>/dev/null
    if git -C "$DEST" diff --quiet 2>/dev/null && git -C "$DEST" diff --cached --quiet 2>/dev/null; then
      git -C "$DEST" reset --hard origin/main
      ok "updated to $(git -C "$DEST" rev-parse --short HEAD 2>/dev/null || echo main)"
    else
      warn "uncommitted changes in $DEST — leaving them intact; run 'git -C $DEST status' if the app seems outdated"
    fi
  else
    warn "existing checkout is broken — re-cloning"
    rm -rf "$DEST"
    git clone "$REPO_URL" "$DEST"
  fi
else
  git clone "$REPO_URL" "$DEST"
  ok "cloned"
fi

# ── 4b. The desktop app must be present in the checkout ──
[ -f "$DEST/app/tauri.conf.json" ] \
  || die "checkout is missing app/tauri.conf.json — the clone is stale or incomplete; delete $DEST and re-run"

# ── 5. SDR capture tools ──
# The desktop app enables the rtlsdr + airspy + soapy backends, so it links
# libairspy, libSoapySDR, librtlsdr and libusb. Install them all up front so
# `cargo tauri dev` works on first try, not just the CLI.
say "SDR capture tools (airspy, libusb, soapysdr, soapyrtlsdr, librtlsdr)"
for pkg in airspy libusb soapysdr soapyrtlsdr librtlsdr pkg-config; do
  if brew list --formula "$pkg" >/dev/null 2>&1; then
    ok "$pkg already installed"
  else
    brew install "$pkg"
  fi
done

# ── 6. Tauri CLI (prebuilt binary via cargo-binstall, source as fallback) ──
# `cargo install tauri-cli` compiles ~500 crates from source, which can OOM or
# stall on 8 GB machines (e.g. MacBook Neo). Prefer the prebuilt binary.
say "Tauri CLI"
if cargo tauri --version >/dev/null 2>&1; then
  ok "tauri-cli already installed ($(cargo tauri --version 2>/dev/null))"
else
  warn "installing tauri-cli"
  if command -v cargo-binstall >/dev/null 2>&1 || brew install cargo-binstall >/dev/null 2>&1; then
    if cargo binstall tauri-cli --no-confirm; then
      ok "tauri-cli installed from prebuilt binary"
    else
      warn "prebuilt install failed — compiling from source (slower)"
      cargo install tauri-cli --version '^2.0'
    fi
  else
    warn "cargo-binstall unavailable — compiling from source (slower)"
    cargo install tauri-cli --version '^2.0'
  fi
fi
cargo tauri --version >/dev/null 2>&1 || die "tauri-cli still missing after install attempt"

# ── 7. App transcription (optional) ──
say "App transcription (faster-whisper, optional)"
if python3 -c "import faster_whisper" >/dev/null 2>&1; then
  ok "faster-whisper already available"
elif command -v python3 >/dev/null 2>&1; then
  warn "attempting \`python3 -m pip install --user faster-whisper\`"
  python3 -m pip install --user faster-whisper \
    || warn "could not install faster-whisper automatically — see app/README.md"
else
  warn "no python3 found; transcription disabled (the app still runs)"
fi

# ── 8. Build the release CLI ──
say "Building the release CLI (first build takes a few minutes)"
# libairspy is installed above, so build live Airspy capture in.
( cd "$DEST" && cargo build --release -p hs-cli --features airspy )
BIN="$DEST/target/release/hoosier-sdr"
[ -x "$BIN" ] || die "build finished but $BIN is missing"
ok "built $BIN"

# ── 9. Verify with a no-hardware demo decode ──
# Capture the whole output before grepping: piping straight into `grep -q`
# would SIGPIPE the decoder, which `set -o pipefail` reads as a failure.
say "Verifying with a synthesized decode (no radio needed)"
demo_out="$( cd "$DEST" && "$BIN" --demo 2>/dev/null || true )"
if printf '%s' "$demo_out" | grep -q "voice grants"; then
  ok "decode pipeline works end to end"
else
  die "the demo decode did not produce output — something is wrong with the build"
fi

# ── 10. Offer to put the binary on PATH ──
LINE='export PATH="'"$DEST"'/target/release:$PATH"'
if ! grep -qsF "$LINE" "$HOME/.zshrc" 2>/dev/null; then
  printf '\n# HoosierSDR\n%s\n' "$LINE" >> "$HOME/.zshrc"
  ok "added hoosier-sdr to your PATH (open a new terminal to pick it up)"
fi

# ── 11. Final verification ──
say "Verifying the desktop app is ready"
[ -f "$DEST/app/tauri.conf.json" ] || die "app/tauri.conf.json missing after install"
ok "tauri-cli $(cargo tauri --version 2>/dev/null)"
ok "app config present at $DEST/app/tauri.conf.json"

cat <<EOF

  HoosierSDR is installed at $DEST

  Try it:
    hoosier-sdr --demo                              # in a NEW terminal
    airspy_info                                     # check an Airspy R2 is seen
    SDR=airspy $DEST/tools/field-probe.sh probe     # capture + scan + decode
    hoosier-sdr --sdr --source airspy --rate 10000000 --freq 855M \\
                --follow --control 851.5375M         # follow a whole site live

  Desktop app (deps already installed above):
    cd $DEST/app && cargo tauri dev                 # launch the GUI

  App transcription (optional — see app/README.md):
    # needs faster-whisper importable from a Python at /usr/local/bin/python3
    # or /opt/homebrew/bin/python3. Run:
    #   python3 -m pip install --user faster-whisper
    # (or set TRANSCRIBE_PYTHON to a specific interpreter)

EOF
