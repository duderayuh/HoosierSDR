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
//! Panes are filtered here in Rust only as far as the stored settings go; the
//! matching itself is done on the page against incidents it already holds, so
//! a board re-renders on a `incident` event without a round trip.

use serde::{Deserialize, Serialize};

const MAX_DASHBOARDS: usize = 40;
const MAX_PANES: usize = 8;
const MAX_EMPHASIS: usize = 12;

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
    Ok(d.join("dashboards.json"))
}

pub fn load(app: &tauri::AppHandle) -> Settings {
    path(app)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
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
    fn a_file_from_before_a_field_existed_still_loads() {
        // dashboards.json is hand-editable and survives upgrades.
        let old = r#"{"dashboards":[{"id":"db1","name":"Wall","panes":[{"id":"p1","kind":"dispatch"}]}]}"#;
        let s: Settings = serde_json::from_str(old).unwrap();
        assert_eq!(s.dashboards[0].panes[0].limit, 0, "absent means default");
        assert_eq!(s.dashboards[0].refresh_secs, 10);
        assert!(s.dashboards[0].enabled);
    }
}
