//! Cases on Telegram.
//!
//! One message per case per chat is the timeline, edited in place as the
//! case grows: an edit is silent, so nobody's phone buzzes for a repage. A
//! reply, which does buzz, goes out only for the events a profile lists:
//! the upgrade to working, ROSC, lost pulses, a report with its ETA (and an
//! ETA that moves by three minutes or more), the downgrade, efforts ceased.
//!
//! Which chats: the profile's own chat hears every case from dispatch. A
//! hospital's chat (its place's destination) hears a case once a crew has
//! called that hospital, and gets the whole timeline so far as its first
//! message. Nothing is sent to a hospital on a guess about where the patient
//! is going.
//!
//! Everything that decides what to send is pure and tested here; the loop at
//! the bottom only compares that with what the library says went out.

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use tauri::{AppHandle, Manager};

use crate::cases::{Arrival, CaseView, Line};
use crate::AppState;

type Db = std::sync::Arc<std::sync::Mutex<Connection>>;

/// A notice older than this is not sent: after a restart, or a sending
/// switched on mid-shift, the thread is brought up to date by the edit and
/// nobody is buzzed about something twenty minutes old.
pub const NOTIFY_WITHIN_SECS: i64 = crate::link::NOTIFY_WITHIN_SECS;
/// A case whose last word is older than this gets no new thread at all.
pub const START_WITHIN_SECS: i64 = 2 * 3600;
/// An ETA that moves by less than this is not news.
pub const ETA_MOVE_SECS: i64 = 180;
/// How long after a case opens its map waits for the run's routes.
const ROUTES_WITHIN_SECS: i64 = 120;
/// Telegram's limit on a message is 4096 characters.
const MAX_CHARS: usize = 3900;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Send {
    pub enabled: bool,
    /// The named destination every case goes to. Blank is none.
    pub dest: String,
    /// Also each hospital's own chat, once a report to it is heard.
    pub hospitals: bool,
    /// Event kinds that get a reply.
    pub notify: Vec<String>,
    /// A map under a thread's first message: the run with its routes to the
    /// hospitals its care pathway names (the nearest, the ECMO centre) in the
    /// chat for every case, scene to hospital in a hospital's chat.
    pub map: bool,
}

impl Default for Send {
    fn default() -> Self {
        Send {
            enabled: false,
            dest: String::new(),
            hospitals: true,
            notify: ["working", "rosc", "rearrest", "report", "downgrade", "terminated"].map(String::from).to_vec(),
            map: true,
        }
    }
}

// ---------------------------------------------------------------------------
// what the timeline message says
// ---------------------------------------------------------------------------

fn hm(epoch: i64) -> String {
    crate::library::local_hm(epoch)
}

pub(crate) fn state_label(state: &str) -> &str {
    match state {
        "dispatched" => "dispatched",
        "working" => "working",
        "rosc" => "ROSC",
        "transporting" => "transporting",
        "reported" => "reported to hospital",
        "arrived" => "at the hospital",
        "terminated" => "efforts ceased",
        "downgraded" => "not an arrest",
        other => other,
    }
}

pub(crate) fn source_label(l: &Line) -> String {
    let who = match l.source.as_str() {
        "readback" => "dispatcher",
        "crew" => "crew",
        _ => "",
    };
    match (who.is_empty(), l.inferred) {
        (true, false) => String::new(),
        (true, true) => " (inferred)".into(),
        (false, false) => format!(" ({who})"),
        (false, true) => format!(" ({who}, inferred)"),
    }
}

const FACTS_SHOWN: &[(&str, &str)] = &[
    ("witnessed", "Witnessed"),
    ("bystander cpr", "Bystander CPR"),
    ("rhythm", "Rhythm"),
    ("downtime", "Downtime (as said)"),
    ("history", "History"),
];

/// The latest value each report gave.
fn facts_of(k: &CaseView) -> HashMap<String, String> {
    let mut got = HashMap::new();
    for l in k.lines.iter().filter(|l| l.kind == "report") {
        for f in &l.facts {
            got.insert(f.key.clone(), f.value.clone());
        }
    }
    got
}

/// The timeline message, as plain text. What goes out is [`linked`]: the
/// same text with the address made a Google Maps link.
pub fn render(k: &CaseView) -> String {
    let mut head = format!("🫀 {} · {}", k.title, state_label(&k.state));
    if !k.address.is_empty() {
        head.push_str(&format!(" · {}", k.address));
    }
    let mut out = vec![head];
    if !k.units.is_empty() {
        out.push(k.units.join(", "));
    }
    // A repage says nothing an edit needs to show, and a crew at the
    // hospital said again an hour later is not a second arrival.
    let first_arrived = k.lines.iter().position(|l| l.kind == crate::cases::ARRIVED);
    let mut timeline: Vec<String> = k
        .lines
        .iter()
        .enumerate()
        .filter(|(i, l)| l.kind != "repage" && (l.kind != crate::cases::ARRIVED || Some(*i) == first_arrived))
        .map(|(_, l)| l)
        .map(|l| format!("{} {}{}", l.clock.clone().unwrap_or_else(|| hm(l.at)), l.label, source_label(l)))
        .collect();
    let facts = facts_of(k);
    let mut tail = Vec::new();
    if let Some(a) = &k.arrival {
        let window = match (a.from, a.to) {
            (Some(f), Some(t)) if f == t => format!("about {}", hm(f)),
            (Some(f), Some(t)) => format!("{}–{}", hm(f), hm(t)),
            _ => "no ETA said".into(),
        };
        let mut s = format!("Expected at {}: {window}", a.place);
        if let Some(d) = a.drive_min {
            s.push_str(&format!(" · {d} min {} from the scene", a.drive_how));
        }
        tail.push(s);
        if !a.note.is_empty() && a.from.is_some() {
            tail.push(format!("⚠️ {}", a.note));
        }
    }
    if k.lines.iter().any(|l| l.kind == "report") {
        let mut who = Vec::new();
        if let Some(age) = facts.get("age") {
            who.push(age.clone());
        }
        if let Some(sex) = facts.get("sex") {
            who.push(sex.clone());
        }
        if !who.is_empty() {
            tail.push(format!("Patient: {}", who.join(", ")));
        }
        tail.push(
            FACTS_SHOWN
                .iter()
                .map(|(key, label)| format!("{label}: {}", facts.get(*key).map(String::as_str).unwrap_or("not stated")))
                .collect::<Vec<_>>()
                .join(" · "),
        );
    }
    if let Some(last) = k.lines.last() {
        tail.push(format!("Updated {}", hm(last.at)));
    }
    // Too long for one message: the oldest lines after the first go, and
    // say so, because the first line and the latest are what the ED needs.
    let size = |t: &[String]| out.iter().chain(t).chain(tail.iter()).map(|x| x.chars().count() + 1).sum::<usize>();
    let mut dropped = 0;
    while size(&timeline) > MAX_CHARS && timeline.len() > 2 {
        timeline.remove(1);
        dropped += 1;
    }
    if dropped > 0 {
        timeline.insert(1, format!("… {dropped} earlier lines"));
    }
    out.extend(timeline);
    out.push(String::new());
    out.extend(tail);
    out.join("\n")
}

/// The timeline as Telegram HTML, its address opening Google Maps at the
/// run's pin (or, with none yet, at the address). `None` when there is no
/// address to link; the plain text goes then.
pub fn linked(k: &CaseView, text: &str, region: &str) -> Option<String> {
    crate::alerts::link_in(text, &k.address, &crate::alerts::maps_url(k.lat, k.lon, &k.address, region))
}

/// Whether the chat for every case is still owed its map: once, while the
/// case is going on. A map turning up for a case that ended long ago, or
/// after an upgrade for every recent one, would be noise.
pub fn map_due(k: &CaseView, t: &Thread, s: &Send, had: Option<&Sent>, now: i64) -> bool {
    let last = k.lines.last().map(|l| l.at).unwrap_or(k.opened);
    s.map && t.place_id.is_none() && !had.is_some_and(|h| h.map_sent) && k.open && now - last <= NOTIFY_WITHIN_SECS
}

// ---------------------------------------------------------------------------
// which events buzz
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    /// Stable across rebuilds: never a row id.
    pub key: String,
    pub at: i64,
    pub text: String,
    /// The report a map goes under, in a hospital's chat.
    pub conversation: Option<i64>,
}

fn window(a: &Arrival) -> String {
    match (a.from, a.to) {
        (Some(f), Some(t)) if f == t => format!(" → about {}", hm(f)),
        (Some(f), Some(t)) => format!(" → {}–{}", hm(f), hm(t)),
        _ => String::new(),
    }
}

/// The replies a case's events call for, oldest first, each with a key that
/// the same event gets on every rebuild.
///
/// ROSC and lost pulses are episodes: the crew's "we have pulses" and the
/// dispatcher's "rosc 1914" a minute later are one ROSC, so a kind buzzes
/// again only after the other one did.
pub fn notices(k: &CaseView, notify: &[String]) -> Vec<Notice> {
    let wants = |kind: &str| notify.iter().any(|n| n == kind);
    let mut out = Vec::new();
    let mut pulse: Option<&str> = None;
    let mut episodes: HashMap<&str, u32> = HashMap::new();
    let mut once: HashSet<&str> = HashSet::new();
    let mut last_to: Option<Option<i64>> = None;
    for l in &k.lines {
        let when = l.clock.clone().unwrap_or_else(|| hm(l.at));
        let kind = l.kind.as_str();
        let (key, text) = match kind {
            "working" | "downgrade" | "terminated" => {
                if !once.insert(kind) {
                    continue;
                }
                let icon = match kind {
                    "working" => "🔴",
                    "downgrade" => "⚪",
                    _ => "⚫",
                };
                (kind.to_string(), format!("{icon} {when} {}{}", l.label, source_label(l)))
            }
            "rosc" | "rearrest" => {
                if pulse == Some(kind) {
                    continue;
                }
                pulse = Some(if kind == "rosc" { "rosc" } else { "rearrest" });
                let n = episodes.entry(if kind == "rosc" { "rosc" } else { "rearrest" }).or_insert(0);
                *n += 1;
                let icon = if kind == "rosc" { "💚" } else { "🔴" };
                (format!("{kind}:{n}"), format!("{icon} {when} {}{}", l.label, source_label(l)))
            }
            "report" => {
                let Some(conv) = l.conversation else { continue };
                let a = k.arrivals.iter().find(|a| a.conversation == conv);
                let to = a.and_then(|a| a.to);
                // The first report is news; a later one only when its ETA
                // moved, or it gave one where the last did not.
                let news = match last_to {
                    None => true,
                    Some(prev) => match (prev, to) {
                        (Some(p), Some(t)) => (t - p).abs() >= ETA_MOVE_SECS,
                        (None, Some(_)) => true,
                        _ => false,
                    },
                };
                if to.is_some() || last_to.is_none() {
                    last_to = Some(to);
                }
                if !news {
                    continue;
                }
                let said = a.and_then(|a| a.said.clone()).map(|s| format!(" · said {s}")).unwrap_or_default();
                let place = a.map(|a| a.place.clone()).unwrap_or_default();
                let place = if place.is_empty() { "the hospital".to_string() } else { place };
                (
                    format!("report:{conv}"),
                    format!("🏥 {when} Report to {place}{said}{}", a.map(window).unwrap_or_default()),
                )
            }
            _ => continue,
        };
        if !wants(kind) {
            continue;
        }
        out.push(Notice { key, at: l.at, text, conversation: l.conversation });
    }
    out
}

// ---------------------------------------------------------------------------
// which chats
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct Thread {
    /// The destination's id, as stored.
    pub dest: String,
    /// When this chat starts hearing about the case: the case opening, or
    /// the first report to that hospital.
    pub since: i64,
    /// The hospital, for a hospital's chat.
    pub place_id: Option<String>,
    pub conversation: Option<i64>,
}

/// The chats a case goes to: the profile's own from the start, and each
/// hospital's once a crew calls it.
pub fn threads(k: &CaseView, s: &Send, places: &crate::places::Settings) -> Vec<Thread> {
    let mut out = Vec::new();
    if !s.dest.is_empty() {
        out.push(Thread { dest: s.dest.clone(), since: k.opened, place_id: None, conversation: None });
    }
    if s.hospitals {
        for a in &k.arrivals {
            let Some(p) = places.places.iter().find(|p| p.enabled && !a.place_id.is_empty() && p.id == a.place_id) else { continue };
            if p.dest.is_empty() || out.iter().any(|t| t.dest == p.dest) {
                continue;
            }
            out.push(Thread { dest: p.dest.clone(), since: a.anchor, place_id: Some(p.id.clone()), conversation: Some(a.conversation) });
        }
    }
    out
}

/// A hospital's chat is told what happens after it joined; the report that
/// brought it in is its first message, not a reply.
pub fn notices_for(t: &Thread, all: &[Notice]) -> Vec<Notice> {
    all.iter()
        .filter(|n| match t.conversation {
            None => true,
            Some(c) => n.at > t.since && n.conversation != Some(c),
        })
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// what went out
// ---------------------------------------------------------------------------

pub fn ensure_schema(c: &Connection) {
    let _ = c.execute_batch(
        "CREATE TABLE IF NOT EXISTS case_sends (
            profile TEXT NOT NULL,
            incident INTEGER NOT NULL,
            dest TEXT NOT NULL,
            target TEXT NOT NULL,
            root_id INTEGER NOT NULL,
            rendered TEXT NOT NULL,
            since INTEGER NOT NULL,
            map_sent INTEGER NOT NULL DEFAULT 0,
            sent_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            error TEXT NOT NULL DEFAULT '',
            PRIMARY KEY (profile, incident, dest)
         );
         CREATE TABLE IF NOT EXISTS case_notices (
            profile TEXT NOT NULL,
            incident INTEGER NOT NULL,
            dest TEXT NOT NULL,
            key TEXT NOT NULL,
            at INTEGER NOT NULL,
            message_id INTEGER,
            sent_at INTEGER NOT NULL,
            PRIMARY KEY (profile, incident, dest, key)
         );",
    );
}

#[derive(Clone, Debug, Default)]
pub struct Sent {
    pub target: String,
    pub root_id: i64,
    pub rendered: String,
    pub map_sent: bool,
    pub notices: HashSet<String>,
}

pub fn sent(c: &Connection, profile: &str, incident: i64, dest: &str) -> Option<Sent> {
    let mut s: Sent = c
        .query_row(
            "SELECT target, root_id, rendered, map_sent FROM case_sends WHERE profile = ?1 AND incident = ?2 AND dest = ?3",
            params![profile, incident, dest],
            |r| Ok(Sent { target: r.get(0)?, root_id: r.get(1)?, rendered: r.get(2)?, map_sent: r.get::<_, i64>(3)? != 0, notices: HashSet::new() }),
        )
        .optional()
        .ok()
        .flatten()?;
    s.notices = c
        .prepare("SELECT key FROM case_notices WHERE profile = ?1 AND incident = ?2 AND dest = ?3")
        .and_then(|mut q| q.query_map(params![profile, incident, dest], |r| r.get(0)).map(|rows| rows.flatten().collect()))
        .unwrap_or_default();
    Some(s)
}

// ---------------------------------------------------------------------------
// what to do
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    /// Start the thread: the timeline as a new message.
    Root { text: String },
    /// Bring the timeline up to date, silently.
    Edit { root_id: i64, text: String },
    /// Buzz, as a reply to the timeline.
    Reply { root_id: Option<i64>, notice: Notice },
    /// Nobody is buzzed about this one; it is recorded as dealt with.
    Stale { notice: Notice },
}

/// What one chat needs for one case, from what it already has.
pub fn plan(k: &CaseView, t: &Thread, notify: &[String], had: Option<&Sent>, now: i64) -> Vec<Step> {
    let text = render(k);
    let mut steps = Vec::new();
    let root = match had {
        Some(h) => {
            if h.rendered != text {
                steps.push(Step::Edit { root_id: h.root_id, text: text.clone() });
            }
            Some(h.root_id)
        }
        None => {
            let last = k.lines.last().map(|l| l.at).unwrap_or(k.opened);
            let over = matches!(k.state.as_str(), "terminated" | "downgraded");
            if now - last > START_WITHIN_SECS || (over && now - last > NOTIFY_WITHIN_SECS) {
                return steps;
            }
            steps.push(Step::Root { text: text.clone() });
            None
        }
    };
    let done = had.map(|h| &h.notices);
    for n in notices_for(t, &notices(k, notify)) {
        if done.is_some_and(|d| d.contains(&n.key)) {
            continue;
        }
        // A new thread already shows everything up to now in its first
        // message; buzzing again for each line in it would be the noise
        // this is built to avoid.
        if had.is_none() || now - n.at > NOTIFY_WITHIN_SECS {
            steps.push(Step::Stale { notice: n });
        } else {
            steps.push(Step::Reply { root_id: root, notice: n });
        }
    }
    steps
}

// ---------------------------------------------------------------------------
// the loop
// ---------------------------------------------------------------------------

fn target_of(al: &crate::alerts::Settings, dest: &str) -> Option<String> {
    al.destinations.iter().find(|d| d.id == dest).map(|d| d.target())
}

/// Failures wait before the same thread is tried again.
static BACKOFF: std::sync::Mutex<Option<HashMap<String, i64>>> = std::sync::Mutex::new(None);

fn waiting(key: &str, now: i64) -> bool {
    BACKOFF.lock().unwrap().as_ref().and_then(|m| m.get(key)).is_some_and(|until| now < *until)
}

fn back_off(key: &str, now: i64) {
    BACKOFF.lock().unwrap().get_or_insert_with(HashMap::new).insert(key.to_string(), now + 60);
}

/// Send what the live cases call for. Called after a live rebuild; never by
/// the Rebuild button, which replays days.
pub fn tick(app: &AppHandle) {
    let settings = crate::cases::load(app);
    if !settings.profiles.iter().any(|p| p.enabled && p.telegram.enabled) {
        return;
    }
    let state = app.state::<AppState>();
    let places = crate::places::load(app).settings;
    let al = state.alerts.lock().unwrap().settings.clone();
    let now = crate::library::now();
    let Some(db) = state.db.lock().unwrap().clone() else { return };
    let view = {
        let c = db.lock().unwrap();
        crate::cases::list(&c, now - START_WITHIN_SECS - crate::cases::LIVE_WINDOW_SECS, &places, now)
    };
    let view = crate::cases::with_roads(&state, view);
    for p in settings.profiles.iter().filter(|p| p.enabled && p.telegram.enabled) {
        for k in view.cases.iter().filter(|k| k.profile == p.id) {
            for t in threads(k, &p.telegram, &places) {
                let key = format!("{}:{}:{}", p.id, k.incident, t.dest);
                if waiting(&key, now) {
                    continue;
                }
                let Some(target) = target_of(&al, &t.dest) else { continue };
                let had = {
                    let c = db.lock().unwrap();
                    sent(&c, &p.id, k.incident, &t.dest)
                };
                let steps = plan(k, &t, &p.telegram.notify, had.as_ref(), now);
                // A run is often placed a little after it is paged, so the
                // map can be owed after its thread has nothing else to say.
                if steps.is_empty() && !(had.is_some() && map_due(k, &t, &p.telegram, had.as_ref(), now)) {
                    continue;
                }
                if let Err(e) = carry_out(app, &db, p, k, &t, &target, had, steps, now) {
                    eprintln!("cases: sending case {} to {}: {e}", k.id, t.dest);
                    back_off(&key, now);
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn carry_out(
    app: &AppHandle,
    db: &Db,
    p: &crate::cases::Profile,
    k: &CaseView,
    t: &Thread,
    target: &str,
    had: Option<Sent>,
    steps: Vec<Step>,
    now: i64,
) -> Result<(), String> {
    let mut root = had.as_ref().map(|h| h.root_id);
    let state = app.state::<AppState>();
    let region = state.dispatch.lock().unwrap().settings.region_hint.clone();
    // A thread lives in the chat it started in, even if the destination has
    // since been pointed somewhere else: that is where its message is.
    let target = match &had {
        Some(h) if !h.target.is_empty() => h.target.clone(),
        _ => target.to_string(),
    };
    for step in steps {
        match step {
            Step::Root { text } => {
                let id = crate::alerts::send_text_reply_html(&target, &text, linked(k, &text, &region).as_deref(), None)?;
                root = Some(id);
                let c = db.lock().unwrap();
                c.execute(
                    "INSERT OR REPLACE INTO case_sends (profile, incident, dest, target, root_id, rendered, since, map_sent, sent_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8, ?8)",
                    params![p.id, k.incident, t.dest, target, id, text, t.since, now],
                )
                .map_err(|e| e.to_string())?;
            }
            Step::Edit { root_id, text } => {
                if let Err(e) = crate::alerts::edit_message_html(&target, root_id, &text, linked(k, &text, &region).as_deref()) {
                    // Past Telegram's edit window, or deleted by someone in
                    // the chat: remember the text so it is not tried forever.
                    if !e.contains("not modified") {
                        eprintln!("cases: edit of case {} in {}: {e}", k.id, t.dest);
                    }
                }
                let c = db.lock().unwrap();
                c.execute(
                    "UPDATE case_sends SET rendered = ?1, updated_at = ?2 WHERE profile = ?3 AND incident = ?4 AND dest = ?5",
                    params![text, now, p.id, k.incident, t.dest],
                )
                .map_err(|e| e.to_string())?;
            }
            Step::Reply { notice, .. } => {
                let id = crate::alerts::send_text_reply(&target, &notice.text, root)?;
                record_notice(db, p, k, t, &notice, Some(id), now)?;
            }
            Step::Stale { notice } => record_notice(db, p, k, t, &notice, None, now)?,
        }
    }
    // In the chat for every case, the run's own map: the scene and the
    // routes to where its pathway says to go, drawn once the run is placed.
    let map_key = format!("{}:{}:{}:map", p.id, k.incident, t.dest);
    if map_due(k, t, &p.telegram, had.as_ref(), now) && !waiting(&map_key, now) {
        let routes = {
            let c = db.lock().unwrap();
            crate::dispatch::inc_get(&c, k.incident).ok().flatten().map(|i| (i.lat.is_some(), i.targets.len()))
        };
        if let (Some(root), Some((placed, targets))) = (root, routes) {
            // A run's position is stored a moment before its routes are
            // worked out from it. Drawn in between, the map would go without
            // them for good; a run whose pathway names nowhere still gets
            // its scene once that moment has plainly passed.
            if placed && (targets > 0 || now - k.opened > ROUTES_WITHIN_SECS) {
                match crate::tripwires::send_map(app, &state, &target, k.incident, Some(root)) {
                    Ok(_) => {
                        let c = db.lock().unwrap();
                        c.execute(
                            "UPDATE case_sends SET map_sent = 1 WHERE profile = ?1 AND incident = ?2 AND dest = ?3",
                            params![p.id, k.incident, t.dest],
                        )
                        .map_err(|e| e.to_string())?;
                    }
                    // The map waits on its own: a tile server down must not
                    // hold up the thread's next reply.
                    Err(e) => {
                        eprintln!("cases: map for case {} in {}: {e}", k.id, t.dest);
                        back_off(&map_key, now);
                    }
                }
            }
        }
    }
    // The map goes once, under the first message in a hospital's chat.
    if p.telegram.map && t.place_id.is_some() && !had.as_ref().is_some_and(|h| h.map_sent) {
        if let (Some(root), Some(a)) = (root, k.arrivals.iter().find(|a| Some(a.conversation) == t.conversation)) {
            if let Some((scene, hospital)) = a.ends {
                let r = crate::routing::shape(&state, scene, hospital);
                let leg = crate::mapshot::Leg { to: hospital, line: r.line, road: r.how == "road" };
                let caption = format!(
                    "{} → {}{}",
                    if k.address.is_empty() { "the scene" } else { &k.address },
                    a.place,
                    a.drive_min.map(|d| format!(" · {d} min {}", a.drive_how)).unwrap_or_default()
                );
                let sent = crate::mapshot::draw(app, scene, &[leg])
                    .and_then(|shot| crate::alerts::send_photo_reply(&target, &shot.png, &caption, None, Some(root)));
                if let Err(e) = &sent {
                    // Tried again on the next pass that has something to do
                    // in this chat; a missing map is not worth a retry loop.
                    eprintln!("cases: map for case {} in {}: {e}", k.id, t.dest);
                    return Ok(());
                }
                let c = db.lock().unwrap();
                c.execute(
                    "UPDATE case_sends SET map_sent = 1 WHERE profile = ?1 AND incident = ?2 AND dest = ?3",
                    params![p.id, k.incident, t.dest],
                )
                .map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(())
}

fn record_notice(
    db: &Db,
    p: &crate::cases::Profile,
    k: &CaseView,
    t: &Thread,
    n: &Notice,
    id: Option<i64>,
    now: i64,
) -> Result<(), String> {
    let c = db.lock().unwrap();
    c.execute(
        "INSERT OR IGNORE INTO case_notices (profile, incident, dest, key, at, message_id, sent_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![p.id, k.incident, t.dest, n.key, n.at, id, now],
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// the preview
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone, Debug, Default)]
pub struct PreviewDay {
    pub date: String,
    pub dest: String,
    pub threads: u32,
    pub replies: u32,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct PreviewCase {
    pub id: i64,
    pub title: String,
    pub address: String,
    pub opened: i64,
    pub chats: Vec<String>,
    pub replies: Vec<String>,
    pub timeline: String,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Preview {
    pub enabled: bool,
    pub days: Vec<PreviewDay>,
    pub cases: Vec<PreviewCase>,
    /// What stops something from going out, said plainly.
    pub warnings: Vec<String>,
}

/// What the cases over a stretch of the library would have sent, counted per
/// day and per chat, with the timelines as they would read at the end.
pub fn preview(view: &crate::cases::CasesView, s: &Send, places: &crate::places::Settings, names: &HashMap<String, String>) -> Preview {
    let name = |id: &str| names.get(id).cloned().unwrap_or_else(|| "a destination that was removed".into());
    let mut days: HashMap<(String, String), PreviewDay> = HashMap::new();
    let mut cases = Vec::new();
    for k in &view.cases {
        let date = chrono::DateTime::from_timestamp(k.opened, 0)
            .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
            .unwrap_or_default();
        let all = notices(k, &s.notify);
        let ts = threads(k, s, places);
        for t in &ts {
            let d = days.entry((date.clone(), t.dest.clone())).or_insert_with(|| PreviewDay { date: date.clone(), dest: name(&t.dest), ..Default::default() });
            d.threads += 1;
            d.replies += notices_for(t, &all).len() as u32;
        }
        cases.push(PreviewCase {
            id: k.id,
            title: k.title.clone(),
            address: k.address.clone(),
            opened: k.opened,
            chats: ts.iter().map(|t| name(&t.dest)).collect(),
            replies: all.iter().map(|n| n.text.clone()).collect(),
            timeline: render(k),
        });
    }
    let mut days: Vec<PreviewDay> = days.into_values().collect();
    days.sort_by(|a, b| b.date.cmp(&a.date).then(a.dest.cmp(&b.dest)));
    let mut warnings = Vec::new();
    if s.dest.is_empty() {
        warnings.push("No chat is chosen for every case, so only hospitals' chats would hear of one.".into());
    } else if !names.contains_key(&s.dest) {
        warnings.push("The chat chosen for every case was removed from Connections.".into());
    }
    if s.hospitals && !places.places.iter().any(|p| p.enabled && !p.dest.is_empty()) {
        warnings.push("No hospital has a chat of its own yet: set one on the place, in Dispatch setup → Places.".into());
    }
    Preview { enabled: s.enabled, days, cases, warnings }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversations::Fact;

    fn line(at: i64, kind: &str, label: &str, source: &str) -> Line {
        Line {
            at,
            clock: None,
            kind: kind.into(),
            label: label.into(),
            source: source.into(),
            how: String::new(),
            inferred: false,
            call: Some(at),
            conversation: None,
            detail: String::new(),
            facts: Vec::new(),
        }
    }

    fn report(at: i64, conv: i64, facts: Vec<Fact>) -> Line {
        Line { conversation: Some(conv), call: None, facts, ..line(at, "report", "Report to Example General", "report") }
    }

    fn arrival(conv: i64, anchor: i64, said: Option<&str>) -> Arrival {
        let mut a = crate::cases::predict(conv, "Example General", anchor, said, None);
        a.place_id = "p-general".into();
        a
    }

    fn case(lines: Vec<Line>, arrivals: Vec<Arrival>) -> CaseView {
        CaseView {
            id: 1,
            profile: "cardiac-arrest".into(),
            incident: 11,
            title: "Cardiac arrest".into(),
            call_type: "Unconscious".into(),
            address: "1200 Example St".into(),
            lat: None,
            lon: None,
            units: vec!["Engine 5".into(), "Medic 7".into()],
            incidents: vec![11],
            opened: lines.first().map(|l| l.at).unwrap_or(0),
            updated: lines.last().map(|l| l.at).unwrap_or(0),
            state: crate::cases::state_of(&lines).into(),
            open: true,
            arrival: arrivals.last().cloned(),
            arrivals,
            lines,
        }
    }

    fn notify() -> Vec<String> {
        Send::default().notify
    }

    fn places() -> crate::places::Settings {
        crate::places::Settings {
            places: vec![crate::places::Place { id: "p-general".into(), name: "Example General".into(), enabled: true, dest: "d-general".into(), ..Default::default() }],
        }
    }

    fn arrest() -> CaseView {
        let t0 = 1_000_000;
        case(
            vec![
                line(t0, "dispatched", "Dispatched as Unconscious", "page"),
                line(t0 + 60, "repage", "Repaged", "page"),
                line(t0 + 400, "working", "Working arrest", "readback"),
                line(t0 + 402, "working", "Repaged as a working arrest", "page"),
                line(t0 + 900, "rosc", "ROSC", "crew"),
                line(t0 + 960, "rosc", "ROSC", "readback"),
                report(t0 + 1200, 3, vec![Fact { key: "witnessed".into(), value: "yes".into() }, Fact { key: "rhythm".into(), value: "VF".into() }]),
                Line { inferred: true, ..line(t0 + 1500, "rearrest", "Lost pulses", "crew") },
                report(t0 + 1560, 4, vec![]),
                report(t0 + 1800, 5, vec![]),
            ],
            vec![arrival(3, t0 + 1190, Some("10 minutes")), arrival(4, t0 + 1550, Some("4 minutes")), arrival(5, t0 + 1790, Some("5 minutes"))],
        )
    }

    #[test]
    fn the_timeline_reads_as_the_ed_needs_it() {
        let text = render(&arrest());
        assert!(text.starts_with("🫀 Cardiac arrest · reported to hospital · 1200 Example St"), "{text}");
        assert!(text.contains("Engine 5, Medic 7"));
        assert!(!text.contains("Repaged\n"), "a plain repage is not a line: {text}");
        assert!(text.contains("ROSC (dispatcher)"));
        assert!(text.contains("Lost pulses (crew, inferred)"));
        let mut twice = arrest();
        twice.lines.push(line(1_003_000, "arrived", "At the hospital", "crew"));
        twice.lines.push(line(1_006_000, "arrived", "At the hospital", "crew"));
        assert_eq!(render(&twice).matches("At the hospital").count(), 1);
        assert!(text.contains("Expected at Example General: about"));
        assert!(text.contains("Witnessed: yes · Bystander CPR: not stated · Rhythm: VF"), "{text}");
        // Before any report, the facts line would only say "not stated".
        let early = case(arrest().lines[..3].to_vec(), vec![]);
        assert!(!render(&early).contains("Witnessed"));
    }

    #[test]
    fn the_address_opens_google_maps_at_the_pin() {
        let mut k = arrest();
        let text = render(&k);
        let html = linked(&k, &text, "Testville, EX").expect("an address to link");
        assert!(html.contains("<a href=\"https://www.google.com/maps/search/?api=1&amp;query=1200+Example+St%2C+Testville%2C+EX\">1200 Example St</a>"), "{html}");
        k.lat = Some(39.5);
        k.lon = Some(-86.25);
        let html = linked(&k, &text, "Testville, EX").unwrap();
        assert!(html.contains("query=39.500000%2C-86.250000\">1200 Example St</a>"), "{html}");
        // Everything else is the timeline as it was, escaped for HTML.
        assert!(html.contains("ROSC (dispatcher)"));
        k.address.clear();
        assert_eq!(linked(&k, &render(&k), "Testville, EX"), None);
    }

    #[test]
    fn the_chat_for_every_case_is_owed_one_map_while_the_case_goes_on() {
        let k = arrest();
        let s = Send { dest: "d-all".into(), ..Send::default() };
        let all = &threads(&k, &s, &places())[0];
        let hosp = &threads(&k, &s, &places())[1];
        let now = k.lines.last().unwrap().at + 30;
        assert!(map_due(&k, all, &s, None, now));
        let mut had = Sent { target: "x".into(), root_id: 77, rendered: render(&k), map_sent: false, notices: HashSet::new() };
        assert!(map_due(&k, all, &s, Some(&had), now), "a map not sent yet is still owed");
        had.map_sent = true;
        assert!(!map_due(&k, all, &s, Some(&had), now), "once");
        assert!(!map_due(&k, hosp, &s, None, now), "a hospital's chat has its own map");
        assert!(!map_due(&k, all, &Send { map: false, ..s.clone() }, None, now), "maps can be switched off");
        assert!(!map_due(&k, all, &s, None, now + NOTIFY_WITHIN_SECS), "not for a case gone quiet");
        assert!(!map_due(&CaseView { open: false, ..k.clone() }, all, &s, None, now), "not for a case that is over");
    }

    #[test]
    fn a_long_timeline_keeps_its_first_and_latest_lines() {
        let mut lines = vec![line(0, "dispatched", "Dispatched as Unconscious", "page")];
        for i in 1..400 {
            lines.push(line(i * 10, "working", &format!("Working arrest, said again {i}"), "crew"));
        }
        let text = render(&case(lines, vec![]));
        assert!(text.chars().count() <= 4096);
        assert!(text.contains("Dispatched as Unconscious"));
        assert!(text.contains("said again 399"));
        assert!(text.contains("earlier lines"));
    }

    #[test]
    fn only_the_events_that_matter_buzz_and_each_once() {
        let n = notices(&arrest(), &notify());
        let keys: Vec<&str> = n.iter().map(|x| x.key.as_str()).collect();
        // Working once for the readback and the repage; ROSC once for the
        // crew and the dispatcher; the second report's "4 minutes", said six
        // minutes later, lands where the first said, which is not news; the
        // third moves it five minutes.
        assert_eq!(keys, vec!["working", "rosc:1", "report:3", "rearrest:1", "report:5"]);
        assert!(n[3].text.contains("inferred"));
        assert!(n[2].text.contains("said 10 minutes"));
        // The same case rebuilt gives the same keys.
        assert_eq!(notices(&arrest(), &notify()).iter().map(|x| x.key.clone()).collect::<Vec<_>>(), keys);
        // A kind the listener dropped is not sent.
        let quiet: Vec<String> = notify().into_iter().filter(|k| k != "working").collect();
        assert!(!notices(&arrest(), &quiet).iter().any(|x| x.key == "working"));
    }

    #[test]
    fn a_hospital_hears_the_case_once_a_crew_calls_it() {
        let s = Send { dest: "d-all".into(), ..Send::default() };
        let k = arrest();
        let t = threads(&k, &s, &places());
        assert_eq!(t.iter().map(|x| x.dest.as_str()).collect::<Vec<_>>(), vec!["d-all", "d-general"]);
        assert_eq!(t[1].since, k.arrivals[0].anchor);
        // Before the report, only the chat for every case.
        let early = case(k.lines[..6].to_vec(), vec![]);
        assert_eq!(threads(&early, &s, &places()).len(), 1);
        // Its first message is the report, so it is not also a reply; what
        // came before it is in that message.
        let hosp = notices_for(&t[1], &notices(&k, &notify()));
        assert_eq!(hosp.iter().map(|x| x.key.as_str()).collect::<Vec<_>>(), vec!["rearrest:1", "report:5"]);
        // Hospitals' chats can be left out.
        assert_eq!(threads(&k, &Send { hospitals: false, ..s }, &places()).len(), 1);
    }

    #[test]
    fn a_thread_starts_with_the_story_so_far_and_then_buzzes_only_for_what_is_new() {
        let k = arrest();
        let t = Thread { dest: "d-all".into(), since: k.opened, place_id: None, conversation: None };
        let now = k.lines.last().unwrap().at + 30;
        let first = plan(&k, &t, &notify(), None, now);
        assert!(matches!(first[0], Step::Root { .. }));
        assert!(first[1..].iter().all(|s| matches!(s, Step::Stale { .. })), "a new thread buzzes for nothing already in it");
        // Later: one new event.
        let mut had = Sent { target: "x".into(), root_id: 77, rendered: render(&k), map_sent: false, notices: HashSet::new() };
        for s in &first[1..] {
            if let Step::Stale { notice } = s {
                had.notices.insert(notice.key.clone());
            }
        }
        assert!(plan(&k, &t, &notify(), Some(&had), now).is_empty(), "nothing changed, nothing sent");
        let mut later = k.clone();
        later.lines.push(line(now + 60, "terminated", "Efforts ceased", "readback"));
        later.state = "terminated".into();
        let steps = plan(&later, &t, &notify(), Some(&had), now + 90);
        assert!(matches!(&steps[0], Step::Edit { root_id: 77, .. }));
        assert!(matches!(&steps[1], Step::Reply { root_id: Some(77), notice } if notice.key == "terminated"));
        assert_eq!(steps.len(), 2);
        // Twenty minutes late, the edit still goes and the buzz does not.
        let late = plan(&later, &t, &notify(), Some(&had), now + 60 + NOTIFY_WITHIN_SECS + 1);
        assert!(matches!(&late[1], Step::Stale { .. }));
        // A case that ended long ago gets no new thread.
        assert!(plan(&later, &t, &notify(), None, now + 60 + NOTIFY_WITHIN_SECS + 1).is_empty());
    }

    /// Against a copy of a library whose cases are built (run the cases
    /// real_library test on it first): `HS_SEND_DB=copy.db
    /// HS_CASES_PLACES=places.json cargo test casesend::tests::real_library
    /// -- --ignored --nocapture`. Every place with the feature named in
    /// `HS_SEND_FEATURE` is given a chat, to see what those would hear.
    #[test]
    #[ignore]
    fn real_library() {
        let c = Connection::open(std::env::var("HS_SEND_DB").unwrap()).unwrap();
        let mut places: crate::places::Settings =
            serde_json::from_str(&std::fs::read_to_string(std::env::var("HS_CASES_PLACES").unwrap()).unwrap()).unwrap();
        let feature = std::env::var("HS_SEND_FEATURE").unwrap_or_else(|_| "ecmo".into());
        let mut names: HashMap<String, String> = [("d-all".to_string(), "every case".to_string())].into();
        for p in places.places.iter_mut().filter(|p| p.features.contains(&feature)) {
            p.dest = format!("d-{}", p.id);
            names.insert(p.dest.clone(), format!("hospital {}", p.id));
        }
        let (to,): (i64,) = c.query_row("SELECT MAX(start) FROM calls", [], |r| Ok((r.get(0)?,))).unwrap();
        let view = crate::cases::list(&c, 0, &places, to);
        let s = Send { dest: "d-all".into(), ..Send::default() };
        let p = preview(&view, &s, &places, &names);
        for d in &p.days {
            println!("{} {:<24} threads {:>2} replies {:>2}", d.date, d.dest, d.threads, d.replies);
        }
        for k in p.cases.iter().filter(|k| k.chats.len() > 1 || k.replies.len() > 2).take(4) {
            println!("\n=== case {} → {:?}\n{}\n--- replies\n{}", k.id, k.chats, k.timeline, k.replies.join("\n"));
        }
    }

    #[test]
    fn the_preview_counts_what_each_chat_would_hear() {
        let s = Send { dest: "d-all".into(), ..Send::default() };
        let view = crate::cases::CasesView { cases: vec![arrest()], unplaced: vec![] };
        let names: HashMap<String, String> = [("d-all".to_string(), "All arrests".to_string()), ("d-general".to_string(), "Example General".to_string())].into();
        let p = preview(&view, &s, &places(), &names);
        let all = p.days.iter().find(|d| d.dest == "All arrests").unwrap();
        assert_eq!((all.threads, all.replies), (1, 5));
        let general = p.days.iter().find(|d| d.dest == "Example General").unwrap();
        assert_eq!((general.threads, general.replies), (1, 2));
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
        let none = preview(&view, &Send::default(), &crate::places::Settings::default(), &names);
        assert_eq!(none.warnings.len(), 2);
    }
}
