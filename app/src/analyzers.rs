//! Analyzers: run a listener-authored prompt over a call's transcript, pull
//! out structured fields, decide whether it matters, and send a custom
//! message.
//!
//! Where an alert's AI gate answers one yes/no question (`{"fire", "summary"}`),
//! an analyzer extracts a *named set of fields* — e.g. an ED-ECPR candidacy
//! screen returns `{candidate, likelihoodPct, criteriaMet, reason, …}` — then
//! sends only when a condition over those fields holds. The extracted fields
//! become `{field.<key>}` tokens in the message, so the Telegram page reads
//! like the n8n survey it replaces.
//!
//! Shape of a run, on each transcript:
//!   pre-filter (talkgroups + optional keywords, cheap) → local Ollama JSON
//!   extraction (the expensive step, gated by the pre-filter) → condition over
//!   the fields → render message with `{field.*}` tokens → Telegram (+ audio).
//!
//! It reuses the alerts module's Ollama config and Telegram send helpers, so
//! the bot token stays in the keyring and all HTTP happens in Rust.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::alerts::CallFacts;
use crate::AppState;

/// One field the model must return, and the hint that tells it how.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Field {
    /// JSON key, also the `{field.<key>}` message token.
    pub key: String,
    /// `string` | `number` | `bool` — how the value is parsed for conditions.
    #[serde(default = "default_kind")]
    pub kind: String,
    /// Instruction to the model for this field.
    #[serde(default)]
    pub desc: String,
}

fn default_kind() -> String {
    "string".into()
}

/// One test over an extracted field. `op` is one of
/// `== != > >= < <= contains`. Numeric ops parse both sides as numbers.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Clause {
    pub field: String,
    pub op: String,
    pub value: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AnalyzerRule {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    /// Which model runs the extraction: `ollama` (local, default) or `cloud`
    /// (the shared cloud provider configured below).
    #[serde(default = "default_engine")]
    pub engine: String,
    /// Talkgroups to watch; empty = any.
    #[serde(default)]
    pub tgs: Vec<u16>,
    /// Cheap pre-filter: only run the model when the transcript contains one of
    /// these (case-insensitive, whole words). Empty = run on every transcript
    /// on the chosen talkgroups.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// The extraction instructions (the "system prompt").
    pub instructions: String,
    /// The fields the model must return.
    #[serde(default)]
    pub fields: Vec<Field>,
    /// `all` = every clause must hold to send; `any` = at least one. Empty
    /// clause list always sends (pure extract-and-notify).
    #[serde(default = "default_match")]
    pub match_mode: String,
    #[serde(default)]
    pub conditions: Vec<Clause>,
    /// Message template. Standard call tokens plus `{field.<key>}` for each
    /// extracted field, and `{json}` for the whole object.
    pub message: String,
    /// Telegram chat; blank = the alerts' chat.
    #[serde(default)]
    pub chat_id: String,
    /// Send the message to Telegram.
    #[serde(default = "default_true")]
    pub telegram: bool,
    /// Also post the message to the alerts' Bluesky account.
    #[serde(default)]
    pub bluesky: bool,
    #[serde(default)]
    pub attach_audio: bool,
    /// Seconds before the same analyzer may fire again for the same talkgroup.
    #[serde(default = "default_cooldown")]
    pub cooldown_secs: u32,
}

fn default_match() -> String {
    "all".into()
}
fn default_cooldown() -> u32 {
    120
}
fn default_engine() -> String {
    "ollama".into()
}
fn default_true() -> bool {
    true
}

/// Shared cloud-model settings for analyzers that use `engine = "cloud"`.
/// The API key is not stored here — it lives in the OS secret store under
/// `analyzer-cloud-key`.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Cloud {
    /// `openrouter` | `openai` | `anthropic`.
    pub provider: String,
    /// Model id, e.g. `anthropic/claude-opus-4.1`, `gpt-4o`, `claude-opus-4-8`.
    pub model: String,
    /// Optional base-URL override; blank = the provider default.
    #[serde(default)]
    pub base_url: String,
    pub timeout_secs: u32,
}

impl Default for Cloud {
    fn default() -> Self {
        Self {
            provider: "openrouter".into(),
            model: String::new(),
            base_url: String::new(),
            timeout_secs: 60,
        }
    }
}

impl Default for AnalyzerRule {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            enabled: true,
            engine: "ollama".into(),
            tgs: Vec::new(),
            keywords: Vec::new(),
            instructions: String::new(),
            fields: Vec::new(),
            match_mode: "all".into(),
            conditions: Vec::new(),
            message: "🔎 {name}\n{tgname} (TG {tg}) · {time}\n{transcript}".into(),
            chat_id: String::new(),
            telegram: true,
            bluesky: false,
            attach_audio: false,
            cooldown_secs: 120,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Settings {
    pub rules: Vec<AnalyzerRule>,
    #[serde(default)]
    pub cloud: Cloud,
}

#[derive(Serialize, Clone, Debug)]
pub struct LogEntry {
    pub at: i64,
    pub rule: String,
    pub tg: u16,
    pub tg_name: String,
    /// Did the condition hold (and so a message was attempted)?
    pub matched: bool,
    pub ok: bool,
    pub detail: String,
    /// Pretty-printed extracted JSON, for the log view.
    pub extracted: String,
}

#[derive(Default)]
pub struct AnalyzerState {
    pub settings: Settings,
    pub log: VecDeque<LogEntry>,
    /// (rule id, tg) → epoch seconds last delivered, for the cooldown.
    pub last_fired: HashMap<(String, u16), i64>,
}

pub type Shared = Mutex<AnalyzerState>;

// ---------------------------------------------------------------------------
// persistence
// ---------------------------------------------------------------------------

fn path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let d = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("config dir: {e}"))?;
    std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    Ok(d.join("analyzers.json"))
}

pub fn load(app: &AppHandle) -> AnalyzerState {
    AnalyzerState {
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

// ---------------------------------------------------------------------------
// firing
// ---------------------------------------------------------------------------

/// A transcript landed for library call `id`: run every analyzer it matches.
pub fn on_transcript(app: &AppHandle, id: i64, text: &str) {
    let state = app.state::<AppState>();
    let rules: Vec<AnalyzerRule> = state
        .analyzers
        .lock()
        .unwrap()
        .settings
        .rules
        .iter()
        .filter(|r| r.enabled && !r.instructions.trim().is_empty())
        .cloned()
        .collect();
    if rules.is_empty() {
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
    for rule in rules {
        if pre_filter(&rule, &f) {
            let app = app.clone();
            let f = f.clone();
            std::thread::spawn(move || run(app, rule, f));
        }
    }
}

/// Cheap gate before the model: talkgroup and optional keywords.
fn pre_filter(r: &AnalyzerRule, f: &CallFacts) -> bool {
    if !r.tgs.is_empty() && !r.tgs.contains(&f.tg) {
        return false;
    }
    if r.keywords.is_empty() {
        return true;
    }
    let text = f.transcript.as_deref().unwrap_or("");
    !crate::alerts::matched_keywords(&r.keywords, text).is_empty()
}

/// Extract, test, send. Runs on its own thread.
fn run(app: AppHandle, r: AnalyzerRule, f: CallFacts) {
    let state = app.state::<AppState>();
    let now = crate::library::now();
    let key = (r.id.clone(), f.tg);
    {
        let st = state.analyzers.lock().unwrap();
        if let Some(last) = st.last_fired.get(&key) {
            if now - last < r.cooldown_secs as i64 {
                return;
            }
        }
    }
    let tg_settings = crate::alerts::shared_settings(&state).0;

    let obj = match run_extract(&state, &r, &f) {
        Ok(v) => v,
        Err(e) => {
            log_it(&app, &r, &f, false, false, format!("extraction failed: {e}"), String::new());
            return;
        }
    };
    let extracted = serde_json::to_string_pretty(&obj).unwrap_or_default();
    let matched = evaluate(&r, &obj);
    if !matched {
        log_it(&app, &r, &f, false, true, "condition not met".into(), extracted);
        return;
    }

    let message = render(&r.message, &r, &f, &obj);
    let _ = app.emit(
        "analyzer",
        serde_json::json!({ "name": r.name, "tg": f.tg, "message": message }),
    );
    let chat = if r.chat_id.trim().is_empty() {
        tg_settings.chat_id.clone()
    } else {
        r.chat_id.clone()
    };
    let (ok, detail) = deliver(&state, &chat, &r, &f, &message);
    if ok {
        state.analyzers.lock().unwrap().last_fired.insert(key, now);
    }
    log_it(&app, &r, &f, true, ok, detail, extracted);
}

/// Pick the extraction engine (local Ollama or the shared cloud model) and run it.
fn run_extract(
    state: &AppState,
    r: &AnalyzerRule,
    f: &CallFacts,
) -> Result<serde_json::Value, String> {
    if r.engine == "cloud" {
        let cloud = state.analyzers.lock().unwrap().settings.cloud.clone();
        let key = crate::secrets::get("analyzer-cloud-key").unwrap_or_default();
        cloud_extract(&cloud, &key, r, f)
    } else {
        let ollama = crate::alerts::shared_settings(state).1;
        ollama_extract(&ollama, r, f)
    }
}

/// Send the rendered message to the analyzer's chosen destinations. Returns
/// (all-ok, joined detail).
fn deliver(
    state: &AppState,
    chat: &str,
    r: &AnalyzerRule,
    f: &CallFacts,
    message: &str,
) -> (bool, String) {
    let mut ok = true;
    let mut parts: Vec<String> = Vec::new();
    if r.telegram {
        match send_telegram(chat, r, f, message) {
            Ok(d) => parts.push(d),
            Err(e) => {
                ok = false;
                parts.push(format!("telegram: {e}"));
            }
        }
    }
    if r.bluesky {
        match crate::alerts::bluesky_post(state, message) {
            Ok(_) => parts.push("bluesky ok".into()),
            Err(e) => {
                ok = false;
                parts.push(format!("bluesky: {e}"));
            }
        }
    }
    if parts.is_empty() {
        parts.push("no destination selected".into());
    }
    (ok, parts.join("; "))
}

fn send_telegram(chat: &str, r: &AnalyzerRule, f: &CallFacts, message: &str) -> Result<String, String> {
    let clip = if r.attach_audio {
        f.audio.as_deref().filter(|p| !p.is_empty())
    } else {
        None
    };
    match clip {
        None => crate::alerts::send_text_id(chat, message).map(|_| "sent".into()),
        Some(path) => {
            let p = std::path::Path::new(path);
            let is_mp3 = path.to_ascii_lowercase().ends_with(".mp3");
            crate::alerts::send_audio_id(chat, p, is_mp3, message, &r.name, &f.tg_name)
                .map(|_| format!("sent with {}", if is_mp3 { "MP3" } else { "audio" }))
        }
    }
}

fn log_it(
    app: &AppHandle,
    r: &AnalyzerRule,
    f: &CallFacts,
    matched: bool,
    ok: bool,
    detail: String,
    extracted: String,
) {
    let state = app.state::<AppState>();
    let mut st = state.analyzers.lock().unwrap();
    st.log.push_front(LogEntry {
        at: crate::library::now(),
        rule: r.name.clone(),
        tg: f.tg,
        tg_name: f.tg_name.clone(),
        matched,
        ok,
        detail: detail.clone(),
        extracted,
    });
    st.log.truncate(200);
    if matched && !ok {
        let _ = app.emit("alert_error", format!("{}: {detail}", r.name));
    }
    let _ = app.emit("analyzers", ());
}

// ---------------------------------------------------------------------------
// extraction
// ---------------------------------------------------------------------------

/// The extraction prompt shared by every engine: the rule's instructions, the
/// call facts, and the exact JSON keys to return.
fn build_prompt(r: &AnalyzerRule, f: &CallFacts) -> String {
    let mut schema = String::new();
    for field in &r.fields {
        let kind = match field.kind.as_str() {
            "number" => "number",
            "bool" => "true/false",
            _ => "string",
        };
        schema.push_str(&format!(
            "- \"{}\" ({kind}): {}\n",
            field.key,
            field.desc.trim()
        ));
    }
    if schema.is_empty() {
        schema.push_str("- \"summary\" (string): one-sentence summary\n");
    }
    format!(
        "You extract structured data from a public-safety radio transcript. \
         The transcript is machine-generated and may contain recognition errors.\n\n\
         Talkgroup: {} (TG {})\nRadio: {}\nTranscript: \"{}\"\n\n\
         {}\n\n\
         Return a JSON object ONLY, with exactly these keys:\n{}\n\
         Use null when a value is not stated in the transcript. Output JSON only, no commentary.",
        f.tg_name,
        f.tg,
        f.unit_name.clone().unwrap_or_else(|| f.unit.to_string()),
        f.transcript.as_deref().unwrap_or(""),
        r.instructions.trim(),
        schema.trim_end(),
    )
}

/// Local-Ollama extraction. Mirrors the alerts gate's `think: false` handling
/// for thinking models.
fn ollama_extract(
    o: &crate::alerts::Ollama,
    r: &AnalyzerRule,
    f: &CallFacts,
) -> Result<serde_json::Value, String> {
    if o.model.trim().is_empty() {
        return Err("no Ollama model chosen".into());
    }
    let full = build_prompt(r, f);

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
        let mut resp = agent
            .post(&format!("{}/api/generate", o.url.trim_end_matches('/')))
            .header("Content-Type", "application/json")
            .send(body.to_string().as_bytes())
            .map_err(|e| format!("ollama: {e}"))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
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
    parse_object(answer)
        .ok_or_else(|| format!("model did not answer in JSON: {}", answer.chars().take(200).collect::<String>()))
}

/// The provider's chat/messages endpoint and which request shape it speaks.
/// `kind` is `openai` (OpenAI-compatible chat/completions — OpenRouter and
/// OpenAI) or `anthropic` (the native Messages API).
fn cloud_endpoint(c: &Cloud) -> Result<(String, &'static str), String> {
    let base = c.base_url.trim().trim_end_matches('/');
    match c.provider.as_str() {
        "openrouter" => {
            let b = if base.is_empty() { "https://openrouter.ai/api/v1" } else { base };
            Ok((format!("{b}/chat/completions"), "openai"))
        }
        "openai" => {
            let b = if base.is_empty() { "https://api.openai.com/v1" } else { base };
            Ok((format!("{b}/chat/completions"), "openai"))
        }
        "anthropic" => {
            let b = if base.is_empty() { "https://api.anthropic.com" } else { base };
            Ok((format!("{b}/v1/messages"), "anthropic"))
        }
        other => Err(format!("unknown cloud provider '{other}'")),
    }
}

/// Cloud-model extraction. One request, JSON out, no streaming.
fn cloud_extract(
    c: &Cloud,
    key: &str,
    r: &AnalyzerRule,
    f: &CallFacts,
) -> Result<serde_json::Value, String> {
    if key.trim().is_empty() {
        return Err("no cloud API key saved".into());
    }
    if c.model.trim().is_empty() {
        return Err("no cloud model set".into());
    }
    let (url, kind) = cloud_endpoint(c)?;
    let full = build_prompt(r, f);
    let system = "You output only a single JSON object. No prose, no code fences.";

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(
            c.timeout_secs.max(5) as u64
        )))
        .http_status_as_error(false)
        .build()
        .into();

    let (status, text) = if kind == "anthropic" {
        let body = serde_json::json!({
            "model": c.model,
            "max_tokens": 1024,
            "system": system,
            "messages": [{ "role": "user", "content": full }],
        });
        let mut resp = agent
            .post(&url)
            .header("x-api-key", key.trim())
            .header("anthropic-version", "2023-06-01")
            .header("Content-Type", "application/json")
            .send(body.to_string().as_bytes())
            .map_err(|e| format!("anthropic: {e}"))?;
        (resp.status().as_u16(), resp.body_mut().read_to_string().unwrap_or_default())
    } else {
        let body = serde_json::json!({
            "model": c.model,
            "temperature": 0,
            "response_format": { "type": "json_object" },
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": full },
            ],
        });
        let mut req = agent
            .post(&url)
            .header("Authorization", &format!("Bearer {}", key.trim()))
            .header("Content-Type", "application/json");
        if c.provider == "openrouter" {
            // Attribution headers OpenRouter surfaces in its dashboard.
            req = req
                .header("HTTP-Referer", "https://github.com/duderayuh/HoosierSDR")
                .header("X-Title", "HoosierSDR Analyzers");
        }
        let mut resp = req
            .send(body.to_string().as_bytes())
            .map_err(|e| format!("{}: {e}", c.provider))?;
        (resp.status().as_u16(), resp.body_mut().read_to_string().unwrap_or_default())
    };

    if status != 200 {
        return Err(format!(
            "{} HTTP {status}: {}",
            c.provider,
            text.chars().take(300).collect::<String>()
        ));
    }
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("{} reply: {e}", c.provider))?;
    let answer = if kind == "anthropic" {
        // content is a list of blocks; take the first text block.
        v["content"]
            .as_array()
            .and_then(|blocks| blocks.iter().find_map(|b| b["text"].as_str()))
            .unwrap_or("")
    } else {
        v["choices"][0]["message"]["content"].as_str().unwrap_or("")
    };
    parse_object(answer).ok_or_else(|| {
        format!("model did not answer in JSON: {}", answer.chars().take(200).collect::<String>())
    })
}

/// The first `{ … }` object in the model's text, tolerating stray prose.
fn parse_object(answer: &str) -> Option<serde_json::Value> {
    let start = answer.find('{')?;
    let end = answer.rfind('}')?;
    let v: serde_json::Value = serde_json::from_str(&answer[start..=end]).ok()?;
    v.is_object().then_some(v)
}

// ---------------------------------------------------------------------------
// conditions
// ---------------------------------------------------------------------------

/// True when the rule's condition holds over the extracted object. No clauses
/// means "always send".
fn evaluate(r: &AnalyzerRule, obj: &serde_json::Value) -> bool {
    if r.conditions.is_empty() {
        return true;
    }
    let any = r.match_mode.eq_ignore_ascii_case("any");
    let mut result = !any; // all → start true; any → start false
    for c in &r.conditions {
        let hit = test_clause(c, obj);
        if any {
            result |= hit;
        } else {
            result &= hit;
        }
    }
    result
}

fn test_clause(c: &Clause, obj: &serde_json::Value) -> bool {
    let lhs = obj.get(&c.field);
    let lhs_str = value_to_string(lhs);
    let rhs = c.value.trim();
    match c.op.as_str() {
        "==" => lhs_str.eq_ignore_ascii_case(rhs),
        "!=" => !lhs_str.eq_ignore_ascii_case(rhs),
        "contains" => lhs_str.to_ascii_lowercase().contains(&rhs.to_ascii_lowercase()),
        ">" | ">=" | "<" | "<=" => match (as_number(lhs), rhs.parse::<f64>()) {
            (Some(l), Ok(r)) => match c.op.as_str() {
                ">" => l > r,
                ">=" => l >= r,
                "<" => l < r,
                _ => l <= r,
            },
            _ => false,
        },
        _ => false,
    }
}

fn as_number(v: Option<&serde_json::Value>) -> Option<f64> {
    match v? {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn value_to_string(v: Option<&serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Bool(b)) => b.to_string(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        Some(serde_json::Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// rendering
// ---------------------------------------------------------------------------

fn render(template: &str, r: &AnalyzerRule, f: &CallFacts, obj: &serde_json::Value) -> String {
    let s = crate::library::now().rem_euclid(86_400);
    let time = format!("{:02}:{:02}:{:02} UTC", s / 3600, (s % 3600) / 60, s % 60);
    let mut out = template
        .replace("{name}", &r.name)
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
        .replace("{json}", &serde_json::to_string_pretty(obj).unwrap_or_default());
    if let Some(map) = obj.as_object() {
        for (k, v) in map {
            out = out.replace(&format!("{{field.{k}}}"), &value_to_string(Some(v)));
        }
    }
    out.trim().to_string()
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn analyzers_get(state: State<AppState>) -> Settings {
    state.analyzers.lock().unwrap().settings.clone()
}

#[tauri::command]
pub fn analyzers_set(
    app: AppHandle,
    state: State<AppState>,
    rules: Vec<AnalyzerRule>,
) -> Result<(), String> {
    let mut rules = rules;
    for (i, r) in rules.iter_mut().enumerate() {
        if r.id.trim().is_empty() {
            r.id = format!("z{}-{i}", crate::library::now());
        }
        if r.name.trim().is_empty() {
            r.name = format!("Analyzer {}", i + 1);
        }
        r.cooldown_secs = r.cooldown_secs.clamp(0, 86_400);
        r.tgs.sort_unstable();
        r.tgs.dedup();
    }
    let mut st = state.analyzers.lock().unwrap();
    let keep: HashSet<(String, u16)> = rules
        .iter()
        .flat_map(|r| r.tgs.iter().map(move |t| (r.id.clone(), *t)))
        .collect();
    st.last_fired.retain(|k, _| keep.contains(k));
    st.settings.rules = rules;
    store(&app, &st.settings)
}

#[tauri::command]
pub fn analyzers_log(state: State<AppState>) -> Vec<LogEntry> {
    state.analyzers.lock().unwrap().log.iter().cloned().collect()
}

/// The cloud settings plus whether an API key is currently stored (the key
/// itself never leaves the secret store).
#[tauri::command]
pub fn analyzer_cloud_get(state: State<AppState>) -> (Cloud, bool) {
    let cloud = state.analyzers.lock().unwrap().settings.cloud.clone();
    let has_key = crate::secrets::get("analyzer-cloud-key")
        .map(|k| !k.trim().is_empty())
        .unwrap_or(false);
    (cloud, has_key)
}

/// Save the cloud provider/model/timeout, and the API key when a non-empty one
/// is supplied (blank leaves the stored key untouched).
#[tauri::command]
pub fn analyzer_cloud_save(
    app: AppHandle,
    state: State<AppState>,
    cloud: Cloud,
    key: Option<String>,
) -> Result<(), String> {
    if let Some(k) = key {
        let k = k.trim();
        if !k.is_empty() {
            crate::secrets::set("analyzer-cloud-key", k)?;
        }
    }
    let mut st = state.analyzers.lock().unwrap();
    st.settings.cloud = cloud;
    st.settings.cloud.timeout_secs = st.settings.cloud.timeout_secs.clamp(5, 300);
    store(&app, &st.settings)
}

/// Forget the stored cloud API key.
#[tauri::command]
pub fn analyzer_cloud_clear_key() -> Result<(), String> {
    crate::secrets::remove("analyzer-cloud-key")
}

/// Built-in starter templates (ECPR candidate screen, SOR survey, stroke).
#[tauri::command]
pub fn analyzer_templates() -> Vec<AnalyzerRule> {
    templates()
}

/// Run one analyzer now against the most recent call that passes its
/// pre-filter, so the rule can be seen working without waiting for traffic.
#[tauri::command]
pub async fn analyzer_test(
    _app: AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> Result<String, String> {
    let r = state
        .analyzers
        .lock()
        .unwrap()
        .settings
        .rules
        .iter()
        .find(|r| r.id == id)
        .cloned()
        .ok_or("no such analyzer")?;
    let db = state
        .db
        .lock()
        .unwrap()
        .clone()
        .ok_or("no call library open")?;
    // Newest transcribed calls on the rule's talkgroups (or any).
    let mut calls: Vec<crate::library::CallRow> = Vec::new();
    {
        let c = db.lock().unwrap();
        let tgs: Vec<Option<u16>> = if r.tgs.is_empty() {
            vec![None]
        } else {
            r.tgs.iter().map(|t| Some(*t)).collect()
        };
        for tg in tgs {
            if let Ok(rows) = crate::library::search(
                &c,
                &crate::library::Query {
                    tg,
                    limit: Some(50),
                    ..Default::default()
                },
            ) {
                calls.extend(rows);
            }
        }
    }
    calls.sort_by_key(|c| std::cmp::Reverse(c.start));
    let f = calls.into_iter().find_map(|c| {
        let text = c
            .transcript_edited
            .clone()
            .or_else(|| c.transcript.clone())
            .filter(|t| !t.trim().is_empty())?;
        let f = CallFacts {
            id: Some(c.id),
            start: c.start,
            tg: c.tg,
            tg_name: c.tg_name,
            unit: c.unit,
            unit_name: c.unit_name,
            secs: c.secs,
            emergency: c.emergency,
            audio: c.audio,
            transcript: Some(text),
        };
        pre_filter(&r, &f).then_some(f)
    });
    let Some(f) = f else {
        return Ok("no recent call matched the pre-filter (talkgroups / keywords)".into());
    };
    let ollama = crate::alerts::shared_settings(&state).1;
    let cloud = state.analyzers.lock().unwrap().settings.cloud.clone();
    let key = crate::secrets::get("analyzer-cloud-key").unwrap_or_default();
    let obj = tauri::async_runtime::spawn_blocking(move || {
        let res = if r.engine == "cloud" {
            cloud_extract(&cloud, &key, &r, &f)
        } else {
            ollama_extract(&ollama, &r, &f)
        };
        res.map(|o| (r, f, o))
    })
    .await
    .map_err(|e| e.to_string())??;
    let (r, f, obj) = obj;
    let matched = evaluate(&r, &obj);
    let extracted = serde_json::to_string_pretty(&obj).unwrap_or_default();
    let preview = render(&r.message, &r, &f, &obj);
    Ok(format!(
        "Tested on TG {} “{}”.\nCondition {}.\n\nExtracted:\n{}\n\nMessage preview:\n{}",
        f.tg,
        f.tg_name,
        if matched { "MET — would send" } else { "not met — would stay quiet" },
        extracted,
        preview
    ))
}

/// The starter templates. ECPR mirrors the n8n ED-ECPR gate; SOR and stroke
/// are common EMS screens in the same shape.
fn templates() -> Vec<AnalyzerRule> {
    vec![
        AnalyzerRule {
            id: String::new(),
            name: "ECPR candidate".into(),
            enabled: false,
            engine: "ollama".into(),
            tgs: Vec::new(),
            keywords: vec![
                "cardiac arrest".into(),
                "working arrest".into(),
                "cpr".into(),
                "vfib".into(),
                "v-fib".into(),
                "unresponsive".into(),
            ],
            instructions: "You are a clinical triage assistant for an ED-ECPR (Extracorporeal CPR) program. From this EMS hospital pre-arrival summary, screen for a POTENTIAL ED-ECPR candidate so the team can decide at the bedside. BIAS TOWARD LETTING MESSAGES THROUGH: missing or ambiguous information is a reason to answer 'maybe', not 'no'.\n\nPath A — adult cardiac arrest, scored on 4 criteria (initial rhythm is NOT scored; note it in reason only): (1) time — age + low-flow minutes < 100, unknown if not stated; (2) witnessed arrest (visual or acoustic, incl. EMS- or near-witnessed); (3) bystander CPR within ~5 min; (4) no known end-stage disease (default met unless stated). Hard exclusions → 'no': traumatic mechanism, age clearly > 65, EtCO2 < 10, aortic dissection with tamponade, > mild aortic regurgitation, impossible femoral access.\n\nPath B — accidental hypothermic arrest, ANY age (cold-water submersion, environmental hypothermia). Age criterion does not apply.\n\nAnswer 'yes' when a path is clearly met with no hard exclusion; 'maybe' when consistent but facts are missing; 'no' only on a hard exclusion or clearly non-arrest.".into(),
            fields: vec![
                Field { key: "candidate".into(), kind: "string".into(), desc: "yes | maybe | no".into() },
                Field { key: "criteriaMet".into(), kind: "number".into(), desc: "count of the 4 criteria clearly met (0-4)".into() },
                Field { key: "likelihoodPct".into(), kind: "number".into(), desc: "rough favourable-outcome likelihood %: 4/4≈46, 3/4≈12, ≤2/4≈0-5".into() },
                Field { key: "reason".into(), kind: "string".into(), desc: "one or two sentences, incl. rhythm if stated".into() },
            ],
            match_mode: "all".into(),
            conditions: vec![
                Clause { field: "candidate".into(), op: "!=".into(), value: "no".into() },
                Clause { field: "likelihoodPct".into(), op: ">=".into(), value: "10".into() },
            ],
            message: "🫀 Possible ECPR candidate — {candidate}\n{tgname} (TG {tg}) · {time}\nCriteria met {field.criteriaMet}/4 · likelihood {field.likelihoodPct}%\n{field.reason}\n\n{transcript}".into(),
            chat_id: String::new(),
            telegram: true,
            bluesky: false,
            attach_audio: true,
            cooldown_secs: 120,
        },
        AnalyzerRule {
            id: String::new(),
            name: "SOR (stroke) survey".into(),
            enabled: false,
            engine: "ollama".into(),
            tgs: Vec::new(),
            keywords: vec!["stroke".into(), "cva".into(), "facial droop".into(), "slurred".into()],
            instructions: "From this EMS hospital pre-arrival summary, extract the stroke (SOR — Stroke On Radio) screen: is a stroke alert or suspected stroke being called, last-known-well time if stated, and the deficits mentioned. Do not infer beyond what is said.".into(),
            fields: vec![
                Field { key: "strokeAlert".into(), kind: "bool".into(), desc: "true if a stroke alert / suspected stroke is stated".into() },
                Field { key: "lastKnownWell".into(), kind: "string".into(), desc: "last-known-well time or interval, or null".into() },
                Field { key: "deficits".into(), kind: "string".into(), desc: "deficits mentioned (facial droop, arm drift, speech, etc.)".into() },
            ],
            match_mode: "all".into(),
            conditions: vec![Clause { field: "strokeAlert".into(), op: "==".into(), value: "true".into() }],
            message: "🧠 Stroke alert\n{tgname} (TG {tg}) · {time}\nLKW: {field.lastKnownWell}\nDeficits: {field.deficits}\n\n{transcript}".into(),
            chat_id: String::new(),
            telegram: true,
            bluesky: false,
            attach_audio: false,
            cooldown_secs: 120,
        },
    ]
}
