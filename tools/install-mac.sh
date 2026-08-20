#!/bin/bash
# HoosierSDR one-command macOS installer.
#
#   curl -fsSL https://raw.githubusercontent.com/duderayuh/HoosierSDR/main/tools/install-mac.sh | bash
#
# Installs every dependency (Xcode CLT, Homebrew, Rust, airspy + libusb),
# clones the repo to ~/HoosierSDR (or updates it if already there), builds the
# release CLI, and verifies it with a no-hardware demo decode. Idempotent —
# re-running only does what is missing. Works on Apple Silicon and Intel.
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

# ── 4. SDR capture tools ──
say "SDR capture tools (airspy, libusb)"
for pkg in airspy libusb; do
  if brew list --formula "$pkg" >/dev/null 2>&1; then
    ok "$pkg already installed"
  else
    brew install "$pkg"
  fi
done

# ── 5. Clone or update the repo ──
say "HoosierSDR source → $DEST"
if [ -d "$DEST/.git" ]; then
  git -C "$DEST" pull --ff-only && ok "updated"
else
  git clone "$REPO_URL" "$DEST"
fi

# ── 6. Build the release CLI ──
say "Building the release CLI (first build takes a few minutes)"
( cd "$DEST" && cargo build --release -p hs-cli )
BIN="$DEST/target/release/hoosier-sdr"
[ -x "$BIN" ] || die "build finished but $BIN is missing"
ok "built $BIN"

# ── 7. Verify with a no-hardware demo decode ──
# Capture the whole output before grepping: piping straight into `grep -q`
# would SIGPIPE the decoder, which `set -o pipefail` reads as a failure.
say "Verifying with a synthesized decode (no radio needed)"
demo_out="$( cd "$DEST" && "$BIN" --demo 2>/dev/null || true )"
if printf '%s' "$demo_out" | grep -q "voice grants"; then
  ok "decode pipeline works end to end"
else
  die "the demo decode did not produce output — something is wrong with the build"
fi

# ── 8. Offer to put the binary on PATH ──
say "Done."
LINE='export PATH="'"$DEST"'/target/release:$PATH"'
if ! grep -qsF "$LINE" "$HOME/.zshrc" 2>/dev/null; then
  printf '\n# HoosierSDR\n%s\n' "$LINE" >> "$HOME/.zshrc"
  ok "added hoosier-sdr to your PATH (open a new terminal to pick it up)"
fi

cat <<EOF

  HoosierSDR is installed at $DEST

  Try it:
    hoosier-sdr --demo                              # in a NEW terminal
    airspy_info                                     # check an Airspy R2 is seen
    SDR=airspy $DEST/tools/field-probe.sh probe     # capture + scan + decode

  Optional extras:
    cd $DEST && cargo build --release -p hs-cli --features rtlsdr   # live RTL-SDR
    cargo install tauri-cli --version '^2.0' && cd $DEST/app && cargo tauri dev  # GUI

EOF
