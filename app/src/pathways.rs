//! Care pathways: what a dispatch run needs, and where the nearest one is.
//!
//! A pathway is one "if this, then that": *if* the run looks like a cardiac
//! arrest, *then* the useful facts are the closest hospital and the closest
//! ECMO centre. One ordered list of them, **first match wins**, so
//! specificity is expressed by position — a pediatric arrest sits above a
//! working arrest, which sits above a plain cardiac arrest — the same way
//! retention rules already work, and orderable by the same drag.
//!
//! Matching reads the text, not just the dispatch type. That is not a
//! preference: on a real library, *"Working pediatric cardiac arrest at
//! 4350 Madison Avenue"* was filed by dispatch as **Unconscious**, and the
//! words "working arrest" and "CPR in progress" appear only in the
//! transcripts, never in the summary. A pathway that keyed off the call
//! type alone would miss the calls that matter most.
//!
//! What comes out is a list of [`Target`]s — a label, a place, and how far
//! away it is by road — stored on the incident so the map, the popup and a
//! Telegram message all read the same numbers instead of working them out
//! three times and disagreeing.

use serde::{Deserialize, Serialize};

pub const MAX_PATHWAYS: usize = 60;
pub const MAX_NEEDS: usize = 6;
/// Straight-line candidates routed per need. The nearest by crow flight is
/// not always the nearest by road — a river or a motorway junction can
/// reverse two of them — but past three it is spending seconds of routing
/// to reorder places nobody would drive to.
pub const CANDIDATES: usize = 3;

/// One facility a run needs: a label to show, and the features a place must
/// have to qualify. All of them, not any — `["ecmo", "peds"]` means a
/// pediatric ECMO centre, not "either".
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Need {
    pub label: String,
    #[serde(default)]
    pub features: Vec<String>,
}

/// What makes a run match.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct When {
    /// Dispatch call types, matched whole and case-insensitively.
    #[serde(default)]
    pub call_types: Vec<String>,
    /// Words anywhere in the type, the summary or the transcripts.
    #[serde(default)]
    pub phrases: Vec<String>,
    /// Words that rule it out, checked first — read only against what the
    /// run is, never the transcripts.
    #[serde(default)]
    pub except: Vec<String>,
    /// Dispatch types that rule it out. Precise where a word is not: a
    /// "medical alarm" is a medical run, so excluding runs by the word
    /// "alarm" refused six of them on a real library, while excluding the
    /// Fire Alarm and Residence Alarm *types* refuses exactly the runs
    /// nobody is taken to hospital from.
    #[serde(default)]
    pub except_types: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Pathway {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub when: When,
    pub needs: Vec<Need>,
}

impl Default for Pathway {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            enabled: true,
            when: When::default(),
            needs: Vec::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub enabled: bool,
    /// In order. The first one that matches a run is the one used.
    pub pathways: Vec<Pathway>,
    /// Set once the starting set has been offered, so it is not offered
    /// again after the listener has deleted the ones they did not want.
    pub seeded: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            pathways: Vec::new(),
            seeded: false,
        }
    }
}

/// One resolved facility for one run.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Target {
    /// The need's label — "closest hospital", "ECMO centre".
    pub label: String,
    pub place_id: String,
    pub place_name: String,
    pub lat: f64,
    pub lon: f64,
    pub meters: f64,
    /// Drive time in seconds; 0 when only the straight line is known.
    pub secs: f64,
    /// `road` — a real route; `straight` — as the crow flies.
    pub how: String,
}

impl Target {
    pub fn miles(&self) -> f64 {
        self.meters / 1609.344
    }
    pub fn mins(&self) -> f64 {
        self.secs / 60.0
    }
    /// One target in words. Shared by the map popup, the incident detail
    /// and the Telegram message, so all three say the same thing — and so
    /// a straight line is never dressed up as a drive time.
    pub fn say(&self) -> String {
        if self.how == "road" && self.secs > 0.0 {
            format!(
                "{} — {:.1} mi, {:.0} min by road",
                self.place_name,
                self.miles(),
                self.mins().max(1.0)
            )
        } else {
            format!(
                "{} — {:.1} mi direct (straight line, no road route)",
                self.place_name,
                self.miles()
            )
        }
    }
}

/// Every target in words, one per line, labelled.
pub fn say_all(targets: &[Target]) -> String {
    targets
        .iter()
        .map(|t| format!("{}: {}", t.label, t.say()))
        .collect::<Vec<_>>()
        .join("\n")
}

// --------------------------------------------------------------- matching

/// All the words a run can be matched against: what dispatch called it,
/// what the model made of it, and what was actually said on the radio.
pub fn haystack(call_type: &str, summary: &str, transcripts: &[String]) -> String {
    let mut s = String::with_capacity(summary.len() + 200);
    s.push_str(call_type);
    s.push(' ');
    s.push_str(summary);
    for t in transcripts {
        s.push(' ');
        s.push_str(t);
    }
    s
}

fn same_type(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

/// One run, in the two forms matching needs it.
#[derive(Clone, Copy, Debug)]
pub struct Run<'a> {
    pub call_type: &'a str,
    /// What the run *is*: the dispatch type and the summary of it.
    pub about: &'a str,
    /// Everything, transcripts included.
    pub text: &'a str,
}

impl<'a> Run<'a> {
    pub fn new(call_type: &'a str, about: &'a str, text: &'a str) -> Self {
        Self { call_type, about, text }
    }
}

/// Does this run match this pathway?
///
/// Inclusion and exclusion deliberately read different things. A phrase
/// searches everything, transcripts included, because "working arrest" and
/// "CPR in progress" are said on the radio and appear nowhere else. An
/// exception reads only what the run *is* — its type and summary — because
/// a word someone happened to say should not disqualify a run.
///
/// That asymmetry was not a guess. Searching exceptions across transcripts
/// too, on a real library, dropped six Sick Person runs and an Unconscious
/// one: each had "alarm" or "investigation" somewhere in the radio traffic,
/// so the catch-all refused them and they got no hospital at all.
pub fn matches(p: &Pathway, r: Run) -> bool {
    if !p.enabled {
        return false;
    }
    if p
        .when
        .except_types
        .iter()
        .any(|t| same_type(t, r.call_type))
    {
        return false;
    }
    if !p.when.except.is_empty()
        && !crate::alerts::matched_keywords(&p.when.except, r.about).is_empty()
    {
        return false;
    }
    let call_type = r.call_type;
    let text = r.text;
    let by_type = p
        .when
        .call_types
        .iter()
        .any(|t| same_type(t, call_type));
    let by_words = !p.when.phrases.is_empty()
        && !crate::alerts::matched_keywords(&p.when.phrases, text).is_empty();
    // A pathway with neither a type nor a phrase is the catch-all: it takes
    // whatever reaches it, which is why order matters and why it belongs at
    // the bottom of the list.
    let catch_all = p.when.call_types.is_empty() && p.when.phrases.is_empty();
    by_type || by_words || catch_all
}

/// The first pathway that matches — the whole point of the ordering.
pub fn first_match<'a>(s: &'a Settings, r: Run) -> Option<&'a Pathway> {
    if !s.enabled {
        return None;
    }
    s.pathways.iter().find(|p| matches(p, r))
}

// -------------------------------------------------------------- resolving

/// Hospitals that can do all of `features`, nearest by crow flight first.
///
/// Unlike [`crate::places::nearest_with`] this insists on `kind ==
/// "hospital"` and on *every* feature: a fire station someone tagged
/// `ecmo` is not somewhere to take a cardiac arrest, and a pediatric ECMO
/// centre has to be both.
pub fn hospitals_with<'a>(
    s: &'a crate::places::Settings,
    features: &[String],
    from: (f64, f64),
) -> Vec<(&'a crate::places::Place, f64)> {
    let want: Vec<String> = features
        .iter()
        .map(|f| f.trim().to_lowercase())
        .filter(|f| !f.is_empty())
        .collect();
    let mut out: Vec<(&crate::places::Place, f64)> = s
        .places
        .iter()
        .filter(|p| p.enabled && p.kind == "hospital")
        .filter(|p| {
            want.iter().all(|w| {
                p.features
                    .iter()
                    .any(|f| f.trim().to_lowercase() == *w)
            })
        })
        .filter_map(|p| {
            let (lat, lon) = (p.lat?, p.lon?);
            Some((p, crate::dispatch::haversine_m(from.0, from.1, lat, lon)))
        })
        .collect();
    out.sort_by(|a, b| a.1.total_cmp(&b.1));
    out
}

/// Turn one need into a target: the nearest qualifying hospital by road.
///
/// The three nearest by crow flight are routed and the quickest wins —
/// which is not always the nearest, since a river or a one-way system can
/// swap two of them. With no router, the crow-flight winner is returned
/// and says so.
pub fn resolve(
    need: &Need,
    places: &crate::places::Settings,
    from: (f64, f64),
    route: &mut dyn FnMut((f64, f64), (f64, f64)) -> crate::routing::Distance,
) -> Option<Target> {
    let near = hospitals_with(places, &need.features, from);
    if near.is_empty() {
        return None;
    }
    let mut best: Option<Target> = None;
    for (p, straight_m) in near.iter().take(CANDIDATES) {
        let (lat, lon) = (p.lat?, p.lon?);
        let d = route(from, (lat, lon));
        let t = Target {
            label: need.label.clone(),
            place_id: p.id.clone(),
            place_name: p.name.clone(),
            lat,
            lon,
            meters: if d.meters > 0.0 { d.meters } else { *straight_m },
            secs: d.secs,
            how: d.how.to_string(),
        };
        // By time when there are times to compare, by distance otherwise.
        let better = match &best {
            None => true,
            Some(b) => {
                if t.secs > 0.0 && b.secs > 0.0 {
                    t.secs < b.secs
                } else {
                    t.meters < b.meters
                }
            }
        };
        if better {
            best = Some(t);
        }
        // Without a router every candidate is the same straight line that
        // was already sorted, so there is nothing to learn from the rest.
        if d.how == "straight" {
            break;
        }
    }
    best
}

/// Everything one run needs, in the order the pathway lists them.
pub fn resolve_all(
    p: &Pathway,
    places: &crate::places::Settings,
    from: (f64, f64),
    route: &mut dyn FnMut((f64, f64), (f64, f64)) -> crate::routing::Distance,
) -> Vec<Target> {
    p.needs
        .iter()
        .take(MAX_NEEDS)
        .filter_map(|n| resolve(n, places, from, route))
        .collect()
}

// ------------------------------------------------------------- the starter set

/// A starting set, in specificity order. Deliberately generic: conditions
/// and facility types, no hospital names and nothing local — the places
/// themselves are the listener's own.
pub fn starter() -> Vec<Pathway> {
    let p = |id: &str, name: &str, types: &[&str], words: &[&str], not_types: &[&str], needs: Vec<Need>| Pathway {
        id: id.into(),
        name: name.into(),
        enabled: true,
        when: When {
            call_types: types.iter().map(|s| s.to_string()).collect(),
            phrases: words.iter().map(|s| s.to_string()).collect(),
            except: Vec::new(),
            except_types: not_types.iter().map(|s| s.to_string()).collect(),
        },
        needs,
    };
    let need = |label: &str, features: &[&str]| Need {
        label: label.into(),
        features: features.iter().map(|s| s.to_string()).collect(),
    };
    vec![
        p(
            "pw-peds-arrest",
            "Pediatric cardiac arrest",
            &[],
            &["pediatric cardiac arrest", "pediatric arrest", "infant arrest", "child in arrest"],
            &[],
            vec![
                need("Closest pediatric hospital", &["peds"]),
                need("Pediatric ECMO", &["peds", "ecmo"]),
                need("Closest hospital", &[]),
            ],
        ),
        p(
            "pw-arrest",
            "Cardiac arrest",
            &["Cardiac Arrest"],
            &["cardiac arrest", "working arrest", "cpr in progress", "full arrest"],
            &[],
            vec![
                need("Closest hospital", &[]),
                need("ECMO centre", &["ecmo"]),
            ],
        ),
        p(
            "pw-stroke",
            "Stroke",
            &["Stroke/CVA"],
            &["stroke", "cva", "facial droop", "slurred speech"],
            &[],
            vec![need("Stroke centre", &["stroke"])],
        ),
        p(
            "pw-stemi",
            "Chest pain / STEMI",
            &["Chest Pain"],
            &["stemi", "st elevation", "twelve lead", "12 lead"],
            &[],
            vec![need("Cath lab", &["stemi"])],
        ),
        p(
            "pw-trauma",
            "Major trauma",
            &["Vehicle Accident"],
            &["gunshot", "stabbing", "ejected", "entrapment", "fall from"],
            &[],
            vec![need("Trauma centre", &["trauma"])],
        ),
        p(
            "pw-burn",
            "Burns",
            &[],
            &["burn victim", "burns to", "thermal burn"],
            &[],
            vec![need("Burn centre", &["burn"])],
        ),
        p(
            "pw-peds",
            "Pediatric",
            &[],
            &["pediatric", "infant", "toddler", "month old", "year old child"],
            &[],
            vec![need("Closest pediatric hospital", &["peds"])],
        ),
        p(
            "pw-ob",
            "Childbirth",
            &["Gynecology"],
            &["childbirth", "in labor", "in labour", "imminent delivery", "ob "],
            &[],
            vec![need("Obstetrics", &["ob"])],
        ),
        // A medical alarm — a lifeline or medical alert activation — is a
        // medical run, and sits above the catch-all so it is not caught by
        // the alarm types excluded there.
        p(
            "pw-medical-alarm",
            "Medical alarm",
            &[],
            &["medical alarm", "medical alert", "lifeline"],
            &[],
            vec![need("Closest hospital", &[])],
        ),
        // The catch-all: no type and no phrase, so it takes whatever is
        // left — which is what "a distance for every run" asks for. Only
        // the dispatch types nobody is taken to hospital from are refused,
        // and by type rather than by word.
        p(
            "pw-any",
            "Any other medical run",
            &[],
            &[],
            &["Fire Alarm", "Residence Alarm", "Gas Odor", "Investigation"],
            vec![need("Closest hospital", &[])],
        ),
    ]
}

// ---------------------------------------------------------------- tidying

pub fn sanitize(s: &mut Settings) {
    s.pathways.truncate(MAX_PATHWAYS);
    let mut seen = std::collections::HashSet::new();
    let now = crate::library::now();
    for (i, p) in s.pathways.iter_mut().enumerate() {
        p.id = crate::analyzers::clean_line(&p.id, 64);
        if p.id.is_empty() || !seen.insert(p.id.clone()) {
            p.id = format!("pw{now}-{i}");
            seen.insert(p.id.clone());
        }
        p.name = crate::analyzers::clean_line(&p.name, 80);
        if p.name.is_empty() {
            p.name = format!("Pathway {}", i + 1);
        }
        p.when.call_types = clean_words(&p.when.call_types, 40);
        p.when.phrases = clean_words(&p.when.phrases, 40);
        p.when.except = clean_words(&p.when.except, 40);
        p.needs.truncate(MAX_NEEDS);
        for (j, n) in p.needs.iter_mut().enumerate() {
            n.label = crate::analyzers::clean_line(&n.label, 60);
            if n.label.is_empty() {
                n.label = format!("Facility {}", j + 1);
            }
            n.features = clean_words(&n.features, 12);
        }
    }
}

fn clean_words(v: &[String], max: usize) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    v.iter()
        .map(|k| crate::analyzers::clean_line(k, 64))
        .filter(|k| !k.is_empty() && seen.insert(k.to_lowercase()))
        .take(max)
        .collect()
}

// --------------------------------------------------------------- settings

fn path(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    use tauri::Manager;
    let d = app.path().app_config_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    Ok(d.join("pathways.json"))
}

pub fn load(app: &tauri::AppHandle) -> Settings {
    let mut s: Settings = path(app)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    // The first run gets the starting set, so the map is useful before
    // anybody has written a rule. Deleting them all and restarting does
    // not bring them back.
    if !s.seeded {
        s.seeded = true;
        if s.pathways.is_empty() {
            s.pathways = starter();
        }
        let _ = store(app, &s);
    }
    s
}

pub fn store(app: &tauri::AppHandle, s: &Settings) -> Result<(), String> {
    std::fs::write(
        path(app)?,
        serde_json::to_string_pretty(s).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

/// What the starting set would have made of the runs already recorded —
/// the same "show me against my own data" the tripwire preview gives,
/// because a pathway that never matches anything is worse than none.
#[derive(Serialize, Clone, Debug, Default)]
pub struct Preview {
    pub incidents: i64,
    pub placed: i64,
    /// (pathway name, how many runs it took) in list order.
    pub per_pathway: Vec<(String, i64)>,
    pub unmatched: i64,
    /// A few runs nothing matched, to read.
    pub samples: Vec<String>,
    /// Call types nothing matched, commonest first.
    pub missed_types: Vec<(String, i64)>,
}

pub fn preview(c: &rusqlite::Connection, s: &Settings) -> Result<Preview, String> {
    let mut out = Preview::default();
    let mut counts: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let mut missed: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let mut st = c
        .prepare(
            "SELECT id, call_type, summary, lat FROM incidents ORDER BY updated DESC LIMIT 5000",
        )
        .map_err(|e| e.to_string())?;
    let rows: Vec<(i64, String, String, Option<f64>)> = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .collect();
    // Transcripts in one pass rather than a query per run.
    let mut texts: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
    if let Ok(mut ts) = c.prepare(
        "SELECT ic.incident, c.transcript FROM incident_calls ic JOIN calls c ON c.id = ic.call
         WHERE c.transcript IS NOT NULL",
    ) {
        if let Ok(it) = ts.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        }) {
            for (id, t) in it.flatten() {
                let e = texts.entry(id).or_default();
                e.push(' ');
                e.push_str(&t);
            }
        }
    }
    for (id, call_type, summary, lat) in &rows {
        out.incidents += 1;
        if lat.is_some() {
            out.placed += 1;
        }
        let about = format!("{call_type} {summary}");
        let text = format!(
            "{} {}",
            about,
            texts.get(id).map(String::as_str).unwrap_or("")
        );
        match first_match(s, Run::new(call_type, &about, &text)) {
            Some(p) => *counts.entry(p.name.clone()).or_default() += 1,
            None => {
                out.unmatched += 1;
                *missed.entry(call_type.clone()).or_default() += 1;
                if out.samples.len() < 8 {
                    let short: String = summary.chars().take(90).collect();
                    out.samples.push(format!("{call_type} — {short}"));
                }
            }
        }
    }
    out.per_pathway = s
        .pathways
        .iter()
        .map(|p| (p.name.clone(), counts.get(&p.name).copied().unwrap_or(0)))
        .collect();
    let mut m: Vec<(String, i64)> = missed.into_iter().collect();
    m.sort_by(|a, b| b.1.cmp(&a.1));
    m.truncate(12);
    out.missed_types = m;
    Ok(out)
}

// --------------------------------------------------------------- commands

#[derive(Serialize, Clone, Debug, Default)]
pub struct View {
    pub settings: Settings,
    /// The feature tags places can carry, with their labels.
    pub features: Vec<(String, String)>,
    /// Hospitals that carry each feature, so a pathway can say when it is
    /// asking for something nothing can satisfy.
    pub have: Vec<(String, i64)>,
    /// Placed hospitals, and how many are missing a location — one without
    /// coordinates can never be chosen, and silently so.
    pub hospitals: i64,
    pub unplaced: i64,
}

#[tauri::command]
pub fn pathways_get(state: tauri::State<crate::AppState>) -> View {
    let settings = state.pathways.lock().unwrap().clone();
    let places = state.places.lock().unwrap().settings.clone();
    let hospitals: Vec<&crate::places::Place> = places
        .places
        .iter()
        .filter(|p| p.enabled && p.kind == "hospital")
        .collect();
    let have = crate::places::KNOWN_FEATURES
        .iter()
        .map(|(k, _)| {
            let n = hospitals
                .iter()
                .filter(|p| {
                    p.lat.is_some()
                        && p.features.iter().any(|f| f.eq_ignore_ascii_case(k))
                })
                .count() as i64;
            (k.to_string(), n)
        })
        .collect();
    View {
        settings,
        features: crate::places::KNOWN_FEATURES
            .iter()
            .map(|(k, l)| (k.to_string(), l.to_string()))
            .collect(),
        have,
        hospitals: hospitals.len() as i64,
        unplaced: hospitals.iter().filter(|p| p.lat.is_none()).count() as i64,
    }
}

#[tauri::command]
pub fn pathways_set(
    app: tauri::AppHandle,
    state: tauri::State<crate::AppState>,
    settings: Settings,
) -> Result<Settings, String> {
    let mut s = settings;
    s.seeded = true;
    sanitize(&mut s);
    store(&app, &s)?;
    *state.pathways.lock().unwrap() = s.clone();
    Ok(s)
}

/// Put the starting set back, for when it has been edited into a corner.
#[tauri::command]
pub fn pathways_reset(
    app: tauri::AppHandle,
    state: tauri::State<crate::AppState>,
) -> Result<Settings, String> {
    let s = Settings {
        enabled: true,
        pathways: starter(),
        seeded: true,
    };
    store(&app, &s)?;
    *state.pathways.lock().unwrap() = s.clone();
    Ok(s)
}

/// A drawn route from a run to one of its facilities, for the map.
#[derive(Serialize, Clone, Debug, Default)]
pub struct Leg {
    pub label: String,
    pub place_name: String,
    pub to: (f64, f64),
    pub meters: f64,
    pub secs: f64,
    pub how: String,
    pub line: Vec<(f64, f64)>,
    /// The same wording the popup and a message use.
    pub say: String,
}

/// The way from a run to the facility it needs, with the shape to draw.
///
/// `which` picks among the run's targets: a label, a place id, or empty for
/// the first — which is the pathway's own first choice, since a pathway
/// lists what it needs in order.
///
/// The targets were worked out and stored when the run came in, so this
/// only asks the router for the shape. If the run has no targets — nothing
/// matched, or it has no position — there is nothing to draw and the page
/// is told so rather than being given a straight line to nowhere.
#[tauri::command]
pub fn incident_route(
    state: tauri::State<crate::AppState>,
    id: i64,
    which: String,
) -> Result<Option<Leg>, String> {
    let db = state.db.lock().unwrap().clone().ok_or("no library open")?;
    let i = {
        let c = db.lock().unwrap();
        crate::dispatch::inc_get(&c, id)?.ok_or("no such run")?
    };
    let Some((lat, lon)) = i.lat.zip(i.lon) else {
        return Ok(None);
    };
    let want = which.trim().to_lowercase();
    let t = i
        .targets
        .iter()
        .find(|t| {
            want.is_empty()
                || t.place_id.eq_ignore_ascii_case(&want)
                || t.label.to_lowercase() == want
        })
        .or_else(|| i.targets.first());
    let Some(t) = t else {
        return Ok(None);
    };
    let r = crate::routing::shape(&state, (lat, lon), (t.lat, t.lon));
    // The stored target already knows the distance; the route is asked for
    // the shape. Where the router answered now, its numbers are the fresher
    // ones and are used — so a popup opened after the router came up stops
    // saying "direct".
    let (meters, secs, how) = if r.how == "road" {
        (r.meters, r.secs, r.how.clone())
    } else {
        (t.meters, t.secs, t.how.clone())
    };
    let said = Target {
        label: t.label.clone(),
        place_name: t.place_name.clone(),
        meters,
        secs,
        how: how.clone(),
        ..t.clone()
    };
    Ok(Some(Leg {
        label: t.label.clone(),
        place_name: t.place_name.clone(),
        to: (t.lat, t.lon),
        meters,
        secs,
        how,
        line: r.line,
        say: said.say(),
    }))
}

#[tauri::command]
pub fn pathways_preview(
    state: tauri::State<crate::AppState>,
    settings: Option<Settings>,
) -> Result<Preview, String> {
    let s = settings.unwrap_or_else(|| state.pathways.lock().unwrap().clone());
    let db = state.db.lock().unwrap().clone().ok_or("no library open")?;
    let c = db.lock().unwrap();
    preview(&c, &s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(id: &str, kind: &str, features: &[&str], lat: f64, lon: f64) -> crate::places::Place {
        crate::places::Place {
            id: id.into(),
            name: id.into(),
            kind: kind.into(),
            lat: Some(lat),
            lon: Some(lon),
            features: features.iter().map(|s| s.to_string()).collect(),
            enabled: true,
            ..Default::default()
        }
    }

    fn road(m: f64, s: f64) -> crate::routing::Distance {
        crate::routing::Distance {
            meters: m,
            secs: s,
            how: "road",
        }
    }

    // The finding that shaped the whole design: dispatch called this one
    // "Unconscious", and only the words give it away.
    #[test]
    fn a_working_pediatric_arrest_filed_as_unconscious_still_matches() {
        let s = Settings {
            enabled: true,
            pathways: starter(),
            seeded: true,
        };
        let about = haystack(
            "Unconscious",
            "Working pediatric cardiac arrest at 4350 Madison Avenue, Apartment 610.",
            &[],
        );
        let got = first_match(&s, Run::new("Unconscious", &about, &about)).expect("nothing matched");
        assert_eq!(got.id, "pw-peds-arrest", "matched {} instead", got.name);
        // And it asks for the pediatric places, not the general ones.
        assert_eq!(got.needs[0].features, vec!["peds"]);
        assert_eq!(got.needs[1].features, vec!["peds", "ecmo"]);
    }

    #[test]
    fn the_first_pathway_that_matches_is_the_one_used() {
        let s = Settings {
            enabled: true,
            pathways: starter(),
            seeded: true,
        };
        // A plain arrest falls through the pediatric one to the general one.
        let t = haystack("Cardiac Arrest", "Dispatching Medic 18 for a cardiac arrest.", &[]);
        assert_eq!(first_match(&s, Run::new("Cardiac Arrest", &t, &t)).unwrap().id, "pw-arrest");
        // A stroke reaches the stroke pathway.
        let t = haystack("Stroke/CVA", "possible stroke", &[]);
        assert_eq!(first_match(&s, Run::new("Stroke/CVA", &t, &t)).unwrap().id, "pw-stroke");
        // And a sick person nobody wrote a pathway for lands on the
        // catch-all, because every run deserves a nearest hospital.
        let t = haystack("Sick Person", "EMS dispatched for a sick person.", &[]);
        assert_eq!(first_match(&s, Run::new("Sick Person", &t, &t)).unwrap().id, "pw-any");
    }

    // A medical alarm is a lifeline activation: an ambulance goes and
    // somebody may well be taken in. Refusing runs by the word "alarm"
    // dropped six of these on a real library, all filed as Sick Person —
    // which is why the catch-all excludes alarm *types*, not the word.
    #[test]
    fn a_medical_alarm_is_a_medical_run() {
        let s = Settings {
            enabled: true,
            pathways: starter(),
            seeded: true,
        };
        for (ct, sum) in [
            ("Sick Person", "Engine 8 dispatched to a medical alarm."),
            ("Residence Alarm", "Engine 30 dispatched to a medical alarm."),
            ("Sick Person", "Medic 5 to a lifeline activation."),
        ] {
            let t = haystack(ct, sum, &[]);
            let got = first_match(&s, Run::new(ct, &t, &t));
            assert!(got.is_some(), "{ct} / {sum} got no hospital at all");
            let got = got.unwrap();
            assert!(
                !got.needs.is_empty() && got.needs.iter().any(|n| n.features.is_empty()),
                "{ct} matched {} but not to any hospital",
                got.name
            );
        }
    }

    #[test]
    fn a_fire_alarm_is_not_taken_to_hospital() {
        let s = Settings {
            enabled: true,
            pathways: starter(),
            seeded: true,
        };
        for (ct, sum) in [
            ("Fire Alarm", "Engine 7 responding to a fire alarm."),
            ("Gas Odor", "Investigating a gas odor."),
            ("Residence Alarm", "Residence alarm, smoke detector sounding."),
        ] {
            let t = haystack(ct, sum, &[]);
            let r = Run::new(ct, &t, &t);
            assert!(
                first_match(&s, r).is_none(),
                "{ct} matched {:?}",
                first_match(&s, r).map(|p| p.name.clone())
            );
        }
    }

    // The words that matter most are on the radio, not in the summary.
    #[test]
    fn words_in_a_transcript_count() {
        let p = &starter()[1];
        let about = haystack("Unconscious", "Units dispatched.", &[]);
        assert!(!matches(p, Run::new("Unconscious", &about, &about)));
        let heard = haystack(
            "Unconscious",
            "Units dispatched.",
            &["Engine 7 on scene, CPR in progress".into()],
        );
        assert!(matches(p, Run::new("Unconscious", &about, &heard)));
    }

    #[test]
    fn except_rules_a_run_out_before_anything_else() {
        let p = Pathway {
            id: "x".into(),
            name: "Arrest".into(),
            enabled: true,
            when: When {
                call_types: vec!["Cardiac Arrest".into()],
                phrases: vec!["cardiac arrest".into()],
                except: vec!["cancelled".into()],
                except_types: vec![],
            },
            needs: vec![],
        };
        let a = "cardiac arrest at the mall";
        assert!(matches(&p, Run::new("Cardiac Arrest", a, a)));
        let b = "cardiac arrest at the mall, run cancelled";
        assert!(!matches(&p, Run::new("Cardiac Arrest", b, b)));
        // A word only said on the radio does not disqualify a run: the
        // exception reads what the run is, not everything anybody said.
        assert!(matches(&p, Run::new("Cardiac Arrest", a, "cardiac arrest at the mall. the other run was cancelled")));
    }

    #[test]
    fn a_switched_off_pathway_matches_nothing() {
        let mut p = starter()[1].clone();
        p.enabled = false;
        assert!(!matches(&p, Run::new("Cardiac Arrest", "cardiac arrest", "cardiac arrest")));
    }

    // A station somebody tagged `ecmo` is not somewhere to take a patient.
    #[test]
    fn only_hospitals_are_chosen_and_every_feature_must_match() {
        let s = crate::places::Settings {
            places: vec![
                place("station", "station", &["ecmo"], 39.77, -86.16),
                place("general", "hospital", &[], 39.78, -86.16),
                place("heart", "hospital", &["ecmo"], 39.90, -86.16),
                place("kids-heart", "hospital", &["ecmo", "peds"], 39.95, -86.16),
            ],
        };
        let from = (39.77, -86.16);

        let any: Vec<&str> = hospitals_with(&s, &[], from).iter().map(|(p, _)| p.id.as_str()).collect();
        assert_eq!(any, vec!["general", "heart", "kids-heart"], "a station was offered");

        let ecmo: Vec<&str> = hospitals_with(&s, &["ecmo".into()], from).iter().map(|(p, _)| p.id.as_str()).collect();
        assert_eq!(ecmo, vec!["heart", "kids-heart"]);

        // Both features, not either.
        let both: Vec<&str> = hospitals_with(&s, &["ecmo".into(), "peds".into()], from)
            .iter()
            .map(|(p, _)| p.id.as_str())
            .collect();
        assert_eq!(both, vec!["kids-heart"]);

        // Nothing qualifies — better no target than the wrong one.
        assert!(hospitals_with(&s, &["burn".into()], from).is_empty());

        // A hospital with no coordinates can never be chosen.
        let mut s2 = s.clone();
        s2.places.push(crate::places::Place {
            id: "unplaced".into(),
            name: "unplaced".into(),
            kind: "hospital".into(),
            features: vec!["burn".into()],
            enabled: true,
            ..Default::default()
        });
        assert!(hospitals_with(&s2, &["burn".into()], from).is_empty());
    }

    // The nearest by crow flight is not always the nearest by road.
    #[test]
    fn the_quickest_by_road_wins_not_the_nearest_on_the_map() {
        let s = crate::places::Settings {
            places: vec![
                place("across-the-river", "hospital", &[], 39.780, -86.160),
                place("straight-up-the-road", "hospital", &[], 39.800, -86.160),
            ],
        };
        let from = (39.770, -86.160);
        // The near one is a long way round; the far one is a straight run.
        let mut route = |_f: (f64, f64), t: (f64, f64)| {
            if (t.0 - 39.780).abs() < 1e-6 {
                road(9_000.0, 900.0)
            } else {
                road(3_400.0, 300.0)
            }
        };
        let need = Need {
            label: "Closest hospital".into(),
            features: vec![],
        };
        let got = resolve(&need, &s, from, &mut route).unwrap();
        assert_eq!(got.place_id, "straight-up-the-road");
        assert_eq!(got.how, "road");
        assert!(got.say().contains("by road"), "{}", got.say());
    }

    // With no router the numbers are still honest about what they are.
    #[test]
    fn with_no_router_the_straight_line_says_so_and_never_shows_a_time() {
        let s = crate::places::Settings {
            places: vec![place("general", "hospital", &[], 39.80, -86.16)],
        };
        let mut straight = |f: (f64, f64), t: (f64, f64)| crate::routing::straight(f, t);
        let need = Need {
            label: "Closest hospital".into(),
            features: vec![],
        };
        let got = resolve(&need, &s, (39.77, -86.16), &mut straight).unwrap();
        assert_eq!(got.how, "straight");
        assert_eq!(got.secs, 0.0);
        assert!(got.meters > 0.0);
        let said = got.say();
        assert!(said.contains("direct"), "{said}");
        assert!(!said.contains("min"), "a straight line was given a time: {said}");
    }

    #[test]
    fn a_needs_list_comes_back_in_the_order_it_was_written() {
        let s = crate::places::Settings {
            places: vec![
                place("general", "hospital", &[], 39.78, -86.16),
                place("heart", "hospital", &["ecmo"], 39.90, -86.16),
            ],
        };
        let mut route = |f: (f64, f64), t: (f64, f64)| crate::routing::straight(f, t);
        let p = &starter()[1];
        let got = resolve_all(p, &s, (39.77, -86.16), &mut route);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].label, "Closest hospital");
        assert_eq!(got[0].place_id, "general");
        assert_eq!(got[1].label, "ECMO centre");
        assert_eq!(got[1].place_id, "heart");
        // Said together, it reads as two lines.
        let text = say_all(&got);
        assert_eq!(text.lines().count(), 2);
        assert!(text.starts_with("Closest hospital: general"), "{text}");
    }

    // A need nothing can satisfy is left out rather than faked.
    #[test]
    fn a_need_with_nowhere_to_send_it_is_left_out() {
        let s = crate::places::Settings {
            places: vec![place("general", "hospital", &[], 39.78, -86.16)],
        };
        let mut route = |f: (f64, f64), t: (f64, f64)| crate::routing::straight(f, t);
        let p = &starter()[0]; // pediatric arrest: peds, peds+ecmo, any
        let got = resolve_all(p, &s, (39.77, -86.16), &mut route);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].label, "Closest hospital");
    }

    #[test]
    fn the_starter_set_names_no_hospital_and_nowhere_in_particular() {
        let text = format!("{:?}", starter()).to_lowercase();
        for local in ["indiana", "indianapolis", "marion", "riley", "methodist", "eskenazi", "iu "] {
            assert!(!text.contains(local), "the starter set mentions {local}");
        }
        // Every pathway asks for something, and the catch-all is last.
        let s = starter();
        assert!(s.iter().all(|p| !p.needs.is_empty()));
        assert_eq!(s.last().unwrap().id, "pw-any");
        assert!(s.last().unwrap().when.call_types.is_empty());
        assert!(s.last().unwrap().when.phrases.is_empty());
    }

    /// What the starting set makes of a real library. Off by default —
    /// point `HS_LIB_DB` at a *copy* of a `calls.db` and run
    /// `cargo test starter_against_a_real_library -- --ignored --nocapture`.
    /// A pathway that never matches anything is worse than no pathway, and
    /// only somebody's own recorded runs can show that.
    #[test]
    #[ignore]
    fn starter_against_a_real_library() {
        let Ok(path) = std::env::var("HS_LIB_DB") else {
            return;
        };
        let c = rusqlite::Connection::open(&path).expect("open the library copy");
        let s = Settings {
            enabled: true,
            pathways: starter(),
            seeded: true,
        };
        let p = preview(&c, &s).expect("preview");
        println!("\n{} runs, {} of them placed", p.incidents, p.placed);
        for (name, n) in &p.per_pathway {
            println!("  {n:>5}  {name}");
        }
        println!("  {:>5}  (no pathway)", p.unmatched);
        println!("\ncall types nothing matched:");
        for (t, n) in &p.missed_types {
            println!("  {n:>5}  {t}");
        }
        println!("\nsamples nothing matched:");
        for s in &p.samples {
            println!("  {s}");
        }
    }

    #[test]
    fn what_the_page_sends_is_tidied() {
        let mut s = Settings {
            enabled: true,
            seeded: false,
            pathways: vec![
                Pathway::default(),
                Pathway {
                    id: "dup".into(),
                    ..Default::default()
                },
                Pathway {
                    id: "dup".into(),
                    when: When {
                        phrases: vec!["  arrest  ".into(), "ARREST".into(), "".into()],
                        ..Default::default()
                    },
                    needs: vec![Need {
                        label: "".into(),
                        features: vec!["ECMO".into(), "ecmo".into()],
                    }],
                    ..Default::default()
                },
            ],
        };
        sanitize(&mut s);
        let ids: std::collections::HashSet<&str> = s.pathways.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids.len(), 3, "two pathways share an id");
        assert!(s.pathways.iter().all(|p| !p.name.is_empty()));
        // Duplicates fold together, blanks go, and a nameless need is named.
        assert_eq!(s.pathways[2].when.phrases, vec!["arrest"]);
        assert_eq!(s.pathways[2].needs[0].features, vec!["ECMO"]);
        assert_eq!(s.pathways[2].needs[0].label, "Facility 1");
    }
}
