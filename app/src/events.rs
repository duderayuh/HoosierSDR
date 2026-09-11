//! Tripwire history: every time a rule fires — or looks and stays quiet —
//! one row in the call library, linked to the calls it was about.
//!
//! The rules' own logs are short in-memory rings, gone on restart, so they
//! could not answer "did anything fire on this call?" an hour later, or
//! "how often does this rule actually send?". This table can: the Library
//! shows a badge on every call a rule sent about, the Tripwires page counts
//! sends and quiet verdicts per rule, and the Telegram message ids kept here
//! are what a later follow-up replies to.
//!
//! Writing is best-effort: a rule never fails because its history could
//! not be written.

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tauri::{AppHandle, Manager, State};

use crate::AppState;

pub fn ensure_schema(c: &Connection) {
    let _ = c.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS tripwire_events (
            id INTEGER PRIMARY KEY,
            at INTEGER NOT NULL,
            source TEXT NOT NULL,
            rule_id TEXT NOT NULL DEFAULT '',
            rule_name TEXT NOT NULL DEFAULT '',
            tg INTEGER NOT NULL DEFAULT 0,
            tg_name TEXT NOT NULL DEFAULT '',
            incident_id INTEGER,
            status TEXT NOT NULL,
            detail TEXT NOT NULL DEFAULT '',
            message TEXT NOT NULL DEFAULT '',
            chat TEXT NOT NULL DEFAULT '',
            message_ids TEXT NOT NULL DEFAULT '[]',
            data TEXT NOT NULL DEFAULT ''
        );
        CREATE INDEX IF NOT EXISTS tripwire_events_at ON tripwire_events(at);
        CREATE INDEX IF NOT EXISTS tripwire_events_rule ON tripwire_events(rule_id, at);
        CREATE TABLE IF NOT EXISTS tripwire_event_calls (
            event INTEGER NOT NULL,
            call INTEGER NOT NULL,
            PRIMARY KEY (event, call)
        );
        CREATE INDEX IF NOT EXISTS tripwire_event_calls_call ON tripwire_event_calls(call);
        "#,
    );
}

/// One outcome to record.
#[derive(Clone, Debug, Default)]
pub struct NewEvent {
    /// `alert` | `analyzer` | `conversation` | `digest` | `tripwire`
    pub source: &'static str,
    pub rule_id: String,
    pub rule_name: String,
    pub tg: u16,
    pub tg_name: String,
    pub incident_id: Option<i64>,
    /// `sent` | `failed` | `quiet` | `held` | `skipped`
    pub status: String,
    pub detail: String,
    pub message: String,
    /// Where it went: `chat` or `chat:topic`.
    pub chat: String,
    /// Telegram message ids, so a follow-up can reply to them.
    pub message_ids: Vec<i64>,
    /// Extra JSON (extracted fields, matched words, the AI's verdict).
    pub data: String,
    /// The library calls it was about.
    pub calls: Vec<i64>,
}

/// A stored outcome.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Event {
    pub id: i64,
    pub at: i64,
    pub source: String,
    pub rule_id: String,
    pub rule_name: String,
    pub tg: u16,
    pub tg_name: String,
    pub incident_id: Option<i64>,
    pub status: String,
    pub detail: String,
    pub message: String,
    pub chat: String,
    pub message_ids: Vec<i64>,
    pub data: String,
    pub calls: Vec<i64>,
}

/// What the Library shows on a call: which rules fired about it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Fired {
    pub source: String,
    pub rule_id: String,
    pub rule_name: String,
    pub status: String,
    pub at: i64,
}

pub fn insert(c: &Connection, e: &NewEvent, at: i64) -> Result<i64, String> {
    c.execute(
        "INSERT INTO tripwire_events (at, source, rule_id, rule_name, tg, tg_name, incident_id, status, detail, message, chat, message_ids, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            at,
            e.source,
            e.rule_id,
            e.rule_name,
            e.tg as i64,
            e.tg_name,
            e.incident_id,
            e.status,
            e.detail,
            e.message,
            e.chat,
            serde_json::to_string(&e.message_ids).unwrap_or_else(|_| "[]".into()),
            e.data,
        ],
    )
    .map_err(|x| format!("event: {x}"))?;
    let id = c.last_insert_rowid();
    for call in &e.calls {
        if *call > 0 {
            let _ = c.execute(
                "INSERT OR IGNORE INTO tripwire_event_calls (event, call) VALUES (?1, ?2)",
                params![id, call],
            );
        }
    }
    Ok(id)
}

/// Record an outcome in the library. Never fails the caller; returns the
/// row id when it was written.
pub fn record(app: &AppHandle, e: NewEvent) -> Option<i64> {
    let state = app.state::<AppState>();
    let db = state.db.lock().unwrap().clone()?;
    let c = db.lock().unwrap();
    match insert(&c, &e, crate::library::now()) {
        Ok(id) => Some(id),
        Err(err) => {
            eprintln!("[events] {err}");
            None
        }
    }
}

/// For each of `ids`, the rules that fired about it — one entry per rule,
/// its newest outcome, sends before quiet verdicts. One query for the lot.
pub fn fired_for(c: &Connection, ids: &[i64]) -> HashMap<i64, Vec<Fired>> {
    let mut out: HashMap<i64, Vec<Fired>> = HashMap::new();
    if ids.is_empty() {
        return out;
    }
    let list = serde_json::to_string(ids).unwrap_or_else(|_| "[]".into());
    let Ok(mut st) = c.prepare(
        "SELECT ec.call, e.source, e.rule_id, e.rule_name, e.status, e.at
         FROM tripwire_event_calls ec JOIN tripwire_events e ON e.id = ec.event
         WHERE ec.call IN (SELECT value FROM json_each(?1))
         ORDER BY e.at DESC",
    ) else {
        return out;
    };
    let rows = st.query_map(params![list], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            Fired {
                source: r.get(1)?,
                rule_id: r.get(2)?,
                rule_name: r.get(3)?,
                status: r.get(4)?,
                at: r.get(5)?,
            },
        ))
    });
    if let Ok(rows) = rows {
        for (call, f) in rows.flatten() {
            let v = out.entry(call).or_default();
            // Newest first, so the first outcome per rule is the one kept.
            if !v.iter().any(|x| x.source == f.source && x.rule_id == f.rule_id) {
                v.push(f);
            }
        }
    }
    for v in out.values_mut() {
        v.sort_by_key(|f| (f.status != "sent", std::cmp::Reverse(f.at)));
    }
    out
}

fn event_row(r: &rusqlite::Row) -> rusqlite::Result<Event> {
    let ids: String = r.get(12)?;
    let calls: String = r.get::<_, Option<String>>(14)?.unwrap_or_default();
    Ok(Event {
        id: r.get(0)?,
        at: r.get(1)?,
        source: r.get(2)?,
        rule_id: r.get(3)?,
        rule_name: r.get(4)?,
        tg: r.get::<_, i64>(5)? as u16,
        tg_name: r.get(6)?,
        incident_id: r.get(7)?,
        status: r.get(8)?,
        detail: r.get(9)?,
        message: r.get(10)?,
        chat: r.get(11)?,
        message_ids: serde_json::from_str(&ids).unwrap_or_default(),
        data: r.get(13)?,
        calls: calls
            .split(',')
            .filter_map(|x| x.trim().parse().ok())
            .collect(),
    })
}

const EVENT_COLS: &str = "e.id, e.at, e.source, e.rule_id, e.rule_name, e.tg, e.tg_name, e.incident_id, e.status, e.detail, e.message, e.chat, e.message_ids, e.data,
    (SELECT group_concat(call) FROM tripwire_event_calls WHERE event = e.id)";

#[derive(Deserialize, Default, Clone, Debug)]
pub struct EventQuery {
    pub rule_id: Option<String>,
    pub source: Option<String>,
    pub status: Option<String>,
    pub call: Option<i64>,
    pub since: Option<i64>,
    pub before: Option<i64>,
    pub limit: Option<u32>,
}

pub fn list(c: &Connection, q: &EventQuery) -> Result<Vec<Event>, String> {
    let mut sql = format!("SELECT {EVENT_COLS} FROM tripwire_events e WHERE 1=1");
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(v) = q.rule_id.as_ref().filter(|v| !v.is_empty()) {
        sql.push_str(" AND e.rule_id = ?");
        args.push(Box::new(v.clone()));
    }
    if let Some(v) = q.source.as_ref().filter(|v| !v.is_empty()) {
        sql.push_str(" AND e.source = ?");
        args.push(Box::new(v.clone()));
    }
    if let Some(v) = q.status.as_ref().filter(|v| !v.is_empty()) {
        sql.push_str(" AND e.status = ?");
        args.push(Box::new(v.clone()));
    }
    if let Some(v) = q.call {
        sql.push_str(" AND e.id IN (SELECT event FROM tripwire_event_calls WHERE call = ?)");
        args.push(Box::new(v));
    }
    if let Some(v) = q.since {
        sql.push_str(" AND e.at >= ?");
        args.push(Box::new(v));
    }
    if let Some(v) = q.before {
        sql.push_str(" AND e.at < ?");
        args.push(Box::new(v));
    }
    sql.push_str(" ORDER BY e.at DESC, e.id DESC LIMIT ?");
    args.push(Box::new(q.limit.unwrap_or(200).clamp(1, 2000) as i64));
    let mut st = c.prepare(&sql).map_err(|e| format!("events: {e}"))?;
    let rows = st
        .query_map(
            rusqlite::params_from_iter(args.iter().map(|a| a.as_ref())),
            event_row,
        )
        .map_err(|e| format!("events: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("events: {e}"))
}

/// Per-rule counts since `since`: how often each rule sent, stayed quiet,
/// or failed, and when it last did anything.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct RuleStat {
    pub source: String,
    pub rule_id: String,
    pub rule_name: String,
    pub sent: u32,
    pub quiet: u32,
    pub failed: u32,
    pub last_at: i64,
    pub last_sent_at: i64,
}

pub fn stats(c: &Connection, since: i64) -> Result<Vec<RuleStat>, String> {
    let mut st = c
        .prepare(
            "SELECT source, rule_id, MAX(rule_name),
                    SUM(status = 'sent'), SUM(status = 'quiet'), SUM(status IN ('failed', 'held')),
                    MAX(at), COALESCE(MAX(CASE WHEN status = 'sent' THEN at END), 0)
             FROM tripwire_events WHERE at >= ?1
             GROUP BY source, rule_id ORDER BY MAX(at) DESC",
        )
        .map_err(|e| format!("event stats: {e}"))?;
    let rows = st
        .query_map(params![since], |r| {
            Ok(RuleStat {
                source: r.get(0)?,
                rule_id: r.get(1)?,
                rule_name: r.get(2)?,
                sent: r.get::<_, i64>(3)? as u32,
                quiet: r.get::<_, i64>(4)? as u32,
                failed: r.get::<_, i64>(5)? as u32,
                last_at: r.get(6)?,
                last_sent_at: r.get(7)?,
            })
        })
        .map_err(|e| format!("event stats: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("event stats: {e}"))
}

/// Drop history older than `days` (the links go with it).
pub fn prune(c: &Connection, days: u32) -> Result<usize, String> {
    let cutoff = crate::library::now() - days as i64 * 86_400;
    c.execute(
        "DELETE FROM tripwire_event_calls WHERE event IN (SELECT id FROM tripwire_events WHERE at < ?1)",
        params![cutoff],
    )
    .map_err(|e| e.to_string())?;
    c.execute("DELETE FROM tripwire_events WHERE at < ?1", params![cutoff])
        .map_err(|e| e.to_string())
}

fn with_db<T>(
    state: &State<AppState>,
    f: impl FnOnce(&Connection) -> Result<T, String>,
) -> Result<T, String> {
    let db = state
        .db
        .lock()
        .unwrap()
        .clone()
        .ok_or("the call library is not open")?;
    let c = db.lock().unwrap();
    f(&c)
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn events_list(state: State<AppState>, query: EventQuery) -> Result<Vec<Event>, String> {
    with_db(&state, |c| list(c, &query))
}

#[tauri::command]
pub fn events_stats(state: State<AppState>, since: i64) -> Result<Vec<RuleStat>, String> {
    with_db(&state, |c| stats(c, since))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        ensure_schema(&c);
        c
    }

    fn ev(rule: &str, status: &str, calls: &[i64]) -> NewEvent {
        NewEvent {
            source: "alert",
            rule_id: rule.into(),
            rule_name: format!("Rule {rule}"),
            tg: 1001,
            tg_name: "Test Dispatch".into(),
            status: status.into(),
            message_ids: vec![41, 42],
            calls: calls.to_vec(),
            ..Default::default()
        }
    }

    #[test]
    fn badges_are_one_per_rule_newest_first_sends_first() {
        let c = db();
        insert(&c, &ev("a", "quiet", &[1, 2]), 100).unwrap();
        insert(&c, &ev("a", "sent", &[1]), 200).unwrap();
        insert(&c, &ev("b", "quiet", &[1]), 300).unwrap();
        insert(&c, &ev("c", "sent", &[3]), 300).unwrap();
        let f = fired_for(&c, &[1, 2, 9]);
        let one = &f[&1];
        assert_eq!(one.len(), 2, "{one:?}");
        assert_eq!((one[0].rule_id.as_str(), one[0].status.as_str()), ("a", "sent"));
        assert_eq!((one[1].rule_id.as_str(), one[1].status.as_str()), ("b", "quiet"));
        assert_eq!(f[&2][0].status, "quiet");
        assert!(!f.contains_key(&9));
        assert!(!f.contains_key(&3), "only the asked-for calls");
    }

    #[test]
    fn listing_filters_and_carries_calls_and_ids() {
        let c = db();
        insert(&c, &ev("a", "sent", &[5, 6]), 100).unwrap();
        insert(&c, &ev("b", "failed", &[7]), 200).unwrap();
        let all = list(&c, &EventQuery::default()).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].rule_id, "b", "newest first");
        let a = list(&c, &EventQuery { rule_id: Some("a".into()), ..Default::default() }).unwrap();
        assert_eq!(a[0].calls, vec![5, 6]);
        assert_eq!(a[0].message_ids, vec![41, 42]);
        let by_call = list(&c, &EventQuery { call: Some(7), ..Default::default() }).unwrap();
        assert_eq!(by_call.len(), 1);
        assert_eq!(by_call[0].status, "failed");
    }

    #[test]
    fn stats_count_outcomes_per_rule() {
        let c = db();
        insert(&c, &ev("a", "sent", &[]), 100).unwrap();
        insert(&c, &ev("a", "quiet", &[]), 110).unwrap();
        insert(&c, &ev("a", "quiet", &[]), 120).unwrap();
        insert(&c, &ev("a", "held", &[]), 130).unwrap();
        insert(&c, &ev("b", "sent", &[]), 50).unwrap();
        let s = stats(&c, 60).unwrap();
        assert_eq!(s.len(), 1, "b is before the window");
        assert_eq!((s[0].sent, s[0].quiet, s[0].failed), (1, 2, 1));
        assert_eq!((s[0].last_at, s[0].last_sent_at), (130, 100));
    }
}
