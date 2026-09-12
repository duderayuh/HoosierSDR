//! Conversations: the back-and-forth on a talkgroup, treated as one incident.
//!
//! An EMS unit calls the hospital, the hospital asks a couple of questions,
//! the unit gives an ETA, and that is the call — four or five transmissions
//! that belong together. A rule names the talkgroups this applies to and the
//! **fixed** radio IDs (the hospital's consoles); every other radio is a
//! mobile unit, and a conversation is keyed by (talkgroup, mobile unit). A
//! second radio keying up within `reply_gap_secs` of a conversation that has
//! only one mobile party so far is the other side of that exchange — two
//! radios taking turns on a talkgroup are talking to each other, whichever
//! of them is really the console — so it joins as a second participant
//! instead of opening a duplicate incident (which is what happened before
//! the fixed IDs were known: one summary per radio, each half a
//! conversation). A third radio, or one arriving after the reply gap,
//! starts its own incident; a radio that already has a live conversation
//! stays in it. A
//! transmission from a fixed ID is attributed to the incident it is part of:
//! the most recently active conversation on that talkgroup, preferring one
//! that has not been summarised yet so the hospital's side lands on the live
//! incident (and its summary carries both sides) rather than reopening an
//! already-sent one and spamming a duplicate revision. A different mobile
//! unit keying up starts a different incident, even mid-way through another.
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
    /// A different radio keying up within this long after a conversation's
    /// last transmission is replying to it (when that conversation has only
    /// one mobile party so far).
    #[serde(default = "default_reply_gap")]
    pub reply_gap_secs: u32,
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
fn default_reply_gap() -> u32 {
    45
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
            reply_gap_secs: 45,
            late_window_secs: 180,
            max_secs: 900,
            min_calls: 1,
            summary_prompt: "Summarise this EMS-to-hospital radio report as a hand-off note for the receiving clinician: which unit is coming and where, patient age/sex, chief complaint, pertinent findings and vitals, interventions given, ETA, and anything the hospital asked for.".into(),
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

#[derive(Serialize, Deserialize, Clone, Debug)]
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
    /// RadioReference "Description"; becomes the `{tgdesc}` token.
    #[serde(default)]
    pub tg_desc: Option<String>,
    pub mobile_unit: Option<u32>,
    /// Every mobile (non-fixed) radio in the exchange, in order of first
    /// appearance; `mobile_unit` is the first of them.
    #[serde(default)]
    pub participants: Vec<u32>,
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
    // A hospital's console shows up in a third or more of its talkgroup's
    // conversations; no ambulance does. Sixty percent (the old bar) missed
    // a console heard in 50 of 107, because the doctor's handset, the
    // operator's, and one-sided calls each take a share.
    n >= 3 && total > 0 && n * 3 >= total
}

/// Which open conversation a transmission belongs to (index into `open`), or
/// `None` to start a new one.
///
/// A mobile unit first looks for its own live thread — a conversation it is
/// already a participant of. Failing that, it may be *replying*: if a
/// conversation on the talkgroup heard its last transmission within
/// `reply_gap` and has fewer than two mobile parties, this radio is the other
/// side of that exchange and joins it (this is what makes an EMS unit and a
/// hospital console land in one incident before either is known as fixed).
/// Otherwise it joins a hospital-initiated conversation that has no mobile
/// unit yet, or opens a new incident — so a third radio, or one keying up
/// after the reply gap, is its own incident even mid-way through another.
///
/// The fixed party (a hospital console) is not an incident of its own — its
/// transmission attaches to the incident it is part of, so both sides of the
/// exchange end up in one conversation and one summary. That is the most
/// recently active conversation on the talkgroup, but a conversation still
/// awaiting its summary is preferred over one already sent: hospital traffic
/// for a live incident then lands on that incident instead of reopening a
/// closed one and triggering a duplicate, still one-sided, revision. A sent
/// conversation is only reopened (within its late window) when no live one is
/// open — a genuine late follow-up.
#[allow(clippy::too_many_arguments)]
fn attach_index(
    open: &[Conversation],
    rule_id: &str,
    tg: u16,
    unit: u32,
    fixed: bool,
    now: i64,
    gap: i64,
    late: i64,
    reply_gap: i64,
) -> Option<usize> {
    let here = |c: &Conversation| c.rule_id == rule_id && c.tg == tg;
    if !fixed {
        // Own thread first.
        let own = open
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                here(c) && c.participants.contains(&unit) && now - c.last_at <= gap + late
            })
            .max_by_key(|(_, c)| c.last_at)
            .map(|(i, _)| i);
        if own.is_some() {
            return own;
        }
        // A reply to a conversation still waiting for its other side.
        let reply = open
            .iter()
            .enumerate()
            .filter(|(_, c)| here(c) && c.participants.len() < 2 && now - c.last_at <= reply_gap)
            .max_by_key(|(_, c)| c.last_at)
            .map(|(i, _)| i);
        if reply.is_some() {
            return reply;
        }
        // A hospital-initiated conversation with no mobile unit yet.
        return open
            .iter()
            .position(|c| here(c) && c.participants.is_empty() && now - c.last_at <= gap);
    }
    // The fixed party: the most recently active conversation here, live first.
    let most_recent = |sent: bool| {
        open.iter()
            .enumerate()
            .filter(|(_, c)| {
                here(c) && c.sent_at.is_some() == sent && now - c.last_at <= gap + late
            })
            .max_by_key(|(_, c)| c.last_at)
            .map(|(i, _)| i)
    };
    most_recent(false).or_else(|| most_recent(true))
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
        let reply_gap = (r.reply_gap_secs as i64).min(gap);
        // Which open conversation does this belong to?
        let idx = attach_index(
            &st.open, &r.id, f.tg, f.unit, fixed, now, gap, late, reply_gap,
        );
        match idx {
            Some(i) => {
                let c = &mut st.open[i];
                if !fixed && !c.participants.contains(&f.unit) {
                    c.participants.push(f.unit);
                }
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
                    tg_desc: f.tg_desc.clone(),
                    mobile_unit: (!fixed).then_some(f.unit),
                    participants: (!fixed).then_some(f.unit).into_iter().collect(),
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
                    let has_mobile = c.pieces.iter().any(|p| !p.fixed);
                    if !has_mobile || (c.pieces.len() as u32) < r.min_calls.max(1) {
                        // Too small, or only the fixed party spoke (a one-sided
                        // hospital-only exchange with no unit) — not worth a
                        // summary. Dropped, but still counts toward learning.
                        learn(&mut st.settings, r, &c);
                        continue;
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
    crate::library::local_hm(epoch)
}

/// The stitched transcript with speaker labels, oldest first.
///
/// Labels are radio *slots*, not identities: the fixed party is HOSPITAL and
/// each mobile radio is RADIO A, RADIO B… in order of first appearance (or
/// its alias when one is known). A raw radio ID never appears — fed one, the
/// model wrote "Unit 4917150 is bringing…" instead of reading the unit's
/// name (Medic 42, Ambulance 7) out of what was actually said.
pub fn stitched_transcript(c: &Conversation) -> String {
    let mut out = String::new();
    let mut slots: Vec<u32> = Vec::new();
    for p in &c.pieces {
        let who = if p.fixed {
            "HOSPITAL".to_string()
        } else if let Some(name) = p.unit_name.as_deref().filter(|n| !n.trim().is_empty()) {
            format!("RADIO \"{}\"", name.trim())
        } else {
            let slot = match slots.iter().position(|u| *u == p.unit) {
                Some(i) => i,
                None => {
                    slots.push(p.unit);
                    slots.len() - 1
                }
            };
            format!("RADIO {}", (b'A' + (slot % 26) as u8) as char)
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

/// The standing instructions every summary gets, on top of the rule's own
/// prompt: how to read the labels, what a good clinical hand-off contains,
/// and what not to do (name radio IDs, grade the transcript, guess).
pub const SUMMARY_GUIDE: &str = "How to read the transcript: it is machine-generated from radio audio and \
may contain recognition errors (mis-heard numbers, drug names, street names). Speaker labels are \
radio slots — HOSPITAL is the fixed party, RADIO A / RADIO B are the mobile radios (a label in quotes \
is that radio's alias). The labels are NOT unit names: identify the EMS unit from what is said \
(\"Medic 42\", \"Ambulance 7\", \"Engine 6\"), and if no unit name is spoken say \"the unit\". \
Never mention radio IDs, label letters, or that the text is a transcript.\n\n\
What to write: plain text, two or three sentences, no preamble, no headings, no markdown. Lead with \
the unit and destination if stated, then the patient (age/sex), chief complaint, pertinent findings \
and vitals, interventions given, and ETA; include any request or instruction from the hospital. \
Use only what was said — do not infer, and do not pad. Where something matters but was garbled or \
missing, write \"unclear\" for that item rather than describing the transcript's quality. If almost \
nothing is intelligible, write one sentence with whatever can be told (e.g. \"Medic 42 inbound with an \
adult patient, details unclear\").";

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
        .replace("{tgdesc}", c.tg_desc.as_deref().unwrap_or(""))
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
        tg.destination()
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
        store_outcome(
            &app,
            &r,
            &c,
            &Outcome {
                status: "skipped",
                detail: &why,
                summary: "",
                message: "",
                prompt: "",
                chat: &chat,
                revision: c.revision,
                message_ids: &[],
            },
        );
        finish(&app, c.key, n_pieces, |cc| {
            cc.last_error = Some(why);
            cc.sent_at = Some(crate::library::now());
            cc.dirty = false;
        });
        return;
    }
    // 1. Summary.
    let transcript = stitched_transcript(&c);
    let prompt = if has_text {
        format!(
            "{}\n\n{SUMMARY_GUIDE}\n\nTalkgroup: {} (TG {}).\n\nTranscript:\n{}\n\nSummary:",
            r.summary_prompt.trim(),
            c.tg_name,
            c.tg,
            transcript
        )
    } else {
        String::new()
    };
    let summary = if has_text {
        match crate::alerts::ollama_complete(&ollama, &prompt) {
            Ok(s) => s,
            Err(e) => format!("(summary unavailable: {e})\n{}", transcript.trim()),
        }
    } else {
        "(no transcript — audio only)".to_string()
    };
    let message = render(&r, &c, &summary);
    let files: Vec<String> = c.pieces.iter().filter_map(|p| p.audio.clone()).collect();
    let text_only = !r.attach_audio || files.is_empty();
    let mut detail = String::new();
    let mut reused_id: Option<i64> = None;

    // 2. Revise in place when we can (a single earlier message and a text-only
    // send); otherwise delete the old and send fresh. Audio can't be edited in
    // place, so those keep the delete + re-send behaviour.
    if text_only && c.sent_ids.len() == 1 {
        let id = c.sent_ids[0];
        match crate::alerts::edit_message(&c.sent_chat, id, &message) {
            Ok(()) => {
                reused_id = Some(id);
                detail.push_str("edited in place; ");
            }
            Err(e) => {
                detail.push_str(&format!("edit failed ({e}); "));
                if let Err(de) = crate::alerts::delete_message(&c.sent_chat, id) {
                    detail.push_str(&format!("could not delete earlier message {id}: {de}; "));
                }
            }
        }
    } else if !c.sent_ids.is_empty() {
        for id in &c.sent_ids {
            if let Err(e) = crate::alerts::delete_message(&c.sent_chat, *id) {
                detail.push_str(&format!("could not delete earlier message {id}: {e}; "));
            }
        }
    }

    // 3. Send (or reuse the edited message), with the combined audio.
    let sent = if let Some(id) = reused_id {
        Ok(vec![id])
    } else if r.attach_audio && !files.is_empty() {
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
                detail.clone(),
                summary.clone(),
            );
            store_outcome(
                &app,
                &r,
                &c,
                &Outcome {
                    status: "sent",
                    detail: &detail,
                    summary: &summary,
                    message: &message,
                    prompt: &prompt,
                    chat: &chat,
                    revision,
                    message_ids: &ids,
                },
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
            store_outcome(
                &app,
                &r,
                &c,
                &Outcome {
                    status: "failed",
                    detail: &format!("{detail}{e}"),
                    summary: &summary,
                    message: &message,
                    prompt: &prompt,
                    chat: &chat,
                    revision: c.revision,
                    message_ids: &[],
                },
            );
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

/// The rules, as compiled from the conversation tripwires. Learned consoles
/// and conversation counts are keyed by rule id and carry over.
pub fn set_rules(app: &AppHandle, rules: Vec<Rule>) {
    let state = app.state::<AppState>();
    let mut st = state.conversations.lock().unwrap();
    if st.settings.rules == rules {
        return;
    }
    st.settings.rules = rules;
    if let Err(e) = store(app, &st.settings) {
        eprintln!("conversations: {e}");
    }
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
            tg_desc: None,
            mobile_unit: pieces.iter().find(|p| !p.fixed).map(|p| p.unit),
            participants: {
                let mut v: Vec<u32> = Vec::new();
                for p in pieces.iter().filter(|p| !p.fixed) {
                    if !v.contains(&p.unit) {
                        v.push(p.unit);
                    }
                }
                v
            },
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

// ---------------------------------------------------------------------------
// stored conversations — the Conversations tab
// ---------------------------------------------------------------------------
//
// `open` is pruned once a conversation's late window passes and `log` is a
// short in-memory ring, so neither can show *what went out* an hour later.
// Every outcome of `summarise_and_send_with` — sent, failed, or skipped for
// want of a transcript — is written to a `conversations` table in the call
// library (`calls.db`, beside dispatch's incidents) with everything that went
// into the message: the pieces with their transcripts, the stitched transcript
// the model saw, the full prompt, the summary it returned, the rendered
// message, and the Telegram result. A revision updates the same row.

use rusqlite::{params, Connection, OptionalExtension};

pub fn ensure_schema(c: &Connection) {
    let _ = c.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS conversations (
            id INTEGER PRIMARY KEY,
            rule_id TEXT NOT NULL,
            rule_name TEXT NOT NULL DEFAULT '',
            tg INTEGER NOT NULL,
            tg_name TEXT NOT NULL DEFAULT '',
            tg_desc TEXT NOT NULL DEFAULT '',
            first_at INTEGER NOT NULL,
            last_at INTEGER NOT NULL,
            sent_at INTEGER NOT NULL DEFAULT 0,
            revision INTEGER NOT NULL DEFAULT 0,
            status TEXT NOT NULL DEFAULT '',
            detail TEXT NOT NULL DEFAULT '',
            summary TEXT NOT NULL DEFAULT '',
            message TEXT NOT NULL DEFAULT '',
            prompt TEXT NOT NULL DEFAULT '',
            transcript TEXT NOT NULL DEFAULT '',
            chat TEXT NOT NULL DEFAULT '',
            participants TEXT NOT NULL DEFAULT '[]',
            pieces TEXT NOT NULL DEFAULT '[]',
            calls INTEGER NOT NULL DEFAULT 0,
            source TEXT NOT NULL DEFAULT 'live'
        );
        CREATE UNIQUE INDEX IF NOT EXISTS conversations_key ON conversations(rule_id, tg, first_at);
        CREATE INDEX IF NOT EXISTS conversations_last ON conversations(last_at);
        "#,
    );
}

/// What one summary attempt produced, for the stored row.
struct Outcome<'a> {
    status: &'a str,
    detail: &'a str,
    summary: &'a str,
    message: &'a str,
    prompt: &'a str,
    chat: &'a str,
    revision: u32,
    /// Telegram message ids of what went out (for the tripwire history).
    message_ids: &'a [i64],
}

/// The display identity of a stored conversation: talkgroup and start time
/// never change across revisions, so this is stable where `key` is not.
pub fn conv_id(tg: u16, first_at: i64) -> String {
    format!("CONV-{tg}-{first_at}")
}

/// Insert or revise the stored row for `c`. Called with no lock held: the
/// library connection is its own mutex and `log_it` takes the conversations
/// one.
fn store_outcome(app: &AppHandle, r: &Rule, c: &Conversation, o: &Outcome) {
    let state = app.state::<AppState>();
    let Some(db) = state.db.lock().unwrap().clone() else {
        return;
    };
    let db = db.lock().unwrap();
    let source = if c.key == 0 { "test" } else { "live" };
    if let Err(e) = store_row(&db, r, c, o, source) {
        eprintln!("conversation store: {e}");
    }
    let ev = crate::events::NewEvent {
        source: "conversation",
        rule_id: r.id.clone(),
        rule_name: r.name.clone(),
        tg: c.tg,
        tg_name: c.tg_name.clone(),
        status: o.status.to_string(),
        detail: if source == "test" {
            format!("test run · {}", o.detail)
        } else {
            o.detail.to_string()
        },
        message: o.message.to_string(),
        chat: o.chat.to_string(),
        message_ids: o.message_ids.to_vec(),
        data: serde_json::json!({ "summary": o.summary, "revision": o.revision }).to_string(),
        calls: c.pieces.iter().filter_map(|p| p.id).collect(),
        ..Default::default()
    };
    if let Err(e) = crate::events::insert(&db, &ev, crate::library::now()) {
        eprintln!("conversation event: {e}");
    }
    // A hospital report belongs to a run somebody was dispatched to; see if
    // the crew who read it can be found in the dispatch history. Off the
    // send path (it may ask the local model) and never for a test run.
    let id: Option<i64> = db
        .query_row(
            "SELECT id FROM conversations WHERE rule_id = ?1 AND tg = ?2 AND first_at = ?3",
            params![r.id, c.tg, c.first_at],
            |row| row.get(0),
        )
        .ok();
    drop(db);
    if source == "live" {
        if let Some(id) = id {
            let app = app.clone();
            std::thread::spawn(move || {
                if let Some((incident, how)) = crate::link::try_link(&app, id) {
                    println!("[link] conversation {id} → incident {incident} ({how})");
                }
            });
        }
    }
}

/// The row for one outcome: insert on first send, update on a revision.
fn store_row(
    db: &Connection,
    r: &Rule,
    c: &Conversation,
    o: &Outcome,
    source: &str,
) -> Result<(), String> {
    let participants = serde_json::to_string(&c.participants).unwrap_or_else(|_| "[]".into());
    let pieces = serde_json::to_string(&c.pieces).unwrap_or_else(|_| "[]".into());
    let transcript = stitched_transcript(c);
    let now = crate::library::now();
    let existing: Option<i64> = db
        .query_row(
            "SELECT id FROM conversations WHERE rule_id = ?1 AND tg = ?2 AND first_at = ?3",
            params![r.id, c.tg, c.first_at],
            |row| row.get(0),
        )
        .optional()
        .unwrap_or(None);
    let res = match existing {
        Some(id) => db.execute(
            "UPDATE conversations SET rule_name = ?1, tg_name = ?2, tg_desc = ?3, last_at = ?4, sent_at = ?5, revision = ?6,
             status = ?7, detail = ?8, summary = ?9, message = ?10, prompt = ?11, transcript = ?12, chat = ?13,
             participants = ?14, pieces = ?15, calls = ?16 WHERE id = ?17",
            params![
                r.name, c.tg_name, c.tg_desc.clone().unwrap_or_default(), c.last_at, now, o.revision,
                o.status, o.detail, o.summary, o.message, o.prompt, transcript, o.chat,
                participants, pieces, c.pieces.len() as i64, id
            ],
        ),
        None => db.execute(
            "INSERT INTO conversations (rule_id, rule_name, tg, tg_name, tg_desc, first_at, last_at, sent_at, revision,
             status, detail, summary, message, prompt, transcript, chat, participants, pieces, calls, source)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
            params![
                r.id, r.name, c.tg, c.tg_name, c.tg_desc.clone().unwrap_or_default(), c.first_at, c.last_at, now, o.revision,
                o.status, o.detail, o.summary, o.message, o.prompt, transcript, o.chat,
                participants, pieces, c.pieces.len() as i64, source
            ],
        ),
    };
    res.map(|_| ()).map_err(|e| e.to_string())
}

/// One stored conversation, in full.
#[derive(Serialize, Clone, Debug)]
pub struct Stored {
    pub id: i64,
    pub conv_id: String,
    pub rule_id: String,
    pub rule_name: String,
    pub tg: u16,
    pub tg_name: String,
    pub tg_desc: String,
    pub first_at: i64,
    pub last_at: i64,
    pub sent_at: i64,
    pub revision: u32,
    pub status: String,
    pub detail: String,
    pub summary: String,
    pub message: String,
    pub prompt: String,
    pub transcript: String,
    pub chat: String,
    pub participants: Vec<u32>,
    pub pieces: Vec<Piece>,
    pub calls: u32,
    pub source: String,
    /// Mobile units by name (or ID), first appearance first.
    pub units: Vec<String>,
}

const STORED_COLS: &str = "id, rule_id, rule_name, tg, tg_name, tg_desc, first_at, last_at, sent_at, revision, status, detail, summary, message, prompt, transcript, chat, participants, pieces, calls, source";

fn stored_row(row: &rusqlite::Row) -> rusqlite::Result<Stored> {
    let tg = row.get::<_, i64>(3)? as u16;
    let first_at: i64 = row.get(6)?;
    let participants: Vec<u32> =
        serde_json::from_str(&row.get::<_, String>(17)?).unwrap_or_default();
    let pieces: Vec<Piece> = serde_json::from_str(&row.get::<_, String>(18)?).unwrap_or_default();
    let mut units: Vec<String> = pieces
        .iter()
        .filter(|p| !p.fixed)
        .map(|p| p.unit_name.clone().unwrap_or_else(|| p.unit.to_string()))
        .collect();
    let mut seen = std::collections::HashSet::new();
    units.retain(|u| seen.insert(u.clone()));
    Ok(Stored {
        id: row.get(0)?,
        conv_id: conv_id(tg, first_at),
        rule_id: row.get(1)?,
        rule_name: row.get(2)?,
        tg,
        tg_name: row.get(4)?,
        tg_desc: row.get(5)?,
        first_at,
        last_at: row.get(7)?,
        sent_at: row.get(8)?,
        revision: row.get::<_, i64>(9)? as u32,
        status: row.get(10)?,
        detail: row.get(11)?,
        summary: row.get(12)?,
        message: row.get(13)?,
        prompt: row.get(14)?,
        transcript: row.get(15)?,
        chat: row.get(16)?,
        participants,
        pieces,
        calls: row.get::<_, i64>(19)? as u32,
        source: row.get(20)?,
        units,
    })
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

/// Stored conversations, newest last activity first. `q` matches the
/// summary, message, transcript, talkgroup, rule, units and display ID;
/// `before` (a `last_at`) pages older.
#[tauri::command]
pub fn conversations_list(
    state: State<AppState>,
    q: Option<String>,
    tg: Option<u16>,
    before: Option<i64>,
    limit: Option<u32>,
) -> Result<Vec<Stored>, String> {
    with_db(&state, |c| list_rows(c, q.as_deref(), tg, before, limit))
}

fn list_rows(
    c: &Connection,
    q: Option<&str>,
    tg: Option<u16>,
    before: Option<i64>,
    limit: Option<u32>,
) -> Result<Vec<Stored>, String> {
    let limit = limit.unwrap_or(100).clamp(1, 500) as i64;
    let q = q.unwrap_or_default().trim();
    let like = format!("%{q}%");
    let tg_filter = tg.map(|t| t as i64).unwrap_or(-1);
    let before = before.unwrap_or(i64::MAX);
    {
        let mut stmt = c
            .prepare(&format!(
                "SELECT {STORED_COLS} FROM conversations
                 WHERE last_at < ?1 AND (?2 < 0 OR tg = ?2)
                   AND (?3 = '' OR summary LIKE ?4 OR message LIKE ?4 OR transcript LIKE ?4 OR tg_name LIKE ?4
                        OR tg_desc LIKE ?4 OR rule_name LIKE ?4 OR pieces LIKE ?4
                        OR ('CONV-' || tg || '-' || first_at) LIKE ?4)
                 ORDER BY last_at DESC LIMIT ?5"
            ))
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![before, tg_filter, q, like, limit], stored_row)
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }
}

#[tauri::command]
pub fn conversation_get(state: State<AppState>, id: i64) -> Result<Stored, String> {
    with_db(&state, |c| get_row(c, id))
}

fn get_row(c: &Connection, id: i64) -> Result<Stored, String> {
    {
        c.query_row(
            &format!("SELECT {STORED_COLS} FROM conversations WHERE id = ?1"),
            params![id],
            stored_row,
        )
        .optional()
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "that conversation is gone".to_string())
    }
}

#[tauri::command]
pub fn conversation_delete(state: State<AppState>, id: i64) -> Result<(), String> {
    with_db(&state, |c| delete_row(c, id))
}

fn delete_row(c: &Connection, id: i64) -> Result<(), String> {
    c.execute("DELETE FROM conversations WHERE id = ?1", params![id])
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Totals for the tab's side panel.
#[derive(Serialize, Default)]
pub struct StoredStats {
    pub total: u32,
    pub sent: u32,
    pub failed: u32,
    pub skipped: u32,
    /// (talkgroup, label, conversations), most first. The label is the
    /// RadioReference description ("IU Health North ER"), or the alpha tag
    /// when the catalog has none.
    pub by_tg: Vec<(u16, String, u32)>,
}

#[tauri::command]
pub fn conversations_stats(state: State<AppState>) -> Result<StoredStats, String> {
    with_db(&state, stats_rows)
}

fn stats_rows(c: &Connection) -> Result<StoredStats, String> {
    {
        let mut st = StoredStats::default();
        let mut stmt = c
            .prepare("SELECT status, COUNT(*) FROM conversations GROUP BY status")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u32))
            })
            .map_err(|e| e.to_string())?;
        for row in rows {
            let (status, n) = row.map_err(|e| e.to_string())?;
            st.total += n;
            match status.as_str() {
                "sent" => st.sent += n,
                "failed" => st.failed += n,
                _ => st.skipped += n,
            }
        }
        let mut stmt = c
            .prepare(
                "SELECT tg, MAX(CASE WHEN tg_desc <> '' THEN tg_desc ELSE tg_name END), COUNT(*) AS n
                 FROM conversations GROUP BY tg ORDER BY n DESC, tg",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)? as u16,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)? as u32,
                ))
            })
            .map_err(|e| e.to_string())?;
        st.by_tg = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(st)
    }
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

    /// A sent conversation is stored with everything that went into the
    /// message; a revision updates the same row; the tab's list, search,
    /// filter, stats and delete all read it back.
    #[test]
    fn stored_conversations_round_trip() {
        let db = Connection::open_in_memory().unwrap();
        ensure_schema(&db);
        let r = rule();
        let mut c = conv(7, 10202, Some(790065), 1_700_000_030, false);
        c.first_at = 1_700_000_000;
        c.pieces = vec![
            Piece {
                id: Some(41),
                unit: 790065,
                unit_name: Some("Medic 3".into()),
                fixed: false,
                at: 1_700_000_000,
                secs: 4.0,
                audio: Some("/tmp/a.wav".into()),
                transcript: Some("Medic 3 inbound with a 60 year old male".into()),
            },
            Piece {
                id: Some(42),
                unit: 900001,
                unit_name: None,
                fixed: true,
                at: 1_700_000_030,
                secs: 2.0,
                audio: Some("/tmp/b.wav".into()),
                transcript: Some("Copy, room 4".into()),
            },
        ];
        let o = Outcome {
            status: "sent",
            detail: "sent (2 pieces)",
            summary: "Medic 3 is inbound with a 60-year-old male.",
            message: "🏥 Hospitals\nMedic 3 is inbound.",
            prompt: "Summarise…",
            chat: "123",
            revision: 0,
            message_ids: &[7],
        };
        store_row(&db, &r, &c, &o, "live").unwrap();
        let rows = list_rows(&db, None, None, None, None).unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.conv_id, "CONV-10202-1700000000");
        assert_eq!(row.units, vec!["Medic 3".to_string()]);
        assert_eq!(row.pieces.len(), 2);
        assert!(row
            .transcript
            .starts_with("RADIO \"Medic 3\": Medic 3 inbound"));
        assert_eq!(row.status, "sent");

        // A late transmission revises the same conversation: one row, rev 1.
        c.pieces.push(Piece {
            id: Some(43),
            unit: 790065,
            unit_name: Some("Medic 3".into()),
            fixed: false,
            at: 1_700_000_090,
            secs: 3.0,
            audio: None,
            transcript: Some("ETA five minutes".into()),
        });
        c.last_at = 1_700_000_090;
        let o2 = Outcome {
            revision: 1,
            detail: "edited in place; sent (3 pieces)",
            ..o
        };
        store_row(&db, &r, &c, &o2, "live").unwrap();
        let rows = list_rows(&db, None, None, None, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].revision, 1);
        assert_eq!(rows[0].calls, 3);
        let full = get_row(&db, rows[0].id).unwrap();
        assert_eq!(
            full.pieces[2].transcript.as_deref(),
            Some("ETA five minutes")
        );
        assert_eq!(full.message, o.message);
        assert_eq!(full.prompt, o.prompt);

        // Search hits summary, units and the display ID; misses miss.
        assert_eq!(
            list_rows(&db, Some("inbound"), None, None, None)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            list_rows(&db, Some("Medic 3"), None, None, None)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            list_rows(&db, Some("CONV-10202"), None, None, None)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            list_rows(&db, Some("zebra"), None, None, None)
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            list_rows(&db, None, Some(10203), None, None).unwrap().len(),
            0
        );
        assert_eq!(
            list_rows(&db, None, Some(10202), None, None).unwrap().len(),
            1
        );
        // Paging: nothing older than the one row's last activity.
        assert_eq!(
            list_rows(&db, None, None, Some(1_700_000_090), None)
                .unwrap()
                .len(),
            0
        );

        let st = stats_rows(&db).unwrap();
        assert_eq!((st.total, st.sent, st.failed, st.skipped), (1, 1, 0, 0));
        assert_eq!(st.by_tg, vec![(10202, "TG".to_string(), 1)]);

        delete_row(&db, full.id).unwrap();
        assert!(list_rows(&db, None, None, None, None).unwrap().is_empty());
        assert!(get_row(&db, full.id).is_err());
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
        s.learned.entry(k.clone()).or_default().insert(790065, 3);
        assert!(is_fixed(&s, &r, 10202, 790065));
        // A console heard in 50 of 107 is the hospital; a unit in 2 of 107
        // (or 3 of 20) is not.
        s.seen.insert(k.clone(), 107);
        s.learned.entry(k.clone()).or_default().insert(4916085, 50);
        s.learned.entry(k.clone()).or_default().insert(4918296, 2);
        assert!(is_fixed(&s, &r, 10202, 4916085));
        assert!(!is_fixed(&s, &r, 10202, 4918296));
        s.seen.insert(k.clone(), 20);
        s.learned.entry(k.clone()).or_default().insert(111, 3);
        assert!(!is_fixed(&s, &r, 10202, 111));
        let mut off = r.clone();
        off.learn_fixed = false;
        assert!(!is_fixed(&s, &off, 10202, 790065));
    }

    /// A bare open conversation for attribution tests.
    fn conv(key: u64, tg: u16, mobile: Option<u32>, last_at: i64, sent: bool) -> Conversation {
        Conversation {
            key,
            rule_id: "r".into(),
            rule_name: "Hospitals".into(),
            tg,
            tg_name: "TG".into(),
            tg_desc: None,
            mobile_unit: mobile,
            participants: mobile.into_iter().collect(),
            pieces: Vec::new(),
            first_at: last_at,
            last_at,
            sent_ids: Vec::new(),
            sent_chat: String::new(),
            sent_at: sent.then_some(last_at),
            dirty: false,
            revision: 0,
            busy: false,
            attempts: 0,
            retry_after: 0,
            last_summary: None,
            last_error: None,
        }
    }

    #[test]
    fn a_mobile_unit_stays_in_its_own_incident() {
        // Two units with their own live threads on one talkgroup.
        let open = vec![
            conv(1, 10202, Some(790065), 100, false),
            conv(2, 10202, Some(790066), 110, false),
        ];
        // 790065 keys up again → its own conversation, not the newer one,
        // even though the newer one is within the reply gap.
        assert_eq!(
            attach_index(&open, "r", 10202, 790065, false, 120, 90, 180, 45),
            Some(0)
        );
        // A brand-new unit after the reply gap → opens a fresh incident.
        assert_eq!(
            attach_index(&open, "r", 10202, 790099, false, 170, 90, 180, 45),
            None
        );
    }

    #[test]
    fn two_radios_taking_turns_share_one_conversation() {
        // Neither radio is known as fixed yet (the failure mode from the
        // field: one summary per radio, each half an exchange). 31705 keys
        // up 12 s after 4917150 → it is the reply, same incident.
        let open = vec![conv(1, 10202, Some(4917150), 100, false)];
        assert_eq!(
            attach_index(&open, "r", 10202, 31705, false, 112, 90, 180, 45),
            Some(0)
        );
        // Now both are participants; each side's later turns stay in it.
        let mut both = conv(1, 10202, Some(4917150), 112, false);
        both.participants = vec![4917150, 31705];
        let open = vec![both];
        assert_eq!(
            attach_index(&open, "r", 10202, 4917150, false, 130, 90, 180, 45),
            Some(0)
        );
        assert_eq!(
            attach_index(&open, "r", 10202, 31705, false, 140, 90, 180, 45),
            Some(0)
        );
        // A third radio keying up right after is a new incident: the
        // exchange already has both of its sides.
        assert_eq!(
            attach_index(&open, "r", 10202, 790099, false, 145, 90, 180, 45),
            None
        );
    }

    #[test]
    fn a_reply_needs_to_come_quickly() {
        let open = vec![conv(1, 10202, Some(4917150), 100, false)];
        // 60 s later is past the 45 s reply gap → a fresh incident.
        assert_eq!(
            attach_index(&open, "r", 10202, 31705, false, 160, 90, 180, 45),
            None
        );
    }

    #[test]
    fn a_reply_lands_on_the_most_recent_waiting_conversation() {
        // Two one-sided conversations; the reply belongs to the one that
        // just spoke.
        let open = vec![
            conv(1, 10202, Some(790065), 100, false),
            conv(2, 10202, Some(790066), 118, false),
        ];
        assert_eq!(
            attach_index(&open, "r", 10202, 31705, false, 120, 90, 180, 45),
            Some(1)
        );
    }

    #[test]
    fn a_mobile_unit_joins_a_hospital_initiated_conversation() {
        let open = vec![conv(1, 10202, None, 100, false)];
        assert_eq!(
            attach_index(&open, "r", 10202, 790065, false, 120, 90, 180, 45),
            Some(0)
        );
    }

    #[test]
    fn the_fixed_party_prefers_the_live_incident_over_a_sent_one() {
        // A conversation was already summarised at t=100; a new incident is
        // live at t=110. The hospital keys up at t=120 — it must land on the
        // live incident (index 1), not reopen the sent one, so the live
        // incident's summary carries the hospital side and the sent one is not
        // spammed with a duplicate revision.
        let open = vec![
            conv(1, 10202, Some(790065), 100, true),
            conv(2, 10202, Some(790066), 110, false),
        ];
        assert_eq!(
            attach_index(&open, "r", 10202, 900001, true, 120, 90, 180, 45),
            Some(1)
        );
    }

    #[test]
    fn the_fixed_party_reopens_a_sent_incident_only_when_nothing_is_live() {
        // No live conversation: a genuine late hospital follow-up reopens the
        // sent one (within its late window) to revise it.
        let open = vec![conv(1, 10202, Some(790065), 100, true)];
        assert_eq!(
            attach_index(&open, "r", 10202, 900001, true, 150, 90, 180, 45),
            Some(0)
        );
        // Past the late window: nothing to attach to → a fresh conversation.
        assert_eq!(
            attach_index(&open, "r", 10202, 900001, true, 400, 90, 180, 45),
            None
        );
    }

    #[test]
    fn the_fixed_party_picks_the_most_recent_live_incident() {
        let open = vec![
            conv(1, 10202, Some(790065), 100, false),
            conv(2, 10202, Some(790066), 130, false),
            conv(3, 10202, Some(790067), 115, false),
        ];
        assert_eq!(
            attach_index(&open, "r", 10202, 900001, true, 140, 90, 180, 45),
            Some(1)
        );
    }

    #[test]
    fn unnamed_radios_get_slot_letters_never_ids() {
        let mut c = conv(1, 10202, Some(4917150), 100, false);
        let piece = |unit: u32, text: &str| Piece {
            id: None,
            unit,
            unit_name: None,
            fixed: false,
            at: 100,
            secs: 3.0,
            audio: None,
            transcript: Some(text.into()),
        };
        c.pieces = vec![
            piece(4917150, "Medic 42 to Methodist"),
            piece(31705, "Go ahead Medic 42"),
            piece(4917150, "14 year old, ETA 5"),
        ];
        let t = stitched_transcript(&c);
        assert_eq!(
            t,
            "RADIO A: Medic 42 to Methodist\nRADIO B: Go ahead Medic 42\nRADIO A: 14 year old, ETA 5\n"
        );
        assert!(!t.contains("4917150") && !t.contains("31705"));
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
            tg_desc: None,
            mobile_unit: Some(790065),
            participants: vec![790065],
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
        assert_eq!(t, "RADIO \"Medic 3\": Medic 3 inbound, 64 year old male chest pain\nHOSPITAL: Copy, ETA?\nRADIO \"Medic 3\": [no transcript]\n");
        let m = render(&r, &c, "Chest pain, ETA unknown.");
        assert!(
            m.starts_with("🏥 Hospitals · Methodist ER\nChest pain, ETA unknown."),
            "{m}"
        );
        // Wall-clock local time, whatever zone the test machine is in.
        let started = crate::library::local_hm(100);
        assert!(
            m.contains(&format!("Medic 3 · 3 transmissions · 30 s · {started} · revised ×1")),
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
