//! Retention: what the call library keeps, for how long, and what it does
//! not keep at all.
//!
//! The old control was one number of days in the page's local storage,
//! pruned from JavaScript — and "0" meant "delete every unstarred call at
//! each start". This keeps the policy in the backend, on an hourly timer,
//! and makes each decision explicit:
//!
//! - a **default** for how long a call's record (row + transcript) and its
//!   audio are kept — audio can go sooner than the record;
//! - **rules** for chosen talkgroups (a hospital set for 90 days, a busy
//!   ops channel for 2), first match wins;
//! - **protected** calls — starred, sent about by a tripwire, or part of an
//!   incident or hospital report — kept whatever the rules say;
//! - **not worth keeping**: audio shorter than a floor (once transcribed),
//!   rows with no audio at all (a grant with no voice), encrypted calls;
//! - an optional **size cap** on audio, oldest unprotected audio first.
//!
//! Every run is planned first; the same plan is what the page previews
//! ("would delete 312 calls and 1,204 recordings — 2.3 GB"), so Apply does
//! exactly what the preview said.

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::AppState;

/// Retention for a chosen set of talkgroups. `None` days = keep forever.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Rule {
    pub id: String,
    pub name: String,
    pub tgs: Vec<u16>,
    pub record_days: Option<u32>,
    pub audio_days: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Settings {
    /// Run on the hourly timer. Off = only when asked (Apply now).
    pub enabled: bool,
    /// Days a call's record (row + transcript) is kept; None = forever.
    pub record_days: Option<u32>,
    /// Days its audio is kept; None = as long as the record.
    pub audio_days: Option<u32>,
    #[serde(default)]
    pub rules: Vec<Rule>,
    pub keep_starred: bool,
    /// Calls a tripwire sent a message about.
    pub keep_fired: bool,
    /// Calls attached to a dispatch incident or a stored conversation.
    pub keep_incidents: bool,
    /// Drop audio shorter than this many seconds (after transcription had
    /// its chance); 0 = off.
    pub min_audio_secs: f64,
    /// Delete rows with no audio (a grant that carried no voice) after this
    /// many hours; None = keep them.
    pub no_audio_hours: Option<u32>,
    /// Delete encrypted-call rows after this many hours; None = keep.
    pub encrypted_hours: Option<u32>,
    /// Cap on audio, in GB; None = no cap.
    pub max_audio_gb: Option<f64>,
    /// Days of tripwire history kept.
    pub history_days: u32,
    /// Set once the old page setting (days in local storage) was carried
    /// over, so it is not carried over again.
    #[serde(default)]
    pub migrated: bool,
    /// What the carried-over value was, when it needed the listener's eye.
    #[serde(default)]
    pub notice: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            record_days: None,
            audio_days: None,
            rules: Vec::new(),
            keep_starred: true,
            keep_fired: true,
            keep_incidents: true,
            min_audio_secs: 0.0,
            no_audio_hours: None,
            encrypted_hours: None,
            max_audio_gb: None,
            history_days: 180,
            migrated: false,
            notice: String::new(),
        }
    }
}

pub fn sanitize(s: &mut Settings) {
    let days = |d: &mut Option<u32>| {
        if let Some(v) = d {
            *v = (*v).clamp(1, 36_500);
        }
    };
    days(&mut s.record_days);
    days(&mut s.audio_days);
    s.rules.truncate(100);
    let mut ids = HashSet::new();
    for (i, r) in s.rules.iter_mut().enumerate() {
        r.name = crate::analyzers::clean_line(&r.name, 60);
        if r.name.is_empty() {
            r.name = format!("Rule {}", i + 1);
        }
        if r.id.is_empty() || !ids.insert(r.id.clone()) {
            r.id = format!("r{}-{i}", crate::library::now());
            ids.insert(r.id.clone());
        }
        r.tgs.sort_unstable();
        r.tgs.dedup();
        days(&mut r.record_days);
        days(&mut r.audio_days);
    }
    s.min_audio_secs = if s.min_audio_secs.is_finite() {
        s.min_audio_secs.clamp(0.0, 60.0)
    } else {
        0.0
    };
    if let Some(h) = &mut s.no_audio_hours {
        *h = (*h).clamp(1, 24 * 3650);
    }
    if let Some(h) = &mut s.encrypted_hours {
        *h = (*h).clamp(1, 24 * 3650);
    }
    if let Some(g) = &mut s.max_audio_gb {
        if !g.is_finite() || *g <= 0.0 {
            s.max_audio_gb = None;
        } else {
            *g = g.clamp(0.1, 100_000.0);
        }
    }
    s.history_days = s.history_days.clamp(7, 3650);
    s.notice = crate::analyzers::clean_line(&s.notice, 300);
}

fn path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let d = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("config dir: {e}"))?;
    std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    Ok(d.join("retention.json"))
}

pub fn load(app: &AppHandle) -> Settings {
    path(app)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn store(app: &AppHandle, s: &Settings) -> Result<(), String> {
    let p = path(app)?;
    std::fs::write(
        &p,
        serde_json::to_string_pretty(s).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("{}: {e}", p.display()))
}

// ---------------------------------------------------------------------------
// planning
// ---------------------------------------------------------------------------

/// One stored call, as much as a retention decision needs.
#[derive(Clone, Debug, Default)]
pub struct Stored {
    pub id: i64,
    pub start: i64,
    pub secs: f64,
    pub tg: u16,
    pub audio: Option<String>,
    pub bytes: u64,
    pub starred: bool,
    pub encrypted: bool,
    pub transcribed: bool,
}

/// Why a call is kept whatever the rules say.
#[derive(Default)]
pub struct Protected {
    pub fired: HashSet<i64>,
    pub incidents: HashSet<i64>,
}

#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub struct Plan {
    /// Calls whose record (and audio) go.
    pub delete: Vec<i64>,
    /// Calls that keep their record but lose their audio.
    pub drop_audio: Vec<i64>,
    pub bytes: u64,
    /// Counts by reason, for the preview.
    pub reasons: Vec<(String, u32)>,
}

fn note(reasons: &mut HashMap<&'static str, u32>, why: &'static str) {
    *reasons.entry(why).or_default() += 1;
}

/// Decide, for every stored call, whether it stays, loses its audio, or
/// goes. Pure: the caller supplies the calls and what protects them.
pub fn plan(s: &Settings, calls: &[Stored], protected: &Protected, now: i64) -> Plan {
    let mut out = Plan::default();
    let mut reasons: HashMap<&'static str, u32> = HashMap::new();
    let day = 86_400i64;
    let rule_for = |tg: u16| s.rules.iter().find(|r| r.tgs.contains(&tg));
    // Audio still held after the per-call pass, for the size cap.
    let mut kept_audio: Vec<&Stored> = Vec::new();
    for c in calls {
        let age = now - c.start;
        let is_protected = (s.keep_starred && c.starred)
            || (s.keep_fired && protected.fired.contains(&c.id))
            || (s.keep_incidents && protected.incidents.contains(&c.id));
        if is_protected {
            continue;
        }
        let (record_days, audio_days) = match rule_for(c.tg) {
            Some(r) => (r.record_days, r.audio_days),
            None => (s.record_days, s.audio_days),
        };
        let has_audio = c.audio.is_some();
        // Not worth keeping at all.
        if !has_audio && !c.encrypted && c.secs <= 0.05 {
            if let Some(h) = s.no_audio_hours {
                if age > h as i64 * 3600 {
                    out.delete.push(c.id);
                    note(&mut reasons, "no audio (a grant with no voice)");
                    continue;
                }
            }
        }
        if c.encrypted {
            if let Some(h) = s.encrypted_hours {
                if age > h as i64 * 3600 {
                    out.delete.push(c.id);
                    out.bytes += c.bytes;
                    note(&mut reasons, "encrypted");
                    continue;
                }
            }
        }
        if let Some(d) = record_days {
            if age > d as i64 * day {
                out.delete.push(c.id);
                out.bytes += c.bytes;
                note(&mut reasons, "older than its record limit");
                continue;
            }
        }
        if has_audio {
            if let Some(d) = audio_days {
                if age > d as i64 * day {
                    out.drop_audio.push(c.id);
                    out.bytes += c.bytes;
                    note(&mut reasons, "audio older than its audio limit");
                    continue;
                }
            }
            // Short clips: once transcription had its chance (transcribed,
            // or an hour has passed).
            if s.min_audio_secs > 0.0 && c.secs < s.min_audio_secs && (c.transcribed || age > 3600)
            {
                out.drop_audio.push(c.id);
                out.bytes += c.bytes;
                note(&mut reasons, "audio shorter than the floor");
                continue;
            }
            kept_audio.push(c);
        }
    }
    if let Some(gb) = s.max_audio_gb {
        let cap = (gb * 1e9) as u64;
        let mut total: u64 = kept_audio.iter().map(|c| c.bytes).sum();
        if total > cap {
            kept_audio.sort_by_key(|c| c.start);
            for c in kept_audio {
                if total <= cap {
                    break;
                }
                out.drop_audio.push(c.id);
                out.bytes += c.bytes;
                total = total.saturating_sub(c.bytes);
                note(&mut reasons, "over the size cap (oldest first)");
            }
        }
    }
    let mut r: Vec<(String, u32)> = reasons
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    r.sort_by(|a, b| b.1.cmp(&a.1));
    out.reasons = r;
    out
}

/// Every stored call, with its audio file's size.
pub fn stored_calls(c: &Connection) -> Result<Vec<Stored>, String> {
    let mut st = c
        .prepare("SELECT id, start, secs, tg, audio, starred, encrypted, transcript IS NOT NULL FROM calls")
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([], |r| {
            Ok(Stored {
                id: r.get(0)?,
                start: r.get(1)?,
                secs: r.get(2)?,
                tg: r.get::<_, i64>(3)? as u16,
                audio: r.get(4)?,
                bytes: 0,
                starred: r.get::<_, i64>(5)? != 0,
                encrypted: r.get::<_, i64>(6)? != 0,
                transcribed: r.get::<_, i64>(7)? != 0,
            })
        })
        .map_err(|e| e.to_string())?;
    let mut out: Vec<Stored> = rows.filter_map(Result::ok).collect();
    for s in out.iter_mut() {
        if let Some(a) = &s.audio {
            s.bytes = std::fs::metadata(a).map(|m| m.len()).unwrap_or(0);
        }
    }
    Ok(out)
}

/// What keeps calls regardless of the rules.
pub fn protected(c: &Connection) -> Protected {
    let ids = |sql: &str| -> HashSet<i64> {
        c.prepare(sql)
            .and_then(|mut st| {
                st.query_map([], |r| r.get::<_, i64>(0))
                    .map(|rows| rows.filter_map(Result::ok).collect())
            })
            .unwrap_or_default()
    };
    let mut incidents = ids("SELECT call FROM incident_calls");
    // Stored conversations list their calls inside the pieces JSON.
    if let Ok(mut st) = c.prepare("SELECT pieces FROM conversations") {
        if let Ok(rows) = st.query_map([], |r| r.get::<_, String>(0)) {
            for p in rows.flatten() {
                if let Ok(v) = serde_json::from_str::<Vec<serde_json::Value>>(&p) {
                    incidents.extend(v.iter().filter_map(|x| x["id"].as_i64()));
                }
            }
        }
    }
    Protected {
        fired: ids(
            "SELECT ec.call FROM tripwire_event_calls ec JOIN tripwire_events e ON e.id = ec.event WHERE e.status = 'sent'",
        ),
        incidents,
    }
}

/// Carry a plan out: delete records (and their audio and sidecars), drop
/// audio from the rest. Returns (records deleted, recordings removed).
pub fn apply(c: &Connection, p: &Plan) -> Result<(usize, usize), String> {
    let audio_of = |id: i64| -> Option<String> {
        c.query_row("SELECT audio FROM calls WHERE id = ?1", params![id], |r| {
            r.get(0)
        })
        .ok()
        .flatten()
    };
    let remove = |a: &str| {
        let _ = std::fs::remove_file(a);
        let _ = std::fs::remove_file(std::path::Path::new(a).with_extension("json"));
    };
    let mut files = 0;
    for id in &p.delete {
        if let Some(a) = audio_of(*id) {
            remove(&a);
            files += 1;
        }
        c.execute("DELETE FROM calls WHERE id = ?1", params![id])
            .map_err(|e| e.to_string())?;
    }
    for id in &p.drop_audio {
        if let Some(a) = audio_of(*id) {
            remove(&a);
            files += 1;
        }
        // The capture hash stays: the record still says what was heard and
        // what the file was, only the file is gone.
        c.execute("UPDATE calls SET audio = NULL WHERE id = ?1", params![id])
            .map_err(|e| e.to_string())?;
    }
    Ok((p.delete.len(), files))
}

// ---------------------------------------------------------------------------
// disk use
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone, Debug, Default)]
pub struct Usage {
    pub calls: u64,
    pub with_audio: u64,
    pub audio_bytes: u64,
    pub db_bytes: u64,
    /// Audio from the last 7 days, per day on average.
    pub bytes_per_day: u64,
    pub oldest: i64,
    /// (talkgroup, name, calls, audio bytes), largest first.
    pub by_tg: Vec<(u16, String, u64, u64)>,
    /// (label, calls, audio bytes) by age.
    pub by_age: Vec<(String, u64, u64)>,
    pub protected_calls: u64,
    pub free_disk_bytes: u64,
}

pub fn usage(c: &Connection, db_path: Option<&std::path::Path>, now: i64) -> Result<Usage, String> {
    let calls = stored_calls(c)?;
    let names: HashMap<u16, String> = c
        .prepare("SELECT tg, MAX(tg_name) FROM calls GROUP BY tg")
        .and_then(|mut st| {
            st.query_map([], |r| {
                Ok((r.get::<_, i64>(0)? as u16, r.get::<_, String>(1)?))
            })
            .map(|rows| rows.filter_map(Result::ok).collect())
        })
        .unwrap_or_default();
    let prot = protected(c);
    let mut u = Usage {
        calls: calls.len() as u64,
        oldest: calls.iter().map(|x| x.start).min().unwrap_or(0),
        db_bytes: db_path
            .map(|p| {
                ["", "-wal", "-shm"]
                    .iter()
                    .map(|s| {
                        std::fs::metadata(format!("{}{s}", p.display()))
                            .map(|m| m.len())
                            .unwrap_or(0)
                    })
                    .sum()
            })
            .unwrap_or(0),
        ..Default::default()
    };
    let mut by_tg: HashMap<u16, (u64, u64)> = HashMap::new();
    let buckets = [
        ("last 24 h", 1i64),
        ("1–7 days", 7),
        ("1–4 weeks", 28),
        ("older", i64::MAX / 86_400),
    ];
    let mut by_age = vec![(0u64, 0u64); buckets.len()];
    let mut week = 0u64;
    for x in &calls {
        u.audio_bytes += x.bytes;
        if x.audio.is_some() {
            u.with_audio += 1;
        }
        let e = by_tg.entry(x.tg).or_default();
        e.0 += 1;
        e.1 += x.bytes;
        let age_days = (now - x.start).max(0) / 86_400;
        let i = buckets
            .iter()
            .position(|(_, d)| age_days < *d)
            .unwrap_or(buckets.len() - 1);
        by_age[i].0 += 1;
        by_age[i].1 += x.bytes;
        if now - x.start < 7 * 86_400 {
            week += x.bytes;
        }
        if x.starred || prot.fired.contains(&x.id) || prot.incidents.contains(&x.id) {
            u.protected_calls += 1;
        }
    }
    let span_days = (((now - u.oldest).max(86_400)) as f64 / 86_400.0).min(7.0);
    u.bytes_per_day = (week as f64 / span_days) as u64;
    let mut t: Vec<(u16, String, u64, u64)> = by_tg
        .into_iter()
        .map(|(tg, (n, b))| (tg, names.get(&tg).cloned().unwrap_or_default(), n, b))
        .collect();
    t.sort_by(|a, b| b.3.cmp(&a.3).then(b.2.cmp(&a.2)));
    t.truncate(12);
    u.by_tg = t;
    u.by_age = buckets
        .iter()
        .zip(by_age)
        .map(|((l, _), (n, b))| (l.to_string(), n, b))
        .collect();
    Ok(u)
}

// ---------------------------------------------------------------------------
// running it
// ---------------------------------------------------------------------------

fn db_of(app: &AppHandle) -> Option<crate::dispatch::Db> {
    app.state::<AppState>().db.lock().unwrap().clone()
}

/// Plan against the library as it is now.
fn plan_now(app: &AppHandle, s: &Settings) -> Result<Plan, String> {
    let db = db_of(app).ok_or("the call library is not open")?;
    let c = db.lock().unwrap();
    let calls = stored_calls(&c)?;
    let prot = protected(&c);
    Ok(plan(s, &calls, &prot, crate::library::now()))
}

/// One cleanup pass: plan, apply, prune the history. Returns a summary.
pub fn run_once(app: &AppHandle, s: &Settings) -> Result<String, String> {
    let p = plan_now(app, s)?;
    let db = db_of(app).ok_or("the call library is not open")?;
    let c = db.lock().unwrap();
    let (deleted, files) = apply(&c, &p)?;
    let _ = crate::events::prune(&c, s.history_days);
    drop(c);
    let msg = format!(
        "{deleted} call{} deleted, {files} recording{} removed, {} freed",
        if deleted == 1 { "" } else { "s" },
        if files == 1 { "" } else { "s" },
        human_bytes(p.bytes)
    );
    if deleted + files > 0 {
        eprintln!("[retention] {msg}");
        let _ = app.emit("retention", &msg);
    }
    Ok(msg)
}

pub fn human_bytes(b: u64) -> String {
    if b >= 1_000_000_000 {
        format!("{:.1} GB", b as f64 / 1e9)
    } else if b >= 1_000_000 {
        format!("{:.0} MB", b as f64 / 1e6)
    } else {
        format!("{} KB", b / 1000)
    }
}

/// The hourly timer: runs a pass when retention is on.
pub fn spawn_ticker(app: AppHandle) {
    std::thread::spawn(move || {
        // Let the library open and the first calls land before the first pass.
        std::thread::sleep(std::time::Duration::from_secs(120));
        loop {
            let s = app.state::<AppState>().retention.lock().unwrap().clone();
            if s.enabled {
                if let Err(e) = run_once(&app, &s) {
                    eprintln!("[retention] {e}");
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    });
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn retention_get(state: State<AppState>) -> Settings {
    state.retention.lock().unwrap().clone()
}

#[tauri::command]
pub fn retention_set(
    app: AppHandle,
    state: State<AppState>,
    settings: Settings,
) -> Result<Settings, String> {
    let mut s = settings;
    sanitize(&mut s);
    store(&app, &s)?;
    *state.retention.lock().unwrap() = s.clone();
    Ok(s)
}

/// Carry over the page's old one-number setting, once. "0" deleted every
/// unstarred call at each start; that is not carried over silently —
/// cleanup stays off and the page says what the old value did.
#[tauri::command]
pub fn retention_migrate(
    app: AppHandle,
    state: State<AppState>,
    days: Option<u32>,
) -> Result<Settings, String> {
    let cur = state.retention.lock().unwrap().clone();
    if cur.migrated {
        return Ok(cur);
    }
    let s = carried_over(cur, days);
    store(&app, &s)?;
    *state.retention.lock().unwrap() = s.clone();
    Ok(s)
}

/// The policy after carrying over the old page setting (`None` = it was
/// blank, "never").
pub fn carried_over(mut s: Settings, days: Option<u32>) -> Settings {
    s.migrated = true;
    match days {
        Some(0) => {
            s.enabled = false;
            s.notice = "Your old setting was “delete calls older than 0 days”, which deleted every unstarred call each time the app started. Automatic cleanup is off until you choose what to keep.".into();
        }
        Some(d) => {
            s.enabled = true;
            s.record_days = Some(d);
        }
        None => {}
    }
    sanitize(&mut s);
    s
}

/// What a run would do, in counts (the id lists stay in the backend).
#[derive(Serialize)]
pub struct Preview {
    pub calls: usize,
    pub recordings: usize,
    pub bytes: u64,
    pub reasons: Vec<(String, u32)>,
    pub summary: String,
}

/// What `settings` would do right now, without doing it.
#[tauri::command]
pub async fn retention_preview(app: AppHandle, settings: Settings) -> Result<Preview, String> {
    let mut s = settings;
    sanitize(&mut s);
    tauri::async_runtime::spawn_blocking(move || {
        let p = plan_now(&app, &s)?;
        let summary = if p.delete.is_empty() && p.drop_audio.is_empty() {
            "Nothing would be deleted right now.".to_string()
        } else {
            format!(
                "Would delete {} call{} and the audio of {} more — {} freed.",
                p.delete.len(),
                if p.delete.len() == 1 { "" } else { "s" },
                p.drop_audio.len(),
                human_bytes(p.bytes)
            )
        };
        Ok(Preview {
            calls: p.delete.len(),
            recordings: p.drop_audio.len(),
            bytes: p.bytes,
            reasons: p.reasons,
            summary,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Run the saved policy now.
#[tauri::command]
pub async fn retention_apply(app: AppHandle, state: State<'_, AppState>) -> Result<String, String> {
    let s = state.retention.lock().unwrap().clone();
    tauri::async_runtime::spawn_blocking(move || run_once(&app, &s))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn retention_usage(app: AppHandle) -> Result<Usage, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let db = db_of(&app).ok_or("the call library is not open")?;
        let dbp = app
            .path()
            .app_data_dir()
            .ok()
            .map(|d| d.join("library").join("calls.db"));
        let c = db.lock().unwrap();
        let mut u = usage(&c, dbp.as_deref(), crate::library::now())?;
        drop(c);
        u.free_disk_bytes = free_bytes(dbp.as_deref());
        Ok(u)
    })
    .await
    .map_err(|e| e.to_string())?
}

fn free_bytes(p: Option<&std::path::Path>) -> u64 {
    let Some(p) = p else { return 0 };
    let disks = sysinfo::Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        .filter(|d| p.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| d.available_space())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;
    fn call(id: i64, age_days: f64, tg: u16, audio: bool, bytes: u64) -> Stored {
        Stored {
            id,
            start: 100 * DAY - (age_days * DAY as f64) as i64,
            secs: 5.0,
            tg,
            audio: audio.then(|| format!("/x/{id}.m4a")),
            bytes,
            transcribed: true,
            ..Default::default()
        }
    }

    #[test]
    fn records_and_audio_age_out_separately_and_rules_win() {
        let s = Settings {
            record_days: Some(30),
            audio_days: Some(7),
            rules: vec![Rule {
                id: "h".into(),
                name: "Hospitals".into(),
                tgs: vec![2],
                record_days: Some(90),
                audio_days: Some(60),
            }],
            ..Default::default()
        };
        let calls = vec![
            call(1, 3.0, 1, true, 10),  // young: kept
            call(2, 10.0, 1, true, 10), // audio past 7 d: dropped
            call(3, 40.0, 1, true, 10), // record past 30 d: deleted
            call(4, 40.0, 2, true, 10), // hospital rule: 40 d is fine
            call(5, 70.0, 2, true, 10), // hospital audio past 60 d
            call(6, 95.0, 2, false, 0), // hospital record past 90 d
        ];
        let p = plan(&s, &calls, &Protected::default(), 100 * DAY);
        assert_eq!(p.delete, vec![3, 6]);
        assert_eq!(p.drop_audio, vec![2, 5]);
        assert_eq!(p.bytes, 30);
    }

    #[test]
    fn protected_calls_stay_whatever_the_rules_say() {
        let s = Settings {
            record_days: Some(1),
            ..Default::default()
        };
        let mut starred = call(1, 10.0, 1, true, 5);
        starred.starred = true;
        let calls = vec![
            starred,
            call(2, 10.0, 1, true, 5),
            call(3, 10.0, 1, true, 5),
            call(4, 10.0, 1, true, 5),
        ];
        let prot = Protected {
            fired: [2].into(),
            incidents: [3].into(),
        };
        let p = plan(&s, &calls, &prot, 100 * DAY);
        assert_eq!(p.delete, vec![4]);
        let loose = Settings {
            keep_starred: false,
            keep_fired: false,
            keep_incidents: false,
            ..s
        };
        assert_eq!(
            plan(&loose, &calls, &prot, 100 * DAY).delete,
            vec![1, 2, 3, 4]
        );
    }

    #[test]
    fn empty_grants_short_clips_and_encrypted_rows_can_go() {
        let s = Settings {
            no_audio_hours: Some(24),
            min_audio_secs: 1.5,
            encrypted_hours: Some(24),
            ..Default::default()
        };
        let mut grant = call(1, 2.0, 1, false, 0);
        grant.secs = 0.0;
        let mut fresh_grant = call(2, 0.5, 1, false, 0);
        fresh_grant.secs = 0.0;
        let mut short = call(3, 0.1, 1, true, 3);
        short.secs = 0.7;
        let mut short_untranscribed = call(4, 0.01, 1, true, 3);
        short_untranscribed.secs = 0.7;
        short_untranscribed.transcribed = false;
        let mut enc = call(5, 2.0, 1, false, 0);
        enc.encrypted = true;
        let p = plan(
            &s,
            &[
                grant,
                fresh_grant,
                short,
                short_untranscribed,
                enc,
                call(6, 2.0, 1, true, 9),
            ],
            &Protected::default(),
            100 * DAY,
        );
        assert_eq!(p.delete, vec![1, 5]);
        assert_eq!(
            p.drop_audio,
            vec![3],
            "an untranscribed clip waits for the transcriber"
        );
    }

    #[test]
    fn the_size_cap_takes_the_oldest_unprotected_audio_first() {
        let s = Settings {
            max_audio_gb: Some(2e-9 * 25.0),
            ..Default::default()
        }; // 50 bytes
        let mut old_star = call(1, 9.0, 1, true, 30);
        old_star.starred = true;
        let calls = vec![
            old_star,
            call(2, 8.0, 1, true, 30),
            call(3, 5.0, 1, true, 30),
            call(4, 1.0, 1, true, 30),
        ];
        let p = plan(&s, &calls, &Protected::default(), 100 * DAY);
        // 90 unprotected bytes against a 50-byte cap: the two oldest go.
        assert_eq!(p.drop_audio, vec![2, 3]);
        assert!(p.delete.is_empty());
    }

    #[test]
    fn the_old_page_setting_is_carried_over_but_zero_is_not_applied() {
        let z = carried_over(Settings::default(), Some(0));
        assert!(
            z.migrated && !z.enabled,
            "0 deleted everything at each start — not carried over"
        );
        assert!(z.notice.contains("0 days"));
        let d = carried_over(Settings::default(), Some(14));
        assert!(d.enabled && d.notice.is_empty());
        assert_eq!(d.record_days, Some(14));
        let n = carried_over(Settings::default(), None);
        assert!(n.migrated && !n.enabled && n.record_days.is_none());
    }

    #[test]
    fn nothing_is_decided_by_default() {
        let calls = vec![call(1, 400.0, 1, true, 5), call(2, 1.0, 1, false, 0)];
        let p = plan(
            &Settings::default(),
            &calls,
            &Protected::default(),
            100 * DAY,
        );
        assert!(p.delete.is_empty() && p.drop_audio.is_empty());
    }
}
