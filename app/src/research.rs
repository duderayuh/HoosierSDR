//! Research statistics: the intervals a study asks for, read off the library.
//!
//! An emergency department usually has two clocks for a critical patient:
//! the phone call from the crew and the patient's arrival. The radio gives
//! an earlier one, the moment this app could have said something, and an
//! earlier one still, the page. This module lines those clocks up for every
//! case and keeps the numbers a paper would need: how far ahead of the crew's
//! call the alert came, how long from page to call, from CPR first reported to
//! ROSC, and how the stated time to arrival compared with what was said on
//! arrival.
//!
//! Three rules shape it:
//!
//! - **Every number is computed here, not on the page**, so the definitions
//!   have one home and the tests check the same arithmetic the tab shows.
//! - **The radio's clock is the trustworthy part.** A moment is the start of
//!   the transmission it was said in. What the crew *meant* is not inferred:
//!   "not stated" stays a gap, and a downtime is quoted as theirs.
//! - **What the radio cannot hear is typed in, and kept apart.** The phone
//!   call the ED logged and the arrival time from the chart are the ED's
//!   record; the tab says which clock each interval used.
//!
//! Rows are keyed on the case's run (`cases.incident`), never on `cases.id`,
//! which a rebuild renumbers. Each computation also files a snapshot, so a
//! run whose calls retention has since removed keeps its numbers.

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tauri::{AppHandle, Manager, State};

use crate::AppState;

/// The facts a report is asked for, in the order a clinician reads them.
const FACT_KEYS: &[&str] = crate::conversations::FACT_KEYS;

pub fn ensure_schema(c: &Connection) {
    let _ = c.execute_batch(
        "CREATE TABLE IF NOT EXISTS research_records (
            profile TEXT NOT NULL,
            incident INTEGER NOT NULL,
            phone_at INTEGER,
            ed_arrived_at INTEGER,
            note TEXT NOT NULL DEFAULT '',
            updated INTEGER NOT NULL,
            PRIMARY KEY (profile, incident)
         );
         CREATE TABLE IF NOT EXISTS research_snapshots (
            profile TEXT NOT NULL,
            incident INTEGER NOT NULL,
            opened INTEGER NOT NULL,
            computed INTEGER NOT NULL,
            row TEXT NOT NULL,
            PRIMARY KEY (profile, incident)
         );
         CREATE INDEX IF NOT EXISTS research_snapshots_opened ON research_snapshots(opened);",
    );
}

// ---------------------------------------------------------------------------
// one case, as moments and the intervals between them
// ---------------------------------------------------------------------------

/// Everything a study wants to know about one case. Moments are epoch
/// seconds; intervals are seconds and may be negative when the later clock
/// came first (an alert after the crew's call is a negative lead).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct CaseRow {
    pub profile: String,
    pub incident: i64,
    pub title: String,
    pub address: String,
    pub state: String,
    pub units: u32,
    /// The case was read from a snapshot: its calls or its case are gone.
    #[serde(default)]
    pub recorded: bool,

    // -- what the radio said, by the transmission it was said in --
    pub dispatched: Option<i64>,
    /// The page's transcript landed: the earliest this app could have spoken.
    pub known: Option<i64>,
    /// The earliest message actually sent about this run, and by what.
    pub alerted: Option<i64>,
    pub alerted_how: String,
    pub working: Option<i64>,
    pub rosc: Option<i64>,
    pub rearrest: Option<i64>,
    pub transporting: Option<i64>,
    pub terminated: Option<i64>,
    pub downgraded: Option<i64>,
    /// The crew's report to the hospital: the first transmission of the
    /// first report joined to this run.
    pub report: Option<i64>,
    pub report_place: String,
    pub reports: u32,
    pub eta_said: Option<String>,
    pub eta_from: Option<i64>,
    pub eta_to: Option<i64>,
    pub drive_min: Option<i64>,
    pub drive_how: String,
    /// A crew said they were at the hospital.
    pub arrived_said: Option<i64>,
    /// Minutes the said arrival fell outside the ETA window; 0 is inside.
    pub off_by_min: Option<i64>,
    /// The facts the report stated, by key.
    pub facts: Vec<String>,
    /// What each stated fact said, by key: the latest report that stated
    /// it, which is the value the case message shows.
    #[serde(default)]
    pub fact_values: BTreeMap<String, String>,
    /// When the first report joined to the run was summarised: the moment
    /// the AI report existed.
    #[serde(default)]
    pub report_generated: Option<i64>,
    /// The first extraction screen (the ECPR tripwire) run on this run's
    /// calls, as the model answered it.
    #[serde(default)]
    pub screen: Option<Screen>,
    /// The survival score over `fact_values`, worked out in code. Only once
    /// there is a report to work it from.
    #[serde(default)]
    pub score: Option<crate::study::Score>,
    /// Every call the case rests on: the run's calls, the timeline's and the
    /// reports' transmissions. For the review packet.
    #[serde(default)]
    pub calls: Vec<i64>,

    // -- the ED's record, typed in --
    pub phone_at: Option<i64>,
    pub ed_arrived_at: Option<i64>,
    pub note: String,

    // -- the pipeline --
    /// Page transcript landed minus the page ended.
    pub transcribe_secs: Option<i64>,
    /// Alert sent minus the page transcript landed.
    pub alert_secs: Option<i64>,
    /// AI report minus the page: the study's end-to-end latency.
    #[serde(default)]
    pub dispatch_to_ai_report: Option<i64>,
    /// AI report minus the start of the crew's report transmission.
    #[serde(default)]
    pub report_to_ai_report: Option<i64>,

    // -- intervals, seconds --
    /// The crew's call minus the alert: how far ahead the alert came.
    pub alert_to_call: Option<i64>,
    /// The crew's call minus the moment the app knew: the lead an alert
    /// could have had, whether or not one was sent.
    pub known_to_call: Option<i64>,
    pub dispatch_to_call: Option<i64>,
    pub dispatch_to_working: Option<i64>,
    pub working_to_rosc: Option<i64>,
    pub dispatch_to_rosc: Option<i64>,
    pub call_to_arrival: Option<i64>,
    pub alert_to_arrival: Option<i64>,
    pub dispatch_to_arrival: Option<i64>,
    /// Which clock `call` used: `ED record` or `radio report`.
    pub call_how: String,
    /// Which clock `arrival` used: `ED record`, `said on air` or `stated ETA`.
    pub arrival_how: String,

    // -- for slicing on the page --
    /// The keys of every count this case is in, so the page can filter on
    /// "ROSC said" without restating what it means.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Each interval this case has, in minutes, by measure key.
    #[serde(default)]
    pub minutes: BTreeMap<String, f64>,
}

impl CaseRow {
    /// The crew's call: the ED's logged phone call when typed in, else the
    /// radio report.
    pub fn call(&self) -> (Option<i64>, &'static str) {
        match (self.phone_at, self.report) {
            (Some(t), _) => (Some(t), "ED record"),
            (None, Some(t)) => (Some(t), "radio report"),
            _ => (None, ""),
        }
    }

    /// The arrival: the chart's time when typed in, else what a crew said on
    /// air, else the middle of the stated ETA window.
    pub fn arrival(&self) -> (Option<i64>, &'static str) {
        if let Some(t) = self.ed_arrived_at {
            return (Some(t), "ED record");
        }
        if let Some(t) = self.arrived_said {
            return (Some(t), "said on air");
        }
        match (self.eta_from, self.eta_to) {
            (Some(a), Some(b)) => (Some((a + b) / 2), "stated ETA"),
            _ => (None, ""),
        }
    }

    /// Fill every interval from the moments. Idempotent.
    pub fn derive(&mut self) {
        let (call, call_how) = self.call();
        let (arrival, arrival_how) = self.arrival();
        self.call_how = call_how.into();
        self.arrival_how = arrival_how.into();
        let diff = |later: Option<i64>, earlier: Option<i64>| Some(later? - earlier?);
        self.alert_secs = diff(self.alerted, self.known);
        self.alert_to_call = diff(call, self.alerted);
        self.known_to_call = diff(call, self.known);
        self.dispatch_to_call = diff(call, self.dispatched);
        self.dispatch_to_working = diff(self.working, self.dispatched);
        self.working_to_rosc = diff(self.rosc, self.working);
        self.dispatch_to_rosc = diff(self.rosc, self.dispatched);
        self.call_to_arrival = diff(arrival, call);
        self.alert_to_arrival = diff(arrival, self.alerted);
        self.dispatch_to_arrival = diff(arrival, self.dispatched);
        self.dispatch_to_ai_report = diff(self.report_generated, self.dispatched);
        self.report_to_ai_report = diff(self.report_generated, self.report);
        self.score = self.report.map(|_| crate::study::score(&self.fact_values));
        self.tags = COUNTS.iter().filter(|c| (c.is)(self)).map(|c| c.key.to_string()).collect();
        self.tags.extend(FACT_KEYS.iter().filter(|k| self.facts.iter().any(|f| f == *k)).map(|k| fact_key(k)));
        self.minutes = MEASURES.iter().filter_map(|m| (m.secs)(self).map(|s| (m.key.to_string(), round1(s as f64 / 60.0)))).collect();
    }
}

/// What an extraction screen answered about a run: the first one with
/// fields. Kept as the model gave it, beside the score worked out in code,
/// so the two can be compared.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Screen {
    pub rule: String,
    pub at: i64,
    /// sent | quiet | failed | held …
    pub status: String,
    pub candidate: String,
    pub criteria_met: String,
    pub likelihood_pct: String,
    pub reason: String,
    /// Every field, as JSON.
    pub fields: String,
}

/// One case from the case builder, with the alert ledger and the ED record
/// looked up.
pub fn row_for(c: &Connection, k: &crate::cases::CaseView) -> CaseRow {
    let first = |kind: &str| k.lines.iter().find(|l| l.kind == kind);
    let mut r = CaseRow {
        profile: k.profile.clone(),
        incident: k.incident,
        title: k.title.clone(),
        address: k.address.clone(),
        state: k.state.clone(),
        units: k.units.len() as u32,
        dispatched: first("dispatched").map(|l| l.at).or(Some(k.opened)),
        // The page itself saying "working" is labelled on the dispatched line,
        // not given a line of its own, so working is at dispatch then.
        working: first("working")
            .map(|l| l.at)
            .or_else(|| first("dispatched").filter(|l| l.label.to_ascii_lowercase().contains("working")).map(|l| l.at)),
        rosc: first("rosc").map(|l| l.at),
        rearrest: first("rearrest").map(|l| l.at),
        transporting: first("transporting").map(|l| l.at),
        terminated: first("terminated").map(|l| l.at),
        downgraded: first("downgrade").map(|l| l.at),
        ..Default::default()
    };
    // The page: when its transcript landed is when the app knew.
    if let Some(call) = first("dispatched").and_then(|l| l.call) {
        if let Ok(Some((start, secs, landed))) = c
            .query_row("SELECT start, secs, transcribed_at FROM calls WHERE id = ?1", [call], |x| {
                Ok((x.get::<_, i64>(0)?, x.get::<_, f64>(1)?, x.get::<_, Option<i64>>(2)?))
            })
            .optional()
        {
            r.known = landed.filter(|t| *t > 0);
            r.transcribe_secs = r.known.map(|t| t - (start + secs.round() as i64));
        }
    }
    // The reports, oldest first; the first is the crew's call.
    let reports: Vec<_> = k.lines.iter().filter(|l| l.kind == "report").collect();
    r.reports = reports.len() as u32;
    if let Some(first_report) = reports.first() {
        r.report = Some(first_report.at);
        r.report_place = first_report
            .label
            .trim_start_matches("Report to ")
            .split(" · ")
            .next()
            .unwrap_or_default()
            .to_string();
    }
    let mut facts: Vec<String> = Vec::new();
    for l in &reports {
        for f in &l.facts {
            if !facts.contains(&f.key) {
                facts.push(f.key.clone());
            }
            // The latest report's word, as the case message shows it.
            r.fact_values.insert(f.key.clone(), f.value.clone());
        }
    }
    r.facts = facts;
    r.report_generated = reports.first().and_then(|l| l.conversation).and_then(|id| summarized_at(c, id));
    r.calls = case_calls(c, k);
    r.screen = first_screen(c, &k.incidents, &r.calls);
    if let Some(a) = &k.arrival {
        r.eta_said = a.said.clone();
        r.eta_from = a.from;
        r.eta_to = a.to;
        r.drive_min = a.drive_min;
        r.drive_how = a.drive_how.clone();
        r.arrived_said = a.arrived;
        r.off_by_min = a.off_by_min;
        if r.report_place.is_empty() {
            r.report_place = a.place.clone();
        }
    }
    if r.arrived_said.is_none() {
        r.arrived_said = first(crate::cases::ARRIVED).map(|l| l.at);
    }
    let (alerted, how) = first_alert(c, &k.profile, &k.incidents);
    r.alerted = alerted;
    r.alerted_how = how;
    if let Some((phone, arrived, note)) = record(c, &k.profile, k.incident) {
        r.phone_at = phone;
        r.ed_arrived_at = arrived;
        r.note = note;
    }
    r.derive();
    r
}

/// When a report was first summarised. Rows stored before that was kept
/// fall back to their send time if they were never revised, since then it
/// is the same moment.
fn summarized_at(c: &Connection, conversation: i64) -> Option<i64> {
    c.query_row(
        "SELECT COALESCE(summarized_at, CASE WHEN revision = 0 AND summary <> '' AND sent_at > 0 THEN sent_at END)
           FROM conversations WHERE id = ?1",
        [conversation],
        |r| r.get(0),
    )
    .ok()
    .flatten()
}

/// The calls a case rests on, oldest first: the run's own, each timeline
/// line's, and every transmission of the reports joined to it.
fn case_calls(c: &Connection, k: &crate::cases::CaseView) -> Vec<i64> {
    let mut ids: std::collections::BTreeSet<i64> = k.lines.iter().filter_map(|l| l.call).collect();
    if !k.incidents.is_empty() {
        if let Ok(mut q) = c.prepare(&format!("SELECT call FROM incident_calls WHERE incident IN ({})", id_list(&k.incidents))) {
            if let Ok(rows) = q.query_map([], |r| r.get::<_, i64>(0)) {
                ids.extend(rows.flatten());
            }
        }
    }
    for conv in k.lines.iter().filter_map(|l| l.conversation) {
        let pieces: String = c.query_row("SELECT pieces FROM conversations WHERE id = ?1", [conv], |r| r.get(0)).unwrap_or_default();
        let pieces: Vec<crate::conversations::Piece> = serde_json::from_str(&pieces).unwrap_or_default();
        ids.extend(pieces.iter().filter_map(|p| p.id));
    }
    if ids.is_empty() {
        return Vec::new();
    }
    let list = ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
    c.prepare(&format!("SELECT id FROM calls WHERE id IN ({list}) ORDER BY start, id"))
        .and_then(|mut q| q.query_map([], |r| r.get(0)).map(|rows| rows.flatten().collect()))
        .unwrap_or_default()
}

/// The first tripwire that extracted fields about this run or any of its
/// calls. One naming a candidate (the ECPR screen's shape) wins over any
/// other extraction.
fn first_screen(c: &Connection, incidents: &[i64], calls: &[i64]) -> Option<Screen> {
    if incidents.is_empty() && calls.is_empty() {
        return None;
    }
    let inc = if incidents.is_empty() { "NULL".to_string() } else { id_list(incidents) };
    let cl = if calls.is_empty() { "NULL".to_string() } else { id_list(calls) };
    let rows: Vec<(i64, String, String, String)> = c
        .prepare(&format!(
            "SELECT at, rule_name, status, data FROM tripwire_events
              WHERE source = 'tripwire' AND data LIKE '%\"fields\":{{%'
                AND (incident_id IN ({inc}) OR id IN (SELECT event FROM tripwire_event_calls WHERE call IN ({cl})))
              ORDER BY at"
        ))
        .and_then(|mut q| q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).map(|rows| rows.flatten().collect()))
        .unwrap_or_default();
    let parsed: Vec<(i64, String, String, serde_json::Map<String, serde_json::Value>)> = rows
        .into_iter()
        .filter_map(|(at, rule, status, data)| {
            let v: serde_json::Value = serde_json::from_str(&data).ok()?;
            Some((at, rule, status, v.get("fields")?.as_object()?.clone()))
        })
        .collect();
    let pick = parsed.iter().find(|(.., f)| f.contains_key("candidate")).or_else(|| parsed.first())?;
    let (at, rule, status, f) = pick;
    let text = |k: &str| match f.get(k) {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Null) | None => String::new(),
        Some(v) => v.to_string(),
    };
    Some(Screen {
        rule: rule.clone(),
        at: *at,
        status: status.clone(),
        candidate: text("candidate"),
        criteria_met: text("criteriaMet"),
        likelihood_pct: text("likelihoodPct"),
        reason: text("reason"),
        fields: serde_json::Value::Object(f.clone()).to_string(),
    })
}

fn id_list(ids: &[i64]) -> String {
    ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",")
}

/// The earliest message sent about any of a case's runs: a tripwire that
/// named the run or fired on one of its calls, the case timeline, or a case
/// reply. Tables that are not there yet (an older library) count as silence.
pub fn first_alert(c: &Connection, profile: &str, incidents: &[i64]) -> (Option<i64>, String) {
    if incidents.is_empty() {
        return (None, String::new());
    }
    let ids = id_list(incidents);
    let mut best: Option<(i64, String)> = None;
    let mut offer = |at: Option<i64>, how: String| {
        if let Some(t) = at.filter(|t| *t > 0) {
            if best.as_ref().map_or(true, |(b, _)| t < *b) {
                best = Some((t, how));
            }
        }
    };
    let tw: Option<(i64, String, String)> = c
        .query_row(
            &format!(
                "SELECT at, source, rule_name FROM tripwire_events
                  WHERE status = 'sent' AND (incident_id IN ({ids})
                     OR id IN (SELECT event FROM tripwire_event_calls
                                WHERE call IN (SELECT call FROM incident_calls WHERE incident IN ({ids}))))
                  ORDER BY at LIMIT 1"
            ),
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .ok()
        .flatten();
    if let Some((at, source, name)) = tw {
        let how = if name.is_empty() { source } else { format!("{source}: {name}") };
        offer(Some(at), how);
    }
    let sends: Option<i64> = c
        .query_row(
            &format!("SELECT MIN(sent_at) FROM case_sends WHERE profile = ?1 AND incident IN ({ids}) AND error = ''"),
            [profile],
            |r| r.get(0),
        )
        .ok()
        .flatten();
    offer(sends, "case timeline".into());
    let notices: Option<i64> = c
        .query_row(
            &format!("SELECT MIN(sent_at) FROM case_notices WHERE profile = ?1 AND incident IN ({ids})"),
            [profile],
            |r| r.get(0),
        )
        .ok()
        .flatten();
    offer(notices, "case reply".into());
    match best {
        Some((t, how)) => (Some(t), how),
        None => (None, String::new()),
    }
}

fn record(c: &Connection, profile: &str, incident: i64) -> Option<(Option<i64>, Option<i64>, String)> {
    c.query_row(
        "SELECT phone_at, ed_arrived_at, note FROM research_records WHERE profile = ?1 AND incident = ?2",
        params![profile, incident],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()
    .ok()
    .flatten()
}

// ---------------------------------------------------------------------------
// summaries
// ---------------------------------------------------------------------------

/// Five-number summary of one interval, in minutes.
#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub struct Measure {
    pub key: String,
    pub label: String,
    /// Which clocks it runs between, for the methods section.
    pub definition: String,
    pub n: usize,
    pub median: f64,
    pub p25: f64,
    pub p75: f64,
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    /// Cases where the later clock came first.
    pub negative: usize,
}

/// A count out of a denominator.
#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub struct Count {
    pub key: String,
    pub label: String,
    pub n: usize,
    pub of: usize,
}

/// Linear-interpolated quantile of a sorted slice; `q` in 0..=1.
pub fn quantile(sorted: &[f64], q: f64) -> f64 {
    match sorted.len() {
        0 => 0.0,
        1 => sorted[0],
        n => {
            let pos = q.clamp(0.0, 1.0) * (n - 1) as f64;
            let lo = pos.floor() as usize;
            let hi = pos.ceil() as usize;
            sorted[lo] + (sorted[hi] - sorted[lo]) * (pos - lo as f64)
        }
    }
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// Summarise seconds as minutes.
pub fn measure(key: &str, label: &str, definition: &str, secs: impl Iterator<Item = i64>) -> Measure {
    let mut v: Vec<f64> = secs.map(|s| s as f64 / 60.0).collect();
    let negative = v.iter().filter(|x| **x < 0.0).count();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    Measure {
        key: key.into(),
        label: label.into(),
        definition: definition.into(),
        n,
        median: round1(quantile(&v, 0.5)),
        p25: round1(quantile(&v, 0.25)),
        p75: round1(quantile(&v, 0.75)),
        min: round1(v.first().copied().unwrap_or(0.0)),
        max: round1(v.last().copied().unwrap_or(0.0)),
        mean: round1(if n == 0 { 0.0 } else { v.iter().sum::<f64>() / n as f64 }),
        negative,
    }
}

/// What the library held over the window, for the denominators.
#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub struct Slice {
    pub calls: i64,
    pub hours: f64,
    pub transcribed: i64,
    pub incidents: i64,
    pub reports: i64,
    pub reports_joined: i64,
    pub reports_joined_by_radio: i64,
    pub reports_with_facts: i64,
    pub alerts_sent: i64,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Stats {
    pub from: i64,
    pub to: i64,
    pub days: u32,
    pub library: Slice,
    /// Case intervals, in the order a reader wants them.
    pub measures: Vec<Measure>,
    /// Pipeline latencies over every call and alert in the window, not only
    /// the cases.
    pub pipeline: Vec<Measure>,
    pub counts: Vec<Count>,
    pub rows: Vec<CaseRow>,
    pub notes: Vec<String>,
}

/// One interval a study reports: which clocks it runs between, and how to
/// read it off a case, in seconds.
pub struct MeasureDef {
    pub key: &'static str,
    pub label: &'static str,
    pub definition: &'static str,
    pub secs: fn(&CaseRow) -> Option<i64>,
}

pub const MEASURES: &[MeasureDef] = &[
    MeasureDef { key: "alert_to_call", label: "Alert → crew's call", definition: "the crew's call (the ED's logged phone call, else the first radio report joined to the run) minus the first message sent about the run", secs: |r| r.alert_to_call },
    MeasureDef { key: "known_to_call", label: "App knew → crew's call", definition: "the crew's call minus the moment the page's transcript landed; the lead an alert could have had", secs: |r| r.known_to_call },
    MeasureDef { key: "dispatch_to_call", label: "Dispatch → crew's call", definition: "the crew's call minus the page", secs: |r| r.dispatch_to_call },
    MeasureDef { key: "call_to_arrival", label: "Crew's call → arrival", definition: "arrival (the chart, else a crew saying they are at the hospital, else the middle of the stated ETA) minus the crew's call", secs: |r| r.call_to_arrival },
    MeasureDef { key: "alert_to_arrival", label: "Alert → arrival", definition: "arrival minus the first message sent", secs: |r| r.alert_to_arrival },
    MeasureDef { key: "dispatch_to_arrival", label: "Dispatch → arrival", definition: "arrival minus the page", secs: |r| r.dispatch_to_arrival },
    MeasureDef { key: "dispatch_to_working", label: "Dispatch → working arrest", definition: "the first 'working' said on air minus the page; 0 when the page itself said working", secs: |r| r.dispatch_to_working },
    MeasureDef { key: "working_to_rosc", label: "Working → ROSC", definition: "ROSC first said minus working first said", secs: |r| r.working_to_rosc },
    MeasureDef { key: "dispatch_to_rosc", label: "Dispatch → ROSC", definition: "ROSC first said minus the page", secs: |r| r.dispatch_to_rosc },
    MeasureDef { key: "eta_off", label: "Said arrival vs stated ETA", definition: "minutes a crew's 'at the hospital' fell outside the window their stated ETA gave; 0 is inside, negative is early", secs: |r| r.off_by_min.map(|m| m * 60) },
    MeasureDef { key: "dispatch_to_ai_report", label: "Arrest page → AI report", definition: "the first summary of the first crew report joined to the run, stored, minus the page", secs: |r| r.dispatch_to_ai_report },
    MeasureDef { key: "report_to_ai_report", label: "Crew's report → AI report", definition: "the first summary of that report, stored, minus the start of its first transmission", secs: |r| r.report_to_ai_report },
    MeasureDef { key: "transcribe", label: "Page ended → transcript landed", definition: "for the case's page only", secs: |r| r.transcribe_secs },
    MeasureDef { key: "alert_lag", label: "Transcript landed → alert sent", definition: "for the case's page only", secs: |r| r.alert_secs },
];

/// A yes-or-no about a case, counted over every case.
pub struct CountDef {
    pub key: &'static str,
    pub label: &'static str,
    pub is: fn(&CaseRow) -> bool,
}

pub const COUNTS: &[CountDef] = &[
    CountDef { key: "alerted", label: "A message was sent", is: |r| r.alerted.is_some() },
    CountDef { key: "reported", label: "A crew reported to a hospital", is: |r| r.report.is_some() },
    CountDef { key: "working", label: "Working arrest said", is: |r| r.working.is_some() },
    CountDef { key: "rosc", label: "ROSC said", is: |r| r.rosc.is_some() },
    CountDef { key: "rearrest", label: "Lost pulses said", is: |r| r.rearrest.is_some() },
    CountDef { key: "transporting", label: "Transporting said", is: |r| r.transporting.is_some() },
    CountDef { key: "terminated", label: "Efforts ceased", is: |r| r.terminated.is_some() },
    CountDef { key: "downgraded", label: "Not an arrest", is: |r| r.downgraded.is_some() },
    CountDef { key: "eta_said", label: "An ETA was stated", is: |r| r.eta_from.is_some() },
    CountDef { key: "arrived_said", label: "At the hospital said on air", is: |r| r.arrived_said.is_some() },
    CountDef { key: "eta_inside", label: "Said arrival inside the ETA window", is: |r| r.off_by_min == Some(0) },
    CountDef { key: "eta_early", label: "Said arrival before the window", is: |r| r.off_by_min.map_or(false, |m| m < 0) },
    CountDef { key: "eta_late", label: "Said arrival after the window", is: |r| r.off_by_min.map_or(false, |m| m > 0) },
    CountDef { key: "ai_report", label: "An AI report was generated", is: |r| r.report_generated.is_some() },
    CountDef { key: "score_complete", label: "Score had every input stated", is: |r| r.score.as_ref().map_or(false, |s| s.complete) },
    CountDef { key: "screened", label: "An extraction screen ran", is: |r| r.screen.is_some() },
    CountDef { key: "ed_phone", label: "ED phone call typed in", is: |r| r.phone_at.is_some() },
    CountDef { key: "ed_arrived", label: "ED arrival typed in", is: |r| r.ed_arrived_at.is_some() },
];

fn fact_key(k: &str) -> String {
    format!("fact_{}", k.replace(' ', "_"))
}

fn fact_label(k: &str) -> String {
    match k {
        "bystander cpr" => "Bystander CPR stated".to_string(),
        "rosc" => "ROSC stated in the report".to_string(),
        "eta" => "ETA stated in the report".to_string(),
        k => {
            let mut s = k.to_string();
            if let Some(f) = s.get_mut(0..1) {
                f.make_ascii_uppercase();
            }
            format!("{s} stated")
        }
    }
}

/// Sum up a set of rows. Pure, so the tests can feed it rows by hand, and
/// the page can hand back any slice of the cases it was given.
pub fn summarise(rows: &[CaseRow]) -> (Vec<Measure>, Vec<Count>) {
    let measures = MEASURES.iter().map(|m| measure(m.key, m.label, m.definition, rows.iter().filter_map(m.secs))).collect();
    let n = rows.len();
    let mut counts = vec![Count { key: "cases".into(), label: "Cases".into(), n, of: n }];
    counts.extend(COUNTS.iter().map(|c| Count { key: c.key.into(), label: c.label.into(), n: rows.iter().filter(|r| (c.is)(r)).count(), of: n }));
    let reported = rows.iter().filter(|r| r.report.is_some()).count();
    for key in FACT_KEYS {
        counts.push(Count {
            key: fact_key(key),
            label: fact_label(key),
            n: rows.iter().filter(|r| r.facts.iter().any(|f| f == key)).count(),
            of: reported,
        });
    }
    (measures, counts)
}

/// Pipeline latency over the whole window, not only the cases: transcript
/// landing and alert sending, per call and per alert.
pub fn pipeline(c: &Connection, from: i64, to: i64) -> Vec<Measure> {
    let announced = crate::rxhealth::ANNOUNCED_ONLY;
    let transcribe: Vec<i64> = c
        .prepare(&format!(
            "SELECT transcribed_at - (start + CAST(ROUND(secs) AS INTEGER)) FROM calls
              WHERE start BETWEEN ?1 AND ?2 AND transcribed_at > 0 AND secs > 0 AND NOT {announced}
                AND transcript IS NOT NULL AND transcript <> ''"
        ))
        .and_then(|mut q| q.query_map(params![from, to], |r| r.get(0)).map(|rows| rows.flatten().collect()))
        .unwrap_or_default();
    let alerts: Vec<i64> = c
        .prepare(
            "SELECT e.at - MAX(c.start + CAST(ROUND(c.secs) AS INTEGER)) FROM tripwire_events e
               JOIN tripwire_event_calls l ON l.event = e.id JOIN calls c ON c.id = l.call
              WHERE e.status = 'sent' AND e.at BETWEEN ?1 AND ?2 GROUP BY e.id",
        )
        .and_then(|mut q| q.query_map(params![from, to], |r| r.get(0)).map(|rows| rows.flatten().collect()))
        .unwrap_or_default();
    let replies: Vec<i64> = c
        .prepare("SELECT sent_at - at FROM case_notices WHERE sent_at BETWEEN ?1 AND ?2")
        .and_then(|mut q| q.query_map(params![from, to], |r| r.get(0)).map(|rows| rows.flatten().collect()))
        .unwrap_or_default();
    vec![
        measure("all_transcribe", "Call ended → transcript landed", "every transcribed call in the window", transcribe.into_iter()),
        measure("all_alert", "Call ended → tripwire sent", "every tripwire sent in the window, from the end of the last call it fired on", alerts.into_iter()),
        measure("all_reply", "Event heard → case reply sent", "every case reply sent in the window, from the transmission it answered", replies.into_iter()),
    ]
}

pub fn slice(c: &Connection, from: i64, to: i64) -> Slice {
    let announced = crate::rxhealth::ANNOUNCED_ONLY;
    let (calls, secs, transcribed): (i64, f64, i64) = c
        .query_row(
            &format!(
                "SELECT COUNT(*), COALESCE(SUM(secs), 0), COALESCE(SUM(transcript IS NOT NULL AND transcript <> ''), 0)
                   FROM calls WHERE start BETWEEN ?1 AND ?2 AND NOT {announced}"
            ),
            params![from, to],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap_or((0, 0.0, 0));
    let incidents: i64 = c
        .query_row("SELECT COUNT(*) FROM incidents WHERE created BETWEEN ?1 AND ?2", params![from, to], |r| r.get(0))
        .unwrap_or(0);
    let (reports, joined, by_radio, with_facts): (i64, i64, i64, i64) = c
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(incident IS NOT NULL), 0),
                    COALESCE(SUM(incident IS NOT NULL AND link_how LIKE '%learned as%'), 0),
                    COALESCE(SUM(facts IS NOT NULL AND facts <> '' AND facts <> '[]'), 0)
               FROM conversations WHERE first_at BETWEEN ?1 AND ?2 AND source = 'live'",
            params![from, to],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap_or((0, 0, 0, 0));
    let alerts_sent: i64 = c
        .query_row("SELECT COUNT(*) FROM tripwire_events WHERE status = 'sent' AND at BETWEEN ?1 AND ?2", params![from, to], |r| r.get(0))
        .unwrap_or(0);
    Slice {
        calls,
        hours: (secs / 3600.0 * 10.0).round() / 10.0,
        transcribed,
        incidents,
        reports,
        reports_joined: joined,
        reports_joined_by_radio: by_radio,
        reports_with_facts: with_facts,
        alerts_sent,
    }
}

// ---------------------------------------------------------------------------
// keeping the numbers
// ---------------------------------------------------------------------------

fn snapshot(c: &Connection, rows: &[CaseRow], now: i64) {
    for r in rows {
        if let Ok(json) = serde_json::to_string(r) {
            let _ = c.execute(
                "INSERT OR REPLACE INTO research_snapshots (profile, incident, opened, computed, row) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![r.profile, r.incident, r.dispatched.unwrap_or(0), now, json],
            );
        }
    }
}

/// Snapshots of cases in the window that the live build no longer has.
fn recorded(c: &Connection, from: i64, to: i64, live: &[CaseRow]) -> Vec<CaseRow> {
    let have: std::collections::HashSet<(String, i64)> = live.iter().map(|r| (r.profile.clone(), r.incident)).collect();
    c.prepare("SELECT row FROM research_snapshots WHERE opened BETWEEN ?1 AND ?2")
        .and_then(|mut q| {
            q.query_map(params![from, to], |r| r.get::<_, String>(0)).map(|rows| {
                rows.flatten()
                    .filter_map(|j| serde_json::from_str::<CaseRow>(&j).ok())
                    .filter(|r| !have.contains(&(r.profile.clone(), r.incident)))
                    .map(|mut r| {
                        r.recorded = true;
                        // The ED's record may have been typed in since.
                        if let Some((phone, arrived, note)) = record(c, &r.profile, r.incident) {
                            r.phone_at = phone;
                            r.ed_arrived_at = arrived;
                            r.note = note;
                        }
                        r.derive();
                        r
                    })
                    .collect()
            })
        })
        .unwrap_or_default()
}

/// The whole picture over the last `days`.
pub fn compute(c: &Connection, days: u32, places: &crate::places::Settings, now: i64) -> Stats {
    let days = days.clamp(1, 3650);
    let from = now - days as i64 * 86_400;
    let mut rows: Vec<CaseRow> = crate::cases::list_between(c, from, now, places, now).iter().map(|k| row_for(c, k)).collect();
    snapshot(c, &rows, now);
    rows.extend(recorded(c, from, now, &rows));
    rows.sort_by_key(|r| std::cmp::Reverse(r.dispatched.unwrap_or(0)));
    let (measures, counts) = summarise(&rows);
    let mut notes = vec![
        "A moment is the start of the transmission it was said in, on this machine's clock.".to_string(),
        "The crew's call is the radio report joined to the run, unless the ED's phone call has been typed in for that case.".to_string(),
        "Arrival is the chart's time when typed in, else a crew saying they are at the hospital, else the middle of the stated ETA. Crews mostly mark arrival on the MDT, so the last two are thin.".to_string(),
        "A negative lead means the message went out after the crew had already called.".to_string(),
    ];
    let unsent = rows.iter().filter(|r| r.alerted.is_none()).count();
    if unsent > 0 {
        notes.push(format!("{unsent} of {} cases had no message sent, so 'Alert → crew's call' covers only the rest; 'App knew → crew's call' covers every case with a transcribed page.", rows.len()));
    }
    let by_hand: BTreeMap<&str, usize> = rows.iter().fold(BTreeMap::new(), |mut m, r| {
        *m.entry(r.call_how.as_str()).or_default() += 1;
        m
    });
    if by_hand.get("ED record").copied().unwrap_or(0) > 0 {
        notes.push(format!(
            "The crew's call is the ED's logged phone call for {} cases and the radio report for {}.",
            by_hand.get("ED record").copied().unwrap_or(0),
            by_hand.get("radio report").copied().unwrap_or(0)
        ));
    }
    Stats {
        from,
        to: now,
        days,
        library: slice(c, from, now),
        measures,
        pipeline: pipeline(c, from, now),
        counts,
        rows,
        notes,
    }
}

// ---------------------------------------------------------------------------
// export
// ---------------------------------------------------------------------------

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn min1(secs: Option<i64>) -> String {
    secs.map(|s| format!("{:.1}", s as f64 / 60.0)).unwrap_or_default()
}

fn when(t: Option<i64>) -> String {
    t.map(|t| crate::library::local_fmt(t, "%Y-%m-%d %H:%M:%S")).unwrap_or_default()
}

/// One row per case, one column per moment and interval. Moments are given
/// twice, as epoch seconds for software and local time for people.
pub fn csv(rows: &[CaseRow]) -> String {
    let moments: &[(&str, fn(&CaseRow) -> Option<i64>)] = &[
        ("dispatched", |r| r.dispatched),
        ("app_knew", |r| r.known),
        ("alerted", |r| r.alerted),
        ("working", |r| r.working),
        ("rosc", |r| r.rosc),
        ("rearrest", |r| r.rearrest),
        ("transporting", |r| r.transporting),
        ("terminated", |r| r.terminated),
        ("downgraded", |r| r.downgraded),
        ("radio_report", |r| r.report),
        ("ai_report", |r| r.report_generated),
        ("eta_from", |r| r.eta_from),
        ("eta_to", |r| r.eta_to),
        ("arrived_said", |r| r.arrived_said),
        ("ed_phone", |r| r.phone_at),
        ("ed_arrived", |r| r.ed_arrived_at),
    ];
    let intervals: &[(&str, fn(&CaseRow) -> Option<i64>)] = &[
        ("alert_to_call_min", |r| r.alert_to_call),
        ("known_to_call_min", |r| r.known_to_call),
        ("dispatch_to_call_min", |r| r.dispatch_to_call),
        ("call_to_arrival_min", |r| r.call_to_arrival),
        ("alert_to_arrival_min", |r| r.alert_to_arrival),
        ("dispatch_to_arrival_min", |r| r.dispatch_to_arrival),
        ("dispatch_to_working_min", |r| r.dispatch_to_working),
        ("working_to_rosc_min", |r| r.working_to_rosc),
        ("dispatch_to_rosc_min", |r| r.dispatch_to_rosc),
        ("transcribe_min", |r| r.transcribe_secs),
        ("alert_lag_min", |r| r.alert_secs),
        ("dispatch_to_ai_report_min", |r| r.dispatch_to_ai_report),
        ("report_to_ai_report_min", |r| r.report_to_ai_report),
    ];
    let mut head: Vec<String> = vec!["profile", "incident", "title", "state", "address", "units", "recorded"].into_iter().map(String::from).collect();
    for (k, _) in moments {
        head.push(format!("{k}_epoch"));
        head.push(format!("{k}_local"));
    }
    head.extend(["alerted_how", "report_place", "reports", "eta_said", "drive_min", "drive_how", "eta_off_by_min", "call_clock", "arrival_clock"].map(String::from));
    for (k, _) in intervals {
        head.push(k.to_string());
    }
    head.extend(FACT_KEYS.iter().map(|k| fact_key(k)));
    head.extend(FACT_KEYS.iter().map(|k| format!("value_{}", k.replace(' ', "_"))));
    head.extend(["score_name", "score_estimate", "score_lo_pct", "score_hi_pct", "score_met", "score_unknown", "score_assumed", "score_excluded"].map(String::from));
    head.extend(["time", "witnessed", "bystander", "disease"].map(|k| format!("score_{k}")));
    head.extend(["screen_rule", "screen_at_epoch", "screen_at_local", "screen_status", "screen_candidate", "screen_criteria_met", "screen_likelihood_pct", "screen_reason", "screen_fields"].map(String::from));
    head.push("note".into());
    let mut out = head.join(",") + "\n";
    for r in rows {
        let mut f: Vec<String> = vec![
            csv_field(&r.profile),
            r.incident.to_string(),
            csv_field(&r.title),
            csv_field(&r.state),
            csv_field(&r.address),
            r.units.to_string(),
            (r.recorded as u8).to_string(),
        ];
        for (_, g) in moments {
            let t = g(r);
            f.push(t.map(|t| t.to_string()).unwrap_or_default());
            f.push(when(t));
        }
        f.push(csv_field(&r.alerted_how));
        f.push(csv_field(&r.report_place));
        f.push(r.reports.to_string());
        f.push(csv_field(r.eta_said.as_deref().unwrap_or_default()));
        f.push(r.drive_min.map(|m| m.to_string()).unwrap_or_default());
        f.push(csv_field(&r.drive_how));
        f.push(r.off_by_min.map(|m| m.to_string()).unwrap_or_default());
        f.push(csv_field(&r.call_how));
        f.push(csv_field(&r.arrival_how));
        for (_, g) in intervals {
            f.push(min1(g(r)));
        }
        for k in FACT_KEYS {
            f.push(if r.facts.iter().any(|x| x == k) { "1".into() } else { "0".into() });
        }
        for k in FACT_KEYS {
            f.push(csv_field(r.fact_values.get(*k).map(String::as_str).unwrap_or_default()));
        }
        let pct = |x: Option<f64>| x.map(|x| x.to_string()).unwrap_or_default();
        match &r.score {
            Some(s) => {
                f.extend([csv_field(&s.name), csv_field(&s.estimate), pct(s.lo_pct), pct(s.hi_pct), s.met.to_string(), s.unknown.to_string(), s.assumed.to_string(), csv_field(&s.excluded)]);
                f.extend(s.criteria.iter().map(|c| csv_field(&c.verdict)));
            }
            None => f.extend(std::iter::repeat(String::new()).take(12)),
        }
        match &r.screen {
            Some(s) => f.extend([
                csv_field(&s.rule),
                s.at.to_string(),
                when(Some(s.at)),
                csv_field(&s.status),
                csv_field(&s.candidate),
                csv_field(&s.criteria_met),
                csv_field(&s.likelihood_pct),
                csv_field(&s.reason),
                csv_field(&s.fields),
            ]),
            None => f.extend(std::iter::repeat(String::new()).take(9)),
        }
        f.push(csv_field(&r.note));
        out.push_str(&f.join(","));
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

fn db(state: &AppState) -> Result<std::sync::Arc<std::sync::Mutex<Connection>>, String> {
    state.db.lock().unwrap().clone().ok_or_else(|| "the call library is not open".to_string())
}

/// The statistics over the last `days`. Read in a blocking task: it walks
/// every case in the window and every call for the pipeline numbers.
#[tauri::command]
pub async fn research_stats(app: AppHandle, days: u32) -> Result<Stats, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let places = crate::places::load(&app).settings;
        let db = db(&state)?;
        let c = db.lock().unwrap();
        Ok(compute(&c, days, &places, crate::library::now()))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The ED's record for one case: the phone call as logged and the arrival
/// from the chart, epoch seconds, either or both empty, and a note.
#[tauri::command]
pub fn research_set_record(
    state: State<AppState>,
    profile: String,
    incident: i64,
    phone_at: Option<i64>,
    ed_arrived_at: Option<i64>,
    note: Option<String>,
) -> Result<(), String> {
    let db = db(&state)?;
    let c = db.lock().unwrap();
    let note = note.unwrap_or_default();
    if phone_at.is_none() && ed_arrived_at.is_none() && note.trim().is_empty() {
        c.execute("DELETE FROM research_records WHERE profile = ?1 AND incident = ?2", params![profile, incident])
            .map_err(|e| format!("research record: {e}"))?;
        return Ok(());
    }
    c.execute(
        "INSERT INTO research_records (profile, incident, phone_at, ed_arrived_at, note, updated) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(profile, incident) DO UPDATE SET phone_at = excluded.phone_at, ed_arrived_at = excluded.ed_arrived_at,
            note = excluded.note, updated = excluded.updated",
        params![profile, incident, phone_at, ed_arrived_at, note.trim(), crate::library::now()],
    )
    .map_err(|e| format!("research record: {e}"))?;
    Ok(())
}

/// The intervals and counts over each slice of cases the page hands back:
/// a filter, or one group of a breakdown. The arithmetic stays here.
#[derive(Serialize, Clone, Debug, Default)]
pub struct Summary {
    pub measures: Vec<Measure>,
    pub counts: Vec<Count>,
}

#[tauri::command]
pub fn research_summary(groups: Vec<Vec<CaseRow>>) -> Vec<Summary> {
    groups
        .into_iter()
        .map(|mut rows| {
            rows.iter_mut().for_each(CaseRow::derive);
            let (measures, counts) = summarise(&rows);
            Summary { measures, counts }
        })
        .collect()
}

/// Write the cases the page is showing — filtered and sorted as they are
/// there — as CSV to ~/Downloads and say where.
#[tauri::command]
pub fn research_export(mut rows: Vec<CaseRow>) -> Result<String, String> {
    rows.iter_mut().for_each(CaseRow::derive);
    let text = csv(&rows);
    let dir = std::path::PathBuf::from(crate::shellexpand_home("~/Downloads"));
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let stem = format!("research-cases-{}", crate::library::local_fmt(crate::library::now(), "%Y%m%d-%H%M"));
    let mut path = dir.join(format!("{stem}.csv"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{stem}-{n}.csv"));
        n += 1;
    }
    std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(dispatched: i64) -> CaseRow {
        CaseRow { profile: "cardiac-arrest".into(), incident: 1, dispatched: Some(dispatched), ..Default::default() }
    }

    #[test]
    fn quantiles_interpolate() {
        let v = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(quantile(&v, 0.5), 2.5);
        assert_eq!(quantile(&v, 0.25), 1.75);
        assert_eq!(quantile(&v, 0.0), 1.0);
        assert_eq!(quantile(&v, 1.0), 4.0);
        assert_eq!(quantile(&[], 0.5), 0.0);
        assert_eq!(quantile(&[7.0], 0.9), 7.0);
    }

    #[test]
    fn the_lead_runs_from_the_alert_to_the_crews_call() {
        let mut r = row(1000);
        r.known = Some(1030);
        r.alerted = Some(1045);
        r.report = Some(1000 + 14 * 60);
        r.derive();
        assert_eq!(r.call_how, "radio report");
        assert_eq!(r.alert_to_call, Some(14 * 60 - 45));
        assert_eq!(r.known_to_call, Some(14 * 60 - 30));
        assert_eq!(r.dispatch_to_call, Some(14 * 60));
        assert_eq!(r.alert_secs, Some(15));
        // No arrival of any kind: nothing to say.
        assert_eq!(r.call_to_arrival, None);
        assert_eq!(r.arrival_how, "");
    }

    #[test]
    fn the_eds_record_wins_over_the_radio() {
        let mut r = row(1000);
        r.alerted = Some(1100);
        r.report = Some(1600);
        r.phone_at = Some(1900);
        r.eta_from = Some(2200);
        r.eta_to = Some(2400);
        r.derive();
        assert_eq!(r.call_how, "ED record");
        assert_eq!(r.alert_to_call, Some(800));
        assert_eq!(r.arrival_how, "stated ETA");
        assert_eq!(r.call_to_arrival, Some(2300 - 1900));
        r.arrived_said = Some(2500);
        r.derive();
        assert_eq!(r.arrival_how, "said on air");
        r.ed_arrived_at = Some(2600);
        r.derive();
        assert_eq!(r.arrival_how, "ED record");
        assert_eq!(r.alert_to_arrival, Some(1500));
    }

    #[test]
    fn a_late_alert_is_a_negative_lead_and_is_counted() {
        let mut a = row(0);
        a.alerted = Some(600);
        a.report = Some(300);
        a.derive();
        let mut b = row(0);
        b.alerted = Some(60);
        b.report = Some(660);
        b.derive();
        let (m, counts) = summarise(&[a, b]);
        let lead = m.iter().find(|m| m.key == "alert_to_call").unwrap();
        assert_eq!(lead.n, 2);
        assert_eq!(lead.negative, 1);
        assert_eq!(lead.min, -5.0);
        assert_eq!(lead.max, 10.0);
        assert_eq!(lead.median, 2.5);
        let alerted = counts.iter().find(|c| c.key == "alerted").unwrap();
        assert_eq!((alerted.n, alerted.of), (2, 2));
    }

    #[test]
    fn facts_are_counted_over_reported_cases_only() {
        let mut a = row(0);
        a.report = Some(10);
        a.facts = vec!["witnessed".into(), "bystander cpr".into()];
        let b = row(0);
        let (_, counts) = summarise(&[a, b]);
        let w = counts.iter().find(|c| c.key == "fact_witnessed").unwrap();
        assert_eq!((w.n, w.of), (1, 1));
        let cpr = counts.iter().find(|c| c.key == "fact_bystander_cpr").unwrap();
        assert_eq!(cpr.label, "Bystander CPR stated");
    }

    #[test]
    fn a_case_carries_its_tags_and_minutes_for_the_page() {
        let mut r = row(0);
        r.working = Some(60);
        r.rosc = Some(60 + 9 * 60);
        r.report = Some(900);
        r.facts = vec!["bystander cpr".into()];
        r.derive();
        assert!(r.tags.contains(&"rosc".to_string()));
        assert!(r.tags.contains(&"working".to_string()));
        assert!(r.tags.contains(&"fact_bystander_cpr".to_string()));
        assert!(!r.tags.contains(&"alerted".to_string()));
        assert_eq!(r.minutes.get("working_to_rosc"), Some(&9.0));
        assert_eq!(r.minutes.get("dispatch_to_rosc"), Some(&10.0));
        assert_eq!(r.minutes.get("alert_to_call"), None);
        // Every count key a case can carry is one the summary counts.
        let (_, counts) = summarise(&[r.clone()]);
        for t in &r.tags {
            assert!(counts.iter().any(|c| &c.key == t), "{t}");
        }
    }

    #[test]
    fn each_slice_the_page_sends_is_summed_on_its_own() {
        let mut a = row(0);
        a.working = Some(0);
        a.rosc = Some(300);
        let mut b = row(0);
        b.working = Some(0);
        b.rosc = Some(900);
        let c = row(0);
        // Rows as the page hands them back: derived fields may be stale.
        a.tags.clear();
        let s = research_summary(vec![vec![a.clone(), b.clone(), c.clone()], vec![a], vec![]]);
        assert_eq!(s.len(), 3);
        let rosc = |i: usize| s[i].counts.iter().find(|c| c.key == "rosc").map(|c| (c.n, c.of)).unwrap();
        assert_eq!(rosc(0), (2, 3));
        assert_eq!(rosc(1), (1, 1));
        assert_eq!(rosc(2), (0, 0));
        let w2r = s[0].measures.iter().find(|m| m.key == "working_to_rosc").unwrap();
        assert_eq!((w2r.n, w2r.median), (2, 10.0));
    }

    fn parse(line: &str) -> Vec<String> {
        crate::study::parse_csv(line).into_iter().next().unwrap_or_default()
    }

    fn library() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE calls (id INTEGER PRIMARY KEY, start INTEGER, secs REAL, tg INTEGER, tg_name TEXT, unit INTEGER, unit_name TEXT,
               transcript TEXT, transcript_edited TEXT, transcribed_at INTEGER, system TEXT NOT NULL DEFAULT '');",
        )
        .unwrap();
        crate::dispatch::ensure_schema(&c);
        crate::events::ensure_schema(&c);
        crate::casesend::ensure_schema(&c);
        ensure_schema(&c);
        c
    }

    #[test]
    fn the_first_alert_is_the_earliest_of_tripwires_and_case_sends() {
        let c = library();
        c.execute_batch(
            "INSERT INTO incidents (id, created, updated, tg) VALUES (7, 1000, 1000, 1), (8, 1010, 1010, 1);
             INSERT INTO incident_calls (incident, call, at, tg) VALUES (7, 100, 1000, 1);
             INSERT INTO tripwire_events (id, at, source, rule_id, rule_name, status) VALUES
               (1, 1200, 'tripwire', 'a', 'Working arrest', 'sent'),
               (2, 1100, 'tripwire', 'b', 'Arrest chatter', 'quiet'),
               (3, 1150, 'tripwire', 'c', 'On the page', 'sent');
             INSERT INTO tripwire_event_calls (event, call) VALUES (1, 100), (2, 100);
             UPDATE tripwire_events SET incident_id = 8 WHERE id = 3;
             INSERT INTO case_sends (profile, incident, dest, target, root_id, rendered, since, sent_at, updated_at)
               VALUES ('cardiac-arrest', 7, 'd', 't', 1, '', 1000, 1300, 1300);",
        )
        .unwrap();
        // The quiet verdict does not count; the sent one on the merged run does.
        assert_eq!(first_alert(&c, "cardiac-arrest", &[7, 8]), (Some(1150), "tripwire: On the page".into()));
        assert_eq!(first_alert(&c, "cardiac-arrest", &[7]), (Some(1200), "tripwire: Working arrest".into()));
        assert_eq!(first_alert(&c, "cardiac-arrest", &[9]), (None, String::new()));
        c.execute("DELETE FROM tripwire_events", []).unwrap();
        assert_eq!(first_alert(&c, "cardiac-arrest", &[7]), (Some(1300), "case timeline".into()));
    }

    #[test]
    fn pipeline_latency_comes_from_every_call() {
        let c = library();
        c.execute_batch(
            "INSERT INTO calls (id, start, secs, tg, unit, transcript, transcribed_at) VALUES
               (1, 1000, 10, 1, 5, 'hello', 1040),
               (2, 2000, 4.6, 1, 5, 'there', 2035),
               (3, 3000, 0, 1, 0, NULL, NULL);
             INSERT INTO tripwire_events (id, at, source, rule_id, status) VALUES (1, 1100, 'tripwire', 'a', 'sent');
             INSERT INTO tripwire_event_calls (event, call) VALUES (1, 1);",
        )
        .unwrap();
        let p = pipeline(&c, 0, 10_000);
        let t = &p[0];
        assert_eq!(t.n, 2);
        assert_eq!(t.median, 0.5); // 30 s and 30 s
        assert_eq!(p[1].n, 1);
        assert_eq!(p[1].median, 1.5); // 1100 - 1010
        let s = slice(&c, 0, 10_000);
        assert_eq!((s.calls, s.transcribed, s.alerts_sent), (2, 2, 1));
    }

    #[test]
    fn a_record_is_kept_and_cleared() {
        let c = library();
        c.execute(
            "INSERT INTO research_records (profile, incident, phone_at, ed_arrived_at, note, updated) VALUES ('p', 1, 5, NULL, 'n', 1)",
            [],
        )
        .unwrap();
        assert_eq!(record(&c, "p", 1), Some((Some(5), None, "n".into())));
        assert_eq!(record(&c, "p", 2), None);
    }

    /// The whole path: a page, its transcript landing, a tripwire that fired
    /// on it, the crew's report joined to the run, the ED's record typed in.
    #[test]
    fn a_case_built_from_the_library_yields_its_row() {
        let c = library();
        crate::conversations::ensure_schema(&c);
        crate::link::ensure_schema(&c);
        crate::radios::ensure_schema(&c);
        crate::cases::ensure_schema(&c);
        let t0 = 1_700_000_000_i64;
        let report = t0 + 14 * 60;
        c.execute_batch(&format!(
            "INSERT INTO incidents (id, created, updated, tg, call_type, address, address_key, units) VALUES
               (1, {t0}, {t0}, 1, 'Cardiac Arrest', '1200 Example St', '1200 example street', '[\"Engine 5\",\"Medic 7\"]');
             INSERT INTO calls (id, start, secs, tg, unit, transcript, transcribed_at) VALUES
               (1, {t0}, 6, 1, 900900, 'Engine 5, Medic 7, 1200 Example St, Cardiac Arrest Working. 1200 Hours', {known}),
               (2, {rosc}, 3, 2, 900001, 'Medic 7, we have ROSC', {rosc_known}),
               (3, {report}, 20, 9, 900001, 'Medic 7 inbound with a 60 year old male', {report_end});
             INSERT INTO incident_calls (incident, call, at, tg, role) VALUES (1, 1, {t0}, 1, 'dispatch');
             INSERT INTO tripwire_events (id, at, source, rule_id, rule_name, status, incident_id, data) VALUES
               (1, {alert}, 'tripwire', 'w', 'Working arrest', 'sent', 1, '{{\"keywords\":[],\"fields\":null}}'),
               (2, {screened}, 'tripwire', 'e', 'ECPR candidate', 'quiet', NULL,
                '{{\"keywords\":[],\"fields\":{{\"candidate\":\"maybe\",\"criteriaMet\":2,\"likelihoodPct\":5,\"reason\":\"witnessed\"}}}}');
             INSERT INTO tripwire_event_calls (event, call) VALUES (2, 3);
             INSERT INTO conversations (id, rule_id, tg, first_at, last_at, summary, pieces, incident, link_how, facts) VALUES
               (5, 'r', 9, {report}, {report_end}, 'Medic 7 with a 60 year old male in arrest, ROSC, ETA of 5 to 7 minutes.',
                '[{{\"id\":3,\"unit\":900001,\"fixed\":false,\"at\":{report},\"secs\":20.0}}]', 1, 'Medic 7 was sent to this run',
                '[{{\"key\":\"witnessed\",\"value\":\"yes\"}},{{\"key\":\"eta\",\"value\":\"5 to 7 minutes\"}}]');",
            known = t0 + 20,
            rosc = t0 + 9 * 60,
            rosc_known = t0 + 9 * 60 + 15,
            alert = t0 + 35,
            report_end = report + 20,
            screened = report + 30,
        ))
        .unwrap();
        c.execute("UPDATE conversations SET summarized_at = ?1, sent_at = ?2, revision = 1 WHERE id = 5", params![report + 28, report + 90]).unwrap();
        let prof = crate::cases::arrest_profile();
        let tactical: std::collections::HashSet<u16> = [2].into();
        crate::cases::rebuild(&c, &crate::cases::Inputs { profile: &prof, tactical_tgs: &tactical }, t0 - 60, t0 + 3600).unwrap();
        let places = crate::places::Settings::default();
        let s = compute(&c, 30, &places, t0 + 2 * 3600);
        assert_eq!(s.rows.len(), 1, "{:?}", s.rows);
        let r = &s.rows[0];
        assert_eq!(r.incident, 1);
        assert_eq!((r.dispatched, r.known, r.alerted), (Some(t0), Some(t0 + 20), Some(t0 + 35)));
        assert_eq!(r.alerted_how, "tripwire: Working arrest");
        assert_eq!(r.transcribe_secs, Some(14));
        assert_eq!(r.alert_secs, Some(15));
        assert_eq!(r.working, Some(t0), "the page itself said working");
        assert_eq!(r.rosc, Some(t0 + 9 * 60));
        assert_eq!(r.report, Some(report));
        assert_eq!(r.call_how, "radio report");
        assert_eq!(r.alert_to_call, Some(14 * 60 - 35));
        assert_eq!(r.known_to_call, Some(14 * 60 - 20));
        assert_eq!(r.eta_said.as_deref(), Some("5 to 7 minutes"));
        assert_eq!((r.eta_from, r.eta_to), (Some(report + 5 * 60), Some(report + 7 * 60)));
        assert_eq!(r.arrival_how, "stated ETA");
        assert_eq!(r.call_to_arrival, Some(6 * 60));
        assert_eq!(r.facts, vec!["witnessed".to_string(), "eta".to_string()]);
        // The values travel with the keys, and the score is worked from them.
        assert_eq!(r.fact_values.get("witnessed").map(String::as_str), Some("yes"));
        let score = r.score.as_ref().expect("a report, so a score");
        assert_eq!(score.estimate, "0–46% (2 of 4 not stated)");
        // The AI report is the first summary, not the revision's send.
        assert_eq!(r.report_generated, Some(report + 28));
        assert_eq!(r.dispatch_to_ai_report, Some(14 * 60 + 28));
        assert_eq!(r.report_to_ai_report, Some(28));
        // The screen ran on the report's call, not the run's, and is found.
        let screen = r.screen.as_ref().expect("the ECPR screen");
        assert_eq!((screen.rule.as_str(), screen.candidate.as_str(), screen.likelihood_pct.as_str()), ("ECPR candidate", "maybe", "5"));
        assert_eq!(r.calls, vec![1, 2, 3]);
        let lead = s.measures.iter().find(|m| m.key == "alert_to_call").unwrap();
        assert_eq!((lead.n, lead.median), (1, 13.4));
        assert_eq!(s.library.reports_joined, 1);
        assert_eq!(s.library.alerts_sent, 1);
        // The snapshot was filed, and the ED's record changes the clock.
        let n: i64 = c.query_row("SELECT COUNT(*) FROM research_snapshots", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        c.execute(
            "INSERT INTO research_records (profile, incident, phone_at, ed_arrived_at, note, updated) VALUES ('cardiac-arrest', 1, ?1, ?2, 'from the chart', 1)",
            params![report + 120, report + 11 * 60],
        )
        .unwrap();
        let s = compute(&c, 30, &places, t0 + 2 * 3600);
        let r = &s.rows[0];
        assert_eq!(r.call_how, "ED record");
        assert_eq!(r.alert_to_call, Some(14 * 60 + 120 - 35));
        assert_eq!(r.arrival_how, "ED record");
        assert_eq!(r.call_to_arrival, Some(9 * 60));
        // A case gone from the build survives as a snapshot, with the record applied.
        c.execute_batch("DELETE FROM cases; DELETE FROM case_incidents; DELETE FROM case_events;").unwrap();
        let s = compute(&c, 30, &places, t0 + 2 * 3600);
        assert_eq!(s.rows.len(), 1);
        assert!(s.rows[0].recorded);
        assert_eq!(s.rows[0].call_how, "ED record");
        let text = csv(&s.rows);
        assert!(text.lines().nth(1).unwrap().contains("from the chart"));
        assert!(text.lines().nth(1).unwrap().contains("0–46% (2 of 4 not stated)"), "the snapshot keeps the score");
        let (head, row) = (parse(text.lines().next().unwrap()), parse(text.lines().nth(1).unwrap()));
        assert_eq!(head.len(), row.len(), "one value per column");
        let col = |k: &str| row[head.iter().position(|h| h == k).unwrap()].clone();
        assert_eq!(col("value_witnessed"), "yes");
        assert_eq!(col("screen_candidate"), "maybe");
        assert_eq!(col("ai_report_epoch"), (report + 28).to_string());
    }

    #[test]
    fn csv_has_one_column_per_moment_and_quotes_commas() {
        let mut r = row(1000);
        r.address = "1200 Main St, Apt 3".into();
        r.report = Some(1600);
        r.facts = vec!["witnessed".into()];
        r.derive();
        let text = csv(&[r]);
        let mut lines = text.lines();
        let head: Vec<&str> = lines.next().unwrap().split(',').collect();
        let body = lines.next().unwrap();
        assert!(head.contains(&"dispatched_epoch"));
        assert!(head.contains(&"alert_to_call_min"));
        assert!(head.contains(&"fact_bystander_cpr"));
        assert!(body.contains("\"1200 Main St, Apt 3\""));
        assert!(body.contains(",10.0,"), "{body}");
        // The header and the row have the same number of fields.
        let n = body.split(',').count() - 1; // the quoted comma in the address
        assert_eq!(n, head.len());
    }
}
