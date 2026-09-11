//! Alerts: tell the listener — on Telegram, with the audio — when something
//! they care about is heard.
//!
//! An alert is a trigger, an optional AI gate, and actions. Triggers:
//! **keywords** in a call's transcript (on chosen talkgroups), an
//! **emergency** flag, any call on a **talkgroup**, or a **radio** keying up.
//! Keyword alerts fire when the transcript lands, so they trail the call by
//! however long whisper takes. A per-alert cooldown (per talkgroup) stops one
//! incident from becoming a message storm.
//!
//! The AI gate sends the transcript and the alert's own prompt to a local
//! Ollama model and asks for a JSON verdict; if the model says no, nothing
//! is sent. If Ollama is unreachable the alert **fails open** by default —
//! a missed cardiac-arrest page is worse than a spurious one — and says so
//! in the message. That is configurable.
//!
//! All HTTP happens here in Rust: the webview's CSP blocks it, and the bot
//! token belongs in the keyring, not in the page.
//!
//! Rules now live in `tripwires`; this module keeps what they share — the
//! Telegram bot, destinations, the Ollama settings, and the send and AI
//! helpers. The `alerts` array in `alerts.json` is kept as it was for an
//! older build to find (tripwires were built from it once); it is no longer
//! run.

use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use tauri::{AppHandle, Manager, State};

use crate::AppState;

const TOKEN_USER: &str = "bot-token";

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Trigger {
    /// keywords | emergency | talkgroup | unit
    pub kind: String,
    /// Phrases, any of which matches (case-insensitive, whole words).
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Talkgroups the alert watches; empty = any.
    #[serde(default)]
    pub tgs: Vec<u16>,
    /// Radio IDs (for `unit`, or to narrow the others); empty = any.
    #[serde(default)]
    pub units: Vec<u32>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Alert {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub trigger: Trigger,
    /// Message template; tokens `{alert} {tg} {tgname} {unit} {unitname}
    /// {time} {secs} {transcript} {keywords} {ai}`.
    pub message: String,
    /// Seconds before the same alert may fire again for the same talkgroup.
    pub cooldown_secs: u32,
    pub telegram: bool,
    /// Telegram chat this alert goes to; blank = the alerts' default chat.
    #[serde(default)]
    pub chat_id: String,
    /// Forum topic (Telegram's `message_thread_id`) inside that chat; blank
    /// = the default topic, or the chat itself when it has no topics.
    #[serde(default)]
    pub topic_id: String,
    pub tone: bool,
    pub attach_audio: bool,
    /// Also attach this many earlier calls on the same talkgroup…
    pub combine_prev: u32,
    /// …heard within this many seconds before the triggering call.
    pub combine_window_secs: u32,
    /// AI gate: when set, the model decides whether to send.
    pub ai_gate: bool,
    pub ai_prompt: String,
    /// Let a thinking model reason before answering (Ollama `think: true`).
    /// Slower, but a model such as qwen3 judges borderline transcripts
    /// better with the room. Ignored by models without a thinking mode.
    #[serde(default)]
    pub ai_think: bool,
}

impl Default for Alert {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            enabled: true,
            trigger: Trigger {
                kind: "keywords".into(),
                ..Default::default()
            },
            message: "🚨 {alert}\n{tgname} (TG {tg}) · {unitname} · {time}\n{transcript}".into(),
            cooldown_secs: 300,
            telegram: true,
            chat_id: String::new(),
            topic_id: String::new(),
            tone: true,
            attach_audio: true,
            combine_prev: 0,
            combine_window_secs: 120,
            ai_gate: false,
            ai_prompt: String::new(),
            ai_think: false,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Telegram {
    pub chat_id: String,
    /// Default forum topic (`message_thread_id`) for the chat; blank = none.
    #[serde(default)]
    pub topic_id: String,
    /// Say in Telegram when the app starts and when it shuts down.
    #[serde(default)]
    pub announce: bool,
    /// Where those messages go (`chat` or `chat:topic`); blank = the
    /// default chat and topic above.
    #[serde(default)]
    pub announce_chat: String,
}

impl Telegram {
    /// The destination as one string: `chat` or `chat:topic`, the form every
    /// send helper accepts (so rules that keep a single chat field can name
    /// a topic too).
    pub fn destination(&self) -> String {
        join_destination(&self.chat_id, &self.topic_id)
    }
}

/// `chat` + optional topic → `chat:topic`.
pub fn join_destination(chat: &str, topic: &str) -> String {
    let (chat, topic) = (chat.trim(), topic.trim());
    if topic.is_empty() {
        chat.to_string()
    } else {
        format!("{chat}:{topic}")
    }
}

/// Split a destination into the chat id and, when it carries one, the
/// forum topic: `-1001234:57` → (`-1001234`, Some(57)). A chat id never
/// contains a colon (`-100…`, `123…` or `@name`), so the last `:` followed
/// by digits is the topic.
pub fn chat_parts(dest: &str) -> (String, Option<i64>) {
    let d = dest.trim();
    if let Some((chat, topic)) = d.rsplit_once(':') {
        if !chat.is_empty() && !topic.is_empty() && topic.chars().all(|c| c.is_ascii_digit()) {
            return (chat.to_string(), topic.parse().ok());
        }
    }
    (d.to_string(), None)
}

/// The JSON body every text send starts from: chat, and the topic when set.
fn text_body(dest: &str, text: &str) -> serde_json::Value {
    let (chat, topic) = chat_parts(dest);
    let mut body = serde_json::json!({ "chat_id": chat, "text": text });
    if let Some(t) = topic {
        body["message_thread_id"] = serde_json::Value::from(t);
    }
    body
}

/// A multipart send starts from the chat and, when set, the topic.
fn multipart_for(dest: &str) -> crate::upload::Multipart {
    let (chat, topic) = chat_parts(dest);
    let mut m = crate::upload::Multipart::new().text("chat_id", &chat);
    if let Some(t) = topic {
        m = m.text("message_thread_id", &t.to_string());
    }
    m
}

/// The alerts' Telegram chat and Ollama settings, for the conversation
/// summaries to share.
pub fn shared_settings(state: &AppState) -> (Telegram, Ollama) {
    let st = state.alerts.lock().unwrap();
    (st.settings.telegram.clone(), st.settings.ollama.clone())
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Ollama {
    pub url: String,
    pub model: String,
    pub timeout_secs: u32,
    /// Send anyway when the model cannot be reached.
    pub fail_open: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Settings {
    pub alerts: Vec<Alert>,
    pub telegram: Telegram,
    pub ollama: Ollama,
    /// Named places to send (a chat, or one topic in a forum chat), set up
    /// on the Connections page and picked by name in every rule.
    #[serde(default)]
    pub destinations: Vec<crate::connections::Destination>,
    /// Chats (and forum topics) the bot has been seen in, remembered so the
    /// pickers can name them after Telegram's one-day update window.
    #[serde(default)]
    pub known_chats: Vec<crate::connections::KnownChat>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            alerts: Vec::new(),
            telegram: Telegram {
                chat_id: String::new(),
                topic_id: String::new(),
                announce: false,
                announce_chat: String::new(),
            },
            ollama: Ollama {
                url: "http://localhost:11434".into(),
                model: String::new(),
                timeout_secs: 60,
                fail_open: true,
            },
            destinations: Vec::new(),
            known_chats: Vec::new(),
        }
    }
}

#[derive(Default)]
pub struct AlertState {
    pub settings: Settings,
}

pub type Shared = Mutex<AlertState>;

/// What an alert is judged against.
#[derive(Clone, Debug, Default)]
pub struct CallFacts {
    pub id: Option<i64>,
    /// Epoch seconds the call ended (≈ when it was reported).
    pub start: i64,
    pub tg: u16,
    pub tg_name: String,
    /// RadioReference "Description" — longer human label next to the alias,
    /// e.g. "IU Methodist" where the alpha tag is "49M-03". Surfaced as the
    /// `{tgdesc}` token in alert templates.
    pub tg_desc: Option<String>,
    pub unit: u32,
    pub unit_name: Option<String>,
    pub secs: f64,
    pub emergency: bool,
    pub audio: Option<String>,
    pub transcript: Option<String>,
    /// The system the call was heard on (its RadioReference name), so a
    /// rule can tell TG 10202 on one system from TG 10202 on another.
    pub system: String,
}

/// The RadioReference description of `tg` on the named system.
pub fn tg_desc_for(app: &AppHandle, system: &str, tg: u16) -> Option<String> {
    let state = app.state::<AppState>();
    let sid = crate::playlists::sids_by_system_name(app)
        .get(system)
        .copied();
    let desc = state
        .catalog
        .lock()
        .unwrap()
        .get(sid, tg)
        .and_then(|t| t.description.clone());
    desc.filter(|d| !d.trim().is_empty())
}

/// What a rule is judged against, from a stored call. `transcript` wins
/// over the row's own (it is the corrected text that just landed); without
/// one the listener's edit, then the machine text, is used.
pub fn facts_from_row(
    app: &AppHandle,
    r: crate::library::CallRow,
    transcript: Option<String>,
) -> CallFacts {
    let transcript = transcript.or(r.transcript_edited).or(r.transcript);
    CallFacts {
        id: Some(r.id),
        start: r.start,
        tg_desc: tg_desc_for(app, &r.system, r.tg),
        tg: r.tg,
        tg_name: r.tg_name,
        unit: r.unit,
        unit_name: r.unit_name,
        secs: r.secs,
        emergency: r.emergency,
        audio: r.audio,
        transcript,
        system: r.system,
    }
}

fn path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let d = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("config dir: {e}"))?;
    std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    Ok(d.join("alerts.json"))
}

pub fn load(app: &AppHandle) -> AlertState {
    // Bluesky posting is gone; its app password has no business lingering.
    if crate::secrets::get("bluesky-password").is_some() {
        let _ = crate::secrets::remove("bluesky-password");
    }
    AlertState {
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

fn token() -> Option<String> {
    crate::secrets::get(TOKEN_USER)
}

// ---------------------------------------------------------------------------
// matching
// ---------------------------------------------------------------------------

/// Which of `keywords` occur in `text` as whole words/phrases, ignoring case.
pub fn matched_keywords(keywords: &[String], text: &str) -> Vec<String> {
    let hay = format!(" {} ", normalize(text));
    keywords
        .iter()
        .filter(|k| {
            let n = normalize(k);
            !n.is_empty() && hay.contains(&format!(" {n} "))
        })
        .cloned()
        .collect()
}

/// Lower-case, punctuation → spaces, single-spaced.
fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
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

// ---------------------------------------------------------------------------
// Ollama
// ---------------------------------------------------------------------------

/// Ask the model whether to send. Returns (fire, summary).
pub fn ask_ollama(
    o: &Ollama,
    prompt: &str,
    f: &CallFacts,
    think: bool,
) -> Result<(bool, String), String> {
    if o.model.trim().is_empty() {
        return Err("no Ollama model chosen".into());
    }
    let full = format!(
        "You screen radio-scanner transcripts for alerts. The transcript below is machine-generated from a public-safety radio call and may contain recognition errors.\n\n\
         Talkgroup: {} (TG {})\nRadio: {}\nTranscript: \"{}\"\n\n\
         Instruction from the listener: {}\n\n\
         Answer with JSON only: {{\"fire\": true or false, \"summary\": \"one sentence\"}}.",
        f.tg_name,
        f.tg,
        f.unit_name.clone().unwrap_or_else(|| f.unit.to_string()),
        f.transcript.as_deref().unwrap_or(""),
        prompt.trim()
    );
    // The `think` parameter matters either way: left unset, a thinking model
    // (qwen3 and friends) in JSON mode spends its output on the thought and
    // returns an empty response — measured locally — so it is always sent
    // explicitly: `false` by default, `true` when the alert asks for
    // reasoning, in which case Ollama returns the thought in `thinking` and
    // the answer in `response`. A model that rejects the parameter is
    // retried without it.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(
            o.timeout_secs.max(5) as u64
        )))
        .http_status_as_error(false)
        .build()
        .into();
    let call = |send_think: bool| -> Result<(u16, String), String> {
        let mut body = serde_json::json!({
            "model": o.model, "prompt": full, "stream": false, "format": "json",
            "options": { "temperature": 0 }
        });
        if send_think {
            body["think"] = serde_json::Value::Bool(think);
        }
        let mut r = agent
            .post(&format!("{}/api/generate", o.url.trim_end_matches('/')))
            .header("Content-Type", "application/json")
            .send(body.to_string().as_bytes())
            .map_err(|e| format!("ollama: {e}"))?;
        let status = r.status().as_u16();
        let text = r.body_mut().read_to_string().unwrap_or_default();
        Ok((status, text))
    };
    let (mut status, mut text) = call(true)?;
    if status != 200 && text.to_ascii_lowercase().contains("think") {
        (status, text) = call(false)?;
    }
    if status != 200 {
        return Err(format!(
            "ollama HTTP {status}: {}",
            text.chars().take(200).collect::<String>()
        ));
    }
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("ollama reply: {e}"))?;
    let answer = v["response"].as_str().unwrap_or("");
    parse_verdict(answer).ok_or_else(|| {
        format!(
            "model did not answer in JSON: {}",
            answer.chars().take(200).collect::<String>()
        )
    })
}

/// `{"fire": bool, "summary": str}`, tolerating text around the JSON.
pub fn parse_verdict(answer: &str) -> Option<(bool, String)> {
    let start = answer.find('{')?;
    let end = answer.rfind('}')?;
    let v: serde_json::Value = serde_json::from_str(&answer[start..=end]).ok()?;
    let fire = match &v["fire"] {
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::String(s) => {
            s.eq_ignore_ascii_case("true") || s.eq_ignore_ascii_case("yes")
        }
        _ => return None,
    };
    let summary = v["summary"].as_str().unwrap_or("").to_string();
    Some((fire, summary))
}

// ---------------------------------------------------------------------------
// Telegram
// ---------------------------------------------------------------------------

pub(crate) fn telegram_api(method: &str) -> Result<String, String> {
    let t = token().ok_or("no Telegram bot token saved")?;
    Ok(format!("https://api.telegram.org/bot{t}/{method}"))
}

fn check(status: u16, text: &str) -> Result<String, String> {
    if (200..300).contains(&status) {
        Ok("sent".into())
    } else {
        let v: serde_json::Value = serde_json::from_str(text).unwrap_or_default();
        Err(format!(
            "Telegram HTTP {status}: {}",
            v["description"].as_str().unwrap_or(text.trim())
        ))
    }
}

/// A plain text message to `dest` (`chat` or `chat:topic`), giving up after
/// `timeout_secs`.
pub fn send_text(dest: &str, text: &str, timeout_secs: u64) -> Result<String, String> {
    let body = text_body(dest, text);
    let (status, out) = crate::upload::post_timeout(
        &telegram_api("sendMessage")?,
        "application/json",
        body.to_string().into_bytes(),
        timeout_secs,
    )?;
    check(status, &out)
}

/// Concatenate audio files (half a second of silence between) into one
/// clip: MP3 via ffmpeg when available (Telegram's `sendAudio`), else WAV.
/// Returns the path and whether it is MP3.
pub(crate) fn combine_clips(
    files: &[String],
    stem: &str,
) -> Result<(std::path::PathBuf, bool), String> {
    let mut pcm: Vec<i16> = Vec::new();
    for (i, p) in files.iter().enumerate() {
        let part = match read_audio(p) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("clip {p}: {e}");
                continue;
            }
        };
        if i > 0 {
            pcm.extend(std::iter::repeat_n(0i16, 4000));
        }
        pcm.extend(part);
    }
    if pcm.is_empty() {
        return Err("no audio to combine".into());
    }
    let dir = std::env::temp_dir().join("hoosier-alerts");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let wav = dir.join(format!("{stem}_{}.wav", crate::library::now()));
    hs_core::wav::write_wav(wav.to_str().ok_or("temp path")?, 8000, &pcm)
        .map_err(|e| e.to_string())?;
    if crate::encode::ffmpeg_available().is_some() {
        let fmt: crate::encode::Format =
            serde_json::from_str(r#"{"codec":"mp3","bitrate_kbps":48,"mode":"cbr"}"#).unwrap();
        if let Ok(mp3) = crate::encode::transcode(&wav, &fmt) {
            let _ = std::fs::remove_file(&wav);
            return Ok((mp3, true));
        }
    }
    Ok((wav, false))
}

/// Message id from a Telegram reply body.
fn message_id(text: &str) -> Option<i64> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    v["result"]["message_id"].as_i64()
}

/// `reply_parameters` for a reply to `id` that still sends if the message
/// it answers was deleted.
fn reply_json(id: i64) -> serde_json::Value {
    serde_json::json!({ "message_id": id, "allow_sending_without_reply": true })
}

/// Send text; returns the Telegram message id (for later deletion).
pub(crate) fn send_text_id(chat_id: &str, text: &str) -> Result<i64, String> {
    send_text_reply(chat_id, text, None)
}

/// Send text, as a reply to `reply_to` when set (in a forum the topic comes
/// from the destination, as for any send). Returns the message id.
pub(crate) fn send_text_reply(
    chat_id: &str,
    text: &str,
    reply_to: Option<i64>,
) -> Result<i64, String> {
    if chat_id.trim().is_empty() {
        return Err("no Telegram chat id".into());
    }
    let mut body = text_body(chat_id, text);
    if let Some(id) = reply_to {
        body["reply_parameters"] = reply_json(id);
    }
    let (status, out) = crate::upload::post(
        &telegram_api("sendMessage")?,
        "application/json",
        body.to_string().into_bytes(),
    )?;
    check(status, &out)?;
    message_id(&out).ok_or("Telegram reply had no message id".into())
}

/// Send an audio file with a caption; returns the message id. Captions are
/// capped at 1024 characters by Telegram, so a longer message is sent as
/// text first and the audio follows with a short caption.
pub(crate) fn send_audio_id(
    chat_id: &str,
    path: &std::path::Path,
    is_mp3: bool,
    caption: &str,
    title: &str,
    performer: &str,
) -> Result<Vec<i64>, String> {
    send_audio_reply(chat_id, path, is_mp3, caption, title, performer, None)
}

/// `send_audio_id`, as a reply to `reply_to` when set.
pub(crate) fn send_audio_reply(
    chat_id: &str,
    path: &std::path::Path,
    is_mp3: bool,
    caption: &str,
    title: &str,
    performer: &str,
    reply_to: Option<i64>,
) -> Result<Vec<i64>, String> {
    if chat_id.trim().is_empty() {
        return Err("no Telegram chat id".into());
    }
    let mut ids = Vec::new();
    let mut cap = caption.to_string();
    if caption.chars().count() > 1000 {
        ids.push(send_text_reply(chat_id, caption, reply_to)?);
        cap = caption
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(200)
            .collect();
    }
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "call.mp3".into());
    let (method, field, mime) = if is_mp3 {
        ("sendAudio", "audio", "audio/mpeg")
    } else {
        ("sendDocument", "document", "audio/wav")
    };
    let mut m = multipart_for(chat_id)
        .text("caption", &cap)
        .file(field, &name, mime, &data);
    if is_mp3 {
        m = m.text("title", title).text("performer", performer);
    }
    if let Some(id) = reply_to {
        // Multipart fields are strings; Telegram parses this one as JSON.
        m = m.text("reply_parameters", &reply_json(id).to_string());
    }
    let (ctype, body) = m.finish();
    let (status, out) = crate::upload::post(&telegram_api(method)?, &ctype, body)?;
    check(status, &out)?;
    ids.push(message_id(&out).ok_or("Telegram reply had no message id")?);
    Ok(ids)
}

/// The audio for a call: the call itself, after up to `earlier` earlier
/// calls on its talkgroup heard within `window_secs`, oldest first, as one
/// clip (MP3 when ffmpeg is there). `None` when the call has no audio.
pub(crate) fn clip_for_call(
    f: &CallFacts,
    earlier: u32,
    window_secs: u32,
    state: &AppState,
) -> Result<Option<(std::path::PathBuf, bool)>, String> {
    let Some(audio) = f.audio.as_ref().filter(|a| !a.is_empty()) else {
        return Ok(None);
    };
    let mut files = Vec::new();
    if earlier > 0 {
        if let (Some(db), Some(id)) = (state.db.lock().unwrap().clone(), f.id) {
            let c = db.lock().unwrap();
            let mut prev = crate::library::previous_on_talkgroup(
                &c,
                id,
                f.tg,
                earlier as usize,
                window_secs as i64,
            )?;
            prev.reverse();
            files.extend(prev);
        }
    }
    files.push(audio.clone());
    combine_clips(&files, &format!("tw_{}", f.tg)).map(Some)
}

/// Delete a message the bot sent (Telegram allows this for 48 hours).
pub(crate) fn delete_message(chat_id: &str, id: i64) -> Result<(), String> {
    let body = serde_json::json!({ "chat_id": chat_parts(chat_id).0, "message_id": id });
    let (status, out) = crate::upload::post(
        &telegram_api("deleteMessage")?,
        "application/json",
        body.to_string().into_bytes(),
    )?;
    check(status, &out).map(|_| ())
}

/// Edit a message the bot sent, in place (Telegram allows this for 48 hours).
pub(crate) fn edit_message(chat_id: &str, id: i64, text: &str) -> Result<(), String> {
    if chat_id.trim().is_empty() {
        return Err("no Telegram chat id".into());
    }
    let body =
        serde_json::json!({ "chat_id": chat_parts(chat_id).0, "message_id": id, "text": text });
    let (status, out) = crate::upload::post(
        &telegram_api("editMessageText")?,
        "application/json",
        body.to_string().into_bytes(),
    )?;
    check(status, &out).map(|_| ())
}

/// Free-text completion from the local model (no JSON mode): the summary
/// path. Same `think: false` handling as the gate.
pub(crate) fn ollama_complete(o: &Ollama, prompt: &str) -> Result<String, String> {
    if o.model.trim().is_empty() {
        return Err("no Ollama model chosen".into());
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(
            o.timeout_secs.max(5) as u64
        )))
        .http_status_as_error(false)
        .build()
        .into();
    let call = |think: bool| -> Result<(u16, String), String> {
        let mut body = serde_json::json!({ "model": o.model, "prompt": prompt, "stream": false, "options": { "temperature": 0.2 } });
        if think {
            body["think"] = serde_json::Value::Bool(false);
        }
        let mut r = agent
            .post(&format!("{}/api/generate", o.url.trim_end_matches('/')))
            .header("Content-Type", "application/json")
            .send(body.to_string().as_bytes())
            .map_err(|e| format!("ollama: {e}"))?;
        let status = r.status().as_u16();
        let text = r.body_mut().read_to_string().unwrap_or_default();
        Ok((status, text))
    };
    let (mut status, mut text) = call(true)?;
    if status != 200 && text.to_ascii_lowercase().contains("think") {
        (status, text) = call(false)?;
    }
    if status != 200 {
        return Err(format!(
            "ollama HTTP {status}: {}",
            text.chars().take(200).collect::<String>()
        ));
    }
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("ollama reply: {e}"))?;
    let out = v["response"].as_str().unwrap_or("").trim().to_string();
    if out.is_empty() {
        return Err("model returned an empty summary".into());
    }
    Ok(out)
}

fn read_audio(path: &str) -> Result<Vec<i16>, String> {
    if path.to_ascii_lowercase().ends_with(".wav") {
        crate::player::read_wav(path)
    } else {
        crate::encode::decode_to_pcm(std::path::Path::new(path))
    }
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct View {
    pub settings: Settings,
    pub has_token: bool,
    pub ffmpeg: bool,
}

#[tauri::command]
pub fn alerts_get(state: State<AppState>) -> View {
    View {
        settings: state.alerts.lock().unwrap().settings.clone(),
        has_token: token().is_some(),
        ffmpeg: crate::encode::ffmpeg_available().is_some(),
    }
}

#[tauri::command]
pub fn alerts_set(
    app: AppHandle,
    state: State<AppState>,
    settings: Settings,
) -> Result<(), String> {
    let mut settings = settings;
    for (i, a) in settings.alerts.iter_mut().enumerate() {
        if a.id.trim().is_empty() {
            a.id = format!("a{}-{i}", crate::library::now());
        }
        if a.name.trim().is_empty() {
            a.name = format!("Alert {}", i + 1);
        }
        a.chat_id = a.chat_id.trim().chars().take(64).collect();
        a.topic_id = a
            .topic_id
            .trim()
            .chars()
            .filter(|c| c.is_ascii_digit())
            .take(16)
            .collect();
    }
    let t = &mut settings.telegram;
    t.announce_chat = t.announce_chat.trim().chars().take(80).collect();
    crate::connections::sanitize_destinations(&mut settings.destinations)?;
    let before = {
        let mut st = state.alerts.lock().unwrap();
        // Discovered chats are the backend's to keep: a page that loaded
        // before the last discovery must not wind them back.
        let mut known = st.settings.known_chats.clone();
        crate::connections::merge_chats(&mut known, std::mem::take(&mut settings.known_chats));
        settings.known_chats = known;
        store(&app, &settings)?;
        std::mem::replace(&mut st.settings, settings).destinations
    };
    // Tripwires name destinations by id; conversation and digest rules hold
    // the chat itself, so they are rebuilt.
    let after = state.alerts.lock().unwrap().settings.destinations.clone();
    crate::tripwires::destinations_changed(&app, &before, &after);
    Ok(())
}

/// Fold freshly discovered chats into the saved list and persist it.
pub fn update_known_chats(
    app: &AppHandle,
    fresh: Vec<crate::connections::KnownChat>,
) -> Result<Vec<crate::connections::KnownChat>, String> {
    let state = app.state::<AppState>();
    let mut st = state.alerts.lock().unwrap();
    crate::connections::merge_chats(&mut st.settings.known_chats, fresh);
    store(app, &st.settings)?;
    Ok(st.settings.known_chats.clone())
}

#[tauri::command]
pub fn telegram_save(token: String) -> Result<(), String> {
    if token.trim().is_empty() {
        crate::secrets::remove(TOKEN_USER)
    } else {
        crate::secrets::set(TOKEN_USER, token.trim())
    }
}

/// The models the local Ollama offers.
#[tauri::command]
pub async fn ollama_models(url: String) -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(5)))
            .build()
            .into();
        let mut r = agent
            .get(&format!("{}/api/tags", url.trim_end_matches('/')))
            .call()
            .map_err(|e| format!("ollama: {e}"))?;
        let text = r.body_mut().read_to_string().map_err(|e| e.to_string())?;
        let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        Ok(v["models"]
            .as_array()
            .map(|m| {
                m.iter()
                    .filter_map(|x| x["name"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// What a model can do, from Ollama's `/api/show` — notably whether it has
/// a `thinking` mode, so the UI offers reasoning only where it exists.
#[tauri::command]
pub async fn ollama_capabilities(url: String, model: String) -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        if model.trim().is_empty() {
            return Ok(Vec::new());
        }
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(5)))
            .http_status_as_error(false)
            .build()
            .into();
        let body = serde_json::json!({ "model": model.trim() });
        let mut r = agent
            .post(&format!("{}/api/show", url.trim_end_matches('/')))
            .header("Content-Type", "application/json")
            .send(body.to_string().as_bytes())
            .map_err(|e| format!("ollama: {e}"))?;
        let status = r.status().as_u16();
        let text = r.body_mut().read_to_string().map_err(|e| e.to_string())?;
        if status != 200 {
            return Err(format!(
                "ollama HTTP {status}: {}",
                text.chars().take(120).collect::<String>()
            ));
        }
        let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        Ok(v["capabilities"]
            .as_array()
            .map(|c| {
                c.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords_match_whole_phrases_case_insensitively() {
        let kws = vec![
            "Ventricular tachycardia".to_string(),
            "CPR".into(),
            "working arrest".into(),
        ];
        assert_eq!(
            matched_keywords(&kws, "Patient in ventricular tachycardia, starting CPR."),
            vec!["Ventricular tachycardia", "CPR"]
        );
        // "cpr" inside another word does not count; punctuation does not
        // break a phrase.
        assert!(matched_keywords(&kws, "the scprinter").is_empty());
        assert_eq!(
            matched_keywords(&kws, "it's a working-arrest"),
            vec!["working arrest"]
        );
    }

    #[test]
    fn verdicts_parse_with_noise_around_the_json() {
        assert_eq!(
            parse_verdict(
                "Sure. {\"fire\": true, \"summary\": \"Working arrest with CPR in progress.\"}"
            ),
            Some((true, "Working arrest with CPR in progress.".into()))
        );
        assert_eq!(
            parse_verdict("{\"fire\":\"no\"}"),
            Some((false, String::new()))
        );
        assert_eq!(parse_verdict("I cannot tell."), None);
    }
}

#[cfg(test)]
mod destination_tests {
    use super::*;

    #[test]
    fn destinations_split_into_chat_and_topic() {
        assert_eq!(
            chat_parts("-1001234567890:57"),
            ("-1001234567890".into(), Some(57))
        );
        assert_eq!(
            chat_parts("-1001234567890"),
            ("-1001234567890".into(), None)
        );
        assert_eq!(chat_parts("@mychannel"), ("@mychannel".into(), None));
        assert_eq!(chat_parts(" 123456789:7 "), ("123456789".into(), Some(7)));
        // Not a topic: nothing before the colon, or non-digits after it.
        assert_eq!(chat_parts(":7"), (":7".into(), None));
        assert_eq!(chat_parts("-100:abc"), ("-100:abc".into(), None));
        assert_eq!(join_destination("-100", "12"), "-100:12");
        assert_eq!(join_destination("-100", " "), "-100");
        let body = text_body("-100:12", "hi");
        assert_eq!(body["chat_id"], "-100");
        assert_eq!(body["message_thread_id"], 12);
        assert!(text_body("-100", "hi").get("message_thread_id").is_none());
    }
}
