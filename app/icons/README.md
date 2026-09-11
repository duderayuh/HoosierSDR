# The mark

A QPSK constellation locked inside a scanning ring.

The inner diamond is the constellation the CQPSK demodulator works in; the
amber core is the carrier in lock; the ring is the site being scanned, broken
at the four points where the diamond comes through it. It is the app's one
claim — *equalize the channel before differential detection* — as a glyph: the
smeared constellation pulled into a shape you can decode.

Teal is `--signal`, amber is `--amber`, both straight from the app's theme.

## Files

| File | Use |
| --- | --- |
| `logo.svg` | Master. Follows `prefers-color-scheme`. Copied to `../dist/favicon.svg`. |
| `logo-dark.svg` / `logo-light.svg` | Flat-colour, for anywhere the theme must be pinned. |
| `logo-mono.svg` | Single colour via `currentColor` — stamps, print, one-colour contexts. |
| `icon.png`, `128x128@2x.png`, `128x128.png`, `32x32.png` | App icon: the mark on a dark tile, so it holds on any desktop. Listed in `tauri.conf.json`. |
| `../../docs/brand/lockup-{dark,light}.png` | Mark + wordmark, for the README. Chakra Petch 700 over IBM Plex Mono 500. |

The SVGs are the source of truth; every PNG is derived from them by rendering
in a browser at the target size and cropping to the element box. Regenerate
after editing an SVG — nothing in the build does it for you.

## Using it

In the app the mark is inlined in the page (see `.brandmark` in `style.css`)
rather than loaded as a file, so it picks up `var(--signal)` and `var(--amber)`
and follows the theme with everything else.

Don't recolour it outside those two variables, don't set the ring and diamond
in different colours, and don't stretch it — the ring is a true circle. Below
about 24 px the arcs close up into a solid ring; that's expected and still
reads. Clear space around the mark: half the diamond's width.
