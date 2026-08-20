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
#
# Airspy firmware note (R2 NOS rc10, 2016): the gain-setting flags (-g/-l/-m/-v)
# hang this firmware's USB streaming, and a hung capture wedges the board until
# it is unplugged. So this script captures at the DEFAULT gains (VGA 5 / mixer 5
# / LNA 1) — conservative, and right for the strong signals near a tower. If you
# genuinely need more gain, update the R2 firmware first. Select one of several
# boards with AIRSPY_SERIAL=0x…  (see `airspy_info`).
set -e
CENTER=857200000
SDR=${SDR:-rtl}
if [[ "$SDR" == airspy ]]; then
  RATE=2500000            # R2 native; normalized to 2.4 MSPS in the decoder
else
  RATE=2400000            # RTL-SDR
  GAIN=${GAIN:-40}        # rtl_sdr -g is dB (0–49); drop to 30 if it overloads
fi
HS="$(dirname "$0")/../target/release/hoosier-sdr"
OUT=~/hoosier-field
mkdir -p "$OUT"

# Capture $1 seconds of IQ to file $2 (.cu8 for RTL, .cs16 for Airspy).
capture() {
  local secs=$1 f=$2
  if [[ "$SDR" == airspy ]]; then
    # airspy_rx -t 2 = INT16_IQ (the format its firmware streams reliably),
    # read by hoosier-sdr as .cs16. -f is MHz (rtl_sdr's is Hz), -a the rate.
    # No gain flag — see the firmware note above.
    local mhz sel=()
    mhz=$(echo "scale=4; $CENTER/1000000" | bc)
    [[ -n "$AIRSPY_SERIAL" ]] && sel=(-s "$AIRSPY_SERIAL")
    airspy_rx "${sel[@]}" -f "$mhz" -a $RATE -t 2 \
      -n $((RATE*secs)) -r "$f" 2>&1 | grep -q Streaming \
      || echo "!! airspy did not stream — board may be wedged; unplug/replug it"
  else
    rtl_sdr -f $CENTER -s $RATE -g $GAIN -n $((RATE*secs)) "$f" 2>/dev/null
  fi
}
EXT=$([[ "$SDR" == airspy ]] && echo cs16 || echo cu8)

probe() {
  local f="$OUT/probe_$(date +%H%M%S).$EXT"
  echo "── capturing 10 s at $((CENTER/1000000)).2 MHz, $SDR, gain ${GAIN:-default}…"
  capture 10 "$f"
  echo "── scanning…"
  local scan
  scan=$("$HS" --rate $RATE --freq $CENTER --scan "$f" 2>/dev/null)
  echo "$scan"
  # Prefer a channel the scan labelled CONTROL, but a 10 s probe often can't
  # see grants and labels the control channel as traffic — so fall back to
  # the top-ranked P25/voice hit. The degradation shows either way.
  local chan
  chan=$(echo "$scan" | awk '/CONTROL/ {print $2; exit}')
  [[ -z "$chan" ]] && chan=$(echo "$scan" | awk '/CQPSK|C4FM/ {print $2; exit}')
  if [[ -z "$chan" ]]; then
    echo "!! no P25 found — antenna, gain (try GAIN=30), or too deep; move toward a tower"
    return
  fi
  local off
  off=$(printf '%.0f' $(echo "($chan - $CENTER/1000000) * 1000000" | bc -l))
  echo "── decoding $chan MHz (offset $off Hz) — equalizer A/B:"
  for eq in "--no-equalizer:bare      " ":CMA       " "--dfe:DFE       "; do
    flag=${eq%%:*}; label=${eq#*:}
    "$HS" --rate $RATE --offset $off --cqpsk $flag --no-wav "$f" 2>/dev/null \
      | grep -E "frame syncs|TSBKs decoded" \
      | sed "s/^/   $label: /"
  done
  echo ""
  echo "READ IT: home baseline is ~35 TSBKs/s (350 per 10 s) and scan err ~0.4."
  echo "  err still < 0.5, TSBKs near max  -> too clean, drive on"
  echo "  err > 1, syncs land, TSBKs down   -> RECORD HERE: $0 record <spotname>"
  echo "  DFE >> CMA >> bare on this spot    -> the thesis regime; this is the capture to keep"
  echo "  no syncs at all                   -> too deep, back toward a tower"
}

record() {
  local name=${1:?usage: field-probe.sh record <spotname>}
  for i in 1 2 3; do
    local f="$OUT/${name}_$i.$EXT"
    echo "── recording 60 s -> $f  ($i/3)…"
    capture 60 "$f"
  done
  echo "center=$CENTER rate=$RATE sdr=$SDR gain=${GAIN:-default} date=$(date)" > "$OUT/${name}.meta"
  echo "done — note the location in $OUT/${name}.meta"
}

case "${1:-probe}" in
  probe)  probe ;;
  record) shift; record "$@" ;;
  *) echo "usage: $0 [probe | record <spotname>]"; exit 2 ;;
esac
