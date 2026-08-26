//! Periodic channel digests: every `interval` seconds, roll up the transcripts
//! heard on a set of talkgroups over the last `window` seconds and send a
//! "what's happening" summary to Telegram.
//!
//! Unlike `conversations` (which treats one back-and-forth as an incident and
//! summarises when it goes quiet), a digest is a timer: on a fixed cadence it
//! gathers everything since the last run and answers "what is happening on
//! these channels right now".

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::AppState;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DigestRule {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub tgs: Vec<u16>,
    /// How often a digest is sent, in seconds.
    pub interval_secs: u32,
    /// How far back the digest looks, in seconds.
    pub window_secs: u32,
    /// Instruction to the model.
    pub prompt: String,
    /// Telegram message template. `{summary} {name} {count} {window} {time}
    /// {transcript}`.
    pub message: String,
    /// Telegram chat; blank = the alerts' chat.
    #[serde(default)]
    pub chat_id: String,
}

impl Default for DigestRule {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            enabled: true,
            tgs: Vec::new(),
            interval_secs: 900,
            window_secs: 900,
            prompt: "Summarise what is happening on these radio channels right now. Group by talkgroup; list the units involved and any notable events (emergencies, fires, pursuits, medical calls, road closures, weather). Stick to what was actually said; mark anything unclear as unclear.".into(),
            message: "📡 {name}\n{summary}\n\n{count} transmissions in the last {window} · {time}".into(),
            chat_id: String::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Settings {
    pub rules: Vec<DigestRule>,
    /// rule id → when it last ran (epoch s), so a restart does not re-fire
    /// before the interval elapses.
    #[serde(default)]
    pub last_run: HashMap<String, i64>,
}

#[derive(Serialize, Clone, Debug)]
pub struct LogEntry {
    pub at: i64,
    pub rule: String,
    pub calls: usize,
    pub ok: bool,
    pub detail: String,
    pub summary: String,
}

#[derive(Default)]
pub struct DigestState {
    pub settings: Settings,
    pub log: VecDeque<LogEntry>,
}

pub type Shared = Mutex<DigestState>;

/// How many of the newest calls to pull per talkgroup for a window.
const MAX_CALLS_PER_TG: u32 = 50;

fn path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let d = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("config dir: {e}"))?;
    std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    Ok(d.join("digests.json"))
}

pub fn load(app: &AppHandle) -> DigestState {
    DigestState {
        settings: path(app)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default(),
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

/// Every 15 s, check whether any enabled digest is due and run it.
pub fn spawn_ticker(app: AppHandle) {
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(15));
        let state = app.state::<AppState>();
        let now = crate::library::now();
        let due: Vec<DigestRule> = {
            let mut st = state.digests.lock().unwrap();
            let rules = st.settings.rules.clone();
            let mut due = Vec::new();
            for r in rules.iter().filter(|r| r.enabled && !r.tgs.is_empty()) {
                let last = st.settings.last_run.get(&r.id).copied().unwrap_or(0);
                if now - last >= r.interval_secs as i64 {
                    due.push(r.clone());
                    // Mark-run up front so a slow model call cannot double-fire
                    // on a later tick while this one is still thinking.
                    st.settings.last_run.insert(r.id.clone(), now);
                }
            }
            let _ = store(&app, &st.settings);
            due
        };
        for r in due {
            let app2 = app.clone();
            std::thread::spawn(move || {
                let _ = run_digest(app2, r);
            });
        }
    });
}

/// Gather the window's transcripts, summarise them, and send to Telegram.
/// Returns the summary (or a status string) on success, an error on failure.
fn run_digest(app: AppHandle, r: DigestRule) -> Result<String, String> {
    let state = app.state::<AppState>();
    let (tg, ollama) = crate::alerts::shared_settings(&state);
    let chat = if r.chat_id.trim().is_empty() {
        tg.chat_id.clone()
    } else {
        r.chat_id.clone()
    };
    let now = crate::library::now();
    let since = now - r.window_secs as i64;

    let mut calls: Vec<crate::library::CallRow> = Vec::new();
    if let Some(db) = state.db.lock().unwrap().clone() {
        let c = db.lock().unwrap();
        for tg in &r.tgs {
            if let Ok(rows) = crate::library::search(
                &c,
                &crate::library::Query {
                    tg: Some(*tg),
                    from: Some(since),
                    limit: Some(MAX_CALLS_PER_TG),
                    ..Default::default()
                },
            ) {
                calls.extend(rows);
            }
        }
    }
    calls.sort_by_key(|c| c.start);
    calls.dedup_by_key(|c| c.id);

    // Only calls whose transcription actually landed contribute.
    let pieces: Vec<&crate::library::CallRow> = calls
        .iter()
        .filter(|c| {
            c.transcript_edited
                .as_deref()
                .or(c.transcript.as_deref())
                .is_some_and(|t| !t.trim().is_empty())
        })
        .collect();
    let n = pieces.len();
    if n == 0 {
        return Ok("no recent transcript traffic".to_string());
    }

    let rollup = rollup(&pieces);

    let prompt = format!(
        "{}\n\nThe following transmissions were heard in the window:\n\n{}",
        r.prompt.trim(),
        rollup
    );
    let summary = match crate::alerts::ollama_complete(&ollama, &prompt) {
        Ok(s) => s.trim().to_string(),
        Err(e) => format!("(summary unavailable: {e})\n{}", rollup.trim()),
    };

    let message = r
        .message
        .replace("{summary}", &summary)
        .replace("{name}", &r.name)
        .replace("{count}", &n.to_string())
        .replace("{window}", &fmt_window(r.window_secs))
        .replace("{time}", &fmt_time(now))
        .replace("{transcript}", &rollup.trim());

    let detail = match crate::alerts::send_text_id(&chat, message.trim()) {
        Ok(_) => "sent".to_string(),
        Err(e) => {
            log_it(&app, &r, n, false, e.clone(), summary.clone());
            let _ = app.emit("digests", ());
            return Err(e);
        }
    };
    log_it(&app, &r, n, true, detail, summary.clone());
    let _ = app.emit("digests", ());
    Ok(summary)
}

/// The window's transmissions stitched into a rollup, grouped by talkgroup,
/// oldest first.
fn rollup(pieces: &[&crate::library::CallRow]) -> String {
    let mut out = String::new();
    let mut last_tg: Option<u16> = None;
    for c in pieces {
        if last_tg != Some(c.tg) {
            out.push_str(&format!("\n[TG {} {}]\n", c.tg, c.tg_name));
            last_tg = Some(c.tg);
        }
        let text = c
            .transcript_edited
            .as_deref()
            .or(c.transcript.as_deref())
            .unwrap_or("")
            .trim();
        let who = c.unit_name.clone().unwrap_or_else(|| c.unit.to_string());
        out.push_str(&format!("{who}: {text}\n"));
    }
    out.trim().to_string()
}

fn fmt_window(secs: u32) -> String {
    if secs < 60 {
        format!("{secs} s")
    } else {
        format!("{} min", secs / 60)
    }
}

fn fmt_time(epoch: i64) -> String {
    let s = epoch.rem_euclid(86_400);
    format!("{:02}:{:02} UTC", s / 3600, (s % 3600) / 60)
}

fn log_it(
    app: &AppHandle,
    r: &DigestRule,
    calls: usize,
    ok: bool,
    detail: String,
    summary: String,
) {
    let state = app.state::<AppState>();
    let mut st = state.digests.lock().unwrap();
    st.log.push_front(LogEntry {
        at: crate::library::now(),
        rule: r.name.clone(),
        calls,
        ok,
        detail,
        summary,
    });
    st.log.truncate(100);
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn digests_get(state: State<AppState>) -> Settings {
    state.digests.lock().unwrap().settings.clone()
}

#[tauri::command]
pub fn digests_set(
    app: AppHandle,
    state: State<AppState>,
    rules: Vec<DigestRule>,
) -> Result<(), String> {
    let mut rules = rules;
    for (i, r) in rules.iter_mut().enumerate() {
        if r.id.trim().is_empty() {
            r.id = format!("d{}-{i}", crate::library::now());
        }
        if r.name.trim().is_empty() {
            r.name = format!("Digest {}", i + 1);
        }
        r.interval_secs = r.interval_secs.clamp(60, 86_400);
        r.window_secs = r.window_secs.clamp(60, 86_400);
        r.tgs.sort_unstable();
        r.tgs.dedup();
    }
    let mut st = state.digests.lock().unwrap();
    let keep: HashSet<&str> = rules.iter().map(|r| r.id.as_str()).collect();
    st.settings
        .last_run
        .retain(|k, _| keep.contains(k.as_str()));
    st.settings.rules = rules;
    store(&app, &st.settings)
}

#[tauri::command]
pub fn digests_log(state: State<AppState>) -> Vec<LogEntry> {
    state.digests.lock().unwrap().log.iter().cloned().collect()
}

/// Run one digest now, against the recent calls, so the rule can be seen working.
#[tauri::command]
pub async fn digest_test(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> Result<String, String> {
    let r = state
        .digests
        .lock()
        .unwrap()
        .settings
        .rules
        .iter()
        .find(|r| r.id == id)
        .cloned()
        .ok_or("no such digest rule")?;
    tauri::async_runtime::spawn_blocking(move || run_digest(app, r))
        .await
        .map_err(|e| e.to_string())?
}
