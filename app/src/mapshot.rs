//! A picture of where a run is, drawn here rather than in a browser.
//!
//! A message that says "Cardiac Arrest · 1400 block of Example Street" asks
//! the reader to know the town by heart. A picture doesn't. So when a
//! tripwire fires on a run, this stitches the same map tiles the app draws
//! from, marks the run, draws the way to each hospital the care pathway
//! picked, and hands back a PNG for Telegram.
//!
//! Drawn in Rust, from the tile cache, on purpose: the alternative is
//! screenshotting a live web view, which needs the window open, a map
//! already centred where the run is, and a round trip through the UI
//! thread. A tripwire fires at 03:00 with the laptop lid shut. This works
//! there.
//!
//! Deliberately crude: no anti-aliasing, no fonts, no labels. The words go
//! in the caption, where they can be read by a screen reader and don't
//! depend on the recipient's eyesight or a font file we'd have to ship.

use tauri::AppHandle;

/// Big enough to read on a phone, small enough that Telegram doesn't
/// recompress it into mush.
pub const W: u32 = 640;
pub const H: u32 = 420;
const TILE: u32 = 256;
/// A stitch this size spans at most 4×3 tiles; anything more means the
/// projection maths went wrong, and hammering the tile server is not the
/// way to find out.
pub const MAX_TILES: usize = 24;
/// Closest in we'll ever go (a lone marker with no route) and furthest out
/// (two ends of a county apart).
const CLOSE: u8 = 16;
const FAR: u8 = 4;

const ROUTE: [u8; 3] = [0x1a, 0x5f, 0xb4];
const CASING: [u8; 3] = [0xff, 0xff, 0xff];
const RUN: [u8; 3] = [0xd5, 0x2b, 0x1e];
const DEST: [u8; 3] = [0x0b, 0x77, 0x3b];
const EDGE: [u8; 3] = [0x1c, 0x1c, 0x1c];

/// One hospital and the way to it.
pub struct Leg {
    pub to: (f64, f64),
    /// (lat, lon) along the way. Two points when nobody could route it.
    pub line: Vec<(f64, f64)>,
    /// A real route, rather than the ends joined up.
    pub road: bool,
}

/// A canvas of 8-bit RGB, row-major.
pub struct Canvas {
    pub w: u32,
    pub h: u32,
    pub px: Vec<u8>,
}

impl Canvas {
    fn new(w: u32, h: u32) -> Self {
        Self {
            w,
            h,
            px: vec![0xdd; (w * h * 3) as usize],
        }
    }

    /// Read one pixel back. Only the tests ask.
    #[cfg(test)]
    pub fn at(&self, x: u32, y: u32) -> [u8; 3] {
        let i = ((y * self.w + x) * 3) as usize;
        [self.px[i], self.px[i + 1], self.px[i + 2]]
    }

    fn set(&mut self, x: i64, y: i64, c: [u8; 3]) {
        if x < 0 || y < 0 || x >= self.w as i64 || y >= self.h as i64 {
            return;
        }
        let i = ((y as u32 * self.w + x as u32) * 3) as usize;
        self.px[i..i + 3].copy_from_slice(&c);
    }

    /// A filled disc. `r` is a radius in pixels, so `r = 1` is a 3×3 blob.
    fn disc(&mut self, x: i64, y: i64, r: i64, c: [u8; 3]) {
        for dy in -r..=r {
            for dx in -r..=r {
                if dx * dx + dy * dy <= r * r {
                    self.set(x + dx, y + dy, c);
                }
            }
        }
    }

    fn rect(&mut self, x: i64, y: i64, w: i64, h: i64, c: [u8; 3]) {
        for dy in 0..h {
            for dx in 0..w {
                self.set(x + dx, y + dy, c);
            }
        }
    }

    /// A thick line. `dash` draws only the on-phase of a 12-pixel cycle,
    /// which is how a straight-line guess is told apart from a route.
    /// `seen` carries the distance already walked, so dashes stay in step
    /// across the segments of one polyline.
    fn stroke(&mut self, a: (i64, i64), b: (i64, i64), r: i64, c: [u8; 3], dash: bool, seen: &mut i64) {
        let (mut x, mut y) = a;
        let (dx, dy) = ((b.0 - x).abs(), -(b.1 - y).abs());
        let (sx, sy) = (if x < b.0 { 1 } else { -1 }, if y < b.1 { 1 } else { -1 });
        let mut err = dx + dy;
        loop {
            if !dash || *seen % 12 < 7 {
                self.disc(x, y, r, c);
            }
            *seen += 1;
            if x == b.0 && y == b.1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x += sx;
            }
            if e2 <= dx {
                err += dx;
                y += sy;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// the projection
// ---------------------------------------------------------------------------

/// (lat, lon) → pixel on the whole-world map at this zoom. The usual Web
/// Mercator, the same one the tiles are cut on.
pub fn project(lat: f64, lon: f64, z: u8) -> (f64, f64) {
    let n = (1u64 << z) as f64 * TILE as f64;
    let lat = lat.clamp(-85.05112878, 85.05112878).to_radians();
    let x = (lon + 180.0) / 360.0 * n;
    let y = (1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0 * n;
    (x, y)
}

/// The closest zoom at which everything still fits, with a margin for the
/// markers — they're drawn around a point and would otherwise hang off the
/// edge.
pub fn fit(points: &[(f64, f64)], w: u32, h: u32) -> u8 {
    let (mw, mh) = (w.saturating_sub(72) as f64, h.saturating_sub(72) as f64);
    for z in (FAR..=CLOSE).rev() {
        let mut lo = (f64::MAX, f64::MAX);
        let mut hi = (f64::MIN, f64::MIN);
        for &(lat, lon) in points {
            let (x, y) = project(lat, lon, z);
            lo = (lo.0.min(x), lo.1.min(y));
            hi = (hi.0.max(x), hi.1.max(y));
        }
        if hi.0 - lo.0 <= mw && hi.1 - lo.1 <= mh {
            return z;
        }
    }
    FAR
}

/// Metres per pixel, for the scale bar. Latitude matters: a pixel at the
/// equator covers more ground than one in Indiana.
pub fn ground(lat: f64, z: u8) -> f64 {
    156_543.033_928 * lat.to_radians().cos() / (1u64 << z) as f64
}

/// A round number of metres that fits in `max` pixels — 1, 2 or 5 times a
/// power of ten, the way every scale bar has always done it.
pub fn bar(mpp: f64, max: u32) -> (u32, f64) {
    let mut best = (0u32, 0.0);
    for p in 0..8 {
        for mult in [1.0, 2.0, 5.0] {
            let m = mult * 10f64.powi(p);
            let px = (m / mpp).round();
            if px >= 24.0 && px <= max as f64 {
                best = (px as u32, m);
            }
        }
    }
    best
}

// ---------------------------------------------------------------------------
// tiles in
// ---------------------------------------------------------------------------

/// One tile's pixels, whatever colour type it arrived in. Tiles come as
/// RGB, RGBA, greyscale or a palette depending on who rendered them, so
/// everything is normalised to 8-bit RGB here rather than in the blitter.
pub fn decode(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let mut d = png::Decoder::new(bytes);
    d.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = d.read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    let (w, h) = (info.width, info.height);
    let n = (w * h) as usize;
    let rgb = match info.color_type {
        png::ColorType::Rgb => buf[..n * 3].to_vec(),
        png::ColorType::Rgba => {
            let mut out = Vec::with_capacity(n * 3);
            for p in buf[..n * 4].chunks(4) {
                // Tiles are opaque; where they aren't, the map's own
                // background shows through, which is what a viewer expects.
                out.extend_from_slice(&p[..3]);
            }
            out
        }
        png::ColorType::Grayscale => {
            let mut out = Vec::with_capacity(n * 3);
            for &g in &buf[..n] {
                out.extend_from_slice(&[g, g, g]);
            }
            out
        }
        png::ColorType::GrayscaleAlpha => {
            let mut out = Vec::with_capacity(n * 3);
            for p in buf[..n * 2].chunks(2) {
                out.extend_from_slice(&[p[0], p[0], p[0]]);
            }
            out
        }
        png::ColorType::Indexed => return Err("palette survived EXPAND".into()),
    };
    Ok((w, h, rgb))
}

fn blit(c: &mut Canvas, tile: (u32, u32, Vec<u8>), ox: i64, oy: i64) {
    let (tw, th, px) = tile;
    for ty in 0..th {
        let y = oy + ty as i64;
        if y < 0 || y >= c.h as i64 {
            continue;
        }
        for tx in 0..tw {
            let x = ox + tx as i64;
            if x < 0 || x >= c.w as i64 {
                continue;
            }
            let s = ((ty * tw + tx) * 3) as usize;
            c.set(x, y, [px[s], px[s + 1], px[s + 2]]);
        }
    }
}

// ---------------------------------------------------------------------------
// the picture
// ---------------------------------------------------------------------------

/// What was drawn, so the caption can describe it honestly.
pub struct Shot {
    pub png: Vec<u8>,
    /// The scale bar's length in metres, or 0 if it wouldn't fit.
    pub bar_m: f64,
    /// Tiles that never arrived. A picture with holes is still worth
    /// sending, but the caption shouldn't pretend it's complete.
    pub missing: usize,
}

/// Stitch, draw and encode, from the app's tile cache.
pub fn draw(app: &AppHandle, at: (f64, f64), legs: &[Leg]) -> Result<Shot, String> {
    compose(at, legs, &mut |z, x, y| {
        crate::tiles::fetch(app, &format!("{z}/{x}/{y}.png"))
    })
}

/// The picture itself. `tile` hands back a PNG for a tile, or an error if
/// it can't — which is how the tile source is kept out of the geometry, so
/// the drawing can be checked without a network or an app.
///
/// `at` is the run; `legs` are the hospitals the pathway picked, nearest
/// first.
pub fn compose(
    at: (f64, f64),
    legs: &[Leg],
    tile: &mut dyn FnMut(u8, i64, i64) -> Result<Vec<u8>, String>,
) -> Result<Shot, String> {
    // Everything that has to be visible: the run, each destination, and
    // every bend in between.
    let mut points = vec![at];
    for l in legs {
        points.push(l.to);
        points.extend(l.line.iter().copied());
    }
    let z = fit(&points, W, H);
    let (cx, cy) = {
        let (mut lo, mut hi) = ((f64::MAX, f64::MAX), (f64::MIN, f64::MIN));
        for &(lat, lon) in &points {
            let (x, y) = project(lat, lon, z);
            lo = (lo.0.min(x), lo.1.min(y));
            hi = (hi.0.max(x), hi.1.max(y));
        }
        ((lo.0 + hi.0) / 2.0, (lo.1 + hi.1) / 2.0)
    };
    // Top-left of the canvas, in whole-world pixels.
    let ox = cx - W as f64 / 2.0;
    let oy = cy - H as f64 / 2.0;
    let world = |lat: f64, lon: f64| {
        let (x, y) = project(lat, lon, z);
        ((x - ox).round() as i64, (y - oy).round() as i64)
    };

    let span = 1u64 << z;
    let (x0, y0) = ((ox / TILE as f64).floor() as i64, (oy / TILE as f64).floor() as i64);
    let (x1, y1) = (
        ((ox + W as f64) / TILE as f64).floor() as i64,
        ((oy + H as f64) / TILE as f64).floor() as i64,
    );
    let want = ((x1 - x0 + 1) * (y1 - y0 + 1)) as usize;
    if want > MAX_TILES {
        return Err(format!("{want} tiles for one picture — refusing"));
    }

    let mut c = Canvas::new(W, H);
    let mut missing = 0;
    for ty in y0..=y1 {
        for tx in x0..=x1 {
            // Off the top or bottom of the world: no tile exists. Wrapping
            // east-west is normal, though, and the tile server expects it.
            if ty < 0 || ty >= span as i64 {
                continue;
            }
            let wx = tx.rem_euclid(span as i64);
            match tile(z, wx, ty).and_then(|b| decode(&b)) {
                Ok(t) => blit(
                    &mut c,
                    t,
                    tx * TILE as i64 - ox.round() as i64,
                    ty * TILE as i64 - oy.round() as i64,
                ),
                Err(e) => {
                    eprintln!("[mapshot] {z}/{wx}/{ty}: {e}");
                    missing += 1;
                }
            }
        }
    }

    // Routes first, so the markers sit on top of them.
    for l in legs {
        let pts: Vec<(i64, i64)> = if l.line.len() >= 2 {
            l.line.iter().map(|&(a, b)| world(a, b)).collect()
        } else {
            vec![world(at.0, at.1), world(l.to.0, l.to.1)]
        };
        // A white casing under the line is what makes it readable over
        // both a pale field and a dark motorway.
        if l.road {
            let mut seen = 0;
            for w in pts.windows(2) {
                c.stroke(w[0], w[1], 4, CASING, false, &mut seen);
            }
        }
        let mut seen = 0;
        for w in pts.windows(2) {
            c.stroke(w[0], w[1], if l.road { 2 } else { 1 }, ROUTE, !l.road, &mut seen);
        }
    }

    for l in legs {
        let (x, y) = world(l.to.0, l.to.1);
        c.rect(x - 7, y - 7, 15, 15, EDGE);
        c.rect(x - 5, y - 5, 11, 11, DEST);
    }
    let (x, y) = world(at.0, at.1);
    c.disc(x, y, 9, EDGE);
    c.disc(x, y, 7, RUN);
    c.disc(x, y, 2, [0xff, 0xff, 0xff]);

    // The scale bar, bottom left, on a white pad so it reads over any map.
    let mpp = ground(at.0, z);
    let (bw, bm) = bar(mpp, 160);
    if bw > 0 {
        let (bx, by) = (16i64, H as i64 - 24);
        c.rect(bx - 4, by - 8, bw as i64 + 8, 18, CASING);
        c.rect(bx, by, bw as i64, 4, EDGE);
        c.rect(bx, by - 5, 3, 14, EDGE);
        c.rect(bx + bw as i64 - 3, by - 5, 3, 14, EDGE);
    }

    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, W, H);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().map_err(|e| e.to_string())?;
        w.write_image_data(&c.px).map_err(|e| e.to_string())?;
    }
    Ok(Shot {
        png: out,
        bar_m: if bw > 0 { bm } else { 0.0 },
        missing,
    })
}

/// "2 mi" / "500 m" — the scale bar's length, for the caption.
pub fn say_bar(m: f64) -> String {
    if m >= 1609.344 {
        let mi = m / 1609.344;
        format!("{} mi", trim(mi))
    } else if m >= 1000.0 {
        format!("{} km", trim(m / 1000.0))
    } else {
        format!("{m:.0} m")
    }
}

fn trim(v: f64) -> String {
    if (v - v.round()).abs() < 0.05 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tile of one flat colour, in each of the colour types a tile server
    /// might hand us.
    fn tile(kind: png::ColorType, c: [u8; 3]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, 4, 4);
        enc.set_color(kind);
        enc.set_depth(png::BitDepth::Eight);
        let px: Vec<u8> = match kind {
            png::ColorType::Rgb => (0..16).flat_map(|_| c).collect(),
            png::ColorType::Rgba => (0..16).flat_map(|_| [c[0], c[1], c[2], 0xff]).collect(),
            png::ColorType::Grayscale => vec![c[0]; 16],
            png::ColorType::Indexed => {
                enc.set_palette(c.to_vec());
                vec![0; 16]
            }
            png::ColorType::GrayscaleAlpha => (0..16).flat_map(|_| [c[0], 0xff]).collect(),
        };
        let mut w = enc.write_header().unwrap();
        w.write_image_data(&px).unwrap();
        drop(w);
        out
    }

    #[test]
    fn a_tile_decodes_whatever_colour_type_it_arrived_in() {
        for kind in [
            png::ColorType::Rgb,
            png::ColorType::Rgba,
            png::ColorType::Grayscale,
            png::ColorType::GrayscaleAlpha,
            png::ColorType::Indexed,
        ] {
            let grey = [0x40, 0x40, 0x40];
            let (w, h, px) = decode(&tile(kind, grey)).unwrap_or_else(|e| panic!("{kind:?}: {e}"));
            assert_eq!((w, h), (4, 4), "{kind:?}");
            assert_eq!(px.len(), 48, "{kind:?}");
            assert_eq!(&px[..3], &grey, "{kind:?} lost its colour");
        }
    }

    #[test]
    fn a_colour_tile_survives_the_palette() {
        // Greyscale can't carry this one, so it's the case that proves the
        // palette is really being expanded rather than read as indices.
        let red = [0xd0, 0x10, 0x20];
        let (_, _, px) = decode(&tile(png::ColorType::Indexed, red)).unwrap();
        assert_eq!(&px[..3], &red);
        let (_, _, px) = decode(&tile(png::ColorType::Rgba, red)).unwrap();
        assert_eq!(&px[..3], &red);
    }

    #[test]
    fn tiles_land_where_the_projection_says_they_should() {
        // Two tiles side by side, blitted at their own offsets: the seam
        // has to fall exactly on the tile boundary.
        let mut c = Canvas::new(8, 4);
        blit(&mut c, decode(&tile(png::ColorType::Rgb, [10, 20, 30])).unwrap(), 0, 0);
        blit(&mut c, decode(&tile(png::ColorType::Rgb, [40, 50, 60])).unwrap(), 4, 0);
        assert_eq!(c.at(0, 0), [10, 20, 30]);
        assert_eq!(c.at(3, 3), [10, 20, 30]);
        assert_eq!(c.at(4, 0), [40, 50, 60], "the second tile is one pixel out");
        assert_eq!(c.at(7, 3), [40, 50, 60]);
    }

    #[test]
    fn a_tile_that_hangs_off_the_canvas_is_clipped_not_wrapped() {
        let mut c = Canvas::new(4, 4);
        blit(&mut c, decode(&tile(png::ColorType::Rgb, [10, 20, 30])).unwrap(), -2, -2);
        assert_eq!(c.at(0, 0), [10, 20, 30]);
        // The bottom-right quarter was never covered, and nothing from the
        // tile's far side leaked into it.
        assert_eq!(c.at(3, 3), [0xdd, 0xdd, 0xdd]);
    }

    #[test]
    fn the_projection_agrees_with_the_tile_grid() {
        // Greenwich at the equator is the middle of the world at any zoom.
        for z in [0, 4, 12, 16] {
            let n = (1u64 << z) as f64 * 256.0;
            let (x, y) = project(0.0, 0.0, z);
            assert!((x - n / 2.0).abs() < 0.001, "z{z} x");
            assert!((y - n / 2.0).abs() < 0.001, "z{z} y");
        }
        // North is up, east is right.
        let (ax, ay) = project(40.0, -86.0, 14);
        let (bx, by) = project(41.0, -85.0, 14);
        assert!(bx > ax, "east should be right");
        assert!(by < ay, "north should be up");
    }

    #[test]
    fn the_zoom_is_the_closest_one_that_still_fits_everything() {
        let run = (39.7684, -86.1581);
        // A hospital a few streets away: close in.
        let near = (39.7754, -86.1520);
        let z = fit(&[run, near], W, H);
        assert!(z >= 13, "two nearby points should not zoom out to {z}");
        // An ECMO centre 60 km away: further out, and both must still land
        // inside the canvas.
        let far = (40.2, -86.6);
        let zf = fit(&[run, far], W, H);
        assert!(zf < z, "a longer route should zoom out ({zf} vs {z})");
        for &(lat, lon) in &[run, far] {
            let (x, y) = project(lat, lon, zf);
            let (cx, cy) = {
                let a = project(run.0, run.1, zf);
                let b = project(far.0, far.1, zf);
                ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0)
            };
            let (px, py) = (x - (cx - W as f64 / 2.0), y - (cy - H as f64 / 2.0));
            assert!(
                px > 0.0 && px < W as f64 && py > 0.0 && py < H as f64,
                "({lat}, {lon}) fell off the picture at z{zf}: ({px}, {py})"
            );
        }
    }

    #[test]
    fn two_points_across_a_state_still_fit() {
        let z = fit(&[(37.8, -88.1), (41.7, -84.8)], W, H);
        let a = project(37.8, -88.1, z);
        let b = project(41.7, -84.8, z);
        assert!((a.0 - b.0).abs() <= (W - 72) as f64, "too wide at z{z}");
        assert!((a.1 - b.1).abs() <= (H - 72) as f64, "too tall at z{z}");
    }

    #[test]
    fn a_line_is_drawn_between_its_ends_and_nowhere_else() {
        let mut c = Canvas::new(64, 64);
        let mut seen = 0;
        c.stroke((8, 32), (56, 32), 1, ROUTE, false, &mut seen);
        assert_eq!(c.at(8, 32), ROUTE, "no ink at the start");
        assert_eq!(c.at(32, 32), ROUTE, "no ink in the middle");
        assert_eq!(c.at(56, 32), ROUTE, "no ink at the end");
        assert_eq!(c.at(32, 50), [0xdd, 0xdd, 0xdd], "ink 18px off the line");
        assert_eq!(c.at(2, 32), [0xdd, 0xdd, 0xdd], "ink before the start");
    }

    #[test]
    fn a_dashed_line_has_gaps_and_a_solid_one_does_not() {
        let gaps = |dash: bool| {
            let mut c = Canvas::new(64, 8);
            let mut seen = 0;
            c.stroke((0, 4), (63, 4), 0, ROUTE, dash, &mut seen);
            (0..64).filter(|&x| c.at(x, 4) != ROUTE).count()
        };
        assert_eq!(gaps(false), 0, "a solid line has holes in it");
        assert!(gaps(true) > 10, "a dashed line came out solid");
    }

    #[test]
    fn the_scale_bar_is_a_round_number_that_fits() {
        // Zoom 14 at this latitude: ~7.3 m per pixel. (9.55 is the figure
        // at the equator; cos(39.77°) is the difference.)
        let mpp = ground(39.77, 14);
        assert!((mpp - 7.34).abs() < 0.1, "{mpp} m/px");
        assert!(
            (ground(0.0, 14) - 9.55).abs() < 0.1,
            "the equator figure moved: {}",
            ground(0.0, 14)
        );
        let (px, m) = bar(mpp, 160);
        assert!(px <= 160 && px >= 24, "{px} px");
        assert!(
            [1.0, 2.0, 5.0].contains(&(m / 10f64.powi(m.log10().floor() as i32))),
            "{m} m is not a round number"
        );
        // Zoomed right out, the bar still finds a length.
        let (px, m) = bar(ground(39.77, 6), 160);
        assert!(px > 0 && m > 10_000.0, "{px} px / {m} m");
    }

    /// A blank 256×256 tile, so a composed picture has a known background.
    fn plain(c: [u8; 3]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, TILE, TILE);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().unwrap();
        w.write_image_data(&(0..TILE * TILE).flat_map(|_| c).collect::<Vec<u8>>())
            .unwrap();
        drop(w);
        out
    }

    /// Where a lat/lon landed on the finished canvas, worked out the same
    /// way `compose` does. Used to check the drawing is where the
    /// projection says it should be.
    fn spot(points: &[(f64, f64)], of: (f64, f64)) -> (u32, u32) {
        let z = fit(points, W, H);
        let (mut lo, mut hi) = ((f64::MAX, f64::MAX), (f64::MIN, f64::MIN));
        for &(lat, lon) in points {
            let (x, y) = project(lat, lon, z);
            lo = (lo.0.min(x), lo.1.min(y));
            hi = (hi.0.max(x), hi.1.max(y));
        }
        let (ox, oy) = (
            (lo.0 + hi.0) / 2.0 - W as f64 / 2.0,
            (lo.1 + hi.1) / 2.0 - H as f64 / 2.0,
        );
        let (x, y) = project(of.0, of.1, z);
        ((x - ox).round() as u32, (y - oy).round() as u32)
    }

    fn read_back(png: &[u8]) -> (u32, u32, Vec<u8>) {
        decode(png).unwrap()
    }

    #[test]
    fn the_run_and_its_hospital_are_marked_where_the_map_puts_them() {
        let at = (39.7684, -86.1581);
        let to = (39.7900, -86.1200);
        let legs = vec![Leg {
            to,
            line: vec![at, (39.78, -86.14), to],
            road: true,
        }];
        let shot = compose(at, &legs, &mut |_, _, _| Ok(plain([0xf0, 0xee, 0xe6]))).unwrap();
        assert_eq!(shot.missing, 0);
        let (w, h, px) = read_back(&shot.png);
        assert_eq!((w, h), (W, H), "the picture is not the size asked for");
        let pixel = |x: u32, y: u32| {
            let i = ((y * w + x) * 3) as usize;
            [px[i], px[i + 1], px[i + 2]]
        };

        let all: Vec<(f64, f64)> = vec![at, to, (39.78, -86.14)];
        let (rx, ry) = spot(&all, at);
        let (dx, dy) = spot(&all, to);
        // The run's marker has a white pip at its centre, inside red.
        assert_eq!(pixel(rx, ry), [0xff, 0xff, 0xff], "no pip at the run");
        assert_eq!(pixel(rx, ry - 5), RUN, "the run is not marked in red");
        assert_eq!(pixel(dx, dy), DEST, "the hospital is not marked in green");
        // Far from both, the map's own colour still shows: the drawing did
        // not flood the canvas.
        assert_eq!(pixel(4, 4), [0xf0, 0xee, 0xe6], "the tile was overpainted");
    }

    #[test]
    fn the_way_there_is_actually_drawn_between_them() {
        let at = (39.7684, -86.1581);
        let to = (39.7900, -86.1200);
        let bend = (39.7700, -86.1300);
        let legs = vec![Leg {
            to,
            line: vec![at, bend, to],
            road: true,
        }];
        let shot = compose(at, &legs, &mut |_, _, _| Ok(plain([0xf0, 0xee, 0xe6]))).unwrap();
        let (w, _, px) = read_back(&shot.png);
        let pixel = |x: u32, y: u32| {
            let i = ((y * w + x) * 3) as usize;
            [px[i], px[i + 1], px[i + 2]]
        };
        // The route passes through its own bend, which is the point a
        // straight line between the ends would miss.
        let (bx, by) = spot(&[at, to, bend], bend);
        assert_eq!(pixel(bx, by), ROUTE, "the route skipped its bend");
        let ink = (0..W * H)
            .filter(|i| {
                let p = ((i % W), (i / W));
                pixel(p.0, p.1) == ROUTE
            })
            .count();
        assert!(ink > 200, "only {ink} route pixels — the line is missing");
    }

    #[test]
    fn a_tile_that_never_arrives_is_counted_not_fatal() {
        let at = (39.7684, -86.1581);
        let mut n = 0;
        let shot = compose(at, &[], &mut |_, _, _| {
            n += 1;
            if n == 1 {
                Err("502".into())
            } else {
                Ok(plain([0xf0, 0xee, 0xe6]))
            }
        })
        .unwrap();
        assert_eq!(shot.missing, 1, "a failed tile was not reported");
        assert_eq!(read_back(&shot.png).0, W, "the picture was abandoned");
    }

    #[test]
    fn a_lone_run_is_still_drawn() {
        // No pathway, no hospitals: the picture is just the place, and the
        // zoom must not run away with a single point.
        let at = (39.7684, -86.1581);
        let shot = compose(at, &[], &mut |_, _, _| Ok(plain([0xff, 0xff, 0xff]))).unwrap();
        let (_, _, px) = read_back(&shot.png);
        let centre = (((H / 2) * W + W / 2) * 3) as usize;
        assert_eq!(&px[centre..centre + 3], &[0xff, 0xff, 0xff], "no pip at the centre");
        assert!(shot.bar_m > 0.0, "no scale bar was drawn");
    }

    #[test]
    fn one_picture_never_costs_more_than_a_handful_of_tiles() {
        let at = (39.7684, -86.1581);
        let far = (41.0, -84.0);
        let mut asked = 0;
        let shot = compose(
            at,
            &[Leg { to: far, line: vec![at, far], road: false }],
            &mut |_, _, _| {
                asked += 1;
                Ok(plain([0xf0, 0xee, 0xe6]))
            },
        )
        .unwrap();
        assert!(asked <= MAX_TILES, "{asked} tiles for one picture");
        assert!(asked >= 4, "only {asked} tiles — the canvas can't be covered");
        assert_eq!(shot.missing, 0);
    }

    /// Renders from the real tile server and writes the result out to look
    /// at. Ignored: it needs the network, and a test suite shouldn't.
    ///   cargo test -- --ignored a_real_map_to_look_at
    #[test]
    #[ignore]
    fn a_real_map_to_look_at() {
        let at = (39.7684, -86.1581);
        let to = (39.8318, -86.0223);
        let legs = vec![Leg {
            to,
            line: vec![at, (39.79, -86.11), (39.81, -86.06), to],
            road: true,
        }];
        let shot = compose(at, &legs, &mut |z, x, y| {
            let url = format!("https://tile.openstreetmap.org/{z}/{x}/{y}.png");
            ureq::get(&url)
                .header("User-Agent", "HoosierSDR/0.1 (map picture check)")
                .call()
                .map_err(|e| e.to_string())?
                .body_mut()
                .with_config()
                .limit(2 * 1024 * 1024)
                .read_to_vec()
                .map_err(|e| e.to_string())
        })
        .unwrap();
        let out = std::env::var("HS_SHOT").unwrap_or_else(|_| "/tmp/mapshot.png".into());
        std::fs::write(&out, &shot.png).unwrap();
        eprintln!(
            "wrote {out} — {} bytes, bar {}, {} tiles missing",
            shot.png.len(),
            say_bar(shot.bar_m),
            shot.missing
        );
        assert_eq!(shot.missing, 0, "tiles did not arrive");
    }

    #[test]
    fn the_bar_is_described_in_units_a_reader_uses() {
        assert_eq!(say_bar(1609.344), "1 mi");
        assert_eq!(say_bar(8046.72), "5 mi");
        assert_eq!(say_bar(500.0), "500 m");
        assert_eq!(say_bar(1000.0), "1 km");
    }
}
