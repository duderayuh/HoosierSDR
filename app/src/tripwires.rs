//! Tripwires: one kind of rule for everything that used to be an alert, an
//! analyzer, a conversation summary or a digest.
//!
//! A tripwire reads as a sentence — **when** something is heard, **check**
//! it (or not), **send** it somewhere:
//!
//! * **When** — a *call* is heard (on these talkgroups, from these radios,
//!   with these phrases in the transcript and none of the "but not" ones, or
//!   an emergency); a *conversation* on a talkgroup ends; or *every* N
//!   minutes (a digest of what was heard).
//! * **Check** — none; *ask* the model a yes/no question; *extract* named
//!   fields and send only when conditions over them hold; or (conversations
//!   and digests) *summarise*.
//! * **Send** — to a named destination from Settings → Connections, with the
//!   audio, at most once per quiet window per talkgroup, and optionally
//!   **follow-ups**: later traffic from the same radio (or on the same
//!   channel, or the same tripwire firing again) goes out as Telegram
//!   *replies* to the first message, so one incident reads as one thread.
//!
//! "Call" tripwires run here. Conversation and digest tripwires are compiled
//! into the rule lists of the `conversations` and `digest` engines, which
//! keep their own state (learned hospital consoles, last run times) — the
//! tripwire id is the engine rule's id, so history and badges line up.
//!
//! The first start after this module existed builds the tripwires from
//! `alerts.json`, `analyzers.json`, `conversations.json` and `digests.json`
//! (each copied to `*.pre-tripwires.bak` first) and keeps every rule id. The
//! old files are left as they were, so an older build still runs its rules.
//!
//! A tripwire's talkgroups carry one `system` (the RadioReference system they
//! were picked on); a call from another system with the same number does not
//! match. Blank means any system. Conversation and digest engines ignore it.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::alerts::CallFacts;
use crate::analyzers::{clean_line, clean_text, Clause, Field};
use crate::AppState;

/// Replies one thread may collect before it stops (a busy radio would
/// otherwise turn one alert into fifty messages).
pub const MAX_REPLIES: u32 = 10;
pub const MAX_TRIPWIRES: usize = 300;

// ---------------------------------------------------------------------------
// the model
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Tripwire {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    /// The recipe it started from — a hint for the editor, nothing more.
    pub recipe: String,
    pub when: When,
    pub check: Check,
    pub send: Send,
}

impl Default for Tripwire {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            enabled: true,
            recipe: String::new(),
            when: When::default(),
            check: Check::default(),
            send: Send::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct When {
    /// `call` | `conversation` | `digest` | `incident`
    pub kind: String,
    /// Talkgroups; empty = any (a call tripwire then needs phrases, radios
    /// or the emergency flag to narrow it).
    pub tgs: Vec<u16>,
    /// The system those talkgroups are on; blank = any.
    pub system: String,
    /// Any of these in the transcript (whole words, any case).
    pub phrases: Vec<String>,
    /// …but none of these.
    pub except: Vec<String>,
    /// Radio IDs; empty = any.
    pub units: Vec<u32>,
    /// Only calls flagged emergency.
    pub emergency: bool,
    pub conversation: ConvOpts,
    pub digest: DigestOpts,
    pub incident: IncOpts,
}

impl Default for When {
    fn default() -> Self {
        Self {
            kind: "call".into(),
            tgs: Vec::new(),
            system: String::new(),
            phrases: Vec::new(),
            except: Vec::new(),
            units: Vec::new(),
            emergency: false,
            conversation: ConvOpts::default(),
            digest: DigestOpts::default(),
            incident: IncOpts::default(),
        }
    }
}

/// Which runs off the dispatch map are worth a message.
///
/// A dispatch is not a call: it is what the model made of one or more calls
/// — a type, an address, the units sent, and often a point on the map. So it
/// is matched on those, not on words in a transcript.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct IncOpts {
    /// Call types as the dispatch settings spell them; empty = any.
    pub call_types: Vec<String>,
    /// Only runs within `within_km` of this place (a place id), or of the
    /// nearest place with this feature. Blank = anywhere.
    pub near_place: String,
    pub near_feature: String,
    pub within_km: f64,
    /// Only runs that ended up on the map — a run with no pin cannot be
    /// near anything.
    pub placed_only: bool,
    /// Wait for the hospital report before sending, so the message can say
    /// where the patient went.
    pub linked_only: bool,
}

/// How a conversation is told apart and when it is over (see
/// `conversations`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct ConvOpts {
    pub fixed_units: Vec<u32>,
    pub learn_fixed: bool,
    pub end_gap_secs: u32,
    pub reply_gap_secs: u32,
    pub late_window_secs: u32,
    pub max_secs: u32,
    pub min_calls: u32,
    pub send_without_transcript: bool,
}

impl Default for ConvOpts {
    fn default() -> Self {
        let r = crate::conversations::Rule::default();
        Self {
            fixed_units: Vec::new(),
            learn_fixed: r.learn_fixed,
            end_gap_secs: r.end_gap_secs,
            reply_gap_secs: r.reply_gap_secs,
            late_window_secs: r.late_window_secs,
            max_secs: r.max_secs,
            min_calls: r.min_calls,
            send_without_transcript: r.send_without_transcript,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct DigestOpts {
    pub every_mins: u32,
    pub window_mins: u32,
}

impl Default for DigestOpts {
    fn default() -> Self {
        Self {
            every_mins: 15,
            window_mins: 15,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Check {
    /// `none` | `ask` | `extract` | `summarize`
    pub kind: String,
    /// `local` (Ollama) | `cloud`
    pub engine: String,
    /// The question (ask), the instructions (extract), or the summary
    /// prompt (summarize).
    pub prompt: String,
    /// Let a thinking model reason first (local models only).
    pub think: bool,
    pub fields: Vec<Field>,
    /// `all` | `any` — how `conditions` combine; no conditions = always.
    pub match_mode: String,
    pub conditions: Vec<Clause>,
    /// When the model cannot be reached: `send` (and say so) | `hold`.
    pub if_unavailable: String,
}

impl Default for Check {
    fn default() -> Self {
        Self {
            kind: "none".into(),
            engine: "local".into(),
            prompt: String::new(),
            think: false,
            fields: Vec::new(),
            match_mode: "all".into(),
            conditions: Vec::new(),
            if_unavailable: "send".into(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Send {
    /// Send to Telegram (off = only the in-app notice and tone).
    pub telegram: bool,
    /// A destination id from Connections; blank = the default destination;
    /// `custom` = the raw `chat` below.
    pub dest: String,
    /// `chat` or `chat:topic`, for `dest = "custom"`.
    pub chat: String,
    pub message: String,
    pub audio: bool,
    /// Attach a map of the run: where it is, and the way to each hospital
    /// the care pathway picked. Runs only (a call has no position), and off
    /// unless asked for — it costs map tiles and a router call.
    #[serde(default)]
    pub map: bool,
    /// Also attach this many earlier calls on the talkgroup…
    pub earlier_calls: u32,
    /// …heard within this many seconds before.
    pub earlier_window_secs: u32,
    /// Play the alert tone in the app.
    pub tone: bool,
    /// At most one message per talkgroup in this many seconds (0 = every
    /// time). The editor offers presets.
    pub quiet_secs: u32,
    /// `off` | `repeats` (this tripwire again) | `radio` (the radio that
    /// tripped it, anywhere) | `channel` (anything on the talkgroup) — sent
    /// as replies to the first message for `follow_mins`.
    pub follow: String,
    pub follow_mins: u32,
}

impl Default for Send {
    fn default() -> Self {
        Self {
            telegram: true,
            dest: String::new(),
            chat: String::new(),
            message: "🚨 {name}\n{tgname} · {unitname} · {time}\n{transcript}".into(),
            audio: true,
            map: false,
            earlier_calls: 0,
            earlier_window_secs: 120,
            tone: true,
            quiet_secs: 300,
            follow: "off".into(),
            follow_mins: 30,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Settings {
    #[serde(default)]
    pub tripwires: Vec<Tripwire>,
}

/// An open reply thread: later traffic in its scope goes out as replies to
/// `root` until `until`.
#[derive(Clone, Debug)]
struct Thread {
    rule: String,
    tg: u16,
    unit: u32,
    scope: String,
    dest: String,
    root: i64,
    until: i64,
    replies: u32,
    /// Calls already sent in this thread.
    calls: HashSet<i64>,
}

#[derive(Default)]
pub struct TripState {
    pub settings: Settings,
    /// (tripwire, talkgroup) → when a message last went out.
    last_sent: HashMap<(String, u16), i64>,
    threads: Vec<Thread>,
}

pub type Shared = Mutex<TripState>;

// ---------------------------------------------------------------------------
// persistence and migration
// ---------------------------------------------------------------------------

fn config_dir(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let d = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("config dir: {e}"))?;
    std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    Ok(d)
}

fn store(app: &AppHandle, s: &Settings) -> Result<(), String> {
    let p = config_dir(app)?.join("tripwires.json");
    let tmp = p.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_string_pretty(s).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &p).map_err(|e| format!("{}: {e}", p.display()))
}

/// Load the tripwires — or, the first time, build them from the old rule
/// files (already loaded into their engines) — then hand conversation and
/// digest tripwires to their engines. Call after those engines are loaded.
pub fn init(app: &AppHandle) {
    let state = app.state::<AppState>();
    let Ok(dir) = config_dir(app) else { return };
    let p = dir.join("tripwires.json");
    let loaded: Option<Settings> = match std::fs::read_to_string(&p) {
        Ok(t) => match serde_json::from_str(&t) {
            Ok(s) => Some(s),
            Err(e) => {
                // Keep the unreadable file for the listener; start over from
                // the old rules rather than from nothing.
                eprintln!("tripwires.json: {e}");
                let _ = std::fs::rename(
                    &p,
                    dir.join(format!("tripwires.json.broken-{}", crate::library::now())),
                );
                None
            }
        },
        Err(_) => None,
    };
    let settings = match loaded {
        Some(s) => s,
        None => {
            for f in [
                "alerts.json",
                "analyzers.json",
                "conversations.json",
                "digests.json",
            ] {
                let src = dir.join(f);
                let bak = dir.join(format!("{f}.pre-tripwires.bak"));
                if src.exists() && !bak.exists() {
                    let _ = std::fs::copy(&src, &bak);
                }
            }
            let al = state.alerts.lock().unwrap().settings.clone();
            let az = state.analyzers.lock().unwrap().settings.clone();
            let cv = state.conversations.lock().unwrap().settings.clone();
            let dg = state.digests.lock().unwrap().settings.clone();
            let mut s = Settings {
                tripwires: migrate(&al, &az, &cv, &dg),
            };
            for t in &mut s.tripwires {
                let _ = sanitize(t);
            }
            eprintln!(
                "tripwires: built {} from the old alerts, analyzers, conversations and digests",
                s.tripwires.len()
            );
            if let Err(e) = store(app, &s) {
                eprintln!("tripwires: {e}");
            }
            s
        }
    };
    state.tripwires.lock().unwrap().settings = settings;
    compile(app);
}

/// Where a destination target lives among the named destinations.
fn dest_for(al: &crate::alerts::Settings, target: &str) -> (String, String) {
    let target = target.trim();
    if target.is_empty() {
        return (String::new(), String::new());
    }
    if let Some(d) = al.destinations.iter().find(|d| d.target() == target) {
        return (d.id.clone(), String::new());
    }
    ("custom".into(), target.to_string())
}

/// The old rules as tripwires, ids kept. Pure, so it is tested.
pub fn migrate(
    al: &crate::alerts::Settings,
    az: &crate::analyzers::Settings,
    cv: &crate::conversations::Settings,
    dg: &crate::digest::Settings,
) -> Vec<Tripwire> {
    let mut out = Vec::new();
    let fail_open = if al.ollama.fail_open { "send" } else { "hold" };
    for a in &al.alerts {
        let t = &a.trigger;
        let (dest, chat) = dest_for(
            al,
            &crate::alerts::join_destination(&a.chat_id, &a.topic_id),
        );
        out.push(Tripwire {
            id: a.id.clone(),
            name: a.name.clone(),
            enabled: a.enabled,
            recipe: format!("alert-{}", t.kind),
            when: When {
                kind: "call".into(),
                tgs: t.tgs.clone(),
                phrases: if t.kind == "keywords" {
                    t.keywords.clone()
                } else {
                    Vec::new()
                },
                units: t.units.clone(),
                emergency: t.kind == "emergency",
                ..Default::default()
            },
            check: if a.ai_gate && !a.ai_prompt.trim().is_empty() {
                Check {
                    kind: "ask".into(),
                    prompt: a.ai_prompt.clone(),
                    think: a.ai_think,
                    if_unavailable: fail_open.into(),
                    ..Default::default()
                }
            } else {
                Check::default()
            },
            send: Send {
                telegram: a.telegram,
                dest,
                chat,
                message: a.message.clone(),
                audio: a.attach_audio,
                map: false,
                earlier_calls: a.combine_prev,
                earlier_window_secs: a.combine_window_secs,
                tone: a.tone,
                quiet_secs: a.cooldown_secs,
                follow: "off".into(),
                follow_mins: 30,
            },
        });
    }
    for r in &az.rules {
        out.push(from_analyzer(r, al));
    }
    for r in &cv.rules {
        let (dest, chat) = dest_for(al, &r.chat_id);
        out.push(Tripwire {
            id: r.id.clone(),
            name: r.name.clone(),
            enabled: r.enabled,
            recipe: "conversation".into(),
            when: When {
                kind: "conversation".into(),
                tgs: r.tgs.clone(),
                conversation: ConvOpts {
                    fixed_units: r.fixed_units.clone(),
                    learn_fixed: r.learn_fixed,
                    end_gap_secs: r.end_gap_secs,
                    reply_gap_secs: r.reply_gap_secs,
                    late_window_secs: r.late_window_secs,
                    max_secs: r.max_secs,
                    min_calls: r.min_calls,
                    send_without_transcript: r.send_without_transcript,
                },
                ..Default::default()
            },
            check: Check {
                kind: "summarize".into(),
                prompt: r.summary_prompt.clone(),
                ..Default::default()
            },
            send: Send {
                dest,
                chat,
                message: r.message.clone(),
                audio: r.attach_audio,
                tone: false,
                quiet_secs: 0,
                ..Default::default()
            },
        });
    }
    for r in &dg.rules {
        let (dest, chat) = dest_for(al, &r.chat_id);
        out.push(Tripwire {
            id: r.id.clone(),
            name: r.name.clone(),
            enabled: r.enabled,
            recipe: "digest".into(),
            when: When {
                kind: "digest".into(),
                tgs: r.tgs.clone(),
                digest: DigestOpts {
                    every_mins: (r.interval_secs / 60).max(1),
                    window_mins: (r.window_secs / 60).max(1),
                },
                ..Default::default()
            },
            check: Check {
                kind: "summarize".into(),
                prompt: r.prompt.clone(),
                ..Default::default()
            },
            send: Send {
                dest,
                chat,
                message: r.message.clone(),
                audio: false,
                tone: false,
                quiet_secs: 0,
                ..Default::default()
            },
        });
    }
    // Ids came from four files; make sure no two collide.
    let mut seen = HashSet::new();
    for (i, t) in out.iter_mut().enumerate() {
        if t.id.trim().is_empty() || !seen.insert(t.id.clone()) {
            t.id = format!("t{}-m{i}", crate::library::now());
            seen.insert(t.id.clone());
        }
    }
    out
}

/// An analyzer as a tripwire: a call with an extract check.
fn from_analyzer(r: &crate::analyzers::AnalyzerRule, al: &crate::alerts::Settings) -> Tripwire {
    let (dest, chat) = dest_for(al, &r.chat_id);
    Tripwire {
        id: r.id.clone(),
        name: r.name.clone(),
        enabled: r.enabled,
        recipe: "analyzer".into(),
        when: When {
            kind: "call".into(),
            tgs: r.tgs.clone(),
            phrases: r.keywords.clone(),
            ..Default::default()
        },
        check: Check {
            kind: "extract".into(),
            engine: if r.engine == "cloud" {
                "cloud"
            } else {
                "local"
            }
            .into(),
            prompt: r.instructions.clone(),
            think: r.think,
            fields: r.fields.clone(),
            match_mode: r.match_mode.clone(),
            conditions: r.conditions.clone(),
            if_unavailable: "hold".into(),
        },
        send: Send {
            telegram: r.telegram,
            dest,
            chat,
            message: r.message.clone(),
            audio: r.attach_audio,
            map: false,
            earlier_calls: 0,
            earlier_window_secs: 120,
            tone: false,
            quiet_secs: r.cooldown_secs,
            follow: "off".into(),
            follow_mins: 30,
        },
    }
}

// ---------------------------------------------------------------------------
// sharing: a file of tripwires (or an old analyzer template)
// ---------------------------------------------------------------------------

pub const FORMAT: &str = "hoosiersdr-tripwires";
pub const VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Bundle {
    pub format: String,
    pub version: u32,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub description: String,
    pub tripwires: Vec<Tripwire>,
}

/// What an imported tripwire may never bring along: an id that could
/// replace one of yours, the enabled flag, and where it sends (your bot
/// would deliver transcripts wherever the file said).
fn quarantine(t: &mut Tripwire, i: usize) {
    t.id = format!("t{}-i{i}", crate::library::now());
    t.enabled = false;
    t.send.dest.clear();
    t.send.chat.clear();
}

/// Read a shared file: a tripwire bundle, or an analyzer template from
/// before tripwires. Everything comes back tidied, disabled and sending
/// nowhere in particular; nothing is saved until the page adds them.
pub fn parse_bundle(text: &str) -> Result<Bundle, String> {
    if text.len() > crate::analyzers::MAX_TEMPLATE_BYTES {
        return Err("that file is too large to be a tripwire file".into());
    }
    let text = text.trim_start_matches('\u{FEFF}');
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("not a tripwire file: {e}"))?;
    let format = v["format"].as_str().unwrap_or("");
    let mut b = if format == crate::analyzers::TEMPLATE_FORMAT {
        let t = crate::analyzers::parse_template(text)?;
        let al = crate::alerts::Settings::default();
        Bundle {
            format: FORMAT.into(),
            version: VERSION,
            name: t.name,
            author: t.author,
            description: t.description,
            tripwires: t.rules.iter().map(|r| from_analyzer(r, &al)).collect(),
        }
    } else if format == FORMAT {
        let b: Bundle =
            serde_json::from_value(v).map_err(|e| format!("not a tripwire file: {e}"))?;
        if b.version > VERSION {
            return Err(format!(
                "this file is version {}, newer than this app understands ({VERSION})",
                b.version
            ));
        }
        b
    } else {
        return Err("not a tripwire file".into());
    };
    if b.tripwires.is_empty() {
        return Err("the file has no tripwires".into());
    }
    if b.tripwires.len() > 50 {
        return Err(format!(
            "the file has {} tripwires (limit 50)",
            b.tripwires.len()
        ));
    }
    b.name = clean_line(&b.name, 80);
    b.author = clean_line(&b.author, 80);
    b.description = clean_text(&b.description, 1000);
    for (i, t) in b.tripwires.iter_mut().enumerate() {
        sanitize(t)?;
        if t.name.is_empty() {
            t.name = format!("Imported tripwire {}", i + 1);
        }
        quarantine(t, i);
    }
    Ok(b)
}

/// A bundle of some of your tripwires, without what is personal to this
/// install (ids, on/off, destinations).
pub fn make_bundle(list: Vec<Tripwire>, name: &str, author: &str, description: &str) -> Bundle {
    Bundle {
        format: FORMAT.into(),
        version: VERSION,
        name: clean_line(name, 80),
        author: clean_line(author, 80),
        description: clean_text(description, 1000),
        tripwires: list
            .into_iter()
            .enumerate()
            .map(|(i, mut t)| {
                t.id = format!("t{}", i + 1);
                t.enabled = false;
                t.send.dest.clear();
                t.send.chat.clear();
                t
            })
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// tidying what the page (or the phone, or a file) hands us
// ---------------------------------------------------------------------------

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn clean_list(v: &[String], max: usize) -> Vec<String> {
    let mut seen = HashSet::new();
    v.iter()
        .map(|k| clean_line(k, 64))
        .filter(|k| !k.is_empty() && seen.insert(k.to_lowercase()))
        .take(max)
        .collect()
}

/// Normalise one tripwire in place. Errors only for what cannot be repaired
/// without changing its meaning (a malformed field key, an unknown operator).
pub fn sanitize(t: &mut Tripwire) -> Result<(), String> {
    t.id = clean_line(&t.id, 64);
    if !t.id.is_empty() && !valid_id(&t.id) {
        return Err(format!("tripwire id '{}' must be [A-Za-z0-9_-]", t.id));
    }
    t.name = clean_line(&t.name, 80);
    t.recipe = clean_line(&t.recipe, 40);
    let w = &mut t.when;
    w.kind = match w.kind.as_str() {
        "conversation" | "digest" | "incident" => w.kind.clone(),
        _ => "call".into(),
    };
    w.tgs.sort_unstable();
    w.tgs.dedup();
    w.tgs.truncate(2000);
    w.system = clean_line(&w.system, 80);
    w.phrases = clean_list(&w.phrases, 64);
    w.except = clean_list(&w.except, 64);
    w.units.sort_unstable();
    w.units.dedup();
    w.units.truncate(500);
    {
        let o = &mut w.incident;
        o.call_types = clean_list(&o.call_types, 60);
        o.call_types.truncate(40);
        o.near_place = clean_line(&o.near_place, 64);
        o.near_feature = clean_line(&o.near_feature, 24).to_ascii_lowercase();
        o.within_km = o.within_km.clamp(0.0, 500.0);
    }
    let c = &mut w.conversation;
    c.fixed_units.sort_unstable();
    c.fixed_units.dedup();
    c.end_gap_secs = c.end_gap_secs.clamp(10, 3600);
    c.reply_gap_secs = c.reply_gap_secs.min(600);
    c.late_window_secs = c.late_window_secs.min(3600);
    c.max_secs = c.max_secs.clamp(60, 7200);
    c.min_calls = c.min_calls.clamp(1, 50);
    let d = &mut w.digest;
    d.every_mins = d.every_mins.clamp(1, 1440);
    d.window_mins = d.window_mins.clamp(1, 1440);

    let k = &mut t.check;
    k.kind = match (k.kind.as_str(), t.when.kind.as_str()) {
        (_, "conversation") | (_, "digest") => "summarize".into(),
        ("ask", _) | ("extract", _) => k.kind.clone(),
        _ => "none".into(),
    };
    k.engine = if k.engine == "cloud" {
        "cloud"
    } else {
        "local"
    }
    .into();
    k.prompt = clean_text(&k.prompt, 12_000);
    k.match_mode = if k.match_mode.eq_ignore_ascii_case("any") {
        "any"
    } else {
        "all"
    }
    .into();
    k.if_unavailable = if k.if_unavailable == "hold" {
        "hold"
    } else {
        "send"
    }
    .into();
    if k.fields.len() > 32 || k.conditions.len() > 32 {
        return Err(format!("'{}' has too many fields or conditions", t.name));
    }
    let mut keys = HashSet::new();
    for f in &mut k.fields {
        f.key = clean_line(&f.key, 40);
        if !crate::analyzers::valid_key(&f.key) {
            return Err(format!(
                "'{}': field key '{}' must be letters, digits, _ or -",
                t.name, f.key
            ));
        }
        if !keys.insert(f.key.clone()) {
            return Err(format!("'{}': field '{}' is declared twice", t.name, f.key));
        }
        f.kind = match f.kind.as_str() {
            "number" | "bool" => f.kind.clone(),
            _ => "string".into(),
        };
        f.desc = clean_line(&f.desc, 400);
    }
    for c in &mut k.conditions {
        c.field = clean_line(&c.field, 40);
        if !crate::analyzers::valid_key(&c.field) {
            return Err(format!(
                "'{}': condition field '{}' must be letters, digits or _",
                t.name, c.field
            ));
        }
        c.op = c.op.trim().to_string();
        if !matches!(
            c.op.as_str(),
            "==" | "!=" | ">" | ">=" | "<" | "<=" | "contains"
        ) {
            return Err(format!("'{}': unknown operator '{}'", t.name, c.op));
        }
        c.value = clean_line(&c.value, 200);
    }

    let s = &mut t.send;
    s.dest = clean_line(&s.dest, 64);
    if !s.dest.is_empty() && s.dest != "custom" && !valid_id(&s.dest) {
        s.dest.clear();
    }
    s.chat = clean_line(&s.chat, 80);
    if !s
        .chat
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '@' | ':'))
    {
        return Err(format!("'{}': chat id has unexpected characters", t.name));
    }
    s.message = clean_text(&s.message, 3000);
    s.earlier_calls = s.earlier_calls.min(10);
    s.earlier_window_secs = s.earlier_window_secs.min(3600);
    s.quiet_secs = s.quiet_secs.min(86_400);
    s.follow = match s.follow.as_str() {
        "repeats" | "radio" | "channel" => s.follow.clone(),
        _ => "off".into(),
    };
    s.follow_mins = s.follow_mins.clamp(1, 240);
    Ok(())
}

// ---------------------------------------------------------------------------
// matching
// ---------------------------------------------------------------------------

/// Does the call need its transcript before this tripwire can decide?
pub fn needs_transcript(t: &Tripwire) -> bool {
    !t.when.phrases.is_empty() || !t.when.except.is_empty() || t.check.kind != "none"
}

/// A call tripwire with nothing narrowing it would fire on every call heard.
pub fn is_narrowed(t: &Tripwire) -> bool {
    let w = &t.when;
    if w.kind == "incident" {
        // A run is already a narrow thing — the model decided it was a
        // dispatch — so even "every run" is a reasonable tripwire.
        return true;
    }
    !w.tgs.is_empty() || !w.units.is_empty() || !w.phrases.is_empty() || w.emergency
}

// ---------------------------------------------------------------------------
// runs off the dispatch map
// ---------------------------------------------------------------------------

/// What an incident tripwire can say in its message, beyond the standard
/// tokens: `{calltype}`, `{address}`, `{units}`, `{place}`, `{km}`,
/// `{nearest}`, `{summary}`, `{hospital}`, `{report}`, `{where}`,
/// `{pathway}`.
///
/// `{where}` is the care pathway's answer — every hospital the run needs,
/// with the drive to each — and it is the one worth putting in a message
/// about a cardiac arrest. `{nearest}`/`{km}` are the older, single-feature
/// question and stay for the tripwires already written against them.
pub fn incident_fields(
    i: &crate::dispatch::Incident,
    places: &crate::places::Settings,
    feature: &str,
    reports: &[crate::link::LinkedReport],
) -> serde_json::Value {
    let mut v = serde_json::json!({
        "calltype": i.call_type,
        "address": i.address,
        "units": i.units.join(", "),
        "summary": i.summary,
        "place": "",
        "km": "",
        "mins": "",
        "nearest": "",
        "hospital": "",
        "report": "",
        "where": crate::pathways::say_all(&i.targets),
        "pathway": i.pathway,
    });
    if let (Some(lat), Some(lon)) = (i.lat, i.lon) {
        // The nearest place that can do the thing this tripwire cares
        // about — a cath lab, a stroke team — and how far the run is from
        // it.
        if let Some((p, m)) = crate::places::nearest_with(places, feature, (lat, lon)).first() {
            v["nearest"] = p.name.clone().into();
            v["km"] = format!("{:.1}", m / 1000.0).into();
            // A drive time only if the pathway already routed to this very
            // place by road. A straight-line guess would read as a real ETA
            // here, with nothing in the token to mark it as a guess.
            if let Some(t) = i
                .targets
                .iter()
                .find(|t| t.place_name == p.name && t.how == "road")
            {
                v["mins"] = t.mins().to_string().into();
            }
        }
        if let Some((p, _)) = crate::places::nearest_with(places, "", (lat, lon)).first() {
            v["place"] = p.name.clone().into();
        }
    }
    if let Some(r) = reports.first() {
        v["hospital"] = r.place.clone().into();
        v["report"] = r.summary.clone().into();
    }
    v
}

/// How many hospitals get a drawn route. Each one is a router call on the
/// send thread, and a picture with four lines crossing it says less than
/// one with two.
const DRAWN: usize = 2;

/// The caption under the picture: what the run is, and where it would go.
/// Kept short — Telegram allows 1024 characters here, and the alert itself
/// has already said everything else.
pub fn photo_caption(i: &crate::dispatch::Incident, shot: &crate::mapshot::Shot) -> String {
    let mut out = format!("{} {} · {}", i.emoji, i.call_type, i.address);
    let where_to = crate::pathways::say_all(&i.targets);
    if !where_to.is_empty() {
        out.push('\n');
        // Capped so the footer below always survives: the credit at the end
        // of it is a condition of using the tiles, not decoration, and the
        // sender truncates from the front.
        if where_to.chars().count() > 700 {
            out.extend(where_to.chars().take(700));
            out.push('…');
        } else {
            out.push_str(&where_to);
        }
    }
    let mut foot = Vec::new();
    // A pin fitted from the hundred-block grid is good to a few hundred
    // metres. Drawn, it looks exactly as certain as a rooftop match, so the
    // caption has to say which one this is.
    if !matches!(i.geocode.as_str(), "ok" | "manual" | "corrected") {
        foot.push("position approximate".into());
    }
    if shot.bar_m > 0.0 {
        foot.push(format!("bar {}", crate::mapshot::say_bar(shot.bar_m)));
    }
    if shot.missing > 0 {
        // Say so rather than let the reader wonder what the grey squares
        // were hiding.
        foot.push(format!(
            "{} map tile{} missing",
            shot.missing,
            if shot.missing == 1 { "" } else { "s" }
        ));
    }
    foot.push("© OpenStreetMap contributors".into());
    out.push('\n');
    out.push_str(&foot.join(" · "));
    out
}

/// Draw the run and send it as a reply to the alert. Returns the photo's
/// message id.
///
/// A failure here is reported but never fails the alert: the words are the
/// alert, and a missing tile server is no reason to lose a cardiac arrest.
fn send_map(
    app: &AppHandle,
    state: &State<'_, AppState>,
    dest: &str,
    incident: i64,
    reply_to: Option<i64>,
) -> Result<i64, String> {
    let Some(db) = state.db.lock().unwrap().clone() else {
        return Err("library not open".into());
    };
    let i = {
        let c = db.lock().unwrap();
        crate::dispatch::inc_get(&c, incident)?.ok_or("no such run")?
    };
    let Some((lat, lon)) = i.lat.zip(i.lon) else {
        return Err("the run has no position".into());
    };
    // The route shapes, which the stored targets don't carry — they hold
    // the distance, not the line.
    let legs: Vec<crate::mapshot::Leg> = i
        .targets
        .iter()
        .take(DRAWN)
        .map(|t| {
            let r = crate::routing::shape(state, (lat, lon), (t.lat, t.lon));
            crate::mapshot::Leg {
                to: (t.lat, t.lon),
                line: r.line,
                road: r.how == "road",
            }
        })
        .collect();
    let shot = crate::mapshot::draw(app, (lat, lon), &legs)?;
    crate::alerts::send_photo_reply(dest, &shot.png, &photo_caption(&i, &shot), reply_to)
}

/// Does this run trip this tripwire?
pub fn matches_incident(
    t: &Tripwire,
    i: &crate::dispatch::Incident,
    places: &crate::places::Settings,
    linked: bool,
) -> bool {
    let w = &t.when;
    let o = &w.incident;
    if !w.tgs.is_empty() && !w.tgs.contains(&i.tg) {
        return false;
    }
    if !o.call_types.is_empty()
        && !o
            .call_types
            .iter()
            .any(|c| c.eq_ignore_ascii_case(&i.call_type))
    {
        return false;
    }
    if o.linked_only && !linked {
        return false;
    }
    let placed = i.lat.is_some() && i.lon.is_some();
    if (o.placed_only || o.within_km > 0.0) && !placed {
        return false;
    }
    // "Within so many kilometres of somewhere": either a place by name, or
    // the nearest place that can do a particular thing.
    if o.within_km > 0.0 {
        let (lat, lon) = (i.lat.unwrap_or(0.0), i.lon.unwrap_or(0.0));
        let near = if !o.near_place.is_empty() {
            places
                .places
                .iter()
                .find(|p| p.id == o.near_place && p.enabled)
                .and_then(|p| Some(crate::dispatch::haversine_m(lat, lon, p.lat?, p.lon?)))
        } else {
            crate::places::nearest_with(places, &o.near_feature, (lat, lon))
                .first()
                .map(|(_, m)| *m)
        };
        match near {
            Some(m) if m <= o.within_km * 1000.0 => {}
            _ => return false,
        }
    }
    true
}

/// A run, dressed as a call so the rest of the machinery — the quiet
/// window, the AI check, the message, the history — works unchanged.
pub fn incident_facts(i: &crate::dispatch::Incident) -> CallFacts {
    CallFacts {
        id: None,
        start: i.created,
        tg: i.tg,
        tg_name: i.tg_name.clone(),
        tg_desc: None,
        unit: 0,
        unit_name: i.units.first().cloned(),
        secs: 0.0,
        emergency: false,
        audio: None,
        // What the model made of the run reads as the transcript, so an AI
        // check has something to read.
        transcript: Some(format!(
            "{} at {}. Units: {}. {}",
            i.call_type,
            if i.address.is_empty() { "an address not heard" } else { &i.address },
            i.units.join(", "),
            i.summary
        )),
        system: String::new(),
    }
}

/// Does the call trip this tripwire? Returns the phrases that matched.
/// Checks nothing about the model — that comes after.
pub fn matches(t: &Tripwire, f: &CallFacts) -> Option<Vec<String>> {
    let w = &t.when;
    if !t.enabled || w.kind != "call" || !is_narrowed(t) {
        return None;
    }
    if !w.system.is_empty() && !f.system.is_empty() && w.system != f.system {
        return None;
    }
    if !w.tgs.is_empty() && !w.tgs.contains(&f.tg) {
        return None;
    }
    if !w.units.is_empty() && !w.units.contains(&f.unit) {
        return None;
    }
    if w.emergency && !f.emergency {
        return None;
    }
    let text = f.transcript.as_deref();
    if needs_transcript(t) && text.is_none() {
        return None;
    }
    let text = text.unwrap_or("");
    if !w.except.is_empty() && !crate::alerts::matched_keywords(&w.except, text).is_empty() {
        return None;
    }
    if w.phrases.is_empty() {
        return Some(Vec::new());
    }
    let m = crate::alerts::matched_keywords(&w.phrases, text);
    (!m.is_empty()).then_some(m)
}

const STANDARD_TOKENS: &[&str] = &[
    "name",
    "alert",
    "tg",
    "tgname",
    "tgdesc",
    "unit",
    "unitname",
    "time",
    "secs",
    "transcript",
    "keywords",
    "ai",
    "json",
];

/// Fill a message template. Tokens: `{name}` (also `{alert}`), `{tg}`,
/// `{tgname}`, `{tgdesc}`, `{unit}`, `{unitname}`, `{time}`, `{secs}`,
/// `{transcript}`, `{keywords}`, `{ai}`, `{json}`, `{field.KEY}`.
pub fn render(
    template: &str,
    name: &str,
    f: &CallFacts,
    keywords: &[String],
    ai: &str,
    fields: Option<&serde_json::Value>,
) -> String {
    let time = crate::library::local_hms(if f.start > 0 {
        f.start
    } else {
        crate::library::now()
    });
    let unit = f.unit_name.clone().unwrap_or_else(|| {
        if f.unit == 0 {
            String::new()
        } else {
            f.unit.to_string()
        }
    });
    let mut out = template
        .replace("{name}", name)
        .replace("{alert}", name)
        .replace("{tg}", &f.tg.to_string())
        .replace("{tgname}", &f.tg_name)
        .replace("{tgdesc}", f.tg_desc.as_deref().unwrap_or(""))
        .replace("{unit}", &f.unit.to_string())
        .replace("{unitname}", &unit)
        .replace("{time}", &time)
        .replace("{secs}", &format!("{:.0}", f.secs))
        .replace("{transcript}", f.transcript.as_deref().unwrap_or(""))
        .replace("{keywords}", &keywords.join(", "))
        .replace("{ai}", ai);
    if let Some(obj) = fields {
        out = out.replace(
            "{json}",
            &serde_json::to_string_pretty(obj).unwrap_or_default(),
        );
        if let Some(map) = obj.as_object() {
            for (k, v) in map {
                let s = match v {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Null => String::new(),
                    other => other.to_string(),
                };
                out = out.replace(&format!("{{field.{k}}}"), &s);
                // `{candidate}` works too, unless it names a standard token.
                if !STANDARD_TOKENS.contains(&k.as_str()) {
                    out = out.replace(&format!("{{{k}}}"), &s);
                }
            }
        }
    } else {
        out = out.replace("{json}", "");
    }
    // A line left with nothing but separators (its tokens were empty) is
    // dropped; blank lines the template put there stay.
    out.lines()
        .filter(|l| {
            let t = l.trim();
            t.is_empty()
                || !t.chars().all(|c| {
                    c.is_whitespace()
                        || matches!(c, '·' | '-' | '—' | '–' | '|' | ',' | ':' | '/' | '(' | ')')
                })
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// The text of a follow-up reply.
fn follow_text(f: &CallFacts) -> String {
    let who = f.unit_name.clone().unwrap_or_else(|| {
        if f.unit == 0 {
            String::new()
        } else {
            f.unit.to_string()
        }
    });
    let time = crate::library::local_hm(if f.start > 0 {
        f.start
    } else {
        crate::library::now()
    });
    let head = [who.as_str(), f.tg_name.as_str(), time.as_str()]
        .iter()
        .filter(|s| !s.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join(" · ");
    format!("↳ {head}\n{}", f.transcript.as_deref().unwrap_or("").trim())
        .trim()
        .to_string()
}

// ---------------------------------------------------------------------------
// destinations
// ---------------------------------------------------------------------------

/// The `chat` / `chat:topic` a tripwire sends to.
pub fn resolve_dest(al: &crate::alerts::Settings, s: &Send) -> Result<String, String> {
    match s.dest.as_str() {
        "" => {
            let d = al.telegram.destination();
            if d.trim().is_empty() {
                Err("no default destination — pick one in Settings → Connections".into())
            } else {
                Ok(d)
            }
        }
        "custom" => {
            if s.chat.trim().is_empty() {
                Err("no chat id set".into())
            } else {
                Ok(s.chat.trim().to_string())
            }
        }
        id => al
            .destinations
            .iter()
            .find(|d| d.id == id)
            .map(|d| d.target())
            .ok_or_else(|| "the destination it sends to was removed".to_string()),
    }
}

/// Point tripwires at destinations again after Connections changed: one
/// that sent to a removed destination keeps sending to its chat, by raw id;
/// one that sent to a raw chat that now has a name sends by the name.
/// Returns whether anything changed.
pub fn relink(
    list: &mut [Tripwire],
    gone: &[&crate::connections::Destination],
    now: &[crate::connections::Destination],
) -> bool {
    let mut changed = false;
    for t in list.iter_mut() {
        if let Some(d) = gone.iter().find(|d| d.id == t.send.dest) {
            t.send.dest = "custom".into();
            t.send.chat = d.target();
            changed = true;
        }
        if t.send.dest == "custom" {
            if let Some(d) = now.iter().find(|d| d.target() == t.send.chat.trim()) {
                t.send.dest = d.id.clone();
                t.send.chat.clear();
                changed = true;
            }
        }
    }
    changed
}

/// Destinations changed on Connections: relink the tripwires, recompile.
pub fn destinations_changed(
    app: &AppHandle,
    before: &[crate::connections::Destination],
    after: &[crate::connections::Destination],
) {
    let gone: Vec<&crate::connections::Destination> = before
        .iter()
        .filter(|d| !after.iter().any(|a| a.id == d.id))
        .collect();
    let state = app.state::<AppState>();
    {
        let mut st = state.tripwires.lock().unwrap();
        if relink(&mut st.settings.tripwires, &gone, after) {
            let _ = store(app, &st.settings);
        }
    }
    compile(app);
}

// ---------------------------------------------------------------------------
// conversations and digests run in their own engines
// ---------------------------------------------------------------------------

/// Hand the conversation and digest tripwires to their engines. Their state
/// (learned consoles, last runs) is keyed by the same ids and kept.
pub fn compile(app: &AppHandle) {
    let state = app.state::<AppState>();
    let list = state.tripwires.lock().unwrap().settings.tripwires.clone();
    let al = state.alerts.lock().unwrap().settings.clone();
    let (convs, digests) = compile_rules(&list, &al);
    crate::conversations::set_rules(app, convs);
    crate::digest::set_rules(app, digests);
}

/// Pure half of `compile`, so it is tested.
pub fn compile_rules(
    list: &[Tripwire],
    al: &crate::alerts::Settings,
) -> (
    Vec<crate::conversations::Rule>,
    Vec<crate::digest::DigestRule>,
) {
    let mut convs = Vec::new();
    let mut digests = Vec::new();
    for t in list {
        // A blank result falls back to the engine's own default chat, which
        // is the same default — and an unresolvable one sends nowhere.
        let chat = if t.send.telegram && !t.send.dest.is_empty() {
            resolve_dest(al, &t.send).unwrap_or_default()
        } else {
            String::new()
        };
        match t.when.kind.as_str() {
            "conversation" => {
                let o = &t.when.conversation;
                convs.push(crate::conversations::Rule {
                    id: t.id.clone(),
                    name: t.name.clone(),
                    enabled: t.enabled && t.send.telegram,
                    tgs: t.when.tgs.clone(),
                    fixed_units: o.fixed_units.clone(),
                    learn_fixed: o.learn_fixed,
                    end_gap_secs: o.end_gap_secs,
                    reply_gap_secs: o.reply_gap_secs,
                    late_window_secs: o.late_window_secs,
                    max_secs: o.max_secs,
                    min_calls: o.min_calls,
                    summary_prompt: t.check.prompt.clone(),
                    message: t.send.message.clone(),
                    chat_id: chat,
                    attach_audio: t.send.audio,
                    send_without_transcript: o.send_without_transcript,
                });
            }
            "digest" => {
                let o = &t.when.digest;
                digests.push(crate::digest::DigestRule {
                    id: t.id.clone(),
                    name: t.name.clone(),
                    enabled: t.enabled && t.send.telegram,
                    tgs: t.when.tgs.clone(),
                    interval_secs: o.every_mins * 60,
                    window_secs: o.window_mins * 60,
                    prompt: t.check.prompt.clone(),
                    message: t.send.message.clone(),
                    chat_id: chat,
                });
            }
            _ => {}
        }
    }
    (convs, digests)
}

// ---------------------------------------------------------------------------
// the call engine
// ---------------------------------------------------------------------------

fn call_tripwires(app: &AppHandle) -> Vec<Tripwire> {
    app.state::<AppState>()
        .tripwires
        .lock()
        .unwrap()
        .settings
        .tripwires
        .iter()
        .filter(|t| t.enabled && t.when.kind == "call")
        .cloned()
        .collect()
}

/// A run appeared on the dispatch map, or grew. Incident tripwires are
/// matched on what the run *is* — its type, where it is, whether the crew
/// has reported to a hospital yet — rather than on words in one call.
///
/// `linked` says a hospital report has been joined to it, which is what a
/// tripwire waiting for the outcome is waiting for.
pub fn on_incident(app: &AppHandle, i: &crate::dispatch::Incident, linked: bool) {
    let state = app.state::<AppState>();
    let list: Vec<Tripwire> = {
        let st = state.tripwires.lock().unwrap();
        st.settings
            .tripwires
            .iter()
            .filter(|t| t.enabled && t.when.kind == "incident")
            .cloned()
            .collect()
    };
    if list.is_empty() {
        return;
    }
    let (places, reports) = {
        let places = state.places.lock().unwrap().settings.clone();
        let db = state.db.lock().unwrap().clone();
        let reports = db
            .map(|db| {
                let c = db.lock().unwrap();
                crate::link::reports_for(&c, i.id, &places)
            })
            .unwrap_or_default();
        (places, reports)
    };
    // Which rules have already had their say about this run. A run grows
    // as more calls land on it, and each growth would otherwise be news.
    let told: std::collections::HashSet<String> = {
        let db = state.db.lock().unwrap().clone();
        db.map(|db| {
            let c = db.lock().unwrap();
            crate::events::rules_for_incident(&c, i.id)
        })
        .unwrap_or_default()
    };
    for t in list {
        if told.contains(&t.id) {
            continue;
        }
        if !matches_incident(&t, i, &places, linked || !reports.is_empty()) {
            continue;
        }
        let f = incident_facts(i);
        let mut extra = incident_fields(i, &places, &t.when.incident.near_feature, &reports);
        // By road if a router is running; the straight line otherwise, and
        // the message never pretends otherwise.
        if let (Some(lat), Some(lon)) = (i.lat, i.lon) {
            if let Some((p, _)) =
                crate::places::nearest_with(&places, &t.when.incident.near_feature, (lat, lon))
                    .first()
            {
                if let (Some(plat), Some(plon)) = (p.lat, p.lon) {
                    let d = crate::routing::distance(&state, (lat, lon), (plat, plon));
                    extra["km"] = format!("{:.1}", d.km()).into();
                    if d.secs > 0.0 {
                        extra["mins"] = format!("{:.0}", d.mins()).into();
                    }
                }
            }
        }
        let (app, t) = (app.clone(), t);
        let id = i.id;
        std::thread::spawn(move || fire_with(&app, t, f, Vec::new(), Some(extra), Some(id)));
    }
}

/// A call finished (its transcript, if any, is still to come).
pub fn on_call(app: &AppHandle, f: &CallFacts) {
    for t in call_tripwires(app) {
        if needs_transcript(&t) {
            continue;
        }
        if let Some(kw) = matches(&t, f) {
            let (app, f) = (app.clone(), f.clone());
            std::thread::spawn(move || fire(&app, t, f, kw));
        }
    }
}

/// A transcript landed for library call `id`.
pub fn on_transcript(app: &AppHandle, id: i64, text: &str) {
    let state = app.state::<AppState>();
    let list = call_tripwires(app);
    let live = {
        let mut st = state.tripwires.lock().unwrap();
        let now = crate::library::now();
        st.threads
            .retain(|t| t.until > now && t.replies < MAX_REPLIES);
        !st.threads.is_empty()
    };
    if list.iter().all(|t| !needs_transcript(t)) && !live {
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
    let f = crate::alerts::facts_from_row(app, r, Some(text.to_string()));
    let mut fired: HashSet<String> = HashSet::new();
    for t in list {
        if !needs_transcript(&t) {
            continue;
        }
        if let Some(kw) = matches(&t, &f) {
            fired.insert(t.id.clone());
            let (app, f) = (app.clone(), f.clone());
            std::thread::spawn(move || fire(&app, t, f, kw));
        }
    }
    if live {
        follow_ups(app, &f, &fired);
    }
}

/// What the check decided.
enum Verdict {
    Pass {
        note: String,
        fields: Option<serde_json::Value>,
    },
    Quiet {
        why: String,
        fields: Option<serde_json::Value>,
    },
    Unavailable(String),
}

/// The throwaway analyzer rule the extraction helpers take.
fn as_analyzer(t: &Tripwire, fields: Vec<Field>) -> crate::analyzers::AnalyzerRule {
    crate::analyzers::AnalyzerRule {
        id: t.id.clone(),
        name: t.name.clone(),
        engine: if t.check.engine == "cloud" {
            "cloud"
        } else {
            "ollama"
        }
        .into(),
        think: t.check.think,
        instructions: t.check.prompt.clone(),
        fields,
        match_mode: t.check.match_mode.clone(),
        conditions: t.check.conditions.clone(),
        ..Default::default()
    }
}

fn run_check(state: &AppState, t: &Tripwire, f: &CallFacts) -> Verdict {
    match t.check.kind.as_str() {
        "ask" if !t.check.prompt.trim().is_empty() => {
            let res = if t.check.engine == "cloud" {
                let mut r = as_analyzer(
                    t,
                    vec![
                        // Not "fire": on fire/EMS radio a model reads that
                        // key as "is there a fire?" and answers the wrong
                        // question.
                        Field {
                            key: "send".into(),
                            kind: "bool".into(),
                            desc: "true if, by the listener's instruction, they should be alerted about this call"
                                .into(),
                        },
                        Field {
                            key: "summary".into(),
                            kind: "string".into(),
                            desc: "one sentence on why".into(),
                        },
                    ],
                );
                r.instructions = format!(
                    "Decide whether to send the listener an alert about this call. Instruction from the listener: {}",
                    t.check.prompt.trim()
                );
                crate::analyzers::run_extract(state, &r, f).map(|v| {
                    let fire = matches!(&v["send"], serde_json::Value::Bool(true))
                        || v["send"].as_str().is_some_and(|s| {
                            s.eq_ignore_ascii_case("true") || s.eq_ignore_ascii_case("yes")
                        });
                    (fire, v["summary"].as_str().unwrap_or("").to_string())
                })
            } else {
                let ollama = crate::alerts::shared_settings(state).1;
                crate::alerts::ask_ollama(&ollama, &t.check.prompt, f, t.check.think)
            };
            match res {
                Ok((true, s)) => Verdict::Pass {
                    note: s,
                    fields: None,
                },
                Ok((false, s)) => Verdict::Quiet {
                    why: format!("AI check said no: {s}"),
                    fields: None,
                },
                Err(e) => Verdict::Unavailable(e),
            }
        }
        "extract" if !t.check.prompt.trim().is_empty() => {
            let r = as_analyzer(t, t.check.fields.clone());
            match crate::analyzers::run_extract(state, &r, f) {
                Ok(obj) => {
                    if crate::analyzers::evaluate(&r, &obj) {
                        Verdict::Pass {
                            note: String::new(),
                            fields: Some(obj),
                        }
                    } else {
                        Verdict::Quiet {
                            why: "conditions not met".into(),
                            fields: Some(obj),
                        }
                    }
                }
                Err(e) => Verdict::Unavailable(e),
            }
        }
        _ => Verdict::Pass {
            note: String::new(),
            fields: None,
        },
    }
}

/// Run the check once, for the preview: (`send` | `quiet` | `unavailable`,
/// the model's note, extracted fields).
pub fn check_once(
    state: &AppState,
    t: &Tripwire,
    f: &CallFacts,
) -> (String, String, Option<serde_json::Value>) {
    match run_check(state, t, f) {
        Verdict::Pass { note, fields } => ("send".into(), note, fields),
        Verdict::Quiet { why, fields } => ("quiet".into(), why, fields),
        Verdict::Unavailable(e) => ("unavailable".into(), e, None),
    }
}

/// Is there a live reply thread this tripwire's repeat on `tg` belongs to?
fn open_thread(st: &TripState, rule: &str, tg: u16, now: i64) -> Option<(String, i64)> {
    st.threads
        .iter()
        .find(|th| th.rule == rule && th.tg == tg && th.until > now && th.replies < MAX_REPLIES)
        .map(|th| (th.dest.clone(), th.root))
}

/// Run one tripwire for one call: quiet window, check, send, record.
fn fire(app: &AppHandle, t: Tripwire, f: CallFacts, keywords: Vec<String>) {
    fire_with(app, t, f, keywords, None, None)
}

/// `extra` are facts the message can use that did not come from a check —
/// what a run is, where it is, which hospital took the patient.
fn fire_with(
    app: &AppHandle,
    t: Tripwire,
    f: CallFacts,
    keywords: Vec<String>,
    extra: Option<serde_json::Value>,
    incident: Option<i64>,
) {
    // Every history row this attempt writes is about the same run.
    let rec = |app: &AppHandle,
               t: &Tripwire,
               f: &CallFacts,
               status: &str,
               detail: String,
               message: String,
               keywords: &[String],
               fields: Option<&serde_json::Value>,
               sent: (String, Vec<i64>)| {
        record_for(app, t, f, status, detail, message, keywords, fields, sent, incident)
    };
    let state = app.state::<AppState>();
    let now = crate::library::now();
    let key = (t.id.clone(), f.tg);
    // Inside a live thread a repeat becomes a reply; otherwise the quiet
    // window drops it before the model is asked anything.
    let reply_to = {
        let st = state.tripwires.lock().unwrap();
        let thread = if t.send.follow != "off" {
            open_thread(&st, &t.id, f.tg, now)
        } else {
            None
        };
        if thread.is_none() {
            if let Some(last) = st.last_sent.get(&key) {
                if t.send.quiet_secs > 0 && now - last < t.send.quiet_secs as i64 {
                    return;
                }
            }
        }
        thread
    };
    let (note, fields) = match run_check(&state, &t, &f) {
        Verdict::Pass { note, fields } => (note, fields),
        Verdict::Quiet { why, fields } => {
            rec(
                app,
                &t,
                &f,
                "quiet",
                why,
                String::new(),
                &keywords,
                fields.as_ref(),
                (String::new(), Vec::new()),
            );
            return;
        }
        Verdict::Unavailable(e) => {
            if t.check.if_unavailable == "hold" {
                rec(
                    app,
                    &t,
                    &f,
                    "held",
                    format!("check unavailable, held: {e}"),
                    String::new(),
                    &keywords,
                    None,
                    (String::new(), Vec::new()),
                );
                return;
            }
            (format!("(AI check unavailable: {e})"), None)
        }
    };
    // What the run is, underneath whatever the check found: a check that
    // names the same key wins, because the listener asked for it.
    let fields = match (extra, fields) {
        (Some(mut base), Some(found)) => {
            if let (Some(b), Some(f)) = (base.as_object_mut(), found.as_object()) {
                for (k, v) in f {
                    b.insert(k.clone(), v.clone());
                }
            }
            Some(base)
        }
        (Some(base), None) => Some(base),
        (None, found) => found,
    };
    let is_reply = reply_to.is_some();
    let message = if is_reply {
        follow_text(&f)
    } else {
        render(
            &t.send.message,
            &t.name,
            &f,
            &keywords,
            &note,
            fields.as_ref(),
        )
    };
    let _ = app.emit(
        "alert",
        serde_json::json!({ "name": t.name, "tg": f.tg, "message": message, "tone": t.send.tone && !is_reply, "call": f.id, "follow": is_reply }),
    );
    if !t.send.telegram {
        state.tripwires.lock().unwrap().last_sent.insert(key, now);
        rec(
            app,
            &t,
            &f,
            "sent",
            "in the app only".into(),
            message,
            &keywords,
            fields.as_ref(),
            (String::new(), Vec::new()),
        );
        return;
    }
    let dest = match &reply_to {
        Some((d, _)) => Ok(d.clone()),
        None => resolve_dest(&state.alerts.lock().unwrap().settings, &t.send),
    };
    let dest = match dest {
        Ok(d) => d,
        Err(e) => {
            rec(
                app,
                &t,
                &f,
                "failed",
                e,
                message,
                &keywords,
                fields.as_ref(),
                (String::new(), Vec::new()),
            );
            return;
        }
    };
    let res = deliver(
        &state,
        &dest,
        &f,
        &message,
        &t.name,
        t.send.audio,
        if is_reply { 0 } else { t.send.earlier_calls },
        t.send.earlier_window_secs,
        reply_to.as_ref().map(|r| r.1),
    );
    match res {
        Ok((detail, mut ids)) => {
            {
                let mut st = state.tripwires.lock().unwrap();
                if let Some((_, root)) = &reply_to {
                    if let Some(th) = st
                        .threads
                        .iter_mut()
                        .find(|th| th.rule == t.id && th.root == *root)
                    {
                        th.replies += 1;
                        th.calls.extend(f.id);
                    }
                } else {
                    st.last_sent.insert(key, now);
                    if t.send.follow != "off" {
                        if let Some(root) = ids.first() {
                            st.threads.retain(|th| !(th.rule == t.id && th.tg == f.tg));
                            st.threads.push(Thread {
                                rule: t.id.clone(),
                                tg: f.tg,
                                unit: f.unit,
                                scope: t.send.follow.clone(),
                                dest: dest.clone(),
                                root: *root,
                                until: now + t.send.follow_mins as i64 * 60,
                                replies: 0,
                                calls: f.id.into_iter().collect(),
                            });
                        }
                    }
                }
            }
            let mut detail = if is_reply {
                format!("follow-up {detail}")
            } else {
                detail
            };
            // The picture hangs off the message that was just sent, so it
            // is drawn after it and never in its way. The thread root is
            // already recorded above, so a photo id can't become one.
            if t.send.map {
                if let Some(inc) = incident {
                    match send_map(app, &state, &dest, inc, ids.first().copied()) {
                        Ok(id) => {
                            ids.push(id);
                            detail.push_str(", with a map");
                        }
                        Err(e) => detail.push_str(&format!(", map failed: {e}")),
                    }
                }
            }
            rec(
                app,
                &t,
                &f,
                "sent",
                detail,
                message,
                &keywords,
                fields.as_ref(),
                (dest, ids),
            );
        }
        Err(e) => rec(
            app,
            &t,
            &f,
            "failed",
            e,
            message,
            &keywords,
            fields.as_ref(),
            (dest, Vec::new()),
        ),
    }
}

/// Later traffic inside a live thread's scope, sent as a reply. `fired` are
/// the tripwires this call trips on its own (they reply for themselves).
/// The live threads this call belongs to, claiming each one as it goes: the
/// reply is counted and the call remembered here, so a second transcript for
/// the same call cannot send it twice.
fn claim_threads(
    threads: &mut [Thread],
    f: &CallFacts,
    fired: &HashSet<String>,
    rules: &HashMap<String, Tripwire>,
    now: i64,
) -> Vec<(Thread, Tripwire)> {
    let mut out = Vec::new();
    for th in threads.iter_mut() {
        if th.until <= now || th.replies >= MAX_REPLIES || fired.contains(&th.rule) {
            continue;
        }
        if f.id.is_some_and(|id| th.calls.contains(&id)) {
            continue;
        }
        let inside = match th.scope.as_str() {
            "radio" => th.unit != 0 && f.unit == th.unit,
            "channel" => f.tg == th.tg,
            _ => false,
        };
        let Some(t) = rules.get(&th.rule).filter(|t| t.enabled) else {
            continue;
        };
        if inside {
            th.replies += 1;
            th.calls.extend(f.id);
            out.push((th.clone(), t.clone()));
        }
    }
    out
}

fn follow_ups(app: &AppHandle, f: &CallFacts, fired: &HashSet<String>) {
    if f.transcript.as_deref().is_none_or(|t| t.trim().is_empty()) {
        return;
    }
    let state = app.state::<AppState>();
    let now = crate::library::now();
    let due: Vec<(Thread, Tripwire)> = {
        let mut st = state.tripwires.lock().unwrap();
        let rules: HashMap<String, Tripwire> = st
            .settings
            .tripwires
            .iter()
            .map(|t| (t.id.clone(), t.clone()))
            .collect();
        claim_threads(&mut st.threads, f, fired, &rules, now)
    };
    for (th, t) in due {
        let (app, f) = (app.clone(), f.clone());
        std::thread::spawn(move || {
            let state = app.state::<AppState>();
            let message = follow_text(&f);
            let _ = app.emit(
                "alert",
                serde_json::json!({ "name": t.name, "tg": f.tg, "message": message, "tone": false, "call": f.id, "follow": true }),
            );
            let res = deliver(
                &state,
                &th.dest,
                &f,
                &message,
                &t.name,
                t.send.audio,
                0,
                0,
                Some(th.root),
            );
            match res {
                Ok((d, ids)) => record(
                    &app,
                    &t,
                    &f,
                    "sent",
                    format!("follow-up {d}"),
                    message,
                    &[],
                    None,
                    (th.dest.clone(), ids),
                ),
                Err(e) => record(
                    &app,
                    &t,
                    &f,
                    "failed",
                    format!("follow-up: {e}"),
                    message,
                    &[],
                    None,
                    (th.dest.clone(), Vec::new()),
                ),
            }
        });
    }
}

/// Send text, or the audio with the text as its caption; as a reply when
/// `reply_to` is set. Returns a short detail and the Telegram message ids.
#[allow(clippy::too_many_arguments)]
fn deliver(
    state: &AppState,
    dest: &str,
    f: &CallFacts,
    message: &str,
    title: &str,
    audio: bool,
    earlier: u32,
    earlier_window: u32,
    reply_to: Option<i64>,
) -> Result<(String, Vec<i64>), String> {
    let clip = if audio {
        crate::alerts::clip_for_call(f, earlier, earlier_window, state)?
    } else {
        None
    };
    match clip {
        None => crate::alerts::send_text_reply(dest, message, reply_to)
            .map(|id| ("sent".to_string(), vec![id])),
        Some((path, is_mp3)) => {
            let res = crate::alerts::send_audio_reply(
                dest, &path, is_mp3, message, title, &f.tg_name, reply_to,
            );
            let _ = std::fs::remove_file(&path);
            res.map(|ids| {
                (
                    format!("sent with {}", if is_mp3 { "MP3" } else { "WAV" }),
                    ids,
                )
            })
        }
    }
}

/// One outcome into the tripwire history. A failed send or a held check is
/// an error the listener hears about; a quiet check is the check working.
#[allow(clippy::too_many_arguments)]
fn record(
    app: &AppHandle,
    t: &Tripwire,
    f: &CallFacts,
    status: &str,
    detail: String,
    message: String,
    keywords: &[String],
    fields: Option<&serde_json::Value>,
    sent: (String, Vec<i64>),
) {
    record_for(app, t, f, status, detail, message, keywords, fields, sent, None)
}

/// As `record`, but naming the run it was about, so a run is never told
/// about twice and the timeline can show what was sent.
#[allow(clippy::too_many_arguments)]
fn record_for(
    app: &AppHandle,
    t: &Tripwire,
    f: &CallFacts,
    status: &str,
    detail: String,
    message: String,
    keywords: &[String],
    fields: Option<&serde_json::Value>,
    (chat, message_ids): (String, Vec<i64>),
    incident_id: Option<i64>,
) {
    crate::events::record(
        app,
        crate::events::NewEvent {
            source: "tripwire",
            rule_id: t.id.clone(),
            rule_name: t.name.clone(),
            tg: f.tg,
            tg_name: f.tg_name.clone(),
            status: status.to_string(),
            detail: detail.clone(),
            message,
            chat,
            message_ids,
            data: serde_json::json!({ "keywords": keywords, "fields": fields }).to_string(),
            calls: f.id.into_iter().collect(),
            incident_id,
            ..Default::default()
        },
    );
    if status == "failed" || status == "held" {
        let _ = app.emit("alert_error", format!("{}: {detail}", t.name));
    }
    let _ = app.emit("tripwires", ());
}

// ---------------------------------------------------------------------------
// recipes: where a new tripwire starts
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone, Debug)]
pub struct Recipe {
    pub id: String,
    pub icon: String,
    pub title: String,
    pub blurb: String,
    pub tripwire: Tripwire,
}

fn recipe(id: &str, icon: &str, title: &str, blurb: &str, mut t: Tripwire) -> Recipe {
    t.recipe = id.into();
    if t.name.is_empty() {
        t.name = title.into();
    }
    Recipe {
        id: id.into(),
        icon: icon.into(),
        title: title.into(),
        blurb: blurb.into(),
        tripwire: t,
    }
}

/// Starting points. Talkgroups are left for the listener to pick.
pub fn recipes() -> Vec<Recipe> {
    let follow = |mut s: Send| {
        s.follow = "radio".into();
        s
    };
    let mut out = vec![
        recipe(
            "run-type",
            "🚑",
            "A kind of run is dispatched",
            "Tell me when the dispatch map shows a run of a type I care about — with the address, who was sent, and the hospitals that kind of run needs.",
            Tripwire {
                when: When {
                    kind: "incident".into(),
                    incident: IncOpts {
                        call_types: vec!["Cardiac Arrest".into(), "Structure Fire".into()],
                        ..Default::default()
                    },
                    ..Default::default()
                },
                send: Send {
                    message: "{calltype} · {address}\n{units}\n{summary}\n{where}".into(),
                    // This recipe is about runs on a map, so it shows one.
                    map: true,
                    ..Default::default()
                },
                ..Default::default()
            },
        ),
        recipe(
            "run-outcome",
            "🏥",
            "Where an arrest ended up",
            "Wait for the crew to report to a hospital, then tell me the run and where the patient went — and how far it was from the nearest cath lab.",
            Tripwire {
                when: When {
                    kind: "incident".into(),
                    incident: IncOpts {
                        call_types: vec!["Cardiac Arrest".into()],
                        near_feature: "stemi".into(),
                        linked_only: true,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                send: Send {
                    message: "{calltype} · {address}\n{units}\nWent to {hospital}\n{report}\nNearest cath lab: {nearest} ({km} km)".into(),
                    quiet_secs: 0,
                    ..Default::default()
                },
                ..Default::default()
            },
        ),
        recipe(
            "words",
            "🔤",
            "Words on a channel",
            "Tell me when someone says one of these phrases on the talkgroups I pick.",
            Tripwire {
                when: When {
                    phrases: vec!["working fire".into(), "second alarm".into()],
                    ..Default::default()
                },
                send: follow(Send::default()),
                ..Default::default()
            },
        ),
        recipe(
            "arrest",
            "🫀",
            "Cardiac arrest, checked by AI",
            "Arrest phrases, then the local model confirms CPR is happening now before anything is sent.",
            Tripwire {
                when: When {
                    phrases: vec![
                        "cardiac arrest".into(),
                        "working arrest".into(),
                        "CPR in progress".into(),
                        "pulseless".into(),
                        "v fib".into(),
                    ],
                    except: vec!["history of cardiac arrest".into()],
                    ..Default::default()
                },
                check: Check {
                    kind: "ask".into(),
                    prompt: "Is this a cardiac arrest happening now — not a patient with a history of one, a training call, or a cancelled response? Say yes only if CPR or defibrillation is under way or about to start.".into(),
                    ..Default::default()
                },
                send: follow(Send {
                    message: "🫀 {name}\n{tgname} · {unitname} · {time}\n{ai}\n\n{transcript}".into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ),
        recipe(
            "emergency",
            "🆘",
            "Emergency button",
            "Any radio pressing its emergency button on the talkgroups I pick.",
            Tripwire {
                when: When {
                    emergency: true,
                    ..Default::default()
                },
                send: follow(Send {
                    message: "🆘 Emergency — {tgname}\n{unitname} · {time}".into(),
                    quiet_secs: 120,
                    ..Default::default()
                }),
                ..Default::default()
            },
        ),
        recipe(
            "radio",
            "📻",
            "A radio keys up",
            "Whenever one particular radio talks — a unit, a supervisor, a friend.",
            Tripwire {
                send: Send {
                    message: "📻 {unitname} on {tgname} · {time}\n{transcript}".into(),
                    quiet_secs: 900,
                    ..Default::default()
                },
                ..Default::default()
            },
        ),
        recipe(
            "channel",
            "📡",
            "Every call on a quiet channel",
            "Each transmission on a talkgroup that is usually silent.",
            Tripwire {
                send: Send {
                    message: "📡 {tgname} · {unitname} · {time}\n{transcript}".into(),
                    quiet_secs: 120,
                    ..Default::default()
                },
                ..Default::default()
            },
        ),
        recipe(
            "hospital",
            "🏥",
            "Hospital reports, summarised",
            "Each EMS-to-hospital exchange becomes one hand-off note with the combined audio.",
            Tripwire {
                when: When {
                    kind: "conversation".into(),
                    ..Default::default()
                },
                check: Check {
                    kind: "summarize".into(),
                    prompt: crate::conversations::Rule::default().summary_prompt,
                    ..Default::default()
                },
                send: Send {
                    message: crate::conversations::Rule::default().message,
                    tone: false,
                    quiet_secs: 0,
                    ..Default::default()
                },
                ..Default::default()
            },
        ),
        recipe(
            "digest",
            "🗞️",
            "What's happening, every 15 minutes",
            "A short roll-up of the talkgroups I pick, on a timer.",
            Tripwire {
                when: When {
                    kind: "digest".into(),
                    ..Default::default()
                },
                check: Check {
                    kind: "summarize".into(),
                    prompt: crate::digest::DigestRule::default().prompt,
                    ..Default::default()
                },
                send: Send {
                    message: crate::digest::DigestRule::default().message,
                    audio: false,
                    tone: false,
                    quiet_secs: 0,
                    ..Default::default()
                },
                ..Default::default()
            },
        ),
    ];
    // The analyzer screens (ECPR, stroke, arrest status and outcome).
    for (i, a) in crate::analyzers::builtin_templates()
        .into_iter()
        .enumerate()
    {
        let id = format!("screen-{}", i + 1);
        let t = Tripwire {
            name: a.name.clone(),
            when: When {
                phrases: a.keywords.clone(),
                ..Default::default()
            },
            check: Check {
                kind: "extract".into(),
                prompt: a.instructions.clone(),
                fields: a.fields.clone(),
                match_mode: a.match_mode.clone(),
                conditions: a.conditions.clone(),
                if_unavailable: "hold".into(),
                ..Default::default()
            },
            send: Send {
                message: a.message.clone(),
                audio: a.attach_audio,
                quiet_secs: a.cooldown_secs,
                ..Default::default()
            },
            ..Default::default()
        };
        let blurb = match i {
            0 => "Screens arrest reports for ED-ECPR candidacy and sends the ones worth a look.",
            1 => "Pulls last-known-well and deficits out of stroke reports.",
            2 => "Only while compressions are actually going on.",
            _ => "ROSC or termination, when it is called.",
        };
        out.push(recipe(&id, "🔎", &a.name, blurb, t));
    }
    out
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

/// Sent / quiet / failed per tripwire over the last week, every source the
/// rule has fired under (a migrated alert's history is still its own).
#[derive(Serialize, Clone, Debug, Default)]
pub struct Stat {
    pub sent: u32,
    pub quiet: u32,
    pub failed: u32,
    /// Epoch seconds; 0 = never.
    pub last_at: i64,
    pub last_sent_at: i64,
}

#[derive(Serialize)]
pub struct View {
    pub tripwires: Vec<Tripwire>,
    pub stats: HashMap<String, Stat>,
    pub has_token: bool,
    pub ffmpeg: bool,
    /// Transcription is on (phrase and AI tripwires depend on it).
    pub transcribing: bool,
}

fn stats_by_rule(state: &AppState, since: i64) -> HashMap<String, Stat> {
    let mut out: HashMap<String, Stat> = HashMap::new();
    let Some(db) = state.db.lock().unwrap().clone() else {
        return out;
    };
    let c = db.lock().unwrap();
    for s in crate::events::stats(&c, since).unwrap_or_default() {
        let e = out.entry(s.rule_id.clone()).or_default();
        e.sent += s.sent;
        e.quiet += s.quiet;
        e.failed += s.failed;
        e.last_at = e.last_at.max(s.last_at);
        e.last_sent_at = e.last_sent_at.max(s.last_sent_at);
    }
    out
}

#[tauri::command]
pub fn tripwires_get(state: State<AppState>) -> View {
    let tripwires = state.tripwires.lock().unwrap().settings.tripwires.clone();
    let stats = stats_by_rule(&state, crate::library::now() - 7 * 86_400);
    let transcribing = state.transcriber.lock().unwrap().settings.enabled;
    View {
        tripwires,
        stats,
        has_token: crate::secrets::get("bot-token").is_some(),
        ffmpeg: crate::encode::ffmpeg_available().is_some(),
        transcribing,
    }
}

/// Save the whole list. Returns it as stored (ids filled in, text tidied).
#[tauri::command]
pub fn tripwires_set(
    app: AppHandle,
    state: State<AppState>,
    tripwires: Vec<Tripwire>,
) -> Result<Vec<Tripwire>, String> {
    if tripwires.len() > MAX_TRIPWIRES {
        return Err(format!(
            "{} tripwires is more than the limit of {MAX_TRIPWIRES}",
            tripwires.len()
        ));
    }
    let mut list = tripwires;
    let mut ids = HashSet::new();
    for (i, t) in list.iter_mut().enumerate() {
        sanitize(t)?;
        if t.id.is_empty() || !ids.insert(t.id.clone()) {
            t.id = format!("t{}-{i}", crate::library::now());
            ids.insert(t.id.clone());
        }
        if t.name.is_empty() {
            t.name = format!("Tripwire {}", i + 1);
        }
    }
    {
        let mut st = state.tripwires.lock().unwrap();
        st.settings.tripwires = list.clone();
        let keep: HashSet<&String> = list.iter().map(|t| &t.id).collect();
        st.last_sent.retain(|k, _| keep.contains(&k.0));
        st.threads.retain(|th| keep.contains(&th.rule));
        store(&app, &st.settings)?;
    }
    compile(&app);
    let _ = app.emit("tripwires", ());
    Ok(list)
}

#[tauri::command]
pub fn tripwire_recipes() -> Vec<Recipe> {
    recipes()
}

/// Parse a shared file for review; nothing is saved.
#[tauri::command]
pub fn tripwires_import(text: String) -> Result<Bundle, String> {
    parse_bundle(&text)
}

/// Some (or, with no ids, all) tripwires as a shareable file's text.
#[tauri::command]
pub fn tripwires_export(
    state: State<AppState>,
    ids: Vec<String>,
    name: String,
    author: String,
    description: String,
) -> Result<String, String> {
    let list: Vec<Tripwire> = state
        .tripwires
        .lock()
        .unwrap()
        .settings
        .tripwires
        .iter()
        .filter(|t| ids.is_empty() || ids.contains(&t.id))
        .cloned()
        .collect();
    if list.is_empty() {
        return Err("no tripwires to export".into());
    }
    serde_json::to_string_pretty(&make_bundle(list, &name, &author, &description))
        .map_err(|e| e.to_string())
}

/// Run a tripwire by hand, now: a call tripwire against the newest stored
/// call it would match (or the newest on its talkgroups), skipping the
/// quiet window; a conversation or digest through its engine.
#[tauri::command]
pub async fn tripwire_test(app: AppHandle, id: String) -> Result<String, String> {
    let t = app
        .state::<AppState>()
        .tripwires
        .lock()
        .unwrap()
        .settings
        .tripwires
        .iter()
        .find(|t| t.id == id)
        .cloned()
        .ok_or("no such tripwire")?;
    match t.when.kind.as_str() {
        "conversation" => crate::conversations::conversation_test(app, id).await,
        "digest" => tauri::async_runtime::spawn_blocking(move || crate::digest::run_now(app, &id))
            .await
            .map_err(|e| e.to_string())?,
        _ => tauri::async_runtime::spawn_blocking(move || {
            let state = app.state::<AppState>();
            let db = state.db.lock().unwrap().clone().ok_or("library not open")?;
            let rows = {
                let c = db.lock().unwrap();
                let mut rows = Vec::new();
                let tgs: Vec<Option<u16>> = if t.when.tgs.is_empty() {
                    vec![None]
                } else {
                    t.when.tgs.iter().map(|x| Some(*x)).collect()
                };
                for tg in tgs {
                    rows.extend(
                        crate::library::search(
                            &c,
                            &crate::library::Query {
                                tg,
                                unit: t
                                    .when
                                    .units
                                    .first()
                                    .copied()
                                    .filter(|_| t.when.units.len() == 1),
                                limit: Some(200),
                                ..Default::default()
                            },
                        )
                        .unwrap_or_default(),
                    );
                }
                rows.sort_by_key(|r| std::cmp::Reverse(r.start));
                rows
            };
            let mut test = t.clone();
            test.enabled = true;
            test.send.quiet_secs = 0;
            test.send.follow = "off".into();
            let facts: Vec<CallFacts> = rows
                .into_iter()
                .map(|r| crate::alerts::facts_from_row(&app, r, None))
                .collect();
            let hit = facts
                .iter()
                .find_map(|f| matches(&test, f).map(|kw| (f.clone(), kw)));
            let (f, kw, how) = match hit {
                Some((f, kw)) => (f, kw, "the newest call it matches"),
                None => {
                    let mut f = facts.into_iter().next().unwrap_or(CallFacts {
                        start: crate::library::now(),
                        tg: t.when.tgs.first().copied().unwrap_or(0),
                        tg_name: "Test talkgroup".into(),
                        ..Default::default()
                    });
                    if f.transcript.is_none() {
                        f.transcript = Some("(test — no transcript on this call)".into());
                    }
                    (
                        f,
                        Vec::new(),
                        "the newest call on its talkgroups (nothing matched it yet)",
                    )
                }
            };
            let desc = format!(
                "“{}” against {how}: {} · {}",
                t.name,
                f.tg_name,
                crate::library::local_hm(f.start)
            );
            let app2 = app.clone();
            std::thread::spawn(move || fire(&app2, test, f, kw));
            Ok(format!("firing {desc}"))
        })
        .await
        .map_err(|e| e.to_string())?,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(text: &str) -> CallFacts {
        CallFacts {
            id: Some(1),
            start: 0,
            tg: 20308,
            tg_name: "Medic 3".into(),
            unit: 790065,
            unit_name: Some("Medic 3".into()),
            secs: 6.0,
            transcript: Some(text.into()),
            system: "Test System".into(),
            ..Default::default()
        }
    }

    fn thread(scope: &str, until: i64) -> Thread {
        Thread {
            rule: "t1".into(),
            tg: 20308,
            unit: 790065,
            scope: scope.into(),
            dest: String::new(),
            root: 42,
            until,
            replies: 0,
            calls: HashSet::new(),
        }
    }

    #[test]
    fn a_live_thread_claims_later_traffic_once() {
        let t = words(&["cardiac arrest"], &[]);
        let rules: HashMap<String, Tripwire> = [("t1".to_string(), t)].into_iter().collect();
        let none = HashSet::new();
        let mut threads = vec![thread("channel", 100)];

        // The first transcript of call 1 replies.
        let f = facts("engine 3 responding");
        assert_eq!(claim_threads(&mut threads, &f, &none, &rules, 0).len(), 1);
        assert_eq!(threads[0].replies, 1);
        // A second transcript for the same call does not.
        assert!(claim_threads(&mut threads, &f, &none, &rules, 0).is_empty());
        assert_eq!(threads[0].replies, 1);

        // A call on another channel is outside a "channel" thread.
        let mut other = f.clone();
        other.id = Some(2);
        other.tg = 20309;
        assert!(claim_threads(&mut threads, &other, &none, &rules, 0).is_empty());

        // Nothing replies once the window closes, or after the cap.
        let mut third = f.clone();
        third.id = Some(3);
        assert!(claim_threads(&mut threads, &third, &none, &rules, 200).is_empty());
        threads[0].replies = MAX_REPLIES;
        assert!(claim_threads(&mut threads, &third, &none, &rules, 0).is_empty());
    }

    #[test]
    fn a_tripwire_that_just_sent_does_not_also_reply_to_itself() {
        let t = words(&["cardiac arrest"], &[]);
        let rules: HashMap<String, Tripwire> = [("t1".to_string(), t)].into_iter().collect();
        let mut threads = vec![thread("channel", 100)];
        let fired: HashSet<String> = ["t1".to_string()].into_iter().collect();
        let f = facts("another cardiac arrest");
        assert!(claim_threads(&mut threads, &f, &fired, &rules, 0).is_empty());
        assert_eq!(threads[0].replies, 0);
    }

    #[test]
    fn a_radio_thread_follows_the_radio_not_the_channel() {
        let t = words(&["cardiac arrest"], &[]);
        let rules: HashMap<String, Tripwire> = [("t1".to_string(), t)].into_iter().collect();
        let none = HashSet::new();
        let mut threads = vec![thread("radio", 100)];

        // Same radio, different channel: still the same story.
        let mut moved = facts("arriving at the hospital");
        moved.tg = 20400;
        assert_eq!(claim_threads(&mut threads, &moved, &none, &rules, 0).len(), 1);

        // Another radio on the original channel is someone else.
        let mut stranger = facts("truck 2 on scene");
        stranger.id = Some(9);
        stranger.unit = 790066;
        assert!(claim_threads(&mut threads, &stranger, &none, &rules, 0).is_empty());
    }

    #[test]
    fn a_deleted_or_switched_off_tripwire_stops_replying() {
        let mut t = words(&["cardiac arrest"], &[]);
        t.enabled = false;
        let off: HashMap<String, Tripwire> = [("t1".to_string(), t)].into_iter().collect();
        let gone: HashMap<String, Tripwire> = HashMap::new();
        let none = HashSet::new();
        let mut threads = vec![thread("channel", 100)];
        let f = facts("still talking");
        assert!(claim_threads(&mut threads, &f, &none, &off, 0).is_empty());
        assert!(claim_threads(&mut threads, &f, &none, &gone, 0).is_empty());
    }

    fn words(ws: &[&str], tgs: &[u16]) -> Tripwire {
        Tripwire {
            id: "t1".into(),
            name: "Arrest".into(),
            when: When {
                phrases: ws.iter().map(|s| s.to_string()).collect(),
                tgs: tgs.to_vec(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn run(call_type: &str, lat: Option<f64>, lon: Option<f64>) -> crate::dispatch::Incident {
        crate::dispatch::Incident {
            pathway: String::new(),
            targets: Vec::new(),
            id: 1,
            created: 100,
            updated: 100,
            tg: 10202,
            tg_name: "EMS Dispatch".into(),
            call_type: call_type.into(),
            emoji: "🫀".into(),
            address: "1 Example Street".into(),
            validated: String::new(),
            lat,
            lon,
            geocode: if lat.is_some() { "ok".into() } else { "none".into() },
            units: vec!["Medic 7".into()],
            summary: "Chest pain, conscious and breathing".into(),
            confidence: 90,
            calls: 1,
            revision: 0,
        }
    }

    fn book() -> crate::places::Settings {
        crate::places::Settings {
            places: vec![
                crate::places::Place {
                    id: "heart".into(),
                    name: "Example Heart".into(),
                    kind: "hospital".into(),
                    lat: Some(40.02),
                    lon: Some(-86.0),
                    features: vec!["stemi".into()],
                    enabled: true,
                    ..Default::default()
                },
                crate::places::Place {
                    id: "general".into(),
                    name: "Example General".into(),
                    kind: "hospital".into(),
                    lat: Some(40.001),
                    lon: Some(-86.0),
                    enabled: true,
                    ..Default::default()
                },
            ],
        }
    }

    fn incident_rule(o: IncOpts) -> Tripwire {
        Tripwire {
            id: "i1".into(),
            name: "Runs".into(),
            when: When {
                kind: "incident".into(),
                incident: o,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn a_run_is_matched_on_what_it_is_and_where_it_is() {
        let places = book();
        let here = run("Cardiac Arrest", Some(40.0), Some(-86.0));

        // Call type, spelled as the dispatch settings spell it.
        let t = incident_rule(IncOpts {
            call_types: vec!["cardiac arrest".into()],
            ..Default::default()
        });
        assert!(matches_incident(&t, &here, &places, false));
        assert!(!matches_incident(&t, &run("Sick Person", Some(40.0), Some(-86.0)), &places, false));

        // Within so far of the nearest place that can do the thing: the
        // cath lab is 2.2 km away, the general hospital is 110 m.
        let near_stemi = incident_rule(IncOpts {
            near_feature: "stemi".into(),
            within_km: 5.0,
            ..Default::default()
        });
        assert!(matches_incident(&near_stemi, &here, &places, false));
        let tight = incident_rule(IncOpts {
            near_feature: "stemi".into(),
            within_km: 1.0,
            ..Default::default()
        });
        assert!(!matches_incident(&tight, &here, &places, false));

        // A run that never made it onto the map is near nothing.
        let nowhere = run("Cardiac Arrest", None, None);
        assert!(!matches_incident(&near_stemi, &nowhere, &places, false));
        assert!(matches_incident(&t, &nowhere, &places, false), "unless distance was never asked about");
    }

    #[test]
    fn waiting_for_the_outcome_means_waiting() {
        let places = book();
        let t = incident_rule(IncOpts {
            linked_only: true,
            ..Default::default()
        });
        let r = run("Cardiac Arrest", Some(40.0), Some(-86.0));
        assert!(!matches_incident(&t, &r, &places, false), "no hospital report yet");
        assert!(matches_incident(&t, &r, &places, true));
    }

    #[test]
    fn a_run_message_can_name_the_nearest_place_that_can_help() {
        let places = book();
        let r = run("Cardiac Arrest", Some(40.0), Some(-86.0));
        let reports = vec![crate::link::LinkedReport {
            id: 9,
            at: 500,
            tg: 10259,
            tg_name: "MED-03".into(),
            tg_desc: String::new(),
            place: "Example General".into(),
            summary: "Medic 7 inbound, ROSC".into(),
            how: "Medic 7 was sent to this run".into(),
        }];
        let fields = incident_fields(&r, &places, "stemi", &reports);
        assert_eq!(fields["nearest"], "Example Heart");
        assert_eq!(fields["km"], "2.2");
        assert_eq!(fields["place"], "Example General", "the nearest place of any kind");
        assert_eq!(fields["hospital"], "Example General");

        let msg = render(
            "{calltype} at {address}\nUnits: {units}\nNearest cath lab: {nearest} ({km} km)\nWent to: {hospital}",
            "Runs",
            &incident_facts(&r),
            &[],
            "",
            Some(&fields),
        );
        assert!(msg.contains("Cardiac Arrest at 1 Example Street"), "{msg}");
        assert!(msg.contains("Nearest cath lab: Example Heart (2.2 km)"), "{msg}");
        assert!(msg.contains("Went to: Example General"), "{msg}");
    }

    fn target(label: &str, place: &str, meters: f64, secs: f64, how: &str) -> crate::pathways::Target {
        crate::pathways::Target {
            label: label.into(),
            place_id: place.to_lowercase(),
            place_name: place.into(),
            lat: 40.02,
            lon: -86.0,
            meters,
            secs,
            how: how.into(),
        }
    }

    #[test]
    fn a_run_message_can_say_every_hospital_the_pathway_asked_for() {
        let places = book();
        let mut r = run("Cardiac Arrest", Some(40.0), Some(-86.0));
        r.pathway = "Cardiac arrest".into();
        r.targets = vec![
            target("Closest hospital", "Example General", 3200.0, 420.0, "road"),
            target("ECMO centre", "Example Heart", 21000.0, 1500.0, "road"),
        ];
        let fields = incident_fields(&r, &places, "stemi", &[]);
        assert_eq!(fields["pathway"], "Cardiac arrest");

        let msg = render(
            "{calltype} · {address}\n{where}",
            "Runs",
            &incident_facts(&r),
            &[],
            "",
            Some(&fields),
        );
        // Both legs, each with its own drive — this is the whole point of
        // the token.
        assert!(msg.contains("Closest hospital"), "{msg}");
        assert!(msg.contains("Example General"), "{msg}");
        assert!(msg.contains("ECMO centre"), "{msg}");
        assert!(msg.contains("Example Heart"), "{msg}");
        assert!(msg.contains('7'), "no drive time for the 420 s leg: {msg}");
    }

    #[test]
    fn the_picture_is_credited_and_says_what_it_is_showing() {
        let mut r = run("Cardiac Arrest", Some(40.0), Some(-86.0));
        r.targets = vec![
            target("Closest hospital", "Example General", 3200.0, 420.0, "road"),
            target("ECMO centre", "Example Heart", 21000.0, 1500.0, "road"),
        ];
        let shot = crate::mapshot::Shot {
            png: Vec::new(),
            bar_m: 3218.688,
            missing: 0,
        };
        let cap = photo_caption(&r, &shot);
        assert!(!cap.contains("approximate"), "an exact pin was hedged: {cap}");
        assert!(cap.contains("Cardiac Arrest · 1 Example Street"), "{cap}");
        assert!(cap.contains("Example General"), "{cap}");
        assert!(cap.contains("ECMO centre"), "{cap}");
        assert!(cap.contains("bar 2 mi"), "the scale bar is unexplained: {cap}");
        assert!(
            cap.contains("© OpenStreetMap contributors"),
            "the tiles are uncredited: {cap}"
        );
        // Telegram's own limit, which the sender also truncates to.
        assert!(cap.chars().count() <= 1024, "{} chars", cap.chars().count());

        // A picture with holes in it says so.
        let holes = crate::mapshot::Shot {
            png: Vec::new(),
            bar_m: 0.0,
            missing: 3,
        };
        let cap = photo_caption(&r, &holes);
        assert!(cap.contains("3 map tiles missing"), "{cap}");
        assert!(!cap.contains("bar "), "a bar was described but not drawn: {cap}");
    }

    #[test]
    fn a_pin_fitted_from_the_grid_is_not_drawn_as_a_certainty() {
        let shot = crate::mapshot::Shot {
            png: Vec::new(),
            bar_m: 1609.344,
            missing: 0,
        };
        let mut r = run("Cardiac Arrest", Some(40.0), Some(-86.0));
        r.geocode = "grid".into();
        let cap = photo_caption(&r, &shot);
        assert!(
            cap.contains("position approximate"),
            "a grid pin was drawn as exact: {cap}"
        );
        // A corrected street name is a real geocode, not a guess.
        r.geocode = "corrected".into();
        assert!(!photo_caption(&r, &shot).contains("approximate"));
        r.geocode = "manual".into();
        assert!(!photo_caption(&r, &shot).contains("approximate"));
    }

    #[test]
    fn the_credit_survives_a_caption_that_runs_long() {
        let mut r = run("Cardiac Arrest", Some(40.0), Some(-86.0));
        // Six legs with names far longer than any real hospital's.
        r.targets = (0..6)
            .map(|n| {
                target(
                    &format!("Destination number {n} with a very long label indeed"),
                    &format!("{} Regional Medical Center and Trauma Campus", "Example ".repeat(8)),
                    4000.0,
                    600.0,
                    "road",
                )
            })
            .collect();
        let shot = crate::mapshot::Shot {
            png: Vec::new(),
            bar_m: 1609.344,
            missing: 0,
        };
        let cap = photo_caption(&r, &shot);
        assert!(
            cap.contains("© OpenStreetMap contributors"),
            "the credit was pushed out: {cap}"
        );
        // And it is still there after the sender's own truncation.
        let sent: String = cap.chars().take(1000).collect();
        assert!(
            sent.contains("© OpenStreetMap contributors"),
            "the credit was truncated away by the sender: {sent}"
        );
    }

    #[test]
    fn a_straight_line_guess_never_becomes_a_bare_drive_time() {
        let places = book();
        let mut r = run("Cardiac Arrest", Some(40.0), Some(-86.0));
        // The pathway reached the same place {nearest} names, but only as
        // the crow flies.
        r.targets = vec![target("Cath lab", "Example Heart", 2200.0, 300.0, "straight")];
        let fields = incident_fields(&r, &places, "stemi", &[]);
        assert_eq!(fields["nearest"], "Example Heart");
        assert_eq!(fields["mins"], "", "a guess was offered as an ETA");

        // Routed by road, the same leg does fill the token.
        r.targets = vec![target("Cath lab", "Example Heart", 2200.0, 300.0, "road")];
        let fields = incident_fields(&r, &places, "stemi", &[]);
        assert_eq!(fields["mins"], "5");
    }

    #[test]
    fn phrases_talkgroups_and_exceptions_decide_a_match() {
        let t = words(&["cardiac arrest", "CPR"], &[20308]);
        assert_eq!(
            matches(&t, &facts("confirmed cardiac arrest, starting CPR")),
            Some(vec!["cardiac arrest".into(), "CPR".into()])
        );
        let mut other = facts("cardiac arrest");
        other.tg = 1;
        assert_eq!(matches(&t, &other), None, "another talkgroup");
        let mut t2 = t.clone();
        t2.when.except = vec!["history of cardiac arrest".into()];
        assert_eq!(
            matches(&t2, &facts("patient with a history of cardiac arrest")),
            None
        );
        let mut off = t.clone();
        off.enabled = false;
        assert_eq!(matches(&off, &facts("cardiac arrest")), None);
        // Before the transcript exists, a phrase tripwire cannot decide.
        let mut f = facts("");
        f.transcript = None;
        assert_eq!(matches(&t, &f), None);
    }

    #[test]
    fn system_scope_tells_same_numbered_talkgroups_apart() {
        let mut t = words(&["cpr"], &[20308]);
        t.when.system = "Test System".into();
        assert!(matches(&t, &facts("cpr")).is_some());
        let mut f = facts("cpr");
        f.system = "Other System".into();
        assert_eq!(matches(&t, &f), None);
        f.system = String::new();
        assert!(
            matches(&t, &f).is_some(),
            "an unknown system is not ruled out"
        );
    }

    #[test]
    fn emergency_radio_and_unnarrowed_tripwires() {
        let mut e = Tripwire::default();
        e.when.emergency = true;
        let mut f = facts("");
        f.transcript = None;
        assert!(!needs_transcript(&e));
        assert_eq!(matches(&e, &f), None);
        f.emergency = true;
        assert_eq!(matches(&e, &f), Some(vec![]));
        let mut u = Tripwire::default();
        u.when.units = vec![790065];
        assert_eq!(matches(&u, &f), Some(vec![]));
        u.when.units = vec![1];
        assert_eq!(matches(&u, &f), None);
        // Nothing narrowing it: never runs (it would fire on every call).
        assert_eq!(matches(&Tripwire::default(), &f), None);
    }

    #[test]
    fn messages_render_tokens_and_drop_empty_lines() {
        let m = render(
            "{name}: {tgname}/{tg} by {unitname} — {keywords}\n{field.score} {json}\n{ai}\n{transcript}",
            "Arrest",
            &facts("starting CPR"),
            &["CPR".into()],
            "",
            Some(&serde_json::json!({ "score": 4 })),
        );
        assert!(m.starts_with("Arrest: Medic 3/20308 by Medic 3 — CPR\n4 {"));
        assert!(m.ends_with("starting CPR"));
        assert!(!m.contains("{ai}"));
        let bare = render(
            "{candidate} {tg}",
            "X",
            &facts(""),
            &[],
            "",
            Some(&serde_json::json!({ "candidate": "maybe", "tg": "no" })),
        );
        assert_eq!(
            bare, "maybe 20308",
            "a field cannot shadow a standard token"
        );
        let plain = render(
            "{name}\n{json}\n{transcript}",
            "X",
            &facts("hi"),
            &[],
            "",
            None,
        );
        assert_eq!(plain, "X\n\nhi");
    }

    fn old_settings() -> (
        crate::alerts::Settings,
        crate::analyzers::Settings,
        crate::conversations::Settings,
        crate::digest::Settings,
    ) {
        let mut al = crate::alerts::Settings::default();
        al.destinations.push(crate::connections::Destination {
            id: "d1".into(),
            name: "Team › Arrests".into(),
            chat_id: "-1001".into(),
            topic_id: "57".into(),
        });
        al.ollama.fail_open = false;
        let mut a = crate::alerts::Alert {
            id: "a1".into(),
            name: "Arrest".into(),
            chat_id: "-1001".into(),
            topic_id: "57".into(),
            ai_gate: true,
            ai_prompt: "Is CPR happening?".into(),
            cooldown_secs: 600,
            combine_prev: 2,
            ..Default::default()
        };
        a.trigger.keywords = vec!["cardiac arrest".into()];
        a.trigger.tgs = vec![20308];
        al.alerts.push(a);
        let mut e = crate::alerts::Alert {
            id: "a2".into(),
            name: "Emergency".into(),
            chat_id: "555".into(),
            ..Default::default()
        };
        e.trigger.kind = "emergency".into();
        al.alerts.push(e);
        let az = crate::analyzers::Settings {
            rules: vec![crate::analyzers::AnalyzerRule {
                id: "z1".into(),
                name: "ECPR".into(),
                engine: "cloud".into(),
                keywords: vec!["arrest".into()],
                instructions: "screen it".into(),
                fields: vec![Field {
                    key: "ok".into(),
                    kind: "bool".into(),
                    desc: String::new(),
                }],
                conditions: vec![Clause {
                    field: "ok".into(),
                    op: "==".into(),
                    value: "true".into(),
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let cv = crate::conversations::Settings {
            rules: vec![crate::conversations::Rule {
                id: "c1".into(),
                name: "Hospitals".into(),
                tgs: vec![10202],
                fixed_units: vec![900001],
                end_gap_secs: 120,
                chat_id: "-1001:57".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let dg = crate::digest::Settings {
            rules: vec![crate::digest::DigestRule {
                id: "d1".into(),
                name: "Southeast".into(),
                tgs: vec![1, 2],
                interval_secs: 1800,
                window_secs: 900,
                ..Default::default()
            }],
            ..Default::default()
        };
        (al, az, cv, dg)
    }

    #[test]
    fn migration_keeps_ids_meaning_and_destinations() {
        let (al, az, cv, dg) = old_settings();
        let list = migrate(&al, &az, &cv, &dg);
        assert_eq!(list.len(), 5);
        let a = &list[0];
        assert_eq!((a.id.as_str(), a.when.kind.as_str()), ("a1", "call"));
        assert_eq!(a.when.phrases, vec!["cardiac arrest"]);
        assert_eq!(a.when.tgs, vec![20308]);
        assert_eq!(a.check.kind, "ask");
        assert_eq!(a.check.if_unavailable, "hold", "fail_open was off");
        assert_eq!(a.send.dest, "d1", "the named destination, by reference");
        assert_eq!((a.send.quiet_secs, a.send.earlier_calls), (600, 2));
        assert_eq!(a.send.follow, "off", "no new behaviour sneaks in");
        let e = &list[1];
        assert!(e.when.emergency && e.when.phrases.is_empty());
        assert_eq!(
            (e.send.dest.as_str(), e.send.chat.as_str()),
            ("custom", "555")
        );
        let z = &list[2];
        assert_eq!(
            (
                z.id.as_str(),
                z.check.kind.as_str(),
                z.check.engine.as_str()
            ),
            ("z1", "extract", "cloud")
        );
        assert_eq!(z.when.phrases, vec!["arrest"]);
        assert_eq!(z.check.conditions.len(), 1);
        let c = &list[3];
        assert_eq!(
            (c.when.kind.as_str(), c.check.kind.as_str()),
            ("conversation", "summarize")
        );
        assert_eq!(c.when.conversation.fixed_units, vec![900001]);
        assert_eq!(c.send.dest, "d1");
        // The digest's id "d1" is not a destination id clash — ids are per
        // rule list — but it must stay unique among tripwires.
        let d = &list[4];
        assert_eq!(d.id, "d1");
        assert_eq!(
            (d.when.digest.every_mins, d.when.digest.window_mins),
            (30, 15)
        );
        for mut t in list {
            sanitize(&mut t).unwrap();
        }
    }

    #[test]
    fn compiling_gives_the_engines_back_their_rules() {
        let (al, az, cv, dg) = old_settings();
        let list = migrate(&al, &az, &cv, &dg);
        let (convs, digests) = compile_rules(&list, &al);
        assert_eq!(convs.len(), 1);
        let mut expect = cv.rules[0].clone();
        expect.chat_id = "-1001:57".into();
        assert_eq!(convs[0], expect);
        assert_eq!(digests.len(), 1);
        let (g, o) = (&digests[0], &dg.rules[0]);
        assert_eq!(
            (
                &g.id,
                &g.tgs,
                g.interval_secs,
                g.window_secs,
                &g.prompt,
                &g.message
            ),
            (
                &o.id,
                &o.tgs,
                o.interval_secs,
                o.window_secs,
                &o.prompt,
                &o.message
            )
        );
        assert_eq!(g.chat_id, "", "no chat set = the default");
    }

    #[test]
    fn destinations_resolve_by_reference() {
        let (al, ..) = old_settings();
        let mut s = Send::default();
        assert!(resolve_dest(&al, &s).is_err(), "no default set");
        s.dest = "d1".into();
        assert_eq!(resolve_dest(&al, &s).unwrap(), "-1001:57");
        s.dest = "gone".into();
        assert!(resolve_dest(&al, &s).is_err());
        s.dest = "custom".into();
        s.chat = "123".into();
        assert_eq!(resolve_dest(&al, &s).unwrap(), "123");
    }

    #[test]
    fn removed_destinations_keep_sending_and_named_chats_link_up() {
        let d1 = crate::connections::Destination {
            id: "d1".into(),
            name: "Team".into(),
            chat_id: "-100".into(),
            topic_id: "5".into(),
        };
        let mut a = Tripwire::default();
        a.send.dest = "d1".into();
        let mut b = Tripwire::default();
        b.send.dest = "custom".into();
        b.send.chat = "777".into();
        let mut list = vec![a, b];
        assert!(relink(&mut list, &[&d1], &[]));
        assert_eq!((list[0].send.dest.as_str(), list[0].send.chat.as_str()), ("custom", "-100:5"));
        let named = crate::connections::Destination {
            id: "d2".into(),
            name: "Me".into(),
            chat_id: "777".into(),
            topic_id: String::new(),
        };
        assert!(relink(&mut list, &[], &[named]));
        assert_eq!((list[1].send.dest.as_str(), list[1].send.chat.as_str()), ("d2", ""));
        assert!(!relink(&mut list, &[], &[]));
    }

    #[test]
    fn sanitize_whitelists_and_refuses_bad_fields() {
        let mut t = words(&["a", "A", " "], &[3, 1, 3]);
        t.check.kind = "shell".into();
        t.send.follow = "everything".into();
        t.send.chat = "123; rm".into();
        assert!(sanitize(&mut t).is_err());
        t.send.chat = "-100:5".into();
        sanitize(&mut t).unwrap();
        assert_eq!(t.when.phrases, vec!["a"]);
        assert_eq!(t.when.tgs, vec![1, 3]);
        assert_eq!(t.check.kind, "none");
        assert_eq!(t.send.follow, "off");
        let mut c = Tripwire::default();
        c.when.kind = "digest".into();
        sanitize(&mut c).unwrap();
        assert_eq!(c.check.kind, "summarize");
        let mut bad = Tripwire::default();
        bad.check.kind = "extract".into();
        bad.check.conditions = vec![Clause {
            field: "x".into(),
            op: "exec".into(),
            value: String::new(),
        }];
        assert!(sanitize(&mut bad).is_err());
    }

    #[test]
    fn recipes_are_valid_tripwires() {
        for r in recipes() {
            let mut t = r.tripwire.clone();
            sanitize(&mut t).unwrap();
            assert!(!t.name.is_empty());
            assert!(t.when.tgs.is_empty(), "recipes carry no talkgroups");
        }
    }

    #[test]
    fn shared_files_arrive_disabled_and_sending_nowhere() {
        let mut t = words(&["cpr"], &[1]);
        t.send.dest = "d1".into();
        t.send.chat = "-100:5".into();
        let text = serde_json::to_string(&make_bundle(vec![t.clone()], "N", "A", "D")).unwrap();
        assert!(!text.contains("-100:5"));
        let mut v: serde_json::Value = serde_json::from_str(&text).unwrap();
        v["tripwires"][0]["enabled"] = true.into();
        v["tripwires"][0]["send"]["chat"] = "666".into();
        v["tripwires"][0]["id"] = "t1".into();
        let b = parse_bundle(&v.to_string()).unwrap();
        let got = &b.tripwires[0];
        assert!(!got.enabled && got.send.chat.is_empty() && got.send.dest.is_empty());
        assert_ne!(got.id, "t1");
        assert_eq!(got.when.phrases, vec!["cpr"]);
        // An old analyzer template still imports.
        let old = crate::analyzers::make_template(
            crate::analyzers::builtin_templates(),
            "Screens",
            "",
            "",
        );
        let b = parse_bundle(&serde_json::to_string(&old).unwrap()).unwrap();
        assert_eq!(b.tripwires.len(), old.rules.len());
        assert!(b
            .tripwires
            .iter()
            .all(|t| t.check.kind == "extract" && !t.enabled));
        assert!(parse_bundle("{}").is_err());
        assert!(parse_bundle(&format!(
            "{{\"format\":\"{FORMAT}\",\"version\":9,\"tripwires\":[]}}"
        ))
        .is_err());
    }

    #[test]
    fn follow_up_text_names_who_and_where() {
        let mut f = facts("on scene, starting compressions");
        f.start = 0;
        let s = follow_text(&f);
        assert!(s.starts_with("↳ Medic 3 · Medic 3 · "));
        assert!(s.ends_with("on scene, starting compressions"));
    }
}

/// `HS_MIGRATE_DIR=<config dir> cargo test migrate_a_real_config -- --ignored
/// --nocapture` prints what the first start would build from that
/// directory's rule files (read-only).
#[cfg(test)]
mod real_config {
    #[test]
    #[ignore]
    fn migrate_a_real_config() {
        let Ok(dir) = std::env::var("HS_MIGRATE_DIR") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        fn read<T: serde::de::DeserializeOwned + Default>(p: std::path::PathBuf) -> T {
            std::fs::read_to_string(&p)
                .ok()
                .map(|t| {
                    serde_json::from_str(&t).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
                })
                .unwrap_or_default()
        }
        let al: crate::alerts::Settings = read(dir.join("alerts.json"));
        let az: crate::analyzers::Settings = read(dir.join("analyzers.json"));
        let cv: crate::conversations::Settings = read(dir.join("conversations.json"));
        let dg: crate::digest::Settings = read(dir.join("digests.json"));
        let mut list = super::migrate(&al, &az, &cv, &dg);
        for t in &mut list {
            super::sanitize(t).unwrap_or_else(|e| panic!("{}: {e}", t.name));
        }
        println!("{}", serde_json::to_string_pretty(&list).unwrap());
        let (convs, digests) = super::compile_rules(&list, &al);
        for (a, b) in convs.iter().zip(&cv.rules) {
            if a != b {
                println!("CONVERSATION DIFFERS {}:\n  old {b:?}\n  new {a:?}", a.id);
            }
        }
        for (a, b) in digests.iter().zip(&dg.rules) {
            if a != b {
                println!("DIGEST DIFFERS {}:\n  old {b:?}\n  new {a:?}", a.id);
            }
        }
        println!(
            "{} alerts + {} analyzers + {} conversations + {} digests → {} tripwires",
            al.alerts.len(),
            az.rules.len(),
            cv.rules.len(),
            dg.rules.len(),
            list.len()
        );
    }
}
