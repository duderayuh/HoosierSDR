//! Placing a run on a highway.
//!
//! "Mile Marker 71, I-70 Westbound" is not an address a geocoder knows, and
//! asked anyway it answers with somewhere else on the interstate or nothing.
//! The dispatcher has said exactly where it is, though, in two ways: the
//! mile marker, and the grid reference read out after it.
//!
//! OpenStreetMap carries a handful of Indiana's mile markers — a few dozen
//! around Indianapolis, most stretches of interstate have none — so a
//! marker that is mapped places the run exactly, and one that is not falls
//! back to the grid reference, put onto the highway it names, on the side of
//! the road the traffic is going. A grid point is a few hundred metres out
//! and lands in a field as often as not; a crew routed there goes to the
//! frontage road.
//!
//! Everything here is pure: the Overpass queries are built here and run in
//! `dispatch`.

use regex::Regex;
use std::sync::OnceLock;

/// Which way the traffic is going.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    North,
    South,
    East,
    West,
}

impl Dir {
    fn heading(self) -> f64 {
        match self {
            Dir::North => 0.0,
            Dir::East => 90.0,
            Dir::South => 180.0,
            Dir::West => 270.0,
        }
    }

    fn word(self) -> &'static str {
        match self {
            Dir::North => "northbound",
            Dir::South => "southbound",
            Dir::East => "eastbound",
            Dir::West => "westbound",
        }
    }
}

/// A place on a highway, as the dispatcher said it.
#[derive(Clone, Debug, PartialEq)]
pub struct Spot {
    /// The route as OpenStreetMap writes its `ref`: "I 70", "US 31", "SR 37".
    pub route: String,
    /// The route as a person writes it: "I-70".
    pub label: String,
    pub miles: Option<f64>,
    pub dir: Option<Dir>,
}

impl Spot {
    /// "I-70 westbound near mile 71".
    pub fn describe(&self) -> String {
        let mut s = self.label.clone();
        if let Some(d) = self.dir {
            s.push(' ');
            s.push_str(d.word());
        }
        if let Some(m) = self.miles {
            s.push_str(&format!(" near mile {}", say_miles(m)));
        }
        s
    }
}

fn say_miles(m: f64) -> String {
    let s = format!("{m:.1}");
    s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
}

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).unwrap())
}

/// Read a highway location out of an address. A route alone is not one — "US
/// 31" is also the name of a street with house numbers on it — so it takes a
/// mile marker or a direction of travel as well.
pub fn parse(s: &str) -> Option<Spot> {
    static ROUTE: OnceLock<Regex> = OnceLock::new();
    static MILE: OnceLock<Regex> = OnceLock::new();
    static DIR: OnceLock<Regex> = OnceLock::new();
    let route = re(
        &ROUTE,
        r"(?i)\b(?:(interstate|i)|(us|u\.s\.)(?:\s+(?:highway|hwy|route))?|(sr|state\s+road|state\s+route|state\s+highway))[\s-]*(\d{1,3})\b",
    );
    let c = route.captures(s)?;
    let n: u32 = c[4].parse().ok()?;
    let (osm, label) = if c.get(1).is_some() {
        (format!("I {n}"), format!("I-{n}"))
    } else if c.get(2).is_some() {
        (format!("US {n}"), format!("US {n}"))
    } else {
        (format!("SR {n}"), format!("SR {n}"))
    };
    let miles = re(&MILE, r"(?i)\b(?:mile\s*markers?|mile\s*posts?|mm|mile)\s*#?\s*(\d{1,3}(?:\.\d{1,2})?)\b")
        .captures(s)
        .and_then(|m| m[1].parse::<f64>().ok());
    let dir = re(&DIR, r"(?i)\b(north|south|east|west)\s*-?\s*bound\b|\b(nb|sb|eb|wb)\b")
        .captures(s)
        .and_then(|d| {
            let w = d.get(1).or(d.get(2))?.as_str().to_ascii_lowercase();
            Some(match &w[..1] {
                "n" => Dir::North,
                "s" => Dir::South,
                "e" => Dir::East,
                _ => Dir::West,
            })
        });
    if miles.is_none() && dir.is_none() {
        return None;
    }
    Some(Spot { route: osm, label, miles, dir })
}

/// A stretch of the route, as OpenStreetMap draws it.
#[derive(Clone, Debug, PartialEq)]
pub struct Way {
    pub points: Vec<(f64, f64)>,
    /// Drawn in the direction of travel. A divided highway is two of these.
    pub oneway: bool,
}

/// A mapped mile marker.
#[derive(Clone, Debug, PartialEq)]
pub struct Marker {
    pub lat: f64,
    pub lon: f64,
    pub miles: f64,
}

/// The route's ways from an Overpass answer (`out geom`).
pub fn ways_of(v: &serde_json::Value) -> Vec<Way> {
    v["elements"]
        .as_array()
        .map(|els| {
            els.iter()
                .filter(|e| e["type"] == "way")
                .filter_map(|e| {
                    let points: Vec<(f64, f64)> = e["geometry"]
                        .as_array()?
                        .iter()
                        .filter_map(|p| Some((p["lat"].as_f64()?, p["lon"].as_f64()?)))
                        .collect();
                    let t = &e["tags"];
                    let oneway = t["oneway"] == "yes" || (t["highway"] == "motorway" && t["oneway"] != "no");
                    (points.len() >= 2).then_some(Way { points, oneway })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The mile markers in an Overpass answer. Mapped with the number in
/// `distance` ("71 mi", "71", "71.0 mi") and now and then only in `ref`
/// ("North I-465 Mile 24.7"); a marker in kilometres is not a mile marker.
pub fn markers_of(v: &serde_json::Value) -> Vec<Marker> {
    static REF: OnceLock<Regex> = OnceLock::new();
    let from_ref = re(&REF, r"(?i)\bmile\s*(\d{1,3}(?:\.\d{1,2})?)\b");
    v["elements"]
        .as_array()
        .map(|els| {
            els.iter()
                .filter(|e| e["type"] == "node")
                .filter_map(|e| {
                    let t = &e["tags"];
                    let miles = match t["distance"].as_str() {
                        Some(d) => {
                            let mut parts = d.split_whitespace();
                            let n: f64 = parts.next()?.parse().ok()?;
                            match parts.next() {
                                None | Some("mi") | Some("mile") | Some("miles") => n,
                                _ => return None,
                            }
                        }
                        None => from_ref.captures(t["ref"].as_str()?)?[1].parse().ok()?,
                    };
                    Some(Marker { lat: e["lat"].as_f64()?, lon: e["lon"].as_f64()? , miles })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Overpass regex for a route `ref`, which lists every route sharing the
/// road ("I 465;I 69"). No backslashes: Overpass's regex engine is POSIX.
fn ref_regex(route: &str) -> String {
    format!("(^|;){route}(;|$)")
}

const ROADS: &str = "^(motorway|trunk|primary|secondary)$";

/// The query for a mapped mile marker: every marker in the box — there are a
/// few dozen — and the route wherever it passes one. Asked the other way
/// round (the route's markers), Overpass takes a minute and more.
pub fn marker_query(route: &str, bbox: &str) -> String {
    let r = ref_regex(route);
    format!(
        "[out:json][timeout:25];node[\"highway\"=\"milestone\"]({bbox})->.m;.m out;way[\"highway\"~\"{ROADS}\"][\"ref\"~\"{r}\"](around.m:{}); out geom;",
        MARKER_M * 2
    )
}

/// The query for the route around a point: a box, which Overpass answers in
/// a second or two, where `around` with the ref regex took twenty.
pub fn route_query(route: &str, lat: f64, lon: f64) -> String {
    let r = ref_regex(route);
    let dlat = SNAP_M as f64 / 110_540.0;
    let dlon = SNAP_M as f64 / (111_320.0 * lat.to_radians().cos().abs().max(0.05));
    format!(
        "[out:json][timeout:25];way[\"highway\"~\"{ROADS}\"][\"ref\"~\"{r}\"]({:.5},{:.5},{:.5},{:.5});out geom;",
        lat - dlat,
        lon - dlon,
        lat + dlat,
        lon + dlon
    )
}

/// How far a mile marker may stand from the road it marks.
const MARKER_M: u32 = 40;
/// How far from a grid point the highway it names may be. The grid is good
/// to about half a kilometre; further than this is another stretch of road.
pub const SNAP_M: u32 = 2_500;

/// Metres east and north of `origin`, near enough flat at this scale.
fn local(origin: (f64, f64), p: (f64, f64)) -> (f64, f64) {
    let kx = origin.0.to_radians().cos() * 111_320.0;
    ((p.1 - origin.1) * kx, (p.0 - origin.0) * 110_540.0)
}

fn unlocal(origin: (f64, f64), xy: (f64, f64)) -> (f64, f64) {
    let kx = origin.0.to_radians().cos() * 111_320.0;
    (origin.0 + xy.1 / 110_540.0, origin.1 + xy.0 / kx)
}

/// The nearest point to `p` on each segment: (point, metres away, heading of
/// travel along it in degrees, whether the way is one-way).
fn projections(p: (f64, f64), ways: &[Way]) -> Vec<((f64, f64), f64, f64, bool)> {
    let mut out = Vec::new();
    for w in ways {
        for s in w.points.windows(2) {
            let (a, b) = (local(p, s[0]), local(p, s[1]));
            let (dx, dy) = (b.0 - a.0, b.1 - a.1);
            let len2 = dx * dx + dy * dy;
            let t = if len2 == 0.0 { 0.0 } else { (-(a.0 * dx + a.1 * dy) / len2).clamp(0.0, 1.0) };
            let q = (a.0 + t * dx, a.1 + t * dy);
            let heading = dx.atan2(dy).to_degrees().rem_euclid(360.0);
            out.push((unlocal(p, q), q.0.hypot(q.1), heading, w.oneway));
        }
    }
    out
}

fn turn(a: f64, b: f64) -> f64 {
    let d = (a - b).rem_euclid(360.0);
    d.min(360.0 - d)
}

/// Put `p` on the route: on the carriageway going `dir` when there is one
/// near, else on the nearest part of the route. `None` when the route is
/// nowhere within `max_m`.
pub fn snap(p: (f64, f64), ways: &[Way], dir: Option<Dir>, max_m: f64) -> Option<(f64, f64)> {
    let all: Vec<_> = projections(p, ways).into_iter().filter(|x| x.1 <= max_m).collect();
    let nearest = all.iter().min_by(|a, b| a.1.total_cmp(&b.1))?;
    let going = |x: &&((f64, f64), f64, f64, bool)| match dir {
        None => true,
        // A two-way road carries both directions on the one line.
        Some(_) if !x.3 => true,
        // Within sixty degrees: an interstate running north-west is signed
        // westbound as often as northbound.
        Some(d) => turn(x.2, d.heading()) <= 60.0,
    };
    // The other carriageway is tens of metres away; a matching one much
    // further off than the nearest road is a different road.
    let best = all
        .iter()
        .filter(going)
        .filter(|x| x.1 <= nearest.1 + 150.0)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .unwrap_or(nearest);
    Some(best.0)
}

/// The mapped marker for this mile, on the route and the carriageway the
/// dispatcher named.
pub fn at_marker(spot: &Spot, markers: &[Marker], ways: &[Way]) -> Option<(f64, f64)> {
    let miles = spot.miles?;
    markers
        .iter()
        .filter(|m| (m.miles - miles).abs() < 0.05)
        .filter_map(|m| snap((m.lat, m.lon), ways, spot.dir, MARKER_M as f64 * 2.0))
        .next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_highway_location_is_read_however_it_was_said() {
        let s = parse("Mile Marker 71, I-70 Westbound").unwrap();
        assert_eq!((s.route.as_str(), s.label.as_str(), s.miles, s.dir), ("I 70", "I-70", Some(71.0), Some(Dir::West)));
        let s = parse("I-70 Westbound Mile Marker 71").unwrap();
        assert_eq!((s.route.as_str(), s.miles, s.dir), ("I 70", Some(71.0), Some(Dir::West)));
        let s = parse("Mile Marker 47.5 I-465 Northbound").unwrap();
        assert_eq!((s.route.as_str(), s.miles, s.dir), ("I 465", Some(47.5), Some(Dir::North)));
        assert_eq!(parse("interstate 65 south bound mm 112").unwrap().route, "I 65");
        assert_eq!(parse("I65 SB mile 112").unwrap().dir, Some(Dir::South));
        assert_eq!(parse("State Road 37 northbound at mile marker 120").unwrap().route, "SR 37");
        assert_eq!(parse("US Highway 31 mile marker 140").unwrap().route, "US 31");
        assert_eq!(parse("Mile Marker 71, I-70 Westbound").unwrap().describe(), "I-70 westbound near mile 71");
        assert_eq!(parse("I-465 mile 47.5").unwrap().describe(), "I-465 near mile 47.5");
    }

    #[test]
    fn a_street_is_not_a_highway_location() {
        assert_eq!(parse("8241 East 41st Street"), None);
        // A road named for a route, with house numbers on it.
        assert_eq!(parse("4501 US 31 South"), None);
        assert_eq!(parse("North Delaware Street and East 32nd Street"), None);
        assert_eq!(parse("Mile Marker 71"), None);
        assert_eq!(parse("1200 Unit Drive, Building I 4"), None);
    }

    fn motorway(tags: serde_json::Value, pts: &[(f64, f64)]) -> serde_json::Value {
        serde_json::json!({ "type": "way", "tags": tags, "geometry": pts.iter().map(|(a, b)| serde_json::json!({ "lat": a, "lon": b })).collect::<Vec<_>>() })
    }

    /// A divided highway running east–west, the westbound carriageway drawn
    /// 40 m north of the eastbound one.
    fn divided() -> Vec<Way> {
        let v = serde_json::json!({ "elements": [
            motorway(serde_json::json!({ "highway": "motorway", "ref": "I 70" }), &[(39.70036, -86.25), (39.70036, -86.35)]),
            motorway(serde_json::json!({ "highway": "motorway", "ref": "I 70" }), &[(39.70, -86.35), (39.70, -86.25)]),
        ]});
        ways_of(&v)
    }

    #[test]
    fn a_grid_point_goes_onto_the_carriageway_the_traffic_is_on() {
        let w = divided();
        assert!(w.iter().all(|x| x.oneway), "a motorway is one-way unless it says otherwise");
        // Two hundred metres south of the road.
        let p = (39.6982, -86.29);
        let west = snap(p, &w, Some(Dir::West), 2500.0).unwrap();
        assert!((west.0 - 39.70036).abs() < 1e-5 && (west.1 + 86.29).abs() < 1e-5, "{west:?}");
        let east = snap(p, &w, Some(Dir::East), 2500.0).unwrap();
        assert!((east.0 - 39.70).abs() < 1e-5, "{east:?}");
        // No direction said: the nearest.
        assert!((snap(p, &w, None, 2500.0).unwrap().0 - 39.70).abs() < 1e-5);
        // Signed northbound on an east–west stretch: nothing matches, so the nearest.
        assert!((snap(p, &w, Some(Dir::North), 2500.0).unwrap().0 - 39.70).abs() < 1e-5);
        // Too far from the road to be on it.
        assert_eq!(snap((39.75, -86.29), &w, Some(Dir::West), 2500.0), None);
    }

    #[test]
    fn a_mapped_mile_marker_places_the_run_exactly() {
        let v = serde_json::json!({ "elements": [
            { "type": "node", "lat": 39.70018, "lon": -86.3277, "tags": { "highway": "milestone", "distance": "69 mi" } },
            { "type": "node", "lat": 39.70018, "lon": -86.2700, "tags": { "highway": "milestone", "distance": "71" } },
            { "type": "node", "lat": 39.70018, "lon": -86.2600, "tags": { "highway": "milestone", "distance": "114 km" } },
            { "type": "node", "lat": 39.70018, "lon": -86.2500, "tags": { "highway": "milestone", "ref": "North I-465 Mile 24.7" } },
        ]});
        let m = markers_of(&v);
        assert_eq!(m.iter().map(|x| x.miles).collect::<Vec<_>>(), vec![69.0, 71.0, 24.7]);
        let spot = parse("Mile Marker 71, I-70 Westbound").unwrap();
        let hit = at_marker(&spot, &m, &divided()).unwrap();
        assert!((hit.1 + 86.27).abs() < 1e-5 && (hit.0 - 39.70036).abs() < 1e-5, "{hit:?}");
        assert_eq!(at_marker(&parse("I-70 westbound mile marker 72").unwrap(), &m, &divided()), None);
        assert_eq!(at_marker(&parse("I-70 westbound").unwrap(), &m, &divided()), None);
    }

    #[test]
    fn the_queries_keep_to_the_route() {
        let q = marker_query("I 465", "39.4,-86.6,40.1,-85.7");
        assert!(q.contains("[\"ref\"~\"(^|;)I 465(;|$)\"]"), "{q}");
        assert!(!q.contains('\\'), "Overpass regexes are POSIX: {q}");
        let q = route_query("I 70", 39.7, -86.29);
        assert!(q.contains("(39.67738,-86.31919,39.72262,-86.26081)"), "{q}");
    }
}
