//! Placing an address the geocoder could not.
//!
//! Roughly a third of dispatch addresses never resolve. Two reasons, and the
//! radio hands us the cure for both: dispatchers read out a grid reference
//! after the address ("… 1834 hours, location 7700 South, 1100 West"). The
//! grid is a plain linear coordinate system, so a handful of addresses that
//! *did* geocode are enough to learn it — and then a grid reference alone
//! places a call within a few hundred metres.
//!
//! That is worth having on its own, but it is worth more as a referee: when
//! the street name came back from the transcriber mis-heard ("LeVont Lane"
//! for "Lamont Lane"), the candidates that sound alike can be scored by how
//! close they land to the grid point, which is how a person would do it.
//!
//! Nothing here talks to the network. The calibration is fitted from the
//! listener's own library, and the gazetteer is built from the addresses
//! their geocoder has already confirmed.

use crate::fuzzy;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A grid reference as read on the radio: blocks north/south and east/west
/// of the zero streets. South and west are negative.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grid {
    pub ns: i32,
    pub ew: i32,
}

/// What the grid means in degrees, learned from calls that did geocode.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Calibration {
    /// Where 0 north/south, 0 east/west lands.
    #[serde(default)]
    pub lat0: f64,
    #[serde(default)]
    pub lon0: f64,
    /// Degrees per grid unit.
    #[serde(default)]
    pub lat_per: f64,
    #[serde(default)]
    pub lon_per: f64,
    /// How many addresses it was fitted from, and how far off it was on
    /// them (metres, median) — shown so the listener can judge it.
    #[serde(default)]
    pub samples: u32,
    #[serde(default)]
    pub median_m: f64,
    #[serde(default)]
    pub at: i64,
}

impl Calibration {
    pub fn usable(&self) -> bool {
        self.samples >= MIN_SAMPLES && self.lat_per > 0.0 && self.lon_per > 0.0
    }

    /// Where a grid reference lands.
    pub fn place(&self, g: Grid) -> Option<(f64, f64)> {
        if !self.usable() {
            return None;
        }
        let lat = self.lat0 + self.lat_per * g.ns as f64;
        let lon = self.lon0 + self.lon_per * g.ew as f64;
        (-90.0..=90.0).contains(&lat).then_some((lat, lon))
    }

    /// Where it lands, if that is anywhere the listener can hear. A digit
    /// mis-heard in a grid reference ("89100 West" for "8900 West") throws
    /// the point a hundred miles; a point outside the search radius is not
    /// evidence of anything.
    pub fn place_near(&self, g: Grid, home: (f64, f64), radius_km: f64) -> Option<(f64, f64)> {
        let (lat, lon) = self.place(g)?;
        let d = crate::dispatch::haversine_m(home.0, home.1, lat, lon);
        (d <= radius_km * 1000.0).then_some((lat, lon))
    }
}

/// Fewer than this and the fit is guesswork.
const MIN_SAMPLES: u32 = 12;
/// Theil–Sen is quadratic in the sample count; this is plenty for a fit.
const MAX_SAMPLES: usize = 400;

/// Read a grid reference out of what the dispatcher said. Both halves must
/// be there — half a reference places nothing.
pub fn parse_grid(s: &str) -> Option<Grid> {
    let low = s.to_ascii_lowercase();
    let ns = axis(&low, "north", "south")?;
    let ew = axis(&low, "east", "west")?;
    Some(Grid { ns, ew })
}

/// The number in front of `pos`/`neg`, signed. Grid numbers run to five
/// digits; a longer run is a phone number or a mis-heard address.
fn axis(low: &str, pos: &str, neg: &str) -> Option<i32> {
    let mut best: Option<(usize, i32)> = None;
    for (word, sign) in [(pos, 1), (neg, -1)] {
        let mut from = 0;
        while let Some(rel) = low[from..].find(word) {
            let at = from + rel;
            from = at + word.len();
            // A word boundary, so "southport" is not "south".
            if low[from..].starts_with(|c: char| c.is_alphanumeric()) {
                continue;
            }
            let head = low[..at].trim_end_matches([' ', ',', '.']);
            let digits: String = head
                .chars()
                .rev()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if digits.is_empty() || digits.len() > 5 {
                continue;
            }
            let n: i32 = digits.chars().rev().collect::<String>().parse().ok()?;
            if best.is_none_or(|(prev, _)| at < prev) {
                best = Some((at, n * sign));
            }
        }
    }
    best.map(|(_, n)| n)
}

// ---------------------------------------------------------------------------
// calibration
// ---------------------------------------------------------------------------

/// Every incident that both geocoded and carried a grid reference, deduped:
/// the same address dispatched twice is one piece of evidence, not two.
fn samples(c: &Connection) -> Vec<(f64, f64, Grid)> {
    let Ok(mut q) = c.prepare(
        "SELECT i.lat, i.lon, json_extract(ic.extracted, '$.grid')
           FROM incidents i JOIN incident_calls ic ON ic.incident = i.id
          WHERE i.lat IS NOT NULL AND i.geocode = 'ok'",
    ) else {
        return Vec::new();
    };
    let rows = q.query_map([], |r| {
        Ok((
            r.get::<_, f64>(0)?,
            r.get::<_, f64>(1)?,
            r.get::<_, Option<String>>(2)?.unwrap_or_default(),
        ))
    });
    let Ok(rows) = rows else { return Vec::new() };
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (lat, lon, g) in rows.flatten() {
        let Some(g) = parse_grid(&g) else { continue };
        let key = ((lat * 1e5) as i64, (lon * 1e5) as i64, g.ns, g.ew);
        if seen.insert(key) {
            out.push((lat, lon, g));
        }
    }
    out
}

/// Fit the grid from the library. Theil–Sen (the median of the slopes
/// through every pair) rather than least squares: a mis-heard digit moves
/// one sample a mile, and the median does not care.
pub fn calibrate(c: &Connection) -> Option<Calibration> {
    let mut s = samples(c);
    if s.len() < MIN_SAMPLES as usize {
        return None;
    }
    s.truncate(MAX_SAMPLES);
    let lat_per = median_slope(s.iter().map(|(lat, _, g)| (g.ns as f64, *lat)))?;
    let lon_per = median_slope(s.iter().map(|(_, lon, g)| (g.ew as f64, *lon)))?;
    let lat0 = median(&mut s.iter().map(|(lat, _, g)| lat - lat_per * g.ns as f64).collect())?;
    let lon0 = median(&mut s.iter().map(|(_, lon, g)| lon - lon_per * g.ew as f64).collect())?;
    let mut cal = Calibration {
        lat0,
        lon0,
        lat_per,
        lon_per,
        samples: s.len() as u32,
        median_m: 0.0,
        at: crate::library::now(),
    };
    let mut errs: Vec<f64> = s
        .iter()
        .filter_map(|(lat, lon, g)| {
            cal.place(*g)
                .map(|(plat, plon)| crate::dispatch::haversine_m(*lat, *lon, plat, plon))
        })
        .collect();
    cal.median_m = median(&mut errs).unwrap_or(0.0);
    Some(cal)
}

fn median(v: &mut Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(v[v.len() / 2])
}

/// The median slope over every pair of points — steady when a third of the
/// samples are nonsense.
fn median_slope(pts: impl Iterator<Item = (f64, f64)>) -> Option<f64> {
    let pts: Vec<(f64, f64)> = pts.collect();
    let mut slopes = Vec::with_capacity(pts.len() * pts.len() / 2);
    for (i, (x1, y1)) in pts.iter().enumerate() {
        for (x2, y2) in pts.iter().skip(i + 1) {
            if (x1 - x2).abs() > f64::EPSILON {
                slopes.push((y1 - y2) / (x1 - x2));
            }
        }
    }
    median(&mut slopes).filter(|s| s.is_finite() && *s != 0.0)
}

// ---------------------------------------------------------------------------
// the gazetteer: streets this listener's geocoder has already confirmed
// ---------------------------------------------------------------------------

/// A street the geocoder has placed before, and roughly where it is.
#[derive(Clone, Debug)]
pub struct Street {
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    /// How many confirmed addresses sit on it.
    pub hits: u32,
}

/// Street names lifted from the geocoder's own answers. Nominatim hands
/// back "5863, East 16th Street, Indianapolis, …" — the second part is the
/// street, and the incident's coordinates say where that instance of it is.
pub fn gazetteer(c: &Connection) -> Vec<Street> {
    let Ok(mut q) = c.prepare(
        "SELECT validated, lat, lon FROM incidents
          WHERE geocode = 'ok' AND lat IS NOT NULL AND validated <> ''",
    ) else {
        return Vec::new();
    };
    let rows = q.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, f64>(1)?,
            r.get::<_, f64>(2)?,
        ))
    });
    let Ok(rows) = rows else { return Vec::new() };
    let mut by_name: HashMap<String, (f64, f64, u32)> = HashMap::new();
    for (display, lat, lon) in rows.flatten() {
        for name in street_names(&display) {
            let e = by_name.entry(name).or_insert((0.0, 0.0, 0));
            e.0 += lat;
            e.1 += lon;
            e.2 += 1;
        }
    }
    let mut out: Vec<Street> = by_name
        .into_iter()
        .map(|(name, (lat, lon, hits))| Street {
            name,
            lat: lat / hits as f64,
            lon: lon / hits as f64,
            hits,
        })
        .collect();
    out.sort_by(|a, b| b.hits.cmp(&a.hits).then_with(|| a.name.cmp(&b.name)));
    out
}

/// The street-looking parts of a geocoder display name. An intersection
/// answer names two streets, so every part that ends in a street word counts.
fn street_names(display: &str) -> Vec<String> {
    display
        .split(',')
        .map(str::trim)
        .filter(|p| looks_like_street(p))
        .map(|p| p.to_ascii_lowercase())
        .collect()
}

const STREET_WORDS: &[&str] = &[
    "street", "avenue", "road", "drive", "lane", "court", "circle", "boulevard", "place", "way",
    "trail", "terrace", "parkway", "pike", "run", "square", "plaza", "crossing", "path", "walk",
    "row", "bend", "loop", "ridge", "highway", "st", "ave", "rd", "dr", "ln", "ct", "blvd", "pkwy",
];

fn looks_like_street(p: &str) -> bool {
    let low = p.to_ascii_lowercase();
    let last = low.split_whitespace().next_back().unwrap_or("");
    // "5863" and "46218" are the house number and the postcode.
    low.split_whitespace().count() >= 2 && STREET_WORDS.contains(&last)
}

/// How near a candidate must land to the grid point before a change of
/// direction word ("South Meridian" for "North Meridian") is believable,
/// and how far is too far for any candidate at all.
const DIR_CONFIRM_M: f64 = 1_200.0;
const TOO_FAR_M: f64 = 4_000.0;

/// The streets that sound like `heard`, nearest first to `near` when a grid
/// reference gave us somewhere to look. Only genuinely close-sounding names
/// come back — this is for fixing a mis-heard word, not for guessing.
///
/// Two things it will not do. It never swaps a number, because "19th" and
/// "17th" sound nothing alike however similar they look. And it only
/// changes the direction word — a mile of difference in a grid city — when
/// the grid reference says that is where the call is.
pub fn sound_alike(list: &[Street], heard: &str, near: Option<(f64, f64)>) -> Vec<Street> {
    let heard = heard.to_ascii_lowercase();
    let (h_body, h_tail) = split_tail(&heard);
    let (h_dir, h_core) = split_dir(&h_body);
    let mut out: Vec<(f64, Street)> = list
        .iter()
        .filter_map(|s| {
            let (body, tail) = split_tail(&s.name);
            // The street word itself must agree; "Lamont Lane" is not a
            // candidate for "Lamont Court".
            if tail != h_tail || body == h_body {
                return None;
            }
            let (dir, core) = split_dir(&body);
            if digits_of(&core) != digits_of(&h_core) {
                return None;
            }
            if !fuzzy::alike(&letters(&core), &letters(&h_core)) {
                return None;
            }
            let d = near.map(|(lat, lon)| crate::dispatch::haversine_m(lat, lon, s.lat, s.lon));
            if d.is_some_and(|d| d > TOO_FAR_M) {
                return None;
            }
            if dir != h_dir && !d.is_some_and(|d| d <= DIR_CONFIRM_M) {
                return None;
            }
            Some((d.unwrap_or(0.0), s.clone()))
        })
        .collect();
    out.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.1.hits.cmp(&a.1.hits))
    });
    out.into_iter().map(|(_, s)| s).collect()
}

/// "north shadeland" → ("north", "shadeland"); "shadeland" → ("", "shadeland").
fn split_dir(body: &str) -> (String, String) {
    let mut parts: Vec<&str> = body.split_whitespace().collect();
    let dir = match parts.first() {
        Some(w) if matches!(*w, "north" | "south" | "east" | "west") && parts.len() > 1 => {
            parts.remove(0).to_string()
        }
        _ => String::new(),
    };
    (dir, parts.join(" "))
}

/// A name with the hyphens and apostrophes taken out, so "T-Stone" and
/// "Keystone" are compared as words rather than as punctuation.
fn letters(body: &str) -> String {
    body.chars().filter(|c| c.is_alphanumeric() || *c == ' ').collect()
}

/// The digits in a street name, in order: "west 86th" → "86".
fn digits_of(body: &str) -> String {
    body.chars().filter(char::is_ascii_digit).collect()
}

/// "east 16th street" → ("east 16th", "street").
fn split_tail(name: &str) -> (String, String) {
    let mut parts: Vec<&str> = name.split_whitespace().collect();
    let tail = parts.pop().unwrap_or("").to_string();
    (parts.join(" "), tail)
}

/// Swap the street part of an address for `street`, keeping the house number
/// and anything after it.
pub fn with_street(address: &str, heard_street: &str, street: &str) -> String {
    let low = address.to_ascii_lowercase();
    let Some(at) = low.find(&heard_street.to_ascii_lowercase()) else {
        return address.to_string();
    };
    format!(
        "{}{}{}",
        &address[..at],
        title_case(street),
        &address[at + heard_street.len()..]
    )
}

fn title_case(s: &str) -> String {
    s.split(' ')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) if f.is_ascii_alphabetic() => {
                    f.to_ascii_uppercase().to_string() + c.as_str()
                }
                _ => w.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The street part of an address as heard: everything after the house
/// number, before an apartment or a city.
pub fn street_of(address: &str) -> Option<String> {
    let a = address.split(',').next().unwrap_or(address).trim();
    // An intersection names two streets; swapping one of them is another
    // job than this.
    if a.to_ascii_lowercase().contains(" and ") {
        return None;
    }
    let rest = a
        .strip_prefix(|c: char| c.is_ascii_digit())
        .map(|_| a.trim_start_matches(|c: char| c.is_ascii_digit() || c == '-'))
        .unwrap_or(a)
        .trim();
    looks_like_street(rest).then(|| rest.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_grid_reference_is_read_off_the_end_of_the_call() {
        assert_eq!(
            parse_grid("1834 hours, location 7700 South, 1100 West"),
            Some(Grid {
                ns: -7700,
                ew: -1100
            })
        );
        assert_eq!(
            parse_grid("Location 6300 South 6800 West"),
            Some(Grid {
                ns: -6300,
                ew: -6800
            })
        );
        assert_eq!(parse_grid("3600 North 2000 East"), Some(Grid { ns: 3600, ew: 2000 }));
        // Half a reference places nothing.
        assert_eq!(parse_grid("9100 North"), None);
        assert_eq!(parse_grid("no grid here"), None);
        // A street name that starts with a compass word is not a grid.
        assert_eq!(parse_grid("4850 East Southport Road"), None);
    }

    /// A synthetic city: one degree of latitude per 10,000 grid units, half
    /// that east-west, with a fifth of the samples badly mis-heard.
    fn fake_library() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE incidents (id INTEGER PRIMARY KEY, lat REAL, lon REAL,
                 geocode TEXT NOT NULL DEFAULT '', validated TEXT NOT NULL DEFAULT '');
             CREATE TABLE incident_calls (incident INTEGER, call INTEGER, extracted TEXT);",
        )
        .unwrap();
        let (lat0, lon0) = (40.0, -86.0);
        let (lat_per, lon_per) = (1.0e-4, 5.0e-5);
        for i in 0..60i32 {
            let ns = (i - 30) * 100;
            let ew = (i * 37 % 61 - 30) * 100;
            let bad = i % 5 == 0;
            let lat = lat0 + lat_per * ns as f64 + if bad { 0.05 } else { 0.0 };
            let lon = lon0 + lon_per * ew as f64;
            c.execute(
                "INSERT INTO incidents (id, lat, lon, geocode, validated) VALUES (?1, ?2, ?3, 'ok', ?4)",
                rusqlite::params![i, lat, lon, format!("{}, Example Street, Testville", 100 + i)],
            )
            .unwrap();
            let ns_w = if ns < 0 { "South" } else { "North" };
            let ew_w = if ew < 0 { "West" } else { "East" };
            c.execute(
                "INSERT INTO incident_calls (incident, call, extracted) VALUES (?1, ?1, ?2)",
                rusqlite::params![
                    i,
                    format!(
                        r#"{{"grid":"{} {} {} {}"}}"#,
                        ns.abs(),
                        ns_w,
                        ew.abs(),
                        ew_w
                    )
                ],
            )
            .unwrap();
        }
        c
    }

    #[test]
    fn the_grid_is_learned_from_calls_that_did_place() {
        let c = fake_library();
        let cal = calibrate(&c).expect("enough samples");
        assert!(cal.usable());
        assert!(
            (cal.lat_per - 1.0e-4).abs() < 1.0e-6,
            "north/south scale: {}",
            cal.lat_per
        );
        assert!(
            (cal.lon_per - 5.0e-5).abs() < 1.0e-6,
            "east/west scale: {}",
            cal.lon_per
        );
        assert!((cal.lat0 - 40.0).abs() < 1.0e-3, "origin: {}", cal.lat0);
        let (lat, lon) = cal.place(Grid { ns: 1000, ew: 2000 }).unwrap();
        assert!((lat - 40.1).abs() < 0.01 && (lon - (-85.9)).abs() < 0.01);
    }

    #[test]
    fn too_few_calls_is_no_calibration() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE incidents (id INTEGER PRIMARY KEY, lat REAL, lon REAL,
                 geocode TEXT NOT NULL DEFAULT '', validated TEXT NOT NULL DEFAULT '');
             CREATE TABLE incident_calls (incident INTEGER, call INTEGER, extracted TEXT);",
        )
        .unwrap();
        assert!(calibrate(&c).is_none());
        assert!(!Calibration::default().usable());
        assert_eq!(Calibration::default().place(Grid { ns: 1, ew: 1 }), None);
    }

    #[test]
    fn the_gazetteer_comes_from_answers_the_geocoder_already_gave() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE incidents (id INTEGER PRIMARY KEY, lat REAL, lon REAL,
                 geocode TEXT NOT NULL DEFAULT '', validated TEXT NOT NULL DEFAULT '');",
        )
        .unwrap();
        for (i, (v, lat, lon)) in [
            ("100, Lamont Lane, Testville, 40001, United States", 40.0, -86.0),
            ("140, Lamont Lane, Testville, 40001, United States", 40.01, -86.0),
            ("7, Beech Grove Road, Testville, 40001, United States", 41.0, -85.0),
        ]
        .into_iter()
        .enumerate()
        {
            c.execute(
                "INSERT INTO incidents (id, lat, lon, geocode, validated) VALUES (?1,?2,?3,'ok',?4)",
                rusqlite::params![i as i64, lat, lon, v],
            )
            .unwrap();
        }
        let g = gazetteer(&c);
        assert_eq!(g[0].name, "lamont lane");
        assert_eq!(g[0].hits, 2);
        assert!((g[0].lat - 40.005).abs() < 1e-6, "averaged: {}", g[0].lat);
        // The house number and the postcode are not streets.
        assert!(g.iter().all(|s| s.name.chars().any(|c| c.is_alphabetic())));
    }

    #[test]
    fn numbers_and_directions_are_not_guessed_away() {
        let list = vec![
            Street { name: "north 17th avenue".into(), lat: 40.0, lon: -86.0, hits: 5 },
            Street { name: "north meridian street".into(), lat: 40.0, lon: -86.0, hits: 9 },
        ];
        // "19th" and "17th" look alike and sound nothing alike.
        assert!(sound_alike(&list, "north 19th avenue", None).is_empty());
        // A flipped direction is a mile of difference: not without evidence.
        assert!(sound_alike(&list, "south meridian street", None).is_empty());
        // With the grid pointing at it, the flip is believable.
        let near = Some((40.001, -86.001));
        assert_eq!(sound_alike(&list, "south meridian street", near).len(), 1);
        // But not when the grid points somewhere else entirely.
        let far = Some((41.0, -85.0));
        assert!(sound_alike(&list, "south meridian street", far).is_empty());
    }

    #[test]
    fn a_mis_heard_street_finds_its_neighbour() {
        let list = vec![
            Street { name: "lamont lane".into(), lat: 40.0, lon: -86.0, hits: 4 },
            Street { name: "lamont court".into(), lat: 40.0, lon: -86.0, hits: 2 },
            Street { name: "beech grove road".into(), lat: 41.0, lon: -85.0, hits: 9 },
        ];
        let hits = sound_alike(&list, "levont lane", None);
        assert_eq!(hits.len(), 1, "only the one that sounds alike: {hits:?}");
        assert_eq!(hits[0].name, "lamont lane");
        // Nothing sounds like this.
        assert!(sound_alike(&list, "quarry lane", None).is_empty());
        // The street word has to agree.
        assert!(sound_alike(&list, "levont drive", None).is_empty());
    }

    #[test]
    fn candidates_come_back_nearest_the_grid_first() {
        let list = vec![
            Street { name: "hawaii court".into(), lat: 41.0, lon: -85.0, hits: 9 },
            Street { name: "hawagh court".into(), lat: 40.0, lon: -86.0, hits: 1 },
        ];
        let near = Some((40.001, -86.001));
        let hits = sound_alike(&list, "hawari court", near);
        assert_eq!(hits[0].name, "hawagh court", "the one by the grid point wins");
    }

    #[test]
    fn the_street_swaps_and_the_house_number_stays() {
        assert_eq!(street_of("6824 LeVont Lane").as_deref(), Some("levont lane"));
        assert_eq!(street_of("6824 LeVont Lane, Apartment 2").as_deref(), Some("levont lane"));
        assert_eq!(street_of("Campanile Drive and Siebel Drive"), None, "an intersection is not one street");
        assert_eq!(
            with_street("6824 LeVont Lane", "levont lane", "lamont lane"),
            "6824 Lamont Lane"
        );
        assert_eq!(
            with_street("6824 LeVont Lane, Apartment 2", "levont lane", "lamont lane"),
            "6824 Lamont Lane, Apartment 2"
        );
    }
}

/// A dry run over a real library, to see how much of it this places.
/// Ignored by default; point it at a copy:
/// `HS_LIB_DB=… cargo test real_library -- --ignored --nocapture`
#[cfg(test)]
mod real_library {
    use super::*;

    #[test]
    #[ignore]
    fn how_much_of_the_unplaced_can_be_placed() {
        let Ok(path) = std::env::var("HS_LIB_DB") else {
            eprintln!("set HS_LIB_DB");
            return;
        };
        let c = Connection::open(&path).unwrap();
        let cal = calibrate(&c).expect("calibration");
        println!(
            "calibration: {} samples, median {:.0} m, 1000 units = {:.0} m N/S, {:.0} m E/W",
            cal.samples,
            cal.median_m,
            cal.lat_per * 1000.0 * 111_320.0,
            cal.lon_per * 1000.0 * 111_320.0 * 0.77
        );
        let streets = gazetteer(&c);
        println!("gazetteer: {} streets", streets.len());

        let mut q = c
            .prepare(
                "SELECT i.id, i.address, json_extract(ic.extracted, '$.grid')
                   FROM incidents i JOIN incident_calls ic ON ic.incident = i.id
                  WHERE i.lat IS NULL AND i.address <> '' GROUP BY i.id",
            )
            .unwrap();
        let rows: Vec<(i64, String, String)> = q
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                ))
            })
            .unwrap()
            .flatten()
            .collect();
        let (mut gridded, mut renamed) = (0, 0);
        for (_, address, grid) in &rows {
            let home = (39.7684, -86.1581);
        let point = parse_grid(grid).and_then(|g| cal.place_near(g, home, 40.0));
            if point.is_some() {
                gridded += 1;
            }
            if let Some(st) = street_of(address) {
                let hits = sound_alike(&streets, &st, point);
                if let Some(best) = hits.first() {
                    renamed += 1;
                    if renamed <= 12 {
                        println!("  {address}  →  {}", with_street(address, &st, &best.name));
                    }
                }
            }
        }
        println!(
            "unplaced {}: {gridded} land on the grid, {renamed} have a street that sounds like one we know",
            rows.len()
        );
    }
}
