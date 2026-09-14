//! A case: one patient's run, as a timeline of what the radio said.
//!
//! The dispatch map already turns pages into runs and joins a crew's
//! hospital report to the run it came from. What it cannot say is what
//! happened in between: that a run dispatched as an unconscious person became
//! a working arrest at 06:12, got pulses back at 06:23 and lost them again at
//! 06:32. Those moments are said on the air, and this module keeps them.
//!
//! A *profile* says which runs are cases and which words are events. Cardiac
//! arrest is the first; stroke or trauma is another profile, not more code.
//!
//! Where an event comes from, in the order it is trusted:
//!
//!   - **The page.** The automated dispatch voice repages a run as it grows,
//!     with the address, so "…, Cardiac Arrest Working, 0629 Hours" is joined
//!     to its run by the dispatch map before anything here reads it.
//!   - **The dispatcher's readback.** A crew's status on an ops channel is
//!     read back by a console within seconds, short and with the logged time:
//!     "Working Arrest 1748", "rosc 1914", "Ceasing efforts 2326". A known
//!     speaker and a fixed shape, so it is an event, and its time is the one
//!     the dispatch centre wrote down.
//!   - **The crew.** "This is going to be a DOA" sometimes gets no readback.
//!     A crew's statement is an event only when it is a statement — not "can
//!     you add us to that cardiac arrest", not "any working arrests?" — and
//!     only when something other than the clock says which run it is about.
//!   - **The hospital report**, joined to the run by `link.rs`, with its
//!     stated time to arrival.
//!
//! Which run an ops-channel event belongs to: the call the dispatch map
//! already attached, then a callsign said, then what the speaking radio is
//! learned to be (`radios.rs`), then — for the readback alone — the only
//! case open at that moment, marked as inferred. Anything else is kept as
//! heard but not placed, never guessed.
//!
//! Rules only; no model is asked. Cases and events are rebuilt from the
//! library over a window, so a better rule, a newly learned radio or a
//! corrected transcript changes the timeline the next time it is built.

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{AppHandle, Manager, State};

use crate::AppState;

/// How far back a live rebuild reaches.
pub const LIVE_WINDOW_SECS: i64 = 6 * 3600;
/// A readback is paired with the crew call it answers inside this.
pub const PAIR_SECS: i64 = 20;
/// A case is open to ops-channel events from a little before its dispatch
/// until this long after, unless it ended.
pub const OPEN_SECS: i64 = 90 * 60;
/// A page that forked a run (a mis-heard house number) is the same case when
/// it names the same street within this.
pub const FORK_SECS: i64 = 20 * 60;
/// A readback is placed on the only open arrest only when that arrest was
/// dispatched this recently; an arrest from an hour ago has usually ended
/// without anyone saying so on the air.
pub const INFER_SECS: i64 = 45 * 60;
/// A readback is at most this many words, or this many without a time.
const READBACK_WORDS: usize = 9;
const READBACK_WORDS_UNTIMED: usize = 6;

// ---------------------------------------------------------------------------
// profiles
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct EventRule {
    /// working | downgrade | rosc | rearrest | terminated | transporting
    pub kind: String,
    /// How the timeline names it.
    pub label: String,
    /// Said by anyone.
    pub phrases: Vec<String>,
    /// Only believed in a dispatcher's readback, where the shape carries the
    /// meaning ("Working 803").
    pub readback: Vec<String>,
}

impl Default for EventRule {
    fn default() -> Self {
        EventRule { kind: String::new(), label: String::new(), phrases: Vec::new(), readback: Vec::new() }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    /// Dispatch call types that make a run a case.
    pub call_types: Vec<String>,
    /// Words on a page that make a run a case, whatever type the model filed.
    pub page_phrases: Vec<String>,
    /// Checked in order; the first that matches names the event.
    pub events: Vec<EventRule>,
}

impl Default for Profile {
    fn default() -> Self {
        Profile {
            id: String::new(),
            name: String::new(),
            enabled: true,
            call_types: Vec::new(),
            page_phrases: Vec::new(),
            events: Vec::new(),
        }
    }
}

fn rule(kind: &str, label: &str, phrases: &[&str], readback: &[&str]) -> EventRule {
    EventRule {
        kind: kind.into(),
        label: label.into(),
        phrases: phrases.iter().map(|s| s.to_string()).collect(),
        readback: readback.iter().map(|s| s.to_string()).collect(),
    }
}

/// The cardiac arrest profile. The order matters: "not a cardiac arrest"
/// contains "arrest", so the downgrade is tried before the upgrade.
pub fn arrest_profile() -> Profile {
    Profile {
        id: "cardiac-arrest".into(),
        name: "Cardiac arrest".into(),
        enabled: true,
        call_types: vec!["Cardiac Arrest".into()],
        page_phrases: vec!["cardiac arrest".into()],
        events: vec![
            rule(
                "downgrade",
                "Not a cardiac arrest",
                &["not a cardiac arrest", "not an arrest", "not in arrest", "not under arrest", "instead of cardiac arrest", "instead of a cardiac arrest"],
                &[],
            ),
            rule(
                "terminated",
                "Efforts ceased",
                &["ceasing efforts", "ceased efforts", "cease efforts", "ceasing effort", "terminating efforts", "termination of efforts", "doa", "pronounced"],
                &["ceasing"],
            ),
            rule(
                "rearrest",
                "Lost pulses",
                &["lost pulses", "lost pulse", "lost the pulse", "rearrest", "re arrest", "coding again", "back in arrest", "arrested again"],
                &[],
            ),
            rule("rosc", "ROSC", &["rosc", "we have pulses", "got pulses", "got a pulse", "pulses back", "we have a pulse"], &["rosc"]),
            rule("transporting", "Transporting", &["transporting emergent", "we are transporting", "transporting to"], &["transporting"]),
            rule(
                "working",
                "Working arrest",
                &["working arrest", "working cardiac arrest", "cardiac arrest working", "working code", "working traumatic arrest"],
                &["working", "arrest"],
            ),
        ],
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Settings {
    pub profiles: Vec<Profile>,
}

fn settings_path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let d = app.path().app_config_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    Ok(d.join("cases.json"))
}

/// The profiles, seeded with cardiac arrest the first time.
pub fn load(app: &AppHandle) -> Settings {
    let path = settings_path(app).ok();
    let read = path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str::<Settings>(&t).ok());
    match read {
        Some(s) => s,
        None => {
            let s = Settings { profiles: vec![arrest_profile()] };
            if let (Some(p), Ok(t)) = (path, serde_json::to_string_pretty(&s)) {
                let _ = std::fs::write(p, t);
            }
            s
        }
    }
}

// ---------------------------------------------------------------------------
// reading words
// ---------------------------------------------------------------------------

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
        .collect()
}

/// One word heard as another. Short words must match exactly: "doa" is not
/// "do", and "rosc" is not "rock".
fn same_word(heard: &str, want: &str) -> bool {
    if want.len() <= 4 || heard.len() <= 4 {
        return heard == want;
    }
    // Misheard, not a different word: "arrest" is not "rearrest".
    heard.chars().next() == want.chars().next()
        && heard.len().abs_diff(want.len()) <= 2
        && crate::fuzzy::alike(heard, want)
}

/// Where `phrase` is said in `w`, allowing one extra word between its words
/// ("working traumatic arrest" is a working arrest).
pub fn phrase_at(w: &[String], phrase: &str) -> Option<usize> {
    let p = words(phrase);
    if p.is_empty() {
        return None;
    }
    'start: for i in 0..w.len() {
        if !same_word(&w[i], &p[0]) {
            continue;
        }
        let mut j = i + 1;
        for want in &p[1..] {
            if w.get(j).is_some_and(|x| same_word(x, want)) {
                j += 1;
            } else if w.get(j + 1).is_some_and(|x| same_word(x, want)) {
                j += 2;
            } else {
                continue 'start;
            }
        }
        return Some(i);
    }
    None
}

const ASKING: &[&str] = &["can", "could", "would", "will", "any", "is", "was", "what", "did", "do", "are", "where", "who", "how", "please", "should"];

/// A question or a request about an arrest, which is not the arrest
/// happening: "can you add us to that cardiac arrest", "any working arrests?"
pub fn is_request(text: &str) -> bool {
    if text.contains('?') {
        return true;
    }
    let w = words(text);
    if w.first().is_some_and(|f| ASKING.contains(&f.as_str())) {
        return true;
    }
    let joined = w.join(" ");
    ["add us", "that cardiac arrest", "that arrest", "the cardiac arrest with", "transporting unit", "transport unit", "second transport"]
        .iter()
        .any(|p| joined.contains(p))
}

/// The time a dispatcher reads out at the end of a readback, as minutes after
/// midnight: "1748", "716", "12-04". Checked against the call's own clock, so
/// a mis-heard "663" or a unit number is not taken for a time.
pub fn spoken_clock(text: &str, call_minute: i64) -> Option<i64> {
    let w = words(text);
    let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    let minutes = match w.as_slice() {
        [.., h, m] if digits(h) && digits(m) && h.len() <= 2 && m.len() == 2 => {
            h.parse::<i64>().ok()? * 60 + m.parse::<i64>().ok()?
        }
        [.., hm] if digits(hm) && (3..=4).contains(&hm.len()) => {
            let n: i64 = hm.parse().ok()?;
            let (h, m) = (n / 100, n % 100);
            if m > 59 || h > 23 {
                return None;
            }
            h * 60 + m
        }
        _ => return None,
    };
    let off = (minutes - call_minute).rem_euclid(24 * 60);
    (off <= 15 || off >= 24 * 60 - 15).then_some(minutes)
}

fn ends_with_number(text: &str) -> bool {
    words(text).last().is_some_and(|l| l.chars().all(|c| c.is_ascii_digit()))
}

/// How an event was heard.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum Source {
    Page,
    Readback,
    Crew,
    Report,
}

impl Source {
    pub fn as_str(&self) -> &'static str {
        match self {
            Source::Page => "page",
            Source::Readback => "readback",
            Source::Crew => "crew",
            Source::Report => "report",
        }
    }
}

/// What one ops-channel transmission says, if it is an event.
#[derive(Clone, Debug, PartialEq)]
pub struct Heard {
    pub kind: String,
    pub label: String,
    pub source: Source,
    /// The dispatcher's time, minutes after midnight.
    pub clock: Option<i64>,
}

/// Read one transmission on an ops channel.
pub fn classify(text: &str, console: bool, call_minute: i64, p: &Profile) -> Option<Heard> {
    if is_request(text) {
        return None;
    }
    let w = words(text);
    let clock = spoken_clock(text, call_minute);
    let timed = clock.is_some() || ends_with_number(text);
    let readback_shape = console && (w.len() <= READBACK_WORDS && timed || w.len() <= READBACK_WORDS_UNTIMED);
    if console && !readback_shape {
        // A console saying a long sentence is relaying or asking, not
        // logging a status.
        return None;
    }
    for r in &p.events {
        let said = r.phrases.iter().any(|ph| phrase_at(&w, ph).is_some());
        // A readback word carries its meaning by the shape alone, so it must
        // be the word: "clearing" is not "ceasing".
        let shape = readback_shape && r.readback.iter().any(|ph| w.iter().any(|x| x == ph));
        if said || shape {
            return Some(Heard {
                kind: r.kind.clone(),
                label: r.label.clone(),
                source: if console { Source::Readback } else { Source::Crew },
                clock: if console { clock } else { None },
            });
        }
    }
    None
}

// ---------------------------------------------------------------------------
// pages
// ---------------------------------------------------------------------------

/// One page of a run, as the dispatch channel said it.
#[derive(Clone, Debug)]
pub struct Page {
    pub call: i64,
    pub at: i64,
    pub minute: i64,
    pub text: String,
}

/// Is this run a case for the profile. The page decides when there is one:
/// the dispatch model filed "Chest Pain/Heart", transcribed as "Tesspain
/// Heart", as a cardiac arrest, and the page itself never said arrest. The
/// type decides only for a run with no page transcript.
pub fn is_case(call_type: &str, pages: &[Page], p: &Profile) -> bool {
    if pages.is_empty() {
        return p.call_types.iter().any(|t| t.eq_ignore_ascii_case(call_type));
    }
    pages.iter().any(|pg| p.page_phrases.iter().any(|ph| phrase_at(&words(&pg.text), ph).is_some()))
}

/// The call type a page reads out, from the configured types and the
/// profile's own words: "Cardiac Arrest Working" comes back as that, not as
/// the "Cardiac Arrest" the dispatch map filed it under.
fn page_says_working(text: &str, p: &Profile) -> bool {
    let w = words(text);
    p.events
        .iter()
        .filter(|r| r.kind == "working")
        .any(|r| r.phrases.iter().any(|ph| phrase_at(&w, ph).is_some()))
}

/// "0629 Hours" on a page.
pub fn page_clock(text: &str, call_minute: i64) -> Option<i64> {
    let w = words(text);
    let i = w.iter().position(|x| x == "hours")?;
    let before = &w[..i];
    spoken_clock(&before.join(" "), call_minute)
}

/// The timeline lines a run's pages make: the first is the dispatch, a later
/// one that says the run is working is the upgrade, and the rest are repages.
/// `continuing` is a run the dispatch map split off an earlier one, whose
/// first page is a repage, not the dispatch.
pub fn page_lines(call_type: &str, pages: &[Page], p: &Profile, continuing: bool) -> Vec<Line> {
    let mut out = Vec::new();
    let mut working = false;
    for (i, pg) in pages.iter().enumerate() {
        let clock = page_clock(&pg.text, pg.minute);
        let says_working = page_says_working(&pg.text, p);
        let (kind, label) = if i == 0 && !continuing {
            working = says_working;
            (
                "dispatched",
                if says_working { format!("Dispatched as a working arrest") } else { format!("Dispatched as {call_type}") },
            )
        } else if says_working && !working {
            working = true;
            ("working", "Repaged as a working arrest".to_string())
        } else {
            ("repage", "Repaged".to_string())
        };
        out.push(Line {
            at: pg.at,
            clock: clock.map(hhmm),
            kind: kind.into(),
            label,
            source: Source::Page.as_str().into(),
            how: String::new(),
            inferred: false,
            call: Some(pg.call),
            conversation: None,
            detail: pg.text.clone(),
        });
    }
    out
}

fn hhmm(minutes: i64) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

// ---------------------------------------------------------------------------
// ops-channel events and which run they belong to
// ---------------------------------------------------------------------------

/// One transmission on an ops channel, as the rules need it.
#[derive(Clone, Debug)]
pub struct OpsCall {
    pub call: i64,
    pub tg: u16,
    pub radio: u32,
    pub at: i64,
    pub minute: i64,
    pub text: String,
    /// The run the dispatch map attached this call to, if any.
    pub incident: Option<i64>,
    /// What the radio is learned to be, when it is a usable unit.
    pub learned: Option<String>,
}

/// An event heard on an ops channel, with the call before it that it answers.
#[derive(Clone, Debug)]
pub struct OpsEvent {
    pub heard: Heard,
    pub call: OpsCall,
    /// The crew call a readback answers.
    pub answers: Option<OpsCall>,
}

/// Find the events in a run of ops calls, oldest first. A readback and the
/// crew statement it repeats are one event: the readback's words and time,
/// the crew's call as what it answers.
pub fn ops_events(calls: &[OpsCall], consoles: &HashSet<u32>, p: &Profile) -> Vec<OpsEvent> {
    let mut out: Vec<OpsEvent> = Vec::new();
    for (i, c) in calls.iter().enumerate() {
        let console = consoles.contains(&c.radio);
        let Some(heard) = classify(&c.text, console, c.minute, p) else {
            continue;
        };
        if heard.source == Source::Readback {
            // The crew call it answers: the last non-console call on the same
            // channel just before it.
            let answers = calls[..i]
                .iter()
                .rev()
                .take_while(|k| c.at - k.at <= PAIR_SECS)
                .find(|k| k.tg == c.tg && !consoles.contains(&k.radio))
                .cloned();
            // The crew's own statement of the same thing becomes this.
            if let Some(a) = &answers {
                out.retain(|e| !(e.call.call == a.call && e.heard.kind == heard.kind));
            }
            out.push(OpsEvent { heard, call: c.clone(), answers });
        } else {
            // A crew statement already read back is not said twice.
            let read_back = out.iter().any(|e| {
                e.heard.source == Source::Readback
                    && e.heard.kind == heard.kind
                    && e.answers.as_ref().is_some_and(|a| a.call == c.call)
            });
            if !read_back {
                out.push(OpsEvent { heard, call: c.clone(), answers: None });
            }
        }
    }
    out
}

/// A run a case is about, as placement sees it.
#[derive(Clone, Debug)]
pub struct Run {
    pub incident: i64,
    pub at: i64,
    pub units: Vec<String>,
    /// The case this run already belongs to.
    pub case: Option<usize>,
    /// When a page of this run said it was working.
    pub upgraded: Vec<i64>,
}

/// A readback of an upgrade this close to a page saying the same is that
/// page's run.
pub const PAGE_AGREES_SECS: i64 = 3 * 60;

/// Where an event was placed, and why.
#[derive(Clone, Debug, PartialEq)]
pub enum Placed {
    /// On this run, for this reason.
    Run { incident: i64, how: String, inferred: bool },
    Nowhere(String),
}

/// Which run an ops event is about.
///
/// `runs` are every run dispatched around the event, cases or not; `open`
/// says which of them are open cases of this profile at the event's time.
pub fn place(e: &OpsEvent, runs: &[Run], open: &dyn Fn(&Run) -> bool, vocab: &HashSet<String>) -> Placed {
    let crew = e.answers.as_ref().unwrap_or(&e.call);
    let near = |r: &&Run| r.at <= e.call.at + 5 * 60 && e.call.at - r.at <= OPEN_SECS;

    // The dispatch map attached the call to a run.
    for c in [&e.call, crew] {
        if let Some(id) = c.incident {
            if runs.iter().any(|r| r.incident == id) {
                return Placed::Run { incident: id, how: "attached to this run by the dispatch map".into(), inferred: false };
            }
        }
    }
    // An upgrade read back within a few minutes of an open arrest's own page
    // saying it is working. Two sources agreeing on the moment outrank a
    // callsign: a page transcribed "End in 36" loses Engine 36 from the run,
    // and the callsign then points at the last run it was sent to instead.
    if e.heard.kind == "working" {
        let agrees: Vec<&Run> = runs
            .iter()
            .filter(|r| open(r) && r.upgraded.iter().any(|t| (t - e.call.at).abs() <= PAGE_AGREES_SECS))
            .collect();
        let cases: HashSet<usize> = agrees.iter().filter_map(|r| r.case).collect();
        if cases.len() == 1 {
            return Placed::Run {
                incident: agrees[0].incident,
                how: "this run's page said it was working within minutes".into(),
                inferred: false,
            };
        }
    }
    // A callsign said, then what the crew's radio is learned to be.
    let mut signs: Vec<(String, &str)> = Vec::new();
    for c in [&e.call, crew] {
        for s in crate::link::callsigns(&c.text, vocab) {
            signs.push((s, "said"));
        }
    }
    if let Some(l) = &crew.learned {
        signs.push((l.clone(), "learned"));
    }
    for (sign, how) in &signs {
        let mut with: Vec<&Run> = runs
            .iter()
            .filter(near)
            .filter(|r| r.units.iter().any(|u| u.eq_ignore_ascii_case(sign)))
            .collect();
        if with.is_empty() {
            continue;
        }
        // The most recent run that unit was sent to, preferring an open case.
        with.sort_by_key(|r| (open(r), r.at));
        let r = with.last().unwrap();
        let why = if *how == "said" {
            format!("{sign} was named, and was sent to this run")
        } else {
            format!("the radio is learned as {sign}, who was sent to this run")
        };
        return Placed::Run { incident: r.incident, how: why, inferred: false };
    }
    // The readback alone: the only case open at the time.
    if e.heard.source == Source::Readback {
        let open_now: Vec<&Run> = runs
            .iter()
            .filter(|r| r.at <= e.call.at + 5 * 60 && e.call.at - r.at <= INFER_SECS)
            .filter(|r| open(r))
            .collect();
        let cases: HashSet<usize> = open_now.iter().filter_map(|r| r.case).collect();
        if cases.len() == 1 {
            let r = open_now.iter().max_by_key(|r| r.at).unwrap();
            return Placed::Run {
                incident: r.incident,
                how: "the only arrest open at the time; nothing on the air named the run".into(),
                inferred: true,
            };
        }
        return Placed::Nowhere(if cases.is_empty() {
            "no arrest was open, and nothing named the run".into()
        } else {
            format!("{} arrests were open, and nothing named the run", cases.len())
        });
    }
    Placed::Nowhere("a crew statement that named no run".into())
}

// ---------------------------------------------------------------------------
// the timeline
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Line {
    pub at: i64,
    /// The dispatcher's own time, when it was read out.
    pub clock: Option<String>,
    pub kind: String,
    pub label: String,
    /// page | readback | crew | report
    pub source: String,
    pub how: String,
    pub inferred: bool,
    pub call: Option<i64>,
    pub conversation: Option<i64>,
    pub detail: String,
}

/// Where a case stands, from its lines.
pub fn state_of(lines: &[Line]) -> &'static str {
    let mut st = "dispatched";
    for l in lines {
        st = match l.kind.as_str() {
            "working" => "working",
            "rosc" => "rosc",
            "rearrest" => "working",
            "transporting" => "transporting",
            "report" if !matches!(st, "terminated" | "downgraded") => "reported",
            "terminated" => "terminated",
            "downgrade" => "downgraded",
            _ => st,
        };
    }
    st
}

/// Minutes a stated time to arrival names: "5 to 7 minutes" → (5, 7),
/// "ten minutes" → (10, 10). Seconds and hours are not an ETA a board wants.
pub fn eta_minutes(phrase: &str) -> Option<(i64, i64)> {
    const WORDS: &[(&str, i64)] = &[
        ("one", 1), ("two", 2), ("three", 3), ("four", 4), ("five", 5), ("six", 6), ("seven", 7), ("eight", 8), ("nine", 9),
        ("ten", 10), ("eleven", 11), ("twelve", 12), ("thirteen", 13), ("fourteen", 14), ("fifteen", 15), ("sixteen", 16),
        ("seventeen", 17), ("eighteen", 18), ("nineteen", 19), ("twenty", 20), ("thirty", 30), ("forty", 40), ("fifty", 50),
    ];
    let w = words(phrase);
    if w.iter().any(|x| x.starts_with("hour") || x.starts_with("second")) {
        return None;
    }
    let nums: Vec<i64> = w
        .iter()
        .filter_map(|x| x.parse::<i64>().ok().or_else(|| WORDS.iter().find(|(n, _)| n == x).map(|(_, v)| *v)))
        .filter(|n| (1..=120).contains(n))
        .collect();
    match nums.as_slice() {
        [] => None,
        [n] => Some((*n, *n)),
        [a, b, ..] => Some(((*a).min(*b), (*a).max(*b))),
    }
}

// ---------------------------------------------------------------------------
// the library side
// ---------------------------------------------------------------------------

pub fn ensure_schema(c: &Connection) {
    let _ = c.execute_batch(
        "CREATE TABLE IF NOT EXISTS cases (
            id INTEGER PRIMARY KEY,
            profile TEXT NOT NULL,
            incident INTEGER NOT NULL,
            opened INTEGER NOT NULL,
            UNIQUE (profile, incident)
         );
         CREATE TABLE IF NOT EXISTS case_incidents (
            case_id INTEGER NOT NULL,
            incident INTEGER NOT NULL,
            how TEXT NOT NULL DEFAULT '',
            PRIMARY KEY (case_id, incident)
         );
         CREATE INDEX IF NOT EXISTS case_incidents_incident ON case_incidents(incident);
         CREATE TABLE IF NOT EXISTS case_events (
            id INTEGER PRIMARY KEY,
            case_id INTEGER,
            profile TEXT NOT NULL DEFAULT '',
            at INTEGER NOT NULL,
            clock TEXT,
            kind TEXT NOT NULL,
            label TEXT NOT NULL,
            source TEXT NOT NULL,
            how TEXT NOT NULL DEFAULT '',
            inferred INTEGER NOT NULL DEFAULT 0,
            call INTEGER,
            answers INTEGER,
            detail TEXT NOT NULL DEFAULT ''
         );
         CREATE INDEX IF NOT EXISTS case_events_case ON case_events(case_id, at);
         CREATE INDEX IF NOT EXISTS case_events_at ON case_events(at);",
    );
}

fn local_minute(epoch: i64) -> i64 {
    use chrono::{TimeZone, Timelike};
    match chrono::Local.timestamp_opt(epoch, 0) {
        chrono::LocalResult::Single(t) | chrono::LocalResult::Ambiguous(t, _) => (t.hour() * 60 + t.minute()) as i64,
        _ => 0,
    }
}

/// What a rebuild did.
#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub struct Built {
    pub cases: usize,
    pub events: usize,
    pub inferred: usize,
    pub unplaced: usize,
}

/// Everything a rebuild needs that is not in the library.
pub struct Inputs<'a> {
    pub profile: &'a Profile,
    pub tactical_tgs: &'a HashSet<u16>,
}

struct IncRow {
    id: i64,
    created: i64,
    call_type: String,
    address_key: String,
    units: Vec<String>,
}

fn street(key: &str) -> String {
    key.split_whitespace().filter(|w| !w.chars().all(|c| c.is_ascii_digit())).collect::<Vec<_>>().join(" ")
}

/// Rebuild the cases of one profile for the runs dispatched in
/// `[from, to]` and the ops traffic in the same window.
pub fn rebuild(c: &Connection, inp: &Inputs, from: i64, to: i64) -> Result<Built, String> {
    let p = inp.profile;
    let vocab = crate::link::vocabulary(c);
    let consoles = crate::radios::consoles(c);

    // Runs dispatched around the window.
    let runs_rows: Vec<IncRow> = {
        let mut q = c
            .prepare("SELECT id, created, call_type, address_key, units FROM incidents WHERE created BETWEEN ?1 AND ?2 ORDER BY created, id")
            .map_err(|e| e.to_string())?;
        let rows = q
            .query_map(params![from - OPEN_SECS, to], |r| {
                Ok(IncRow {
                    id: r.get(0)?,
                    created: r.get(1)?,
                    call_type: r.get(2)?,
                    address_key: r.get(3)?,
                    units: serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or_default(),
                })
            })
            .map_err(|e| e.to_string())?;
        rows.flatten().collect()
    };
    let pages_for = |incident: i64| -> Vec<Page> {
        let Ok(mut q) = c.prepare(
            "SELECT k.id, k.start, COALESCE(k.transcript_edited, k.transcript, '') FROM incident_calls ic
               JOIN calls k ON k.id = ic.call
              WHERE ic.incident = ?1 AND ic.role = 'dispatch' ORDER BY k.start, k.id",
        ) else {
            return Vec::new();
        };
        q.query_map([incident], |r| {
            let at: i64 = r.get(1)?;
            Ok(Page { call: r.get(0)?, at, minute: local_minute(at), text: r.get(2)? })
        })
        .map(|rows| rows.flatten().filter(|p| !p.text.trim().is_empty()).collect())
        .unwrap_or_default()
    };

    // Runs on the same street inside a few minutes are one run the dispatch
    // map split in two, usually because a repage was transcribed without its
    // house number. Group them first; a group is a case when any run in it
    // is, and its first run is the dispatch.
    let mut group: HashMap<i64, i64> = HashMap::new(); // incident -> first incident of its group
    let mut pages: HashMap<i64, Vec<Page>> = HashMap::new();
    let mut is_case_run: HashSet<i64> = HashSet::new();
    let house = |key: &str| key.split_whitespace().next().filter(|w| w.chars().all(|c| c.is_ascii_digit())).map(str::to_string);
    for (i, r) in runs_rows.iter().enumerate() {
        let pg = pages_for(r.id);
        if is_case(&r.call_type, &pg, p) {
            is_case_run.insert(r.id);
        }
        pages.insert(r.id, pg);
        let s = street(&r.address_key);
        let joins = runs_rows[..i].iter().rev().take_while(|pr| r.created - pr.created <= FORK_SECS).find(|pr| {
            let (a, b) = (house(&pr.address_key), house(&r.address_key));
            // The same street, and no two different house numbers.
            let same_street = !s.is_empty()
                && street(&pr.address_key) == s
                && match (&a, &b) {
                    (Some(a), Some(b)) => a == b,
                    _ => true,
                };
            // The same house number of three digits or more, on a street
            // name heard two ways ("Maple Court", "Mabel Court").
            let same_house = matches!((&a, &b), (Some(a), Some(b)) if a == b && a.len() >= 3);
            same_street || same_house
        });
        let first = joins.map(|pr| group[&pr.id]).unwrap_or(r.id);
        group.insert(r.id, first);
    }
    let merged_how = |inc: i64, first: i64| {
        if inc == first { String::new() } else { "the same street, filed by the dispatch map as a separate run".to_string() }
    };
    let mut case_of: HashMap<i64, i64> = HashMap::new(); // incident -> primary incident, cases only
    let mut primaries: Vec<i64> = Vec::new();
    let make_case = |first: i64, case_of: &mut HashMap<i64, i64>, primaries: &mut Vec<i64>| {
        if primaries.contains(&first) {
            return;
        }
        primaries.push(first);
        for (inc, g) in &group {
            if *g == first {
                case_of.insert(*inc, first);
            }
        }
    };
    for r in &runs_rows {
        if is_case_run.contains(&r.id) {
            make_case(group[&r.id], &mut case_of, &mut primaries);
        }
    }

    // Ops traffic in the window, with what the dispatch map and the radio
    // identities already know about each call.
    let ops: Vec<OpsCall> = {
        let mut q = c
            .prepare(
                "SELECT k.id, k.tg, k.unit, k.start, COALESCE(k.transcript_edited, k.transcript, ''), k.system,
                        (SELECT ic.incident FROM incident_calls ic WHERE ic.call = k.id LIMIT 1)
                   FROM calls k
                  WHERE k.start BETWEEN ?1 AND ?2 AND length(COALESCE(k.transcript_edited, k.transcript, '')) > 2
                  ORDER BY k.start, k.id",
            )
            .map_err(|e| e.to_string())?;
        let rows = q
            .query_map(params![from, to], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)? as u16,
                    r.get::<_, i64>(2)? as u32,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let raw: Vec<_> = rows.flatten().filter(|x| inp.tactical_tgs.contains(&x.1)).collect();
        let radios: Vec<(String, u32)> = raw.iter().map(|x| (x.5.clone(), x.2)).collect();
        let ids = crate::radios::identities(c, &radios);
        raw.into_iter()
            .map(|(call, tg, radio, at, text, system, incident)| OpsCall {
                call,
                tg,
                radio,
                at,
                minute: local_minute(at),
                text,
                incident,
                learned: ids
                    .get(&(system, radio))
                    .filter(|i| i.usable() && i.role == crate::radios::UNIT)
                    .map(|i| i.callsign.clone()),
            })
            .collect()
    };
    let events = ops_events(&ops, &consoles, p);

    // Place each event. An upgrade placed on a run that was not a case makes
    // it one.
    let run_list = |case_of: &HashMap<i64, i64>, primaries: &[i64]| -> Vec<Run> {
        runs_rows
            .iter()
            .map(|r| Run {
                incident: r.id,
                at: r.created,
                units: r.units.clone(),
                case: case_of.get(&r.id).and_then(|pid| primaries.iter().position(|x| x == pid)),
                upgraded: pages
                    .get(&r.id)
                    .map(|pg| pg.iter().filter(|x| page_says_working(&x.text, p)).map(|x| x.at).collect())
                    .unwrap_or_default(),
            })
            .collect()
    };
    let mut ended: HashMap<i64, i64> = HashMap::new(); // primary -> when it ended
    let mut placed: Vec<(Option<i64>, &OpsEvent, String, bool)> = Vec::new();
    for e in &events {
        let runs = run_list(&case_of, &primaries);
        let open = |r: &Run| {
            r.case.is_some()
                && case_of
                    .get(&r.incident)
                    .and_then(|pid| ended.get(pid))
                    .map_or(true, |t| *t > e.call.at)
        };
        match place(e, &runs, &open, &vocab) {
            Placed::Run { incident, how, inferred } => {
                let primary = match case_of.get(&incident) {
                    Some(pid) => *pid,
                    None if e.heard.kind == "working" => {
                        let first = group.get(&incident).copied().unwrap_or(incident);
                        make_case(first, &mut case_of, &mut primaries);
                        first
                    }
                    None => {
                        placed.push((None, e, format!("{how}, but that run is not an arrest"), false));
                        continue;
                    }
                };
                if matches!(e.heard.kind.as_str(), "terminated" | "downgrade") {
                    ended.entry(primary).or_insert(e.call.at);
                }
                placed.push((Some(primary), e, how, inferred));
            }
            Placed::Nowhere(why) => placed.push((None, e, why, false)),
        }
    }

    // Write: the cases, the runs each is made of, and the events.
    let tx = c.unchecked_transaction().map_err(|e| e.to_string())?;
    let mut ids: HashMap<i64, i64> = HashMap::new();
    for pid in &primaries {
        let opened = runs_rows.iter().find(|r| r.id == *pid).map(|r| r.created).unwrap_or(from);
        tx.execute(
            "INSERT INTO cases (profile, incident, opened) VALUES (?1, ?2, ?3) ON CONFLICT(profile, incident) DO NOTHING",
            params![p.id, pid, opened],
        )
        .map_err(|e| e.to_string())?;
        let id: i64 = tx
            .query_row("SELECT id FROM cases WHERE profile = ?1 AND incident = ?2", params![p.id, pid], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        ids.insert(*pid, id);
        tx.execute("DELETE FROM case_incidents WHERE case_id = ?1", [id]).map_err(|e| e.to_string())?;
        tx.execute("DELETE FROM case_events WHERE case_id = ?1", [id]).map_err(|e| e.to_string())?;
    }
    tx.execute(
        "DELETE FROM case_events WHERE case_id IS NULL AND profile = ?1 AND at BETWEEN ?2 AND ?3",
        params![p.id, from, to],
    )
    .map_err(|e| e.to_string())?;
    let mut built = Built { cases: primaries.len(), ..Default::default() };
    for (inc, pid) in &case_of {
        let how = merged_how(*inc, *pid);
        tx.execute(
            "INSERT OR REPLACE INTO case_incidents (case_id, incident, how) VALUES (?1, ?2, ?3)",
            params![ids[pid], inc, how],
        )
        .map_err(|e| e.to_string())?;
        let call_type = runs_rows.iter().find(|r| r.id == *inc).map(|r| r.call_type.clone()).unwrap_or_default();
        let lines = page_lines(&call_type, pages.get(inc).map(Vec::as_slice).unwrap_or(&[]), p, inc != pid);
        for l in lines {
            insert_line(&tx, Some(ids[pid]), &p.id, &l, None)?;
            built.events += 1;
        }
    }
    for (primary, e, how, inferred) in &placed {
        let line = Line {
            at: e.call.at,
            clock: e.heard.clock.map(hhmm),
            kind: e.heard.kind.clone(),
            label: e.heard.label.clone(),
            source: e.heard.source.as_str().into(),
            how: how.clone(),
            inferred: *inferred,
            call: Some(e.call.call),
            conversation: None,
            detail: match &e.answers {
                Some(a) => format!("{} — answering: {}", e.call.text, a.text),
                None => e.call.text.clone(),
            },
        };
        let case = primary.map(|pid| ids[&pid]);
        insert_line(&tx, case, &p.id, &line, e.answers.as_ref().map(|a| a.call))?;
        match case {
            Some(_) => {
                built.events += 1;
                if *inferred {
                    built.inferred += 1;
                }
            }
            None => built.unplaced += 1,
        }
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(built)
}

fn insert_line(c: &Connection, case: Option<i64>, profile: &str, l: &Line, answers: Option<i64>) -> Result<(), String> {
    c.execute(
        "INSERT INTO case_events (case_id, profile, at, clock, kind, label, source, how, inferred, call, answers, detail)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![case, profile, l.at, l.clock, l.kind, l.label, l.source, l.how, l.inferred as i64, l.call, answers, l.detail],
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// reading cases back
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone, Debug)]
pub struct CaseView {
    pub id: i64,
    pub profile: String,
    pub title: String,
    pub call_type: String,
    pub address: String,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub units: Vec<String>,
    pub incidents: Vec<i64>,
    pub opened: i64,
    pub updated: i64,
    /// dispatched | working | rosc | transporting | reported | terminated | downgraded
    pub state: String,
    /// Still happening: not ended, and heard from in the last two hours.
    pub open: bool,
    pub lines: Vec<Line>,
}

#[derive(Serialize, Clone, Debug)]
pub struct Unplaced {
    pub at: i64,
    pub clock: Option<String>,
    pub kind: String,
    pub label: String,
    pub source: String,
    pub why: String,
    pub call: Option<i64>,
    pub detail: String,
}

#[derive(Serialize, Clone, Debug)]
pub struct CasesView {
    pub cases: Vec<CaseView>,
    pub unplaced: Vec<Unplaced>,
}

fn lines_for(c: &Connection, case: i64) -> Vec<Line> {
    let Ok(mut q) = c.prepare(
        "SELECT at, clock, kind, label, source, how, inferred, call, detail FROM case_events WHERE case_id = ?1 ORDER BY at, id",
    ) else {
        return Vec::new();
    };
    q.query_map([case], |r| {
        Ok(Line {
            at: r.get(0)?,
            clock: r.get(1)?,
            kind: r.get(2)?,
            label: r.get(3)?,
            source: r.get(4)?,
            how: r.get(5)?,
            inferred: r.get::<_, i64>(6)? != 0,
            call: r.get(7)?,
            conversation: None,
            detail: r.get(8)?,
        })
    })
    .map(|rows| rows.flatten().collect())
    .unwrap_or_default()
}

/// Hospital reports joined to any run of the case, as timeline lines.
fn report_lines(c: &Connection, incidents: &[i64], places: &crate::places::Settings) -> Vec<Line> {
    let mut out = Vec::new();
    for inc in incidents {
        for r in crate::link::reports_for(c, *inc, places) {
            let (summary, pieces): (String, String) = c
                .query_row("SELECT summary, pieces FROM conversations WHERE id = ?1", [r.id], |x| Ok((x.get(0)?, x.get(1)?)))
                .unwrap_or_default();
            let pieces: Vec<crate::conversations::Piece> = serde_json::from_str(&pieces).unwrap_or_default();
            let anchor = pieces.iter().find(|p| !p.fixed).map(|p| p.at).unwrap_or(r.at);
            let eta = crate::conversations::eta_phrase(&summary);
            let place = if r.place.is_empty() { r.tg_desc.clone() } else { r.place.clone() };
            let mut label = format!("Report to {}", if place.is_empty() { r.tg_name.clone() } else { place });
            if let Some(e) = &eta {
                label.push_str(&format!(" · said {e}"));
                if let Some((lo, hi)) = eta_minutes(e) {
                    let (a, b) = (local_minute(anchor + lo * 60), local_minute(anchor + hi * 60));
                    if lo == hi {
                        label.push_str(&format!(" → about {}", hhmm(a)));
                    } else {
                        label.push_str(&format!(" → {}–{}", hhmm(a), hhmm(b)));
                    }
                }
            }
            out.push(Line {
                at: anchor,
                clock: None,
                kind: "report".into(),
                label,
                source: Source::Report.as_str().into(),
                how: r.how.clone(),
                inferred: r.how.contains("learned"),
                call: pieces.iter().find(|p| !p.fixed).and_then(|p| p.id),
                conversation: Some(r.id),
                detail: if r.headline.is_empty() { summary } else { format!("{} — {}", r.headline, summary) },
            });
        }
    }
    out
}

fn view(c: &Connection, id: i64, profile: &str, primary: i64, opened: i64, places: &crate::places::Settings, now: i64) -> Option<CaseView> {
    let inc = crate::dispatch::inc_get(c, primary).ok().flatten()?;
    let incidents: Vec<i64> = c
        .prepare("SELECT incident FROM case_incidents WHERE case_id = ?1 ORDER BY incident")
        .and_then(|mut q| q.query_map([id], |r| r.get(0)).map(|rows| rows.flatten().collect()))
        .unwrap_or_else(|_| vec![primary]);
    let mut units: Vec<String> = Vec::new();
    for i in &incidents {
        if let Ok(Some(x)) = crate::dispatch::inc_get(c, *i) {
            for u in x.units {
                if !units.iter().any(|v| v.eq_ignore_ascii_case(&u)) {
                    units.push(u);
                }
            }
        }
    }
    let mut lines = lines_for(c, id);
    lines.extend(report_lines(c, &incidents, places));
    lines.sort_by_key(|l| l.at);
    let state = state_of(&lines).to_string();
    let updated = lines.iter().map(|l| l.at).max().unwrap_or(opened);
    let pediatric = lines.iter().any(|l| l.source == "page" && words(&l.detail).iter().any(|w| w == "pediatric"));
    let title = match (pediatric, state.as_str()) {
        (_, "downgraded") => "Not a cardiac arrest".to_string(),
        (true, _) => "Pediatric cardiac arrest".to_string(),
        _ if profile == "cardiac-arrest" => "Cardiac arrest".to_string(),
        _ => inc.call_type.clone(),
    };
    Some(CaseView {
        id,
        profile: profile.to_string(),
        title,
        call_type: inc.call_type.clone(),
        address: inc.address.clone(),
        lat: inc.lat,
        lon: inc.lon,
        units,
        incidents,
        opened,
        updated,
        open: !matches!(state.as_str(), "terminated" | "downgraded") && now - updated <= 2 * 3600,
        state,
        lines,
    })
}

pub fn list(c: &Connection, since: i64, places: &crate::places::Settings, now: i64) -> CasesView {
    let rows: Vec<(i64, String, i64, i64)> = c
        .prepare("SELECT id, profile, incident, opened FROM cases WHERE opened >= ?1 ORDER BY opened DESC LIMIT 200")
        .and_then(|mut q| q.query_map([since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).map(|rows| rows.flatten().collect()))
        .unwrap_or_default();
    let cases = rows.into_iter().filter_map(|(id, p, inc, opened)| view(c, id, &p, inc, opened, places, now)).collect();
    let unplaced = c
        .prepare(
            "SELECT at, clock, kind, label, source, how, call, detail FROM case_events
              WHERE case_id IS NULL AND at >= ?1 ORDER BY at DESC LIMIT 200",
        )
        .and_then(|mut q| {
            q.query_map([since], |r| {
                Ok(Unplaced {
                    at: r.get(0)?,
                    clock: r.get(1)?,
                    kind: r.get(2)?,
                    label: r.get(3)?,
                    source: r.get(4)?,
                    why: r.get(5)?,
                    call: r.get(6)?,
                    detail: r.get(7)?,
                })
            })
            .map(|rows| rows.flatten().collect())
        })
        .unwrap_or_default();
    CasesView { cases, unplaced }
}

// ---------------------------------------------------------------------------
// keeping it current
// ---------------------------------------------------------------------------

static DIRTY: AtomicBool = AtomicBool::new(true);

/// Something a case could be built from changed: a transcript, a run, a join.
pub fn touch() {
    DIRTY.store(true, Ordering::Relaxed);
}

/// The ops channels: the ones Dispatch setup marks tactical.
fn tactical_tgs(app: &AppHandle) -> HashSet<u16> {
    let state = app.state::<AppState>();
    let d = state.dispatch.lock().unwrap();
    d.settings.channels.iter().filter(|ch| ch.enabled && ch.role == "tactical").map(|ch| ch.tg).collect()
}

fn rebuild_all(app: &AppHandle, from: i64, to: i64) -> Result<Built, String> {
    let settings = load(app);
    let tactical_tgs = tactical_tgs(app);
    let state = app.state::<AppState>();
    let db = state.db.lock().unwrap().clone().ok_or("library not open")?;
    let c = db.lock().unwrap();
    let mut total = Built::default();
    for p in settings.profiles.iter().filter(|p| p.enabled) {
        let b = rebuild(&c, &Inputs { profile: p, tactical_tgs: &tactical_tgs }, from, to)?;
        total.cases += b.cases;
        total.events += b.events;
        total.inferred += b.inferred;
        total.unplaced += b.unplaced;
    }
    Ok(total)
}

/// Rebuild the last few hours whenever something changed, at most every few
/// seconds, and every ten minutes regardless so a radio learned in between
/// can place an event it could not before.
pub fn spawn(app: AppHandle) {
    std::thread::spawn(move || {
        let mut last = Built::default();
        let mut since_full = std::time::Instant::now();
        loop {
            std::thread::sleep(std::time::Duration::from_secs(5));
            let due = since_full.elapsed().as_secs() >= crate::radios::SWEEP_SECS;
            if !DIRTY.swap(false, Ordering::Relaxed) && !due {
                continue;
            }
            if due {
                since_full = std::time::Instant::now();
            }
            let now = crate::library::now();
            match rebuild_all(&app, now - LIVE_WINDOW_SECS, now) {
                Ok(b) => {
                    if b != last {
                        let _ = tauri::Emitter::emit(&app, "cases", ());
                        last = b;
                    }
                }
                Err(e) => eprintln!("[cases] {e}"),
            }
        }
    });
}

#[tauri::command]
pub fn cases_list(app: AppHandle, state: State<AppState>, hours: Option<u32>) -> Result<CasesView, String> {
    let places = crate::places::load(&app).settings;
    let db = state.db.lock().unwrap().clone().ok_or("library not open")?;
    let c = db.lock().unwrap();
    let now = crate::library::now();
    Ok(list(&c, now - hours.unwrap_or(24).clamp(1, 24 * 60) as i64 * 3600, &places, now))
}

/// Build the cases again over the last `days` of the library. Sends nothing.
#[tauri::command]
pub async fn cases_rebuild(app: AppHandle, days: u32) -> Result<Built, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let now = crate::library::now();
        let b = rebuild_all(&app, now - days.clamp(1, 365) as i64 * 86_400, now)?;
        let _ = tauri::Emitter::emit(&app, "cases", ());
        Ok(b)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub fn cases_profiles(app: AppHandle) -> Settings {
    load(&app)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p() -> Profile {
        arrest_profile()
    }

    fn kind(text: &str, console: bool, minute: i64) -> Option<(String, Source, Option<i64>)> {
        classify(text, console, minute, &p()).map(|h| (h.kind, h.source, h.clock))
    }

    const M1748: i64 = 17 * 60 + 48;

    #[test]
    fn a_dispatcher_readback_is_the_event_and_its_time() {
        assert_eq!(kind("Working Arrest 1748.", true, M1748), Some(("working".into(), Source::Readback, Some(M1748))));
        assert_eq!(kind("Working cardiac arrest, 716.", true, 7 * 60 + 16).map(|k| k.2), Some(Some(7 * 60 + 16)));
        assert_eq!(kind("rosc 1914", true, 19 * 60 + 14).map(|k| k.0), Some("rosc".into()));
        assert_eq!(kind("Ceasing efforts 2326.", true, 23 * 60 + 26).map(|k| k.0), Some("terminated".into()));
        assert_eq!(kind("DOA 2052.", true, 20 * 60 + 52).map(|k| k.0), Some("terminated".into()));
        assert_eq!(kind("Transporting Example General, EMS92 on board, 653.", true, 6 * 60 + 53).map(|k| k.0), Some("transporting".into()));
    }

    #[test]
    fn a_misheard_readback_still_reads_by_its_shape() {
        // What the transcriber actually wrote.
        assert_eq!(kind("Oregon arrest 1157.", true, 11 * 60 + 57).map(|k| k.0), Some("working".into()));
        assert_eq!(kind("Working RAS 629.", true, 6 * 60 + 29).map(|k| k.0), Some("working".into()));
        assert_eq!(kind("Working at 725.", true, 7 * 60 + 25).map(|k| k.0), Some("working".into()));
        assert_eq!(kind("A ceasing effort to hate.", true, 8 * 60 + 28).map(|k| k.0), Some("terminated".into()));
        assert_eq!(kind("Clear overdose, not under arrest, 1644.", true, 16 * 60 + 44).map(|k| k.0), Some("downgrade".into()));
        assert_eq!(kind("Not in arrest 12-04.", true, 12 * 60 + 4), Some(("downgrade".into(), Source::Readback, Some(12 * 60 + 4))));
    }

    #[test]
    fn talk_about_an_arrest_is_not_an_arrest_event() {
        assert_eq!(kind("Can you add us to that cardiac arrest with Ladder 27? Keep them on the run.", false, 0), None);
        assert_eq!(kind("Any few working arrests?", false, 0), None);
        assert_eq!(kind("Working or else?", true, 0), None);
        // A long sentence from a console is not a logged status.
        assert_eq!(kind("They are supposed to both be transported to the hospital, but neither of them show en route at the moment.", true, 0), None);
        // "Working" alone is only a readback's word.
        assert_eq!(kind("We're working on getting access to the building now", false, 0), None);
        // A request for another ambulance is not a transport, and a word
        // that sounds like a readback's is not it.
        assert_eq!(kind("Claire, other transporting unit 1520.", true, 15 * 60 + 20), None);
        assert_eq!(kind("Clearing starting. Transfers for second patient.", true, 0), None);
        // Short words are not stretched: "do" is not DOA.
        assert_eq!(kind("Do you need anything else from us", false, 0), None);
    }

    #[test]
    fn a_crew_statement_is_an_event_of_its_own() {
        assert_eq!(kind("This is going to be a DOA. We can cancel the medic.", false, 0).map(|k| (k.0, k.1)), Some(("terminated".into(), Source::Crew)));
        assert_eq!(kind("Control from Engine 44, this is overdose, not a cardiac arrest.", false, 0).map(|k| k.0), Some("downgrade".into()));
        assert_eq!(kind("Control, this is Medic 30, working traumatic arrest.", false, 0).map(|k| k.0), Some("working".into()));
        assert_eq!(kind("We have rosc", false, 0).map(|k| k.0), Some("rosc".into()));
    }

    #[test]
    fn a_time_that_is_not_the_calls_time_is_not_a_time() {
        assert_eq!(spoken_clock("Working Arrest 1748", M1748 + 3), Some(M1748));
        assert_eq!(spoken_clock("Working Arrest 1748", M1748 + 40), None);
        assert_eq!(spoken_clock("Transporting, 663", 6 * 60 + 53), None, "663 is not a time");
        assert_eq!(spoken_clock("Working 2359", 1), Some(23 * 60 + 59), "across midnight");
        assert_eq!(page_clock("Medic 20, 1200 Example Street, Cardiac Arrest, 626 Hours, Location 1000 North", 6 * 60 + 26), Some(6 * 60 + 26));
    }

    fn page(call: i64, at: i64, text: &str) -> Page {
        Page { call, at, minute: 0, text: text.into() }
    }

    #[test]
    fn a_run_is_a_case_by_its_type_or_its_page_and_the_repage_is_the_upgrade() {
        let pages = vec![
            page(1, 100, "Engine 27, Medic 20, 1200 Example St, Unconscious Person. Engine 27, Medic 20."),
            page(2, 160, "Ambulance 15, EMS 92, 1200 Example St, Cardiac Arrest."),
            page(3, 400, "Squad 10, 1200 Example Street, Cardiac Arrest Working, Squad 10."),
            page(4, 600, "Ladder 27, 1200 Example Street, Cardiac Arrest Working."),
        ];
        assert!(is_case("Unconscious", &pages, &p()));
        assert!(!is_case("Sick Person", &pages[..1], &p()));
        assert!(is_case("Cardiac Arrest", &[], &p()));
        // The page outranks a type the model got wrong.
        assert!(!is_case("Cardiac Arrest", &[page(9, 1, "Engine 1, Medic 18, 1200 Example Avenue, Tesspain Heart.")], &p()));
        let l = page_lines("Unconscious", &pages, &p(), false);
        let kinds: Vec<&str> = l.iter().map(|x| x.kind.as_str()).collect();
        assert_eq!(kinds, vec!["dispatched", "repage", "working", "repage"]);
        assert_eq!(l[0].label, "Dispatched as Unconscious");
    }

    fn ops(call: i64, radio: u32, at: i64, text: &str) -> OpsCall {
        OpsCall { call, tg: 1, radio, at, minute: 0, text: text.into(), incident: None, learned: None }
    }

    #[test]
    fn a_readback_and_the_crew_call_it_repeats_are_one_event() {
        let consoles: HashSet<u32> = [900001].into();
        let calls = vec![ops(1, 900222, 100, "We have rosc"), ops(2, 900001, 104, "rosc")];
        let e = ops_events(&calls, &consoles, &p());
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].heard.source, Source::Readback);
        assert_eq!(e[0].answers.as_ref().map(|a| a.call), Some(1));
        // A readback long after is not answering it.
        let late = vec![ops(1, 900222, 100, "We have rosc"), ops(2, 900001, 200, "rosc")];
        assert_eq!(ops_events(&late, &consoles, &p()).len(), 2);
    }

    fn run(incident: i64, at: i64, units: &[&str], case: Option<usize>) -> Run {
        Run { incident, at, units: units.iter().map(|s| s.to_string()).collect(), case, upgraded: Vec::new() }
    }

    fn vocab() -> HashSet<String> {
        ["medic", "engine", "ladder", "squad"].iter().map(|s| s.to_string()).collect()
    }

    fn event(text: &str, console: bool, crew: Option<OpsCall>) -> OpsEvent {
        OpsEvent {
            heard: classify(text, console, 0, &p()).unwrap(),
            call: ops(9, if console { 900001 } else { 900222 }, 1_000, text),
            answers: crew,
        }
    }

    #[test]
    fn an_event_is_placed_by_what_names_the_run_before_the_clock() {
        let runs = vec![run(10, 900, &["Engine 31", "Medic 32"], Some(0)), run(11, 950, &["Medic 18"], Some(1))];
        let open = |r: &Run| r.case.is_some();
        // Said.
        let said = event("Control from Engine 31, this is not a cardiac arrest.", false, None);
        assert!(matches!(place(&said, &runs, &open, &vocab()), Placed::Run { incident: 10, inferred: false, .. }));
        // Learned: the crew call the readback answers came from a radio
        // learned as Medic 18.
        let mut crew = ops(8, 900333, 995, "This is a working arrest.");
        crew.learned = Some("Medic 18".into());
        let learned = event("Working Arrest", true, Some(crew));
        match place(&learned, &runs, &open, &vocab()) {
            Placed::Run { incident, how, inferred } => {
                assert_eq!((incident, inferred), (11, false));
                assert!(how.contains("learned as Medic 18"));
            }
            other => panic!("{other:?}"),
        }
        // Two arrests open and nothing names the run: not guessed.
        let bare = event("Working Arrest", true, None);
        assert!(matches!(place(&bare, &runs, &open, &vocab()), Placed::Nowhere(_)));
        // One open: placed, and marked as inferred.
        assert!(matches!(place(&bare, &runs[..1], &open, &vocab()), Placed::Run { incident: 10, inferred: true, .. }));
        // A page upgrading an open arrest at that moment outranks a callsign
        // that points at an older run the page lost the unit from.
        let mut arrest = run(12, 950, &["Medic 17"], Some(2));
        arrest.upgraded = vec![1_050];
        let older = run(13, -2_000, &["Engine 36"], None);
        let named = event("Fire control, this is engine 36, mark this working cardiac arrest.", false, None);
        let both = vec![older, arrest];
        assert!(matches!(place(&named, &both, &open, &vocab()), Placed::Run { incident: 12, .. }));
        // A crew statement is never placed by the clock.
        let crew_only = event("This is going to be a DOA.", false, None);
        assert!(matches!(place(&crew_only, &runs[..1], &open, &vocab()), Placed::Nowhere(_)));
    }

    #[test]
    fn where_a_case_stands() {
        let l = |kind: &str| Line {
            at: 0, clock: None, kind: kind.into(), label: String::new(), source: String::new(), how: String::new(),
            inferred: false, call: None, conversation: None, detail: String::new(),
        };
        assert_eq!(state_of(&[l("dispatched"), l("working"), l("rosc")]), "rosc");
        assert_eq!(state_of(&[l("dispatched"), l("rosc"), l("rearrest")]), "working");
        assert_eq!(state_of(&[l("dispatched"), l("terminated"), l("report")]), "terminated");
        assert_eq!(state_of(&[l("dispatched"), l("downgrade")]), "downgraded");
    }

    #[test]
    fn a_stated_eta_in_minutes() {
        assert_eq!(eta_minutes("ETA of 5 to 7 minutes"), Some((5, 7)));
        assert_eq!(eta_minutes("approximately ten minutes out"), Some((10, 10)));
        assert_eq!(eta_minutes("arriving in about 2 hours"), None);
    }

    #[test]
    fn a_rebuild_over_a_library_makes_the_timeline() {
        let c = Connection::open_in_memory().unwrap();
        crate::dispatch::ensure_schema(&c);
        c.execute_batch(
            "CREATE TABLE calls (id INTEGER PRIMARY KEY, start INTEGER, secs REAL, tg INTEGER, tg_name TEXT, unit INTEGER, unit_name TEXT,
               transcript TEXT, transcript_edited TEXT, system TEXT NOT NULL DEFAULT '');
             CREATE TABLE conversations (id INTEGER PRIMARY KEY, incident INTEGER);",
        )
        .unwrap();
        crate::radios::ensure_schema(&c);
        ensure_schema(&c);
        let t0 = 1_000_000;
        c.execute_batch(&format!(
            "INSERT INTO incidents (id, created, updated, tg, call_type, address, address_key, units) VALUES
               (1, {t0}, {t0}, 1, 'Unconscious', '1200 Example St', '1200 example street', '[\"Engine 5\",\"Medic 7\",\"Medic 9\",\"Medic 11\",\"Medic 12\"]'),
               (2, {t1}, {t1}, 1, 'Cardiac Arrest', 'Example St', 'example street', '[\"Squad 3\"]'),
               (3, {t0}, {t0}, 1, 'Sick Person', '9 Other Road', '9 other road', '[\"Medic 40\"]');
             INSERT INTO calls (id, start, secs, tg, unit, transcript) VALUES
               (1, {t0}, 5, 1, 900900, 'Engine 5, Medic 7, 1200 Example St, Unconscious Person. 1200 Hours, Location 1 North'),
               (2, {t1}, 5, 1, 900900, 'Squad 3, Example Street, Cardiac Arrest Working. 1205 Hours, Location 1 North'),
               (3, {t2}, 3, 2, 900222, 'Control, Medic 7, we have rosc'),
               (4, {t3}, 2, 2, 900001, 'rosc'),
               (5, {t4}, 2, 2, 900001, 'Working Arrest'),
               (6, {t5}, 3, 2, 900555, 'This is going to be a DOA.');
             INSERT INTO incident_calls (incident, call, at, tg, role) VALUES (1, 1, {t0}, 1, 'dispatch'), (2, 2, {t1}, 1, 'dispatch');
             INSERT INTO radio_evidence (system, radio, callsign, role, how, call, at, weight) VALUES
               ('', 900001, '', 'console', 'from_control', 101, 1, 1), ('', 900001, '', 'console', 'from_control', 102, 1, 1), ('', 900001, '', 'console', 'from_control', 103, 1, 1);",
            t1 = t0 + 300, t2 = t0 + 900, t3 = t0 + 904, t4 = t0 + 1200, t5 = t0 + 1500,
        ))
        .unwrap();
        let tactical: HashSet<u16> = [2].into();
        let prof = p();
        let inp = Inputs { profile: &prof, tactical_tgs: &tactical };
        let b = rebuild(&c, &inp, t0 - 60, t0 + 3600).unwrap();
        // One case: run 2 is run 1's street, repaged.
        assert_eq!(b.cases, 1, "{b:?}");
        let again = rebuild(&c, &inp, t0 - 60, t0 + 3600).unwrap();
        assert_eq!(again, b, "a rebuild is repeatable");
        let v = list(&c, 0, &crate::places::Settings::default(), t0 + 3600);
        assert_eq!(v.cases.len(), 1);
        let kinds: Vec<(&str, &str)> = v.cases[0].lines.iter().map(|l| (l.kind.as_str(), l.source.as_str())).collect();
        assert_eq!(
            kinds,
            vec![("dispatched", "page"), ("working", "page"), ("rosc", "readback"), ("working", "readback")],
            "the rosc is placed by the callsign its crew call said, the bare readback is the only open arrest, and a crew DOA naming no run is not placed"
        );
        assert!(v.cases[0].lines[3].inferred);
        assert_eq!(v.unplaced.len(), 1);
        assert_eq!(v.unplaced[0].kind, "terminated");
    }

    /// Against a copy of a real library: `HS_CASES_DB=copy.db HS_CASES_TACTICAL=101,102
    /// HS_CASES_HOSPITAL=201,202 cargo test cases::tests::real_library -- --ignored --nocapture`.
    /// Writes into that copy. Never point it at the live file.
    #[test]
    #[ignore]
    fn real_library() {
        let Ok(path) = std::env::var("HS_CASES_DB") else { return };
        let set = |k: &str| -> HashSet<u16> {
            std::env::var(k).unwrap_or_default().split(',').filter_map(|t| t.trim().parse().ok()).collect()
        };
        let c = Connection::open(&path).unwrap();
        crate::conversations::ensure_schema(&c);
        crate::link::ensure_schema(&c);
        crate::radios::ensure_schema(&c);
        ensure_schema(&c);
        crate::radios::backfill(&c, &set("HS_CASES_HOSPITAL")).unwrap();
        let tactical = set("HS_CASES_TACTICAL");
        let prof = p();
        let (from, to): (i64, i64) = c.query_row("SELECT MIN(start), MAX(start) FROM calls", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        let t0 = std::time::Instant::now();
        let b = rebuild(&c, &Inputs { profile: &prof, tactical_tgs: &tactical }, from, to).unwrap();
        println!("{b:?} in {:?}", t0.elapsed());
        let v = list(&c, 0, &crate::places::Settings::default(), to);
        for k in &v.cases {
            println!("\n#{} {} · {} · {} · {} · units {:?}", k.id, k.title, k.call_type, k.address, k.state, k.units);
            for l in &k.lines {
                println!(
                    "  {} {:<5} {:<12} {:<9} {}{}  | {}",
                    crate::library::local_hm(l.at),
                    l.clock.clone().unwrap_or_default(),
                    l.kind,
                    l.source,
                    l.label,
                    if l.inferred { " (inferred)" } else { "" },
                    if l.source == "page" { l.detail.chars().take(70).collect::<String>() } else { format!("{} · {}", l.how, l.detail.chars().take(90).collect::<String>()) }
                );
            }
        }
        println!("\nUNPLACED {}", v.unplaced.len());
        for u in &v.unplaced {
            println!("  {} {:<12} {:<9} {} | {}", crate::library::local_hm(u.at), u.kind, u.source, u.why, u.detail.chars().take(90).collect::<String>());
        }
    }
}
