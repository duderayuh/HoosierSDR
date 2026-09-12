//! The place book: the hospitals, stations and landmarks on this system.
//!
//! A scanner hears "Medic 41 to Community East" and a listener who knows the
//! county hears a destination, a drive time and — for a cardiac arrest — a
//! reason. The app has known none of that: talkgroups were numbers, and
//! hospitals were a word in a prompt.
//!
//! A place is a name, a point on the map, what it can do (a cath lab, a
//! stroke team) and which talkgroups belong to it. That last part is what
//! makes it more than a pin: the hospital channels on a trunked system are
//! already named after their hospitals in the listener's own catalog, so the
//! book can offer to fill itself in and the listener only has to say yes.
//!
//! Nothing here knows anything about any particular city. What a hospital
//! can do is never guessed — a feature is ticked by the listener or it is
//! not true.

use crate::AppState;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, State};

/// Somewhere on the map worth naming.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Place {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// `hospital` | `station` | `landmark` | `other`.
    #[serde(default = "other")]
    pub kind: String,
    #[serde(default)]
    pub lat: Option<f64>,
    #[serde(default)]
    pub lon: Option<f64>,
    /// The address used to place it, kept so it can be geocoded again.
    #[serde(default)]
    pub address: String,
    /// What it can do: `stemi`, `stroke`, `trauma`, `ecmo`, `peds`, `burn`
    /// — or anything else the listener types.
    #[serde(default)]
    pub features: Vec<String>,
    /// The talkgroups that belong to it, so traffic on them is about here.
    #[serde(default)]
    pub tgs: Vec<u16>,
    /// Which system those talkgroups are on. Two systems in range can use
    /// the same numbers, so a place without this claims a number, not a
    /// channel. Blank means any system.
    #[serde(default)]
    pub system: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn other() -> String {
    "other".into()
}
fn yes() -> bool {
    true
}

/// The features the app offers as tick-boxes. A listener can type others;
/// these are only the ones with a label ready.
pub const KNOWN_FEATURES: &[(&str, &str)] = &[
    ("stemi", "Cath lab · STEMI"),
    ("stroke", "Stroke centre"),
    ("trauma", "Trauma centre"),
    ("ecmo", "ECMO"),
    ("peds", "Children's"),
    ("burn", "Burn unit"),
    ("ob", "Obstetrics"),
    ("psych", "Psychiatric"),
];

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub places: Vec<Place>,
}

#[derive(Default)]
pub struct PlaceState {
    pub settings: Settings,
}

pub type Shared = Mutex<PlaceState>;

fn path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let d = app.path().app_config_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    Ok(d.join("places.json"))
}

pub fn load(app: &AppHandle) -> PlaceState {
    let settings = path(app)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<Settings>(&s).ok())
        .map(|mut s| {
            sanitize(&mut s);
            s
        })
        .unwrap_or_default();
    PlaceState { settings }
}

fn store(app: &AppHandle, s: &Settings) -> Result<(), String> {
    let p = path(app)?;
    let text = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    std::fs::write(p, text).map_err(|e| e.to_string())
}

const MAX_PLACES: usize = 500;

pub fn sanitize(s: &mut Settings) {
    s.places.truncate(MAX_PLACES);
    let mut seen = std::collections::HashSet::new();
    s.places.retain_mut(|p| {
        p.name = crate::analyzers::clean_line(&p.name, 120);
        p.address = crate::analyzers::clean_line(&p.address, 200);
        p.notes = crate::analyzers::clean_line(&p.notes, 400);
        p.system = crate::analyzers::clean_line(&p.system, 80);
        if p.id.trim().is_empty() {
            p.id = new_id(&p.name);
        }
        if !matches!(p.kind.as_str(), "hospital" | "station" | "landmark") {
            p.kind = other();
        }
        p.features = p
            .features
            .iter()
            .map(|f| f.trim().to_ascii_lowercase())
            .filter(|f| !f.is_empty() && f.len() <= 24)
            .take(12)
            .collect();
        p.features.sort();
        p.features.dedup();
        p.tgs.sort_unstable();
        p.tgs.dedup();
        p.tgs.truncate(32);
        if p.lat.is_some_and(|v| !(-90.0..=90.0).contains(&v))
            || p.lon.is_some_and(|v| !(-180.0..=180.0).contains(&v))
        {
            p.lat = None;
            p.lon = None;
        }
        !p.name.is_empty() && seen.insert(p.id.clone())
    });
}

fn new_id(name: &str) -> String {
    let slug: String = name
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let slug = slug.trim_matches('-').replace("--", "-");
    format!(
        "p{}-{}",
        crate::library::now(),
        if slug.is_empty() {
            "place".into()
        } else {
            slug.chars().take(24).collect::<String>()
        }
    )
}

// ---------------------------------------------------------------------------
// what the book is for
// ---------------------------------------------------------------------------

/// The place a talkgroup on `system` belongs to, if any.
pub fn for_tg<'a>(s: &'a Settings, tg: u16, system: &str) -> Option<&'a Place> {
    s.places.iter().find(|p| {
        p.enabled
            && p.tgs.contains(&tg)
            && (p.system.is_empty() || system.is_empty() || p.system == system)
    })
}

/// Every placed place that can do `feature`, nearest to `from` first.
pub fn nearest_with<'a>(
    s: &'a Settings,
    feature: &str,
    from: (f64, f64),
) -> Vec<(&'a Place, f64)> {
    let feature = feature.trim().to_ascii_lowercase();
    let mut out: Vec<(&Place, f64)> = s
        .places
        .iter()
        .filter(|p| p.enabled && (feature.is_empty() || p.features.iter().any(|f| *f == feature)))
        .filter_map(|p| {
            let (lat, lon) = (p.lat?, p.lon?);
            Some((p, crate::dispatch::haversine_m(from.0, from.1, lat, lon)))
        })
        .collect();
    out.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    out
}

// ---------------------------------------------------------------------------
// seeding from the listener's own catalog
// ---------------------------------------------------------------------------

/// A place the app thinks it can see in the talkgroup catalog, offered for
/// the listener to accept, edit or ignore.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Suggestion {
    pub name: String,
    pub kind: String,
    pub tg: u16,
    pub tg_name: String,
    pub system: String,
    /// The catalog line it was read from, so the listener can judge it.
    pub from: String,
}

/// Pull a hospital name out of a channel description.
///
/// Hospital channels on a trunked system are named after where they ring:
/// "Med 06 - Example General ER", "MED 12 — Example Children's Hospital ER".
/// The name is what sits between the channel number and the ER suffix. A
/// spare or unassigned channel names nowhere.
pub fn hospital_from_description(desc: &str) -> Option<String> {
    let d = desc.trim();
    let low = d.to_ascii_lowercase();
    if !low.starts_with("med ") && !low.starts_with("med-") {
        return None;
    }
    // Everything after the first dash that follows the channel number.
    let rest = d
        .split_once(['-', '–', '—'])
        .map(|(_, r)| r)
        .unwrap_or("")
        .trim();
    if rest.is_empty() {
        return None;
    }
    // "(Also on the statewide system)" and the like are notes, not the name.
    let rest = rest.split('(').next().unwrap_or(rest).trim();
    let low = rest.to_ascii_lowercase();
    if low.starts_with("spare") || low.contains("unassigned") || low.contains("not used") {
        return None;
    }
    // Trim a trailing "ER"/"Hospital ER", which every one of them has.
    let name = rest
        .trim_end_matches(|c: char| c == '.' || c.is_whitespace())
        .strip_suffix("ER")
        .unwrap_or(rest)
        .trim_end_matches([',', '-', ' '])
        .trim();
    (name.len() >= 3).then(|| name.to_string())
}

/// Read the catalog for places worth adding, skipping talkgroups the book
/// already claims.
///
/// One catalog per system, never the merged one: two systems in range
/// commonly use the same talkgroup numbers, and the merged view would hand
/// back whichever description won.
pub fn suggest(app: &AppHandle, only: Option<u32>) -> Vec<Suggestion> {
    let state = app.state::<AppState>();
    let known: std::collections::HashSet<(u16, String)> = {
        let st = state.places.lock().unwrap();
        st.settings
            .places
            .iter()
            .flat_map(|p| p.tgs.iter().map(|t| (*t, p.system.clone())))
            .collect()
    };
    let systems = crate::playlists::sids_by_system_name(app);
    let mut out = Vec::new();
    for (system, sid) in systems {
        if only.is_some_and(|s| s != sid) {
            continue;
        }
        let rows = {
            let cat = state.catalog.lock().unwrap();
            cat.talkgroups(Some(sid))
        };
        for t in rows {
            if known.contains(&(t.id, system.clone())) || known.contains(&(t.id, String::new())) {
                continue;
            }
            let desc = t.description.clone().unwrap_or_default();
            let Some(name) = hospital_from_description(&desc) else {
                continue;
            };
            out.push(Suggestion {
                name,
                kind: "hospital".into(),
                tg: t.id,
                tg_name: t.alias.clone().unwrap_or_else(|| format!("TG {}", t.id)),
                system: system.clone(),
                from: desc,
            });
        }
    }
    out.sort_by(|a, b| a.system.cmp(&b.system).then(a.tg.cmp(&b.tg)));
    out
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn places_get(state: State<AppState>) -> Settings {
    state.places.lock().unwrap().settings.clone()
}

#[tauri::command]
pub fn places_set(
    app: AppHandle,
    state: State<AppState>,
    settings: Settings,
) -> Result<Settings, String> {
    let mut s = settings;
    sanitize(&mut s);
    {
        let mut st = state.places.lock().unwrap();
        st.settings = s.clone();
    }
    store(&app, &s)?;
    let _ = app.emit("places", ());
    Ok(s)
}

/// Places the catalog suggests, for the "add the hospitals" button.
#[tauri::command]
pub fn places_suggest(app: AppHandle, sid: Option<u32>) -> Vec<Suggestion> {
    suggest(&app, sid)
}

/// The feature tick-boxes the editor offers.
#[tauri::command]
pub fn place_features() -> Vec<(String, String)> {
    KNOWN_FEATURES
        .iter()
        .map(|(k, l)| (k.to_string(), l.to_string()))
        .collect()
}

/// Put a place on the map from its name or address, using the same geocoder
/// (and the same cache and throttle) the dispatch map uses.
#[tauri::command]
pub async fn place_locate(
    state: State<'_, AppState>,
    query: String,
) -> Result<Option<(f64, f64, String)>, String> {
    crate::dispatch::dispatch_geocode(state, query).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(name: &str, feats: &[&str], lat: f64, lon: f64) -> Place {
        Place {
            id: name.to_lowercase(),
            name: name.into(),
            kind: "hospital".into(),
            lat: Some(lat),
            lon: Some(lon),
            features: feats.iter().map(|s| s.to_string()).collect(),
            enabled: true,
            ..Default::default()
        }
    }

    #[test]
    fn a_hospital_channel_names_its_hospital() {
        assert_eq!(
            hospital_from_description("Med 03 - Example General ER").as_deref(),
            Some("Example General")
        );
        assert_eq!(
            hospital_from_description("Med 12 - Example Children's Hospital ER").as_deref(),
            Some("Example Children's Hospital")
        );
        // A note in brackets is not part of the name.
        assert_eq!(
            hospital_from_description("Med 18 - Example Regional Hospital ER  (Also on 32M-XYZ)")
                .as_deref(),
            Some("Example Regional Hospital")
        );
        // Spares name nowhere.
        assert_eq!(hospital_from_description("Med 20 - Spare"), None);
        assert_eq!(
            hospital_from_description("Med 10 - Spare (formerly Example Westview Hospital ER)"),
            None
        );
        // Everything else on the system is not a hospital channel.
        assert_eq!(hospital_from_description("Fire: Dispatch (Regional)"), None);
        assert_eq!(hospital_from_description("Medic 3 patch"), None);
    }

    #[test]
    fn the_nearest_place_that_can_do_the_thing() {
        let s = Settings {
            places: vec![
                place("Near General", &["stroke"], 40.00, -86.00),
                place("Far Heart", &["stemi", "ecmo"], 40.50, -86.00),
                place("Near Clinic", &[], 40.001, -86.001),
                {
                    let mut p = place("Closed Heart", &["stemi"], 40.001, -86.0);
                    p.enabled = false;
                    p
                },
            ],
        };
        let from = (40.0, -86.0);
        let stemi = nearest_with(&s, "stemi", from);
        assert_eq!(stemi.len(), 1, "the switched-off one does not count");
        assert_eq!(stemi[0].0.name, "Far Heart");
        // No feature asked for: everything placed, nearest first.
        let any = nearest_with(&s, "", from);
        assert_eq!(any[0].0.name, "Near General");
        assert_eq!(any.len(), 3);
        // Nothing can do this.
        assert!(nearest_with(&s, "burn", from).is_empty());
    }

    #[test]
    fn a_talkgroup_belongs_to_one_place() {
        let mut s = Settings {
            places: vec![place("Example General", &[], 40.0, -86.0)],
        };
        s.places[0].tgs = vec![10257];
        s.places[0].system = "Example System".into();
        assert_eq!(for_tg(&s, 10257, "Example System").unwrap().name, "Example General");
        assert!(for_tg(&s, 10258, "Example System").is_none());
        // The same number on the other system in range is not this place.
        assert!(for_tg(&s, 10257, "Other System").is_none());
        // A place that names no system claims the number anywhere.
        s.places[0].system = String::new();
        assert!(for_tg(&s, 10257, "Other System").is_some());
        s.places[0].enabled = false;
        assert!(for_tg(&s, 10257, "Example System").is_none(), "a switched-off place claims nothing");
    }

    #[test]
    fn sanitising_fills_in_ids_and_throws_out_nonsense() {
        let mut s = Settings {
            places: vec![
                Place {
                    name: "  Example General  ".into(),
                    kind: "clinic".into(),
                    features: vec!["  STEMI ".into(), "stemi".into(), String::new()],
                    tgs: vec![7, 7, 3],
                    lat: Some(999.0),
                    lon: Some(-86.0),
                    ..Default::default()
                },
                Place::default(),
            ],
        };
        sanitize(&mut s);
        assert_eq!(s.places.len(), 1, "a nameless place is not a place");
        let p = &s.places[0];
        assert_eq!(p.name, "Example General");
        assert_eq!(p.kind, "other", "an unknown kind falls back");
        assert_eq!(p.features, vec!["stemi"]);
        assert_eq!(p.tgs, vec![3, 7]);
        assert_eq!(p.lat, None, "an impossible point is no point");
        assert!(p.id.starts_with('p'));
    }
}
