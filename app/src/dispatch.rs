//! Dispatch: a live incident map built from the calls the radio hears.
//!
//! The listener marks some talkgroups as *dispatch* channels (a call there
//! announces a new incident: units, an address, the nature of the call) and
//! others as *tactical* (fireground and EMS ops traffic that updates an
//! incident already under way). As each call is transcribed, the model pulls
//! out the address, classifies the call type, picks an emoji, and names the
//! units; the address is geocoded through a Nominatim (OpenStreetMap) server
//! and the result is folded into an incident — a new pin on the map, or an
//! update to one that is already there.
//!
//! Grouping, in order: same normalised address → same incident; a geocoded
//! point within `group_radius_m` of an open incident → that incident; a call
//! that names a unit already working an open incident → that incident;
//! otherwise (dispatch channels only) a new incident. "Open" means updated
//! within `group_window_secs`.
//!
//! Everything the model returns is data, never markup: names, addresses and
//! summaries are cleaned like analyzer text, the emoji is checked to be an
//! emoji, and the front end escapes all of it before it meets the DOM. All
//! HTTP (model and geocoder) happens here in Rust, rate-limited to the
//! public Nominatim policy (one request per second, identifying User-Agent).

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::alerts::CallFacts;
use crate::analyzers::{clean_line, clean_text, AnalyzerRule, Field};
use crate::AppState;

// ---------------------------------------------------------------------------
// settings
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Channel {
    pub tg: u16,
    #[serde(default)]
    pub name: String,
    /// `dispatch` — calls here open incidents (address + call type are
    /// extracted); `tactical` — calls here only update incidents that already
    /// involve one of the units heard.
    #[serde(default = "default_role")]
    pub role: String,
    /// When set, every incident from this channel gets this call type instead
    /// of the model's classification (for a channel that carries one kind of
    /// traffic only).
    #[serde(default)]
    pub fixed_call_type: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// One call type the model classifies into, and the emoji used when the
/// model's own pick is missing or is not an emoji.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CallType {
    pub name: String,
    pub emoji: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Settings {
    #[serde(default)]
    pub channels: Vec<Channel>,
    #[serde(default = "default_call_types")]
    pub call_types: Vec<CallType>,
    /// Where the listener is: the map opens here and geocoding is bounded to
    /// `search_radius_km` around it.
    #[serde(default = "default_home_lat")]
    pub home_lat: f64,
    #[serde(default = "default_home_lon")]
    pub home_lon: f64,
    /// Appended to every address before geocoding, e.g. "Indianapolis, IN".
    #[serde(default = "default_region")]
    pub region_hint: String,
    #[serde(default = "default_radius_km")]
    pub search_radius_km: f64,
    /// Nominatim base URL. The public server is fine for one listener; a
    /// self-hosted one lifts the one-per-second limit.
    #[serde(default = "default_geocoder")]
    pub geocoder_url: String,
    /// Contact e-mail sent with each public-Nominatim request (their policy
    /// asks for one). Blank = none.
    #[serde(default)]
    pub geocoder_email: String,
    /// `ollama` | `cloud` — the same engines analyzers use.
    #[serde(default = "default_engine")]
    pub engine: String,
    #[serde(default = "default_group_window")]
    pub group_window_secs: u32,
    #[serde(default = "default_group_radius")]
    pub group_radius_m: u32,
    /// Place a call the geocoder could not, from the grid reference the
    /// dispatcher reads out. Off means an address that will not geocode
    /// simply has no pin.
    #[serde(default = "yes")]
    pub grid_fallback: bool,
    /// What the grid means here, learned from calls that did geocode.
    #[serde(default)]
    pub calibration: crate::addr::Calibration,
    #[serde(default = "default_retention")]
    pub retention_days: u32,
    /// Extra guidance appended to the extraction prompt (local street-naming
    /// quirks, unit naming, what the dispatcher usually says).
    #[serde(default)]
    pub extra_instructions: String,
}

fn default_role() -> String {
    "dispatch".into()
}
fn default_true() -> bool {
    true
}
fn default_home_lat() -> f64 {
    39.7684
}
fn default_home_lon() -> f64 {
    -86.1581
}
fn default_region() -> String {
    "Indianapolis, IN".into()
}
fn default_radius_km() -> f64 {
    40.0
}
fn default_geocoder() -> String {
    "https://nominatim.openstreetmap.org".into()
}
fn default_engine() -> String {
    "ollama".into()
}
fn default_group_window() -> u32 {
    45 * 60
}
fn default_group_radius() -> u32 {
    150
}
fn yes() -> bool {
    true
}

fn default_retention() -> u32 {
    14
}
pub fn default_call_types() -> Vec<CallType> {
    [
        ("Cardiac Arrest", "🫀"),
        ("Chest Pain", "❤️‍🩹"),
        ("Difficulty Breathing", "😮‍💨"),
        ("Stroke/CVA", "🧠"),
        ("Unconscious", "😵"),
        ("Sick Person", "🤒"),
        ("Injured Person", "🤕"),
        ("Overdose", "💊"),
        ("Mental-Emotional", "😰"),
        ("Vehicle Accident", "🚗"),
        ("Structure Fire", "🔥"),
        ("Fire Alarm", "🚨"),
        ("Gas Odor", "⚠️"),
        ("Residence Alarm", "🔔"),
        ("Water Rescue", "🌊"),
        ("Hazmat", "☣️"),
        ("Assault", "👊"),
        ("Unknown", "📍"),
    ]
    .iter()
    .map(|(n, e)| CallType {
        name: (*n).into(),
        emoji: (*e).into(),
    })
    .collect()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            channels: Vec::new(),
            call_types: default_call_types(),
            home_lat: default_home_lat(),
            home_lon: default_home_lon(),
            region_hint: default_region(),
            search_radius_km: default_radius_km(),
            geocoder_url: default_geocoder(),
            geocoder_email: String::new(),
            engine: default_engine(),
            group_window_secs: default_group_window(),
            group_radius_m: default_group_radius(),
            grid_fallback: yes(),
            calibration: crate::addr::Calibration::default(),
            retention_days: default_retention(),
            extra_instructions: String::new(),
        }
    }
}

/// Clamp and clean everything the listener (or the mobile web API) hands us.
pub fn sanitize_settings(s: &mut Settings) -> Result<(), String> {
    s.channels.truncate(256);
    let mut seen = HashSet::new();
    s.channels.retain(|c| seen.insert(c.tg));
    for c in &mut s.channels {
        c.name = clean_line(&c.name, 80);
        c.role = if c.role == "tactical" {
            "tactical".into()
        } else {
            "dispatch".into()
        };
        c.fixed_call_type = clean_line(&c.fixed_call_type, 60);
    }
    s.call_types.truncate(64);
    for t in &mut s.call_types {
        t.name = clean_line(&t.name, 60);
        t.emoji = if is_emoji(t.emoji.trim()) {
            t.emoji.trim().to_string()
        } else {
            "📍".into()
        };
    }
    s.call_types.retain(|t| !t.name.is_empty());
    if s.call_types.is_empty() {
        s.call_types = default_call_types();
    }
    if !s
        .call_types
        .iter()
        .any(|t| t.name.eq_ignore_ascii_case("unknown"))
    {
        s.call_types.push(CallType {
            name: "Unknown".into(),
            emoji: "📍".into(),
        });
    }
    if !(-90.0..=90.0).contains(&s.home_lat) || !(-180.0..=180.0).contains(&s.home_lon) {
        return Err("home latitude/longitude out of range".into());
    }
    s.region_hint = clean_line(&s.region_hint, 120);
    s.search_radius_km = if s.search_radius_km.is_finite() {
        s.search_radius_km.clamp(2.0, 500.0)
    } else {
        default_radius_km()
    };
    s.geocoder_url = clean_line(&s.geocoder_url, 200)
        .trim_end_matches('/')
        .to_string();
    if s.geocoder_url.is_empty() {
        s.geocoder_url = default_geocoder();
    }
    if !(s.geocoder_url.starts_with("https://") || s.geocoder_url.starts_with("http://")) {
        return Err("geocoder URL must start with http:// or https://".into());
    }
    s.geocoder_email = clean_line(&s.geocoder_email, 120);
    if !s
        .geocoder_email
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '-' | '_' | '+'))
    {
        return Err("geocoder e-mail has unexpected characters".into());
    }
    s.engine = if s.engine == "cloud" {
        "cloud".into()
    } else {
        "ollama".into()
    };
    s.group_window_secs = s.group_window_secs.clamp(60, 24 * 3600);
    s.group_radius_m = s.group_radius_m.clamp(10, 5_000);
    s.retention_days = s.retention_days.clamp(1, 365);
    s.extra_instructions = clean_text(&s.extra_instructions, 4_000);
    Ok(())
}

// ---------------------------------------------------------------------------
// state + persistence
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone, Debug)]
pub struct LogEntry {
    pub at: i64,
    pub tg: u16,
    pub tg_name: String,
    pub call: i64,
    /// `new` | `update` | `skip` | `error`
    pub outcome: String,
    pub detail: String,
    pub incident: Option<i64>,
}

#[derive(Default)]
pub struct DispatchState {
    pub settings: Settings,
    pub log: VecDeque<LogEntry>,
    /// Backfill in progress (one at a time).
    pub busy: bool,
}

pub type Shared = Mutex<DispatchState>;

fn path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let d = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("config dir: {e}"))?;
    std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    Ok(d.join("dispatch.json"))
}

pub fn load(app: &AppHandle) -> DispatchState {
    let mut settings: Settings = path(app)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    if sanitize_settings(&mut settings).is_err() {
        settings = Settings::default();
    }
    DispatchState {
        settings,
        ..Default::default()
    }
}

fn store(app: &AppHandle, s: &Settings) -> Result<(), String> {
    let p = path(app)?;
    std::fs::write(
        &p,
        serde_json::to_string_pretty(s).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("{}: {e}", p.display()))
}

/// Tables live in the call library so incidents and calls share one file.
pub fn ensure_schema(c: &Connection) {
    let _ = c.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS incidents (
            id INTEGER PRIMARY KEY,
            created INTEGER NOT NULL,
            updated INTEGER NOT NULL,
            tg INTEGER NOT NULL,
            tg_name TEXT NOT NULL DEFAULT '',
            call_type TEXT NOT NULL DEFAULT 'Unknown',
            emoji TEXT NOT NULL DEFAULT '📍',
            address TEXT NOT NULL DEFAULT '',
            address_key TEXT NOT NULL DEFAULT '',
            validated TEXT NOT NULL DEFAULT '',
            lat REAL,
            lon REAL,
            geocode TEXT NOT NULL DEFAULT '',
            units TEXT NOT NULL DEFAULT '[]',
            summary TEXT NOT NULL DEFAULT '',
            confidence INTEGER NOT NULL DEFAULT 0,
            calls INTEGER NOT NULL DEFAULT 0,
            revision INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS incidents_updated ON incidents(updated);
        CREATE TABLE IF NOT EXISTS incident_calls (
            incident INTEGER NOT NULL,
            call INTEGER NOT NULL,
            at INTEGER NOT NULL,
            tg INTEGER NOT NULL,
            role TEXT NOT NULL DEFAULT '',
            summary TEXT NOT NULL DEFAULT '',
            extracted TEXT NOT NULL DEFAULT '',
            PRIMARY KEY (incident, call)
        );
        CREATE INDEX IF NOT EXISTS incident_calls_call ON incident_calls(call);
        CREATE TABLE IF NOT EXISTS geocache (
            q TEXT PRIMARY KEY,
            lat REAL,
            lon REAL,
            display TEXT NOT NULL DEFAULT '',
            at INTEGER NOT NULL
        );
        "#,
    );
    // Added later: which care pathway a run matched, and where its patient
    // would go. `execute` rather than the batch above so an existing table
    // gains them; the error when they are already there is the expected
    // case and is ignored.
    let _ = c.execute("ALTER TABLE incidents ADD COLUMN pathway TEXT", []);
    let _ = c.execute("ALTER TABLE incidents ADD COLUMN targets TEXT", []);
}

// ---------------------------------------------------------------------------
// incidents
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Incident {
    pub id: i64,
    pub created: i64,
    pub updated: i64,
    pub tg: u16,
    pub tg_name: String,
    pub call_type: String,
    pub emoji: String,
    /// The address as heard (cleaned).
    pub address: String,
    /// The geocoder's display name for it, when found.
    pub validated: String,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    /// `ok` | `none` | `error` | `manual` | `` (no address)
    pub geocode: String,
    pub units: Vec<String>,
    pub summary: String,
    pub confidence: i64,
    pub calls: i64,
    pub revision: i64,
    /// The care pathway this run matched, by name; empty if none did.
    #[serde(default)]
    pub pathway: String,
    /// Where this run's patient would go, worked out once and stored, so
    /// the map, the popup and a Telegram message cannot disagree.
    #[serde(default)]
    pub targets: Vec<crate::pathways::Target>,
}

#[derive(Serialize, Clone, Debug)]
pub struct IncidentCall {
    pub call: i64,
    pub at: i64,
    pub tg: u16,
    pub role: String,
    pub summary: String,
    pub extracted: String,
    pub tg_name: String,
    pub unit_name: Option<String>,
    pub secs: f64,
    pub audio: Option<String>,
    pub transcript: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct IncidentDetail {
    pub incident: Incident,
    pub calls: Vec<IncidentCall>,
    /// Hospital reports joined to this run.
    pub reports: Vec<crate::link::LinkedReport>,
}

const INC_COLS: &str = "id, created, updated, tg, tg_name, call_type, emoji, address, validated, lat, lon, geocode, units, summary, confidence, calls, revision, address_key, pathway, targets";

/// Work out which care pathway a run matches and where its patient would
/// go, store that on the row, and put it on `i` so the map event and the
/// tripwires that follow all see the same numbers.
///
/// Called after the row is written and the database lock is released:
/// resolving a target can route over the network, and holding the library
/// lock across that would stall every other reader for as long as the
/// router takes. Each step takes its own short lock instead.
pub fn apply_pathways(app: &AppHandle, i: &mut Incident) {
    let state = app.state::<AppState>();
    let Some((lat, lon)) = i.lat.zip(i.lon) else {
        // Nowhere to measure from. Anything worked out earlier is stale.
        i.pathway.clear();
        i.targets.clear();
        return;
    };
    let settings = state.pathways.lock().unwrap().clone();
    if !settings.enabled || settings.pathways.is_empty() {
        return;
    }
    let Some(db) = state.db.lock().unwrap().clone() else {
        return;
    };
    // What was actually said on the radio matters: "working arrest" and
    // "CPR in progress" appear in transcripts and nowhere else.
    let transcripts: Vec<String> = {
        let c = db.lock().unwrap();
        c.prepare(
            "SELECT c.transcript FROM incident_calls ic JOIN calls c ON c.id = ic.call
             WHERE ic.incident = ?1 AND c.transcript IS NOT NULL ORDER BY c.start LIMIT 40",
        )
        .and_then(|mut st| {
            st.query_map(params![i.id], |r| r.get::<_, String>(0))
                .map(|rows| rows.filter_map(Result::ok).collect())
        })
        .unwrap_or_default()
    };
    // What the run is, and everything said about it. Exceptions read the
    // first; phrases read the second.
    let about = crate::pathways::haystack(&i.call_type, &i.summary, &[]);
    let text = crate::pathways::haystack(&i.call_type, &i.summary, &transcripts);
    let run = crate::pathways::Run::new(&i.call_type, &about, &text);
    let Some(p) = crate::pathways::first_match(&settings, run) else {
        i.pathway.clear();
        i.targets.clear();
        let _ = store_pathways(&db, i);
        return;
    };
    let places = state.places.lock().unwrap().settings.clone();
    let mut route = |from: (f64, f64), to: (f64, f64)| crate::routing::distance(&state, from, to);
    let targets = crate::pathways::resolve_all(p, &places, (lat, lon), &mut route);
    i.pathway = p.name.clone();
    i.targets = targets;
    let _ = store_pathways(&db, i);
}

fn store_pathways(
    db: &std::sync::Arc<std::sync::Mutex<Connection>>,
    i: &Incident,
) -> Result<(), String> {
    let json = serde_json::to_string(&i.targets).unwrap_or_else(|_| "[]".into());
    let c = db.lock().unwrap();
    c.execute(
        "UPDATE incidents SET pathway = ?1, targets = ?2 WHERE id = ?3",
        params![i.pathway, json, i.id],
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

fn inc_row(r: &rusqlite::Row) -> rusqlite::Result<(Incident, String)> {
    let units: String = r.get(12)?;
    Ok((
        Incident {
            id: r.get(0)?,
            created: r.get(1)?,
            updated: r.get(2)?,
            tg: r.get::<_, i64>(3)? as u16,
            tg_name: r.get(4)?,
            call_type: r.get(5)?,
            emoji: r.get(6)?,
            address: r.get(7)?,
            validated: r.get(8)?,
            lat: r.get(9)?,
            lon: r.get(10)?,
            geocode: r.get(11)?,
            units: serde_json::from_str(&units).unwrap_or_default(),
            summary: r.get(13)?,
            confidence: r.get(14)?,
            calls: r.get(15)?,
            revision: r.get(16)?,
            pathway: r.get::<_, Option<String>>(18)?.unwrap_or_default(),
            targets: r
                .get::<_, Option<String>>(19)?
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_default(),
        },
        r.get(17)?,
    ))
}

pub(crate) fn inc_get(c: &Connection, id: i64) -> Result<Option<Incident>, String> {
    c.query_row(
        &format!("SELECT {INC_COLS} FROM incidents WHERE id = ?1"),
        params![id],
        |r| inc_row(r).map(|(i, _)| i),
    )
    .optional()
    .map_err(|e| format!("incident: {e}"))
}

fn inc_list(c: &Connection, since: i64, limit: u32) -> Result<Vec<Incident>, String> {
    let mut st = c
        .prepare(&format!(
            "SELECT {INC_COLS} FROM incidents WHERE updated >= ?1 ORDER BY updated DESC LIMIT ?2"
        ))
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map(params![since, limit.max(1) as i64], |r| {
            inc_row(r).map(|(i, _)| i)
        })
        .map_err(|e| e.to_string())?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

/// Open incidents (updated inside the grouping window), with their address key.
fn inc_open(c: &Connection, window_secs: u32) -> Result<Vec<(Incident, String)>, String> {
    let since = crate::library::now() - window_secs as i64;
    let mut st = c
        .prepare(&format!(
            "SELECT {INC_COLS} FROM incidents WHERE updated >= ?1 ORDER BY updated DESC LIMIT 500"
        ))
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map(params![since], inc_row)
        .map_err(|e| e.to_string())?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

fn inc_insert(c: &Connection, i: &Incident, key: &str) -> Result<i64, String> {
    c.execute(
        "INSERT INTO incidents (created, updated, tg, tg_name, call_type, emoji, address, address_key, validated, lat, lon, geocode, units, summary, confidence, calls, revision)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
        params![
            i.created,
            i.updated,
            i.tg as i64,
            i.tg_name,
            i.call_type,
            i.emoji,
            i.address,
            key,
            i.validated,
            i.lat,
            i.lon,
            i.geocode,
            serde_json::to_string(&i.units).unwrap_or_else(|_| "[]".into()),
            i.summary,
            i.confidence,
            i.calls,
            i.revision
        ],
    )
    .map_err(|e| format!("insert incident: {e}"))?;
    Ok(c.last_insert_rowid())
}

fn inc_update(c: &Connection, i: &Incident, key: &str) -> Result<(), String> {
    c.execute(
        "UPDATE incidents SET updated=?2, call_type=?3, emoji=?4, address=?5, address_key=?6, validated=?7, lat=?8, lon=?9, geocode=?10, units=?11, summary=?12, confidence=?13, calls=?14, revision=?15 WHERE id=?1",
        params![
            i.id,
            i.updated,
            i.call_type,
            i.emoji,
            i.address,
            key,
            i.validated,
            i.lat,
            i.lon,
            i.geocode,
            serde_json::to_string(&i.units).unwrap_or_else(|_| "[]".into()),
            i.summary,
            i.confidence,
            i.calls,
            i.revision
        ],
    )
    .map_err(|e| format!("update incident: {e}"))?;
    Ok(())
}

fn inc_attach_call(
    c: &Connection,
    incident: i64,
    f: &CallFacts,
    role: &str,
    summary: &str,
    extracted: &str,
) -> Result<(), String> {
    c.execute(
        "INSERT OR IGNORE INTO incident_calls (incident, call, at, tg, role, summary, extracted) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![incident, f.id.unwrap_or(0), f.start, f.tg as i64, role, summary, extracted],
    )
    .map_err(|e| format!("attach call: {e}"))?;
    Ok(())
}

fn call_attached(c: &Connection, call: i64) -> bool {
    c.query_row(
        "SELECT incident FROM incident_calls WHERE call = ?1 LIMIT 1",
        params![call],
        |r| r.get::<_, i64>(0),
    )
    .optional()
    .ok()
    .flatten()
    .is_some()
}

fn prune(c: &Connection, retention_days: u32) {
    let cutoff = crate::library::now() - retention_days as i64 * 86_400;
    let _ = c.execute(
        "DELETE FROM incident_calls WHERE incident IN (SELECT id FROM incidents WHERE updated < ?1)",
        params![cutoff],
    );
    let _ = c.execute("DELETE FROM incidents WHERE updated < ?1", params![cutoff]);
    let _ = c.execute(
        "DELETE FROM geocache WHERE at < ?1",
        params![crate::library::now() - 90 * 86_400],
    );
}

// ---------------------------------------------------------------------------
// text helpers: address keys, emoji, units
// ---------------------------------------------------------------------------

/// Normalise an address so "8241 E. 41st St" and "8241 East 41st Street"
/// land on the same key.
pub fn address_key(addr: &str) -> String {
    let lower = addr.to_lowercase();
    let mut out: Vec<String> = Vec::new();
    for raw in lower.split(|c: char| !c.is_alphanumeric() && c != '&') {
        let w = raw.trim();
        if w.is_empty() {
            continue;
        }
        let w = match w {
            "st" | "str" => "street",
            "ave" | "av" => "avenue",
            "rd" => "road",
            "dr" => "drive",
            "blvd" => "boulevard",
            "ln" => "lane",
            "ct" => "court",
            "pl" => "place",
            "pkwy" => "parkway",
            "hwy" => "highway",
            "cir" => "circle",
            "ter" | "terr" => "terrace",
            "trl" => "trail",
            "n" => "north",
            "s" => "south",
            "e" => "east",
            "w" => "west",
            "&" | "and" | "at" => "&",
            _ => w,
        };
        out.push(w.to_string());
    }
    out.join(" ")
}

/// True when `s` is one emoji (possibly a ZWJ sequence with skin tones and
/// variation selectors) and nothing else — so a marker can never carry text.
pub fn is_emoji(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    if chars.is_empty() || chars.len() > 12 {
        return false;
    }
    let modifier = |c: char| matches!(c, '\u{FE0F}' | '\u{FE0E}' | '\u{200D}' | '\u{20E3}' | '\u{1F3FB}'..='\u{1F3FF}' | '\u{E0020}'..='\u{E007F}');
    let base = |c: char| {
        matches!(
            c,
            '\u{1F000}'..='\u{1FAFF}'
                | '\u{2600}'..='\u{27BF}'
                | '\u{2B00}'..='\u{2BFF}'
                | '\u{2300}'..='\u{23FF}'
                | '\u{2190}'..='\u{21FF}'
                | '\u{25AA}'..='\u{25FE}'
                | '\u{2934}'..='\u{2935}'
                | '\u{3030}'
                | '\u{303D}'
                | '\u{3297}'
                | '\u{3299}'
                | '\u{00A9}'
                | '\u{00AE}'
                | '\u{2122}'
                | '\u{2139}'
                | '\u{24C2}'
        )
    };
    let keycap = chars.len() >= 2
        && (chars[0].is_ascii_digit() || chars[0] == '#' || chars[0] == '*')
        && chars.contains(&'\u{20E3}');
    if keycap {
        return chars.iter().skip(1).all(|&c| modifier(c));
    }
    let bases = chars.iter().filter(|&&c| base(c)).count();
    let zwj = chars.iter().filter(|&&c| c == '\u{200D}').count();
    bases >= 1 && bases <= 4 && zwj <= 3 && chars.iter().all(|&c| base(c) || modifier(c))
}

/// Split the model's unit list ("Medic 42, Engine 6") into tidy callsigns.
fn parse_units(v: &serde_json::Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let push = |s: &str, out: &mut Vec<String>| {
        let u = clean_line(s, 32);
        let u = u.trim_matches(|c: char| !c.is_alphanumeric()).to_string();
        if u.len() >= 2 && !out.iter().any(|x| x.eq_ignore_ascii_case(&u)) {
            out.push(u);
        }
    };
    match v {
        serde_json::Value::Array(a) => {
            for x in a.iter().take(24) {
                if let Some(s) = x.as_str() {
                    push(s, &mut out);
                }
            }
        }
        serde_json::Value::String(s) => {
            for part in s.split([',', ';', '/', '\n']).take(24) {
                push(part, &mut out);
            }
        }
        _ => {}
    }
    out
}

fn unit_key(u: &str) -> String {
    u.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

pub fn haversine_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let r = 6_371_000.0;
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = (lat2 - lat1).to_radians();
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * r * a.sqrt().atan2((1.0 - a).sqrt())
}

// ---------------------------------------------------------------------------
// geocoding
// ---------------------------------------------------------------------------

/// A geocoder answer: `Some((lat, lon, display))`, or `None` for "no match".
pub type Geo = Option<(f64, f64, String)>;

fn last_request() -> &'static Mutex<Option<Instant>> {
    static LAST: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    LAST.get_or_init(|| Mutex::new(None))
}

/// Space public-Nominatim requests at least 1.1 s apart, process-wide.
fn throttle(url: &str) {
    if !url.contains("nominatim.openstreetmap.org") {
        return;
    }
    let mut last = last_request().lock().unwrap();
    if let Some(t) = *last {
        let gap = Duration::from_millis(1100);
        let el = t.elapsed();
        if el < gap {
            std::thread::sleep(gap - el);
        }
    }
    *last = Some(Instant::now());
}

fn cache_get(c: &Connection, q: &str) -> Option<(Geo, i64)> {
    c.query_row(
        "SELECT lat, lon, display, at FROM geocache WHERE q = ?1",
        params![q],
        |r| {
            let lat: Option<f64> = r.get(0)?;
            let lon: Option<f64> = r.get(1)?;
            let display: String = r.get(2)?;
            let at: i64 = r.get(3)?;
            Ok((lat.zip(lon).map(|(a, b)| (a, b, display)), at))
        },
    )
    .optional()
    .ok()
    .flatten()
}

fn cache_put(c: &Connection, q: &str, g: &Geo) {
    let _ = c.execute(
        "INSERT OR REPLACE INTO geocache (q, lat, lon, display, at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            q,
            g.as_ref().map(|x| x.0),
            g.as_ref().map(|x| x.1),
            g.as_ref().map(|x| x.2.clone()).unwrap_or_default(),
            crate::library::now()
        ],
    );
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Ask Nominatim for `address` near home. Bounded to the search box so a
/// mis-heard street cannot land in another state.
fn nominatim(s: &Settings, address: &str) -> Result<Geo, String> {
    let q = if s.region_hint.is_empty() {
        address.to_string()
    } else {
        format!("{address}, {}", s.region_hint)
    };
    // Bounding box: search_radius_km around home (1° lat ≈ 111 km).
    let dlat = s.search_radius_km / 111.0;
    let dlon = s.search_radius_km / (111.0 * s.home_lat.to_radians().cos().abs().max(0.05));
    let mut url = format!(
        "{}/search?format=jsonv2&limit=1&addressdetails=0&countrycodes=us&accept-language=en&viewbox={:.5},{:.5},{:.5},{:.5}&bounded=1&q={}",
        s.geocoder_url,
        s.home_lon - dlon,
        s.home_lat + dlat,
        s.home_lon + dlon,
        s.home_lat - dlat,
        url_encode(&q)
    );
    if !s.geocoder_email.is_empty() {
        url.push_str(&format!("&email={}", url_encode(&s.geocoder_email)));
    }
    throttle(&s.geocoder_url);
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut resp = agent
        .get(&url)
        .header(
            "User-Agent",
            "HoosierSDR/0.1 (+https://github.com/duderayuh/HoosierSDR)",
        )
        .call()
        .map_err(|e| format!("geocoder: {e}"))?;
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().unwrap_or_default();
    if status != 200 {
        return Err(format!(
            "geocoder HTTP {status}: {}",
            text.chars().take(120).collect::<String>()
        ));
    }
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("geocoder reply: {e}"))?;
    let first = v.as_array().and_then(|a| a.first());
    let Some(first) = first else { return Ok(None) };
    let lat = first["lat"]
        .as_str()
        .and_then(|x| x.parse::<f64>().ok())
        .or_else(|| first["lat"].as_f64());
    let lon = first["lon"]
        .as_str()
        .and_then(|x| x.parse::<f64>().ok())
        .or_else(|| first["lon"].as_f64());
    let display = clean_line(first["display_name"].as_str().unwrap_or(""), 200);
    match (lat, lon) {
        (Some(a), Some(b)) if (-90.0..=90.0).contains(&a) && (-180.0..=180.0).contains(&b) => {
            Ok(Some((a, b, display)))
        }
        _ => Ok(None),
    }
}

/// Cached, throttled geocode. Negative answers are cached for a day so one
/// garbled address does not hammer the server every time it is repeated.
fn cache_key(s: &Settings, address_key: &str) -> String {
    format!(
        "{}|{}|{:.2},{:.2}",
        address_key,
        s.region_hint.to_lowercase(),
        s.home_lat,
        s.home_lon
    )
}

/// The shared library connection; locked only around the two cache touches,
/// never across the network request.
pub type Db = std::sync::Arc<Mutex<Connection>>;

pub fn geocode(db: &Db, s: &Settings, address: &str) -> Result<Geo, String> {
    geocode_with(db, s, address, false)
}

/// `retry_negative`: ignore a cached "not found" (a manual retry after the
/// server had a bad day, or after the query normalisation improved).
fn geocode_with(db: &Db, s: &Settings, address: &str, retry_negative: bool) -> Result<Geo, String> {
    // The geocoder sees the address without the apartment / suite / room
    // and without a city the region hint already supplies; the incident
    // keeps the address as heard.
    let address = geocode_query(&clean_line(address, 160), &s.region_hint);
    if address.len() < 4 {
        return Ok(None);
    }
    let key = cache_key(s, &address_key(&address));
    let cached = cache_get(&db.lock().unwrap(), &key);
    if let Some((g, at)) = cached {
        if g.is_some() || (!retry_negative && crate::library::now() - at < 86_400) {
            return Ok(g);
        }
    }
    // An intersection is two streets, not an address: Nominatim returns
    // nothing for "X and Y", so find the node the two ways share instead.
    let g = match intersection_parts(&address) {
        Some((a, b)) => match overpass_intersection(s, &a, &b) {
            Ok(Some(hit)) => Some(hit),
            Ok(None) => nominatim(s, &address)?,
            Err(e) => {
                eprintln!("[dispatch] intersection lookup failed ({e}); trying Nominatim");
                nominatim(s, &address)?
            }
        },
        None => nominatim(s, &address)?,
    };
    cache_put(&db.lock().unwrap(), &key, &g);
    Ok(g)
}

/// Secondary-unit designators a dispatcher reads after the street address
/// ("apartment 604", "Suite 200", "#4"). Nominatim indexes buildings, not
/// units, and returns nothing at all for a query that carries one.
const UNIT_WORDS: &[&str] = &[
    "apartment",
    "apt",
    "suite",
    "ste",
    "unit",
    "room",
    "rm",
    "building",
    "bldg",
    "floor",
    "fl",
    "lot",
    "trailer",
    "space",
    "spc",
];

/// The address as the geocoder wants it: the unit designator and its number
/// dropped ("4350 Madison Avenue, apartment 604" → "4350 Madison Avenue"),
/// and a trailing city / state that `region_hint` supplies anyway removed
/// ("7510 Rogate Drive, Indianapolis" → "7510 Rogate Drive"). A unit word
/// is only stripped when it is followed by something unit-like (a token
/// with a digit, or one or two letters), so "1200 Unit Drive" survives.
pub fn geocode_query(addr: &str, region_hint: &str) -> String {
    let region: Vec<String> = region_hint
        .split(',')
        .map(|p| p.trim().to_lowercase())
        .filter(|p| !p.is_empty())
        .collect();
    let unit_word = |w: &str| UNIT_WORDS.contains(&w.to_lowercase().trim_end_matches('.'));
    let unit_id = |w: &str| {
        let w = w.trim_matches(|c: char| c == '#' || c == '.' || c == ',');
        !w.is_empty()
            && w.len() <= 6
            && w.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && (w.chars().any(|c| c.is_ascii_digit()) || w.len() <= 2)
    };
    let mut parts: Vec<String> = Vec::new();
    for (pi, part) in addr.split(',').enumerate() {
        let words: Vec<&str> = part.split_whitespace().collect();
        if words.is_empty() {
            continue;
        }
        let lower = words.join(" ").to_lowercase();
        // A whole part that is the city / state from the region hint.
        if pi > 0 && region.iter().any(|r| *r == lower) {
            continue;
        }
        let mut kept: Vec<&str> = Vec::new();
        let mut skip = false;
        for (wi, w) in words.iter().enumerate() {
            if skip {
                skip = false;
                continue;
            }
            let next = words.get(wi + 1).copied();
            // "#604" on its own, or "# 604".
            if w.starts_with('#') && !(pi == 0 && wi == 0) {
                if w.len() == 1 && next.is_some_and(unit_id) {
                    skip = true;
                }
                continue;
            }
            // "apartment 604" / "Suite 200" / "Apt B" — never the leading word
            // of the first part, which is the house number or street.
            if !(pi == 0 && wi == 0) && unit_word(w) && next.is_some_and(unit_id) {
                skip = true;
                continue;
            }
            kept.push(w);
        }
        if !kept.is_empty() {
            parts.push(kept.join(" "));
        }
    }
    parts.join(", ")
}

// ---------------------------------------------------------------------------
// intersections
// ---------------------------------------------------------------------------

/// "3200 North 100 East": hundred-block grid coordinates, read after the
/// address on Indianapolis-style dispatches. Not geocodable as written.
pub fn is_grid_ref(s: &str) -> bool {
    let w: Vec<&str> = s
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|t| !t.is_empty())
        .collect();
    let dir_ns = |t: &str| {
        matches!(
            t.to_ascii_lowercase().as_str(),
            "north" | "south" | "n" | "s"
        )
    };
    let dir_ew = |t: &str| matches!(t.to_ascii_lowercase().as_str(), "east" | "west" | "e" | "w");
    let num = |t: &str| !t.is_empty() && t.chars().all(|c| c.is_ascii_digit());
    w.len() == 4
        && num(w[0])
        && num(w[2])
        && ((dir_ns(w[1]) && dir_ew(w[3])) || (dir_ew(w[1]) && dir_ns(w[3])))
}

/// Split "A and B" / "A & B" / "A at B" / "A/B" into two street names, when
/// both halves look like streets (contain a letter; neither is a
/// house-number address).
pub fn intersection_parts(s: &str) -> Option<(String, String)> {
    let lower = s.to_lowercase();
    let (i, w) = [" and ", " & ", " at ", "/"]
        .iter()
        .find_map(|sep| lower.find(sep).map(|i| (i, sep.len())))?;
    let (a, b) = (s[..i].trim(), s[i + w..].trim());
    let streetish = |t: &str| {
        let first = t.split_whitespace().next().unwrap_or("");
        t.chars().any(|c| c.is_alphabetic())
            && t.len() >= 3
            && !(first.len() >= 3 && first.chars().all(|c| c.is_ascii_digit()))
    };
    (streetish(a) && streetish(b)).then(|| (a.to_string(), b.to_string()))
}

const DIRS: &[(&str, &str)] = &[
    ("north", r"(North|N[.]?)"),
    ("south", r"(South|S[.]?)"),
    ("east", r"(East|E[.]?)"),
    ("west", r"(West|W[.]?)"),
    ("n", r"(North|N[.]?)"),
    ("s", r"(South|S[.]?)"),
    ("e", r"(East|E[.]?)"),
    ("w", r"(West|W[.]?)"),
];
const SUFFIXES: &[(&str, &str)] = &[
    ("street", r"(Street|St[.]?)"),
    ("st", r"(Street|St[.]?)"),
    ("avenue", r"(Avenue|Ave[.]?)"),
    ("ave", r"(Avenue|Ave[.]?)"),
    ("road", r"(Road|Rd[.]?)"),
    ("rd", r"(Road|Rd[.]?)"),
    ("drive", r"(Drive|Dr[.]?)"),
    ("dr", r"(Drive|Dr[.]?)"),
    ("boulevard", r"(Boulevard|Blvd[.]?)"),
    ("blvd", r"(Boulevard|Blvd[.]?)"),
    ("lane", r"(Lane|Ln[.]?)"),
    ("ln", r"(Lane|Ln[.]?)"),
    ("court", r"(Court|Ct[.]?)"),
    ("ct", r"(Court|Ct[.]?)"),
    ("place", r"(Place|Pl[.]?)"),
    ("pl", r"(Place|Pl[.]?)"),
    ("parkway", r"(Parkway|Pkwy[.]?)"),
    ("pkwy", r"(Parkway|Pkwy[.]?)"),
    ("highway", r"(Highway|Hwy[.]?)"),
    ("hwy", r"(Highway|Hwy[.]?)"),
    ("circle", r"(Circle|Cir[.]?)"),
    ("cir", r"(Circle|Cir[.]?)"),
    ("terrace", r"(Terrace|Ter[.]?)"),
    ("trail", r"(Trail|Trl[.]?)"),
    ("pike", r"(Pike)"),
    ("way", r"(Way)"),
];
const ANY_SUFFIX: &str = "(Street|St|Avenue|Ave|Road|Rd|Drive|Dr|Boulevard|Blvd|Lane|Ln|Court|Ct|Place|Pl|Parkway|Pkwy|Highway|Hwy|Circle|Cir|Terrace|Ter|Trail|Trl|Pike|Way)[.]?";
const ANY_DIR: &str = "(North|South|East|West|N|S|E|W)[.]?";

/// An OSM-name regex for a street as the dispatcher said it: a directional
/// or suffix matches either its word or its abbreviation, a directional or
/// suffix the dispatcher left out is allowed on the map side, and everything
/// else must match literally (case-insensitively, the whole name). Written
/// without backslashes: Overpass's regex engine is POSIX (no `\s`), and a
/// backslash inside its string literal would need escaping of its own.
pub fn street_name_regex(spoken: &str) -> String {
    let words: Vec<String> = spoken
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|w| !w.is_empty())
        .map(|w| w.trim_matches('.').to_string())
        .collect();
    let n = words.len();
    let (mut has_dir, mut has_suf) = (false, false);
    let mut parts: Vec<String> = Vec::new();
    for (i, w) in words.iter().enumerate() {
        let lw = w.to_ascii_lowercase();
        if i == 0 && n > 1 {
            if let Some((_, re)) = DIRS.iter().find(|(k, _)| *k == lw) {
                parts.push((*re).to_string());
                has_dir = true;
                continue;
            }
        }
        if i == n - 1 && n > 1 {
            if let Some((_, re)) = SUFFIXES.iter().find(|(k, _)| *k == lw) {
                parts.push((*re).to_string());
                has_suf = true;
                continue;
            }
        }
        // Literal word: letters, digits, apostrophe and hyphen only — nothing
        // a regex could misread.
        parts.push(
            w.chars()
                .filter(|c| c.is_alphanumeric() || *c == '\'' || *c == '-')
                .collect(),
        );
    }
    let mut re = String::from("^");
    if !has_dir {
        re.push_str(&format!("({ANY_DIR} )?"));
    }
    re.push_str(&parts.join(" "));
    if !has_suf {
        re.push_str(&format!("( {ANY_SUFFIX})?"));
    }
    re.push('$');
    re
}

/// Public Overpass endpoints, tried in order (the primary has bad days).
const OVERPASS: &[&str] = &[
    "https://overpass-api.de/api/interpreter",
    "https://lz4.overpass-api.de/api/interpreter",
    "https://overpass.kumi.systems/api/interpreter",
];

/// The node where two named highways meet, inside the search box around
/// home. Several nodes (a divided road crossing) are averaged.
fn overpass_intersection(s: &Settings, a: &str, b: &str) -> Result<Geo, String> {
    let dlat = s.search_radius_km / 111.0;
    let dlon = s.search_radius_km / (111.0 * s.home_lat.to_radians().cos().abs().max(0.05));
    let bbox = format!(
        "{:.5},{:.5},{:.5},{:.5}",
        s.home_lat - dlat,
        s.home_lon - dlon,
        s.home_lat + dlat,
        s.home_lon + dlon
    );
    let (ra, rb) = (street_name_regex(a), street_name_regex(b));
    let q = format!(
        "[out:json][timeout:20];way[\"highway\"][\"name\"~\"{ra}\",i]({bbox});node(w)->.a;way[\"highway\"][\"name\"~\"{rb}\",i]({bbox});node(w)->.b;node.a.b;out 8;"
    );
    let body = format!("data={}", url_encode(&q));
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut last_err = String::from("no Overpass endpoint answered");
    for url in OVERPASS {
        // Same courtesy pacing as the public geocoder.
        throttle("nominatim.openstreetmap.org");
        let mut resp = match agent
            .post(*url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("User-Agent", crate::tiles::USER_AGENT)
            .send(body.as_bytes())
        {
            Ok(r) => r,
            Err(e) => {
                last_err = format!("{url}: {e}");
                continue;
            }
        };
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        if status != 200 || !text.trim_start().starts_with('{') {
            last_err = format!("{url}: HTTP {status}");
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                last_err = format!("{url}: {e}");
                continue;
            }
        };
        let nodes: Vec<(f64, f64)> = v["elements"]
            .as_array()
            .map(|els| {
                els.iter()
                    .filter_map(|e| Some((e["lat"].as_f64()?, e["lon"].as_f64()?)))
                    .collect()
            })
            .unwrap_or_default();
        if nodes.is_empty() {
            return Ok(None);
        }
        let n = nodes.len() as f64;
        let lat = nodes.iter().map(|p| p.0).sum::<f64>() / n;
        let lon = nodes.iter().map(|p| p.1).sum::<f64>() / n;
        return Ok(Some((
            lat,
            lon,
            format!("{a} & {b} (intersection, OpenStreetMap)"),
        )));
    }
    Err(last_err)
}

// ---------------------------------------------------------------------------
// extraction
// ---------------------------------------------------------------------------

fn extraction_rule(s: &Settings, ch: &Channel) -> AnalyzerRule {
    let types = s
        .call_types
        .iter()
        .map(|t| t.name.as_str())
        .collect::<Vec<_>>()
        .join(" | ");
    let role = if ch.role == "tactical" {
        "This talkgroup carries tactical / operations traffic for incidents already under way (fireground, EMS ops, unit-to-unit). Report the units speaking and what they say is happening; an address is only present if one is spoken."
    } else {
        "This talkgroup is a DISPATCH channel: the dispatcher tones out units to a new call, giving the units, a street address or intersection, and the nature of the call. Acknowledgements, status checks, radio tests and chatter are NOT dispatches."
    };
    let instructions = format!(
        "{role}\n\nExtract what is actually said. The address is a street address (\"8241 East 41st Street\") or an INTERSECTION of two streets (\"North Delaware Street and East 32nd Street\"), without city or state; write numbers as digits and spell out directions (North/South/East/West). When the dispatcher names two cross streets, the address IS that intersection — write it as \"<street> and <street>\". A trailing \"location NNNN North NNNN East\" (hundred-block coordinates, e.g. \"3200 North 100 East\") is a map GRID reference the dispatcher reads after the address: put it in `grid`, NEVER in `address`. Never invent an address. Units are radio callsigns such as \"Medic 42\", \"Engine 6\", \"Ladder 38\", \"Battalion 4\".\n\nClassify the call as exactly one of: {types}. Use \"Unknown\" when it is not stated. Choose ONE emoji that best pictures the call for a map pin. Confidence is 0-100: how sure you are about the address AND call type from this transcript (which may contain recognition errors).{}",
        if s.extra_instructions.is_empty() {
            String::new()
        } else {
            format!("\n\nLocal notes from the listener: {}", s.extra_instructions)
        }
    );
    let f = |k: &str, kind: &str, d: &str| Field {
        key: k.into(),
        kind: kind.into(),
        desc: d.into(),
    };
    AnalyzerRule {
        id: "dispatch".into(),
        name: "Dispatch".into(),
        enabled: true,
        engine: s.engine.clone(),
        think: false,
        tgs: Vec::new(),
        keywords: Vec::new(),
        instructions,
        fields: vec![
            f("is_dispatch", "bool", "true if this transmission dispatches units to a call (dispatch channel) or reports on an active incident (tactical channel)"),
            f("call_type", "string", "one of the listed call types, or Unknown"),
            f("address", "string", "street address, or the intersection as \"<street> and <street>\", or null"),
            f("cross_street", "string", "cross street or landmark if given, or null"),
            f("grid", "string", "map grid reference like \"3200 North 100 East\" if spoken, else null"),
            f("units", "string", "comma-separated unit callsigns, or empty"),
            f("summary", "string", "one line, what is happening, in plain words"),
            f("emoji", "string", "exactly one emoji for the map pin"),
            f("confidence", "number", "0-100"),
        ],
        match_mode: "all".into(),
        conditions: Vec::new(),
        message: String::new(),
        chat_id: String::new(),
        telegram: false,
        attach_audio: false,
        cooldown_secs: 0,
    }
}

/// What the model said about one call, tidied.
#[derive(Serialize, Clone, Debug, Default)]
pub struct Extracted {
    pub is_dispatch: bool,
    pub call_type: String,
    pub address: String,
    pub cross_street: String,
    /// Hundred-block grid reference ("3200 North 100 East"), when spoken.
    pub grid: String,
    pub units: Vec<String>,
    pub summary: String,
    pub emoji: String,
    pub confidence: i64,
}

fn tidy(s: &Settings, ch: &Channel, obj: &serde_json::Value) -> Extracted {
    let str_of = |k: &str| -> String {
        match obj.get(k) {
            Some(serde_json::Value::String(x)) => clean_line(x, 200),
            Some(serde_json::Value::Number(n)) => n.to_string(),
            _ => String::new(),
        }
    };
    let is_dispatch = match obj.get("is_dispatch") {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::String(x)) => {
            x.eq_ignore_ascii_case("true") || x.eq_ignore_ascii_case("yes")
        }
        _ => false,
    };
    // Call type: the channel's fixed type wins; otherwise the model's answer
    // snapped to the configured list (case-insensitive), else Unknown.
    let raw_type = str_of("call_type");
    let call_type = if !ch.fixed_call_type.is_empty() {
        ch.fixed_call_type.clone()
    } else {
        s.call_types
            .iter()
            .find(|t| t.name.eq_ignore_ascii_case(raw_type.trim()))
            .map(|t| t.name.clone())
            .or_else(|| {
                // Loose match: the model's phrase contains a configured name.
                let lower = raw_type.to_lowercase();
                s.call_types
                    .iter()
                    .filter(|t| !t.name.eq_ignore_ascii_case("unknown"))
                    .find(|t| lower.contains(&t.name.to_lowercase()))
                    .map(|t| t.name.clone())
            })
            .unwrap_or_else(|| "Unknown".into())
    };
    let fallback = s
        .call_types
        .iter()
        .find(|t| t.name.eq_ignore_ascii_case(&call_type))
        .map(|t| t.emoji.clone())
        .unwrap_or_else(|| "📍".into());
    let raw_emoji = str_of("emoji");
    let emoji = if is_emoji(raw_emoji.trim()) {
        raw_emoji.trim().to_string()
    } else {
        fallback
    };
    let mut address = str_of("address");
    if address.eq_ignore_ascii_case("null")
        || address.eq_ignore_ascii_case("none")
        || address.len() < 4
    {
        address.clear();
    }
    let mut grid = str_of("grid");
    if grid.eq_ignore_ascii_case("null") {
        grid.clear();
    }
    let cross = str_of("cross_street");
    // The dispatcher's trailing "3200 North 100 East" is a grid reference,
    // not a place a geocoder knows. If the model put it in the address and
    // gave the cross streets separately, the intersection is the address.
    if is_grid_ref(&address) {
        if grid.is_empty() {
            grid = address.clone();
        }
        address = if intersection_parts(&cross).is_some() {
            cross.clone()
        } else {
            String::new()
        };
    } else if address.is_empty() && intersection_parts(&cross).is_some() {
        address = cross.clone();
    }
    let address = address.replace(" & ", " and ");
    let confidence = match obj.get("confidence") {
        Some(serde_json::Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(serde_json::Value::String(x)) => x.trim().trim_end_matches('%').parse().unwrap_or(0.0),
        _ => 0.0,
    }
    .clamp(0.0, 100.0) as i64;
    Extracted {
        is_dispatch,
        call_type,
        address,
        cross_street: cross,
        grid,
        units: obj.get("units").map(parse_units).unwrap_or_default(),
        summary: clean_line(&str_of("summary"), 300),
        emoji,
        confidence,
    }
}

// ---------------------------------------------------------------------------
// the pipeline
// ---------------------------------------------------------------------------

fn channel_for(s: &Settings, tg: u16) -> Option<Channel> {
    s.channels.iter().find(|c| c.enabled && c.tg == tg).cloned()
}

/// A transcript landed for library call `id`: if it is on a dispatch or
/// tactical channel, extract and fold it into the incidents.
pub fn on_transcript(app: &AppHandle, id: i64, text: &str) {
    let state = app.state::<AppState>();
    let settings = state.dispatch.lock().unwrap().settings.clone();
    if settings.channels.iter().all(|c| !c.enabled) {
        return;
    }
    let Some(db) = state.db.lock().unwrap().clone() else {
        return;
    };
    let row = {
        let c = db.lock().unwrap();
        crate::library::get(&c, id).ok().flatten()
    };
    let Some(r) = row else { return };
    if channel_for(&settings, r.tg).is_none() {
        return;
    }
    let f = crate::alerts::facts_from_row(app, r, Some(text.to_string()));
    let app = app.clone();
    std::thread::spawn(move || {
        let _ = process(&app, &f);
    });
}

/// Extract, geocode, group, store, announce. Returns what happened, for the
/// What the grid reference and the sound of the street name rescued.
pub struct Rescued {
    pub lat: f64,
    pub lon: f64,
    pub validated: String,
    /// `corrected` — a mis-heard street name put right and geocoded;
    /// `grid` — no better than the grid reference, so approximate.
    pub status: &'static str,
    /// The address as it should have been heard.
    pub address: Option<String>,
}

/// How far a corrected address may land from the grid reference before we
/// stop believing the correction.
const CORRECTION_M: f64 = 1_500.0;

/// Place an address the geocoder refused, using the grid reference the
/// dispatcher read out.
///
/// Two rungs. If the street name sounds like one this listener's geocoder
/// has confirmed before, put the name right and geocode that — an exact
/// address, as long as it lands near where the grid says. Otherwise fall
/// back to the grid point itself, which is a few hundred metres out and
/// honest about it.
fn rescue(db: &Db, settings: &Settings, address: &str, grid: &str) -> Option<Rescued> {
    let cal = &settings.calibration;
    let home = (settings.home_lat, settings.home_lon);
    let point = crate::addr::parse_grid(grid)
        .and_then(|g| cal.place_near(g, home, settings.search_radius_km));

    if let Some(heard) = crate::addr::street_of(address) {
        let streets = {
            let c = db.lock().unwrap();
            crate::addr::gazetteer(&c)
        };
        for cand in crate::addr::sound_alike(&streets, &heard, point)
            .into_iter()
            .take(3)
        {
            let fixed = crate::addr::with_street(address, &heard, &cand.name);
            if let Ok(Some((lat, lon, display))) = geocode(db, settings, &fixed) {
                let near_grid = point
                    .map(|(a, b)| haversine_m(a, b, lat, lon) <= CORRECTION_M)
                    .unwrap_or(true);
                if near_grid {
                    return Some(Rescued {
                        lat,
                        lon,
                        validated: display,
                        status: "corrected",
                        address: Some(fixed),
                    });
                }
            }
        }
    }

    let (lat, lon) = point?;
    Some(Rescued {
        lat,
        lon,
        validated: format!("near {} (grid reference)", grid.trim()),
        status: "grid",
        address: None,
    })
}

/// log and for the test command.
pub fn process(app: &AppHandle, f: &CallFacts) -> Result<(String, Option<Incident>), String> {
    let state = app.state::<AppState>();
    let settings = state.dispatch.lock().unwrap().settings.clone();
    let Some(ch) = channel_for(&settings, f.tg) else {
        return Ok(("not a dispatch channel".into(), None));
    };
    let Some(db) = state.db.lock().unwrap().clone() else {
        return Err("library not open".into());
    };
    if let Some(id) = f.id {
        if call_attached(&db.lock().unwrap(), id) {
            return Ok(("already attached".into(), None));
        }
    }
    let text = f.transcript.as_deref().unwrap_or("").trim();
    if text.chars().count() < 8 {
        return Ok(("transcript too short".into(), None));
    }
    let rule = extraction_rule(&settings, &ch);
    let obj = match crate::analyzers::run_extract(&state, &rule, f) {
        Ok(o) => o,
        Err(e) => {
            log_it(app, f, "error", format!("extraction failed: {e}"), None);
            return Err(e);
        }
    };
    let mut x = tidy(&settings, &ch, &obj);
    let extracted = serde_json::to_string(&x).unwrap_or_default();
    if !x.is_dispatch && x.address.is_empty() && x.units.is_empty() {
        log_it(app, f, "skip", "not a dispatch".into(), None);
        return Ok(("not a dispatch".into(), None));
    }
    // Geocode outside the database lock: it can take a second.
    // Geocode (cached, throttled); the connection lock is not held across
    // the network call.
    let geo: Result<Geo, String> = if x.address.is_empty() {
        Ok(None)
    } else {
        geocode(&db, &settings, &x.address)
    };
    let (mut lat, mut lon, mut validated, mut status) = match &geo {
        Ok(Some((a, b, d))) => (Some(*a), Some(*b), d.clone(), "ok"),
        Ok(None) if x.address.is_empty() => (None, None, String::new(), ""),
        Ok(None) => (None, None, String::new(), "none"),
        Err(_) => (None, None, String::new(), "error"),
    };
    // The address did not resolve. The dispatcher read out a grid reference
    // after it; see what that can do.
    if lat.is_none() && !x.address.is_empty() && settings.grid_fallback {
        if let Some(r) = rescue(&db, &settings, &x.address, &x.grid) {
            lat = Some(r.lat);
            lon = Some(r.lon);
            validated = r.validated;
            status = r.status;
            // The address as it should have been heard, so the card, the
            // grouping key and any later search all agree.
            if let Some(fixed) = r.address {
                x.address = fixed;
            }
        }
    }
    let (lat, lon, validated) = (lat, lon, validated);
    let key = address_key(&x.address);

    let now = crate::library::now();
    let c = db.lock().unwrap();
    let open = inc_open(&c, settings.group_window_secs)?;
    let unit_keys: Vec<String> = x.units.iter().map(|u| unit_key(u)).collect();
    let (target, how) = pick_target(
        &open,
        &ch.role,
        &key,
        lat,
        lon,
        &x.call_type,
        &unit_keys,
        settings.group_radius_m as f64,
        now,
    );
    let role = ch.role.clone();
    match target {
        Some(mut i) => {
            i.updated = now;
            i.calls += 1;
            i.revision += 1;
            for u in &x.units {
                if !i.units.iter().any(|v| unit_key(v) == unit_key(u)) {
                    i.units.push(u.clone());
                }
            }
            i.units.truncate(40);
            if !x.summary.is_empty() {
                i.summary = x.summary.clone();
            }
            let unknown = i.call_type.eq_ignore_ascii_case("unknown");
            if unknown && !x.call_type.eq_ignore_ascii_case("unknown") {
                i.call_type = x.call_type.clone();
                i.emoji = x.emoji.clone();
            } else if role == "dispatch"
                && x.confidence > i.confidence
                && !x.call_type.eq_ignore_ascii_case("unknown")
            {
                i.call_type = x.call_type.clone();
                i.emoji = x.emoji.clone();
            }
            i.confidence = i.confidence.max(x.confidence);
            let mut ikey = address_key(&i.address);
            if i.address.is_empty() && !x.address.is_empty() {
                i.address = x.address.clone();
                ikey = key.clone();
                if lat.is_some() {
                    i.lat = lat;
                    i.lon = lon;
                    i.validated = validated.clone();
                    i.geocode = status.into();
                } else {
                    i.geocode = status.into();
                }
            } else if i.lat.is_none() && lat.is_some() && (i.geocode != "manual") {
                i.lat = lat;
                i.lon = lon;
                i.validated = validated.clone();
                i.geocode = status.into();
            }
            inc_update(&c, &i, &ikey)?;
            inc_attach_call(&c, i.id, f, &role, &x.summary, &extracted)?;
            drop(c);
            apply_pathways(app, &mut i);
            let _ = app.emit("incident", &i);
            crate::tripwires::on_incident(app, &i, false);
            log_it(app, f, "update", format!("{how} → #{}", i.id), Some(i.id));
            Ok((format!("updated incident #{} ({how})", i.id), Some(i)))
        }
        None => {
            if role != "dispatch" {
                drop(c);
                log_it(
                    app,
                    f,
                    "skip",
                    "tactical call, no open incident with these units".into(),
                    None,
                );
                return Ok(("no matching incident".into(), None));
            }
            if !x.is_dispatch && x.address.is_empty() {
                drop(c);
                log_it(app, f, "skip", "not a dispatch".into(), None);
                return Ok(("not a dispatch".into(), None));
            }
            let mut i = Incident {
                id: 0,
                created: f.start.max(1),
                updated: now,
                tg: f.tg,
                tg_name: f.tg_name.clone(),
                call_type: x.call_type.clone(),
                emoji: x.emoji.clone(),
                address: x.address.clone(),
                validated,
                lat,
                lon,
                geocode: status.into(),
                units: x.units.clone(),
                summary: x.summary.clone(),
                confidence: x.confidence,
                calls: 1,
                revision: 0,
                pathway: String::new(),
                targets: Vec::new(),
            };
            i.id = inc_insert(&c, &i, &key)?;
            inc_attach_call(&c, i.id, f, &role, &x.summary, &extracted)?;
            prune(&c, settings.retention_days);
            drop(c);
            apply_pathways(app, &mut i);
            let _ = app.emit("incident", &i);
            crate::tripwires::on_incident(app, &i, false);
            log_it(
                app,
                f,
                "new",
                format!(
                    "#{} {} · {}",
                    i.id,
                    i.call_type,
                    if i.address.is_empty() {
                        "no address"
                    } else {
                        &i.address
                    }
                ),
                Some(i.id),
            );
            Ok((format!("new incident #{}", i.id), Some(i)))
        }
    }
}

/// Which open incident a call belongs to, and why — or `None` for a new one.
///
/// Dispatch calls are separated by **address and time**: a dispatch merges
/// only into an open incident at the same normalised address, or one whose
/// geocoded point is within `radius_m` and whose call type agrees (or is
/// still Unknown). Sharing a unit is never enough: the same engine or medic
/// is toned out to many different calls inside one grouping window, and
/// merging on the unit strung every kind of call together. The one
/// exception is a dispatch-channel call that names a unit but carries no
/// address at all (a cancel, a "disregard", a status back to the
/// dispatcher): that attaches to the unit's most recent incident from the
/// last `FOLLOW_UP_SECS`. Tactical calls (fireground / EMS ops) attach by
/// address first, then by unit, since that traffic rarely repeats the
/// address.
#[allow(clippy::too_many_arguments)]
fn pick_target(
    open: &[(Incident, String)],
    role: &str,
    key: &str,
    lat: Option<f64>,
    lon: Option<f64>,
    call_type: &str,
    unit_keys: &[String],
    radius_m: f64,
    now: i64,
) -> (Option<Incident>, &'static str) {
    const FOLLOW_UP_SECS: i64 = 10 * 60;
    let unknown = |t: &str| t.eq_ignore_ascii_case("unknown");
    if !key.is_empty() {
        if let Some((i, _)) = open.iter().find(|(_, k)| k == key) {
            return (Some(i.clone()), "same address");
        }
    }
    if let (Some(a), Some(b)) = (lat, lon) {
        let mut best: Option<(f64, &Incident)> = None;
        for (i, _) in open {
            if let (Some(ia), Some(ib)) = (i.lat, i.lon) {
                let types_agree = unknown(call_type)
                    || unknown(&i.call_type)
                    || i.call_type.eq_ignore_ascii_case(call_type);
                let d = haversine_m(a, b, ia, ib);
                if types_agree && d <= radius_m && best.map_or(true, |(bd, _)| d < bd) {
                    best = Some((d, i));
                }
            }
        }
        if let Some((_, i)) = best {
            return (Some(i.clone()), "nearby");
        }
    }
    if unit_keys.is_empty() {
        return (None, "");
    }
    let shares_unit = |i: &Incident| i.units.iter().any(|u| unit_keys.contains(&unit_key(u)));
    if role == "tactical" {
        if let Some((i, _)) = open.iter().find(|(i, _)| shares_unit(i)) {
            return (Some(i.clone()), "same unit");
        }
        return (None, "");
    }
    // Dispatch channel, no address heard: a follow-up to the unit's latest call.
    if key.is_empty() {
        if let Some((i, _)) = open
            .iter()
            .filter(|(i, _)| shares_unit(i) && now - i.updated <= FOLLOW_UP_SECS)
            .max_by_key(|(i, _)| i.updated)
        {
            return (Some(i.clone()), "unit follow-up");
        }
    }
    (None, "")
}

fn log_it(app: &AppHandle, f: &CallFacts, outcome: &str, detail: String, incident: Option<i64>) {
    let state = app.state::<AppState>();
    let mut st = state.dispatch.lock().unwrap();
    st.log.push_front(LogEntry {
        at: crate::library::now(),
        tg: f.tg,
        tg_name: f.tg_name.clone(),
        call: f.id.unwrap_or(0),
        outcome: outcome.into(),
        detail,
        incident,
    });
    st.log.truncate(300);
    let _ = app.emit("dispatch", ());
}

fn facts_of(app: &AppHandle, r: crate::library::CallRow) -> Option<CallFacts> {
    let text = r
        .transcript_edited
        .clone()
        .or_else(|| r.transcript.clone())
        .filter(|t| !t.trim().is_empty())?;
    Some(crate::alerts::facts_from_row(app, r, Some(text)))
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn dispatch_get(state: State<AppState>) -> Settings {
    state.dispatch.lock().unwrap().settings.clone()
}

#[tauri::command]
pub fn dispatch_set(
    app: AppHandle,
    state: State<AppState>,
    settings: Settings,
) -> Result<(), String> {
    let mut settings = settings;
    sanitize_settings(&mut settings)?;
    let mut st = state.dispatch.lock().unwrap();
    st.settings = settings;
    store(&app, &st.settings)
}

#[tauri::command]
pub fn dispatch_log(state: State<AppState>) -> Vec<LogEntry> {
    state.dispatch.lock().unwrap().log.iter().cloned().collect()
}

/// Incidents updated since `since` (epoch seconds; 0 = everything kept),
/// newest first.
#[tauri::command]
pub fn incidents_list(
    state: State<AppState>,
    since: i64,
    limit: Option<u32>,
) -> Result<Vec<Incident>, String> {
    let Some(db) = state.db.lock().unwrap().clone() else {
        return Ok(Vec::new());
    };
    let c = db.lock().unwrap();
    inc_list(&c, since.max(0), limit.unwrap_or(500).min(5_000))
}

#[tauri::command]
pub fn incident_get(state: State<AppState>, id: i64) -> Result<Option<IncidentDetail>, String> {
    let Some(db) = state.db.lock().unwrap().clone() else {
        return Ok(None);
    };
    let c = db.lock().unwrap();
    let Some(incident) = inc_get(&c, id)? else {
        return Ok(None);
    };
    let mut st = c
        .prepare("SELECT call, at, tg, role, summary, extracted FROM incident_calls WHERE incident = ?1 ORDER BY at ASC")
        .map_err(|e| e.to_string())?;
    let rows: Vec<(i64, i64, i64, String, String, String)> = st
        .query_map(params![id], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })
        .map_err(|e| e.to_string())?
        .filter_map(|r| r.ok())
        .collect();
    let mut calls = Vec::new();
    for (call, at, tg, role, summary, extracted) in rows {
        let row = crate::library::get(&c, call).ok().flatten();
        calls.push(IncidentCall {
            call,
            at,
            tg: tg as u16,
            role,
            summary,
            extracted,
            tg_name: row.as_ref().map(|r| r.tg_name.clone()).unwrap_or_default(),
            unit_name: row.as_ref().and_then(|r| r.unit_name.clone()),
            secs: row.as_ref().map(|r| r.secs).unwrap_or(0.0),
            audio: row.as_ref().and_then(|r| r.audio.clone()),
            transcript: row.and_then(|r| r.transcript_edited.or(r.transcript)),
        });
    }
    // The hospital reports this run turned into, if a crew was heard
    // reading one.
    let places = state.places.lock().unwrap().settings.clone();
    let reports = crate::link::reports_for(&c, id, &places);
    Ok(Some(IncidentDetail {
        incident,
        calls,
        reports,
    }))
}

#[tauri::command]
pub fn incident_delete(app: AppHandle, state: State<AppState>, id: i64) -> Result<(), String> {
    let Some(db) = state.db.lock().unwrap().clone() else {
        return Err("library not open".into());
    };
    let c = db.lock().unwrap();
    c.execute(
        "DELETE FROM incident_calls WHERE incident = ?1",
        params![id],
    )
    .map_err(|e| e.to_string())?;
    c.execute("DELETE FROM incidents WHERE id = ?1", params![id])
        .map_err(|e| e.to_string())?;
    let _ = app.emit("incident_deleted", id);
    Ok(())
}

/// Fix an incident's location by hand: a corrected address (re-geocoded),
/// or explicit coordinates (e.g. picked on the map).
#[tauri::command]
pub async fn incident_locate(
    app: AppHandle,
    state: State<'_, AppState>,
    id: i64,
    address: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
) -> Result<Incident, String> {
    let settings = state.dispatch.lock().unwrap().settings.clone();
    let Some(db) = state.db.lock().unwrap().clone() else {
        return Err("library not open".into());
    };
    // Off the main thread: a re-geocode can wait on the public server.
    tauri::async_runtime::spawn_blocking(move || {
        locate_blocking(&app, &db, &settings, id, address, lat, lon)
    })
    .await
    .map_err(|e| e.to_string())?
}

fn locate_blocking(
    app: &AppHandle,
    db: &Db,
    settings: &Settings,
    id: i64,
    address: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
) -> Result<Incident, String> {
    let mut i = {
        let c = db.lock().unwrap();
        inc_get(&c, id)?.ok_or("no such incident")?
    };
    if let Some(a) = address
        .map(|a| clean_line(&a, 160))
        .filter(|a| !a.is_empty())
    {
        i.address = a;
    }
    match (lat, lon) {
        (Some(a), Some(b)) => {
            if !(-90.0..=90.0).contains(&a) || !(-180.0..=180.0).contains(&b) {
                return Err("coordinates out of range".into());
            }
            i.lat = Some(a);
            i.lon = Some(b);
            i.validated = String::new();
            i.geocode = "manual".into();
        }
        _ => {
            let g = geocode(db, settings, &i.address)?;
            match g {
                Some((a, b, d)) => {
                    i.lat = Some(a);
                    i.lon = Some(b);
                    i.validated = d;
                    i.geocode = "ok".into();
                }
                None => {
                    i.geocode = "none".into();
                }
            }
        }
    }
    i.revision += 1;
    let key = address_key(&i.address);
    {
        let c = db.lock().unwrap();
        inc_update(&c, &i, &key)?;
    }
    let _ = app.emit("incident", &i);
    Ok(i)
}

/// Incidents the geocoder could not place, newest first.
fn inc_unmapped(c: &Connection, limit: u32) -> Result<Vec<Incident>, String> {
    let mut st = c
        .prepare(&format!(
            // 'grid' is in here too: an approximate pin is a candidate for
            // an exact one, so a later retry can upgrade it.
            "SELECT {INC_COLS} FROM incidents WHERE geocode IN ('none', 'error', 'grid') AND address <> '' ORDER BY created DESC LIMIT ?1"
        ))
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map(params![limit.max(1) as i64], |r| inc_row(r).map(|(i, _)| i))
        .map_err(|e| e.to_string())?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

/// Try the geocoder again on every unmapped incident (the query
/// normalisation may have improved, or the server was down). Returns
/// (tried, placed). One request per second against the public server, so
/// this runs off the main thread and can take a minute.
#[tauri::command]
pub async fn dispatch_regeocode(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(u32, u32), String> {
    let settings = state.dispatch.lock().unwrap().settings.clone();
    let Some(db) = state.db.lock().unwrap().clone() else {
        return Err("library not open".into());
    };
    tauri::async_runtime::spawn_blocking(move || regeocode_blocking(&app, &db, &settings))
        .await
        .map_err(|e| e.to_string())?
}

/// Re-fit the grid from everything the geocoder has confirmed so far and
/// remember it. Cheap, and the fit only improves as the library grows.
pub fn refresh_calibration(app: &AppHandle, db: &Db) -> Option<crate::addr::Calibration> {
    let cal = {
        let c = db.lock().unwrap();
        crate::addr::calibrate(&c)?
    };
    let state = app.state::<AppState>();
    let settings = {
        let mut st = state.dispatch.lock().unwrap();
        st.settings.calibration = cal.clone();
        st.settings.clone()
    };
    let _ = store(app, &settings);
    Some(cal)
}

/// The grid reference a call carried, for an incident already stored.
fn grid_of(c: &Connection, incident: i64) -> String {
    c.query_row(
        "SELECT json_extract(extracted, '$.grid') FROM incident_calls
          WHERE incident = ?1 AND json_extract(extracted, '$.grid') <> '' LIMIT 1",
        [incident],
        |r| r.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()
    .unwrap_or_default()
}

fn regeocode_blocking(app: &AppHandle, db: &Db, settings: &Settings) -> Result<(u32, u32), String> {
    // Fit the grid first: the rescue below leans on it.
    let mut settings = settings.clone();
    if let Some(cal) = refresh_calibration(app, db) {
        settings.calibration = cal;
    }
    let settings = &settings;
    let todo = {
        let c = db.lock().unwrap();
        inc_unmapped(&c, 500)?
    };
    let (mut tried, mut placed, mut upgraded) = (0u32, 0u32, 0u32);
    for mut i in todo {
        // An approximate pin is only worth retrying for an exact answer.
        let was_grid = i.geocode == "grid";
        // A hundred-block grid reference is not a place; Indiana county
        // roads are named like one ("N 100 E"), so never offer it.
        if is_grid_ref(&i.address) {
            continue;
        }
        tried += 1;
        match geocode_with(db, settings, &i.address, true) {
            Ok(Some((a, b, d))) => {
                i.lat = Some(a);
                i.lon = Some(b);
                i.validated = d;
                i.geocode = "ok".into();
                i.revision += 1;
                let key = address_key(&i.address);
                {
                    let c = db.lock().unwrap();
                    inc_update(&c, &i, &key)?;
                }
                let _ = app.emit("incident", &i);
                if was_grid {
                    upgraded += 1;
                } else {
                    placed += 1;
                }
            }
            Ok(None) | Err(_) if settings.grid_fallback && !was_grid => {
                let grid = {
                    let c = db.lock().unwrap();
                    grid_of(&c, i.id)
                };
                if let Some(r) = rescue(db, settings, &i.address, &grid) {
                    i.lat = Some(r.lat);
                    i.lon = Some(r.lon);
                    i.validated = r.validated;
                    i.geocode = r.status.into();
                    i.revision += 1;
                    if let Some(fixed) = r.address {
                        i.address = fixed;
                    }
                    let key = address_key(&i.address);
                    {
                        let c = db.lock().unwrap();
                        inc_update(&c, &i, &key)?;
                    }
                    let _ = app.emit("incident", &i);
                    placed += 1;
                }
            }
            Ok(None) => {}
            Err(e) => eprintln!("[dispatch] retry geocode #{}: {e}", i.id),
        }
    }
    Ok((tried, placed + upgraded))
}

/// Re-fit the grid on demand, for the Setup panel's button.
#[tauri::command]
pub fn dispatch_calibrate(
    app: AppHandle,
    state: State<AppState>,
) -> Result<crate::addr::Calibration, String> {
    let Some(db) = state.db.lock().unwrap().clone() else {
        return Err("library not open".into());
    };
    refresh_calibration(&app, &db)
        .ok_or_else(|| "not enough placed calls with a grid reference yet".into())
}

/// Geocode a free-text query (the Fix-address dialog's preview).
#[tauri::command]
pub async fn dispatch_geocode(state: State<'_, AppState>, q: String) -> Result<Geo, String> {
    let settings = state.dispatch.lock().unwrap().settings.clone();
    let Some(db) = state.db.lock().unwrap().clone() else {
        return Err("library not open".into());
    };
    tauri::async_runtime::spawn_blocking(move || geocode(&db, &settings, &q))
        .await
        .map_err(|e| e.to_string())?
}

/// Run the extractor on the newest transcribed call on a dispatch/tactical
/// channel (or the given talkgroup) and report what the model returned and
/// what the pipeline did with it. This does create/update incidents — it is
/// the quickest way to see the map light up from calls already recorded.
#[tauri::command]
pub async fn dispatch_test(
    app: AppHandle,
    state: State<'_, AppState>,
    tg: Option<u16>,
) -> Result<String, String> {
    let settings = state.dispatch.lock().unwrap().settings.clone();
    let tgs: Vec<u16> = match tg {
        Some(t) => vec![t],
        None => settings
            .channels
            .iter()
            .filter(|c| c.enabled)
            .map(|c| c.tg)
            .collect(),
    };
    if tgs.is_empty() {
        return Err("no dispatch channels configured".into());
    }
    let Some(db) = state.db.lock().unwrap().clone() else {
        return Err("no call library open".into());
    };
    let mut calls: Vec<crate::library::CallRow> = Vec::new();
    {
        let c = db.lock().unwrap();
        for t in tgs {
            if let Ok(rows) = crate::library::search(
                &c,
                &crate::library::Query {
                    tg: Some(t),
                    limit: Some(30),
                    ..Default::default()
                },
            ) {
                calls.extend(rows);
            }
        }
    }
    calls.sort_by_key(|c| std::cmp::Reverse(c.start));
    let f = calls.into_iter().find_map(|r| facts_of(&app, r));
    let Some(f) = f else {
        return Ok("no transcribed call found on those talkgroups yet".into());
    };
    let app2 = app.clone();
    let res = tauri::async_runtime::spawn_blocking(move || process(&app2, &f))
        .await
        .map_err(|e| e.to_string())?;
    match res {
        Ok((what, Some(i))) => Ok(format!(
            "{what}\n\n{} {} · {}\nUnits: {}\nLocation: {}{}\nSummary: {}",
            i.emoji,
            i.call_type,
            if i.address.is_empty() {
                "no address heard"
            } else {
                &i.address
            },
            if i.units.is_empty() {
                "—".into()
            } else {
                i.units.join(", ")
            },
            match (i.lat, i.lon) {
                (Some(a), Some(b)) => format!("{a:.5}, {b:.5}"),
                _ => format!(
                    "not geocoded ({})",
                    if i.geocode.is_empty() {
                        "no address"
                    } else {
                        &i.geocode
                    }
                ),
            },
            if i.validated.is_empty() {
                String::new()
            } else {
                format!(" — {}", i.validated)
            },
            i.summary
        )),
        Ok((what, None)) => Ok(what),
        Err(e) => Err(e),
    }
}

/// Fold every transcribed call from the last `hours` on the configured
/// channels into incidents, oldest first, in the background. Progress and
/// completion arrive as `dispatch` events; returns how many calls were queued.
#[tauri::command]
pub fn dispatch_backfill(
    app: AppHandle,
    state: State<AppState>,
    hours: u32,
) -> Result<usize, String> {
    let settings = state.dispatch.lock().unwrap().settings.clone();
    let tgs: Vec<u16> = settings
        .channels
        .iter()
        .filter(|c| c.enabled)
        .map(|c| c.tg)
        .collect();
    if tgs.is_empty() {
        return Err("no dispatch channels configured".into());
    }
    {
        let mut st = state.dispatch.lock().unwrap();
        if st.busy {
            return Err("a backfill is already running".into());
        }
        st.busy = true;
    }
    let Some(db) = state.db.lock().unwrap().clone() else {
        state.dispatch.lock().unwrap().busy = false;
        return Err("no call library open".into());
    };
    let since = crate::library::now() - hours.clamp(1, 24 * 30) as i64 * 3600;
    let mut calls: Vec<crate::library::CallRow> = Vec::new();
    {
        let c = db.lock().unwrap();
        for t in &tgs {
            if let Ok(rows) = crate::library::search(
                &c,
                &crate::library::Query {
                    tg: Some(*t),
                    from: Some(since),
                    limit: Some(2_000),
                    ..Default::default()
                },
            ) {
                calls.extend(rows);
            }
        }
        calls.retain(|r| !call_attached(&c, r.id));
    }
    calls.sort_by_key(|c| c.start);
    let facts: Vec<CallFacts> = calls.into_iter().filter_map(|r| facts_of(&app, r)).collect();
    let n = facts.len();
    let app2 = app.clone();
    std::thread::spawn(move || {
        for (k, f) in facts.iter().enumerate() {
            let _ = process(&app2, f);
            let _ = app2.emit(
                "dispatch_progress",
                serde_json::json!({ "done": k + 1, "total": n }),
            );
        }
        app2.state::<AppState>().dispatch.lock().unwrap().busy = false;
        let _ = app2.emit(
            "dispatch_progress",
            serde_json::json!({ "done": n, "total": n, "finished": true }),
        );
        let _ = app2.emit("dispatch", ());
    });
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_keys_merge_abbreviations() {
        assert_eq!(
            address_key("8241 E. 41st St"),
            address_key("8241 East 41st Street")
        );
        assert_eq!(
            address_key("38th & Keystone Ave"),
            address_key("38th and Keystone Avenue")
        );
        assert_ne!(
            address_key("8241 East 41st Street"),
            address_key("8241 East 21st Street")
        );
    }

    #[test]
    fn geocode_query_drops_units_and_region() {
        let r = "Indianapolis, IN";
        for (heard, want) in [
            ("4350 Madison Avenue, apartment 604", "4350 Madison Avenue"),
            (
                "2250 Harvest Moon Drive, Room 402",
                "2250 Harvest Moon Drive",
            ),
            ("9135 Bryant Lane, Apartment 1B", "9135 Bryant Lane"),
            (
                "8820 South Meridian Street, Suite 200",
                "8820 South Meridian Street",
            ),
            ("8241 East 41st Street Apt. 3", "8241 East 41st Street"),
            ("8241 East 41st Street Apt B", "8241 East 41st Street"),
            ("123 Main Street #4", "123 Main Street"),
            ("123 Main Street # 4", "123 Main Street"),
            ("7510 Rogate Drive, Indianapolis", "7510 Rogate Drive"),
            ("7510 Rogate Drive, Indianapolis, IN", "7510 Rogate Drive"),
            ("1200 Unit Drive", "1200 Unit Drive"),
            ("Unit Street and Lot Road", "Unit Street and Lot Road"),
            (
                "North Delaware Street and East 32nd Street",
                "North Delaware Street and East 32nd Street",
            ),
        ] {
            assert_eq!(geocode_query(heard, r), want, "{heard:?}");
        }
        assert_eq!(
            geocode_query("7510 Rogate Drive, Indianapolis", ""),
            "7510 Rogate Drive, Indianapolis"
        );
    }

    #[test]
    fn emoji_check() {
        for ok in [
            "🫀",
            "❤️",
            "❤️‍🩹",
            "😮‍💨",
            "🚗",
            "🔥",
            "👨‍🚒",
            "🧑🏽‍🚒",
            "1️⃣",
            "☣️",
            "⚠️",
            "📍",
        ] {
            assert!(is_emoji(ok), "{ok:?} should pass");
        }
        for bad in [
            "",
            "fire",
            "🔥 fire",
            "<b>",
            "🔥🔥🔥🔥🔥",
            "a",
            "🔥\n",
            "ab🔥",
        ] {
            assert!(!is_emoji(bad), "{bad:?} should fail");
        }
    }

    #[test]
    fn units_are_tidied() {
        let v = serde_json::json!("Medic 42, Engine 6 ,  medic 42, ,x");
        assert_eq!(parse_units(&v), vec!["Medic 42", "Engine 6"]);
        let v = serde_json::json!(["Ladder 38", "<script>", "Ladder 38"]);
        assert_eq!(parse_units(&v), vec!["Ladder 38", "script"]);
    }

    #[test]
    fn tidy_snaps_types_and_falls_back_on_emoji() {
        let s = Settings::default();
        let ch = Channel {
            tg: 1,
            name: "".into(),
            role: "dispatch".into(),
            fixed_call_type: String::new(),
            enabled: true,
        };
        let obj = serde_json::json!({ "is_dispatch": true, "call_type": "cardiac arrest working", "address": "8241 East 41st Street", "units": "Ladder 38", "summary": "x", "emoji": "not an emoji", "confidence": "95%" });
        let x = tidy(&s, &ch, &obj);
        assert_eq!(x.call_type, "Cardiac Arrest");
        assert_eq!(x.emoji, "🫀");
        assert_eq!(x.confidence, 95);
        assert_eq!(x.units, vec!["Ladder 38"]);
        let fixed = Channel {
            fixed_call_type: "EMS".into(),
            ..ch.clone()
        };
        assert_eq!(tidy(&s, &fixed, &obj).call_type, "EMS");
        let obj = serde_json::json!({ "call_type": "Sasquatch", "address": null, "emoji": "🚗" });
        let x = tidy(&s, &ch, &obj);
        assert_eq!(x.call_type, "Unknown");
        assert_eq!(x.emoji, "🚗");
        assert!(x.address.is_empty());
    }

    #[test]
    fn settings_are_sanitized() {
        let mut s = Settings::default();
        s.geocoder_url = "ftp://x".into();
        assert!(sanitize_settings(&mut s).is_err());
        let mut s = Settings::default();
        s.geocoder_url = "https://geo.example.org/".into();
        s.group_radius_m = 1;
        s.channels = vec![
            Channel {
                tg: 5,
                name: "A\u{202E}".into(),
                role: "weird".into(),
                fixed_call_type: String::new(),
                enabled: true,
            },
            Channel {
                tg: 5,
                name: "dup".into(),
                role: "tactical".into(),
                fixed_call_type: String::new(),
                enabled: true,
            },
        ];
        s.call_types = vec![CallType {
            name: "Fire".into(),
            emoji: "text".into(),
        }];
        sanitize_settings(&mut s).unwrap();
        assert_eq!(s.geocoder_url, "https://geo.example.org");
        assert_eq!(s.group_radius_m, 10);
        assert_eq!(s.channels.len(), 1);
        assert_eq!(s.channels[0].name, "A");
        assert_eq!(s.channels[0].role, "dispatch");
        assert_eq!(s.call_types.len(), 2);
        assert_eq!(s.call_types[0].emoji, "📍");
        assert_eq!(s.call_types[1].name, "Unknown");
    }

    fn inc(
        id: i64,
        addr: &str,
        lat: Option<f64>,
        lon: Option<f64>,
        call_type: &str,
        units: &[&str],
        updated: i64,
    ) -> (Incident, String) {
        (
            Incident {
                id,
                updated,
                created: updated,
                address: addr.into(),
                lat,
                lon,
                call_type: call_type.into(),
                units: units.iter().map(|u| u.to_string()).collect(),
                ..Default::default()
            },
            address_key(addr),
        )
    }

    #[test]
    fn dispatches_are_separated_by_address_not_unit() {
        let open = vec![
            inc(
                1,
                "8241 East 41st Street",
                Some(39.83),
                Some(-86.02),
                "Cardiac Arrest",
                &["Medic 21"],
                1000,
            ),
            inc(
                2,
                "2000 South Meridian Street",
                Some(39.74),
                Some(-86.15),
                "Structure Fire",
                &["Engine 23"],
                1100,
            ),
        ];
        let units = vec![unit_key("Medic 21")];
        // Same unit, different address, far away → a new incident.
        let (t, _) = pick_target(
            &open,
            "dispatch",
            &address_key("1124 North Whitcomb Avenue"),
            Some(39.78),
            Some(-86.24),
            "Sick Person",
            &units,
            150.0,
            1200,
        );
        assert!(t.is_none());
        // Same unit, address not geocoded → still a new incident.
        let (t, _) = pick_target(
            &open,
            "dispatch",
            &address_key("9111 Avenue"),
            None,
            None,
            "Injured Person",
            &units,
            150.0,
            1200,
        );
        assert!(t.is_none());
        // Same address (abbreviated) → the same incident.
        let (t, how) = pick_target(
            &open,
            "dispatch",
            &address_key("8241 E 41st St"),
            None,
            None,
            "Unknown",
            &[],
            150.0,
            1200,
        );
        assert_eq!(t.map(|i| i.id), Some(1));
        assert_eq!(how, "same address");
    }

    #[test]
    fn nearby_merges_only_when_call_types_agree() {
        let open = vec![inc(
            1,
            "8241 East 41st Street",
            Some(39.8300),
            Some(-86.0200),
            "Cardiac Arrest",
            &[],
            1000,
        )];
        let close = (Some(39.8303), Some(-86.0200)); // ~33 m away
        let (t, _) = pick_target(
            &open,
            "dispatch",
            "x",
            close.0,
            close.1,
            "Structure Fire",
            &[],
            150.0,
            1100,
        );
        assert!(
            t.is_none(),
            "different call type next door is a different incident"
        );
        let (t, how) = pick_target(
            &open,
            "dispatch",
            "x",
            close.0,
            close.1,
            "Unknown",
            &[],
            150.0,
            1100,
        );
        assert_eq!(t.map(|i| i.id), Some(1));
        assert_eq!(how, "nearby");
    }

    #[test]
    fn unit_follow_ups_and_tactical_updates_attach_by_unit() {
        let open = vec![inc(
            1,
            "8241 East 41st Street",
            Some(39.83),
            Some(-86.02),
            "Cardiac Arrest",
            &["Medic 21"],
            1000,
        )];
        let units = vec![unit_key("Medic 21")];
        // Dispatch channel, no address, same unit, 2 min later → follow-up.
        let (t, how) = pick_target(
            &open, "dispatch", "", None, None, "Unknown", &units, 150.0, 1120,
        );
        assert_eq!(t.map(|i| i.id), Some(1));
        assert_eq!(how, "unit follow-up");
        // …but not 20 minutes later.
        let (t, _) = pick_target(
            &open, "dispatch", "", None, None, "Unknown", &units, 150.0, 2300,
        );
        assert!(t.is_none());
        // Tactical traffic naming the unit attaches to it.
        let (t, how) = pick_target(
            &open, "tactical", "", None, None, "Unknown", &units, 150.0, 2300,
        );
        assert_eq!(t.map(|i| i.id), Some(1));
        assert_eq!(how, "same unit");
    }

    #[test]
    fn grid_references_and_intersections_are_recognised() {
        assert!(is_grid_ref("3200 North 100 East"));
        assert!(is_grid_ref("4100 North, 8200 East"));
        assert!(!is_grid_ref("8241 East 41st Street"));
        assert!(!is_grid_ref("North Delaware Street and East 32nd Street"));
        assert_eq!(
            intersection_parts("North Delaware Street and East 32nd Street"),
            Some(("North Delaware Street".into(), "East 32nd Street".into()))
        );
        assert_eq!(
            intersection_parts("38th & Keystone").map(|p| p.0),
            Some("38th".into())
        );
        assert!(intersection_parts("8241 East 41st Street").is_none());
        assert!(intersection_parts("Meridian and 100").is_none());
    }

    #[test]
    fn a_grid_reference_in_the_address_yields_to_the_cross_streets() {
        let s = Settings::default();
        let ch = Channel {
            tg: 1,
            name: "".into(),
            role: "dispatch".into(),
            fixed_call_type: String::new(),
            enabled: true,
        };
        let obj = serde_json::json!({ "is_dispatch": true, "call_type": "Overdose", "address": "3200 North 100 East", "cross_street": "North Delaware Street and East 32nd Street", "units": "Engine 14, Ambulance 2, EMS 94", "summary": "x", "emoji": "💊", "confidence": 95 });
        let x = tidy(&s, &ch, &obj);
        assert_eq!(x.address, "North Delaware Street and East 32nd Street");
        assert_eq!(x.grid, "3200 North 100 East");
        let obj = serde_json::json!({ "is_dispatch": true, "call_type": "Overdose", "address": "North Delaware Street & East 32nd Street", "grid": "3200 North 100 East", "emoji": "💊" });
        let x = tidy(&s, &ch, &obj);
        assert_eq!(x.address, "North Delaware Street and East 32nd Street");
        assert_eq!(x.grid, "3200 North 100 East");
        let obj = serde_json::json!({ "is_dispatch": true, "call_type": "Overdose", "address": "3200 North 100 East", "emoji": "💊" });
        let x = tidy(&s, &ch, &obj);
        assert!(x.address.is_empty());
        assert_eq!(x.grid, "3200 North 100 East");
    }

    #[test]
    fn street_regexes_accept_osm_spellings() {
        let re = |s: &str| {
            regex::RegexBuilder::new(&street_name_regex(s))
                .case_insensitive(true)
                .build()
                .unwrap()
        };
        let r = re("North Delaware Street");
        assert!(r.is_match("North Delaware Street") && r.is_match("N Delaware St"));
        assert!(
            !r.is_match("South Delaware Street") && !r.is_match("North Delaware Street Extension")
        );
        let r = re("Keystone");
        assert!(r.is_match("North Keystone Avenue") && r.is_match("Keystone Ave"));
        assert!(!r.is_match("Keystone Crossing"));
        let r = re("East 32nd Street");
        assert!(r.is_match("E 32nd St") && !r.is_match("East 32nd Place"));
    }

    /// Network: the real Overpass service. `cargo test -- --ignored overpass`.
    #[test]
    #[ignore]
    fn overpass_finds_a_real_intersection() {
        let s = Settings::default();
        let g = overpass_intersection(&s, "North Delaware Street", "East 32nd Street").unwrap();
        let (lat, lon, d) = g.expect("intersection");
        eprintln!("{lat:.5}, {lon:.5} {d}");
        assert!((lat - 39.8235).abs() < 0.01 && (lon + 86.1548).abs() < 0.01);
    }

    #[test]
    fn haversine_is_sane() {
        let d = haversine_m(39.7684, -86.1581, 39.7684, -86.1581);
        assert!(d < 0.01);
        let d = haversine_m(39.7684, -86.1581, 39.7774, -86.1581);
        assert!((d - 1000.0).abs() < 15.0);
    }

    #[test]
    fn url_encoding() {
        assert_eq!(
            url_encode("38th & Keystone, Indianapolis"),
            "38th+%26+Keystone%2C+Indianapolis"
        );
    }
}
