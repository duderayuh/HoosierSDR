//! Who a radio is, learned from what is said on the air.
//!
//! A trunked system names a transmission by the radio that keyed it, and
//! that is a bare number. The alias table in `units.rs` names a number when
//! someone has typed it in or the system broadcast it, which on most
//! systems is almost never. But the crews say who they are all the time:
//! "control, Medic 32", "Engine 44 to control", "Example General, this is
//! Medic 17". And the other end says it for them: a hospital answers
//! "Medic 44, this is Example General", a dispatcher calls "Medic 35 from
//! control" and the next radio to key up is Medic 35.
//!
//! Each of those is *evidence*, kept as a row with the call it came from, so
//! the listener can play the reason back. An *identity* is what the
//! evidence adds up to, worked out on read, never stored, so a better rule
//! or a rejected guess changes every radio at once:
//!
//!   - a radio is **learned** as a callsign when that callsign carries at
//!     least 3 of weight and at least 60 % of everything said about it —
//!     the threshold the conversation engine already uses for a hospital's
//!     own consoles. One mis-heard digit ("Medic 24" for "Medic 44") is
//!     outvoted, not believed.
//!   - below that it is **tentative**: shown, never used to join anything.
//!   - radios move between trucks and people, so when the last three things
//!     said about a radio all name another callsign it is **changed**, and
//!     it stops being used until the listener decides.
//!   - the listener's own word — **confirmed**, or a callsign rejected for
//!     that radio — beats all of it.
//!
//! Some radios are not units: the dispatch centre's consoles, the automated
//! voice that reads out pages, a hospital's own radios. Those are roles, and
//! they matter here because a console that says "Medic 35" is calling Medic
//! 35, not being it.
//!
//! Rules only. Nothing here asks a model.

use rusqlite::{params, Connection};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use tauri::{AppHandle, Manager, State};

use crate::AppState;

/// What a callsign needs to be believed: this much weight, and this share of
/// everything said about the radio.
pub const ACCEPT_WEIGHT: f64 = 3.0;
pub const ACCEPT_SHARE: f64 = 0.6;
/// A reply to a console this soon after it is the radio it called.
pub const REPLY_SECS: i64 = 20;
/// How many of the most recent sightings must agree on another callsign
/// before a radio is taken to have changed hands.
const CHANGE_RUN: usize = 3;

pub const UNIT: &str = "unit";
pub const CONSOLE: &str = "console";
pub const AUTOMATED: &str = "automated";
pub const HOSPITAL: &str = "hospital";

/// What one transmission says about the radio that sent it.
#[derive(Clone, Debug, PartialEq)]
pub enum Said {
    /// The speaker named themselves.
    Callsign {
        sign: String,
        weight: f64,
        how: &'static str,
    },
    /// The speaker is a dispatch console ("Medic 35 from control").
    Console,
    /// The automated page voice ("… 1823 Hours, Location …").
    Automated,
}

// ---------------------------------------------------------------------------
// reading a transcript
// ---------------------------------------------------------------------------

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
        .collect()
}

/// A title word and its number at `i`, and the index after it. "Medic 32"
/// and "medic32" both count; a number of more than four digits is a radio
/// ID or an address, not a unit.
fn callsign_at(w: &[String], i: usize, vocab: &HashSet<String>) -> Option<(String, usize)> {
    let word = w.get(i)?;
    let unit_number = |s: &str| {
        (s.len() <= 4 && s.chars().all(|c| c.is_ascii_digit()))
            .then(|| s.parse::<u32>().ok())
            .flatten()
            .filter(|n| *n > 0)
    };
    if vocab.contains(word) {
        let n = unit_number(w.get(i + 1)?)?;
        return Some((display(word, n), i + 2));
    }
    // "medic32": a title glued to its number.
    let split = word.find(|c: char| c.is_ascii_digit())?;
    let (title, num) = word.split_at(split);
    if !title.is_empty() && vocab.contains(title) {
        let n = unit_number(num)?;
        return Some((display(title, n), i + 1));
    }
    None
}

/// "ems" + 93 → "EMS 93", "medic" + 32 → "Medic 32". Short titles are
/// initials far more often than words.
pub fn display(title: &str, n: u32) -> String {
    let t = if title.len() <= 3 && title != "car" {
        title.to_uppercase()
    } else {
        let mut c = title.chars();
        match c.next() {
            Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            None => String::new(),
        }
    };
    format!("{t} {n}")
}

const FILLERS: &[&str] = &["hey", "uh", "um", "yeah", "yes", "ok", "okay", "and", "so", "hi", "hello"];

fn skip_fillers(w: &[String]) -> usize {
    w.iter().take_while(|x| FILLERS.contains(&x.as_str())).count()
}

/// Words after a callsign at the start that make it the speaker's own status
/// ("Medic 20, transporting"). Weaker than "control, Medic 20": a console
/// relays these too, and consoles are excluded before this is believed.
const STATUS_WORDS: &[&str] = &["transporting", "enroute", "arriving", "responding", "is", "en", "on", "with", "we"];

/// What a transmission on a dispatch or ops channel says about its speaker.
///
/// `vocab` is the set of title words this county puts in front of a unit
/// number, learned from its own dispatches (`link::vocabulary`).
pub fn read_speaker(text: &str, vocab: &HashSet<String>) -> Vec<Said> {
    let w = words(text);
    let mut out = Vec::new();
    if is_page(text) {
        out.push(Said::Automated);
        return out;
    }
    let s = skip_fillers(&w);
    let at = |i: usize| w.get(i).map(String::as_str);

    // "control, Medic 32" · "fire control, this is Engine 36" ·
    // "control from Engine 44" · "control engine 24, can you…"
    let mut i = s;
    if matches!(at(i), Some("fire" | "ems")) && at(i + 1) == Some("control") {
        i += 1;
    }
    if at(i) == Some("control") {
        let mut j = i + 1;
        if at(j) == Some("this") && at(j + 1) == Some("is") {
            j += 2;
        } else if matches!(at(j), Some("from" | "its" | "it")) {
            j += if at(j) == Some("it") && at(j + 1) == Some("s") { 2 } else { 1 };
        }
        if let Some((sign, _)) = callsign_at(&w, j, vocab) {
            out.push(Said::Callsign { sign, weight: 1.0, how: "said_self" });
            return out;
        }
    }

    // One or more callsigns at the start, then what follows them.
    let mut signs = Vec::new();
    let mut k = s;
    while let Some((sign, next)) = callsign_at(&w, k, vocab) {
        signs.push(sign);
        k = next;
    }
    if !signs.is_empty() {
        // "Medic 35 from control": a console calling a unit.
        if at(k) == Some("from") && at(k + 1) == Some("control") {
            out.push(Said::Console);
            return out;
        }
        if signs.len() == 1 {
            // "Medic 32 to control"
            if matches!(at(k), Some("to" | "for")) && at(k + 1) == Some("control") {
                out.push(Said::Callsign { sign: signs[0].clone(), weight: 1.0, how: "said_self" });
                return out;
            }
            // "Medic 37 calling Example General"
            if at(k) == Some("calling") {
                out.push(Said::Callsign { sign: signs[0].clone(), weight: 1.0, how: "said_self" });
                return out;
            }
            // "Medic 20, transporting"
            if at(k).is_some_and(|x| STATUS_WORDS.contains(&x)) {
                out.push(Said::Callsign { sign: signs[0].clone(), weight: 0.5, how: "said_status" });
                return out;
            }
        }
    }

    // "…, this is Medic 17" near the start.
    for i in s..w.len().min(s + 10) {
        if at(i) == Some("this") && at(i + 1) == Some("is") {
            if let Some((sign, _)) = callsign_at(&w, i + 2, vocab) {
                out.push(Said::Callsign { sign, weight: 1.0, how: "said_self" });
                return out;
            }
        }
    }
    out
}

/// The one callsign a transmission opens by calling, if it opens with
/// exactly one and names nobody as itself: "Medic 35, you're safe on
/// arrival". Only believed of a console or a hospital's radio.
pub fn addressed(text: &str, vocab: &HashSet<String>) -> Option<String> {
    let w = words(text);
    let s = skip_fillers(&w);
    let (sign, next) = callsign_at(&w, s, vocab)?;
    if callsign_at(&w, next, vocab).is_some() {
        return None; // "Medic 35, Medic 26, advise…": more than one
    }
    let tail: Vec<&str> = w.iter().skip(next).take(2).map(String::as_str).collect();
    if matches!(tail.as_slice(), ["to" | "for" | "from", "control", ..] | ["calling", ..]) {
        return None;
    }
    Some(sign)
}

/// The automated dispatch voice reads a clock time and a grid location on
/// every page: "… Cardiac Arrest, 1823 Hours, Location 1000 North 4000 East."
pub fn is_page(text: &str) -> bool {
    let w = words(text);
    let clock = w.windows(2).any(|p| {
        p[1] == "hours" && (3..=4).contains(&p[0].len()) && p[0].chars().all(|c| c.is_ascii_digit())
    });
    clock && w.iter().any(|x| x == "location")
}

// ---------------------------------------------------------------------------
// a stretch of one talkgroup, in order
// ---------------------------------------------------------------------------

/// One transmission, as the rules need it.
#[derive(Clone, Debug)]
pub struct Heard {
    pub call: i64,
    pub radio: u32,
    pub tg: u16,
    pub at: i64,
    pub secs: f64,
    pub text: String,
}

/// A piece of evidence, before it is stored.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub radio: u32,
    pub callsign: String,
    pub role: &'static str,
    pub how: &'static str,
    pub call: i64,
    pub at: i64,
    pub weight: f64,
}

/// Read a run of transmissions on dispatch and ops channels, oldest first.
///
/// `consoles` are radios already known to be consoles. A console's words
/// never name it as a unit, and a console calling one unit makes the next
/// other radio on that talkgroup inside `REPLY_SECS` that unit — at half
/// weight, because sometimes someone else answers.
pub fn read_calls(heard: &[Heard], vocab: &HashSet<String>, consoles: &HashSet<u32>) -> Vec<Found> {
    let mut out = Vec::new();
    // talkgroup → (console call that addressed someone, sign, when it ended)
    let mut calling: HashMap<u16, (u32, String, i64)> = HashMap::new();
    for h in heard {
        if h.radio == 0 {
            continue;
        }
        let said = read_speaker(&h.text, vocab);
        let is_console = consoles.contains(&h.radio);
        let mut named_self = false;
        for s in &said {
            match s {
                Said::Automated => out.push(found(h, "", AUTOMATED, "automated_page", 1.0)),
                Said::Console => out.push(found(h, "", CONSOLE, "from_control", 1.0)),
                Said::Callsign { sign, weight, how } if !is_console => {
                    named_self = true;
                    out.push(found(h, sign, UNIT, how, *weight));
                }
                Said::Callsign { .. } => {}
            }
        }
        let end = h.at + h.secs.ceil() as i64;
        if is_console || said.contains(&Said::Console) {
            match addressed(&h.text, vocab) {
                Some(sign) => {
                    calling.insert(h.tg, (h.radio, sign, end));
                }
                None => {
                    calling.remove(&h.tg);
                }
            }
            continue;
        }
        if let Some((console, sign, ended)) = calling.remove(&h.tg) {
            if h.radio != console && h.at - ended <= REPLY_SECS && !named_self {
                out.push(found(h, &sign, UNIT, "reply_to_control", 0.5));
            }
        }
    }
    out
}

fn found(h: &Heard, sign: &str, role: &'static str, how: &'static str, weight: f64) -> Found {
    Found {
        radio: h.radio,
        callsign: sign.to_string(),
        role,
        how,
        call: h.call,
        at: h.at,
        weight,
    }
}

/// Read one stored hospital conversation. The hospital's own radios are
/// hospital radios; when exactly one crew radio took part, the hospital
/// opening with a callsign names it ("Medic 44, this is Example General");
/// and a crew radio naming itself ("this is Medic 17", "Medic 37 calling
/// Example General") names itself.
pub fn read_conversation(pieces: &[crate::conversations::Piece], vocab: &HashSet<String>) -> Vec<Found> {
    let mut out = Vec::new();
    let mobiles: HashSet<u32> = pieces.iter().filter(|p| !p.fixed && p.unit != 0).map(|p| p.unit).collect();
    let mut hospital_seen: HashSet<u32> = HashSet::new();
    for p in pieces {
        if p.unit == 0 {
            continue;
        }
        let text = p.transcript.clone().unwrap_or_default();
        let h = Heard {
            call: p.id.unwrap_or(0),
            radio: p.unit,
            tg: 0,
            at: p.at,
            secs: p.secs,
            text: text.clone(),
        };
        if p.fixed {
            if hospital_seen.insert(p.unit) {
                out.push(found(&h, "", HOSPITAL, "hospital_radio", 1.0));
            }
            if mobiles.len() == 1 {
                if let Some(sign) = addressed(&text, vocab) {
                    let crew = *mobiles.iter().next().unwrap();
                    out.push(Found {
                        radio: crew,
                        ..found(&h, &sign, UNIT, "hospital_addressed", 1.0)
                    });
                }
            }
            continue;
        }
        for s in read_speaker(&text, vocab) {
            if let Said::Callsign { sign, weight, .. } = s {
                out.push(found(&h, &sign, UNIT, "said_to_hospital", weight));
            }
        }
    }
    // The same thing said twice in one conversation is one piece of evidence.
    let mut seen = HashSet::new();
    out.retain(|f| seen.insert((f.radio, f.callsign.clone(), f.role, f.how)));
    out
}

// ---------------------------------------------------------------------------
// what the evidence adds up to
// ---------------------------------------------------------------------------

/// One row of evidence as the fold sees it.
#[derive(Clone, Debug)]
pub struct Sighting {
    pub callsign: String,
    pub role: String,
    pub weight: f64,
    pub at: i64,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Candidate {
    pub callsign: String,
    pub weight: f64,
    pub last_at: i64,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Identity {
    pub system: String,
    pub radio: u32,
    /// unit | console | automated | hospital | "" (not enough to say)
    pub role: String,
    /// The callsign, for a unit.
    pub callsign: String,
    /// confirmed | learned | tentative | changed | ""
    pub state: String,
    /// Share of the unit evidence behind `callsign`, 0–1.
    pub share: f64,
    pub weight: f64,
    /// The callsign the most recent sightings agree on, when it is not
    /// `callsign` (state `changed`).
    pub latest: String,
    pub candidates: Vec<Candidate>,
    pub evidence: usize,
    pub first_at: i64,
    pub last_at: i64,
}

impl Identity {
    /// Safe to join things on: the listener said so, or the evidence does.
    pub fn usable(&self) -> bool {
        matches!(self.state.as_str(), "confirmed" | "learned")
    }

    /// What to call the radio where a person reads it.
    pub fn label(&self) -> Option<String> {
        if !self.usable() {
            return None;
        }
        match self.role.as_str() {
            UNIT => (!self.callsign.is_empty()).then(|| self.callsign.clone()),
            CONSOLE => Some("Dispatch console".into()),
            AUTOMATED => Some("Dispatch page voice".into()),
            HOSPITAL => Some("Hospital radio".into()),
            _ => None,
        }
    }
}

/// The listener's word on one radio.
#[derive(Clone, Debug, Default)]
pub struct Verdicts {
    /// (callsign, role) confirmed.
    pub confirmed: Option<(String, String)>,
    /// Callsigns rejected for this radio, lowercased.
    pub rejected: HashSet<String>,
}

pub fn fold(system: &str, radio: u32, sightings: &[Sighting], v: &Verdicts) -> Identity {
    let mut id = Identity {
        system: system.to_string(),
        radio,
        role: String::new(),
        callsign: String::new(),
        state: String::new(),
        share: 0.0,
        weight: 0.0,
        latest: String::new(),
        candidates: Vec::new(),
        evidence: sightings.len(),
        first_at: sightings.iter().map(|s| s.at).min().unwrap_or(0),
        last_at: sightings.iter().map(|s| s.at).max().unwrap_or(0),
    };

    // Callsigns, heaviest first, without the ones the listener refused.
    let mut by_sign: HashMap<String, (String, f64, i64)> = HashMap::new();
    for s in sightings.iter().filter(|s| s.role == UNIT && !s.callsign.is_empty()) {
        let key = s.callsign.to_lowercase();
        if v.rejected.contains(&key) {
            continue;
        }
        let e = by_sign.entry(key).or_insert((s.callsign.clone(), 0.0, 0));
        e.1 += s.weight;
        e.2 = e.2.max(s.at);
    }
    let mut cands: Vec<Candidate> = by_sign
        .into_values()
        .map(|(callsign, weight, last_at)| Candidate { callsign, weight, last_at })
        .collect();
    cands.sort_by(|a, b| b.weight.total_cmp(&a.weight).then(b.last_at.cmp(&a.last_at)));
    let unit_total: f64 = cands.iter().map(|c| c.weight).sum();

    // Roles. A radio that is a console most of the time is a console, even
    // though a console's words sometimes read like a unit's.
    let mut roles: HashMap<&str, f64> = HashMap::new();
    for s in sightings {
        let r = match s.role.as_str() {
            CONSOLE => CONSOLE,
            AUTOMATED => AUTOMATED,
            HOSPITAL => HOSPITAL,
            UNIT if !v.rejected.contains(&s.callsign.to_lowercase()) => UNIT,
            _ => continue,
        };
        *roles.entry(r).or_default() += s.weight;
    }
    let role_total: f64 = roles.values().sum();
    let fixed_role = [CONSOLE, AUTOMATED, HOSPITAL]
        .into_iter()
        .map(|r| (r, roles.get(r).copied().unwrap_or(0.0)))
        .filter(|(_, w)| *w >= ACCEPT_WEIGHT && *w >= ACCEPT_SHARE * role_total)
        .max_by(|a, b| a.1.total_cmp(&b.1));

    if let Some(top) = cands.first() {
        id.callsign = top.callsign.clone();
        id.weight = top.weight;
        id.share = if unit_total > 0.0 { top.weight / unit_total } else { 0.0 };
    }
    id.candidates = cands;

    if let Some((r, w)) = fixed_role {
        id.role = r.to_string();
        id.state = "learned".into();
        id.weight = w;
        id.share = if role_total > 0.0 { w / role_total } else { 0.0 };
        id.callsign.clear();
    } else if !id.callsign.is_empty() {
        id.role = UNIT.to_string();
        id.state = if id.weight >= ACCEPT_WEIGHT && id.share >= ACCEPT_SHARE {
            "learned".into()
        } else {
            "tentative".into()
        };
        // Has it changed hands? The last few sightings all name one other
        // callsign.
        let mut recent: Vec<&Sighting> = sightings
            .iter()
            .filter(|s| s.role == UNIT && !s.callsign.is_empty() && !v.rejected.contains(&s.callsign.to_lowercase()))
            .collect();
        recent.sort_by_key(|s| std::cmp::Reverse(s.at));
        if recent.len() >= CHANGE_RUN {
            let last = &recent[..CHANGE_RUN];
            let other = &last[0].callsign;
            if !other.eq_ignore_ascii_case(&id.callsign)
                && last.iter().all(|s| s.callsign.eq_ignore_ascii_case(other))
            {
                id.latest = other.clone();
                id.state = "changed".into();
            }
        }
    }

    if let Some((sign, role)) = &v.confirmed {
        id.role = if role.is_empty() { UNIT.into() } else { role.clone() };
        id.callsign = if id.role == UNIT { sign.clone() } else { String::new() };
        id.state = "confirmed".into();
        id.latest.clear();
    }
    id
}

// ---------------------------------------------------------------------------
// the library side
// ---------------------------------------------------------------------------

pub fn ensure_schema(c: &Connection) {
    let _ = c.execute_batch(
        "CREATE TABLE IF NOT EXISTS radio_evidence (
            id INTEGER PRIMARY KEY,
            system TEXT NOT NULL DEFAULT '',
            radio INTEGER NOT NULL,
            callsign TEXT NOT NULL DEFAULT '',
            role TEXT NOT NULL,
            how TEXT NOT NULL,
            call INTEGER NOT NULL DEFAULT 0,
            conversation INTEGER NOT NULL DEFAULT 0,
            at INTEGER NOT NULL,
            weight REAL NOT NULL,
            UNIQUE (system, radio, call, conversation, how, callsign)
         );
         CREATE INDEX IF NOT EXISTS radio_evidence_radio ON radio_evidence(system, radio, at);
         CREATE TABLE IF NOT EXISTS radio_verdicts (
            system TEXT NOT NULL DEFAULT '',
            radio INTEGER NOT NULL,
            callsign TEXT NOT NULL DEFAULT '',
            role TEXT NOT NULL DEFAULT '',
            verdict TEXT NOT NULL,
            at INTEGER NOT NULL,
            PRIMARY KEY (system, radio, verdict, callsign)
         );",
    );
}

fn store(c: &Connection, system: &str, conversation: i64, f: &[Found]) -> usize {
    let mut n = 0;
    for x in f {
        n += c
            .execute(
                "INSERT OR IGNORE INTO radio_evidence
                   (system, radio, callsign, role, how, call, conversation, at, weight)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![system, x.radio, x.callsign, x.role, x.how, x.call, conversation, x.at, x.weight],
            )
            .unwrap_or(0);
    }
    n
}

/// Store evidence, and say which radios it turned from unusable into usable
/// — the moment their recent reports are worth trying to join again.
fn store_noticing(c: &Connection, system: &str, conversation: i64, f: &[Found]) -> (usize, Vec<u32>) {
    let radios: Vec<(String, u32)> = f.iter().map(|x| (system.to_string(), x.radio)).collect();
    let before = identities(c, &radios);
    let added = store(c, system, conversation, f);
    if added == 0 {
        return (0, Vec::new());
    }
    (added, newly_usable(&before, &identities(c, &radios)))
}

/// Radios usable in `after` that were not in `before`.
pub fn newly_usable(before: &HashMap<(String, u32), Identity>, after: &HashMap<(String, u32), Identity>) -> Vec<u32> {
    let mut out: Vec<u32> = after
        .iter()
        .filter(|(k, id)| id.usable() && id.role == UNIT && !before.get(*k).is_some_and(Identity::usable))
        .map(|((_, radio), _)| *radio)
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

fn retry_joins(app: &AppHandle, radios: &[u32]) {
    for radio in radios {
        let n = crate::link::retry_for_radio(app, *radio);
        if n > 0 {
            println!("[radios] radio {radio} is learned; joined {n} earlier report(s)");
        }
    }
}

/// Every ten minutes: read the last six hours of dispatch and ops calls
/// again, which catches a reply whose transcript landed before the call it
/// answered, and try once more to join the reports still unjoined. Both are
/// rules and cost milliseconds.
pub const SWEEP_SECS: u64 = 10 * 60;

pub fn spawn_sweep(app: AppHandle) {
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(90));
        loop {
            let (added, learned, joined) = sweep(&app);
            if added + joined > 0 {
                println!("[radios] sweep: {added} new evidence, {} radio(s) learned, {joined} report(s) joined", learned.len());
            }
            std::thread::sleep(std::time::Duration::from_secs(SWEEP_SECS));
        }
    });
}

fn sweep(app: &AppHandle) -> (usize, Vec<u32>, usize) {
    let tgs = hospital_tgs(app);
    let state = app.state::<AppState>();
    let Some(db) = state.db.lock().unwrap().clone() else { return (0, Vec::new(), 0) };
    let (added, learned) = {
        let c = db.lock().unwrap();
        let since = crate::library::now() - crate::link::RETRY_WITHIN_SECS;
        let mut by_system: HashMap<String, Vec<Heard>> = HashMap::new();
        if let Ok(mut q) = c.prepare(
            "SELECT id, unit, tg, start, secs, COALESCE(transcript_edited, transcript, ''), system FROM calls
              WHERE start >= ?1 AND unit <> 0 AND length(COALESCE(transcript_edited, transcript, '')) > 3
              ORDER BY start, id",
        ) {
            if let Ok(rows) = q.query_map([since], |r| Ok((heard_row(r)?, r.get::<_, String>(6)?))) {
                for (h, system) in rows.flatten() {
                    if !tgs.contains(&h.tg) {
                        by_system.entry(system).or_default().push(h);
                    }
                }
            }
        }
        let vocab = crate::link::vocabulary(&c);
        let known = consoles(&c);
        let (mut added, mut learned) = (0, Vec::new());
        for (system, heard) in &by_system {
            let (n, new) = store_noticing(&c, system, 0, &read_calls(heard, &vocab, &known));
            added += n;
            learned.extend(new);
        }
        (added, learned)
    };
    if added > 0 {
        let _ = tauri::Emitter::emit(app, "radios", ());
    }
    retry_joins(app, &learned);
    let joined = crate::link::retry_recent(app);
    (added, learned, joined)
}

/// Radios the evidence, or the listener, already calls consoles — the page
/// voice included, since whoever keys up after a page is not answering it.
pub fn consoles(c: &Connection) -> HashSet<u32> {
    let mut out = HashSet::new();
    if let Ok(mut q) = c.prepare(
        "SELECT radio FROM radio_evidence WHERE role IN ('console', 'automated') GROUP BY system, radio HAVING SUM(weight) >= ?1
         UNION SELECT radio FROM radio_verdicts WHERE verdict = 'confirm' AND role IN ('console', 'automated')",
    ) {
        if let Ok(rows) = q.query_map([ACCEPT_WEIGHT], |r| r.get::<_, i64>(0)) {
            out.extend(rows.flatten().map(|r| r as u32));
        }
    }
    out
}

fn verdicts(c: &Connection, system: &str, radio: u32) -> Verdicts {
    let mut v = Verdicts::default();
    if let Ok(mut q) = c.prepare("SELECT callsign, role, verdict FROM radio_verdicts WHERE system = ?1 AND radio = ?2") {
        if let Ok(rows) = q.query_map(params![system, radio], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        }) {
            for (sign, role, verdict) in rows.flatten() {
                match verdict.as_str() {
                    "confirm" => v.confirmed = Some((sign, role)),
                    "reject" => {
                        v.rejected.insert(sign.to_lowercase());
                    }
                    _ => {}
                }
            }
        }
    }
    v
}

fn sightings(c: &Connection, system: &str, radio: u32) -> Vec<Sighting> {
    let Ok(mut q) = c.prepare("SELECT callsign, role, weight, at FROM radio_evidence WHERE system = ?1 AND radio = ?2") else {
        return Vec::new();
    };
    q.query_map(params![system, radio], |r| {
        Ok(Sighting {
            callsign: r.get(0)?,
            role: r.get(1)?,
            weight: r.get(2)?,
            at: r.get(3)?,
        })
    })
    .map(|rows| rows.flatten().collect())
    .unwrap_or_default()
}

/// The identity of one radio.
pub fn identity(c: &Connection, system: &str, radio: u32) -> Identity {
    fold(system, radio, &sightings(c, system, radio), &verdicts(c, system, radio))
}

/// Identities for a set of radios, keyed by (system, radio).
pub fn identities(c: &Connection, radios: &[(String, u32)]) -> HashMap<(String, u32), Identity> {
    let mut out = HashMap::new();
    let mut seen = HashSet::new();
    for (system, radio) in radios {
        if *radio == 0 || !seen.insert((system.clone(), *radio)) {
            continue;
        }
        out.insert((system.clone(), *radio), identity(c, system, *radio));
    }
    out
}

/// Put learned names on call rows whose radio has no alias.
pub fn fill_learned(c: &Connection, rows: &mut [crate::library::CallRow]) {
    let radios: Vec<(String, u32)> = rows
        .iter()
        .filter(|r| r.unit_name.is_none() && r.unit != 0)
        .map(|r| (r.system.clone(), r.unit))
        .collect();
    if radios.is_empty() {
        return;
    }
    let ids = identities(c, &radios);
    for r in rows.iter_mut().filter(|r| r.unit_name.is_none()) {
        r.learned = ids.get(&(r.system.clone(), r.unit)).and_then(Identity::label);
    }
}

/// Talkgroups that belong to a hospital in the place book. Those are read
/// as whole conversations, where it is known which radio is the hospital's.
fn hospital_tgs(app: &AppHandle) -> HashSet<u16> {
    crate::places::load(app)
        .settings
        .places
        .iter()
        .filter(|p| p.enabled)
        .flat_map(|p| p.tgs.iter().copied())
        .collect()
}

/// The transmissions on one talkgroup in the half minute before `at`, oldest
/// first, so a reply can find the console that called it.
fn lead_in(c: &Connection, tg: u16, system: &str, at: i64, call: i64) -> Vec<Heard> {
    let Ok(mut q) = c.prepare(
        "SELECT id, unit, tg, start, secs, COALESCE(transcript_edited, transcript, '') FROM calls
          WHERE tg = ?1 AND system = ?2 AND start BETWEEN ?3 AND ?4 AND id <> ?5
          ORDER BY start, id",
    ) else {
        return Vec::new();
    };
    q.query_map(params![tg, system, at - 3 * REPLY_SECS, at, call], heard_row)
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
}

fn heard_row(r: &rusqlite::Row) -> rusqlite::Result<Heard> {
    Ok(Heard {
        call: r.get(0)?,
        radio: r.get::<_, i64>(1)? as u32,
        tg: r.get::<_, i64>(2)? as u16,
        at: r.get(3)?,
        secs: r.get(4)?,
        text: r.get(5)?,
    })
}

/// A transcript landed: read what it says about its radio.
pub fn on_transcript(app: &AppHandle, id: i64, _text: &str) {
    let app = app.clone();
    std::thread::spawn(move || {
        let tgs = hospital_tgs(&app);
        let state = app.state::<AppState>();
        let Some(db) = state.db.lock().unwrap().clone() else { return };
        let c = db.lock().unwrap();
        let Ok(Some(row)) = crate::library::get(&c, id) else { return };
        if row.unit == 0 || tgs.contains(&row.tg) {
            return;
        }
        let text = row.transcript_edited.clone().or(row.transcript.clone()).unwrap_or_default();
        let mut heard = lead_in(&c, row.tg, &row.system, row.start, row.id);
        heard.push(Heard {
            call: row.id,
            radio: row.unit,
            tg: row.tg,
            at: row.start,
            secs: row.secs,
            text,
        });
        let vocab = crate::link::vocabulary(&c);
        let found: Vec<Found> = read_calls(&heard, &vocab, &consoles(&c))
            .into_iter()
            .filter(|f| f.call == id)
            .collect();
        let (added, learned) = store_noticing(&c, &row.system, 0, &found);
        drop(c);
        if added > 0 {
            let _ = tauri::Emitter::emit(&app, "radios", ());
        }
        retry_joins(&app, &learned);
    });
}

/// A hospital conversation was stored: read who took part.
pub fn from_conversation(app: &AppHandle, conversation: i64) {
    let state = app.state::<AppState>();
    let Some(db) = state.db.lock().unwrap().clone() else { return };
    let c = db.lock().unwrap();
    let (added, learned) = read_conversation_noticing(&c, conversation);
    drop(c);
    if added > 0 {
        let _ = tauri::Emitter::emit(app, "radios", ());
    }
    retry_joins(app, &learned);
}

fn read_stored_conversation(c: &Connection, conversation: i64) -> usize {
    read_conversation_noticing(c, conversation).0
}

fn read_conversation_noticing(c: &Connection, conversation: i64) -> (usize, Vec<u32>) {
    let Ok(pieces) = c.query_row("SELECT pieces FROM conversations WHERE id = ?1", [conversation], |r| {
        r.get::<_, String>(0)
    }) else {
        return (0, Vec::new());
    };
    let pieces: Vec<crate::conversations::Piece> = serde_json::from_str(&pieces).unwrap_or_default();
    let system = pieces
        .iter()
        .find_map(|p| p.id)
        .and_then(|id| c.query_row("SELECT system FROM calls WHERE id = ?1", [id], |r| r.get::<_, String>(0)).ok())
        .unwrap_or_default();
    let vocab = crate::link::vocabulary(c);
    store_noticing(c, &system, conversation, &read_conversation(&pieces, &vocab))
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct BackfillReport {
    pub calls: usize,
    pub conversations: usize,
    pub added: usize,
    pub radios: usize,
    pub learned: usize,
}

/// Read the whole library for evidence. Nothing is sent anywhere and no
/// model is asked; running it twice adds nothing the second time.
/// Read the whole library for evidence. Nothing is sent anywhere and no
/// model is asked; running it twice adds nothing the second time.
#[tauri::command]
pub async fn radios_backfill(app: AppHandle) -> Result<BackfillReport, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let tgs = hospital_tgs(&app);
        let state = app.state::<AppState>();
        let db = state.db.lock().unwrap().clone().ok_or("library not open")?;
        let report = {
            let c = db.lock().unwrap();
            backfill(&c, &tgs)?
        };
        let _ = tauri::Emitter::emit(&app, "radios", ());
        Ok(report)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The backfill itself, over one open library. `hospital` are the
/// talkgroups read as conversations rather than as single calls.
pub fn backfill(c: &Connection, hospital: &HashSet<u16>) -> Result<BackfillReport, String> {
    let mut report = BackfillReport::default();
    let vocab = crate::link::vocabulary(c);

    // Calls, per system, in order. Consoles are read first so the reply rule
    // knows who is calling.
    let mut by_system: HashMap<String, Vec<Heard>> = HashMap::new();
    {
        let mut q = c
            .prepare(
                "SELECT id, unit, tg, start, secs, COALESCE(transcript_edited, transcript, ''), system FROM calls
                  WHERE unit <> 0 AND length(COALESCE(transcript_edited, transcript, '')) > 3
                  ORDER BY start, id",
            )
            .map_err(|e| e.to_string())?;
        let rows = q
            .query_map([], |r| Ok((heard_row(r)?, r.get::<_, String>(6)?)))
            .map_err(|e| e.to_string())?;
        for (h, system) in rows.flatten() {
            if hospital.contains(&h.tg) {
                continue;
            }
            report.calls += 1;
            by_system.entry(system).or_default().push(h);
        }
    }
    let none = HashSet::new();
    for (system, heard) in &by_system {
        // First pass: only who the consoles and the page voice are.
        let first: Vec<Found> = read_calls(heard, &vocab, &none)
            .into_iter()
            .filter(|f| f.role != UNIT)
            .collect();
        report.added += store(c, system, 0, &first);
    }
    let known = consoles(c);
    for (system, heard) in &by_system {
        report.added += store(c, system, 0, &read_calls(heard, &vocab, &known));
    }

    let ids: Vec<i64> = {
        let mut q = c.prepare("SELECT id FROM conversations").map_err(|e| e.to_string())?;
        let rows = q.query_map([], |r| r.get(0)).map_err(|e| e.to_string())?;
        rows.flatten().collect()
    };
    for id in ids {
        report.conversations += 1;
        report.added += read_stored_conversation(c, id);
    }

    let all = list(c);
    report.radios = all.len();
    report.learned = all.iter().filter(|i| i.usable()).count();
    Ok(report)
}

fn list(c: &Connection) -> Vec<Identity> {
    let radios: Vec<(String, u32)> = c
        .prepare(
            "SELECT system, radio FROM radio_evidence GROUP BY system, radio
             UNION SELECT system, radio FROM radio_verdicts WHERE verdict = 'confirm'",
        )
        .and_then(|mut q| {
            q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u32)))
                .map(|rows| rows.flatten().collect())
        })
        .unwrap_or_default();
    let mut out: Vec<Identity> = identities(c, &radios).into_values().collect();
    out.sort_by(|a, b| b.last_at.cmp(&a.last_at).then(a.radio.cmp(&b.radio)));
    out
}

fn with_db<T>(state: &State<AppState>, f: impl FnOnce(&Connection) -> Result<T, String>) -> Result<T, String> {
    let db = state.db.lock().unwrap().clone().ok_or("library not open")?;
    let c = db.lock().unwrap();
    f(&c)
}

/// Every radio anything has been learned about, most recently heard first.
#[tauri::command]
pub fn radios_list(state: State<AppState>) -> Result<Vec<Identity>, String> {
    with_db(&state, |c| Ok(list(c)))
}

#[tauri::command]
pub fn radio_identity(state: State<AppState>, system: String, radio: u32) -> Result<Identity, String> {
    with_db(&state, |c| Ok(identity(c, &system, radio)))
}

/// One piece of evidence, with the call behind it so it can be played.
#[derive(Serialize, Clone, Debug)]
pub struct EvidenceRow {
    pub at: i64,
    pub callsign: String,
    pub role: String,
    pub how: String,
    pub weight: f64,
    pub call: i64,
    pub conversation: i64,
    pub tg: Option<u16>,
    pub tg_name: String,
    pub transcript: String,
}

#[tauri::command]
pub fn radio_evidence(state: State<AppState>, system: String, radio: u32) -> Result<Vec<EvidenceRow>, String> {
    with_db(&state, |c| {
        let mut q = c
            .prepare(
                "SELECT e.at, e.callsign, e.role, e.how, e.weight, e.call, e.conversation,
                        k.tg, COALESCE(k.tg_name, ''), COALESCE(k.transcript_edited, k.transcript, '')
                   FROM radio_evidence e LEFT JOIN calls k ON k.id = e.call
                  WHERE e.system = ?1 AND e.radio = ?2
                  ORDER BY e.at DESC LIMIT 200",
            )
            .map_err(|e| e.to_string())?;
        let rows = q
            .query_map(params![system, radio], |r| {
                Ok(EvidenceRow {
                    at: r.get(0)?,
                    callsign: r.get(1)?,
                    role: r.get(2)?,
                    how: r.get(3)?,
                    weight: r.get(4)?,
                    call: r.get(5)?,
                    conversation: r.get(6)?,
                    tg: r.get::<_, Option<i64>>(7)?.map(|t| t as u16),
                    tg_name: r.get(8)?,
                    transcript: r.get(9)?,
                })
            })
            .map_err(|e| e.to_string())?;
        Ok(rows.flatten().collect())
    })
}

/// A callsign typed by a person: "medic32", " Medic  32 " → "Medic 32".
pub fn tidy_callsign(s: &str) -> Option<String> {
    let w = words(s);
    let (title, n) = match w.as_slice() {
        [t, n] => (t.clone(), n.parse::<u32>().ok()?),
        [one] => {
            let split = one.find(|c: char| c.is_ascii_digit())?;
            let (t, n) = one.split_at(split);
            (t.to_string(), n.parse::<u32>().ok()?)
        }
        _ => return None,
    };
    (!title.is_empty() && title.chars().all(|c| c.is_alphabetic())).then(|| display(&title, n))
}

/// The listener says what a radio is. `role` is unit (with a callsign),
/// console, automated or hospital.
#[tauri::command]
pub fn radio_confirm(
    app: AppHandle,
    state: State<AppState>,
    system: String,
    radio: u32,
    role: String,
    callsign: String,
) -> Result<Identity, String> {
    let role = match role.as_str() {
        UNIT | CONSOLE | AUTOMATED | HOSPITAL => role,
        _ => return Err(format!("unknown role `{role}`")),
    };
    let sign = if role == UNIT {
        tidy_callsign(&callsign).ok_or("a unit needs a callsign like \"Medic 32\"")?
    } else {
        String::new()
    };
    if radio == 0 {
        return Err("no radio ID on this call".into());
    }
    let out = with_db(&state, |c| {
        c.execute(
            "DELETE FROM radio_verdicts WHERE system = ?1 AND radio = ?2 AND verdict = 'confirm'",
            params![system, radio],
        )
        .map_err(|e| e.to_string())?;
        c.execute(
            "DELETE FROM radio_verdicts WHERE system = ?1 AND radio = ?2 AND verdict = 'reject' AND lower(callsign) = lower(?3)",
            params![system, radio, sign],
        )
        .map_err(|e| e.to_string())?;
        c.execute(
            "INSERT INTO radio_verdicts (system, radio, callsign, role, verdict, at) VALUES (?1, ?2, ?3, ?4, 'confirm', ?5)",
            params![system, radio, sign, role, crate::library::now()],
        )
        .map_err(|e| e.to_string())?;
        Ok(identity(c, &system, radio))
    })?;
    let _ = tauri::Emitter::emit(&app, "radios", ());
    if out.usable() && out.role == UNIT {
        let app = app.clone();
        std::thread::spawn(move || retry_joins(&app, &[radio]));
    }
    Ok(out)
}

/// What a person typed about a radio: a callsign ("medic 32"), or what kind
/// of radio it is. Returns (role, callsign).
pub fn parse_answer(s: &str) -> Option<(String, String)> {
    let w = words(s).join(" ");
    let role = match w.as_str() {
        "console" | "dispatch console" | "control" | "dispatch" => CONSOLE,
        "hospital" | "hospital radio" | "er" | "ed" => HOSPITAL,
        "page" | "page voice" | "dispatch page voice" | "automated" | "tone out" => AUTOMATED,
        _ => return tidy_callsign(s).map(|c| (UNIT.to_string(), c)),
    };
    Some((role.to_string(), String::new()))
}

fn call_radio(c: &Connection, call: i64) -> Result<(String, u32), String> {
    let row = crate::library::get(c, call)?.ok_or("that call is not in the library")?;
    if row.unit == 0 {
        return Err("the system did not say which radio made this call".into());
    }
    Ok((row.system, row.unit))
}

/// The identity of the radio that made a call.
#[tauri::command]
pub fn radio_for_call(state: State<AppState>, call: i64) -> Result<Identity, String> {
    with_db(&state, |c| {
        let (system, radio) = call_radio(c, call)?;
        Ok(identity(c, &system, radio))
    })
}

/// Say what the radio that made a call is: "Medic 32", "console",
/// "hospital" or "page voice".
#[tauri::command]
pub fn radio_answer_call(app: AppHandle, state: State<AppState>, call: i64, answer: String) -> Result<Identity, String> {
    let (system, radio) = with_db(&state, |c| call_radio(c, call))?;
    radio_answer(app, state, system, radio, answer)
}

/// Say what a radio is, in words: "Medic 32", "console", "hospital" or
/// "page voice".
#[tauri::command]
pub fn radio_answer(app: AppHandle, state: State<AppState>, system: String, radio: u32, answer: String) -> Result<Identity, String> {
    let (role, callsign) = parse_answer(&answer)
        .ok_or("say a callsign like \"Medic 32\", or console, hospital or page voice")?;
    radio_confirm(app, state, system, radio, role, callsign)
}

/// The listener says a radio is not this callsign.
#[tauri::command]
pub fn radio_reject(
    app: AppHandle,
    state: State<AppState>,
    system: String,
    radio: u32,
    callsign: String,
) -> Result<Identity, String> {
    let out = with_db(&state, |c| {
        c.execute(
            "DELETE FROM radio_verdicts WHERE system = ?1 AND radio = ?2 AND verdict = 'confirm' AND lower(callsign) = lower(?3)",
            params![system, radio, callsign],
        )
        .map_err(|e| e.to_string())?;
        c.execute(
            "INSERT OR REPLACE INTO radio_verdicts (system, radio, callsign, role, verdict, at) VALUES (?1, ?2, ?3, 'unit', 'reject', ?4)",
            params![system, radio, callsign, crate::library::now()],
        )
        .map_err(|e| e.to_string())?;
        Ok(identity(c, &system, radio))
    })?;
    let _ = tauri::Emitter::emit(&app, "radios", ());
    Ok(out)
}

/// Forget what the listener said about a radio; the evidence decides again.
#[tauri::command]
pub fn radio_unconfirm(app: AppHandle, state: State<AppState>, system: String, radio: u32) -> Result<Identity, String> {
    let out = with_db(&state, |c| {
        c.execute(
            "DELETE FROM radio_verdicts WHERE system = ?1 AND radio = ?2",
            params![system, radio],
        )
        .map_err(|e| e.to_string())?;
        Ok(identity(c, &system, radio))
    })?;
    let _ = tauri::Emitter::emit(&app, "radios", ());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversations::Piece;

    fn vocab() -> HashSet<String> {
        ["medic", "engine", "ladder", "squad", "ambulance", "ems"].iter().map(|s| s.to_string()).collect()
    }

    fn one(text: &str) -> Vec<Said> {
        read_speaker(text, &vocab())
    }

    fn sign(text: &str) -> Option<(String, f64)> {
        one(text).into_iter().find_map(|s| match s {
            Said::Callsign { sign, weight, .. } => Some((sign, weight)),
            _ => None,
        })
    }

    #[test]
    fn a_crew_calling_control_names_itself() {
        assert_eq!(sign("Control, Medic 32."), Some(("Medic 32".into(), 1.0)));
        assert_eq!(sign("Fire control, this is engine 36. Mark this working."), Some(("Engine 36".into(), 1.0)));
        assert_eq!(sign("Control from engine 44, this is not a cardiac arrest."), Some(("Engine 44".into(), 1.0)));
        assert_eq!(sign("Control engine 24, can you start us an ALS transport?"), Some(("Engine 24".into(), 1.0)));
        assert_eq!(sign("Medic 32 to control."), Some(("Medic 32".into(), 1.0)));
        assert_eq!(sign("uh control, EMS93, I can take that from 91"), Some(("EMS 93".into(), 1.0)));
        assert_eq!(sign("Example General, this is Medic 17, how do you copy?"), Some(("Medic 17".into(), 1.0)));
        assert_eq!(sign("Medic 37 calling Children's. Go ahead."), Some(("Medic 37".into(), 1.0)));
    }

    #[test]
    fn what_a_person_types_about_a_radio() {
        assert_eq!(parse_answer("medic 32"), Some((UNIT.into(), "Medic 32".into())));
        assert_eq!(parse_answer("Console"), Some((CONSOLE.into(), "".into())));
        assert_eq!(parse_answer("page voice"), Some((AUTOMATED.into(), "".into())));
        assert_eq!(parse_answer("Hospital"), Some((HOSPITAL.into(), "".into())));
        assert_eq!(parse_answer("somebody"), None);
    }

    #[test]
    fn a_radio_is_news_the_moment_it_becomes_usable() {
        let v = Verdicts::default();
        let two = [s("Medic 44", 1.0, 1), s("Medic 44", 1.0, 2)];
        let three = [s("Medic 44", 1.0, 1), s("Medic 44", 1.0, 2), s("Medic 44", 1.0, 3)];
        let key = ("sys".to_string(), 900777);
        let at = |sightings: &[Sighting]| HashMap::from([(key.clone(), fold("sys", 900777, sightings, &v))]);
        assert_eq!(newly_usable(&at(&two), &at(&three)), vec![900777]);
        // Already usable is not news again, and a radio that is still
        // tentative is not news yet.
        assert!(newly_usable(&at(&three), &at(&three)).is_empty());
        assert!(newly_usable(&HashMap::new(), &at(&two)).is_empty());
        // A console is not a crew whose reports can be joined.
        let console: Vec<Sighting> = (0..3).map(|i| Sighting { callsign: "".into(), role: CONSOLE.into(), weight: 1.0, at: i }).collect();
        assert!(newly_usable(&HashMap::new(), &at(&console)).is_empty());
    }

    #[test]
    fn a_status_is_weaker_evidence() {
        assert_eq!(sign("Medic 20, transporting. Emergent."), Some(("Medic 20".into(), 0.5)));
    }

    #[test]
    fn a_console_calling_a_unit_is_not_that_unit() {
        assert_eq!(one("Medic 35 from control."), vec![Said::Console]);
        assert_eq!(sign("Medic 29, just information, please stay on scene."), None);
        assert_eq!(sign("Medic 35, Medic 26, advise control of your status at the hospital."), None);
        // Someone else's callsign in the middle is not the speaker's.
        assert_eq!(sign("They asked for a second transport for engine 44."), None);
        assert_eq!(sign("Was that traffic for Ambulance 15?"), None);
    }

    #[test]
    fn a_page_is_the_automated_voice() {
        let page = "Engine 27, Medic 20, 1200 Example St, Cardiac Arrest. Engine 27, Medic 20, 1200 Example St, Cardiac Arrest. 623 Hours. Location 1000 North 4000 East.";
        assert_eq!(one(page), vec![Said::Automated]);
        assert!(!is_page("Working arrest 1748."));
    }

    #[test]
    fn numbers_that_are_not_unit_numbers_are_ignored() {
        assert_eq!(sign("Control, medic 9004321"), None);
        assert_eq!(sign("Control, a 67 year old"), None);
        assert_eq!(tidy_callsign(" medic  32 "), Some("Medic 32".into()));
        assert_eq!(tidy_callsign("ems93"), Some("EMS 93".into()));
        assert_eq!(tidy_callsign("medic"), None);
    }

    fn h(call: i64, radio: u32, at: i64, text: &str) -> Heard {
        Heard { call, radio, tg: 1, at, secs: 3.0, text: text.into() }
    }

    #[test]
    fn the_radio_that_answers_a_console_is_the_unit_it_called() {
        let consoles: HashSet<u32> = [900001].into_iter().collect();
        let heard = vec![
            h(1, 900001, 100, "Medic 35, you're safe upon arrival."),
            h(2, 900222, 106, "Copy, thank you."),
            h(3, 900333, 200, "Clear."),
        ];
        let f = read_calls(&heard, &vocab(), &consoles);
        assert_eq!(f.len(), 1);
        assert_eq!((f[0].radio, f[0].callsign.as_str(), f[0].how, f[0].weight), (900222, "Medic 35", "reply_to_control", 0.5));
    }

    #[test]
    fn a_late_or_self_naming_reply_is_not_taken_for_the_called_unit() {
        let consoles: HashSet<u32> = [900001].into_iter().collect();
        let late = vec![h(1, 900001, 100, "Medic 35, you're safe upon arrival."), h(2, 900222, 160, "Copy.")];
        assert!(read_calls(&late, &vocab(), &consoles).is_empty());
        let other = vec![
            h(1, 900001, 100, "Medic 35, you're safe upon arrival."),
            h(2, 900222, 105, "Control, Engine 9, we'll be clear."),
        ];
        let f = read_calls(&other, &vocab(), &consoles);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].callsign, "Engine 9");
    }

    #[test]
    fn a_console_never_learns_a_callsign_from_its_own_words() {
        let consoles: HashSet<u32> = [900001].into_iter().collect();
        let heard = vec![h(1, 900001, 100, "Medic 20, transporting to Example General, 1453.")];
        assert!(read_calls(&heard, &vocab(), &consoles).is_empty());
    }

    fn piece(unit: u32, fixed: bool, at: i64, text: &str) -> Piece {
        Piece { id: Some(at), unit, unit_name: None, fixed, at, secs: 3.0, audio: None, transcript: Some(text.into()) }
    }

    #[test]
    fn a_hospital_answering_a_crew_names_the_crew() {
        let p = vec![
            piece(900500, true, 10, "Medic 44, this is Example General."),
            piece(900777, false, 15, "We're en route with a 70 year old male."),
        ];
        let f = read_conversation(&p, &vocab());
        assert!(f.contains(&Found { radio: 900500, callsign: "".into(), role: HOSPITAL, how: "hospital_radio", call: 10, at: 10, weight: 1.0 }));
        assert!(f.iter().any(|x| x.radio == 900777 && x.callsign == "Medic 44" && x.how == "hospital_addressed"));
    }

    #[test]
    fn with_two_crews_the_hospital_names_neither() {
        let p = vec![
            piece(900500, true, 10, "Medic 44, go ahead."),
            piece(900777, false, 15, "Go ahead."),
            piece(900778, false, 20, "Standing by."),
        ];
        assert!(read_conversation(&p, &vocab()).iter().all(|x| x.role == HOSPITAL));
    }

    fn s(callsign: &str, weight: f64, at: i64) -> Sighting {
        Sighting { callsign: callsign.into(), role: UNIT.into(), weight, at }
    }

    #[test]
    fn a_callsign_needs_weight_and_a_majority() {
        let v = Verdicts::default();
        let two = fold("", 1, &[s("Medic 44", 1.0, 1), s("Medic 44", 1.0, 2)], &v);
        assert_eq!((two.callsign.as_str(), two.state.as_str()), ("Medic 44", "tentative"));
        assert!(!two.usable());
        // A mis-heard digit is outvoted.
        let three = fold("", 1, &[s("Medic 44", 1.0, 1), s("Medic 24", 1.0, 2), s("Medic 44", 1.0, 3), s("Medic 44", 1.0, 4)], &v);
        assert_eq!((three.callsign.as_str(), three.state.as_str()), ("Medic 44", "learned"));
        assert_eq!(three.label(), Some("Medic 44".into()));
        // Three of weight but not most of what was said is not enough.
        let split = fold("", 1, &[s("Medic 44", 3.0, 1), s("Medic 24", 2.5, 2)], &v);
        assert_eq!(split.state, "tentative");
    }

    #[test]
    fn a_radio_that_changes_hands_stops_being_used() {
        let v = Verdicts::default();
        let mut old: Vec<Sighting> = (0..6).map(|i| s("Medic 44", 1.0, i)).collect();
        old.extend((10..13).map(|i| s("Medic 12", 1.0, i)));
        let id = fold("", 1, &old, &v);
        assert_eq!((id.callsign.as_str(), id.latest.as_str(), id.state.as_str()), ("Medic 44", "Medic 12", "changed"));
        assert!(!id.usable());
    }

    #[test]
    fn the_listener_has_the_last_word() {
        let learned = [s("Medic 24", 1.0, 1), s("Medic 24", 1.0, 2), s("Medic 24", 1.0, 3)];
        let mut v = Verdicts::default();
        v.rejected.insert("medic 24".into());
        let rejected = fold("", 1, &learned, &v);
        assert_eq!(rejected.state, "");
        assert!(rejected.label().is_none());
        v.confirmed = Some(("Medic 44".into(), UNIT.into()));
        let confirmed = fold("", 1, &learned, &v);
        assert_eq!((confirmed.callsign.as_str(), confirmed.state.as_str()), ("Medic 44", "confirmed"));
    }

    #[test]
    fn a_console_is_a_console_even_when_its_words_look_like_a_unit() {
        let mut sightings: Vec<Sighting> = (0..5)
            .map(|i| Sighting { callsign: "".into(), role: CONSOLE.into(), weight: 1.0, at: i })
            .collect();
        sightings.push(s("Medic 20", 0.5, 9));
        let id = fold("", 1, &sightings, &Verdicts::default());
        assert_eq!((id.role.as_str(), id.label()), (CONSOLE, Some("Dispatch console".into())));
    }

    #[test]
    fn evidence_is_stored_once() {
        let c = Connection::open_in_memory().unwrap();
        ensure_schema(&c);
        let f = vec![Found { radio: 900222, callsign: "Medic 35".into(), role: UNIT, how: "said_self", call: 7, at: 100, weight: 1.0 }];
        assert_eq!(store(&c, "sys", 0, &f), 1);
        assert_eq!(store(&c, "sys", 0, &f), 0);
        assert_eq!(sightings(&c, "sys", 900222).len(), 1);
        assert!(sightings(&c, "other", 900222).is_empty(), "a radio ID is only unique within one system");
    }

    /// Against a copy of a real library: `HS_RADIOS_DB=/path/to/copy.db
    /// HS_RADIOS_HOSPITAL_TGS=101,102 cargo test radios::tests::real -- --ignored --nocapture`.
    /// Writes evidence into that copy. Never point it at the live file.
    #[test]
    #[ignore]
    fn real_library() {
        let Ok(path) = std::env::var("HS_RADIOS_DB") else { return };
        let c = Connection::open(&path).unwrap();
        crate::conversations::ensure_schema(&c);
        crate::link::ensure_schema(&c);
        ensure_schema(&c);
        let tgs: HashSet<u16> = std::env::var("HS_RADIOS_HOSPITAL_TGS")
            .unwrap_or_default()
            .split(',')
            .filter_map(|t| t.trim().parse().ok())
            .collect();
        let t0 = std::time::Instant::now();
        let r = backfill(&c, &tgs).unwrap();
        println!("{r:?} in {:?}", t0.elapsed());
        let again = backfill(&c, &tgs).unwrap();
        assert_eq!(again.added, 0, "a second backfill adds nothing");
        let all = list(&c);
        let mut by: HashMap<(String, String), usize> = HashMap::new();
        for i in &all {
            *by.entry((i.role.clone(), i.state.clone())).or_default() += 1;
        }
        let mut by: Vec<_> = by.into_iter().collect();
        by.sort();
        println!("role/state: {by:?}");
        for i in all.iter().filter(|i| i.state == "learned" || i.state == "changed").take(60) {
            println!(
                "{:>8} {:<9} {:<9} {:<12} w={:.1} share={:.2} latest={} cands={:?}",
                i.radio, i.role, i.state, i.callsign, i.weight, i.share, i.latest,
                i.candidates.iter().take(3).map(|c| format!("{}:{:.1}", c.callsign, c.weight)).collect::<Vec<_>>()
            );
        }
    }
}
