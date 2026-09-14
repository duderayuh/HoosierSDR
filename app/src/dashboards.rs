//! Dashboards: boards the listener composes out of panes.
//!
//! A dashboard is a row of panes and a footer. Each pane is one question
//! asked of the traffic — "dispatch runs whose closest hospital is Methodist,
//! within three miles of it" on the left, "what Methodist is being told on the
//! radio" on the right — and the board is the two of them side by side, newest
//! at the top, refreshing itself as calls land.
//!
//! Nothing here is specific to any hospital, call type or word. A pane names
//! what it wants by referring to a place in the place book and to call types
//! the dispatch settings already know; the emphasis rules that make a run
//! stand out are the listener's own. That is the whole point: the board that
//! matters to a STEMI centre is not the one that matters to a burn unit, and
//! neither is worth hard-coding.
//!
//! Matching happens here, not on the page. A board can be shared over the
//! tailnet to a machine that must see that board and nothing else, and that
//! is only true if the filtering is done before anything is sent — a page
//! that chose for itself would have to be handed every run first. The page,
//! and the shared one, are both given finished cards.

use serde::{Deserialize, Serialize};

const MAX_DASHBOARDS: usize = 40;
const MAX_PANES: usize = 8;
const MAX_EMPHASIS: usize = 12;
/// Rows a pane draws when it does not say.
const DEFAULT_LIMIT: u32 = 25;

/// How a pane draws attention to some of its rows.
///
/// The screenshot this was built from highlighted cardiac arrests, but that is
/// a property of the listener's interest, not of the software: the rule is a
/// `When` like any other, so a board can shout about strokes, or about the
/// word "entrapment", or about nothing.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Emphasis {
    /// What makes a row stand out. Reuses the pathway matcher's shape, so the
    /// call-type and phrase lists behave the same everywhere.
    pub when: crate::pathways::When,
    /// One of [`STYLES`]. Not free-form: it becomes a CSS class.
    pub style: String,
    /// Shown next to the row when the rule fires. Optional.
    pub note: String,
}

/// The emphases a pane may use, loudest first. The names are deliberately
/// about weight rather than meaning — "alarm" is not "cardiac arrest".
pub const STYLES: &[(&str, &str)] = &[
    ("alarm", "Alarm — red, pulsing"),
    ("warn", "Warn — amber"),
    ("note", "Note — blue"),
    ("calm", "Calm — green"),
    ("dim", "Dim — played down"),
];

fn default_style() -> String {
    "alarm".into()
}

/// What a pane shows.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Pane {
    pub id: String,
    /// Shown in the pane's header. Empty takes the kind's own name.
    pub title: String,
    /// `dispatch` — runs from the dispatch channels; `reports` — the
    /// EMS-to-hospital hand-offs for a place's talkgroups.
    pub kind: String,
    /// Flex weight against the other panes on the board. 1 is a column.
    pub width: u32,
    /// Most rows to show. The rest are still matched, just not drawn.
    pub limit: u32,

    // ---- dispatch panes ----
    /// Only runs of these call types. Empty means every type.
    pub call_types: Vec<String>,
    /// Only runs whose type, summary or transcripts contain one of these.
    pub phrases: Vec<String>,
    /// Runs matching one of these are left out, whatever else they match.
    pub except: Vec<String>,
    /// Only runs whose nearest hospital in the place book is this place.
    ///
    /// A rank, not a radius: a run four miles out still belongs to the
    /// hospital it is closest to. Held separately from [`Pane::within_place`]
    /// because the listener asked for both at once — the runs Methodist would
    /// receive, *and* the runs on Methodist's doorstep, which are not the
    /// same set.
    pub closest_place: String,
    /// Only runs within [`Pane::within_miles`] of this place.
    pub within_place: String,
    pub within_miles: f64,

    // ---- report panes ----
    /// Whose hand-off reports to show; matched through the place's talkgroups.
    pub place: String,

    /// Rows that should catch the eye, first match wins.
    pub emphasis: Vec<Emphasis>,
}

/// One board.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Dashboard {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub panes: Vec<Pane>,
    /// Free text pinned along the bottom — the attestation a board is read
    /// under, whatever that is here. Empty by default: the wording is a local
    /// legal matter and not something to ship an opinion about.
    pub footer: String,
    /// Seconds between the clock ticking over and stale rows fading. The board
    /// redraws on events regardless; this only governs the "20 minutes ago"
    /// text and the freshness fade.
    pub refresh_secs: u32,

    /// Whether this board answers on the tailnet at all. Off unless the
    /// listener says otherwise, one board at a time.
    pub shared: bool,
    /// The secret in a share link, and the only thing that opens a shared
    /// board.
    ///
    /// A board gets its own credential rather than reusing the web token or
    /// tailnet trust, because both of those open `/api/command` — every
    /// command the desktop has, including the one that hands back the token
    /// itself. Lending someone a board must not lend them the radio.
    pub share_key: String,
}

impl Default for Dashboard {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            enabled: true,
            panes: Vec::new(),
            footer: String::new(),
            refresh_secs: 10,
            shared: false,
            share_key: String::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Settings {
    pub dashboards: Vec<Dashboard>,
}

pub fn sanitize(s: &mut Settings) {
    s.dashboards.truncate(MAX_DASHBOARDS);
    let now = crate::library::now();
    let mut seen = std::collections::HashSet::new();
    for (i, d) in s.dashboards.iter_mut().enumerate() {
        d.id = crate::analyzers::clean_line(&d.id, 64);
        if d.id.is_empty() || !seen.insert(d.id.clone()) {
            d.id = format!("db{now}-{i}");
            seen.insert(d.id.clone());
        }
        d.name = crate::analyzers::clean_line(&d.name, 80);
        if d.name.is_empty() {
            d.name = format!("Dashboard {}", i + 1);
        }
        // The footer is the one multi-line field: it is an attestation, and
        // those have paragraphs. Everything else is a single line.
        d.footer = crate::analyzers::clean_text(&d.footer, 2_000);
        d.refresh_secs = d.refresh_secs.clamp(5, 3_600);
        // A shared board always has a key, and keeps the one it has: the link
        // is already in someone's browser. Sharing is the only thing that
        // mints one, and un-sharing forgets it, so switching sharing back on
        // hands out a new link rather than reviving the old one.
        //
        // This is also how "new link" works, and why there is no command for
        // it: the editor clears `share_key` and saves, and a board that is
        // still shared is issued a fresh one on the way in.
        d.share_key = d
            .share_key
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(64)
            .collect();
        if d.shared && d.share_key.len() < SHARE_KEY_CHARS {
            d.share_key = new_share_key();
        }
        if !d.shared {
            d.share_key = String::new();
        }

        d.panes.truncate(MAX_PANES);
        let mut pane_ids = std::collections::HashSet::new();
        for (j, p) in d.panes.iter_mut().enumerate() {
            p.id = crate::analyzers::clean_line(&p.id, 64);
            if p.id.is_empty() || !pane_ids.insert(p.id.clone()) {
                p.id = format!("pn{now}-{i}-{j}");
                pane_ids.insert(p.id.clone());
            }
            if p.kind != "reports" {
                p.kind = "dispatch".into();
            }
            p.title = crate::analyzers::clean_line(&p.title, 60);
            p.width = p.width.clamp(1, 6);
            // Absent means the default, not "none": a board written before
            // this field existed reads back as zero, and clamping that to one
            // would quietly shrink a full pane to a single row.
            if p.limit == 0 {
                p.limit = DEFAULT_LIMIT;
            }
            p.limit = p.limit.clamp(1, 200);
            p.call_types = clean_words(&p.call_types, 40);
            p.phrases = clean_words(&p.phrases, 40);
            p.except = clean_words(&p.except, 40);
            p.closest_place = crate::analyzers::clean_line(&p.closest_place, 64);
            p.within_place = crate::analyzers::clean_line(&p.within_place, 64);
            p.place = crate::analyzers::clean_line(&p.place, 64);
            // A radius of zero would hide everything; treat it as "no radius"
            // and let the place stand for itself.
            if !p.within_miles.is_finite() || p.within_miles <= 0.0 {
                p.within_miles = 0.0;
                p.within_place = String::new();
            }
            p.within_miles = p.within_miles.min(200.0);

            p.emphasis.truncate(MAX_EMPHASIS);
            for e in p.emphasis.iter_mut() {
                e.style = crate::analyzers::clean_line(&e.style, 16).to_lowercase();
                if !STYLES.iter().any(|(k, _)| *k == e.style) {
                    e.style = default_style();
                }
                e.note = crate::analyzers::clean_line(&e.note, 60);
                e.when.call_types = clean_words(&e.when.call_types, 40);
                e.when.phrases = clean_words(&e.when.phrases, 40);
                e.when.except = clean_words(&e.when.except, 40);
                e.when.except_types = clean_words(&e.when.except_types, 40);
            }
        }
    }
}

/// Long enough that guessing is not a strategy: 32 characters of base-36 is
/// about 165 bits.
const SHARE_KEY_CHARS: usize = 32;

fn new_share_key() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..SHARE_KEY_CHARS)
        .map(|_| {
            let n: u8 = rng.gen_range(0..36);
            if n < 10 {
                (b'0' + n) as char
            } else {
                (b'a' + n - 10) as char
            }
        })
        .collect()
}

/// May this request see this board?
///
/// Written apart from the routing so it can be tested without a server, and
/// so the rule is in one readable place. A board is reachable only when it
/// exists, is switched on, is shared, and the caller quotes its key — and
/// when any of that fails the answer is the same, because saying "wrong key"
/// would confirm the board is there.
pub fn shared_board<'a>(s: &'a Settings, id: &str, key: &str) -> Option<&'a Dashboard> {
    let d = s.dashboards.iter().find(|d| d.id == id)?;
    if !d.enabled || !d.shared || d.share_key.is_empty() {
        return None;
    }
    // Fixed-time compare: the key is a secret, and a byte-at-a-time answer
    // is one someone can walk.
    let (a, b) = (d.share_key.as_bytes(), key.as_bytes());
    let same = a.len() == b.len()
        && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0;
    same.then_some(d)
}

fn clean_words(v: &[String], max: usize) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    v.iter()
        .map(|k| crate::analyzers::clean_line(k, 64))
        .filter(|k| !k.is_empty() && seen.insert(k.to_lowercase()))
        .take(max)
        .collect()
}

// ------------------------------------------------------------- rendering

/// One row on a board, with every decision already made.
///
/// The page receives this and writes it out; it does not decide what belongs
/// on the board, because a board is also served to machines that must see
/// this board and nothing else. Matching where the data lives is the only
/// way that promise can be kept — a page that filtered for itself would have
/// to be handed every incident first.
#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub struct Card {
    pub id: i64,
    /// When this row last changed, epoch seconds — the "12 min ago" line.
    pub at: i64,
    pub emoji: String,
    pub title: String,
    /// The emphasis that fired, if one did: a [`STYLES`] key and its note.
    pub style: String,
    pub note: String,
    /// Small print under the title, already in order.
    pub meta: Vec<String>,
    pub body: String,
    /// The stated time to arrival inside `body`, for a report row that gives
    /// one. The page marks the span by looking this phrase back up.
    pub eta: Option<String>,
}

#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub struct RenderedPane {
    pub id: String,
    pub kind: String,
    pub title: String,
    /// The qualifier under the title — "closest to Methodist".
    pub sub: String,
    pub width: u32,
    /// How many matched, which is not how many are drawn.
    pub total: usize,
    pub cards: Vec<Card>,
}

#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub struct RenderedBoard {
    pub id: String,
    pub name: String,
    pub footer: String,
    pub refresh_secs: u32,
    pub panes: Vec<RenderedPane>,
    /// This machine's clock at render. A wall display's own clock may be
    /// wrong, and "3 minutes ago" is the whole point of the board.
    pub at: i64,
}

const MI: f64 = 1609.344;

fn haversine_m(a: (f64, f64), b: (f64, f64)) -> f64 {
    let r = std::f64::consts::PI / 180.0;
    let (d_lat, d_lon) = ((b.0 - a.0) * r, (b.1 - a.1) * r);
    let s = (d_lat / 2.0).sin().powi(2)
        + (a.0 * r).cos() * (b.0 * r).cos() * (d_lon / 2.0).sin().powi(2);
    2.0 * 6_371_000.0 * s.sqrt().min(1.0).asin()
}

fn at(p: &crate::places::Place) -> Option<(f64, f64)> {
    Some((p.lat?, p.lon?))
}

/// The hospital a run is closest to, as the crow flies.
///
/// Deliberately not a drive time: a road route for every run against every
/// hospital on every render is a lot of asking, and the answer to "whose
/// patch is this" does not turn on a minute either way.
fn closest_hospital<'a>(
    places: &'a [crate::places::Place],
    p: (f64, f64),
) -> Option<(&'a crate::places::Place, f64)> {
    places
        .iter()
        .filter(|h| h.kind == "hospital")
        .filter_map(|h| at(h).map(|c| (h, haversine_m(p, c))))
        .min_by(|a, b| a.1.total_cmp(&b.1))
}

/// Drive time, but only where it was actually measured: the pathway resolver
/// stores a road route on the run for the facilities it picked. Anything else
/// gets a distance and no minutes, rather than a guess.
fn road_minutes(inc: &crate::dispatch::Incident, place_id: &str) -> Option<i64> {
    inc.targets
        .iter()
        .find(|t| t.place_id == place_id && t.how == "road" && t.secs > 0.0)
        .map(|t| ((t.secs / 60.0).round() as i64).max(1))
}

fn place_by<'a>(places: &'a [crate::places::Place], id: &str) -> Option<&'a crate::places::Place> {
    places.iter().find(|p| p.id == id)
}

fn hay(inc: &crate::dispatch::Incident) -> String {
    format!(
        "{} {} {} {}",
        inc.call_type,
        inc.summary,
        inc.address,
        inc.units.join(" ")
    )
}

fn any_word(words: &[String], text: &str) -> bool {
    !words.is_empty() && !crate::alerts::matched_keywords(words, text).is_empty()
}

/// Does this run belong in this pane?
///
/// Note that `except` is read against everything the row shows, not only
/// against what the run *is* — which is the narrower reading
/// [`crate::pathways::matches`] uses. The two are different questions: a
/// pathway decides where a patient is taken and must not be talked out of it
/// by a passing word, whereas a pane is a view and the listener excluding a
/// word means "keep it off my board".
fn pane_takes(
    pane: &Pane,
    inc: &crate::dispatch::Incident,
    places: &[crate::places::Place],
) -> bool {
    let h = hay(inc);
    if any_word(&pane.except, &h) {
        return false;
    }
    if !pane.call_types.is_empty()
        && !pane
            .call_types
            .iter()
            .any(|t| crate::pathways::same_type(t, &inc.call_type))
    {
        return false;
    }
    if !pane.phrases.is_empty() && !any_word(&pane.phrases, &h) {
        return false;
    }
    let here = inc.lat.zip(inc.lon);
    if !pane.closest_place.is_empty() {
        let Some(p) = here else { return false };
        match closest_hospital(places, p) {
            Some((h, _)) if h.id == pane.closest_place => {}
            _ => return false,
        }
    }
    if !pane.within_place.is_empty() && pane.within_miles > 0.0 {
        let (Some(p), Some(w)) = (here, place_by(places, &pane.within_place).and_then(at)) else {
            return false;
        };
        if haversine_m(p, w) > pane.within_miles * MI {
            return false;
        }
    }
    true
}

/// The first emphasis that fires, so the loudest goes at the top of the list.
fn emphasis_for<'a>(pane: &'a Pane, text: &str, call_type: &str) -> Option<&'a Emphasis> {
    pane.emphasis.iter().find(|e| {
        if e.when
            .except_types
            .iter()
            .any(|t| crate::pathways::same_type(t, call_type))
        {
            return false;
        }
        if any_word(&e.when.except, text) {
            return false;
        }
        let by_type = e
            .when
            .call_types
            .iter()
            .any(|t| crate::pathways::same_type(t, call_type));
        by_type || any_word(&e.when.phrases, text)
    })
}

fn dispatch_card(
    pane: &Pane,
    inc: &crate::dispatch::Incident,
    places: &[crate::places::Place],
) -> Card {
    let h = hay(inc);
    let em = emphasis_for(pane, &h, &inc.call_type);
    let mut meta = Vec::new();
    if !inc.address.is_empty() {
        meta.push(inc.address.clone());
    }
    // Measured against whichever place the pane is about; the radius wins
    // when it has one, because that is the number the listener asked for.
    let refer = if pane.within_place.is_empty() {
        &pane.closest_place
    } else {
        &pane.within_place
    };
    if let (Some(p), Some(r)) = (inc.lat.zip(inc.lon), place_by(places, refer)) {
        if let Some(c) = at(r) {
            meta.push(format!("{:.1} mi", haversine_m(p, c) / MI));
            if let Some(m) = road_minutes(inc, &r.id) {
                meta.push(format!("{m} min"));
            }
        }
    }
    Card {
        id: inc.id,
        at: inc.updated,
        emoji: inc.emoji.clone(),
        title: if inc.call_type.is_empty() {
            "Unknown".into()
        } else {
            inc.call_type.clone()
        },
        style: em.map(|e| e.style.clone()).unwrap_or_default(),
        note: em.map(|e| e.note.clone()).unwrap_or_default(),
        meta,
        body: inc.summary.clone(),
        eta: None,
    }
}

fn report_card(pane: &Pane, r: &crate::conversations::Stored) -> Card {
    let text = format!(
        "{} {} {} {}",
        r.headline,
        r.summary,
        r.tg_name,
        r.units.join(" ")
    );
    let em = emphasis_for(pane, &text, "");
    // Rows stored before headlines existed have none; the summary still reads
    // on its own, so the card loses its title rather than its meaning.
    let title = if !r.headline.is_empty() {
        r.headline.clone()
    } else if !r.units.is_empty() {
        r.units.join(", ")
    } else if !r.tg_name.is_empty() {
        r.tg_name.clone()
    } else {
        "Report".into()
    };
    Card {
        id: r.id,
        at: r.last_at,
        emoji: String::new(),
        title,
        style: em.map(|e| e.style.clone()).unwrap_or_default(),
        note: em.map(|e| e.note.clone()).unwrap_or_default(),
        meta: if r.units.is_empty() {
            Vec::new()
        } else {
            vec![r.units.join(", ")]
        },
        body: r.summary.clone(),
        eta: r.eta.clone(),
    }
}

/// Draw one board from the traffic as it stands.
pub fn render(
    board: &Dashboard,
    incidents: &[crate::dispatch::Incident],
    reports: &[crate::conversations::Stored],
    places: &[crate::places::Place],
    now: i64,
) -> RenderedBoard {
    let panes = board
        .panes
        .iter()
        .map(|pane| {
            let limit = pane.limit.max(1) as usize;
            let (sub, total, cards) = if pane.kind == "reports" {
                let p = place_by(places, &pane.place);
                let tgs: std::collections::HashSet<u16> =
                    p.map(|p| p.tgs.iter().copied().collect()).unwrap_or_default();
                let mut rows: Vec<&crate::conversations::Stored> =
                    reports.iter().filter(|r| tgs.contains(&r.tg)).collect();
                rows.sort_by_key(|r| std::cmp::Reverse(r.last_at));
                let total = rows.len();
                (
                    p.map(|p| p.name.clone()).unwrap_or_default(),
                    total,
                    rows.into_iter()
                        .take(limit)
                        .map(|r| report_card(pane, r))
                        .collect::<Vec<_>>(),
                )
            } else {
                let mut rows: Vec<&crate::dispatch::Incident> = incidents
                    .iter()
                    .filter(|i| pane_takes(pane, i, places))
                    .collect();
                rows.sort_by_key(|i| std::cmp::Reverse(i.updated));
                let total = rows.len();
                let sub = place_by(places, &pane.closest_place)
                    .map(|p| format!("closest to {}", p.name))
                    .unwrap_or_default();
                (
                    sub,
                    total,
                    rows.into_iter()
                        .take(limit)
                        .map(|i| dispatch_card(pane, i, places))
                        .collect::<Vec<_>>(),
                )
            };
            RenderedPane {
                id: pane.id.clone(),
                kind: pane.kind.clone(),
                title: if !pane.title.is_empty() {
                    pane.title.clone()
                } else if pane.kind == "reports" {
                    "Reports".into()
                } else {
                    "Dispatch".into()
                },
                sub,
                width: pane.width.max(1),
                total,
                cards,
            }
        })
        .collect();
    RenderedBoard {
        id: board.id.clone(),
        name: board.name.clone(),
        footer: board.footer.clone(),
        refresh_secs: board.refresh_secs,
        panes,
        at: now,
    }
}

// --------------------------------------------------------------- settings

fn path(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    use tauri::Manager;
    let d = app.path().app_config_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    Ok(d.join("dashboards.json"))
}

/// Read the stored boards, and put them through the same cleaning a save
/// does. `dashboards.json` is hand-editable and outlives the fields it was
/// written with, so what comes off disk is not necessarily what a save would
/// have produced: a board written before `limit` existed reads back as zero,
/// which would draw one row where it used to draw twenty-five.
pub fn load(app: &tauri::AppHandle) -> Settings {
    let raw: Settings = path(app)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let mut s = raw.clone();
    sanitize(&mut s);
    // Write back when cleaning changed something. A file hand-edited to share
    // a board is given a key here, and a key that only ever lived in memory
    // would be a different one after every restart — so the link someone was
    // sent would stop working on the next launch.
    if s != raw {
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

// --------------------------------------------------------------- commands

/// The settings plus what the editor needs to offer real choices.
///
/// The places come back whole rather than as names: a board measures
/// distances against them, so the page needs their coordinates and — for a
/// reports pane — their talkgroups. Handing over the same `places::Place` the
/// place book holds keeps one definition of a place rather than two.
#[derive(Serialize, Clone, Debug, Default)]
pub struct View {
    pub settings: Settings,
    /// Every enabled place, in the place book's order.
    pub places: Vec<crate::places::Place>,
    /// The dispatch call types, so a pane picks rather than types.
    pub call_types: Vec<String>,
    pub styles: Vec<(String, String)>,
}

#[tauri::command]
pub fn dashboards_get(state: tauri::State<crate::AppState>) -> View {
    let settings = state.dashboards.lock().unwrap().clone();
    let places = state
        .places
        .lock()
        .unwrap()
        .settings
        .places
        .iter()
        .filter(|p| p.enabled)
        .cloned()
        .collect();
    let call_types = state
        .dispatch
        .lock()
        .unwrap()
        .settings
        .call_types
        .iter()
        .map(|t| t.name.clone())
        .collect();
    View {
        settings,
        places,
        call_types,
        styles: STYLES
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    }
}

/// Draw one board, ready to show.
///
/// The whole board is worked out here rather than on the page, because this
/// same answer is what a shared board hands to another machine — and that
/// machine must be able to see this board and nothing else. Filtering on the
/// page would mean sending it everything first.
#[tauri::command]
pub fn dashboards_render(
    state: tauri::State<crate::AppState>,
    id: String,
) -> Result<RenderedBoard, String> {
    let board = state
        .dashboards
        .lock()
        .unwrap()
        .dashboards
        .iter()
        .find(|d| d.id == id)
        .cloned()
        .ok_or("no such dashboard")?;
    draw(&state, &board)
}

/// Gather what a board is matched against, and draw it.
///
/// Split out because two callers reach it: the desktop, which may draw any
/// board, and the tailnet handler, which may draw only a shared one. Which
/// board is allowed is settled before this is called; this only fetches and
/// renders.
pub fn draw(state: &crate::AppState, board: &Dashboard) -> Result<RenderedBoard, String> {
    let places: Vec<crate::places::Place> = state
        .places
        .lock()
        .unwrap()
        .settings
        .places
        .iter()
        .filter(|p| p.enabled)
        .cloned()
        .collect();
    let db = state.db.lock().unwrap().clone();
    let (incidents, reports) = match db {
        Some(db) => {
            let c = db.lock().unwrap();
            (
                crate::dispatch::inc_list(&c, 0, INCIDENT_SCAN)?,
                crate::conversations::list_rows(&c, None, None, None, Some(REPORT_SCAN))?,
            )
        }
        None => (Vec::new(), Vec::new()),
    };
    Ok(render(
        board,
        &incidents,
        &reports,
        &places,
        crate::library::now(),
    ))
}

/// How much recent traffic a board is matched against. A pane's own limit
/// governs what it draws; these bound the work, and are well past what any
/// board shows.
const INCIDENT_SCAN: u32 = 2_000;
const REPORT_SCAN: u32 = 400;

#[tauri::command]
pub fn dashboards_set(
    app: tauri::AppHandle,
    state: tauri::State<crate::AppState>,
    settings: Settings,
) -> Result<Settings, String> {
    let mut s = settings;
    sanitize(&mut s);
    store(&app, &s)?;
    *state.dashboards.lock().unwrap() = s.clone();
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(p: Pane) -> Settings {
        Settings {
            dashboards: vec![Dashboard {
                panes: vec![p],
                ..Dashboard::default()
            }],
        }
    }

    #[test]
    fn a_board_and_its_panes_always_come_back_identified() {
        let mut s = Settings {
            dashboards: vec![Dashboard::default(), Dashboard::default()],
        };
        sanitize(&mut s);
        let ids: Vec<_> = s.dashboards.iter().map(|d| d.id.clone()).collect();
        assert!(ids.iter().all(|i| !i.is_empty()));
        assert_ne!(ids[0], ids[1], "two blank boards must not share an id");
        assert_eq!(s.dashboards[0].name, "Dashboard 1");
    }

    #[test]
    fn an_unknown_pane_kind_falls_back_to_dispatch() {
        // The kind reaches the page as a branch; anything not understood must
        // land on a pane that renders, not on a blank column.
        let mut s = one(Pane {
            kind: "sparkline".into(),
            ..Pane::default()
        });
        sanitize(&mut s);
        assert_eq!(s.dashboards[0].panes[0].kind, "dispatch");
    }

    #[test]
    fn an_emphasis_style_is_never_free_text() {
        // `style` becomes a CSS class name on the page.
        let mut s = one(Pane {
            emphasis: vec![
                Emphasis {
                    style: "alarm\" onload=\"".into(),
                    ..Emphasis::default()
                },
                Emphasis {
                    style: "WARN".into(),
                    ..Emphasis::default()
                },
            ],
            ..Pane::default()
        });
        sanitize(&mut s);
        let e = &s.dashboards[0].panes[0].emphasis;
        assert_eq!(e[0].style, "alarm", "an unknown style falls back");
        assert_eq!(e[1].style, "warn", "a known style is just lower-cased");
    }

    #[test]
    fn a_radius_of_zero_drops_the_radius_rather_than_hiding_everything() {
        let mut s = one(Pane {
            within_place: "p-methodist".into(),
            within_miles: 0.0,
            ..Pane::default()
        });
        sanitize(&mut s);
        let p = &s.dashboards[0].panes[0];
        assert_eq!(p.within_miles, 0.0);
        assert_eq!(p.within_place, "", "a radius-less place would match nothing");
    }

    #[test]
    fn the_two_place_filters_stay_independent() {
        // "closest to Methodist" and "within 3 miles of Methodist" are
        // different questions and the listener asked for both at once, so
        // neither may quietly become the other.
        let mut s = one(Pane {
            closest_place: "p-methodist".into(),
            within_place: "p-eskenazi".into(),
            within_miles: 3.0,
            ..Pane::default()
        });
        sanitize(&mut s);
        let p = &s.dashboards[0].panes[0];
        assert_eq!(p.closest_place, "p-methodist");
        assert_eq!(p.within_place, "p-eskenazi");
        assert_eq!(p.within_miles, 3.0);
    }

    #[test]
    fn the_footer_keeps_its_paragraphs_but_is_still_bounded() {
        let mut s = Settings {
            dashboards: vec![Dashboard {
                footer: "CONFIDENTIAL — peer review only.\n\nSecond paragraph.".into(),
                ..Dashboard::default()
            }],
        };
        sanitize(&mut s);
        assert!(s.dashboards[0].footer.contains('\n'), "it is an attestation");
        let mut long = Settings {
            dashboards: vec![Dashboard {
                footer: "x".repeat(9_000),
                ..Dashboard::default()
            }],
        };
        sanitize(&mut long);
        assert!(long.dashboards[0].footer.chars().count() <= 2_000);
    }

    fn shared_one() -> Settings {
        let mut s = Settings {
            dashboards: vec![Dashboard {
                id: "b1".into(),
                name: "Wall".into(),
                shared: true,
                ..Dashboard::default()
            }],
        };
        sanitize(&mut s);
        s
    }

    #[test]
    fn sharing_a_board_mints_a_key_and_saving_it_again_keeps_it() {
        // The link is already in someone's browser by the second save. The
        // editor sends the whole board back on every Save, so a key that did
        // not survive `sanitize` would break the display silently.
        let s = shared_one();
        let key = s.dashboards[0].share_key.clone();
        assert_eq!(key.len(), SHARE_KEY_CHARS);
        assert!(key.chars().all(|c| c.is_ascii_alphanumeric()));
        let mut again = s.clone();
        sanitize(&mut again);
        assert_eq!(again.dashboards[0].share_key, key, "a save must not rotate it");
    }

    #[test]
    fn two_shared_boards_do_not_get_the_same_key() {
        let mut s = Settings {
            dashboards: vec![
                Dashboard { id: "b1".into(), shared: true, ..Dashboard::default() },
                Dashboard { id: "b2".into(), shared: true, ..Dashboard::default() },
            ],
        };
        sanitize(&mut s);
        assert_ne!(s.dashboards[0].share_key, s.dashboards[1].share_key);
    }

    #[test]
    fn un_sharing_forgets_the_key_so_the_old_link_cannot_come_back() {
        let mut s = shared_one();
        let old = s.dashboards[0].share_key.clone();
        s.dashboards[0].shared = false;
        sanitize(&mut s);
        assert_eq!(s.dashboards[0].share_key, "");
        assert!(shared_board(&s, "b1", &old).is_none());
        // Sharing again is a new link, not the old one waking up.
        s.dashboards[0].shared = true;
        sanitize(&mut s);
        assert_ne!(s.dashboards[0].share_key, old);
    }

    #[test]
    fn only_the_right_key_opens_a_shared_board() {
        let s = shared_one();
        let key = s.dashboards[0].share_key.clone();
        assert!(shared_board(&s, "b1", &key).is_some());
        for wrong in ["", "x", &key[..key.len() - 1], &format!("{key}x")] {
            assert!(shared_board(&s, "b1", wrong).is_none(), "opened with {wrong:?}");
        }
        assert!(shared_board(&s, "nosuch", &key).is_none(), "another board's id");
    }

    #[test]
    fn a_board_that_was_never_shared_is_not_reachable_even_with_a_key() {
        let mut s = Settings {
            dashboards: vec![Dashboard {
                id: "b1".into(),
                // As if hand-edited into dashboards.json: a key, but no
                // intention to share.
                share_key: "a".repeat(SHARE_KEY_CHARS),
                shared: false,
                ..Dashboard::default()
            }],
        };
        let key = s.dashboards[0].share_key.clone();
        assert!(shared_board(&s, "b1", &key).is_none(), "before cleaning");
        sanitize(&mut s);
        assert!(shared_board(&s, "b1", &key).is_none(), "after cleaning");
    }

    #[test]
    fn switching_a_board_off_takes_it_off_the_tailnet_too() {
        let mut s = shared_one();
        let key = s.dashboards[0].share_key.clone();
        s.dashboards[0].enabled = false;
        sanitize(&mut s);
        assert!(shared_board(&s, "b1", &key).is_none());
    }

    #[test]
    fn a_drawn_board_never_carries_the_key_that_opens_it() {
        // `RenderedBoard` is what goes out over the wire to the display.
        let s = shared_one();
        let out = render(&s.dashboards[0], &[], &[], &[], 1_000);
        let json = serde_json::to_string(&out).unwrap();
        assert!(!json.contains(&s.dashboards[0].share_key), "{json}");
        assert!(!json.contains("share_key"));
    }

    #[test]
    fn settings_round_trip_through_json() {
        let mut s = one(Pane {
            kind: "reports".into(),
            place: "p-methodist".into(),
            emphasis: vec![Emphasis {
                when: crate::pathways::When {
                    phrases: vec!["working arrest".into()],
                    ..Default::default()
                },
                style: "alarm".into(),
                note: "arrest".into(),
            }],
            ..Pane::default()
        });
        sanitize(&mut s);
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn a_board_written_before_a_limit_existed_still_draws_a_full_pane() {
        // What reaches `render` has been through `sanitize`, because `load`
        // puts it there. Without that, `limit: 0` draws a single row.
        let old = r#"{"dashboards":[{"id":"db1","name":"Wall","panes":[{"id":"p1","kind":"dispatch"}]}]}"#;
        let mut s: Settings = serde_json::from_str(old).unwrap();
        assert_eq!(s.dashboards[0].panes[0].limit, 0, "as it reads off disk");
        sanitize(&mut s);
        assert_eq!(s.dashboards[0].panes[0].limit, 25, "as it is used");
    }

    #[test]
    fn a_file_from_before_a_field_existed_still_loads() {
        // dashboards.json is hand-editable and survives upgrades.
        let old = r#"{"dashboards":[{"id":"db1","name":"Wall","panes":[{"id":"p1","kind":"dispatch"}]}]}"#;
        let s: Settings = serde_json::from_str(old).unwrap();
        assert_eq!(s.dashboards[0].panes[0].limit, 0, "absent means default");
        assert_eq!(s.dashboards[0].refresh_secs, 10);
        assert!(s.dashboards[0].enabled);
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;

    fn place(id: &str, name: &str, lat: f64, lon: f64, tgs: Vec<u16>) -> crate::places::Place {
        crate::places::Place {
            id: id.into(),
            name: name.into(),
            kind: "hospital".into(),
            lat: Some(lat),
            lon: Some(lon),
            tgs,
            enabled: true,
            ..Default::default()
        }
    }

    // Two real hospitals about a mile apart, and a third across town.
    fn places() -> Vec<crate::places::Place> {
        vec![
            place("p-meth", "Methodist", 39.7810, -86.1650, vec![10256]),
            place("p-esk", "Eskenazi", 39.7830, -86.1780, vec![10257]),
            place("p-far", "South", 39.7000, -86.1600, vec![]),
        ]
    }

    fn run(id: i64, call_type: &str, lat: f64, lon: f64) -> crate::dispatch::Incident {
        crate::dispatch::Incident {
            id,
            created: 100,
            updated: 100 + id,
            tg: 1001,
            tg_name: "Dispatch".into(),
            call_type: call_type.into(),
            emoji: "🫀".into(),
            address: "1 Example Way".into(),
            validated: String::new(),
            lat: Some(lat),
            lon: Some(lon),
            geocode: "ok".into(),
            units: vec!["Medic 21".into()],
            summary: String::new(),
            confidence: 90,
            calls: 1,
            revision: 0,
            pathway: String::new(),
            targets: Vec::new(),
        }
    }

    fn board(panes: Vec<Pane>) -> Dashboard {
        Dashboard {
            id: "b1".into(),
            name: "Wall".into(),
            panes,
            ..Dashboard::default()
        }
    }

    fn drawn(b: &Dashboard, incs: &[crate::dispatch::Incident]) -> Vec<i64> {
        render(b, incs, &[], &places(), 1_000).panes[0]
            .cards
            .iter()
            .map(|c| c.id)
            .collect()
    }

    #[test]
    fn a_pane_keeps_only_the_runs_whose_nearest_hospital_is_the_one_named() {
        // "Whose patch is this" — a rank, not a radius.
        let near_meth = run(1, "Chest Pain", 39.7805, -86.1640);
        let near_esk = run(2, "Chest Pain", 39.7835, -86.1790);
        let p = Pane {
            limit: 25,
            closest_place: "p-meth".into(),
            ..Pane::default()
        };
        assert_eq!(drawn(&board(vec![p]), &[near_meth, near_esk]), vec![1]);
    }

    #[test]
    fn a_radius_and_a_rank_ask_different_questions() {
        // A run can be closest to Methodist and still be miles away, and a run
        // on Methodist's doorstep can belong to Eskenazi. The listener asked
        // for both at once, so neither may stand in for the other.
        // Closest hospital is Methodist at 1.45 mi; Eskenazi is 1.73 mi away.
        let far = run(1, "Chest Pain", 39.7600, -86.1650);
        let p_rank = Pane {
            limit: 25,
            closest_place: "p-meth".into(),
            ..Pane::default()
        };
        let p_near = Pane {
            limit: 25,
            within_place: "p-meth".into(),
            within_miles: 1.0,
            ..Pane::default()
        };
        assert_eq!(drawn(&board(vec![p_rank]), std::slice::from_ref(&far)), vec![1]);
        assert!(
            drawn(&board(vec![p_near]), std::slice::from_ref(&far)).is_empty(),
            "3 miles out is not within one mile"
        );
    }

    #[test]
    fn a_call_type_is_matched_whole_and_without_regard_to_case() {
        let i = run(1, "Cardiac Arrest", 39.7805, -86.1640);
        let p = Pane {
            limit: 25,
            call_types: vec!["cardiac arrest".into()],
            ..Pane::default()
        };
        assert_eq!(drawn(&board(vec![p]), std::slice::from_ref(&i)), vec![1]);
        let p = Pane {
            limit: 25,
            call_types: vec!["Cardiac".into()],
            ..Pane::default()
        };
        assert!(
            drawn(&board(vec![p]), std::slice::from_ref(&i)).is_empty(),
            "half a type is not the type"
        );
    }

    #[test]
    fn an_exception_refuses_a_run_whatever_else_it_matches() {
        let mut i = run(1, "Cardiac Arrest", 39.7805, -86.1640);
        i.summary = "History of cardiac arrest, patient alert".into();
        let p = Pane {
            limit: 25,
            call_types: vec!["Cardiac Arrest".into()],
            except: vec!["history of".into()],
            ..Pane::default()
        };
        assert!(drawn(&board(vec![p]), &[i]).is_empty());
    }

    #[test]
    fn the_newest_run_is_at_the_top_and_the_limit_cuts_the_tail() {
        let runs: Vec<_> = (1..=5).map(|n| run(n, "Chest Pain", 39.7805, -86.1640)).collect();
        let p = Pane {
            limit: 3,
            ..Pane::default()
        };
        let b = board(vec![p]);
        assert_eq!(drawn(&b, &runs), vec![5, 4, 3], "latest first");
        assert_eq!(
            render(&b, &runs, &[], &places(), 1_000).panes[0].total,
            5,
            "the rest still matched, they are just not drawn"
        );
    }

    #[test]
    fn the_first_emphasis_that_fires_is_the_one_that_shows() {
        // Loudest at the top of the list is the whole ordering convention.
        let mut i = run(1, "Cardiac Arrest", 39.7805, -86.1640);
        i.summary = "working arrest, CPR in progress".into();
        let em = |style: &str, phrase: &str| Emphasis {
            when: crate::pathways::When {
                phrases: vec![phrase.into()],
                ..Default::default()
            },
            style: style.into(),
            note: style.into(),
        };
        let p = Pane {
            limit: 25,
            emphasis: vec![em("alarm", "working arrest"), em("warn", "cpr in progress")],
            ..Pane::default()
        };
        let out = render(&board(vec![p]), &[i], &[], &places(), 1_000);
        assert_eq!(out.panes[0].cards[0].style, "alarm");
        assert_eq!(out.panes[0].cards[0].note, "alarm");
    }

    #[test]
    fn an_emphasis_exception_beats_its_own_match() {
        let mut i = run(1, "Fire Alarm", 39.7805, -86.1640);
        i.summary = "medical alarm".into();
        let p = Pane {
            limit: 25,
            emphasis: vec![Emphasis {
                when: crate::pathways::When {
                    phrases: vec!["alarm".into()],
                    except_types: vec!["Fire Alarm".into()],
                    ..Default::default()
                },
                style: "alarm".into(),
                note: String::new(),
            }],
            ..Pane::default()
        };
        let out = render(&board(vec![p]), &[i], &[], &places(), 1_000);
        assert_eq!(out.panes[0].cards[0].style, "", "the type ruled it out");
    }

    #[test]
    fn drive_time_shows_only_where_a_road_route_was_actually_measured() {
        let mut i = run(1, "Chest Pain", 39.7805, -86.1640);
        i.targets = vec![crate::pathways::Target {
            label: "Closest hospital".into(),
            place_id: "p-meth".into(),
            place_name: "Methodist".into(),
            lat: 39.7810,
            lon: -86.1650,
            meters: 800.0,
            secs: 480.0,
            how: "road".into(),
        }];
        let p = Pane {
            limit: 25,
            closest_place: "p-meth".into(),
            ..Pane::default()
        };
        let out = render(&board(vec![p]), std::slice::from_ref(&i), &[], &places(), 1_000);
        assert!(out.panes[0].cards[0].meta.iter().any(|m| m == "8 min"));

        i.targets[0].how = "straight".into();
        let out = render(&board(vec![p_of(&out)]), &[i], &[], &places(), 1_000);
        assert!(
            !out.panes[0].cards[0].meta.iter().any(|m| m.ends_with(" min")),
            "a straight line is a distance, not a drive"
        );
        assert!(out.panes[0].cards[0].meta.iter().any(|m| m.ends_with(" mi")));
    }

    // The pane used above, rebuilt — `render` borrows it, so it cannot be
    // moved into the second call.
    fn p_of(_prev: &RenderedBoard) -> Pane {
        Pane {
            limit: 25,
            closest_place: "p-meth".into(),
            ..Pane::default()
        }
    }

    #[test]
    fn a_reports_pane_follows_the_places_talkgroups_and_carries_the_eta() {
        let stored = |id: i64, tg: u16, summary: &str| crate::conversations::Stored {
            id,
            tg,
            tg_name: "Med 03".into(),
            last_at: 100 + id,
            headline: "Chest Pain".into(),
            summary: summary.into(),
            units: vec!["Medic 21".into()],
            eta: crate::conversations::eta_phrase(summary),
            ..Default::default()
        };
        let rows = vec![
            stored(1, 10256, "Inbound. The ETA is 15 minutes."),
            stored(2, 10257, "Different hospital's talkgroup."),
        ];
        let p = Pane {
            kind: "reports".into(),
            limit: 25,
            place: "p-meth".into(),
            ..Pane::default()
        };
        let out = render(&board(vec![p]), &[], &rows, &places(), 1_000);
        let cards = &out.panes[0].cards;
        assert_eq!(cards.len(), 1, "only Methodist's talkgroup");
        assert_eq!(cards[0].id, 1);
        assert_eq!(cards[0].eta.as_deref(), Some("ETA is 15 minutes"));
        assert_eq!(out.panes[0].sub, "Methodist");
    }

    #[test]
    fn a_run_with_no_position_cannot_satisfy_a_place_filter() {
        let mut i = run(1, "Chest Pain", 0.0, 0.0);
        i.lat = None;
        i.lon = None;
        let p = Pane {
            limit: 25,
            closest_place: "p-meth".into(),
            ..Pane::default()
        };
        assert!(drawn(&board(vec![p]), std::slice::from_ref(&i)).is_empty());
        // ...but with no place filter it is still a run worth showing.
        let p = Pane {
            limit: 25,
            ..Pane::default()
        };
        assert_eq!(drawn(&board(vec![p]), &[i]), vec![1]);
    }
}
