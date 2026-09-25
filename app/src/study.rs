//! The study: the survival score worked out in code, and a reference standard
//! kept apart from everything the app itself produces.
//!
//! Two jobs, both for a validation study of the arrest pipeline:
//!
//! - **The score.** The ECPR screen used to ask the model for a likelihood.
//!   A number a model is asked to guess cannot be checked for how accurately
//!   it was carried over, so the score is computed here, from the facts the
//!   crew's report stated, by arithmetic a reader can follow. A criterion
//!   whose input was not said is *unknown* and widens the estimate to a
//!   range; it is never filled in.
//! - **The reference standard.** Reviewers abstract the same variables from
//!   the audio, blind to what the app made of it. They work from a packet of
//!   audio and blank sheets written to disk, so nothing they see carries the
//!   app's output, and what they send back lands in tables of its own. The
//!   pipeline never reads those tables: a reference transcript is not the
//!   edited transcript (which the pipeline does read, and which rebuilds the
//!   case), so reviewing can never change what is being measured.
//!
//! Comparison writes the app's value beside each reviewer's and the
//! reference, per variable, and word error rates per call, as CSV for the
//! statistician. The error class (transcription, extraction, calculator,
//! unsupported) is left as a column to be coded by hand: telling a mis-heard
//! word from a mis-read one takes a person with the audio.

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use tauri::State;

use crate::research::CaseRow;
use crate::AppState;

// ---------------------------------------------------------------------------
// the score
// ---------------------------------------------------------------------------

/// The score, by the name a methods section would give it.
pub const SCORE_NAME: &str = "ED-ECPR 4-criterion screen";

/// Favourable-outcome estimate, percent, by criteria met: the table the
/// program's ED-ECPR gate uses (4 of 4 about 46 %, 3 of 4 about 12 %, two or
/// fewer 0 to 5 %). One home, so a study that names a different table
/// changes it here and nowhere else.
pub fn band(met: u8) -> (f64, f64) {
    match met {
        4.. => (46.0, 46.0),
        3 => (12.0, 12.0),
        _ => (0.0, 5.0),
    }
}

/// Oldest age the screen accepts; older is a hard exclusion.
pub const MAX_AGE: u32 = 65;
/// Age plus low-flow minutes must come in under this.
pub const AGE_PLUS_LOW_FLOW: u32 = 100;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Criterion {
    pub key: String,
    pub label: String,
    /// `met` | `not met` | `unknown` | `assumed` (met because nothing said
    /// otherwise, as the gate specifies for end-stage disease).
    pub verdict: String,
    /// The arithmetic or the words it rests on.
    pub why: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Score {
    pub name: String,
    pub criteria: Vec<Criterion>,
    /// Criteria met, counting `assumed`.
    pub met: u8,
    pub unknown: u8,
    pub assumed: u8,
    /// A hard exclusion, and why; empty when none.
    pub excluded: String,
    /// The estimate if every unknown criterion failed, and if every one held.
    pub lo_pct: Option<f64>,
    pub hi_pct: Option<f64>,
    /// Every input was stated (an assumed criterion still counts as stated).
    pub complete: bool,
    /// What a reader is shown: "46%", "12–46% (1 of 4 not stated)", "excluded: …".
    pub estimate: String,
}

/// Work the score out from the facts a report stated, by fact key
/// (`age`, `downtime`, `witnessed`, `bystander cpr`, `history`). A key that
/// is absent was not stated.
pub fn score(facts: &BTreeMap<String, String>) -> Score {
    let get = |k: &str| facts.get(k).map(String::as_str).filter(|v| !is_unstated(v));
    let age = get("age").and_then(parse_age);
    let low_flow = get("downtime").and_then(parse_minutes);
    let crit = |key: &str, label: &str, verdict: &str, why: String| Criterion { key: key.into(), label: label.into(), verdict: verdict.into(), why };

    let time = match (age, low_flow) {
        (Some(a), Some(m)) => {
            let ok = a + m < AGE_PLUS_LOW_FLOW;
            crit("time", "Age + low-flow minutes < 100", if ok { "met" } else { "not met" }, format!("{a} + {m} min stated = {}", a + m))
        }
        (Some(a), None) if a >= AGE_PLUS_LOW_FLOW => crit("time", "Age + low-flow minutes < 100", "not met", format!("age {a} alone")),
        (a, m) => crit(
            "time",
            "Age + low-flow minutes < 100",
            "unknown",
            match (a, m) {
                (None, None) => "age and downtime not stated".into(),
                (None, _) => "age not stated".into(),
                _ => "downtime not stated".into(),
            },
        ),
    };
    let yes_no = |key: &str, fact: &str, label: &str| match get(fact) {
        Some(v) => match parse_yes_no(v) {
            Some(true) => crit(key, label, "met", format!("“{v}”")),
            Some(false) => crit(key, label, "not met", format!("“{v}”")),
            None => crit(key, label, "unknown", format!("“{v}” does not say")),
        },
        None => crit(key, label, "unknown", "not stated".into()),
    };
    let witnessed = yes_no("witnessed", "witnessed", "Witnessed arrest");
    let bystander = yes_no("bystander", "bystander cpr", "Bystander CPR");
    let disease = match get("history") {
        Some(v) if end_stage(v) => crit("disease", "No known end-stage disease", "not met", format!("“{v}”")),
        Some(v) => crit("disease", "No known end-stage disease", "met", format!("“{v}”")),
        None => crit("disease", "No known end-stage disease", "assumed", "no history stated; the screen counts it met".into()),
    };
    let criteria = vec![time, witnessed, bystander, disease];
    let count = |v: &str| criteria.iter().filter(|c| c.verdict == v).count() as u8;
    let (met, unknown, assumed) = (count("met") + count("assumed"), count("unknown"), count("assumed"));
    let excluded = match age {
        Some(a) if a > MAX_AGE => format!("age {a} is over {MAX_AGE}"),
        _ => String::new(),
    };
    let mut s = Score { name: SCORE_NAME.into(), criteria, met, unknown, assumed, excluded, complete: unknown == 0, ..Default::default() };
    if !s.excluded.is_empty() {
        s.estimate = format!("excluded: {}", s.excluded);
        return s;
    }
    let (lo, _) = band(met);
    let (_, hi) = band(met + unknown);
    s.lo_pct = Some(lo);
    s.hi_pct = Some(hi);
    let pct = |x: f64| format!("{}", x.round() as i64);
    s.estimate = if lo == hi {
        format!("{}%", pct(lo))
    } else {
        format!("{}–{}%", pct(lo), pct(hi))
    };
    if unknown > 0 {
        s.estimate.push_str(&format!(" ({unknown} of 4 not stated)"));
    }
    s
}

// ---------------------------------------------------------------------------
// reading a stated value
// ---------------------------------------------------------------------------

/// Words that mean the thing was not said, which is not the same as "no".
pub fn is_unstated(v: &str) -> bool {
    let v = v.trim().trim_end_matches('.').to_ascii_lowercase();
    ["", "not stated", "not said", "unknown", "unclear", "n/a", "na", "none stated", "not mentioned", "not reported", "?"].contains(&v.as_str())
}

fn numbers(v: &str) -> Vec<(u32, usize, usize)> {
    let b = v.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() {
            let s = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if let Ok(n) = v[s..i].parse() {
                out.push((n, s, i));
            }
        } else {
            i += 1;
        }
    }
    out
}

/// An age in years. "62", "62 yo", "62-year-old male" read; "60s", "50 to 60"
/// and "elderly" do not, because they do not give one number. An infant
/// ("8 months") is 0.
pub fn parse_age(v: &str) -> Option<u32> {
    let l = v.to_ascii_lowercase();
    let n = numbers(&l);
    if n.len() != 1 {
        return None;
    }
    let (age, _, end) = n[0];
    let after = &l[end..];
    if after.starts_with('s') || after.starts_with("'s") {
        return None;
    }
    let unit = after.trim_start_matches([' ', '-']);
    let unit = unit.split(|c: char| !c.is_ascii_alphabetic()).next().unwrap_or("");
    if ["month", "months", "mo", "mos", "week", "weeks", "day", "days"].contains(&unit) {
        return Some(0);
    }
    (age <= 120).then_some(age)
}

/// Minutes, as stated. A range gives its upper end ("15 to 20 minutes" is
/// 20), hours are converted ("1 hour 10 minutes" is 70), and a clock time
/// ("since 06:12") is not a duration. The upper end because the screen
/// asks whether the time is short enough: a range is judged at its worst.
pub fn parse_minutes(v: &str) -> Option<u32> {
    let l = v.to_ascii_lowercase();
    if l.contains("half an hour") || l.contains("half hour") {
        return Some(30);
    }
    // (is hours, index just past it) of the next unit word after `from`
    let unit_at = |from: usize| -> Option<(bool, usize)> {
        let mut off = from;
        for piece in l[from..].split_inclusive(|c: char| !c.is_ascii_alphanumeric()) {
            let word = piece.trim_end_matches(|c: char| !c.is_ascii_alphanumeric());
            let end = off + word.len();
            off += piece.len();
            if ["hour", "hours", "hr", "hrs", "h"].contains(&word) {
                return Some((true, end));
            }
            if ["minute", "minutes", "min", "mins", "m"].contains(&word) {
                return Some((false, end));
            }
        }
        None
    };
    let nums: Vec<_> = numbers(&l)
        .into_iter()
        .filter(|(n, s, e)| {
            // a clock time or a four-digit time of day is not a duration
            let clock = l[..*s].ends_with(':') || l[*e..].starts_with(':') || (*e - *s == 4 && *n >= 100);
            !clock
        })
        .collect();
    if nums.is_empty() {
        return (l.contains("an hour") || l.contains("one hour")).then_some(60);
    }
    let mut best: Option<u32> = None;
    let mut i = 0;
    while i < nums.len() {
        let (n, _, e) = nums[i];
        let (hours, past) = unit_at(e).unwrap_or((false, e));
        let mut mins = if hours { n * 60 } else { n };
        // "1 hour 10 minutes": an hour count followed by a minute count
        if hours {
            if let Some(&(m, s2, e2)) = nums.get(i + 1) {
                if s2 >= past && !unit_at(e2).map_or(true, |(h, _)| h) {
                    mins += m;
                    i += 1;
                }
            }
        }
        best = Some(best.map_or(mins, |b: u32| b.max(mins)));
        i += 1;
    }
    best.filter(|m| *m <= 24 * 60)
}

/// Yes or no, from how a report or a reviewer put it. `None` when it says
/// neither.
pub fn parse_yes_no(v: &str) -> Option<bool> {
    let l = v.trim().to_ascii_lowercase();
    let first = l.split(|c: char| !c.is_ascii_alphanumeric()).find(|w| !w.is_empty()).unwrap_or("");
    if ["no", "not", "none", "negative", "never", "n", "false", "0", "unwitnessed", "without"].contains(&first)
        || ["unwitnessed", "not witnessed", "no bystander", "no cpr", "no rosc", "no pulse", "remains in arrest", "still in arrest"]
            .iter()
            .any(|p| l.contains(p))
    {
        return Some(false);
    }
    if ["yes", "y", "true", "1", "positive", "affirmative"].contains(&first)
        || ["witnessed", "bystander", "cpr", "compressions", "rosc", "pulses back", "return of", "got pulses", "pulse back"]
            .iter()
            .any(|p| l.contains(p))
    {
        return Some(true);
    }
    None
}

/// History that names an end-stage disease, the screen's criterion 4.
pub fn end_stage(v: &str) -> bool {
    let l = v.to_ascii_lowercase();
    ["end stage", "end-stage", "esrd", "dialysis", "hospice", "dnr", "metastatic", "terminal", "comfort care"]
        .iter()
        .any(|p| l.contains(p))
}

fn norm_sex(v: &str) -> Option<&'static str> {
    let l = v.trim().to_ascii_lowercase();
    let w = l.split(|c: char| !c.is_ascii_alphabetic()).find(|w| !w.is_empty()).unwrap_or("");
    match w {
        "m" | "male" | "man" | "boy" | "gentleman" => Some("male"),
        "f" | "female" | "woman" | "girl" | "lady" => Some("female"),
        _ => None,
    }
}

fn norm_rhythm(v: &str) -> Option<&'static str> {
    let l = v.to_ascii_lowercase();
    let w: Vec<&str> = l.split(|c: char| !c.is_ascii_alphanumeric()).filter(|x| !x.is_empty()).collect();
    let joined = w.join(" ");
    let any = |words: &[&str], phrases: &[&str]| words.iter().any(|x| w.contains(x)) || phrases.iter().any(|x| joined.contains(x));
    if any(&["vf", "vfib"], &["v fib", "ventricular fib"]) {
        Some("VF")
    } else if any(&["vt", "vtach", "pvt"], &["v tach", "ventricular tach"]) {
        Some("VT")
    } else if any(&["asystole", "asystolic", "flatline"], &["flat line"]) {
        Some("asystole")
    } else if any(&["pea"], &["pulseless electrical"]) {
        Some("PEA")
    } else if any(&["nonshockable"], &["non shockable", "not shockable"]) {
        Some("non-shockable")
    } else if any(&["shockable"], &[]) {
        Some("shockable")
    } else {
        None
    }
}

/// The variables a reviewer abstracts, as fact keys; `arrest` is whether the
/// traffic describes an active cardiac arrest at all.
pub const VARIABLES: &[&str] = &["arrest", "age", "sex", "witnessed", "bystander cpr", "rhythm", "rosc", "downtime", "history"];

/// Variables compared as free text: agreement is left to a person.
const FREE_TEXT: &[&str] = &["history"];

/// A value in the form two readings of it can be compared in: "" for not
/// stated, a number for age and minutes, a category for the rest. A value
/// that does not read as its kind is kept as lower-case text, so it can
/// still agree with the same words.
pub fn normalise(key: &str, v: &str) -> String {
    if is_unstated(v) {
        return String::new();
    }
    let text = || v.split_whitespace().collect::<Vec<_>>().join(" ").to_ascii_lowercase();
    match key {
        "age" => parse_age(v).map(|a| a.to_string()).unwrap_or_else(text),
        "downtime" => parse_minutes(v).map(|m| m.to_string()).unwrap_or_else(text),
        "sex" => norm_sex(v).map(String::from).unwrap_or_else(text),
        "rhythm" => norm_rhythm(v).map(String::from).unwrap_or_else(text),
        "arrest" | "witnessed" | "bystander cpr" | "rosc" => match parse_yes_no(v) {
            Some(true) => "yes".into(),
            Some(false) => "no".into(),
            None => text(),
        },
        _ => text(),
    }
}

fn column(key: &str) -> String {
    key.replace(' ', "_")
}

// ---------------------------------------------------------------------------
// word error rate
// ---------------------------------------------------------------------------

/// Words as the error rate counts them: lower case, punctuation dropped,
/// apostrophes kept inside a word.
pub fn words(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .map(|w| w.trim_matches('\'').to_string())
        .filter(|w| !w.is_empty())
        .collect()
}

#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub struct Wer {
    pub words: usize,
    pub substitutions: usize,
    pub deletions: usize,
    pub insertions: usize,
}

impl Wer {
    pub fn errors(&self) -> usize {
        self.substitutions + self.deletions + self.insertions
    }
    pub fn rate(&self) -> Option<f64> {
        (self.words > 0).then(|| self.errors() as f64 / self.words as f64)
    }
}

/// Word error rate of `hyp` against the reference `r`: the fewest
/// substitutions, deletions and insertions that turn one into the other,
/// over the reference's word count.
pub fn wer(r: &str, hyp: &str) -> Wer {
    let (r, h) = (words(r), words(hyp));
    let (n, m) = (r.len(), h.len());
    // d[i][j] = (cost, subs, dels, ins) turning r[..i] into h[..j]
    let mut d = vec![vec![(0usize, 0usize, 0usize, 0usize); m + 1]; n + 1];
    for i in 1..=n {
        d[i][0] = (i, 0, i, 0);
    }
    for j in 1..=m {
        d[0][j] = (j, 0, 0, j);
    }
    for i in 1..=n {
        for j in 1..=m {
            let same = r[i - 1] == h[j - 1];
            let (c, s, dl, ins) = d[i - 1][j - 1];
            let mut best = if same { (c, s, dl, ins) } else { (c + 1, s + 1, dl, ins) };
            let (c, s, dl, ins) = d[i - 1][j];
            if c + 1 < best.0 {
                best = (c + 1, s, dl + 1, ins);
            }
            let (c, s, dl, ins) = d[i][j - 1];
            if c + 1 < best.0 {
                best = (c + 1, s, dl, ins + 1);
            }
            d[i][j] = best;
        }
    }
    let (_, substitutions, deletions, insertions) = d[n][m];
    Wer { words: n, substitutions, deletions, insertions }
}

// ---------------------------------------------------------------------------
// agreement
// ---------------------------------------------------------------------------

/// Gwet's AC1 for two raters over nominal categories, with the raw
/// agreement. `None` for AC1 when fewer than two categories were used, where
/// chance agreement is total and the coefficient is undefined.
pub fn gwet_ac1(pairs: &[(String, String)]) -> (f64, Option<f64>) {
    if pairs.is_empty() {
        return (0.0, None);
    }
    let n = pairs.len() as f64;
    let pa = pairs.iter().filter(|(a, b)| a == b).count() as f64 / n;
    let mut share: BTreeMap<&str, f64> = BTreeMap::new();
    for (a, b) in pairs {
        *share.entry(a).or_default() += 0.5 / n;
        *share.entry(b).or_default() += 0.5 / n;
    }
    let q = share.len();
    if q < 2 {
        return (pa, None);
    }
    let pe = share.values().map(|p| p * (1.0 - p)).sum::<f64>() / (q - 1) as f64;
    (pa, (pe < 1.0).then(|| (pa - pe) / (1.0 - pe)))
}

// ---------------------------------------------------------------------------
// the reference tables
// ---------------------------------------------------------------------------

/// The reviewer whose values settle a disagreement.
pub const ADJUDICATED: &str = "adjudicated";

pub fn ensure_schema(c: &Connection) {
    let _ = c.execute_batch(
        "CREATE TABLE IF NOT EXISTS reference_values (
            profile TEXT NOT NULL,
            incident INTEGER NOT NULL,
            reviewer TEXT NOT NULL,
            key TEXT NOT NULL,
            value TEXT NOT NULL,
            updated INTEGER NOT NULL,
            PRIMARY KEY (profile, incident, reviewer, key)
         );
         CREATE TABLE IF NOT EXISTS reference_transcripts (
            call INTEGER NOT NULL,
            reviewer TEXT NOT NULL,
            text TEXT NOT NULL,
            updated INTEGER NOT NULL,
            PRIMARY KEY (call, reviewer)
         );",
    );
}

/// Every reviewer's values for one case: reviewer → key → value.
fn reference_for(c: &Connection, profile: &str, incident: i64) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    if let Ok(mut q) = c.prepare("SELECT reviewer, key, value FROM reference_values WHERE profile = ?1 AND incident = ?2") {
        if let Ok(rows) = q.query_map(params![profile, incident], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))) {
            for (who, k, v) in rows.flatten() {
                out.entry(who).or_default().insert(k, v);
            }
        }
    }
    out
}

/// The reference value for one variable: the adjudicator's, else what every
/// reviewer agreed on, else the one reviewer's. The second part says which.
pub fn settle(key: &str, by: &BTreeMap<String, BTreeMap<String, String>>) -> (Option<String>, &'static str) {
    if let Some(v) = by.get(ADJUDICATED).and_then(|m| m.get(key)) {
        return (Some(v.clone()), "adjudicated");
    }
    let said: Vec<&String> = by.iter().filter(|(who, _)| *who != ADJUDICATED).filter_map(|(_, m)| m.get(key)).collect();
    match said.len() {
        0 => (None, "not reviewed"),
        1 => (Some(said[0].clone()), "one reviewer"),
        _ => {
            let first = normalise(key, said[0]);
            if said.iter().all(|v| normalise(key, v) == first) {
                (Some(said[0].clone()), "reviewers agree")
            } else {
                (None, "needs adjudication")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// CSV in and out
// ---------------------------------------------------------------------------

fn field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn line(f: &[String]) -> String {
    f.iter().map(|x| field(x)).collect::<Vec<_>>().join(",") + "\n"
}

/// Parse CSV as a spreadsheet writes it: quoted fields may hold commas,
/// quotes (doubled) and newlines. A byte-order mark is dropped.
pub fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let text = text.trim_start_matches('\u{feff}');
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if quoted {
            match ch {
                '"' if chars.peek() == Some(&'"') => {
                    cur.push('"');
                    chars.next();
                }
                '"' => quoted = false,
                c => cur.push(c),
            }
            continue;
        }
        match ch {
            '"' => quoted = true,
            ',' => row.push(std::mem::take(&mut cur)),
            '\r' => {}
            '\n' => {
                row.push(std::mem::take(&mut cur));
                rows.push(std::mem::take(&mut row));
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() || !row.is_empty() {
        row.push(cur);
        rows.push(row);
    }
    rows.retain(|r| r.iter().any(|f| !f.trim().is_empty()));
    rows
}

fn out_dir(stem: &str) -> Result<std::path::PathBuf, String> {
    let base = std::path::PathBuf::from(crate::shellexpand_home("~/Downloads"));
    let stem = format!("{stem}-{}", crate::library::local_fmt(crate::library::now(), "%Y%m%d-%H%M"));
    let mut dir = base.join(&stem);
    let mut n = 2;
    while dir.exists() {
        dir = base.join(format!("{stem}-{n}"));
        n += 1;
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

fn write(dir: &std::path::Path, name: &str, text: &str) -> Result<(), String> {
    let p = dir.join(name);
    std::fs::write(&p, text).map_err(|e| format!("{}: {e}", p.display()))
}

struct CallInfo {
    id: i64,
    start: i64,
    secs: f64,
    tg: i64,
    tg_name: String,
    audio: Option<String>,
    asr: String,
    edited: Option<String>,
}

fn calls_of(c: &Connection, ids: &[i64]) -> Vec<CallInfo> {
    if ids.is_empty() {
        return Vec::new();
    }
    let list = ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
    c.prepare(&format!(
        "SELECT id, start, secs, tg, COALESCE(tg_name, ''), audio, COALESCE(transcript, ''), transcript_edited
           FROM calls WHERE id IN ({list}) ORDER BY start, id"
    ))
    .and_then(|mut q| {
        q.query_map([], |r| {
            Ok(CallInfo {
                id: r.get(0)?,
                start: r.get(1)?,
                secs: r.get(2)?,
                tg: r.get(3)?,
                tg_name: r.get(4)?,
                audio: r.get(5)?,
                asr: r.get(6)?,
                edited: r.get(7)?,
            })
        })
        .map(|rows| rows.flatten().collect())
    })
    .unwrap_or_default()
}

const README: &str = "Reference-standard review packet
================================

Work from the audio only. This packet holds nothing the app made of these
calls: no transcripts, no extracted values, no score.

abstraction.csv — one row per case. Make one copy per reviewer.
  reviewer     your initials (required on every row you fill). The person
               settling disagreements writes \"adjudicated\" here, and fills
               only the variables in dispute.
  arrest       yes / no: does the traffic describe an active cardiac arrest?
  age          years, as a number
  sex          male / female
  witnessed    yes / no
  bystander_cpr yes / no
  rhythm       as said (VF, VT, asystole, PEA, shockable …)
  rosc         yes / no
  downtime     minutes, as the crew stated it
  history      as said, free text
  comment      anything else

  Write \"not stated\" when the audio does not say it. A BLANK cell means
  \"not reviewed\" and is ignored on import, so do not leave a stated-or-not
  judgement blank.

transcripts.csv — one row per call. Type what is said in reference_transcript,
word for word, numbers as digits (\"Medic 32\", not \"Medic thirty-two\"), and
put your initials in reviewer. Leave a row blank to skip it.

audio/ — one folder per case, the calls in the order they were heard. A call
whose audio the library no longer keeps is listed with \"no audio kept\".

Bring the filled sheets back through Research → Import sheet.
";

/// Write a blinded review packet for these cases to ~/Downloads and say where.
pub fn packet(c: &Connection, rows: &[CaseRow]) -> Result<String, String> {
    packet_into(c, rows, &out_dir("study-packet")?)
}

fn packet_into(c: &Connection, rows: &[CaseRow], dir: &std::path::Path) -> Result<String, String> {
    let audio_dir = dir.join("audio");
    let mut sheet = line(
        &["profile", "case", "first_heard", "audio_folder", "reviewer"]
            .iter()
            .map(|s| s.to_string())
            .chain(VARIABLES.iter().map(|k| column(k)))
            .chain(["comment".to_string()])
            .collect::<Vec<_>>(),
    );
    let mut tx = line(&["profile", "case", "call", "audio_file", "heard", "secs", "talkgroup", "reviewer", "reference_transcript"].map(String::from));
    let (mut copied, mut missing) = (0usize, 0usize);
    for r in rows {
        let calls = calls_of(c, &r.calls);
        let folder = format!("case-{}", r.incident);
        let first = calls.first().map(|k| k.start).or(r.dispatched);
        let mut f = vec![r.profile.clone(), r.incident.to_string(), first.map(|t| crate::library::local_fmt(t, "%Y-%m-%d %H:%M:%S")).unwrap_or_default(), format!("audio/{folder}"), String::new()];
        f.extend(VARIABLES.iter().map(|_| String::new()));
        f.push(String::new());
        sheet.push_str(&line(&f));
        for (i, k) in calls.iter().enumerate() {
            let src = k.audio.as_deref().map(crate::shellexpand_home).map(std::path::PathBuf::from).filter(|p| p.exists());
            let file = match src {
                Some(p) => {
                    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("wav");
                    let name = format!("{:02}-{}-tg{}-call{}.{ext}", i + 1, crate::library::local_fmt(k.start, "%H%M%S"), k.tg, k.id);
                    let dest = audio_dir.join(&folder);
                    std::fs::create_dir_all(&dest).map_err(|e| format!("{}: {e}", dest.display()))?;
                    std::fs::copy(&p, dest.join(&name)).map_err(|e| format!("{}: {e}", p.display()))?;
                    copied += 1;
                    format!("audio/{folder}/{name}")
                }
                None => {
                    missing += 1;
                    "no audio kept".to_string()
                }
            };
            tx.push_str(&line(&[
                r.profile.clone(),
                r.incident.to_string(),
                k.id.to_string(),
                file,
                crate::library::local_fmt(k.start, "%Y-%m-%d %H:%M:%S"),
                format!("{:.1}", k.secs),
                if k.tg_name.is_empty() { k.tg.to_string() } else { format!("{} ({})", k.tg_name, k.tg) },
                String::new(),
                String::new(),
            ]));
        }
    }
    write(dir, "abstraction.csv", &sheet)?;
    write(dir, "transcripts.csv", &tx)?;
    write(dir, "README.txt", README)?;
    let mut said = format!("{} cases, {copied} recordings", rows.len());
    if missing > 0 {
        said.push_str(&format!(" ({missing} calls no longer have audio)"));
    }
    Ok(format!("{said} → {}", dir.display()))
}

/// Take a filled sheet back in. Which sheet it is comes from its header.
/// Only filled cells are kept; a row with values and no reviewer is refused
/// whole, because a value nobody owns cannot be adjudicated.
pub fn import(c: &Connection, text: &str, now: i64) -> Result<String, String> {
    let rows = parse_csv(text);
    let Some((head, body)) = rows.split_first() else {
        return Err("the sheet is empty".into());
    };
    let head: Vec<String> = head.iter().map(|h| h.trim().to_ascii_lowercase()).collect();
    let at = |name: &str| head.iter().position(|h| h == name);
    let reviewer = at("reviewer").ok_or("the sheet has no reviewer column")?;
    let get = |r: &Vec<String>, i: usize| r.get(i).map(|s| s.trim().to_string()).unwrap_or_default();
    let mut who: BTreeSet<String> = BTreeSet::new();
    let mut unowned = 0usize;
    if let Some(text_col) = at("reference_transcript") {
        let call = at("call").ok_or("the transcript sheet has no call column")?;
        let mut kept = 0usize;
        for r in body {
            let t = get(r, text_col);
            if t.is_empty() {
                continue;
            }
            let w = get(r, reviewer);
            let Ok(id) = get(r, call).parse::<i64>() else { continue };
            if w.is_empty() {
                unowned += 1;
                continue;
            }
            c.execute(
                "INSERT INTO reference_transcripts (call, reviewer, text, updated) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(call, reviewer) DO UPDATE SET text = excluded.text, updated = excluded.updated",
                params![id, w, t, now],
            )
            .map_err(|e| format!("reference transcript: {e}"))?;
            who.insert(w);
            kept += 1;
        }
        let mut said = format!("Kept {kept} reference transcripts from {}", names(&who));
        if unowned > 0 {
            said.push_str(&format!("; {unowned} rows had no reviewer and were skipped"));
        }
        return Ok(said);
    }
    let case = at("case").ok_or("the sheet has neither a case nor a call column")?;
    let profile = at("profile");
    let cols: Vec<(&str, usize)> = VARIABLES.iter().filter_map(|k| at(&column(k)).map(|i| (*k, i))).collect();
    if cols.is_empty() {
        return Err("the sheet has none of the variables (age, witnessed …) as columns".into());
    }
    let (mut cases, mut values) = (0usize, 0usize);
    for r in body {
        let filled: Vec<(&str, String)> = cols.iter().map(|(k, i)| (*k, get(r, *i))).filter(|(_, v)| !v.is_empty()).collect();
        if filled.is_empty() {
            continue;
        }
        let w = get(r, reviewer);
        let Ok(incident) = get(r, case).parse::<i64>() else { continue };
        if w.is_empty() {
            unowned += 1;
            continue;
        }
        let p = profile.map(|i| get(r, i)).filter(|p| !p.is_empty()).unwrap_or_else(|| "cardiac-arrest".into());
        for (k, v) in &filled {
            c.execute(
                "INSERT INTO reference_values (profile, incident, reviewer, key, value, updated) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(profile, incident, reviewer, key) DO UPDATE SET value = excluded.value, updated = excluded.updated",
                params![p, incident, w, k, v, now],
            )
            .map_err(|e| format!("reference value: {e}"))?;
        }
        who.insert(w);
        cases += 1;
        values += filled.len();
    }
    let mut said = format!("Kept {values} values over {cases} cases from {}", names(&who));
    if unowned > 0 {
        said.push_str(&format!("; {unowned} rows had no reviewer and were skipped"));
    }
    Ok(said)
}

fn names(who: &BTreeSet<String>) -> String {
    if who.is_empty() {
        "nobody".into()
    } else {
        who.iter().cloned().collect::<Vec<_>>().join(", ")
    }
}

/// What the app said for a variable of a case: the report's fact, or for
/// `arrest` whether the case still stands as one.
fn ai_value(r: &CaseRow, key: &str) -> String {
    if key == "arrest" {
        return if r.downgraded.is_some() { "no".into() } else { "yes".into() };
    }
    r.fact_values.get(key).cloned().unwrap_or_default()
}

fn pct(n: usize, of: usize) -> String {
    if of == 0 {
        String::new()
    } else {
        format!("{:.1}", n as f64 * 100.0 / of as f64)
    }
}

/// Write the app's values beside the reference, per variable and per call,
/// with a summary sheet, to ~/Downloads and say where.
pub fn compare(c: &Connection, rows: &[CaseRow]) -> Result<String, String> {
    compare_into(c, rows, &out_dir("study-compare")?)
}

fn compare_into(c: &Connection, rows: &[CaseRow], dir: &std::path::Path) -> Result<String, String> {
    let mut vars = line(
        &["profile", "case", "variable", "ai_value", "ai_normalised", "reviewers", "reference", "reference_normalised", "reference_how", "ai_stated", "reference_stated", "agree", "error_class"]
            .map(String::from),
    );
    // per variable: (tp, fp, fn, tn, both stated, agreeing) and rater pairs
    let mut tally: BTreeMap<&str, [usize; 6]> = BTreeMap::new();
    let mut raters: BTreeMap<&str, Vec<(String, String)>> = BTreeMap::new();
    let mut reviewed = 0usize;
    for r in rows {
        let by = reference_for(c, &r.profile, r.incident);
        if by.is_empty() {
            continue;
        }
        reviewed += 1;
        let mut settled: BTreeMap<String, String> = BTreeMap::new();
        for key in VARIABLES {
            let ai = ai_value(r, key);
            let (reference, how) = settle(key, &by);
            let (an, rn) = (normalise(key, &ai), reference.as_deref().map(|v| normalise(key, v)));
            let listed = by.iter().filter_map(|(w, m)| m.get(*key).map(|v| format!("{w}={v}"))).collect::<Vec<_>>().join("; ");
            let agree = match &rn {
                Some(rn) if FREE_TEXT.contains(key) && !(an.is_empty() && rn.is_empty()) => String::new(),
                Some(rn) => ((an == *rn) as u8).to_string(),
                None => String::new(),
            };
            if let Some(rn) = &rn {
                let t = tally.entry(key).or_default();
                match (!an.is_empty(), !rn.is_empty()) {
                    (true, true) => {
                        t[0] += 1;
                        t[4] += 1;
                        if an == *rn {
                            t[5] += 1;
                        }
                    }
                    (true, false) => t[1] += 1,
                    (false, true) => t[2] += 1,
                    (false, false) => t[3] += 1,
                }
                if let Some(v) = &reference {
                    settled.insert(key.to_string(), v.clone());
                }
            }
            // the first two reviewers, before adjudication, for reliability
            let pair: Vec<String> = by.iter().filter(|(w, _)| *w != ADJUDICATED).filter_map(|(_, m)| m.get(*key)).take(2).map(|v| normalise(key, v)).collect();
            if pair.len() == 2 && !FREE_TEXT.contains(key) {
                raters.entry(key).or_default().push((pair[0].clone(), pair[1].clone()));
            }
            vars.push_str(&line(&[
                r.profile.clone(),
                r.incident.to_string(),
                column(key),
                ai.clone(),
                an.clone(),
                listed,
                reference.clone().unwrap_or_default(),
                rn.clone().unwrap_or_default(),
                how.to_string(),
                (!an.is_empty() as u8).to_string(),
                rn.as_ref().map(|v| (!v.is_empty() as u8).to_string()).unwrap_or_default(),
                agree,
                String::new(),
            ]));
        }
        // The score, from the app's facts and from the reference's, by the
        // same arithmetic: any difference is the extraction's, not the sum's.
        let ai_score = score(&r.fact_values);
        let complete_ref = VARIABLES.iter().filter(|k| **k != "arrest").all(|k| settled.contains_key(*k));
        let ref_facts: BTreeMap<String, String> = settled.into_iter().filter(|(k, v)| k != "arrest" && !is_unstated(v)).collect();
        let ref_score = complete_ref.then(|| score(&ref_facts));
        for (name, a, b) in [
            ("score_estimate", ai_score.estimate.clone(), ref_score.as_ref().map(|s| s.estimate.clone())),
            ("score_criteria_met", ai_score.met.to_string(), ref_score.as_ref().map(|s| s.met.to_string())),
        ] {
            let agree = b.as_ref().map(|b| ((a == *b) as u8).to_string()).unwrap_or_default();
            vars.push_str(&line(&[
                r.profile.clone(),
                r.incident.to_string(),
                name.to_string(),
                a.clone(),
                a,
                String::new(),
                b.clone().unwrap_or_default(),
                b.unwrap_or_default(),
                if complete_ref { "from the reference values".into() } else { "reference incomplete".into() },
                String::new(),
                String::new(),
                agree,
                String::new(),
            ]));
        }
    }
    write(dir, "variables.csv", &vars)?;

    // transcripts
    let mut tx = line(&["profile", "case", "call", "reviewer", "asr", "pipeline_text", "hand_edited", "reference", "reference_words", "substitutions", "deletions", "insertions", "wer"].map(String::from));
    let mut total = Wer::default();
    let mut scored_calls = 0usize;
    for r in rows {
        for k in calls_of(c, &r.calls) {
            let refs: Vec<(String, String)> = c
                .prepare("SELECT reviewer, text FROM reference_transcripts WHERE call = ?1 ORDER BY reviewer")
                .and_then(|mut q| q.query_map([k.id], |x| Ok((x.get(0)?, x.get(1)?))).map(|rows| rows.flatten().collect()))
                .unwrap_or_default();
            let edited = k.edited.as_deref().filter(|e| !e.trim().is_empty());
            for (who, text) in refs {
                // Against what the recogniser wrote, never the hand edit.
                let w = wer(&text, &k.asr);
                total.words += w.words;
                total.substitutions += w.substitutions;
                total.deletions += w.deletions;
                total.insertions += w.insertions;
                scored_calls += 1;
                tx.push_str(&line(&[
                    r.profile.clone(),
                    r.incident.to_string(),
                    k.id.to_string(),
                    who,
                    k.asr.clone(),
                    edited.unwrap_or(&k.asr).to_string(),
                    (edited.is_some() as u8).to_string(),
                    text,
                    w.words.to_string(),
                    w.substitutions.to_string(),
                    w.deletions.to_string(),
                    w.insertions.to_string(),
                    w.rate().map(|x| format!("{x:.3}")).unwrap_or_default(),
                ]));
            }
        }
    }
    write(dir, "transcripts.csv", &tx)?;

    // summary
    let mut sum = line(
        &["variable", "cases_with_reference", "ai_stated_ref_stated", "ai_stated_ref_not", "ai_not_ref_stated", "neither_stated", "sensitivity_pct", "specificity_pct", "ppv_pct", "npv_pct", "value_agree_n", "value_agree_of", "value_agree_pct", "raters_n", "raters_agree_pct", "gwet_ac1"]
            .map(String::from),
    );
    for key in VARIABLES {
        let t = tally.get(key).copied().unwrap_or_default();
        let [tp, fp, fnn, tn, both, agree] = t;
        let (pa, ac1) = raters.get(key).map(|p| gwet_ac1(p)).unwrap_or((0.0, None));
        let rn = raters.get(key).map_or(0, Vec::len);
        sum.push_str(&line(&[
            column(key),
            (tp + fp + fnn + tn).to_string(),
            tp.to_string(),
            fp.to_string(),
            fnn.to_string(),
            tn.to_string(),
            pct(tp, tp + fnn),
            pct(tn, tn + fp),
            pct(tp, tp + fp),
            pct(tn, tn + fnn),
            if FREE_TEXT.contains(key) { String::new() } else { agree.to_string() },
            if FREE_TEXT.contains(key) { String::new() } else { both.to_string() },
            if FREE_TEXT.contains(key) { String::new() } else { pct(agree, both) },
            rn.to_string(),
            if rn > 0 { format!("{:.1}", pa * 100.0) } else { String::new() },
            ac1.map(|x| format!("{x:.3}")).unwrap_or_default(),
        ]));
    }
    sum.push_str(&line(&[
        "transcripts (all calls)".into(),
        scored_calls.to_string(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        total.rate().map(|x| format!("WER {x:.3} over {} words", total.words)).unwrap_or_default(),
    ]));
    write(dir, "summary.csv", &sum)?;
    write(dir, "README.txt", COMPARE_README)?;
    Ok(format!("{reviewed} reviewed cases, {scored_calls} reference transcripts → {}", dir.display()))
}

const COMPARE_README: &str = "App output against the reference standard
==========================================

variables.csv — one row per case and variable.
  ai_value / ai_normalised     what the app's report stated, as stated and as compared
  reviewers                    every reviewer's value, before adjudication
  reference / reference_how    the adjudicator's value; else the reviewers' when they
                               agree; else a lone reviewer's; else blank with
                               \"needs adjudication\"
  ai_stated / reference_stated 1 when a value was stated, 0 for \"not stated\"
  agree                        normalised values equal (blank for free text: judge by hand)
  error_class                  for hand coding: transcription / extraction /
                               calculator / unsupported
  score_estimate and score_criteria_met rows run the same score over the app's
  facts and over the reference values; a difference is the extraction's.

  Normalising: ages and minutes compare as numbers (a range as its upper end),
  sex and rhythm as categories, yes/no as yes/no.

transcripts.csv — one row per call and reference transcript. The word error rate
is the recogniser's own output (asr) against the reference, never the hand-edited
text; pipeline_text is what the app actually read, and hand_edited says whether
that was a correction.

summary.csv — per variable: stated/not-stated as a detection (reference = truth),
exact agreement where both stated a value, and agreement between the first two
reviewers before adjudication (raw and Gwet's AC1). Continuous agreement
(ICC, Bland-Altman) is for the statistician, from variables.csv.
";

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

fn db(state: &AppState) -> Result<std::sync::Arc<std::sync::Mutex<Connection>>, String> {
    state.db.lock().unwrap().clone().ok_or_else(|| "the call library is not open".to_string())
}

/// A blinded review packet for the cases the page is showing.
#[tauri::command]
pub fn study_packet(state: State<AppState>, rows: Vec<CaseRow>) -> Result<String, String> {
    let db = db(&state)?;
    let c = db.lock().unwrap();
    packet(&c, &rows)
}

/// A filled abstraction or transcript sheet, as its text.
#[tauri::command]
pub fn study_import(state: State<AppState>, text: String) -> Result<String, String> {
    let db = db(&state)?;
    let c = db.lock().unwrap();
    import(&c, &text, crate::library::now())
}

/// The app against the reference, for the cases the page is showing.
#[tauri::command]
pub fn study_compare(state: State<AppState>, mut rows: Vec<CaseRow>) -> Result<String, String> {
    rows.iter_mut().for_each(CaseRow::derive);
    let db = db(&state)?;
    let c = db.lock().unwrap();
    compare(&c, &rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(kv: &[(&str, &str)]) -> BTreeMap<String, String> {
        kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn every_criterion_stated_gives_one_number() {
        let s = score(&facts(&[("age", "52"), ("downtime", "20 minutes"), ("witnessed", "yes"), ("bystander cpr", "yes"), ("history", "hypertension")]));
        assert_eq!((s.met, s.unknown, s.complete), (4, 0, true));
        assert_eq!(s.estimate, "46%");
        assert_eq!(s.criteria[0].why, "52 + 20 min stated = 72");
        let s = score(&facts(&[("age", "52"), ("downtime", "20 minutes"), ("witnessed", "unwitnessed"), ("bystander cpr", "yes"), ("history", "none")]));
        assert_eq!(s.estimate, "12%");
    }

    #[test]
    fn what_was_not_said_widens_the_estimate_and_is_never_filled_in() {
        // Age and downtime missing: the time criterion is unknown, the
        // estimate runs from three criteria met to four.
        let s = score(&facts(&[("witnessed", "yes"), ("bystander cpr", "yes"), ("history", "diabetes")]));
        assert_eq!((s.met, s.unknown, s.complete), (3, 1, false));
        assert_eq!(s.estimate, "12–46% (1 of 4 not stated)");
        assert_eq!((s.lo_pct, s.hi_pct), (Some(12.0), Some(46.0)));
        // Nothing at all: history is assumed per the screen, the rest unknown.
        let s = score(&BTreeMap::new());
        assert_eq!((s.met, s.unknown, s.assumed), (1, 3, 1));
        assert_eq!(s.estimate, "0–46% (3 of 4 not stated)");
        // "not stated" written out is the same as absent.
        let s = score(&facts(&[("witnessed", "not stated")]));
        assert_eq!(s.criteria[1].verdict, "unknown");
    }

    #[test]
    fn a_hard_exclusion_gives_no_estimate() {
        let s = score(&facts(&[("age", "71"), ("witnessed", "yes")]));
        assert_eq!(s.estimate, "excluded: age 71 is over 65");
        assert_eq!(s.lo_pct, None);
        let s = score(&facts(&[("age", "40"), ("downtime", "70 min"), ("witnessed", "yes"), ("bystander cpr", "yes"), ("history", "ESRD on dialysis")]));
        assert_eq!(s.criteria[0].verdict, "not met");
        assert_eq!(s.criteria[3].verdict, "not met");
        assert_eq!(s.estimate, "0–5%");
    }

    #[test]
    fn ages_read_only_when_they_give_one_number() {
        assert_eq!(parse_age("62"), Some(62));
        assert_eq!(parse_age("62-year-old male"), Some(62));
        assert_eq!(parse_age("approximately 45 yo"), Some(45));
        assert_eq!(parse_age("60s"), None);
        assert_eq!(parse_age("50 to 60"), None);
        assert_eq!(parse_age("elderly"), None);
        assert_eq!(parse_age("8 months"), Some(0));
    }

    #[test]
    fn minutes_read_as_the_worst_case() {
        assert_eq!(parse_minutes("about 20 minutes"), Some(20));
        assert_eq!(parse_minutes("15 to 20 minutes"), Some(20));
        assert_eq!(parse_minutes("15-20 min"), Some(20));
        assert_eq!(parse_minutes("1 hour 10 minutes"), Some(70));
        assert_eq!(parse_minutes("an hour"), Some(60));
        assert_eq!(parse_minutes("2 hours"), Some(120));
        assert_eq!(parse_minutes("half an hour"), Some(30));
        assert_eq!(parse_minutes("CPR since 06:12"), None);
        assert_eq!(parse_minutes("unknown"), None);
        assert_eq!(parse_minutes("25"), Some(25));
        assert_eq!(parse_minutes("20 minutes, three rounds of epi"), Some(20));
    }

    #[test]
    fn yes_and_no_as_crews_and_reviewers_say_them() {
        assert_eq!(parse_yes_no("yes"), Some(true));
        assert_eq!(parse_yes_no("Witnessed by family"), Some(true));
        assert_eq!(parse_yes_no("unwitnessed"), Some(false));
        assert_eq!(parse_yes_no("not witnessed"), Some(false));
        assert_eq!(parse_yes_no("No bystander CPR"), Some(false));
        assert_eq!(parse_yes_no("bystander CPR by spouse"), Some(true));
        assert_eq!(parse_yes_no("ROSC after 2 shocks"), Some(true));
        assert_eq!(parse_yes_no("no ROSC"), Some(false));
        assert_eq!(parse_yes_no("Not by bystanders"), Some(false));
        assert_eq!(parse_yes_no("maybe"), None);
    }

    #[test]
    fn values_compare_in_one_form() {
        assert_eq!(normalise("rhythm", "V-fib"), "VF");
        assert_eq!(normalise("rhythm", "initial rhythm ventricular fibrillation"), "VF");
        assert_eq!(normalise("rhythm", "asystole"), "asystole");
        assert_eq!(normalise("rhythm", "PEA"), "PEA");
        assert_eq!(normalise("rhythm", "patient speaking"), "patient speaking", "no PEA inside a word");
        assert_eq!(normalise("rhythm", "non-shockable"), "non-shockable");
        assert_eq!(normalise("sex", "M"), "male");
        assert_eq!(normalise("age", "62 yo"), "62");
        assert_eq!(normalise("downtime", "15-20 minutes"), "20");
        assert_eq!(normalise("witnessed", "not stated"), "");
        assert_eq!(normalise("history", "  HTN,  DM "), "htn, dm");
    }

    #[test]
    fn word_error_rate_counts_each_kind() {
        let w = wer("medic 32 inbound with a working arrest", "medic 32 inbound with working arrest now");
        assert_eq!((w.words, w.substitutions, w.deletions, w.insertions), (7, 0, 1, 1));
        assert!((w.rate().unwrap() - 2.0 / 7.0).abs() < 1e-9);
        let w = wer("v fib arrest", "v fib arrest");
        assert_eq!(w.errors(), 0);
        let w = wer("medic 32", "medic 33");
        assert_eq!(w.substitutions, 1);
        assert_eq!(wer("", "anything").rate(), None);
    }

    #[test]
    fn ac1_matches_a_worked_example() {
        // 10 pairs: 8 agree, categories split evenly: pa 0.8, pe 0.5, AC1 0.6.
        let mut p: Vec<(String, String)> = Vec::new();
        for _ in 0..4 {
            p.push(("yes".into(), "yes".into()));
            p.push(("no".into(), "no".into()));
        }
        p.push(("yes".into(), "no".into()));
        p.push(("no".into(), "yes".into()));
        let (pa, ac1) = gwet_ac1(&p);
        assert!((pa - 0.8).abs() < 1e-9);
        assert!((ac1.unwrap() - 0.6).abs() < 1e-9);
        assert_eq!(gwet_ac1(&[("yes".into(), "yes".into())]).1, None);
    }

    #[test]
    fn csv_reads_what_a_spreadsheet_writes() {
        let rows = parse_csv("\u{feff}a,b,c\r\n1,\"two, three\",\"say \"\"hi\"\"\"\r\n,,\r\n4,\"line\nbreak\",\n");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1], vec!["1", "two, three", "say \"hi\""]);
        assert_eq!(rows[2], vec!["4", "line\nbreak", ""]);
    }

    fn library() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        ensure_schema(&c);
        c
    }

    #[test]
    fn a_filled_sheet_lands_apart_and_blanks_are_not_answers() {
        let c = library();
        let sheet = "profile,case,first_heard,audio_folder,reviewer,arrest,age,sex,witnessed,bystander_cpr,rhythm,rosc,downtime,history,comment\n\
                     cardiac-arrest,11,,,AB,yes,62,male,yes,not stated,,,,,\n\
                     cardiac-arrest,12,,,,yes,50,,,,,,,,\n\
                     cardiac-arrest,13,,,AB,,,,,,,,,,\n";
        let said = import(&c, sheet, 5).unwrap();
        assert_eq!(said, "Kept 5 values over 1 cases from AB; 1 rows had no reviewer and were skipped");
        let by = reference_for(&c, "cardiac-arrest", 11);
        assert_eq!(by["AB"].get("bystander cpr").map(String::as_str), Some("not stated"));
        assert!(by["AB"].get("rhythm").is_none(), "a blank cell is not reviewed, not 'not stated'");
        // A transcript sheet is told apart by its header.
        let said = import(&c, "profile,case,call,audio_file,heard,secs,talkgroup,reviewer,reference_transcript\ncardiac-arrest,11,501,,,,,AB,medic 32 working arrest\n", 5).unwrap();
        assert_eq!(said, "Kept 1 reference transcripts from AB");
    }

    #[test]
    fn a_packet_goes_out_blind_and_comes_back_compared() {
        let c = library();
        c.execute_batch(
            "CREATE TABLE calls (id INTEGER PRIMARY KEY, start INTEGER, secs REAL, tg INTEGER, tg_name TEXT, audio TEXT,
               transcript TEXT, transcript_edited TEXT);",
        )
        .unwrap();
        let tmp = std::env::temp_dir().join(format!("hs-study-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let wav = tmp.join("src.wav");
        std::fs::write(&wav, b"RIFF").unwrap();
        c.execute(
            "INSERT INTO calls VALUES (501, 1700000000, 4.0, 9, 'MED 9', ?1, 'medic 32 working arrest witnessed', 'Medic 32, working arrest, witnessed'),
                                      (502, 1700000100, 3.0, 9, 'MED 9', '/nowhere/gone.wav', 'sixty two year old', NULL)",
            [wav.to_string_lossy()],
        )
        .unwrap();
        let mut r = CaseRow { profile: "cardiac-arrest".into(), incident: 11, report: Some(1700000000), calls: vec![501, 502], ..Default::default() };
        r.fact_values.insert("witnessed".into(), "yes".into());
        r.fact_values.insert("rhythm".into(), "V-fib".into());
        r.derive();

        let out = tmp.join("packet");
        std::fs::create_dir_all(&out).unwrap();
        let said = packet_into(&c, &[r.clone()], &out).unwrap();
        assert!(said.starts_with("1 cases, 1 recordings (1 calls no longer have audio)"), "{said}");
        let sheet = std::fs::read_to_string(out.join("abstraction.csv")).unwrap();
        let tx = std::fs::read_to_string(out.join("transcripts.csv")).unwrap();
        for text in [&sheet, &tx] {
            assert!(!text.contains("witnessed") || text.lines().next().unwrap().contains("witnessed"), "no app value in a packet");
            assert!(!text.to_lowercase().contains("medic 32") && !text.contains("V-fib"), "no transcript or fact in a packet");
        }
        assert!(tx.contains("no audio kept"));
        assert!(out.join("audio/case-11").read_dir().unwrap().count() == 1);

        // Two reviewers disagree on rhythm; an adjudicator settles it.
        import(&c, "profile,case,reviewer,arrest,age,witnessed,rhythm\ncardiac-arrest,11,A,yes,62,yes,VF\n", 1).unwrap();
        import(&c, "profile,case,reviewer,arrest,age,witnessed,rhythm\ncardiac-arrest,11,B,yes,62 yo,yes,asystole\n", 1).unwrap();
        import(&c, "profile,case,reviewer,rhythm\ncardiac-arrest,11,adjudicated,ventricular fibrillation\n", 1).unwrap();
        import(&c, "case,call,reviewer,reference_transcript\n11,501,A,Medic 32 working arrest witnessed by family\n", 1).unwrap();
        let cmp = tmp.join("compare");
        std::fs::create_dir_all(&cmp).unwrap();
        compare_into(&c, &[r], &cmp).unwrap();
        let vars = parse_csv(&std::fs::read_to_string(cmp.join("variables.csv")).unwrap());
        let head = &vars[0];
        let get = |var: &str, col: &str| {
            let row = vars.iter().find(|x| x[2] == var).unwrap();
            row[head.iter().position(|h| h == col).unwrap()].clone()
        };
        assert_eq!(get("rhythm", "reference_how"), "adjudicated");
        assert_eq!(get("rhythm", "agree"), "1", "V-fib and ventricular fibrillation are both VF");
        assert_eq!(get("age", "reference_how"), "reviewers agree");
        assert_eq!((get("age", "ai_stated"), get("age", "reference_stated")), ("0".into(), "1".into()), "a miss, not a disagreement");
        assert_eq!(get("witnessed", "agree"), "1");
        let tx = parse_csv(&std::fs::read_to_string(cmp.join("transcripts.csv")).unwrap());
        let wer_col = tx[0].iter().position(|h| h == "wer").unwrap();
        // Against the recogniser's words, not the hand edit: 2 missing of 7.
        assert_eq!(tx[1][wer_col], "0.286");
        let sum = parse_csv(&std::fs::read_to_string(cmp.join("summary.csv")).unwrap());
        let rhythm = sum.iter().find(|x| x[0] == "rhythm").unwrap();
        assert_eq!(rhythm[sum[0].iter().position(|h| h == "raters_agree_pct").unwrap()], "0.0");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn the_reference_is_the_adjudicators_else_what_reviewers_agree_on() {
        let mut by: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        by.entry("A".into()).or_default().insert("age".into(), "62".into());
        by.entry("B".into()).or_default().insert("age".into(), "62 yo".into());
        by.entry("A".into()).or_default().insert("rhythm".into(), "VF".into());
        by.entry("B".into()).or_default().insert("rhythm".into(), "asystole".into());
        assert_eq!(settle("age", &by), (Some("62".into()), "reviewers agree"));
        assert_eq!(settle("rhythm", &by), (None, "needs adjudication"));
        by.entry(ADJUDICATED.into()).or_default().insert("rhythm".into(), "VF".into());
        assert_eq!(settle("rhythm", &by), (Some("VF".into()), "adjudicated"));
        assert_eq!(settle("sex", &by), (None, "not reviewed"));
    }
}
