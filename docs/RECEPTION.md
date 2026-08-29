# Improving simulcast reception

A P25 simulcast site transmits the same signal from several towers on the
same frequency at the same time. Where more than one tower reaches your
antenna, the copies arrive at different delays and interfere — the garble
scanner users know as *simulcast distortion* (see the
[RadioReference wiki](https://wiki.radioreference.com/index.php/Simulcast_Distortion)).
HoosierSDR's equalizer-first CQPSK receiver is built for exactly this
regime, but the equalizer can only do so much: what reaches the antenna
still decides how well you decode. This page is the operational side —
what to change at the antenna, and how to use the app's meters to see
whether it helped instead of guessing by ear.

## The meters

All of these update live in the **Control channel** panel while following a
site (the control channel transmits continuously, so on a simulcast site it
tracks what your antenna hears moment to moment):

- **Multipath** — the simulcast-distortion meter. It reads the echo
  structure the adaptive equalizer has learned off its own taps: the
  percentage of tap energy spent cancelling echoes, and the RMS spread of
  their delays in microseconds. Near zero means one tower dominates;
  rising numbers mean several towers (or reflections) reach the antenna
  with comparable strength. The absolute number under-reads the true echo
  (a converged equalizer keeps most energy on its main tap), but it moves
  monotonically with echo strength — which is what you need to compare two
  antenna positions.
- **Lock** — CQPSK carrier-lock quality, 0–1. Solidly above ~0.8 when the
  receiver is decoding comfortably; sagging lock with strong signal is the
  classic simulcast signature.
- **Signal** — power in dBFS. When it turns red with a **⚠ clip** flag,
  samples are hitting the ADC rails: front-end overload, which garbles
  decode in a way that *looks* like simulcast distortion but is cured by
  turning the gain down, not by moving the antenna.

## What actually helps

Simulcast distortion is a geometry problem, so the fixes are geometric.
Change one thing at a time and watch the Multipath meter:

1. **Favor one tower.** A directional antenna (even a cheap corner
   reflector or yagi) aimed at the nearest tower is the single most
   effective fix — it shades the other towers so one copy dominates. Aim
   it while watching the Multipath meter, not the signal meter: the best
   heading is the lowest echo reading, which is often *not* the strongest
   signal.
2. **Move the antenna.** The interference pattern between towers has
   structure at the scale of the symbol period and the carrier wavelength —
   at 850 MHz a fade null is centimetres across. Moving the antenna a few
   feet, or even inches, can step out of a null. Window sills facing a
   single tower beat attic centres.
3. **Try lower, not just higher.** Height is the usual reception advice,
   but on simulcast it can hurt: a higher antenna sees *more* towers.
   Terrain or buildings that shade the distant towers are your friends.
4. **Set gain by the clip flag, not by the signal meter.** Raise gain until
   just before **⚠ clip** appears, then back off. Unlike an analog scanner,
   uniform attenuation does not change the *ratio* between tower signals —
   all copies drop together — so an attenuator only helps here when the
   problem was overload all along. If the Signal readout is strong, Lock is
   poor, and Multipath is high, you have true multipath: work items 1–3.
5. **Two antennas beat one.** Multipath fades at two antennas a fraction of
   a wavelength apart are largely independent; combining both receivers
   recovers what either alone loses. See [`DIVERSITY.md`](DIVERSITY.md)
   for the two-Airspy maximal-ratio-combining work.

## Reading the numbers together

| Signal | Lock | Multipath | Likely cause | Fix |
|---|---|---|---|---|
| strong | high | low | healthy | nothing |
| strong ⚠ clip | any | any | front-end overload | reduce gain |
| strong | low | high | several towers heard | aim / move antenna (items 1–3) |
| weak | low | low | not enough signal | more gain, better antenna, height |
| varies suddenly | dips | spikes | moving through fade nulls | reposition; diversity |

When a spot decodes poorly no matter what, capture it: export diagnostics
and IQ (see [`DIAGNOSTICS.md`](DIAGNOSTICS.md)) so the decode can be
reproduced and the DSP tuned against your signal.
