//! Conversations: the back-and-forth on a talkgroup, treated as one incident.
//!
//! An EMS unit calls the hospital, the hospital asks a couple of questions,
//! the unit gives an ETA, and that is the call — four or five transmissions
//! that belong together. A rule names the talkgroups this applies to and the
//! **fixed** radio IDs (the hospital's consoles); every other radio is a
//! mobile unit, and a conversation is keyed by (talkgroup, mobile unit). A
//! transmission from a fixed ID is attributed to the conversation that was
//! most recently active on that talkgroup. A different mobile unit keying up
//! starts a different incident, even mid-way through another.
//!
//! The conversation ends after `end_gap_secs` of silence. Then the
//! transcripts are stitched with speaker labels, the local model writes a
//! summary from the rule's prompt, the audio of every transmission is
//! combined into one clip, and it all goes to Telegram. A transmission
//! arriving within `late_window_secs` after that reopens the conversation:
//! the summary is redone, the earlier Telegram messages are deleted, and a
//! revised one is sent.
//!
//! Fixed IDs can also be learned: a radio heard in most conversations on a
//! talkgroup is proposed as fixed (shown in the UI; the listener accepts it).

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::alerts::CallFacts;
use crate::AppState;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Rule {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub tgs: Vec<u16>,
    /// The fixed party's radio IDs (hospital consoles).
    #[serde(default)]
    pub fixed_units: Vec<u32>,
    /// Also learn fixed IDs from traffic.
    #[serde(default = "t")]
    pub learn_fixed: bool,
    /// Silence that ends the conversation.
    pub end_gap_secs: u32,
    /// A transmission within this long after the summary went out reopens
    /// the conversation and revises the summary.
    pub late_window_secs: u32,
    /// A conversation longer than this is summarised regardless.
    pub max_secs: u32,
    /// Fewer transmissions than this are not worth a summary.
    pub min_calls: u32,
    /// Instruction to the model.
    pub summary_prompt: String,
    /// Message template: `{summary} {rule} {tg} {tgname} {units} {unitnames}
    /// {calls} {duration} {started} {transcript} {revision}`.
    pub message: String,
    /// Telegram chat; blank = the alerts' chat.
    #[serde(default)]
    pub chat_id: String,
    #[serde(default = "t")]
    pub attach_audio: bool,
    /// Send even when no transcript arrived (audio + placeholder).
    #[serde(default)]
    pub send_without_transcript: bool,
}

fn t() -> bool {
    true
}

impl Default for Rule {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            enabled: true,
            tgs: Vec::new(),
            fixed_units: Vec::new(),
            learn_fixed: true,
            end_gap_secs: 90,
            late_window_secs: 180,
            max_secs: 900,
            min_calls: 1,
            summary_prompt: "Summarise this EMS-to-hospital radio report for a clinician in two or three sentences: unit, patient age/sex, chief complaint, vitals or interventions mentioned, and ETA. Use only what was said; mark anything unclear as unclear.".into(),
            message: "🏥 {rule} · {tgname}\n{summary}\n\n{unitnames} · {calls} transmissions · {duration} · {started}{revision}".into(),
            chat_id: String::new(),
            attach_audio: true,
            send_without_transcript: false,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Settings {
    pub rules: Vec<Rule>,
    /// Learned fixed IDs per (rule id, talkgroup): unit → conversations seen in.
    #[serde(default)]
    pub learned: HashMap<String, HashMap<u32, u32>>,
    /// Conversations seen per (rule id, talkgroup), the denominator.
    #[serde(default)]
    pub seen: HashMap<String, u32>,
}

#[derive(Serialize, Clone, Debug)]
pub struct Piece {
    pub id: Option<i64>,
    pub unit: u32,
    pub unit_name: Option<String>,
    pub fixed: bool,
    pub at: i64,
    pub secs: f64,
    pub audio: Option<String>,
    pub transcript: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct Conversation {
    pub key: u64,
    pub rule_id: String,
    pub rule_name: String,
    pub tg: u16,
    pub tg_name: String,
    pub mobile_unit: Option<u32>,
    pub pieces: Vec<Piece>,
    pub first_at: i64,
    pub last_at: i64,
    /// Telegram message ids of the summary sent so far (to delete on revision).
    pub sent_ids: Vec<i64>,
    pub sent_chat: String,
    pub sent_at: Option<i64>,
    /// Transmissions added since the last send.
    pub dirty: bool,
    pub revision: u32,
    /// A summary is being produced right now.
    pub busy: bool,
    /// Failed send attempts, and when the next may be made.
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub retry_after: i64,
    pub last_summary: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct LogEntry {
    pub at: i64,
    pub rule: String,
    pub tg_name: String,
    pub units: String,
    pub calls: usize,
    pub revision: u32,
    pub ok: bool,
    pub detail: String,
    pub summary: String,
}

#[derive(Default)]
pub struct ConvState {
    pub settings: Settings,
    pub open: Vec<Conversation>,
    pub log: VecDeque<LogEntry>,
    next_key: u64,
}

pub type Shared = Mutex<ConvState>;

fn path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let d = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("config dir: {e}"))?;
    std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    Ok(d.join("conversations.json"))
}

pub fn load(app: &AppHandle) -> ConvState {
    ConvState {
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

fn learn_key(rule: &str, tg: u16) -> String {
    format!("{rule}:{tg}")
}

/// Is this radio the fixed party for the rule on this talkgroup — listed,
/// or learned (heard in at least 3 conversations and most of them)?
pub fn is_fixed(s: &Settings, r: &Rule, tg: u16, unit: u32) -> bool {
    if r.fixed_units.contains(&unit) {
        return true;
    }
    if !r.learn_fixed {
        return false;
    }
    let k = learn_key(&r.id, tg);
    let total = s.seen.get(&k).copied().unwrap_or(0);
    let n = s
        .learned
        .get(&k)
        .and_then(|m| m.get(&unit))
        .copied()
        .unwrap_or(0);
    n >= 3 && total > 0 && n * 10 >= total * 6
}

/// A completed transmission: attach it to a conversation, or open one.
pub fn on_call(app: &AppHandle, f: &CallFacts) {
    let state = app.state::<AppState>();
    let mut st = state.conversations.lock().unwrap();
    let rules: Vec<Rule> = st
        .settings
        .rules
        .iter()
        .filter(|r| r.enabled && r.tgs.contains(&f.tg))
        .cloned()
        .collect();
    if rules.is_empty() {
        return;
    }
    let now = f.start;
    for r in rules {
        let fixed = f.unit == 0 || is_fixed(&st.settings, &r, f.tg, f.unit);
        let piece = Piece {
            id: f.id,
            unit: f.unit,
            unit_name: f.unit_name.clone(),
            fixed,
            at: now,
            secs: f.secs,
            audio: f.audio.clone(),
            transcript: f.transcript.clone(),
        };
        let late = r.late_window_secs as i64;
        let gap = r.end_gap_secs as i64;
        // Which open conversation does this belong to?
        let idx = if !fixed {
            st.open
                .iter()
                .position(|c| {
                    c.rule_id == r.id
                        && c.tg == f.tg
                        && c.mobile_unit == Some(f.unit)
                        && now - c.last_at <= gap + late
                })
                .or_else(|| {
                    // A hospital-initiated conversation with no mobile unit yet.
                    st.open.iter().position(|c| {
                        c.rule_id == r.id
                            && c.tg == f.tg
                            && c.mobile_unit.is_none()
                            && now - c.last_at <= gap
                    })
                })
        } else {
            // The fixed party: the most recently active conversation here.
            st.open
                .iter()
                .enumerate()
                .filter(|(_, c)| c.rule_id == r.id && c.tg == f.tg && now - c.last_at <= gap + late)
                .max_by_key(|(_, c)| c.last_at)
                .map(|(i, _)| i)
        };
        match idx {
            Some(i) => {
                let c = &mut st.open[i];
                if c.mobile_unit.is_none() && !fixed {
                    c.mobile_unit = Some(f.unit);
                }
                c.pieces.push(piece);
                c.last_at = now;
                if c.sent_at.is_some() {
                    c.dirty = true;
                }
            }
            None => {
                st.next_key += 1;
                let key = st.next_key;
                st.open.push(Conversation {
                    key,
                    rule_id: r.id.clone(),
                    rule_name: r.name.clone(),
                    tg: f.tg,
                    tg_name: f.tg_name.clone(),
                    mobile_unit: (!fixed).then_some(f.unit),
                    pieces: vec![piece],
                    first_at: now,
                    last_at: now,
                    sent_ids: Vec::new(),
                    sent_chat: String::new(),
                    sent_at: None,
                    dirty: false,
                    revision: 0,
                    busy: false,
                    attempts: 0,
                    retry_after: 0,
                    last_summary: None,
                    last_error: None,
                });
            }
        }
    }
    let _ = app.emit("conversations", ());
}

/// A transcript arrived for a library call: fill it into any conversation.
pub fn on_transcript(app: &AppHandle, id: i64, text: &str) {
    let state = app.state::<AppState>();
    let mut st = state.conversations.lock().unwrap();
    let mut hit = false;
    for c in st.open.iter_mut() {
        for p in c.pieces.iter_mut() {
            if p.id == Some(id) {
                p.transcript = Some(text.to_string());
                hit = true;
            }
        }
    }
    if hit {
        let _ = app.emit("conversations", ());
    }
}

/// Periodic: end conversations that have gone quiet, summarise and send,
/// revise ones that were reopened, forget ones past their late window.
pub fn spawn_ticker(app: AppHandle) {
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(5));
        let state = app.state::<AppState>();
        let now = crate::library::now();
        let transcribing = state.transcriber.lock().unwrap().settings.enabled;
        let mut due: Vec<Conversation> = Vec::new();
        {
            let mut st = state.conversations.lock().unwrap();
            let rules = st.settings.rules.clone();
            let mut keep: Vec<Conversation> = Vec::new();
            for mut c in std::mem::take(&mut st.open) {
                let Some(r) = rules.iter().find(|r| r.id == c.rule_id) else {
                    eprintln!("conversation on TG {} dropped: its rule was deleted", c.tg);
                    continue;
                };
                let quiet = now - c.last_at;
                let gap = r.end_gap_secs as i64;
                let late = r.late_window_secs as i64;
                if c.busy || now < c.retry_after {
                    keep.push(c);
                    continue;
                }
                let ended = quiet >= gap || now - c.first_at >= r.max_secs as i64;
                let needs_send = c.sent_at.is_none() || c.dirty;
                if ended && needs_send {
                    // Give transcription a chance to catch up, but not forever.
                    let missing = c
                        .pieces
                        .iter()
                        .any(|p| p.audio.is_some() && p.transcript.is_none());
                    if transcribing && missing && quiet < gap + 90 {
                        keep.push(c);
                        continue;
                    }
                    if (c.pieces.len() as u32) < r.min_calls.max(1) {
                        learn(&mut st.settings, r, &c);
                        continue; // too small to bother; dropped (still counts toward learning)
                    }
                    c.busy = true;
                    due.push(c.clone());
                    keep.push(c);
                } else if c.sent_at.is_some() && !c.dirty && quiet >= gap + late {
                    // Closed for good; learn the fixed party from it.
                    learn(&mut st.settings, r, &c);
                    let _ = store(&app, &st.settings);
                } else {
                    keep.push(c);
                }
            }
            st.open = keep;
        }
        for c in due {
            let app2 = app.clone();
            std::thread::spawn(move || summarise_and_send(app2, c));
        }
    });
}

fn learn(s: &mut Settings, r: &Rule, c: &Conversation) {
    if !r.learn_fixed {
        return;
    }
    let k = learn_key(&r.id, c.tg);
    *s.seen.entry(k.clone()).or_default() += 1;
    let m = s.learned.entry(k).or_default();
    let mut units: Vec<u32> = c
        .pieces
        .iter()
        .map(|p| p.unit)
        .filter(|u| *u != 0)
        .collect();
    units.sort_unstable();
    units.dedup();
    for u in units {
        *m.entry(u).or_default() += 1;
    }
}

fn fmt_duration(secs: i64) -> String {
    if secs < 60 {
        format!("{secs} s")
    } else {
        format!("{}m {:02}s", secs / 60, secs % 60)
    }
}

fn fmt_time(epoch: i64) -> String {
    let s = epoch.rem_euclid(86_400);
    format!("{:02}:{:02} UTC", s / 3600, (s % 3600) / 60)
}

/// The stitched transcript with speaker labels, oldest first.
pub fn stitched_transcript(c: &Conversation) -> String {
    let mut out = String::new();
    for p in &c.pieces {
        let who = if p.fixed {
            "FIXED (hospital)".to_string()
        } else {
            format!(
                "UNIT {}",
                p.unit_name.clone().unwrap_or_else(|| p.unit.to_string())
            )
        };
        let text = p
            .transcript
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .unwrap_or("[no transcript]");
        out.push_str(&format!("{who}: {text}\n"));
    }
    out
}

pub fn render(r: &Rule, c: &Conversation, summary: &str) -> String {
    let units: Vec<String> = {
        let mut v: Vec<u32> = c
            .pieces
            .iter()
            .filter(|p| !p.fixed)
            .map(|p| p.unit)
            .collect();
        v.sort_unstable();
        v.dedup();
        v.iter().map(|u| u.to_string()).collect()
    };
    let unit_names: Vec<String> = {
        let mut v: Vec<String> = c
            .pieces
            .iter()
            .filter(|p| !p.fixed)
            .map(|p| p.unit_name.clone().unwrap_or_else(|| p.unit.to_string()))
            .collect();
        v.dedup();
        v
    };
    r.message
        .replace("{summary}", summary)
        .replace("{rule}", &r.name)
        .replace("{tg}", &c.tg.to_string())
        .replace("{tgname}", &c.tg_name)
        .replace("{units}", &units.join(", "))
        .replace("{unitnames}", &unit_names.join(", "))
        .replace("{calls}", &c.pieces.len().to_string())
        .replace("{duration}", &fmt_duration(c.last_at - c.first_at))
        .replace("{started}", &fmt_time(c.first_at))
        .replace("{transcript}", &stitched_transcript(c))
        .replace(
            "{revision}",
            &if c.revision > 0 {
                format!(" · revised ×{}", c.revision)
            } else {
                String::new()
            },
        )
        .trim()
        .to_string()
}

/// Apply the outcome of a summary to the live conversation. `sent_pieces` is
/// how many transmissions the summary covered: anything that arrived while
/// the model was thinking keeps the conversation dirty, so the next tick
/// revises rather than silently missing the ETA exchange.
fn finish(app: &AppHandle, key: u64, sent_pieces: usize, f: impl FnOnce(&mut Conversation)) {
    let state = app.state::<AppState>();
    let mut st = state.conversations.lock().unwrap();
    if let Some(c) = st.open.iter_mut().find(|c| c.key == key) {
        c.busy = false;
        f(c);
        if c.pieces.len() > sent_pieces {
            c.dirty = true;
        }
    }
    let _ = app.emit("conversations", ());
}

fn log_it(app: &AppHandle, r: &Rule, c: &Conversation, ok: bool, detail: String, summary: String) {
    let state = app.state::<AppState>();
    let mut st = state.conversations.lock().unwrap();
    st.log.push_front(LogEntry {
        at: crate::library::now(),
        rule: r.name.clone(),
        tg_name: c.tg_name.clone(),
        units: c
            .pieces
            .iter()
            .filter(|p| !p.fixed)
            .map(|p| p.unit_name.clone().unwrap_or_else(|| p.unit.to_string()))
            .collect::<Vec<_>>()
            .join(", "),
        calls: c.pieces.len(),
        revision: c.revision,
        ok,
        detail: detail.clone(),
        summary,
    });
    st.log.truncate(200);
    if !ok {
        let _ = app.emit("alert_error", format!("conversation {}: {detail}", r.name));
    }
}

/// Summarise one conversation and send (or re-send) it.
fn summarise_and_send(app: AppHandle, c: Conversation) {
    let state = app.state::<AppState>();
    let rule = state
        .conversations
        .lock()
        .unwrap()
        .settings
        .rules
        .iter()
        .find(|r| r.id == c.rule_id)
        .cloned();
    let Some(r) = rule else {
        finish(&app, c.key, c.pieces.len(), |_| {});
        return;
    };
    summarise_and_send_with(app, c, r);
}

/// Retries before a conversation whose send keeps failing is given up.
const MAX_ATTEMPTS: u32 = 3;

/// The rule is passed by value so a test can override flags without
/// touching the saved settings underneath a concurrent Save.
fn summarise_and_send_with(app: AppHandle, c: Conversation, r: Rule) {
    let state = app.state::<AppState>();
    let n_pieces = c.pieces.len();
    let (tg, ollama) = crate::alerts::shared_settings(&state);
    let chat = if r.chat_id.trim().is_empty() {
        tg.chat_id.clone()
    } else {
        r.chat_id.clone()
    };
    let has_text = c.pieces.iter().any(|p| {
        p.transcript
            .as_deref()
            .is_some_and(|t| !t.trim().is_empty())
    });
    if !has_text && !r.send_without_transcript {
        let why = "no transcript arrived (is transcription enabled?) — summary skipped".to_string();
        log_it(&app, &r, &c, false, why.clone(), String::new());
        finish(&app, c.key, n_pieces, |cc| {
            cc.last_error = Some(why);
            cc.sent_at = Some(crate::library::now());
            cc.dirty = false;
        });
        return;
    }
    // 1. Summary.
    let transcript = stitched_transcript(&c);
    let summary = if has_text {
        let prompt = format!(
            "{}\n\nTalkgroup: {} (TG {}). Speakers are labelled UNIT (a mobile radio) and FIXED (the fixed party). The transcript is machine-generated from radio audio and may contain recognition errors.\n\nTranscript:\n{}\n\nSummary:",
            r.summary_prompt.trim(),
            c.tg_name,
            c.tg,
            transcript
        );
        match crate::alerts::ollama_complete(&ollama, &prompt) {
            Ok(s) => s,
            Err(e) => format!("(summary unavailable: {e})\n{}", transcript.trim()),
        }
    } else {
        "(no transcript — audio only)".to_string()
    };
    let message = render(&r, &c, &summary);
    // 2. Delete what an earlier revision sent.
    let mut detail = String::new();
    if !c.sent_ids.is_empty() {
        for id in &c.sent_ids {
            if let Err(e) = crate::alerts::delete_message(&c.sent_chat, *id) {
                detail.push_str(&format!("could not delete earlier message {id}: {e}; "));
            }
        }
    }
    // 3. Send, with the combined audio.
    let files: Vec<String> = c.pieces.iter().filter_map(|p| p.audio.clone()).collect();
    let sent = if r.attach_audio && !files.is_empty() {
        match crate::alerts::combine_clips(&files, &format!("conv_{}", c.tg)) {
            Ok((path, mp3)) => {
                let res = crate::alerts::send_audio_id(
                    &chat,
                    &path,
                    mp3,
                    &message,
                    &format!("{} · {}", r.name, c.tg_name),
                    &c.pieces
                        .iter()
                        .find(|p| !p.fixed)
                        .and_then(|p| p.unit_name.clone())
                        .unwrap_or_else(|| c.tg_name.clone()),
                );
                let _ = std::fs::remove_file(&path);
                res
            }
            Err(e) => {
                detail.push_str(&format!("audio: {e}; "));
                crate::alerts::send_text_id(&chat, &message).map(|i| vec![i])
            }
        }
    } else {
        crate::alerts::send_text_id(&chat, &message).map(|i| vec![i])
    };
    let _ = app.emit(
        "alert",
        serde_json::json!({ "name": r.name, "tg": c.tg, "message": message, "tone": false }),
    );
    match sent {
        Ok(ids) => {
            detail.push_str(&format!("sent ({} pieces)", c.pieces.len()));
            let revision = if c.sent_at.is_some() {
                c.revision + 1
            } else {
                0
            };
            log_it(
                &app,
                &r,
                &Conversation {
                    revision,
                    ..c.clone()
                },
                true,
                detail,
                summary.clone(),
            );
            finish(&app, c.key, n_pieces, |cc| {
                cc.sent_ids = ids;
                cc.sent_chat = chat;
                cc.sent_at = Some(crate::library::now());
                cc.revision = revision;
                cc.dirty = false;
                cc.last_summary = Some(summary);
                cc.last_error = None;
            });
        }
        Err(e) => {
            log_it(&app, &r, &c, false, format!("{detail}{e}"), summary.clone());
            finish(&app, c.key, n_pieces, |cc| {
                cc.attempts += 1;
                cc.last_summary = Some(summary);
                if cc.attempts >= MAX_ATTEMPTS {
                    // Give up: mark as sent-and-clean so it closes after the
                    // late window instead of re-running the model every tick.
                    cc.last_error = Some(format!("gave up after {} attempts: {e}", cc.attempts));
                    cc.sent_at = Some(crate::library::now());
                    cc.dirty = false;
                } else {
                    cc.last_error = Some(e);
                    // Try again next tick, with a delay that grows.
                    cc.dirty = true;
                    cc.sent_at = cc.sent_at.or(Some(0));
                    cc.retry_after = crate::library::now() + 30 * cc.attempts as i64;
                }
            });
        }
    }
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct View {
    pub settings: Settings,
    /// Learned fixed IDs that pass the threshold, per "rule:tg".
    pub proposed_fixed: HashMap<String, Vec<u32>>,
}

#[tauri::command]
pub fn conversations_get(state: State<AppState>) -> View {
    let st = state.conversations.lock().unwrap();
    let mut proposed = HashMap::new();
    for r in &st.settings.rules {
        for tg in &r.tgs {
            let k = learn_key(&r.id, *tg);
            let units: Vec<u32> = st
                .settings
                .learned
                .get(&k)
                .map(|m| {
                    m.keys()
                        .copied()
                        .filter(|u| {
                            is_fixed(&st.settings, r, *tg, *u) && !r.fixed_units.contains(u)
                        })
                        .collect()
                })
                .unwrap_or_default();
            if !units.is_empty() {
                proposed.insert(k, units);
            }
        }
    }
    View {
        settings: st.settings.clone(),
        proposed_fixed: proposed,
    }
}

#[tauri::command]
pub fn conversations_set(
    app: AppHandle,
    state: State<AppState>,
    rules: Vec<Rule>,
) -> Result<(), String> {
    let mut rules = rules;
    for (i, r) in rules.iter_mut().enumerate() {
        if r.id.trim().is_empty() {
            r.id = format!("c{}-{i}", crate::library::now());
        }
        if r.name.trim().is_empty() {
            r.name = format!("Conversation rule {}", i + 1);
        }
        r.end_gap_secs = r.end_gap_secs.clamp(10, 3600);
        r.late_window_secs = r.late_window_secs.min(3600);
        r.max_secs = r.max_secs.clamp(60, 7200);
    }
    let mut st = state.conversations.lock().unwrap();
    st.settings.rules = rules;
    store(&app, &st.settings)
}

#[derive(Serialize)]
pub struct StateView {
    pub open: Vec<Conversation>,
    pub log: Vec<LogEntry>,
}

#[tauri::command]
pub fn conversations_state(state: State<AppState>) -> StateView {
    let st = state.conversations.lock().unwrap();
    StateView {
        open: st.open.clone(),
        log: st.log.iter().cloned().collect(),
    }
}

/// Build a conversation from the newest calls on the rule's talkgroups
/// (within one end-gap of each other) and run the whole path on it —
/// summary, audio, Telegram — so the rule can be seen working.
#[tauri::command]
pub async fn conversation_test(app: AppHandle, id: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let (rule, settings) = {
            let st = state.conversations.lock().unwrap();
            (
                st.settings
                    .rules
                    .iter()
                    .find(|r| r.id == id)
                    .cloned()
                    .ok_or("no such rule")?,
                st.settings.clone(),
            )
        };
        let rows = {
            let db = state.db.lock().unwrap().clone().ok_or("library not open")?;
            let c = db.lock().unwrap();
            let list = rule
                .tgs
                .iter()
                .map(|t| t.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let sql = if rule.tgs.is_empty() {
                "SELECT id FROM calls ORDER BY id DESC LIMIT 12".to_string()
            } else {
                format!("SELECT id FROM calls WHERE tg IN ({list}) ORDER BY id DESC LIMIT 12")
            };
            let mut stmt = c.prepare(&sql).map_err(|e| e.to_string())?;
            let ids: Vec<i64> = stmt
                .query_map([], |r| r.get(0))
                .map_err(|e| e.to_string())?
                .filter_map(Result::ok)
                .collect();
            let mut rows = Vec::new();
            for id in ids {
                if let Some(r) = crate::library::get(&c, id)? {
                    rows.push(r);
                }
            }
            rows
        };
        if rows.is_empty() {
            return Err("no calls on those talkgroups in the library yet".into());
        }
        // Newest first → keep the run that belongs together, oldest first.
        let mut pieces: Vec<Piece> = Vec::new();
        let mut last = rows[0].start;
        for r in rows {
            if last - r.start > rule.end_gap_secs as i64 && !pieces.is_empty() {
                break;
            }
            last = r.start;
            pieces.push(Piece {
                id: Some(r.id),
                unit: r.unit,
                unit_name: r.unit_name.clone(),
                fixed: r.unit == 0 || is_fixed(&settings, &rule, r.tg, r.unit),
                at: r.start,
                secs: r.secs,
                audio: r.audio.clone(),
                transcript: r.transcript_edited.or(r.transcript),
            });
        }
        pieces.reverse();
        let tg = pieces
            .first()
            .map(|_| rule.tgs.first().copied().unwrap_or(0))
            .unwrap_or(0);
        let c = Conversation {
            key: 0,
            rule_id: rule.id.clone(),
            rule_name: rule.name.clone(),
            tg,
            tg_name: format!("TG {tg}"),
            mobile_unit: pieces.iter().find(|p| !p.fixed).map(|p| p.unit),
            first_at: pieces.first().map(|p| p.at).unwrap_or(0),
            last_at: pieces.last().map(|p| p.at).unwrap_or(0),
            pieces,
            sent_ids: Vec::new(),
            sent_chat: String::new(),
            sent_at: None,
            dirty: false,
            revision: 0,
            busy: true,
            attempts: 0,
            retry_after: 0,
            last_summary: None,
            last_error: None,
        };
        let n = c.pieces.len();
        let mut test = rule.clone();
        test.send_without_transcript = true;
        let app2 = app.clone();
        std::thread::spawn(move || summarise_and_send_with(app2, c, test));
        Ok(format!(
            "summarising the last {n} transmission(s) on the rule's talkgroups — watch the log"
        ))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Re-send a conversation's summary now (revision), e.g. after fixing the prompt.
#[tauri::command]
pub fn conversation_resend(app: AppHandle, state: State<AppState>, key: u64) -> Result<(), String> {
    let c = {
        let mut st = state.conversations.lock().unwrap();
        let c = st
            .open
            .iter_mut()
            .find(|c| c.key == key)
            .ok_or("that conversation is gone")?;
        if c.busy {
            return Err("already working on it".into());
        }
        c.busy = true;
        c.clone()
    };
    std::thread::spawn(move || summarise_and_send(app, c));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule() -> Rule {
        Rule {
            id: "r".into(),
            name: "Hospitals".into(),
            tgs: vec![10202],
            fixed_units: vec![900001],
            ..Default::default()
        }
    }

    #[test]
    fn fixed_ids_are_listed_or_learned() {
        let r = rule();
        let mut s = Settings::default();
        assert!(is_fixed(&s, &r, 10202, 900001));
        assert!(!is_fixed(&s, &r, 10202, 790065));
        // Learned: seen in 3 of 4 conversations.
        let k = learn_key("r", 10202);
        s.seen.insert(k.clone(), 4);
        s.learned.entry(k).or_default().insert(790065, 3);
        assert!(is_fixed(&s, &r, 10202, 790065));
        let mut off = r.clone();
        off.learn_fixed = false;
        assert!(!is_fixed(&s, &off, 10202, 790065));
    }

    #[test]
    fn transcript_is_stitched_with_speaker_labels_and_message_renders() {
        let r = rule();
        let c = Conversation {
            key: 1,
            rule_id: "r".into(),
            rule_name: "Hospitals".into(),
            tg: 10202,
            tg_name: "Methodist ER".into(),
            mobile_unit: Some(790065),
            pieces: vec![
                Piece {
                    id: Some(1),
                    unit: 790065,
                    unit_name: Some("Medic 3".into()),
                    fixed: false,
                    at: 100,
                    secs: 8.0,
                    audio: None,
                    transcript: Some("Medic 3 inbound, 64 year old male chest pain".into()),
                },
                Piece {
                    id: Some(2),
                    unit: 900001,
                    unit_name: None,
                    fixed: true,
                    at: 120,
                    secs: 3.0,
                    audio: None,
                    transcript: Some("Copy, ETA?".into()),
                },
                Piece {
                    id: Some(3),
                    unit: 790065,
                    unit_name: Some("Medic 3".into()),
                    fixed: false,
                    at: 130,
                    secs: 2.0,
                    audio: None,
                    transcript: None,
                },
            ],
            first_at: 100,
            last_at: 130,
            sent_ids: vec![],
            sent_chat: String::new(),
            sent_at: None,
            dirty: false,
            revision: 1,
            busy: false,
            attempts: 0,
            retry_after: 0,
            last_summary: None,
            last_error: None,
        };
        let t = stitched_transcript(&c);
        assert_eq!(t, "UNIT Medic 3: Medic 3 inbound, 64 year old male chest pain\nFIXED (hospital): Copy, ETA?\nUNIT Medic 3: [no transcript]\n");
        let m = render(&r, &c, "Chest pain, ETA unknown.");
        assert!(
            m.starts_with("🏥 Hospitals · Methodist ER\nChest pain, ETA unknown."),
            "{m}"
        );
        assert!(
            m.contains("Medic 3 · 3 transmissions · 30 s · 00:01 UTC · revised ×1"),
            "{m}"
        );
    }
}

#[cfg(test)]
mod payload_tests {
    use super::*;

    /// Exactly what the page sends on Save must deserialize.
    #[test]
    fn the_pages_rule_payload_deserializes() {
        let js = r#"[{"id":"c1755800000000","name":"Hospitals","enabled":true,"tgs":[10202,10244],"fixed_units":[],"learn_fixed":true,"end_gap_secs":90,"late_window_secs":180,"max_secs":900,"min_calls":1,"summary_prompt":"x","message":"y","chat_id":"","attach_audio":true,"send_without_transcript":false}]"#;
        let rules: Vec<Rule> = serde_json::from_str(js).expect("rule payload");
        assert_eq!(rules[0].tgs, vec![10202, 10244]);
        let s = Settings {
            rules,
            ..Default::default()
        };
        let text = serde_json::to_string_pretty(&s).unwrap();
        let back: Settings = serde_json::from_str(&text).unwrap();
        assert_eq!(back.rules.len(), 1);
    }
}
