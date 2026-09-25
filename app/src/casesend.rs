//! Cases on Telegram.
//!
//! A case is three messages in a chat, and no more:
//!
//! 1. The page heard, with the timeline as its caption. The caption is
//!    edited in place as the case grows — an edit is silent, so nobody's
//!    phone buzzes for a repage — and it carries every word about the case:
//!    where it is, how it stands, who is on it, what happened line by line,
//!    what the crew told the hospital and when to expect them.
//! 2. The map, under it: the run and its routes to where its care pathway
//!    says to go. Taken down when the case ends, which is what concludes
//!    the thread.
//! 3. The crew's report to the hospital, heard: the clip of that call, under
//!    the first message, with one line to say what it is. Its summary goes
//!    into the timeline above, not under the clip.
//!
//! Which chats: the profile's own chat hears every case from dispatch. A
//! hospital's chat (its place's destination) hears a case once a crew has
//! called that hospital, and its first message is that report heard, with
//! the whole timeline so far as its caption. Nothing is sent to a hospital
//! on a guess about where the patient is going.
//!
//! Everything that decides what to send is pure and tested here; the loop at
//! the bottom only compares that with what the library says went out.

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use tauri::{AppHandle, Manager};

use crate::cases::{CaseView, Line};
use crate::AppState;

type Db = std::sync::Arc<std::sync::Mutex<Connection>>;

/// A clip older than this is not sent: after a restart, or a sending
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
pub const TEXT_CHARS: usize = 3900;
/// Telegram's limit on a caption is 1024 characters after its markup is
/// parsed, and the sender splits a longer one into a message of its own;
/// the timeline under the page heard keeps inside that.
pub const CAPTION_CHARS: usize = 1000;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Send {
    pub enabled: bool,
    /// The named destination every case goes to. Blank is none.
    pub dest: String,
    /// Also each hospital's own chat, once a report to it is heard.
    pub hospitals: bool,
    /// A map under a thread's first message: the run with its routes to the
    /// hospitals its care pathway names (the nearest, the ECMO centre) in the
    /// chat for every case, scene to hospital in a hospital's chat. Deleted
    /// when the case ends.
    pub map: bool,
    /// The radio with it: the thread's first message is the page heard (in a
    /// hospital's chat, the report), with the timeline as its caption, and a
    /// crew's report to a hospital follows as the clip of that call. Off,
    /// the timeline is a plain message and nothing follows it but the map.
    pub audio: bool,
    /// Also by email: every case to `email_to`, and with `hospitals`, each
    /// hospital's own addresses once a report to it is heard.
    pub email: bool,
    pub email_to: String,
}

impl Default for Send {
    fn default() -> Self {
        Send { enabled: false, dest: String::new(), hospitals: true, map: true, audio: true, email: false, email_to: String::new() }
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

/// Who said a line, for a reader: "dispatcher", "crew", and whether the
/// run itself went unnamed — an event placed on the only arrest open at
/// the time says so, rather than a bare "inferred".
fn said_by(l: &Line) -> String {
    let who = match l.source.as_str() {
        "readback" => "dispatcher",
        "crew" => "crew",
        _ => "",
    };
    match (who, l.inferred) {
        ("", false) => String::new(),
        ("", true) => "run not named on the air".into(),
        (w, false) => w.into(),
        (w, true) => format!("{w}, run not named on the air"),
    }
}

/// The fact's key, how it is labelled, and how it is named when unstated.
const FACTS_SHOWN: &[(&str, &str, &str)] = &[
    ("witnessed", "Witnessed", "witnessed"),
    ("bystander cpr", "Bystander CPR", "bystander CPR"),
    ("rhythm", "Rhythm", "rhythm"),
    ("downtime", "Downtime (as said)", "downtime"),
    ("history", "History", "history"),
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

/// The number a case goes by, in its messages and on its recordings: the
/// dispatch map's own number for the run, so the app finds it by the same
/// one. Never the talkgroup, whose "49F" reads as a patient's age and sex.
pub fn number(k: &CaseView) -> String {
    format!("Incident #{}", k.incident)
}

/// Where a case stands, as it heads the message: a colour and the words.
pub(crate) fn banner(state: &str) -> (&'static str, String) {
    match state {
        "dispatched" => ("🟡", "DISPATCHED".into()),
        "working" => ("🔴", "WORKING ARREST".into()),
        "rosc" => ("💚", "ROSC · PULSES BACK".into()),
        "transporting" => ("🚑", "TRANSPORTING".into()),
        "reported" => ("🏥", "HOSPITAL NOTIFIED".into()),
        "arrived" => ("🏥", "AT THE HOSPITAL".into()),
        "terminated" => ("⚫", "EFFORTS CEASED".into()),
        "downgraded" => ("⚪", "NOT A CARDIAC ARREST".into()),
        other => ("🔵", state_label(other).to_uppercase()),
    }
}

fn esc(s: &str) -> String {
    crate::alerts::html_escape(s)
}

/// What a line of the message is for, when it has to be shortened: the
/// lines that give way first are named, so the ones that matter stay.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Part {
    #[default]
    Fixed,
    /// A report's summary, under its line.
    Summary,
    /// What the report did not say.
    Unstated,
}

/// A line of a message twice over: as plain text, which is what is kept and
/// compared, and as Telegram HTML, which is what goes out.
#[derive(Clone, Debug, Default)]
struct Row {
    plain: String,
    html: String,
    part: Part,
}

impl Row {
    fn new(plain: String, html: String) -> Row {
        Row { plain, html, part: Part::Fixed }
    }
}

/// The timeline message: what it is and where, how it stands and who is on
/// it; then what happened, a line each, with what the crew told the
/// hospital under the line that says they called; then when to expect
/// them and what the report said about the patient.
fn compose(k: &CaseView, region: &str) -> (Vec<Row>, Vec<Row>, Vec<Row>) {
    let mut head = Vec::new();
    let place = if k.address.is_empty() { "address not heard".to_string() } else { k.address.clone() };
    let url = crate::alerts::maps_url(k.lat, k.lon, &k.address, region);
    let place_html = if k.address.is_empty() || url.is_empty() {
        esc(&place)
    } else {
        format!("<a href=\"{}\">{}</a>", esc(&url), esc(&place))
    };
    head.push(Row::new(format!("🫀 {} · {place}", k.title), format!("🫀 <b>{} · {place_html}</b>", esc(&k.title))));
    let (icon, words) = banner(&k.state);
    head.push(Row::new(format!("{icon} {words}"), format!("{icon} <b>{}</b>", esc(&words))));
    if !k.units.is_empty() {
        let units = k.units.join(", ");
        head.push(Row::new(format!("🚒 {units}"), format!("🚒 {}", esc(&units))));
    }
    // The crew's own account, when it names no arrest at all. Said here
    // rather than left to the timeline, because the line at the top of this
    // message is what a clinician reads before anything else.
    if let Some(said) = &k.contested {
        head.push(Row::new(format!("⚠️ {said}"), format!("⚠️ <b>{}</b>", esc(said))));
    }
    // An address that never placed is only what the transcript heard, and
    // may be misheard: it must not read as a checked one.
    if !k.address.is_empty() && k.lat.is_none() {
        let s = "⚠️ Address as heard on the radio; not found on the map";
        head.push(Row::new(s.into(), format!("<i>{}</i>", esc(s))));
    }

    // A repage that says nothing new is not a line, and a crew at the
    // hospital said again an hour later is not a second arrival.
    let first_arrived = k.lines.iter().position(|l| l.kind == crate::cases::ARRIVED);
    let mut timeline = Vec::new();
    for (i, l) in k.lines.iter().enumerate() {
        if (l.kind == "repage" && l.label == crate::cases::REPAGED) || (l.kind == crate::cases::ARRIVED && Some(i) != first_arrived) {
            continue;
        }
        let clock = l.clock.clone().unwrap_or_else(|| hm(l.at));
        let by = said_by(l);
        timeline.push(Row::new(
            format!("{clock} {}{}", l.label, if by.is_empty() { String::new() } else { format!(" · {by}") }),
            format!("<b>{clock}</b> {}{}", esc(&l.label), if by.is_empty() { String::new() } else { format!(" <i>· {}</i>", esc(&by)) }),
        ));
        // What the crew told the hospital, as the summary put it: under the
        // line that says they called, so the story reads in order.
        if l.kind == "report" && !l.detail.trim().is_empty() {
            let d = l.detail.trim();
            timeline.push(Row { plain: format!("↳ {d}"), html: format!("↳ <i>{}</i>", esc(d)), part: Part::Summary });
        }
    }

    let facts = facts_of(k);
    let mut tail = Vec::new();
    // Once the case has ended nobody is expected anywhere; the line would
    // only mislead.
    if let Some(a) = k.arrival.as_ref().filter(|_| !concluded(k)) {
        let window = match (a.from, a.to) {
            (Some(f), Some(t)) if f == t => format!("about {}", hm(f)),
            (Some(f), Some(t)) => format!("{}–{}", hm(f), hm(t)),
            _ => "no ETA said".into(),
        };
        let drive = a.drive_min.map(|d| format!(" · {d} min {} from the scene", a.drive_how)).unwrap_or_default();
        tail.push(Row::new(
            format!("🏥 Expected at {}: {window}{drive}", a.place),
            format!("🏥 <b>Expected at {}</b>: {}{}", esc(&a.place), esc(&window), esc(&drive)),
        ));
        if !a.note.is_empty() && a.from.is_some() {
            tail.push(Row::new(format!("⚠️ {}", a.note), format!("⚠️ <i>{}</i>", esc(&a.note))));
        }
    }
    if k.lines.iter().any(|l| l.kind == "report") {
        let who: Vec<&str> = ["age", "sex"].iter().filter_map(|f| facts.get(*f).map(String::as_str)).collect();
        if !who.is_empty() {
            let who = who.join(", ");
            tail.push(Row::new(format!("👤 Patient: {who}"), format!("👤 <b>Patient</b>: {}", esc(&who))));
        }
        let stated: Vec<(&str, &str)> = FACTS_SHOWN.iter().filter_map(|(key, label, _)| facts.get(*key).map(|v| (*label, v.as_str()))).collect();
        if !stated.is_empty() {
            tail.push(Row::new(
                format!("🩺 {}", stated.iter().map(|(l, v)| format!("{l}: {v}")).collect::<Vec<_>>().join(" · ")),
                format!("🩺 {}", stated.iter().map(|(l, v)| format!("{}: <b>{}</b>", esc(l), esc(v))).collect::<Vec<_>>().join(" · ")),
            ));
        }
        // The score, worked out from what was stated: a range while any of
        // its inputs is missing, never a guess.
        let score = crate::study::score(&facts.iter().map(|(k, v)| (k.clone(), v.clone())).collect());
        tail.push(Row::new(format!("📈 {}: {}", score.name, score.estimate), format!("📈 {}: <b>{}</b>", esc(&score.name), esc(&score.estimate))));
        let unstated: Vec<&str> = FACTS_SHOWN.iter().filter(|(key, _, _)| !facts.contains_key(*key)).map(|(_, _, name)| *name).collect();
        if !unstated.is_empty() {
            let s = format!("Not stated: {}", unstated.join(", "));
            tail.push(Row { plain: s.clone(), html: format!("<i>{}</i>", esc(&s)), part: Part::Unstated });
        }
    }
    (head, timeline, tail)
}

/// A summary is cut to no shorter than this before older lines go: its
/// headline and first sentence are what the ED acts on.
const SUMMARY_MIN: usize = 200;

/// Cut the latest summary down by `over` characters, to no fewer than
/// `floor`; gone altogether if that leaves nothing worth reading.
fn trim_summary(timeline: &mut Vec<Row>, over: usize, floor: usize) {
    let Some(i) = timeline.iter().position(|r| r.part == Part::Summary) else { return };
    let len = timeline[i].plain.chars().count();
    let keep = len.saturating_sub(over + 1).max(floor.min(len));
    if keep + 1 >= len {
        return;
    }
    if keep <= 8 {
        timeline.remove(i);
        return;
    }
    let short: String = timeline[i].plain.chars().take(keep).collect::<String>().trim_end().to_string() + "…";
    let body = short.trim_start_matches("↳ ").to_string();
    timeline[i] = Row { plain: short, html: format!("↳ <i>{}</i>", esc(&body)), part: Part::Summary };
}

/// Make the message fit `cap` characters, giving up the least first: the
/// summaries of earlier reports (the latest is the one the ED acts on),
/// then what the report did not say, then the tail of the latest summary
/// down to its headline and first sentence, then the oldest lines after
/// the first — said so, since the first line and the latest are what the
/// ED needs — and last whatever is left of that summary.
fn fit(head: &[Row], mut timeline: Vec<Row>, mut tail: Vec<Row>, cap: usize) -> (Vec<Row>, Vec<Row>) {
    let size = |t: &[Row], tail: &[Row]| head.iter().chain(t).chain(tail).map(|x| x.plain.chars().count() + 1).sum::<usize>() + 40;
    if size(&timeline, &tail) > cap {
        if let Some(last) = timeline.iter().rposition(|r| r.part == Part::Summary) {
            timeline = timeline.into_iter().enumerate().filter(|(i, r)| r.part != Part::Summary || *i == last).map(|(_, r)| r).collect();
        }
    }
    if size(&timeline, &tail) > cap {
        tail.retain(|r| r.part != Part::Unstated);
    }
    if size(&timeline, &tail) > cap {
        let over = size(&timeline, &tail) - cap;
        trim_summary(&mut timeline, over, SUMMARY_MIN);
    }
    // The line that says lines were dropped takes room of its own.
    let marker = |n: usize| if n > 0 { format!("… {n} earlier lines") } else { String::new() };
    let mut dropped = 0;
    while timeline.len() > 2 && size(&timeline, &tail) + marker(dropped).chars().count() > cap {
        timeline.remove(1);
        dropped += 1;
    }
    if dropped > 0 {
        let s = marker(dropped);
        timeline.insert(1, Row::new(s.clone(), format!("<i>{}</i>", esc(&s))));
    }
    if size(&timeline, &tail) > cap {
        let over = size(&timeline, &tail) - cap;
        trim_summary(&mut timeline, over, 0);
    }
    (timeline, tail)
}

/// The timeline message, as plain text within `cap` characters: what is
/// kept, compared to know when to edit, and shown in the Cases tab's
/// preview.
pub fn render(k: &CaseView, cap: usize) -> String {
    let (head, timeline, tail) = compose(k, "");
    let (timeline, tail) = fit(&head, timeline, tail, cap);
    let mut out: Vec<&str> = head.iter().map(|r| r.plain.as_str()).collect();
    for block in [&timeline[..], &tail[..]] {
        if !block.is_empty() {
            out.push("");
            out.extend(block.iter().map(|r| r.plain.as_str()));
        }
    }
    let number = number(k);
    out.push("");
    out.push(&number);
    out.join("\n")
}

/// The same message as Telegram HTML: the headline bold with its address
/// opening Google Maps at the run's pin (or, with none yet, at the
/// address), the timeline set off as a quote, and the incident number in
/// monospace, which Telegram copies with a tap.
pub fn html(k: &CaseView, region: &str, cap: usize) -> String {
    let (head, timeline, tail) = compose(k, region);
    let (timeline, tail) = fit(&head, timeline, tail, cap);
    let mut out = head.iter().map(|r| r.html.clone()).collect::<Vec<_>>().join("\n");
    if !timeline.is_empty() {
        out.push_str(&format!("\n<blockquote>{}</blockquote>", timeline.iter().map(|r| r.html.as_str()).collect::<Vec<_>>().join("\n")));
    }
    if !tail.is_empty() {
        out.push_str(&format!("\n{}", tail.iter().map(|r| r.html.as_str()).collect::<Vec<_>>().join("\n")));
    }
    out.push_str(&format!("\n\n<code>{}</code>", esc(&number(k))));
    out
}

/// Whether the chat for every case is still owed its map: once, while the
/// case is going on. A map turning up for a case that ended long ago, or
/// after an upgrade for every recent one, would be noise.
pub fn map_due(k: &CaseView, t: &Thread, s: &Send, had: Option<&Sent>, now: i64) -> bool {
    let last = k.lines.last().map(|l| l.at).unwrap_or(k.opened);
    s.map && t.place_id.is_none() && !had.is_some_and(|h| h.map_sent) && k.open && now - last <= NOTIFY_WITHIN_SECS
}

/// Whether the case has ended: efforts ceased, or it was not an arrest.
/// That is when the map comes down.
pub fn concluded(k: &CaseView) -> bool {
    matches!(k.state.as_str(), "terminated" | "downgraded")
}

// ---------------------------------------------------------------------------
// what is heard
// ---------------------------------------------------------------------------

/// A call worth hearing: the page that opened the thread, or a crew's
/// report to a hospital.
#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    /// Stable across rebuilds: never a row id.
    pub key: String,
    pub at: i64,
    /// One line, under the clip: what it is and when.
    pub text: String,
    /// The same, as Telegram HTML.
    pub html: String,
    /// What happened, in a few words: the recording's title.
    pub name: String,
    /// A report's audio is every transmission of it.
    pub conversation: Option<i64>,
    /// The call that said it, whose recording goes with it.
    pub call: Option<i64>,
}

/// A clip's one line: the event in bold and its time.
fn event_text(icon: &str, words: &str, when: &str) -> (String, String) {
    (format!("{icon} {words} · {when}"), format!("{icon} <b>{}</b> · {when}", esc(words)))
}

/// The reports worth hearing, oldest first, each with a key that the same
/// call gets on every rebuild: the first report to a hospital, and a later
/// one only when its ETA moved, or it gave one where the last did not.
/// Everything else the case says is in the timeline already.
pub fn notices(k: &CaseView) -> Vec<Notice> {
    let mut out = Vec::new();
    let mut last_to: Option<Option<i64>> = None;
    for l in k.lines.iter().filter(|l| l.kind == "report") {
        let Some(conv) = l.conversation else { continue };
        let a = k.arrivals.iter().find(|a| a.conversation == conv);
        let to = a.and_then(|a| a.to);
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
        let place = a.map(|a| a.place.clone()).unwrap_or_default();
        let place = if place.is_empty() { "the hospital".to_string() } else { place };
        let when = l.clock.clone().unwrap_or_else(|| hm(l.at));
        let words = format!("Report to {place}");
        let (text, html) = event_text("🏥", &words, &when);
        out.push(Notice { key: format!("report:{conv}"), at: l.at, text, html, name: words, conversation: Some(conv), call: l.call });
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
///
/// A hospital's chat is for the patient coming to it. When the crew's report
/// to that hospital names no arrest — a seizure paged as a cardiac arrest —
/// the case is not that hospital's business: its chat is often the team that
/// answers an arrest, and it would be paged for a patient the crew never
/// said was one. The chat for every case still hears it, with the warning.
pub fn threads(k: &CaseView, s: &Send, places: &crate::places::Settings) -> Vec<Thread> {
    let mut out = Vec::new();
    if !s.dest.is_empty() {
        out.push(Thread { dest: s.dest.clone(), since: k.opened, place_id: None, conversation: None });
    }
    if s.hospitals {
        for (a, p) in hospitals(k, places) {
            if p.dest.is_empty() || out.iter().any(|t| t.dest == p.dest) {
                continue;
            }
            out.push(Thread { dest: p.dest.clone(), since: a.anchor, place_id: Some(p.id.clone()), conversation: Some(a.conversation) });
        }
    }
    out
}

/// The hospitals a case has been reported to, each with the report that
/// brought it in — leaving out any the crew told of something other than
/// an arrest.
fn hospitals<'a>(k: &'a CaseView, places: &'a crate::places::Settings) -> impl Iterator<Item = (&'a crate::cases::Arrival, &'a crate::places::Place)> {
    k.arrivals.iter().filter(|a| !no_arrest_reported(k, a.conversation)).filter_map(|a| {
        places.places.iter().find(|p| p.enabled && !a.place_id.is_empty() && p.id == a.place_id).map(|p| (a, p))
    })
}

/// The crew's report to a hospital, on an arrest case, names no arrest.
/// Read the same way as the case's own warning, so the two agree.
fn no_arrest_reported(k: &CaseView, conversation: i64) -> bool {
    k.profile == "cardiac-arrest"
        && k.state != "downgraded"
        && k.lines
            .iter()
            .any(|l| l.kind == "report" && l.conversation == Some(conversation) && crate::cases::contradicts_arrest(&l.detail))
}

/// What a thread opens with, heard: the page that opened the case, in the
/// chat for every case, and the report that brought a hospital's chat in,
/// in that chat. The timeline is its caption, so this is only the audio
/// and its title.
pub fn intro(k: &CaseView, t: &Thread) -> Option<Notice> {
    let (key, l, name, icon) = match t.conversation {
        None => {
            let l = k.lines.iter().find(|l| l.kind == "dispatched" && l.call.is_some())?;
            ("audio:page".to_string(), l, "Dispatch page".to_string(), "📻")
        }
        Some(conv) => {
            let l = k.lines.iter().find(|l| l.kind == "report" && l.conversation == Some(conv))?;
            let place = l.label.trim_start_matches("Report to ").split(" · ").next().unwrap_or("the hospital");
            (format!("audio:report:{conv}"), l, format!("Report to {place}"), "🏥")
        }
    };
    let when = l.clock.clone().unwrap_or_else(|| hm(l.at));
    let (text, html) = event_text(icon, &name, &when);
    Some(Notice { key, at: l.at, text, html, name, conversation: l.conversation, call: l.call })
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
    // Whether the first message is the page heard (its timeline is a
    // caption, edited as one) and the map's own message, so it can be
    // taken down. Threads from before either are text with no map id,
    // which is what the defaults say.
    let _ = c.execute("ALTER TABLE case_sends ADD COLUMN root_audio INTEGER NOT NULL DEFAULT 0", []);
    let _ = c.execute("ALTER TABLE case_sends ADD COLUMN map_id INTEGER", []);
    ensure_email_schema(c);
}

#[derive(Clone, Debug, Default)]
pub struct Sent {
    pub target: String,
    pub root_id: i64,
    /// The first message is an audio message, so its timeline is a caption.
    pub root_audio: bool,
    pub rendered: String,
    pub map_sent: bool,
    /// The map's message, while it is up.
    pub map_id: Option<i64>,
    pub notices: HashSet<String>,
}

pub fn sent(c: &Connection, profile: &str, incident: i64, dest: &str) -> Option<Sent> {
    let mut s: Sent = c
        .query_row(
            "SELECT target, root_id, rendered, map_sent, root_audio, map_id FROM case_sends WHERE profile = ?1 AND incident = ?2 AND dest = ?3",
            params![profile, incident, dest],
            |r| {
                Ok(Sent {
                    target: r.get(0)?,
                    root_id: r.get(1)?,
                    rendered: r.get(2)?,
                    map_sent: r.get::<_, i64>(3)? != 0,
                    root_audio: r.get::<_, i64>(4)? != 0,
                    map_id: r.get(5)?,
                    notices: HashSet::new(),
                })
            },
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
    /// Start the thread: the timeline as a new message — the caption of the
    /// call heard, when there is one to hear.
    Root { text: String, heard: Option<Notice> },
    /// Bring the timeline up to date, silently.
    Edit { root_id: i64, text: String },
    /// A report heard, as a reply to the timeline.
    Reply { root_id: Option<i64>, notice: Notice },
    /// Nobody hears this one; it is recorded as dealt with.
    Stale { notice: Notice },
    /// The case has ended: the map comes down.
    DropMap { map_id: i64 },
}

/// The room the timeline has: a caption's, when it rides on the call heard.
fn cap_for(audio: bool) -> usize {
    if audio {
        CAPTION_CHARS
    } else {
        TEXT_CHARS
    }
}

/// What one chat needs for one case, from what it already has.
pub fn plan(k: &CaseView, t: &Thread, s: &Send, had: Option<&Sent>, now: i64) -> Vec<Step> {
    let heard = if s.audio { intro(k, t) } else { None };
    let cap = cap_for(had.map_or(heard.is_some(), |h| h.root_audio));
    let text = render(k, cap);
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
            if now - last > START_WITHIN_SECS || (concluded(k) && now - last > NOTIFY_WITHIN_SECS) {
                return steps;
            }
            steps.push(Step::Root { text: text.clone(), heard });
            None
        }
    };
    let done = had.map(|h| &h.notices);
    if s.audio {
        for n in notices_for(t, &notices(k)) {
            if done.is_some_and(|d| d.contains(&n.key)) {
                continue;
            }
            // A new thread already shows everything up to now in its first
            // message; a clip for each call in it would be the noise this
            // is built to avoid.
            if had.is_none() || now - n.at > NOTIFY_WITHIN_SECS {
                steps.push(Step::Stale { notice: n });
            } else {
                steps.push(Step::Reply { root_id: root, notice: n });
            }
        }
    }
    if concluded(k) {
        if let Some(id) = had.and_then(|h| h.map_id) {
            steps.push(Step::DropMap { map_id: id });
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
    let on = |p: &crate::cases::Profile| p.enabled && (p.telegram.enabled || p.telegram.email);
    if !settings.profiles.iter().any(on) {
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
    for p in settings.profiles.iter().filter(|p| on(p)) {
        for k in view.cases.iter().filter(|k| k.profile == p.id) {
            send_case_emails(app, &db, p, k, &places, now);
            if !p.telegram.enabled {
                continue;
            }
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
                let steps = plan(k, &t, &p.telegram, had.as_ref(), now);
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
            Step::Root { text, heard } => {
                // The timeline was rendered to fit a caption when there is a
                // call to hear; the markup follows the same cut whether or
                // not the clip can be put together.
                let cap = cap_for(heard.is_some());
                let markup = html(k, &region, cap);
                let clip = heard.as_ref().and_then(|n| clip_of(db, k, n).map(|c| (n, c)));
                let (id, audio) = match clip {
                    Some((n, (path, mp3))) => {
                        let sent = crate::alerts::send_audio_reply(&target, &path, mp3, &text, Some(&markup), &clip_title(k, n), &number(k), None);
                        let _ = std::fs::remove_file(&path);
                        (sent?.last().copied().ok_or("Telegram audio had no message id")?, true)
                    }
                    None => (crate::alerts::send_text_reply_html(&target, &text, Some(&markup), None)?, false),
                };
                root = Some(id);
                let c = db.lock().unwrap();
                c.execute(
                    "INSERT OR REPLACE INTO case_sends (profile, incident, dest, target, root_id, rendered, since, map_sent, sent_at, updated_at, root_audio, map_id)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8, ?8, ?9, NULL)",
                    params![p.id, k.incident, t.dest, target, id, text, t.since, now, audio as i64],
                )
                .map_err(|e| e.to_string())?;
            }
            Step::Edit { root_id, text } => {
                let audio = had.as_ref().is_some_and(|h| h.root_audio);
                let markup = html(k, &region, cap_for(audio));
                let edited = if audio {
                    crate::alerts::edit_message_caption_html(&target, root_id, &text, Some(&markup))
                } else {
                    crate::alerts::edit_message_html(&target, root_id, &text, Some(&markup))
                };
                if let Err(e) = edited {
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
                // The words are in the timeline already; a report with no
                // recording to hand has nothing to add under it.
                let id = match clip_of(db, k, &notice) {
                    Some((path, mp3)) => {
                        let sent = crate::alerts::send_audio_reply(&target, &path, mp3, &notice.text, Some(&notice.html), &clip_title(k, &notice), &number(k), root);
                        let _ = std::fs::remove_file(&path);
                        Some(sent?.last().copied().ok_or("Telegram audio had no message id")?)
                    }
                    None => None,
                };
                record_notice(db, p, k, t, &notice, id, now)?;
            }
            Step::Stale { notice } => record_notice(db, p, k, t, &notice, None, now)?,
            Step::DropMap { map_id } => {
                // Already gone, or past Telegram's window: either way there
                // is nothing more to take down, so it is not tried again.
                if let Err(e) = crate::alerts::delete_message(&target, map_id) {
                    eprintln!("cases: taking down the map for case {} in {}: {e}", k.id, t.dest);
                }
                let c = db.lock().unwrap();
                c.execute(
                    "UPDATE case_sends SET map_id = NULL WHERE profile = ?1 AND incident = ?2 AND dest = ?3",
                    params![p.id, k.incident, t.dest],
                )
                .map_err(|e| e.to_string())?;
            }
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
                    Ok(id) => record_map(db, p, k, t, id)?,
                    // The map waits on its own: a tile server down must not
                    // hold up the thread's next clip.
                    Err(e) => {
                        eprintln!("cases: map for case {} in {}: {e}", k.id, t.dest);
                        back_off(&map_key, now);
                    }
                }
            }
        }
    }
    // The map goes once, under the first message in a hospital's chat.
    if p.telegram.map && t.place_id.is_some() && k.open && !had.as_ref().is_some_and(|h| h.map_sent) {
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
                match sent {
                    Ok(id) => record_map(db, p, k, t, id)?,
                    // Tried again on the next pass that has something to do
                    // in this chat; a missing map is not worth a retry loop.
                    Err(e) => eprintln!("cases: map for case {} in {}: {e}", k.id, t.dest),
                }
            }
        }
    }
    Ok(())
}

fn record_map(db: &Db, p: &crate::cases::Profile, k: &CaseView, t: &Thread, id: i64) -> Result<(), String> {
    let c = db.lock().unwrap();
    c.execute(
        "UPDATE case_sends SET map_sent = 1, map_id = ?1 WHERE profile = ?2 AND incident = ?3 AND dest = ?4",
        params![id, p.id, k.incident, t.dest],
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// The recordings behind a notice, oldest first: every transmission of a
/// report, else the call that said it — with its second half, when the
/// radio heard a page as two calls.
fn recordings(c: &Connection, n: &Notice) -> Vec<String> {
    if let Some(conv) = n.conversation {
        let pieces: Option<String> = c.query_row("SELECT pieces FROM conversations WHERE id = ?1", [conv], |r| r.get(0)).optional().ok().flatten();
        if let Some(pieces) = pieces {
            let mut pieces: Vec<crate::conversations::Piece> = serde_json::from_str(&pieces).unwrap_or_default();
            pieces.sort_by_key(|p| p.at);
            return pieces.into_iter().filter_map(|p| p.audio).collect();
        }
    }
    let Some(call) = n.call else { return Vec::new() };
    let audio = |id: i64| c.query_row("SELECT audio FROM calls WHERE id = ?1", [id], |r| r.get::<_, Option<String>>(0)).optional().ok().flatten().flatten();
    let mut out: Vec<String> = audio(call).into_iter().collect();
    if let Some((rest, false)) = crate::dispatch::split_partner(c, call) {
        out.extend(audio(rest));
    }
    out
}

/// A notice as the radio said it: one file, named for the case and the
/// event so that saved or forwarded it keeps the number, and whether it is
/// MP3. None with no recording to hand, or one that cannot be put together.
fn clip_of(db: &Db, k: &CaseView, n: &Notice) -> Option<(std::path::PathBuf, bool)> {
    let files = recordings(&db.lock().unwrap(), n);
    let files: Vec<String> = files.into_iter().filter(|f| !f.is_empty() && std::path::Path::new(f).exists()).collect();
    if files.is_empty() {
        return None;
    }
    let (path, mp3) = match crate::alerts::combine_clips(&files, &format!("case_{}", k.incident)) {
        Ok(clip) => clip,
        Err(e) => {
            eprintln!("cases: audio for case {}: {e}", k.id);
            return None;
        }
    };
    let named = path.with_file_name(format!("{}.{}", clip_stem(k, n), if mp3 { "mp3" } else { "wav" }));
    Some((if std::fs::rename(&path, &named).is_ok() { named } else { path }, mp3))
}

/// The title on the player: what happened and where.
fn clip_title(k: &CaseView, n: &Notice) -> String {
    if k.address.is_empty() {
        n.name.clone()
    } else {
        format!("{} · {}", n.name, k.address)
    }
}

/// "incident-2048_2329_dispatch-page": the number, the time and the event.
pub fn clip_stem(k: &CaseView, n: &Notice) -> String {
    let slug: String = n
        .name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|w| !w.is_empty())
        .take(6)
        .collect::<Vec<_>>()
        .join("-");
    format!("incident-{}_{}_{slug}", k.incident, hm(n.at).replace(':', ""))
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
// by email
// ---------------------------------------------------------------------------
//
// Email cannot be edited, so a case by email is a thread of its own: the
// first email is the timeline so far, and each thing worth hearing about —
// the arrest confirmed working, ROSC, lost pulses, transport, efforts
// ceased, not an arrest after all, a report to a hospital — follows as a
// reply carrying the timeline as it then stands. Routine changes (a unit
// added, an ETA moved) wait for the next of those. The same hospitals
// that would hear it on Telegram hear it by email, from the same report.

/// One set of addresses a case goes to by email.
#[derive(Clone, Debug, PartialEq)]
pub struct EmailThread {
    /// `all`, or `place:<id>` for a hospital's addresses.
    pub key: String,
    pub to: Vec<String>,
    pub since: i64,
    pub conversation: Option<i64>,
}

pub fn email_threads(k: &CaseView, s: &Send, places: &crate::places::Settings) -> Vec<EmailThread> {
    let mut out = Vec::new();
    if !s.email {
        return out;
    }
    let all = crate::email::recipients(&s.email_to);
    if !all.is_empty() {
        out.push(EmailThread { key: "all".into(), to: all, since: k.opened, conversation: None });
    }
    if s.hospitals {
        for (a, p) in hospitals(k, places) {
            let to = crate::email::recipients(&p.email);
            if to.is_empty() || out.iter().any(|t| t.key == format!("place:{}", p.id)) {
                continue;
            }
            out.push(EmailThread { key: format!("place:{}", p.id), to, since: a.anchor, conversation: Some(a.conversation) });
        }
    }
    out
}

/// An email has no length limit worth the name; this only stops a runaway.
const EMAIL_CHARS: usize = 100_000;

/// The kinds of line that are news by email.
const EMAIL_EVENTS: &[&str] = &["working", "rosc", "rearrest", "transporting", "terminated", "downgrade", crate::cases::ARRIVED];

/// Something that earns a case its next email.
#[derive(Clone, Debug, PartialEq)]
pub struct Update {
    pub key: String,
    pub at: i64,
    pub text: String,
    pub html: String,
    pub conversation: Option<i64>,
    pub call: Option<i64>,
    pub name: String,
}

/// Every update in a case, in order: each event the first time it is said
/// (a readback of it is the same news), and each report heard.
pub fn updates(k: &CaseView) -> Vec<Update> {
    let mut out: Vec<Update> = Vec::new();
    let mut last_kind = "";
    for l in &k.lines {
        if !EMAIL_EVENTS.contains(&l.kind.as_str()) || l.kind == last_kind {
            continue;
        }
        last_kind = l.kind.as_str();
        let when = l.clock.clone().unwrap_or_else(|| hm(l.at));
        let (text, html) = event_text("•", &l.label, &when);
        out.push(Update { key: format!("{}:{}", l.kind, l.at), at: l.at, text, html, conversation: None, call: l.call, name: l.label.clone() });
    }
    for n in notices(k) {
        out.push(Update { key: n.key, at: n.at, text: n.text, html: n.html, conversation: n.conversation, call: n.call, name: n.name });
    }
    out.sort_by_key(|u| u.at);
    out
}

/// What one set of addresses has had of a case.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EmailSent {
    pub root: String,
    pub subject: String,
    pub keys: HashSet<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum EmailStep {
    /// The first email: the timeline so far, with the page (or, for a
    /// hospital, its report) heard.
    Open { subject: String },
    /// An update, as a reply carrying the timeline as it now stands.
    Follow { update: Update },
    /// Already in an email that was sent, or too old to be news.
    Stale { key: String },
}

pub fn plan_email(k: &CaseView, t: &EmailThread, had: Option<&EmailSent>, now: i64) -> Vec<EmailStep> {
    let mut steps = Vec::new();
    let mine: Vec<Update> = updates(k)
        .into_iter()
        .filter(|u| match t.conversation {
            None => true,
            Some(c) => u.at > t.since && u.conversation != Some(c),
        })
        .collect();
    match had {
        None => {
            let last = k.lines.last().map(|l| l.at).unwrap_or(k.opened);
            if now - last > START_WITHIN_SECS || (concluded(k) && now - last > NOTIFY_WITHIN_SECS) {
                return steps;
            }
            let subject = render(k, TEXT_CHARS).lines().next().unwrap_or("Case").to_string();
            steps.push(EmailStep::Open { subject });
            // The first email tells everything so far.
            steps.extend(mine.into_iter().map(|u| EmailStep::Stale { key: u.key }));
        }
        Some(h) => {
            for u in mine.into_iter().filter(|u| !h.keys.contains(&u.key)) {
                if now - u.at > NOTIFY_WITHIN_SECS {
                    steps.push(EmailStep::Stale { key: u.key });
                } else {
                    steps.push(EmailStep::Follow { update: u });
                }
            }
        }
    }
    steps
}

pub fn ensure_email_schema(c: &Connection) {
    let _ = c.execute_batch(
        "CREATE TABLE IF NOT EXISTS case_emails (
            profile TEXT NOT NULL,
            incident INTEGER NOT NULL,
            thread TEXT NOT NULL,
            root TEXT NOT NULL,
            subject TEXT NOT NULL,
            sent_at INTEGER NOT NULL,
            PRIMARY KEY (profile, incident, thread)
         );
         CREATE TABLE IF NOT EXISTS case_email_updates (
            profile TEXT NOT NULL,
            incident INTEGER NOT NULL,
            thread TEXT NOT NULL,
            key TEXT NOT NULL,
            sent_at INTEGER NOT NULL,
            PRIMARY KEY (profile, incident, thread, key)
         );",
    );
}

pub fn email_sent(c: &Connection, profile: &str, incident: i64, thread: &str) -> Option<EmailSent> {
    let (root, subject): (String, String) = c
        .query_row(
            "SELECT root, subject FROM case_emails WHERE profile = ?1 AND incident = ?2 AND thread = ?3",
            params![profile, incident, thread],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .ok()
        .flatten()?;
    let keys = c
        .prepare("SELECT key FROM case_email_updates WHERE profile = ?1 AND incident = ?2 AND thread = ?3")
        .and_then(|mut q| q.query_map(params![profile, incident, thread], |r| r.get(0)).map(|rows| rows.flatten().collect()))
        .unwrap_or_default();
    Some(EmailSent { root, subject, keys })
}

/// The recording that goes with an email: the page, or the report that
/// brought a hospital in; for an update, that report's clip.
fn email_clip(db: &Db, k: &CaseView, t: &EmailThread, update: Option<&Update>) -> Option<crate::email::Attachment> {
    let n = match update {
        Some(u) if u.conversation.is_some() => Notice { key: u.key.clone(), at: u.at, text: String::new(), html: String::new(), name: u.name.clone(), conversation: u.conversation, call: u.call },
        Some(_) => return None,
        None => intro(k, &Thread { dest: String::new(), since: t.since, place_id: None, conversation: t.conversation })?,
    };
    let (path, _) = clip_of(db, k, &n)?;
    let a = crate::email::Attachment::file(&path);
    let _ = std::fs::remove_file(&path);
    a
}

fn send_case_emails(app: &AppHandle, db: &Db, p: &crate::cases::Profile, k: &CaseView, places: &crate::places::Settings, now: i64) {
    let state = app.state::<AppState>();
    let region = state.dispatch.lock().unwrap().settings.region_hint.clone();
    for t in email_threads(k, &p.telegram, places) {
        let key = format!("{}:{}:email:{}", p.id, k.incident, t.key);
        if waiting(&key, now) {
            continue;
        }
        let had = {
            let c = db.lock().unwrap();
            email_sent(&c, &p.id, k.incident, &t.key)
        };
        let steps = plan_email(k, &t, had.as_ref(), now);
        let mut root = had.as_ref().map(|h| (h.root.clone(), h.subject.clone()));
        for step in steps {
            let res: Result<(), String> = match step {
                EmailStep::Open { subject } => {
                    let id = crate::email::thread_id(&["case", &p.id, &k.incident.to_string(), &t.key]);
                    let mut attachments: Vec<crate::email::Attachment> = Vec::new();
                    if p.telegram.audio {
                        attachments.extend(email_clip(db, k, &t, None));
                    }
                    if p.telegram.map && t.conversation.is_none() {
                        if let Ok((png, _, _)) = crate::tripwires::draw_map(app, &state, k.incident) {
                            attachments.push(crate::email::Attachment { name: "map.png".into(), mime: "image/png".into(), bytes: png });
                        }
                    }
                    let mail = crate::email::Mail {
                        to: t.to.clone(),
                        subject: subject.clone(),
                        text: render(k, EMAIL_CHARS),
                        html: Some(crate::email::html_body(&html(k, &region, EMAIL_CHARS))),
                        attachments,
                        message_id: Some(id.clone()),
                        in_reply_to: None,
                    };
                    crate::email::send(&mail).and_then(|_| {
                        let c = db.lock().unwrap();
                        c.execute(
                            "INSERT OR REPLACE INTO case_emails (profile, incident, thread, root, subject, sent_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                            params![p.id, k.incident, t.key, id, subject, now],
                        )
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                    })
                    .map(|_| root = Some((id, subject)))
                }
                EmailStep::Follow { update } => match &root {
                    None => Ok(()),
                    Some((id, subject)) => {
                        let mail = crate::email::Mail {
                            to: t.to.clone(),
                            subject: format!("Re: {subject}"),
                            text: format!("{}\n\n{}", update.text, render(k, EMAIL_CHARS)),
                            html: Some(crate::email::html_body(&format!("{}\n\n{}", update.html, html(k, &region, EMAIL_CHARS)))),
                            attachments: if p.telegram.audio { email_clip(db, k, &t, Some(&update)).into_iter().collect() } else { Vec::new() },
                            message_id: None,
                            in_reply_to: Some(id.clone()),
                        };
                        crate::email::send(&mail).and_then(|_| record_email_update(db, p, k, &t, &update.key, now))
                    }
                },
                EmailStep::Stale { key } => record_email_update(db, p, k, &t, &key, now),
            };
            if let Err(e) = res {
                eprintln!("cases: emailing case {} to {}: {e}", k.id, t.to.join(", "));
                back_off(&key, now);
                break;
            }
        }
    }
}

fn record_email_update(db: &Db, p: &crate::cases::Profile, k: &CaseView, t: &EmailThread, key: &str, now: i64) -> Result<(), String> {
    let c = db.lock().unwrap();
    c.execute(
        "INSERT OR IGNORE INTO case_email_updates (profile, incident, thread, key, sent_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![p.id, k.incident, t.key, key, now],
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
    /// Reports heard under the timelines.
    pub replies: u32,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct PreviewCase {
    pub id: i64,
    pub title: String,
    pub address: String,
    pub opened: i64,
    pub chats: Vec<String>,
    /// The line under each report heard.
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
        let all = if s.audio { notices(k) } else { Vec::new() };
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
            timeline: render(k, cap_for(s.audio)),
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
    use crate::cases::Arrival;
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
            contested: None,
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

    fn places() -> crate::places::Settings {
        crate::places::Settings {
            places: vec![crate::places::Place { id: "p-general".into(), name: "Example General".into(), enabled: true, dest: "d-general".into(), ..Default::default() }],
        }
    }

    const SUMMARY: &str = "Cardiac Arrest, ROSC, VF — Medic 7 inbound to Example General with a 60-year-old male, witnessed arrest with bystander CPR, VF on arrival, ROSC after two shocks. ETA 10 minutes.";

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
                Line {
                    detail: SUMMARY.into(),
                    ..report(t0 + 1200, 3, vec![Fact { key: "witnessed".into(), value: "yes".into() }, Fact { key: "rhythm".into(), value: "VF".into() }])
                },
                Line { inferred: true, ..line(t0 + 1500, "rearrest", "Lost pulses", "crew") },
                report(t0 + 1560, 4, vec![]),
                report(t0 + 1800, 5, vec![]),
            ],
            vec![arrival(3, t0 + 1190, Some("10 minutes")), arrival(4, t0 + 1550, Some("4 minutes")), arrival(5, t0 + 1790, Some("5 minutes"))],
        )
    }

    #[test]
    fn a_case_is_emailed_to_its_list_and_each_hospital_reported_to() {
        let mut pl = places();
        pl.places[0].email = "ed@general.example.org".into();
        let s = Send { email: true, email_to: "team@example.org, charge@example.org".into(), ..Send::default() };
        let k = arrest();
        let t = email_threads(&k, &s, &pl);
        assert_eq!(t.iter().map(|x| x.key.as_str()).collect::<Vec<_>>(), vec!["all", "place:p-general"]);
        assert_eq!(t[0].to, vec!["team@example.org", "charge@example.org"]);
        assert_eq!(t[1].to, vec!["ed@general.example.org"]);
        // Email off, or no hospitals: as on Telegram.
        assert!(email_threads(&k, &Send { email: false, ..s.clone() }, &pl).is_empty());
        assert_eq!(email_threads(&k, &Send { hospitals: false, ..s.clone() }, &pl).len(), 1);
        // A hospital told of a seizure is not emailed an arrest.
        let mut seizure = arrest();
        for l in seizure.lines.iter_mut().filter(|l| l.kind == "report") {
            l.detail = "Seizure, Altered Mental Status — Medic 7 inbound, witnessed seizure, remains altered.".into();
        }
        assert_eq!(email_threads(&seizure, &s, &pl).len(), 1);
    }

    #[test]
    fn an_emailed_case_opens_with_the_story_so_far_and_follows_with_news() {
        let k = arrest();
        let t0 = k.opened;
        let all = EmailThread { key: "all".into(), to: vec!["a@example.org".into()], since: t0, conversation: None };
        // Each event once, a readback being the same news; reports as heard.
        let keys: Vec<String> = updates(&k).into_iter().map(|u| u.key).collect();
        assert_eq!(keys, vec![format!("working:{}", t0 + 400), format!("rosc:{}", t0 + 900), "report:3".into(), format!("rearrest:{}", t0 + 1500), "report:5".into()]);
        // First email: everything so far is in it, so nothing follows.
        let first = plan_email(&k, &all, None, t0 + 1850);
        assert!(matches!(&first[0], EmailStep::Open { subject } if subject.starts_with("🫀")), "{first:?}");
        assert!(first[1..].iter().all(|s| matches!(s, EmailStep::Stale { .. })));
        // Already open: news within the window follows, older news is stale.
        let had = EmailSent { root: "<r@x>".into(), subject: "s".into(), keys: HashSet::new() };
        let later = plan_email(&k, &all, Some(&had), t0 + 1850);
        let follows: Vec<&str> = later.iter().filter_map(|s| match s { EmailStep::Follow { update } => Some(update.key.as_str()), _ => None }).collect();
        assert_eq!(follows.len(), 4, "{later:?}");
        assert!(matches!(&later[0], EmailStep::Stale { key } if key.starts_with("working:")));
        // Once recorded, nothing more.
        let done = EmailSent { keys: updates(&k).into_iter().map(|u| u.key).collect(), ..had };
        assert!(plan_email(&k, &all, Some(&done), t0 + 1850).is_empty());
        // A hospital hears what came after the report that brought it in.
        let hosp = EmailThread { key: "place:p".into(), to: vec!["ed@x.org".into()], since: t0 + 1190, conversation: Some(3) };
        let empty = EmailSent { keys: HashSet::new(), ..done };
        let h: Vec<String> = plan_email(&k, &hosp, Some(&empty), t0 + 1850).into_iter().filter_map(|s| match s { EmailStep::Follow { update } => Some(update.key), _ => None }).collect();
        assert_eq!(h, vec![format!("rearrest:{}", t0 + 1500), "report:5".to_string()]);
    }

    fn thread_for_all(k: &CaseView) -> Thread {
        Thread { dest: "d-all".into(), since: k.opened, place_id: None, conversation: None }
    }

    fn had(k: &CaseView, audio: bool, map_id: Option<i64>) -> Sent {
        Sent { target: "x".into(), root_id: 77, root_audio: audio, rendered: render(k, cap_for(audio)), map_sent: map_id.is_some(), map_id, notices: HashSet::new() }
    }

    #[test]
    fn the_timeline_reads_as_the_ed_needs_it() {
        let text = render(&arrest(), TEXT_CHARS);
        let rows: Vec<&str> = text.lines().collect();
        // Where it is first, since that is what tells one arrest from the
        // next in a chat of them; then how it stands, then who is on it.
        assert_eq!(rows[..3], ["🫀 Cardiac arrest · 1200 Example St", "🏥 HOSPITAL NOTIFIED", "🚒 Engine 5, Medic 7"], "{text}");
        assert!(text.contains("⚠️ Address as heard on the radio; not found on the map"), "an unplaced address says it is unchecked: {text}");
        assert!(!text.contains("Repaged\n") && !text.ends_with("Repaged"), "a plain repage is not a line: {text}");
        assert!(text.contains("Repaged as a working arrest"));
        assert!(text.contains("ROSC · dispatcher"), "{text}");
        assert!(text.contains("Lost pulses · crew, run not named on the air"), "{text}");
        // What the crew told the hospital, under the line that says they
        // called; a report with no summary has nothing under it.
        assert!(text.contains(&format!("Report to Example General\n↳ {SUMMARY}\n")), "{text}");
        assert_eq!(text.matches("↳ ").count(), 1, "{text}");
        let mut twice = arrest();
        twice.lines.push(line(1_003_000, "arrived", "At the hospital", "crew"));
        twice.lines.push(line(1_006_000, "arrived", "At the hospital", "crew"));
        assert_eq!(render(&twice, TEXT_CHARS).matches("At the hospital").count(), 1);
        assert!(text.contains("🏥 Expected at Example General: about"));
        assert!(text.contains("🩺 Witnessed: yes · Rhythm: VF"), "only what was said is a fact: {text}");
        assert!(text.contains("Not stated: bystander CPR, downtime, history"), "{text}");
        assert!(text.contains("📈 ED-ECPR 4-criterion screen: 0–46% (2 of 4 not stated)"), "the score, as a range while inputs are missing: {text}");
        assert!(text.ends_with("\n\nIncident #11"), "the number it goes by closes it: {text}");
        assert!(!text.contains("Updated"), "the last line's own time says when: {text}");
        // Placed, the warning goes; before any report there are no facts.
        let placed = CaseView { lat: Some(39.5), lon: Some(-86.25), ..arrest() };
        assert!(!render(&placed, TEXT_CHARS).contains("not found on the map"));
        let early = case(arrest().lines[..3].to_vec(), vec![]);
        assert!(!render(&early, TEXT_CHARS).contains("Witnessed") && !render(&early, TEXT_CHARS).contains("Not stated"));
        let unheard = CaseView { address: String::new(), ..early };
        assert!(render(&unheard, TEXT_CHARS).starts_with("🫀 Cardiac arrest · address not heard\n🔴 WORKING ARREST\n"), "{}", render(&unheard, TEXT_CHARS));
    }

    /// The board and the chat both led with CARDIAC ARREST while the report
    /// attached to the case described a seizure. Whatever else the timeline
    /// says, that has to be said before it.
    #[test]
    fn a_report_that_names_no_arrest_is_said_at_the_top() {
        let mut k = arrest();
        k.contested = Some("Reported to Community North as: Seizure, Hypotension".into());
        let text = render(&k, TEXT_CHARS);
        let rows: Vec<&str> = text.lines().collect();
        assert_eq!(
            rows[3], "⚠️ Reported to Community North as: Seizure, Hypotension",
            "above the timeline, under the units: {text}"
        );
        let h = html(&k, "", TEXT_CHARS);
        assert!(h.contains("⚠️ <b>Reported to Community North as: Seizure, Hypotension</b>"), "{h}");
        // A case whose report is about an arrest says nothing extra.
        k.contested = None;
        assert!(!render(&k, TEXT_CHARS).contains("⚠️ Reported"));
    }

    #[test]
    fn the_message_goes_out_formatted_with_the_address_opening_google_maps() {
        let mut k = arrest();
        let h = html(&k, "Testville, EX", TEXT_CHARS);
        assert!(
            h.starts_with("🫀 <b>Cardiac arrest · <a href=\"https://www.google.com/maps/search/?api=1&amp;query=1200+Example+St%2C+Testville%2C+EX\">1200 Example St</a></b>\n🏥 <b>HOSPITAL NOTIFIED</b>\n"),
            "{h}"
        );
        assert!(h.contains("\n<blockquote><b>"), "the timeline is set off: {h}");
        assert!(h.contains(" <i>· dispatcher</i>"), "{h}");
        assert!(h.contains("\n↳ <i>Cardiac Arrest, ROSC, VF — Medic 7"), "the summary is set in italics: {h}");
        assert!(h.ends_with("\n\n<code>Incident #11</code>"), "{h}");
        k.lat = Some(39.5);
        k.lon = Some(-86.25);
        assert!(html(&k, "Testville, EX", TEXT_CHARS).contains("query=39.500000%2C-86.250000\">1200 Example St</a>"));
        // What was heard is data: it cannot become markup.
        k.address = "12 <b>Oak</b> & Elm".into();
        k.units = vec!["Medic <7>".into()];
        k.lines[6].detail = "STEMI <alert> — Medic 7 & Engine 5".into();
        let h = html(&k, "", TEXT_CHARS);
        assert!(h.contains("12 &lt;b&gt;Oak&lt;/b&gt; &amp; Elm</a>") && h.contains("Medic &lt;7&gt;"), "{h}");
        assert!(h.contains("↳ <i>STEMI &lt;alert&gt; — Medic 7 &amp; Engine 5</i>"), "{h}");
        assert_eq!(h.matches("<b>").count(), h.matches("</b>").count());
        k.address.clear();
        assert!(html(&k, "Testville, EX", TEXT_CHARS).starts_with("🫀 <b>Cardiac arrest · address not heard</b>"));
    }

    #[test]
    fn the_chat_for_every_case_is_owed_one_map_while_the_case_goes_on() {
        let k = arrest();
        let s = Send { dest: "d-all".into(), ..Send::default() };
        let all = &threads(&k, &s, &places())[0];
        let hosp = &threads(&k, &s, &places())[1];
        let now = k.lines.last().unwrap().at + 30;
        assert!(map_due(&k, all, &s, None, now));
        let mut had = had(&k, true, None);
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
        let k = case(lines, vec![]);
        for cap in [TEXT_CHARS, CAPTION_CHARS] {
            let text = render(&k, cap);
            assert!(text.chars().count() <= cap, "{cap}: {}", text.chars().count());
            assert!(text.contains("Dispatched as Unconscious"));
            assert!(text.contains("said again 399"));
            assert!(text.contains("earlier lines"));
        }
        assert!(render(&k, TEXT_CHARS).chars().count() > CAPTION_CHARS, "a message holds more than a caption");
    }

    /// Under the page heard, the timeline is a caption, and Telegram gives a
    /// caption a quarter of what it gives a message. What goes first is what
    /// the ED misses least.
    #[test]
    fn a_caption_gives_up_the_least_first() {
        let long = |n: u32| format!("Report {n} — {}", "the crew said a great deal about the patient and the drive in. ".repeat(4));
        let mut k = arrest();
        for (i, conv) in [(6usize, 3u32), (8, 4), (9, 5)] {
            k.lines[i].detail = long(conv);
        }
        let full = render(&k, TEXT_CHARS);
        assert_eq!(full.matches("↳ Report").count(), 3, "with room, every report's summary: {full}");
        let text = render(&k, CAPTION_CHARS);
        assert!(text.chars().count() <= CAPTION_CHARS, "{}", text.chars().count());
        // The latest summary stays whole; the earlier two go first, then the
        // list of what was not said. The timeline's own lines all stay.
        assert!(text.contains(&format!("↳ {}\n", long(5).trim())), "{text}");
        assert!(!text.contains("↳ Report 3") && !text.contains("↳ Report 4"), "{text}");
        assert!(text.contains("Dispatched as Unconscious") && text.contains("Lost pulses") && text.contains("Working arrest"), "{text}");
        assert!(!text.contains("earlier lines"), "{text}");
        assert!(text.contains("🩺 Witnessed"), "what was said stays: {text}");
        assert!(text.contains("📈 "), "the score stays: {text}");
        // Twice as long again, the summary is cut to its opening rather
        // than the ROSC being dropped; only past that do older lines go.
        k.lines[9].detail = long(5).repeat(3);
        let text = render(&k, CAPTION_CHARS);
        assert!(text.chars().count() <= CAPTION_CHARS, "{}", text.chars().count());
        assert!(text.contains("↳ Report 5 — the crew said") && text.contains("…\n"), "{text}");
        assert!(text.contains("ROSC · crew") && !text.contains("earlier lines"), "{text}");
        assert!(!text.contains("Not stated"), "{text}");
        // A summary alone bigger than the whole caption is cut to fit
        // before any line goes.
        k.lines[9].detail = "x".repeat(3000);
        let text = render(&k, CAPTION_CHARS);
        assert!(text.chars().count() <= CAPTION_CHARS, "{}", text.chars().count());
        assert!(text.contains("Lost pulses") && !text.contains("earlier lines"), "{text}");
        assert!(text.contains("↳ xxxx") && text.contains("x…\n"), "{text}");
        // With a long night behind it as well, the summary keeps its opening
        // and the oldest lines go, the first line and the latest staying.
        let mut long_night = vec![line(1_000_000, "dispatched", "Dispatched as Unconscious", "page")];
        for i in 1..40 {
            long_night.push(line(1_000_000 + i * 10, "working", &format!("Working arrest, said again {i}"), "crew"));
        }
        long_night.push(Line { detail: "x".repeat(3000), ..report(1_001_000, 3, vec![]) });
        let k = case(long_night, vec![arrival(3, 1_000_990, Some("10 minutes"))]);
        let text = render(&k, CAPTION_CHARS);
        assert!(text.chars().count() <= CAPTION_CHARS, "{}", text.chars().count());
        assert!(text.contains("Dispatched as Unconscious") && text.contains("earlier lines") && text.contains("said again 39"), "{text}");
        let summary = text.lines().find(|l| l.starts_with("↳ ")).unwrap();
        assert!(summary.ends_with('…') && summary.chars().count() >= SUMMARY_MIN, "{} of {}\n{text}", summary.chars().count(), text.chars().count());
        // The markup follows the same cut.
        let h = html(&k, "", CAPTION_CHARS);
        assert!(h.contains("<i>… ") && h.contains("↳ <i>xxxx") && !h.contains("Not stated"), "{h}");
    }

    #[test]
    fn only_a_report_to_a_hospital_is_heard_and_each_once() {
        let n = notices(&arrest());
        let keys: Vec<&str> = n.iter().map(|x| x.key.as_str()).collect();
        // The second report's "4 minutes", said six minutes later, lands
        // where the first said, which is not news; the third moves it five
        // minutes. Working, ROSC and lost pulses are lines in the timeline,
        // not clips.
        assert_eq!(keys, vec!["report:3", "report:5"]);
        assert!(n[0].text.starts_with("🏥 Report to Example General · ") && !n[0].text.contains('\n'), "one line under the clip: {}", n[0].text);
        assert!(n[0].html.starts_with("🏥 <b>Report to Example General</b> · "), "{}", n[0].html);
        assert_eq!((n[0].name.as_str(), n[0].conversation, n[0].call), ("Report to Example General", Some(3), None));
        // The same case rebuilt gives the same keys.
        assert_eq!(notices(&arrest()).iter().map(|x| x.key.clone()).collect::<Vec<_>>(), keys);
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
        let hosp = notices_for(&t[1], &notices(&k));
        assert_eq!(hosp.iter().map(|x| x.key.as_str()).collect::<Vec<_>>(), vec!["report:5"]);
        // Hospitals' chats can be left out.
        assert_eq!(threads(&k, &Send { hospitals: false, ..s.clone() }, &places()).len(), 1);
    }

    #[test]
    fn a_hospital_is_not_paged_for_a_report_that_names_no_arrest() {
        let s = Send { dest: "d-all".into(), ..Send::default() };
        let mut k = arrest();
        // Dispatched as a cardiac arrest; the crew calls the hospital with a seizure.
        for l in k.lines.iter_mut().filter(|l| l.kind == "report") {
            l.detail = "Seizure, Altered Mental Status — Medic 7 inbound with a 48-year-old male, witnessed seizure, remains altered.".into();
        }
        let t = threads(&k, &s, &places());
        assert_eq!(t.iter().map(|x| x.dest.as_str()).collect::<Vec<_>>(), vec!["d-all"], "only the chat for every case");
        // Another profile is not read for arrest words.
        k.profile = "stroke".into();
        assert_eq!(threads(&k, &s, &places()).len(), 2);
    }

    #[test]
    fn a_thread_starts_with_the_story_so_far_and_then_only_edits() {
        let k = arrest();
        let t = thread_for_all(&k);
        let now = k.lines.last().unwrap().at + 30;
        let first = plan(&k, &t, &Send::default(), None, now);
        assert!(matches!(&first[0], Step::Root { heard: Some(n), .. } if n.key == "audio:page"), "{first:?}");
        assert!(first[1..].iter().all(|s| matches!(s, Step::Stale { .. })), "a new thread sends nothing already in it: {first:?}");
        assert_eq!(first.len(), 3, "the two reports are recorded as dealt with: {first:?}");
        // Later: nothing new, nothing sent.
        let mut had = had(&k, true, Some(78));
        for s in &first[1..] {
            if let Step::Stale { notice } = s {
                had.notices.insert(notice.key.clone());
            }
        }
        assert!(plan(&k, &t, &Send::default(), Some(&had), now).is_empty(), "nothing changed, nothing sent");
        // Efforts ceased: the timeline says so, and the map comes down.
        // Nothing buzzes.
        let mut later = k.clone();
        later.lines.push(line(now + 60, "terminated", "Efforts ceased", "readback"));
        later.state = "terminated".into();
        later.open = false;
        let steps = plan(&later, &t, &Send::default(), Some(&had), now + 90);
        assert!(matches!(&steps[0], Step::Edit { root_id: 77, text } if text.contains("⚫ EFFORTS CEASED") && text.contains("Efforts ceased · dispatcher") && !text.contains("Expected at")), "nobody is expected anywhere once it is over: {steps:?}");
        assert_eq!(steps[1], Step::DropMap { map_id: 78 });
        assert_eq!(steps.len(), 2);
        // Once down, it stays down; a thread that never had a map has
        // nothing to take down.
        let down = Sent { map_id: None, rendered: render(&later, CAPTION_CHARS), ..had.clone() };
        assert!(plan(&later, &t, &Send::default(), Some(&down), now + 120).is_empty());
        // Not an arrest concludes it the same way.
        let mut no = k.clone();
        no.lines.push(line(now + 60, "downgrade", "Not a cardiac arrest", "readback"));
        no.state = "downgraded".into();
        assert!(plan(&no, &t, &Send::default(), Some(&had), now + 90).iter().any(|s| matches!(s, Step::DropMap { map_id: 78 })));
        // Twenty minutes late, the edit still goes.
        let late = plan(&later, &t, &Send::default(), Some(&had), now + 60 + NOTIFY_WITHIN_SECS + 1);
        assert!(matches!(&late[0], Step::Edit { .. }));
        // A case that ended long ago gets no new thread.
        assert!(plan(&later, &t, &Send::default(), None, now + 60 + NOTIFY_WITHIN_SECS + 1).is_empty());
    }

    #[test]
    fn a_report_to_the_hospital_follows_as_its_clip() {
        let k = arrest();
        let t = thread_for_all(&k);
        // The thread is up to date as of the ROSC; the crew then calls.
        let before = case(k.lines[..6].to_vec(), vec![]);
        let had = had(&before, true, Some(78));
        let now = k.lines[6].at + 30;
        let upto = case(k.lines[..7].to_vec(), k.arrivals[..1].to_vec());
        let steps = plan(&upto, &t, &Send::default(), Some(&had), now);
        assert!(matches!(&steps[0], Step::Edit { text, .. } if text.contains(&format!("↳ {SUMMARY}"))), "the summary lands in the timeline: {steps:?}");
        assert!(matches!(&steps[1], Step::Reply { root_id: Some(77), notice } if notice.key == "report:3"), "{steps:?}");
        assert_eq!(steps.len(), 2);
        // With the radio off, the timeline is the whole of it.
        let quiet = Send { audio: false, ..Send::default() };
        let had_text = Sent { root_audio: false, rendered: render(&before, TEXT_CHARS), ..had.clone() };
        let steps = plan(&upto, &t, &quiet, Some(&had_text), now);
        assert!(matches!(&steps[..], [Step::Edit { .. }]), "{steps:?}");
    }

    #[test]
    fn a_thread_opens_with_the_page_heard_and_a_hospital_with_its_report() {
        let k = arrest();
        let s = Send { dest: "d-all".into(), ..Send::default() };
        let ts = threads(&k, &s, &places());
        let page = intro(&k, &ts[0]).unwrap();
        assert_eq!((page.key.as_str(), page.call), ("audio:page", Some(1_000_000)));
        assert_eq!(page.name, "Dispatch page");
        assert!(clip_stem(&k, &page).starts_with("incident-11_") && clip_stem(&k, &page).ends_with("_dispatch-page"), "{}", clip_stem(&k, &page));
        assert_eq!(clip_title(&k, &page), "Dispatch page · 1200 Example St");
        let report = intro(&k, &ts[1]).unwrap();
        assert_eq!((report.key.as_str(), report.conversation, report.name.as_str()), ("audio:report:3", Some(3), "Report to Example General"));

        // A case just paged: one message, the page heard with the timeline
        // under it, cut to what a caption holds.
        let fresh = case(k.lines[..1].to_vec(), vec![]);
        let now = fresh.opened + 30;
        let steps = plan(&fresh, &ts[0], &s, None, now);
        assert!(matches!(&steps[0], Step::Root { heard: Some(n), text } if n.key == "audio:page" && *text == render(&fresh, CAPTION_CHARS)), "{steps:?}");
        assert_eq!(steps.len(), 1);
        // Sent, nothing more is owed.
        assert!(plan(&fresh, &ts[0], &s, Some(&had(&fresh, true, None)), now + 5).is_empty());
        // Switched off, the timeline is a message of its own, with a
        // message's room.
        let quiet = Send { audio: false, ..s.clone() };
        assert!(matches!(&plan(&fresh, &ts[0], &quiet, None, now)[..], [Step::Root { heard: None, text }] if *text == render(&fresh, TEXT_CHARS)));
        // A case with no dispatch page has nothing to play first.
        let no_page = case(vec![line(1_000_000, "working", "Working arrest", "readback")], vec![]);
        assert_eq!(intro(&no_page, &ts[0]), None);
        assert!(matches!(&plan(&no_page, &ts[0], &s, None, 1_000_030)[..], [Step::Root { heard: None, .. }]));
        // A thread from before, sent as text, keeps a message's room.
        let heard_all: HashSet<String> = notices(&k).into_iter().map(|n| n.key).collect();
        let old = Sent { root_audio: false, rendered: render(&k, TEXT_CHARS), notices: heard_all, ..had(&k, false, None) };
        assert!(plan(&k, &ts[0], &s, Some(&old), k.updated + 30).is_empty());
    }

    #[test]
    fn a_report_is_heard_whole_and_an_event_as_its_call() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE calls (id INTEGER PRIMARY KEY, start INTEGER, secs REAL, tg INTEGER, unit INTEGER, audio TEXT);
             CREATE TABLE conversations (id INTEGER PRIMARY KEY, pieces TEXT NOT NULL);
             INSERT INTO calls VALUES (7, 100, 5.0, 1, 900900, '/lib/page.m4a'), (8, 200, 5.0, 1, 900900, NULL),
                                      (9, 300, 10.0, 1, 900900, '/lib/head.m4a'), (10, 311, 4.0, 1, 0, '/lib/tail.m4a');",
        )
        .unwrap();
        let pieces = r#"[{"id":2,"unit":1,"unit_name":null,"fixed":true,"at":20,"secs":2.0,"audio":"/lib/b.m4a","transcript":"go ahead"},
                         {"id":1,"unit":5,"unit_name":null,"fixed":false,"at":10,"secs":9.0,"audio":"/lib/a.m4a","transcript":"report"},
                         {"id":3,"unit":5,"unit_name":null,"fixed":false,"at":30,"secs":1.0,"audio":null,"transcript":"thanks"}]"#;
        c.execute("INSERT INTO conversations VALUES (3, ?1)", [pieces]).unwrap();
        let n = |conversation: Option<i64>, call: Option<i64>| Notice { key: "k".into(), at: 0, text: "t".into(), html: "t".into(), name: "t".into(), conversation, call };
        assert_eq!(recordings(&c, &n(Some(3), Some(1))), vec!["/lib/a.m4a".to_string(), "/lib/b.m4a".to_string()]);
        assert_eq!(recordings(&c, &n(None, Some(7))), vec!["/lib/page.m4a".to_string()]);
        assert_eq!(recordings(&c, &n(None, Some(9))), vec!["/lib/head.m4a".to_string(), "/lib/tail.m4a".to_string()], "a page heard as two calls is heard whole");
        assert_eq!(recordings(&c, &n(None, Some(8))), Vec::<String>::new(), "a call with no recording");
        assert_eq!(recordings(&c, &n(None, None)), Vec::<String>::new());
    }

    /// What went out is read back with the columns added since: a thread
    /// from before them is text with no map to take down.
    #[test]
    fn what_went_out_is_kept_across_the_schema_growing() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE case_sends (profile TEXT NOT NULL, incident INTEGER NOT NULL, dest TEXT NOT NULL, target TEXT NOT NULL,
                root_id INTEGER NOT NULL, rendered TEXT NOT NULL, since INTEGER NOT NULL, map_sent INTEGER NOT NULL DEFAULT 0,
                sent_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, error TEXT NOT NULL DEFAULT '', PRIMARY KEY (profile, incident, dest));
             INSERT INTO case_sends (profile, incident, dest, target, root_id, rendered, since, map_sent, sent_at, updated_at) VALUES ('p', 11, 'd', 'chat', 5, 'old', 0, 1, 0, 0);",
        )
        .unwrap();
        ensure_schema(&c);
        ensure_schema(&c);
        let s = sent(&c, "p", 11, "d").unwrap();
        assert_eq!((s.root_id, s.root_audio, s.map_sent, s.map_id), (5, false, true, None));
        c.execute("UPDATE case_sends SET root_audio = 1, map_id = 9 WHERE incident = 11", []).unwrap();
        let s = sent(&c, "p", 11, "d").unwrap();
        assert_eq!((s.root_audio, s.map_id), (true, Some(9)));
    }

    /// Against a copy of a library whose cases are built (run the cases
    /// real_library test on it first): `HS_SEND_DB=copy.db
    /// HS_CASES_PLACES=places.json cargo test casesend::tests::real_library
    /// -- --ignored --nocapture`. Every place with the feature named in
    /// `HS_SEND_FEATURE` is given a chat, to see what those would hear;
    /// `HS_SEND_CASE` shows that one case.
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
            println!("{} {:<24} threads {:>2} clips {:>2}", d.date, d.dest, d.threads, d.replies);
        }
        let only: Option<i64> = std::env::var("HS_SEND_CASE").ok().and_then(|v| v.parse().ok());
        for k in p.cases.iter().filter(|k| only.map_or(k.chats.len() > 1 || !k.replies.is_empty(), |id| k.id == id)).take(4) {
            println!("\n=== case {} → {:?} ({} chars)\n{}\n--- clips\n{}", k.id, k.chats, k.timeline.chars().count(), k.timeline, k.replies.join("\n"));
        }
    }

    #[test]
    fn the_preview_counts_what_each_chat_would_hear() {
        let s = Send { dest: "d-all".into(), ..Send::default() };
        let view = crate::cases::CasesView { cases: vec![arrest()], unplaced: vec![] };
        let names: HashMap<String, String> = [("d-all".to_string(), "All arrests".to_string()), ("d-general".to_string(), "Example General".to_string())].into();
        let p = preview(&view, &s, &places(), &names);
        let all = p.days.iter().find(|d| d.dest == "All arrests").unwrap();
        assert_eq!((all.threads, all.replies), (1, 2), "two reports heard");
        let general = p.days.iter().find(|d| d.dest == "Example General").unwrap();
        assert_eq!((general.threads, general.replies), (1, 1), "the report that brought it in is its first message");
        assert!(p.cases[0].timeline.chars().count() <= CAPTION_CHARS, "as it would read under the page heard");
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
        let quiet = preview(&view, &Send { audio: false, ..s }, &places(), &names);
        assert!(quiet.days.iter().all(|d| d.replies == 0) && quiet.cases[0].replies.is_empty(), "with the radio off, only timelines");
        let none = preview(&view, &Send::default(), &crate::places::Settings::default(), &names);
        assert_eq!(none.warnings.len(), 2);
    }
}
