#!/bin/zsh
# Field probe for the degraded-simulcast capture hunt (MESA site 010).
#
#   ./tools/field-probe.sh probe            # 10 s capture -> is THIS spot degraded?
#   ./tools/field-probe.sh record <name>    # three 60 s captures worth keeping
#
# Center 857.2 MHz covers all four site-010 control channels in one capture:
# 856.1625, 856.5125, 857.6625, 858.1875.
#
# Radio: set SDR=airspy for an Airspy R2 (captures 2.5 MSPS → the decoder
# normalizes it to 2.4 automatically), or leave default for an RTL-SDR at
# 2.4 MSPS. Airspy's 12-bit ADC is the reason to prefer it here: the weak
# tower in a simulcast pair stays cleanly represented next to the strong one.
set -e
CENTER=857200000
SDR=${SDR:-rtl}
if [[ "$SDR" == airspy ]]; then
  RATE=2500000            # R2 native; normalized to 2.4 MSPS in the decoder
  GAIN=${GAIN:-14}        # airspy_rx -g is a 0–21 linearity index, not dB
else
  RATE=2400000            # RTL-SDR
  GAIN=${GAIN:-40}        # rtl_sdr -g is dB (0–49); drop to 30 if it overloads
fi
HS="$(dirname "$0")/../target/release/hoosier-sdr"
OUT=~/hoosier-field
mkdir -p "$OUT"

# Capture $1 seconds of IQ to file $2 (.cu8 for RTL, .cf32 for Airspy).
capture() {
  local secs=$1 f=$2
  if [[ "$SDR" == airspy ]]; then
    # airspy_rx writes float32 IQ (-t 0), which hoosier-sdr reads as .cf32.
    # Its -f is in MHz (rtl_sdr's is in Hz), and -a selects the sample rate.
    local mhz
    mhz=$(echo "scale=4; $CENTER/1000000" | bc)
    airspy_rx -f "$mhz" -a $RATE -t 0 -g $GAIN \
      -n $((RATE*secs)) -r "$f" >/dev/null 2>&1
  else
    rtl_sdr -f $CENTER -s $RATE -g $GAIN -n $((RATE*secs)) "$f" 2>/dev/null
  fi
}
EXT=$([[ "$SDR" == airspy ]] && echo cf32 || echo cu8)

probe() {
  local f="$OUT/probe_$(date +%H%M%S).$EXT"
  echo "── capturing 10 s at $((CENTER/1000000)).2 MHz, $SDR, gain $GAIN…"
  capture 10 "$f"
  echo "── scanning…"
  local scan
  scan=$("$HS" --rate $RATE --freq $CENTER --scan "$f" 2>/dev/null)
  echo "$scan"
  local ctrl
  ctrl=$(echo "$scan" | awk '/CONTROL/ {print $2; exit}')
  if [[ -z "$ctrl" ]]; then
    echo "!! no control channel found — antenna, gain (try GAIN=30), or too deep; move toward a tower"
    return
  fi
  local off
  off=$(printf '%.0f' $(echo "($ctrl - $CENTER/1000000) * 1000000" | bc -l))
  echo "── control at $ctrl MHz (offset $off Hz) — equalizer A/B:"
  for eq in "" "--no-equalizer"; do
    "$HS" --rate $RATE --offset $off --cqpsk $eq --no-wav "$f" 2>/dev/null \
      | grep -E "frame syncs|TSBKs decoded" \
      | sed "s/^/   ${eq:-equalized   }: /"
  done
  echo ""
  echo "READ IT: home baseline is ~35 TSBKs/s (350 per 10 s) and scan err ~0.4."
  echo "  err still < 0.5, TSBKs near max  -> too clean, drive on"
  echo "  err > 1, syncs land, TSBKs down, eq != no-eq -> RECORD HERE: $0 record <spotname>"
  echo "  no syncs at all                  -> too deep, back toward a tower"
}

record() {
  local name=${1:?usage: field-probe.sh record <spotname>}
  for i in 1 2 3; do
    local f="$OUT/${name}_$i.$EXT"
    echo "── recording 60 s -> $f  ($i/3)…"
    capture 60 "$f"
  done
  echo "center=$CENTER rate=$RATE gain=$GAIN date=$(date)" > "$OUT/${name}.meta"
  echo "done — note the location in $OUT/${name}.meta"
}

case "${1:-probe}" in
  probe)  probe ;;
  record) shift; record "$@" ;;
  *) echo "usage: $0 [probe | record <spotname>]"; exit 2 ;;
esac
