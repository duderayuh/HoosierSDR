//! The tripwire editor's live preview: run a draft tripwire over the calls
//! already in the library and show what it would have done — how often it
//! would have fired, how many messages that is after the quiet window, the
//! calls it would have caught, and which phrases are *dead* (never heard on
//! those channels) along with what the transcriber actually writes instead.
//!
//! Also: trying the AI check on a few of those calls before saving, and
//! drafting a tripwire from one call ("alert me when something like this
//! happens").
//!
//! Nothing here sends anything.

use rusqlite::{params_from_iter, Connection};
use serde::Serialize;
use std::collections::HashMap;
use tauri::{AppHandle, Manager};

use crate::alerts::CallFacts;
use crate::tripwires::{self, Tripwire};
use crate::AppState;

/// The vocabulary of what was said, for suggestions (`col` so talkgroup and
/// radio names stay out of it). Created with the library.
pub fn ensure_schema(c: &Connection) {
    let _ = c.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS calls_fts_vocab USING fts5vocab('calls_fts', 'col');",
    );
}

/// An FTS5 query for any of `phrases`, in the transcript columns only.
/// Quotes are doubled, so a phrase is always a phrase.
pub fn fts_any(phrases: &[String]) -> Option<String> {
    let parts: Vec<String> = phrases
        .iter()
        .map(|p| normalize(p))
        .filter(|p| !p.is_empty())
        .map(|p| format!("\"{}\"", p.replace('"', "\"\"")))
        .collect();
    (!parts.is_empty()).then(|| {
        format!(
            "{{transcript transcript_edited}} : ({})",
            parts.join(" OR ")
        )
    })
}

/// The same folding the matcher does: lower case, punctuation → spaces.
fn normalize(s: &str) -> String {
    let mut out = String::new();
    let mut space = true;
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            space = false;
        } else if !space {
            out.push(' ');
            space = true;
        }
    }
    out.trim().to_string()
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Suggestion {
    pub text: String,
    pub hits: u32,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct PhraseStat {
    pub phrase: String,
    /// Calls in the window, in the tripwire's scope, with this phrase.
    pub hits: u32,
    /// …on any talkgroup.
    pub anywhere: u32,
    /// For a phrase that is never heard: what is heard instead.
    pub suggestions: Vec<Suggestion>,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Sample {
    pub id: i64,
    pub start: i64,
    pub tg: u16,
    pub tg_name: String,
    pub unit_name: String,
    pub transcript: String,
    pub keywords: Vec<String>,
    pub audio: bool,
    pub emergency: bool,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Preview {
    pub days: u32,
    /// Calls in scope (talkgroups, radios, system) in the window.
    pub scanned: u32,
    pub transcribed: u32,
    /// Calls the tripwire would have fired on (before any AI check).
    pub matches: u32,
    /// Messages after the quiet window (conversations: exchanges; digests:
    /// runs).
    pub messages: u32,
    /// Calls a "but not" phrase kept out.
    pub excepted: u32,
    /// (day label, matches), oldest first.
    pub per_day: Vec<(String, u32)>,
    pub phrases: Vec<PhraseStat>,
    pub samples: Vec<Sample>,
    pub warnings: Vec<String>,
}

#[derive(Clone)]
struct Row {
    id: i64,
    start: i64,
    secs: f64,
    tg: u16,
    tg_name: String,
    unit: u32,
    unit_name: Option<String>,
    emergency: bool,
    system: String,
    audio: bool,
    text: Option<String>,
}

/// The `WHERE` for calls in the window on the tripwire's talkgroups,
/// radios, system and emergency flag — and, with `fts`, containing a phrase.
fn scope_where(
    t: &Tripwire,
    since: i64,
    fts: Option<&str>,
) -> (String, Vec<rusqlite::types::Value>) {
    let w = &t.when;
    let mut sql = String::from("start >= ?");
    let mut args: Vec<rusqlite::types::Value> = vec![since.into()];
    if !w.tgs.is_empty() {
        sql.push_str(&format!(
            " AND tg IN ({})",
            w.tgs
                .iter()
                .map(|t| t.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    if !w.units.is_empty() && w.kind == "call" {
        sql.push_str(&format!(
            " AND unit IN ({})",
            w.units
                .iter()
                .map(|u| u.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    if w.emergency && w.kind == "call" {
        sql.push_str(" AND emergency = 1");
    }
    if !w.system.is_empty() {
        sql.push_str(" AND (system = ? OR system = '')");
        args.push(w.system.clone().into());
    }
    if let Some(q) = fts {
        sql.push_str(" AND id IN (SELECT rowid FROM calls_fts WHERE calls_fts MATCH ?)");
        args.push(q.to_string().into());
    }
    (sql, args)
}

/// `with_text` decides whether the transcripts come along: a scan that only
/// counts (every call on a channel, a conversation's traffic) must not drag
/// tens of thousands of transcripts through the library lock.
fn scope_rows(
    c: &Connection,
    t: &Tripwire,
    since: i64,
    fts: Option<&str>,
    limit: u32,
    with_text: bool,
) -> Result<Vec<Row>, String> {
    let (wh, args) = scope_where(t, since, fts);
    let text_col = if with_text {
        "COALESCE(NULLIF(transcript_edited, ''), transcript)"
    } else {
        "NULL"
    };
    let sql = format!(
        "SELECT id, start, secs, tg, tg_name, unit, unit_name, emergency, system, audio IS NOT NULL,
                {text_col}
         FROM calls WHERE {wh} ORDER BY start ASC LIMIT {limit}"
    );
    let mut st = c.prepare(&sql).map_err(|e| format!("preview: {e}"))?;
    let rows = st
        .query_map(params_from_iter(args), |r| {
            Ok(Row {
                id: r.get(0)?,
                start: r.get(1)?,
                secs: r.get(2)?,
                tg: r.get::<_, i64>(3)? as u16,
                tg_name: r.get(4)?,
                unit: r.get::<_, i64>(5)? as u32,
                unit_name: r.get(6)?,
                emergency: r.get::<_, i64>(7)? != 0,
                system: r.get(8)?,
                audio: r.get(9)?,
                text: r.get(10)?,
            })
        })
        .map_err(|e| format!("preview: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("preview: {e}"))
}

/// (calls, transcribed calls) in scope.
fn scope_count(c: &Connection, t: &Tripwire, since: i64) -> (u32, u32) {
    let (wh, args) = scope_where(t, since, None);
    c.query_row(
        &format!(
            "SELECT COUNT(*), COALESCE(SUM(COALESCE(NULLIF(transcript_edited, ''), transcript, '') <> ''), 0) FROM calls WHERE {wh}"
        ),
        params_from_iter(args),
        |r| Ok((r.get::<_, i64>(0)? as u32, r.get::<_, i64>(1)? as u32)),
    )
    .unwrap_or((0, 0))
}

fn facts_of(r: &Row) -> CallFacts {
    CallFacts {
        id: Some(r.id),
        start: r.start,
        tg: r.tg,
        tg_name: r.tg_name.clone(),
        unit: r.unit,
        unit_name: r.unit_name.clone(),
        secs: r.secs,
        emergency: r.emergency,
        transcript: r.text.clone().filter(|t| !t.trim().is_empty()),
        system: r.system.clone(),
        ..Default::default()
    }
}

/// How many calls since `since` contain `phrase` (FTS phrase match).
fn count_phrase(c: &Connection, phrase: &str, since: i64) -> u32 {
    let Some(q) = fts_any(&[phrase.to_string()]) else {
        return 0;
    };
    c.query_row(
        "SELECT COUNT(*) FROM calls WHERE start >= ?1 AND id IN (SELECT rowid FROM calls_fts WHERE calls_fts MATCH ?2)",
        rusqlite::params![since, q],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n as u32)
    .unwrap_or(0)
}

/// The words the transcriber has written, with how many calls each is in
/// (words seen once are mostly noise and are left out).
fn vocabulary(c: &Connection) -> Vec<(String, u32)> {
    let Ok(mut st) = c.prepare(
        "SELECT term, SUM(doc) FROM calls_fts_vocab WHERE col IN ('transcript', 'transcript_edited')
         GROUP BY term HAVING SUM(doc) >= 2",
    ) else {
        return Vec::new();
    };
    st.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u32))
    })
    .map(|rows| rows.filter_map(Result::ok).collect())
    .unwrap_or_default()
}

/// What a never-heard phrase is heard as: each word swapped for words that
/// look or sound like it, split or joined ("v fib" ↔ "vfib"), kept when the
/// result is actually in the library.
pub fn suggest(
    c: &Connection,
    phrase: &str,
    since: i64,
    vocab: &[(String, u32)],
) -> Vec<Suggestion> {
    let words: Vec<String> = normalize(phrase).split(' ').map(str::to_string).collect();
    if words.is_empty() || words[0].is_empty() {
        return Vec::new();
    }
    let df: HashMap<&str, u32> = vocab.iter().map(|(t, n)| (t.as_str(), *n)).collect();
    let mut candidates: Vec<String> = Vec::new();
    if words.len() > 1 {
        candidates.push(words.concat());
    }
    for (i, w) in words.iter().enumerate() {
        // Only the words that are rarely or never heard are suspects.
        if df.get(w.as_str()).copied().unwrap_or(0) >= 5 && words.len() > 1 {
            continue;
        }
        let mut alts: Vec<(&str, u32)> = vocab
            .iter()
            .filter(|(t, _)| t != w && crate::fuzzy::alike(t, w))
            .map(|(t, n)| (t.as_str(), *n))
            .collect();
        alts.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        for (alt, _) in alts.into_iter().take(4) {
            let mut v = words.clone();
            v[i] = alt.to_string();
            candidates.push(v.join(" "));
        }
        // "vfib" → "v fib": a word the vocabulary knows as two.
        if words.len() == 1 && w.len() >= 4 {
            for k in 1..w.len() {
                let (a, b) = w.split_at(k);
                if df.contains_key(a) && df.contains_key(b) && (a.len() >= 2 || b.len() >= 2) {
                    candidates.push(format!("{a} {b}"));
                }
            }
        }
    }
    let mut out: Vec<Suggestion> = Vec::new();
    for cand in candidates {
        if cand == words.join(" ") || out.iter().any(|s| s.text == cand) {
            continue;
        }
        let hits = count_phrase(c, &cand, since);
        if hits > 0 {
            out.push(Suggestion { text: cand, hits });
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.hits));
    out.truncate(4);
    out
}

fn day_label(epoch: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(epoch, 0)
        .single()
        .map(|d| d.format("%a %-d").to_string())
        .unwrap_or_default()
}

/// Run `t` over the last `days` of the library.
pub fn preview(c: &Connection, t: &Tripwire, days: u32, now: i64) -> Result<Preview, String> {
    let days = days.clamp(1, 30);
    let since = now - days as i64 * 86_400;
    let mut p = Preview {
        days,
        ..Default::default()
    };
    // Day buckets, oldest first.
    let mut buckets: Vec<(i64, String, u32)> = (0..days as i64)
        .rev()
        .map(|d| {
            let at = now - d * 86_400;
            (at, day_label(at), 0)
        })
        .collect();
    let bucket = |start: i64, b: &mut Vec<(i64, String, u32)>| {
        let i = ((now - start) / 86_400).clamp(0, days as i64 - 1) as usize;
        let n = b.len();
        b[n - 1 - i].2 += 1;
    };
    match t.when.kind.as_str() {
        "conversation" | "digest" => {
            // Only the timing matters here, so leave the transcripts in the
            // library rather than dragging them all through the lock.
            let rows = scope_rows(c, t, since, None, 20_000, false)?;
            (p.scanned, p.transcribed) = scope_count(c, t, since);
            if t.when.kind == "digest" {
                p.messages = days * 1440 / t.when.digest.every_mins.max(1);
                p.matches = p.scanned;
                for r in &rows {
                    bucket(r.start, &mut buckets);
                }
            } else {
                // An exchange: calls on one talkgroup with no gap longer
                // than the end-of-conversation quiet.
                let gap = t.when.conversation.end_gap_secs as i64;
                let mut last: HashMap<u16, i64> = HashMap::new();
                for r in &rows {
                    let fresh = last.get(&r.tg).is_none_or(|l| r.start - l > gap);
                    if fresh {
                        p.messages += 1;
                        bucket(r.start, &mut buckets);
                    }
                    last.insert(r.tg, r.start);
                }
                p.matches = p.messages;
            }
            for r in with_text(c, rows.iter().rev().take(12)) {
                p.samples.push(sample(&r, Vec::new()));
            }
        }
        _ => {
            (p.scanned, p.transcribed) = scope_count(c, t, since);
            if !tripwires::is_narrowed(t) {
                // It would fire on everything; do not scan everything to
                // say so.
                p.warnings.push("Nothing narrows this down yet — pick talkgroups, radios or phrases, or it would fire on every call.".into());
                p.per_day = buckets.into_iter().map(|(_, l, n)| (l, n)).collect();
                return Ok(p);
            }
            let mut probe = t.clone();
            probe.enabled = true;
            let fts = fts_any(&t.when.phrases);
            // The transcripts are only needed where the words matter.
            let rows = scope_rows(c, t, since, fts.as_deref(), 20_000, tripwires::needs_transcript(t))?;
            let mut last: HashMap<u16, i64> = HashMap::new();
            let mut hits: Vec<(Row, Vec<String>)> = Vec::new();
            let mut per_phrase: HashMap<String, u32> = HashMap::new();
            for r in rows {
                let f = facts_of(&r);
                if let Some(text) = f.transcript.as_deref() {
                    for k in crate::alerts::matched_keywords(&t.when.phrases, text) {
                        *per_phrase.entry(k).or_default() += 1;
                    }
                }
                match tripwires::matches(&probe, &f) {
                    Some(kw) => {
                        p.matches += 1;
                        bucket(r.start, &mut buckets);
                        let quiet = t.send.quiet_secs as i64;
                        let counts = last
                            .get(&r.tg)
                            .is_none_or(|l| quiet == 0 || r.start - l >= quiet);
                        if counts {
                            p.messages += 1;
                            last.insert(r.tg, r.start);
                        }
                        hits.push((r, kw));
                    }
                    None => {
                        let text = f.transcript.as_deref().unwrap_or("");
                        if !t.when.except.is_empty()
                            && !crate::alerts::matched_keywords(&t.when.except, text).is_empty()
                        {
                            p.excepted += 1;
                        }
                    }
                }
            }
            let newest: Vec<&(Row, Vec<String>)> = hits.iter().rev().take(25).collect();
            let texts = with_text(c, newest.iter().map(|(r, _)| r));
            for (r, (_, kw)) in texts.into_iter().zip(newest) {
                p.samples.push(sample(&r, kw.clone()));
            }
            let vocab = if t
                .when
                .phrases
                .iter()
                .any(|ph| per_phrase.get(ph).copied().unwrap_or(0) == 0)
            {
                vocabulary(c)
            } else {
                Vec::new()
            };
            for ph in &t.when.phrases {
                let hits = per_phrase.get(ph).copied().unwrap_or(0);
                let anywhere = count_phrase(c, ph, since);
                let suggestions = if hits == 0 {
                    suggest(c, ph, since, &vocab)
                } else {
                    Vec::new()
                };
                p.phrases.push(PhraseStat {
                    phrase: ph.clone(),
                    hits,
                    anywhere,
                    suggestions,
                });
            }
        }
    }
    p.per_day = buckets.into_iter().map(|(_, l, n)| (l, n)).collect();
    let per_day = p.messages as f64 / days as f64;
    if t.when.kind == "call" && per_day > 40.0 {
        p.warnings.push(format!(
            "That is about {} messages a day. A longer quiet window, more specific phrases or an AI check would calm it down.",
            per_day.round()
        ));
    }
    Ok(p)
}

/// Fetch the transcripts of these rows (the scans that count do not carry
/// them), newest first.
fn with_text<'a>(c: &Connection, rows: impl Iterator<Item = &'a Row>) -> Vec<Row> {
    let mut out: Vec<Row> = rows.cloned().collect();
    for r in out.iter_mut() {
        if r.text.is_none() {
            r.text = c
                .query_row(
                    "SELECT COALESCE(NULLIF(transcript_edited, ''), transcript) FROM calls WHERE id = ?1",
                    [r.id],
                    |x| x.get(0),
                )
                .ok()
                .flatten();
        }
    }
    out
}

fn sample(r: &Row, keywords: Vec<String>) -> Sample {
    Sample {
        id: r.id,
        start: r.start,
        tg: r.tg,
        tg_name: r.tg_name.clone(),
        unit_name: r.unit_name.clone().unwrap_or_else(|| {
            if r.unit == 0 {
                String::new()
            } else {
                r.unit.to_string()
            }
        }),
        transcript: r.text.clone().unwrap_or_default(),
        keywords,
        audio: r.audio,
        emergency: r.emergency,
    }
}

/// Warnings about setup rather than traffic: transcription off, no model,
/// nowhere to send.
fn setup_warnings(state: &AppState, t: &Tripwire) -> Vec<String> {
    let mut w = Vec::new();
    let transcribing = state.transcriber.lock().unwrap().settings.enabled;
    if !transcribing && (tripwires::needs_transcript(t) || t.when.kind != "call") {
        w.push(
            "Transcription is off, so this cannot run — turn it on in Settings → Transcription."
                .into(),
        );
    }
    let al = state.alerts.lock().unwrap().settings.clone();
    if t.check.kind != "none" {
        if t.check.engine == "cloud" {
            let cloud = state.analyzers.lock().unwrap().settings.cloud.clone();
            if cloud.model.trim().is_empty() || crate::secrets::get("analyzer-cloud-key").is_none()
            {
                w.push("The cloud model is not set up — Settings → Connections.".into());
            }
        } else if al.ollama.model.trim().is_empty() {
            w.push("No local model is chosen for the check — Settings → Connections.".into());
        }
    }
    if t.send.telegram {
        if crate::secrets::get("bot-token").is_none() {
            w.push("No Telegram bot yet — Settings → Connections.".into());
        } else if let Err(e) = tripwires::resolve_dest(&al, &t.send) {
            w.push(format!("Send to: {e}."));
        }
    }
    w
}

#[tauri::command]
pub async fn tripwire_preview(
    app: AppHandle,
    tripwire: Tripwire,
    days: Option<u32>,
) -> Result<Preview, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let mut t = tripwire;
        tripwires::sanitize(&mut t)?;
        let state = app.state::<AppState>();
        let db = state
            .db
            .lock()
            .unwrap()
            .clone()
            .ok_or("the call library is not open")?;
        let mut p = {
            let c = db.lock().unwrap();
            preview(&c, &t, days.unwrap_or(7), crate::library::now())?
        };
        let mut w = setup_warnings(&state, &t);
        w.append(&mut p.warnings);
        p.warnings = w;
        Ok(p)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Serialize, Clone, Debug)]
pub struct Tried {
    pub id: i64,
    /// `send` | `quiet` | `unavailable`
    pub verdict: String,
    pub note: String,
    pub fields: Option<serde_json::Value>,
    pub message: String,
}

/// Run the check on a few calls (at most five) and show what would be sent.
#[tauri::command]
pub async fn tripwire_try(
    app: AppHandle,
    tripwire: Tripwire,
    ids: Vec<i64>,
) -> Result<Vec<Tried>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let mut t = tripwire;
        tripwires::sanitize(&mut t)?;
        let state = app.state::<AppState>();
        let db = state
            .db
            .lock()
            .unwrap()
            .clone()
            .ok_or("the call library is not open")?;
        let rows: Vec<crate::library::CallRow> = {
            let c = db.lock().unwrap();
            ids.iter()
                .take(5)
                .filter_map(|id| crate::library::get(&c, *id).ok().flatten())
                .collect()
        };
        let mut out = Vec::new();
        for r in rows {
            let id = r.id;
            let f = crate::alerts::facts_from_row(&app, r, None);
            let kw = f
                .transcript
                .as_deref()
                .map(|x| crate::alerts::matched_keywords(&t.when.phrases, x))
                .unwrap_or_default();
            let (verdict, note, fields) = tripwires::check_once(&state, &t, &f);
            let message = if verdict == "quiet" {
                String::new()
            } else {
                tripwires::render(&t.send.message, &t.name, &f, &kw, &note, fields.as_ref())
            };
            out.push(Tried {
                id,
                verdict,
                note,
                fields,
                message,
            });
        }
        Ok(out)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Serialize, Clone, Debug)]
pub struct Draft {
    pub tripwire: Tripwire,
    /// The call's words, rarest first, with how many calls each is in — the
    /// editor offers them as phrases to click.
    pub words: Vec<(String, u32)>,
    pub transcript: String,
}

const STOP: &[&str] = &[
    "the",
    "and",
    "for",
    "you",
    "your",
    "are",
    "was",
    "with",
    "that",
    "this",
    "have",
    "has",
    "from",
    "they",
    "them",
    "there",
    "their",
    "will",
    "can",
    "copy",
    "okay",
    "yeah",
    "just",
    "what",
    "when",
    "where",
    "who",
    "our",
    "out",
    "any",
    "all",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "about",
    "been",
    "into",
    "then",
    "than",
    "also",
    "like",
    "get",
    "got",
    "not",
    "but",
    "his",
    "her",
    "she",
    "him",
    "its",
    "it's",
    "we're",
    "i'm",
    "we'll",
    "let",
    "know",
    "going",
    "go",
    "come",
    "back",
    "over",
    "here",
    "now",
    "ahead",
    "thank",
    "thanks",
    "please",
    "roger",
    "affirmative",
    "negative",
    "unit",
    "units",
    "clear",
    "received",
];

/// Words that say where or who, not what: street types, compass points,
/// apparatus and dispatch boilerplate. A tripwire on "road" or "medic" fires
/// on everything.
const PLACE_AND_UNIT: &[&str] = &[
    "road",
    "street",
    "avenue",
    "drive",
    "lane",
    "court",
    "way",
    "place",
    "boulevard",
    "highway",
    "parkway",
    "circle",
    "trail",
    "pike",
    "terrace",
    "north",
    "south",
    "east",
    "west",
    "apartment",
    "apt",
    "suite",
    "block",
    "hundred",
    "thousand",
    "engine",
    "medic",
    "ladder",
    "squad",
    "ambulance",
    "battalion",
    "truck",
    "rescue",
    "tower",
    "chief",
    "car",
    "ems",
    "edo",
    "hours",
    "hour",
    "location",
    "room",
    "floor",
    "respond",
    "responding",
    "route",
    "scene",
    "dispatch",
    "channel",
    "district",
    "station",
    "sector",
    "zone",
    "cross",
    "between",
    "near",
    "block",
    "unit",
    "time",
    "number",
    "code",
    "priority",
    "alpha",
    "bravo",
    "charlie",
    "delta",
    "echo",
];

/// The phrase dispatch already named this call's incident with, when the
/// transcript says it ("Cardiac Arrest").
fn incident_type(c: &Connection, call: i64, text: &str) -> Option<String> {
    let t: String = c
        .query_row(
            "SELECT i.call_type FROM incident_calls ic JOIN incidents i ON i.id = ic.incident WHERE ic.call = ?1 LIMIT 1",
            [call],
            |r| r.get(0),
        )
        .ok()?;
    let t = t.trim().to_string();
    (!t.is_empty()
        && t != "Unknown"
        && !crate::alerts::matched_keywords(&[t.clone()], text).is_empty())
    .then_some(t.to_lowercase())
}

/// A tripwire drafted from one call: its talkgroup (and system), and the
/// phrase that says what happened — the incident's call type when dispatch
/// knows it, else the call's most specific run of content words.
#[tauri::command]
pub fn tripwire_draft(app: AppHandle, id: i64) -> Result<Draft, String> {
    let state = app.state::<AppState>();
    let db = state
        .db
        .lock()
        .unwrap()
        .clone()
        .ok_or("the call library is not open")?;
    let c = db.lock().unwrap();
    let r = crate::library::get(&c, id)?.ok_or("that call is no longer in the library")?;
    Ok(draft_from(&c, &r))
}

/// The pure half of `tripwire_draft`.
pub fn draft_from(c: &Connection, r: &crate::library::CallRow) -> Draft {
    let id = r.id;
    let text = r
        .transcript_edited
        .clone()
        .filter(|t| !t.trim().is_empty())
        .or(r.transcript.clone())
        .unwrap_or_default();
    let vocab: HashMap<String, u32> = vocabulary(c).into_iter().collect();
    let content = |w: &str| {
        w.len() >= 3
            && !STOP.contains(&w)
            && !PLACE_AND_UNIT.contains(&w)
            && !w.chars().any(|c| c.is_ascii_digit())
    };
    let toks: Vec<String> = normalize(&text).split(' ').map(str::to_string).collect();
    let mut words: Vec<(String, u32)> = Vec::new();
    for w in &toks {
        if content(w) && !words.iter().any(|(x, _)| x == w) {
            words.push((w.clone(), vocab.get(w.as_str()).copied().unwrap_or(1)));
        }
    }
    words.sort_by_key(|(_, n)| *n);
    // Two content words side by side ("cardiac arrest") say more than one;
    // among them, the ones heard before but not everywhere.
    let mut pairs: Vec<(String, u32)> = Vec::new();
    for w in toks.windows(2) {
        if content(&w[0]) && content(&w[1]) {
            let p = format!("{} {}", w[0], w[1]);
            if !pairs.iter().any(|(x, _)| *x == p) {
                let n = count_phrase(c, &p, 0);
                pairs.push((p, n));
            }
        }
    }
    pairs.sort_by_key(|(_, n)| (u32::from(*n < 2), *n));
    let phrases: Vec<String> = if let Some(t) = incident_type(c, id, &text) {
        vec![t]
    } else if let Some((p, _)) = pairs.first() {
        vec![p.clone()]
    } else {
        words
            .iter()
            .filter(|(_, n)| *n >= 2)
            .take(2)
            .map(|(w, _)| w.clone())
            .collect()
    };
    let mut t = Tripwire {
        name: if phrases.is_empty() {
            format!("Like this on {}", r.tg_name)
        } else {
            let p = phrases.join(" / ");
            let mut cs = p.chars();
            let cap = cs
                .next()
                .map(|f| f.to_uppercase().collect::<String>() + cs.as_str())
                .unwrap_or_default();
            format!("{cap} on {}", r.tg_name)
        },
        recipe: "from-call".into(),
        ..Default::default()
    };
    t.when.tgs = vec![r.tg];
    t.when.system = r.system.clone();
    t.when.phrases = phrases;
    t.when.emergency = r.emergency && t.when.phrases.is_empty();
    t.send.follow = "radio".into();
    // Offer the pairs as well as the single words.
    let mut offer: Vec<(String, u32)> = pairs.into_iter().take(6).collect();
    offer.extend(words);
    Draft {
        tripwire: t,
        words: offer,
        transcript: text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lib() -> (Connection, std::path::PathBuf) {
        let d =
            std::env::temp_dir().join(format!("hs_bt_{}_{}", std::process::id(), rand_suffix()));
        let _ = std::fs::remove_dir_all(&d);
        let c = crate::library::open(&d).unwrap();
        ensure_schema(&c);
        (c, d)
    }
    fn rand_suffix() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        N.fetch_add(1, Ordering::SeqCst)
    }
    fn call(c: &Connection, start: i64, tg: u16, unit: u32, text: &str) -> i64 {
        let id = crate::library::insert(
            c,
            &crate::library::CallRow {
                start,
                tg,
                tg_name: format!("TG {tg}"),
                unit,
                unit_name: Some(format!("Medic {unit}")),
                system: "Test".into(),
                ..Default::default()
            },
        )
        .unwrap();
        crate::library::set_transcript(c, id, text, "test").unwrap();
        id
    }

    #[test]
    fn fts_queries_stay_in_the_transcript_and_quote_safely() {
        assert_eq!(
            fts_any(&["V-Fib".into(), "say \"hi\"".into()]).unwrap(),
            "{transcript transcript_edited} : (\"v fib\" OR \"say hi\")"
        );
        assert!(fts_any(&["  ".into()]).is_none());
    }

    #[test]
    fn a_wide_open_call_tripwire_is_answered_without_reading_the_library() {
        let (c, d) = lib();
        let now = 10 * 86_400;
        for i in 0..5 {
            call(&c, now - 60 * i, 1, 7, "routine traffic");
        }
        // The recipe gallery opens an editor like this before the channel
        // picker; it must not drag every transcript through the lock.
        let t = Tripwire {
            id: "t1".into(),
            name: "Anything".into(),
            ..Default::default()
        };
        let p = preview(&c, &t, 7, now).unwrap();
        assert_eq!(p.scanned, 5, "it still says how much there is");
        assert_eq!(p.matches, 0);
        assert!(p.samples.is_empty());
        assert!(p.warnings.iter().any(|w| w.contains("Nothing narrows")));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn preview_counts_matches_messages_and_dead_phrases() {
        let (c, d) = lib();
        let now = 10 * 86_400;
        call(
            &c,
            now - 3600,
            1,
            7,
            "medic 7 respond cardiac arrest at the mall",
        );
        call(
            &c,
            now - 3500,
            1,
            7,
            "cardiac arrest update cpr in progress",
        );
        call(
            &c,
            now - 2 * 86_400,
            1,
            8,
            "cardiac arrest on the east side",
        );
        call(&c, now - 100, 2, 9, "cardiac arrest on another talkgroup");
        call(&c, now - 50, 1, 9, "history of cardiac arrest, stable");
        for i in 0..3 {
            call(&c, now - 200 - i, 1, 9, "patient in vfib starting shocks");
        }
        let mut t = Tripwire::default();
        t.when.tgs = vec![1];
        t.when.phrases = vec!["cardiac arrest".into(), "v fib".into()];
        t.when.except = vec!["history of cardiac arrest".into()];
        t.send.quiet_secs = 300;
        let p = preview(&c, &t, 7, now).unwrap();
        assert_eq!(p.matches, 3, "three arrests on TG 1, one excepted");
        assert_eq!(p.excepted, 1);
        assert_eq!(p.messages, 2, "the two within five minutes are one message");
        assert_eq!(p.per_day.len(), 7);
        assert_eq!(p.per_day.iter().map(|x| x.1).sum::<u32>(), 3);
        let vf = p.phrases.iter().find(|x| x.phrase == "v fib").unwrap();
        assert_eq!(vf.hits, 0);
        assert_eq!(
            vf.suggestions.first().map(|s| (s.text.as_str(), s.hits)),
            Some(("vfib", 3))
        );
        let ca = p
            .phrases
            .iter()
            .find(|x| x.phrase == "cardiac arrest")
            .unwrap();
        assert_eq!((ca.hits, ca.anywhere), (4, 5), "in scope vs any talkgroup");
        assert_eq!(
            p.samples.first().map(|s| s.start),
            Some(now - 3500),
            "newest first"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn misspelled_single_words_get_heard_alternatives() {
        let (c, d) = lib();
        let now = 5 * 86_400;
        for i in 0..4 {
            call(&c, now - 100 - i, 1, 1, "lucus device applied");
        }
        let got = suggest(&c, "lucas", 0, &vocabulary(&c));
        assert_eq!(got.first().map(|s| s.text.as_str()), Some("lucus"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_draft_listens_for_what_happened_not_where() {
        let (c, d) = lib();
        crate::dispatch::ensure_schema(&c);
        let id = call(
            &c,
            100,
            3,
            1,
            "Engine 74, Medic 74, EDO 70, 5609 Furnace Road, cardiac arrest.",
        );
        call(&c, 50, 3, 2, "Medic 12, 300 West Road, cardiac arrest.");
        call(&c, 60, 3, 2, "Engine 9, 12 West Road, smoke investigation.");
        let r = crate::library::get(&c, id).unwrap().unwrap();
        let dr = draft_from(&c, &r);
        assert_eq!(dr.tripwire.when.phrases, vec!["cardiac arrest"]);
        assert_eq!(dr.tripwire.when.tgs, vec![3]);
        assert!(dr.words.iter().all(|(w, _)| w != "road" && w != "medic"));
        // Dispatch's own call type wins when the call says it.
        c.execute("INSERT INTO incidents (id, created, updated, tg, call_type) VALUES (1, 0, 0, 3, 'Furnace Road Fire')", []).unwrap();
        c.execute(
            "INSERT INTO incident_calls (incident, call, at, tg) VALUES (1, ?1, 0, 3)",
            [id],
        )
        .unwrap();
        assert_eq!(
            draft_from(&c, &r).tripwire.when.phrases,
            vec!["cardiac arrest"],
            "not said in the call"
        );
        c.execute("UPDATE incidents SET call_type = 'Cardiac Arrest'", [])
            .unwrap();
        assert_eq!(
            draft_from(&c, &r).tripwire.when.phrases,
            vec!["cardiac arrest"]
        );
        assert!(draft_from(&c, &r)
            .tripwire
            .name
            .starts_with("Cardiac arrest on"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn conversations_preview_as_exchanges() {
        let (c, d) = lib();
        let now = 3 * 86_400;
        for s in [0, 30, 60, 1000, 1030] {
            call(&c, now - 5000 + s, 5, 1, "report");
        }
        let mut t = Tripwire::default();
        t.when.kind = "conversation".into();
        t.when.tgs = vec![5];
        let p = preview(&c, &t, 1, now).unwrap();
        assert_eq!((p.scanned, p.messages), (5, 2));
        let _ = std::fs::remove_dir_all(&d);
    }
}
