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

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::AppState;

const SERVICE: &str = "HoosierSDR Telegram";
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
    pub tone: bool,
    pub attach_audio: bool,
    /// Also attach this many earlier calls on the same talkgroup…
    pub combine_prev: u32,
    /// …heard within this many seconds before the triggering call.
    pub combine_window_secs: u32,
    /// AI gate: when set, the model decides whether to send.
    pub ai_gate: bool,
    pub ai_prompt: String,
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
            tone: true,
            attach_audio: true,
            combine_prev: 0,
            combine_window_secs: 120,
            ai_gate: false,
            ai_prompt: String::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Telegram {
    pub chat_id: String,
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
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            alerts: Vec::new(),
            telegram: Telegram {
                chat_id: String::new(),
            },
            ollama: Ollama {
                url: "http://localhost:11434".into(),
                model: String::new(),
                timeout_secs: 60,
                fail_open: true,
            },
        }
    }
}

/// One firing, for the in-app log.
#[derive(Serialize, Clone, Debug)]
pub struct LogEntry {
    pub at: i64,
    pub alert: String,
    pub tg: u16,
    pub tg_name: String,
    pub message: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Default)]
pub struct AlertState {
    pub settings: Settings,
    /// (alert id, talkgroup) → when it last fired (epoch seconds).
    last_fired: HashMap<(String, u16), i64>,
    pub log: VecDeque<LogEntry>,
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
    pub unit: u32,
    pub unit_name: Option<String>,
    pub secs: f64,
    pub emergency: bool,
    pub audio: Option<String>,
    pub transcript: Option<String>,
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
    keyring::Entry::new(SERVICE, TOKEN_USER)
        .ok()?
        .get_password()
        .ok()
        .filter(|t| !t.is_empty())
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

/// Does this alert's trigger apply to the call? Returns the keywords that
/// matched (empty for non-keyword triggers).
pub fn matches(a: &Alert, f: &CallFacts) -> Option<Vec<String>> {
    if !a.enabled {
        return None;
    }
    let t = &a.trigger;
    if !t.tgs.is_empty() && !t.tgs.contains(&f.tg) {
        return None;
    }
    if !t.units.is_empty() && !t.units.contains(&f.unit) {
        return None;
    }
    match t.kind.as_str() {
        "keywords" => {
            let text = f.transcript.as_deref()?;
            let m = matched_keywords(&t.keywords, text);
            (!m.is_empty()).then_some(m)
        }
        "emergency" => f.emergency.then(Vec::new),
        "talkgroup" => (!t.tgs.is_empty()).then(Vec::new),
        "unit" => (!t.units.is_empty()).then(Vec::new),
        _ => None,
    }
}

pub fn render(template: &str, a: &Alert, f: &CallFacts, keywords: &[String], ai: &str) -> String {
    let time = chrono_like(crate::library::now());
    template
        .replace("{alert}", &a.name)
        .replace("{tg}", &f.tg.to_string())
        .replace("{tgname}", &f.tg_name)
        .replace("{unit}", &f.unit.to_string())
        .replace(
            "{unitname}",
            f.unit_name.as_deref().unwrap_or(&f.unit.to_string()),
        )
        .replace("{time}", &time)
        .replace("{secs}", &format!("{:.0}", f.secs))
        .replace("{transcript}", f.transcript.as_deref().unwrap_or(""))
        .replace("{keywords}", &keywords.join(", "))
        .replace("{ai}", ai)
        .trim()
        .to_string()
}

fn chrono_like(epoch: i64) -> String {
    // Local-ish HH:MM:SS without a date crate: use the system's TZ offset via
    // `date` is overkill; show UTC and say so.
    let s = epoch.rem_euclid(86_400);
    format!("{:02}:{:02}:{:02} UTC", s / 3600, (s % 3600) / 60, s % 60)
}

// ---------------------------------------------------------------------------
// firing
// ---------------------------------------------------------------------------

/// A completed call (before its transcript exists).
pub fn on_call(app: &AppHandle, f: &CallFacts) {
    let state = app.state::<AppState>();
    let alerts: Vec<Alert> = state
        .alerts
        .lock()
        .unwrap()
        .settings
        .alerts
        .iter()
        .filter(|a| a.trigger.kind != "keywords")
        .cloned()
        .collect();
    for a in alerts {
        if let Some(kw) = matches(&a, f) {
            fire(app.clone(), a, f.clone(), kw);
        }
    }
}

/// A transcript landed for library call `id`.
pub fn on_transcript(app: &AppHandle, id: i64, text: &str) {
    let state = app.state::<AppState>();
    let alerts: Vec<Alert> = state
        .alerts
        .lock()
        .unwrap()
        .settings
        .alerts
        .iter()
        .filter(|a| a.trigger.kind == "keywords" && a.enabled)
        .cloned()
        .collect();
    if alerts.is_empty() {
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
    let f = CallFacts {
        id: Some(r.id),
        start: r.start,
        tg: r.tg,
        tg_name: r.tg_name,
        unit: r.unit,
        unit_name: r.unit_name,
        secs: r.secs,
        emergency: r.emergency,
        audio: r.audio,
        transcript: Some(text.to_string()),
    };
    for a in alerts {
        if let Some(kw) = matches(&a, &f) {
            fire(app.clone(), a, f.clone(), kw);
        }
    }
}

/// Run the alert on its own thread: cooldown, AI gate, actions, log.
fn fire(app: AppHandle, a: Alert, f: CallFacts, keywords: Vec<String>) {
    std::thread::spawn(move || {
        let state = app.state::<AppState>();
        let now = crate::library::now();
        let key = (a.id.clone(), f.tg);
        {
            let st = state.alerts.lock().unwrap();
            if let Some(last) = st.last_fired.get(&key) {
                if now - last < a.cooldown_secs as i64 {
                    return;
                }
            }
        }
        // The cooldown is armed only once something was actually delivered
        // (below): an AI "no" or a failed send must not throttle the retry.
        let (tg_settings, ollama) = {
            let st = state.alerts.lock().unwrap();
            (st.settings.telegram.clone(), st.settings.ollama.clone())
        };
        let mut ai_note = String::new();
        if a.ai_gate && !a.ai_prompt.trim().is_empty() {
            match ask_ollama(&ollama, &a.ai_prompt, &f) {
                Ok((true, summary)) => ai_note = summary,
                Ok((false, summary)) => {
                    log_entry(
                        &app,
                        &a,
                        &f,
                        false,
                        format!("AI gate said no: {summary}"),
                        String::new(),
                    );
                    return;
                }
                Err(e) => {
                    if !ollama.fail_open {
                        log_entry(
                            &app,
                            &a,
                            &f,
                            false,
                            format!("AI gate unavailable, alert held: {e}"),
                            String::new(),
                        );
                        return;
                    }
                    ai_note = format!("(AI gate unavailable: {e})");
                }
            }
        }
        let message = render(&a.message, &a, &f, &keywords, &ai_note);
        let _ = app.emit(
            "alert",
            serde_json::json!({ "name": a.name, "tg": f.tg, "message": message, "tone": a.tone }),
        );
        let mut ok = true;
        let mut detail = String::new();
        if a.telegram {
            match send_telegram(&tg_settings, &a, &f, &message, &state) {
                Ok(d) => detail = d,
                Err(e) => {
                    ok = false;
                    detail = e;
                }
            }
        }
        if ok {
            state.alerts.lock().unwrap().last_fired.insert(key, now);
        }
        log_entry(&app, &a, &f, ok, detail, message);
    });
}

fn log_entry(app: &AppHandle, a: &Alert, f: &CallFacts, ok: bool, detail: String, message: String) {
    let state = app.state::<AppState>();
    let mut st = state.alerts.lock().unwrap();
    st.log.push_front(LogEntry {
        at: crate::library::now(),
        alert: a.name.clone(),
        tg: f.tg,
        tg_name: f.tg_name.clone(),
        message,
        ok,
        detail: detail.clone(),
    });
    st.log.truncate(200);
    if !ok {
        let _ = app.emit("alert_error", format!("{}: {detail}", a.name));
    }
}

// ---------------------------------------------------------------------------
// Ollama
// ---------------------------------------------------------------------------

/// Ask the model whether to send. Returns (fire, summary).
pub fn ask_ollama(o: &Ollama, prompt: &str, f: &CallFacts) -> Result<(bool, String), String> {
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
    // `think: false` matters: a thinking model (qwen3 and friends) in JSON
    // mode otherwise spends its output on the thought and returns an empty
    // response — measured locally. A model that rejects the parameter is
    // retried without it.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(
            o.timeout_secs.max(5) as u64
        )))
        .http_status_as_error(false)
        .build()
        .into();
    let call = |think: bool| -> Result<(u16, String), String> {
        let mut body = serde_json::json!({
            "model": o.model, "prompt": full, "stream": false, "format": "json",
            "options": { "temperature": 0 }
        });
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

pub fn send_message(tg: &Telegram, text: &str) -> Result<String, String> {
    if tg.chat_id.trim().is_empty() {
        return Err("no Telegram chat id".into());
    }
    let body = serde_json::json!({ "chat_id": tg.chat_id.trim(), "text": text });
    let (status, out) = crate::upload::post(
        &telegram_api("sendMessage")?,
        "application/json",
        body.to_string().into_bytes(),
    )?;
    check(status, &out)
}

/// The audio to attach: the call, with up to `combine_prev` earlier calls on
/// the same talkgroup within the window, concatenated oldest first. Encoded
/// to MP3 when ffmpeg is present (what Telegram's `sendAudio` wants), else
/// WAV as a document.
fn clip_for(
    a: &Alert,
    f: &CallFacts,
    state: &AppState,
) -> Result<Option<(std::path::PathBuf, bool)>, String> {
    let Some(audio) = f.audio.as_ref() else {
        return Ok(None);
    };
    let mut files = vec![audio.clone()];
    if a.combine_prev > 0 {
        if let (Some(db), Some(id)) = (state.db.lock().unwrap().clone(), f.id) {
            let c = db.lock().unwrap();
            let prev = crate::library::previous_on_talkgroup(
                &c,
                id,
                f.tg,
                a.combine_prev as usize,
                a.combine_window_secs as i64,
            )?;
            for p in prev.into_iter().rev() {
                files.insert(0, p);
            }
        }
    }
    let mut pcm: Vec<i16> = Vec::new();
    for (i, p) in files.iter().enumerate() {
        let part = read_audio(p)?;
        if i > 0 {
            pcm.extend(std::iter::repeat_n(0i16, 4000)); // half a second between calls
        }
        pcm.extend(part);
    }
    let dir = std::env::temp_dir().join("hoosier-alerts");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let wav = dir.join(format!("alert_{}_{}.wav", f.tg, crate::library::now()));
    hs_core::wav::write_wav(wav.to_str().ok_or("temp path")?, 8000, &pcm)
        .map_err(|e| e.to_string())?;
    if crate::encode::ffmpeg_available().is_some() {
        let fmt: crate::encode::Format =
            serde_json::from_str(r#"{"codec":"mp3","bitrate_kbps":48,"mode":"cbr"}"#).unwrap();
        if let Ok(mp3) = crate::encode::transcode(&wav, &fmt) {
            let _ = std::fs::remove_file(&wav);
            return Ok(Some((mp3, true)));
        }
    }
    Ok(Some((wav, false)))
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

/// Send text; returns the Telegram message id (for later deletion).
pub(crate) fn send_text_id(chat_id: &str, text: &str) -> Result<i64, String> {
    if chat_id.trim().is_empty() {
        return Err("no Telegram chat id".into());
    }
    let body = serde_json::json!({ "chat_id": chat_id.trim(), "text": text });
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
    if chat_id.trim().is_empty() {
        return Err("no Telegram chat id".into());
    }
    let mut ids = Vec::new();
    let mut cap = caption.to_string();
    if caption.chars().count() > 1000 {
        ids.push(send_text_id(chat_id, caption)?);
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
    let mut m = crate::upload::Multipart::new()
        .text("chat_id", chat_id.trim())
        .text("caption", &cap)
        .file(field, &name, mime, &data);
    if is_mp3 {
        m = m.text("title", title).text("performer", performer);
    }
    let (ctype, body) = m.finish();
    let (status, out) = crate::upload::post(&telegram_api(method)?, &ctype, body)?;
    check(status, &out)?;
    ids.push(message_id(&out).ok_or("Telegram reply had no message id")?);
    Ok(ids)
}

/// Delete a message the bot sent (Telegram allows this for 48 hours).
pub(crate) fn delete_message(chat_id: &str, id: i64) -> Result<(), String> {
    let body = serde_json::json!({ "chat_id": chat_id.trim(), "message_id": id });
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
    let body = serde_json::json!({ "chat_id": chat_id.trim(), "message_id": id, "text": text });
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

fn send_telegram(
    tg: &Telegram,
    a: &Alert,
    f: &CallFacts,
    message: &str,
    state: &AppState,
) -> Result<String, String> {
    let clip = if a.attach_audio {
        clip_for(a, f, state)?
    } else {
        None
    };
    match clip {
        None => send_message(tg, message),
        Some((path, is_mp3)) => {
            let data = std::fs::read(&path).map_err(|e| e.to_string())?;
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "call.mp3".into());
            let caption: String = message.chars().take(1000).collect();
            let (method, field, mime) = if is_mp3 {
                ("sendAudio", "audio", "audio/mpeg")
            } else {
                ("sendDocument", "document", "audio/wav")
            };
            let mut m = crate::upload::Multipart::new()
                .text("chat_id", tg.chat_id.trim())
                .text("caption", &caption)
                .file(field, &name, mime, &data);
            if is_mp3 {
                m = m.text("title", &a.name).text("performer", &f.tg_name);
            }
            let (ctype, body) = m.finish();
            let (status, out) = crate::upload::post(&telegram_api(method)?, &ctype, body)?;
            let _ = std::fs::remove_file(&path);
            // A long message does not fit a caption; send the rest as text.
            if message.chars().count() > 1000 {
                let _ = send_message(tg, message);
            }
            check(status, &out).map(|_| format!("sent with {}", if is_mp3 { "MP3" } else { "WAV" }))
        }
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
    }
    store(&app, &settings)?;
    state.alerts.lock().unwrap().settings = settings;
    Ok(())
}

#[tauri::command]
pub fn telegram_save(token: String) -> Result<(), String> {
    let e = keyring::Entry::new(SERVICE, TOKEN_USER).map_err(|e| format!("keyring: {e}"))?;
    if token.trim().is_empty() {
        let _ = e.delete_credential();
    } else {
        e.set_password(token.trim())
            .map_err(|e| format!("keyring: {e}"))?;
    }
    Ok(())
}

#[tauri::command]
pub fn alerts_log(state: State<AppState>) -> Vec<LogEntry> {
    state.alerts.lock().unwrap().log.iter().cloned().collect()
}

/// Fire an alert by hand, against the most recent library call on one of
/// its talkgroups (or any call), skipping the cooldown — so the whole path,
/// Telegram and AI gate included, can be seen working.
#[tauri::command]
pub async fn alerts_test(app: AppHandle, id: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let a = state
            .alerts
            .lock()
            .unwrap()
            .settings
            .alerts
            .iter()
            .find(|a| a.id == id)
            .cloned()
            .ok_or("no such alert")?;
        let row = {
            let db = state.db.lock().unwrap().clone().ok_or("library not open")?;
            let c = db.lock().unwrap();
            crate::library::latest_call(&c, &a.trigger.tgs)?
        };
        let f = match row {
            Some(r) => CallFacts {
                id: Some(r.id),
                start: r.start,
                tg: r.tg,
                tg_name: r.tg_name,
                unit: r.unit,
                unit_name: r.unit_name,
                secs: r.secs,
                emergency: r.emergency,
                audio: r.audio,
                transcript: r
                    .transcript_edited
                    .or(r.transcript)
                    .or(Some("(no transcript yet)".into())),
            },
            None => CallFacts {
                id: None,
                start: crate::library::now(),
                tg: a.trigger.tgs.first().copied().unwrap_or(0),
                tg_name: "Test talkgroup".into(),
                unit: 0,
                unit_name: None,
                secs: 0.0,
                emergency: false,
                audio: None,
                transcript: Some("test message — no calls in the library yet".into()),
            },
        };
        let keywords = f
            .transcript
            .as_deref()
            .map(|t| matched_keywords(&a.trigger.keywords, t))
            .unwrap_or_default();
        state
            .alerts
            .lock()
            .unwrap()
            .last_fired
            .remove(&(a.id.clone(), f.tg));
        let mut test = a.clone();
        test.enabled = true;
        fire(app.clone(), test, f.clone(), keywords);
        Ok(format!(
            "firing “{}” against {} (TG {}){}",
            a.name,
            f.tg_name,
            f.tg,
            f.audio
                .as_ref()
                .map(|_| " with audio")
                .unwrap_or(" — no audio on that call")
        ))
    })
    .await
    .map_err(|e| e.to_string())?
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
            emergency: false,
            audio: None,
            transcript: Some(text.into()),
        }
    }

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
    fn triggers_respect_talkgroup_and_kind() {
        let mut a = Alert {
            name: "Arrest".into(),
            ..Default::default()
        };
        a.trigger.keywords = vec!["cardiac arrest".into()];
        a.trigger.tgs = vec![20308];
        assert_eq!(
            matches(&a, &facts("confirmed cardiac arrest")),
            Some(vec!["cardiac arrest".into()])
        );
        let mut other = facts("confirmed cardiac arrest");
        other.tg = 1;
        assert_eq!(matches(&a, &other), None, "other talkgroup");
        a.enabled = false;
        assert_eq!(matches(&a, &facts("cardiac arrest")), None, "disabled");
        let mut e = Alert::default();
        e.trigger.kind = "emergency".into();
        let mut f = facts("");
        assert_eq!(matches(&e, &f), None);
        f.emergency = true;
        assert_eq!(matches(&e, &f), Some(vec![]));
        let mut u = Alert::default();
        u.trigger.kind = "unit".into();
        u.trigger.units = vec![790065];
        assert_eq!(matches(&u, &f), Some(vec![]));
        u.trigger.units = vec![1];
        assert_eq!(matches(&u, &f), None);
    }

    #[test]
    fn message_template_renders_tokens() {
        let a = Alert {
            name: "Arrest".into(),
            ..Default::default()
        };
        let m = render(
            "{alert}: {tgname}/{tg} by {unitname} — {keywords} — {transcript} {ai}",
            &a,
            &facts("starting CPR"),
            &["CPR".into()],
            "likely real",
        );
        assert_eq!(
            m,
            "Arrest: Medic 3/20308 by Medic 3 — CPR — starting CPR likely real"
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
