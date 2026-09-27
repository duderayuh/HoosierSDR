//! Email: a second way out for anything that goes to Telegram.
//!
//! One SMTP account, set on Connections, sends every email. What goes out,
//! and to whom, is set where the Telegram message is — a tripwire's own
//! email recipients and subject and body templates, a case profile's, a
//! hospital's — so either, both or neither can be on.
//!
//! Email cannot edit or delete what it sent, which is how Telegram keeps a
//! case or a report current. Here a later word is a new email in the same
//! thread instead: every email of a thread carries `In-Reply-To` and
//! `References` naming the first, so a mail client files them together.
//!
//! The password lives in the secret store with the bot token, never in
//! `email.json`. Sending is blocking, like the Telegram calls it sits beside.

use lettre::message::header::ContentType;
use lettre::message::{Attachment as Attach, Mailbox, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{Message, SmtpTransport, Transport};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};
use std::time::Duration;
use tauri::{AppHandle, Manager};

const PASSWORD_KEY: &str = "smtp-password";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    /// `starttls` (587), `tls` (465) or `none` (a relay inside the building).
    pub security: String,
    pub username: String,
    /// The name and address emails come from. The address defaults to the
    /// username, which most providers require anyway.
    pub from_name: String,
    pub from_addr: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            host: String::new(),
            port: 587,
            security: "starttls".into(),
            username: String::new(),
            from_name: "HoosierSDR".into(),
            from_addr: String::new(),
        }
    }
}

impl Settings {
    fn from_addr(&self) -> &str {
        if self.from_addr.trim().is_empty() { self.username.trim() } else { self.from_addr.trim() }
    }

    /// Why this account cannot send, or None when it can.
    pub fn problem(&self) -> Option<String> {
        if !self.enabled {
            return Some("email is switched off on Connections".into());
        }
        if self.host.trim().is_empty() {
            return Some("no mail server is set on Connections".into());
        }
        if !self.from_addr().contains('@') {
            return Some("no From address is set on Connections".into());
        }
        None
    }
}

static DIR: OnceLock<PathBuf> = OnceLock::new();
static SETTINGS: OnceLock<RwLock<Settings>> = OnceLock::new();

fn cell() -> &'static RwLock<Settings> {
    SETTINGS.get_or_init(|| RwLock::new(Settings::default()))
}

/// Called once at startup: read `email.json` so any sender can reach it.
pub fn init(app: &AppHandle) {
    if let Ok(d) = app.path().app_config_dir() {
        let _ = DIR.set(d);
    }
    let s = DIR
        .get()
        .and_then(|d| std::fs::read_to_string(d.join("email.json")).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    *cell().write().unwrap() = s;
}

pub fn settings() -> Settings {
    cell().read().unwrap().clone()
}

fn store(s: &Settings) -> Result<(), String> {
    let d = DIR.get().ok_or("the config folder is not known yet")?;
    std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    let p = d.join("email.json");
    std::fs::write(&p, serde_json::to_string_pretty(s).map_err(|e| e.to_string())?).map_err(|e| format!("{}: {e}", p.display()))?;
    *cell().write().unwrap() = s.clone();
    Ok(())
}

// ---------------------------------------------------------------------------
// recipients and threads
// ---------------------------------------------------------------------------

/// The addresses in a recipients field: separated by commas, semicolons or
/// spaces, each checked well enough to be worth a try. `Name <a@b>` is kept.
pub fn recipients(field: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in field.split([',', ';', '\n']) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let candidates: Vec<&str> = if part.contains('<') { vec![part] } else { part.split_whitespace().collect() };
        for c in candidates {
            if c.parse::<Mailbox>().is_ok() && !out.iter().any(|o| o.eq_ignore_ascii_case(c)) {
                out.push(c.to_string());
            }
        }
    }
    out
}

/// The parts of a recipients field that are not addresses, for the editor.
pub fn bad_recipients(field: &str) -> Vec<String> {
    field
        .split([',', ';', '\n'])
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .flat_map(|p| if p.contains('<') { vec![p] } else { p.split_whitespace().collect() })
        .filter(|c| c.parse::<Mailbox>().is_err())
        .map(String::from)
        .collect()
}

/// A stable Message-ID for the first email of a thread, so every later
/// email can name it without anything else being kept.
pub fn thread_id(parts: &[&str]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    parts.hash(&mut h);
    let host = settings().from_addr().split('@').nth(1).unwrap_or("hoosiersdr.local").to_string();
    format!("<hs-{:016x}@{}>", h.finish(), host)
}

// ---------------------------------------------------------------------------
// the message
// ---------------------------------------------------------------------------

pub struct Attachment {
    pub name: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}

impl Attachment {
    /// A recording or a map from disk, typed by its extension.
    pub fn file(path: &std::path::Path) -> Option<Attachment> {
        let bytes = std::fs::read(path).ok()?;
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        let mime = match ext.as_str() {
            "mp3" => "audio/mpeg",
            "wav" => "audio/wav",
            "m4a" => "audio/mp4",
            "png" => "image/png",
            _ => "application/octet-stream",
        };
        Some(Attachment { name: path.file_name()?.to_string_lossy().into_owned(), mime: mime.into(), bytes })
    }
}

#[derive(Default)]
pub struct Mail {
    pub to: Vec<String>,
    pub subject: String,
    pub text: String,
    /// The body as HTML; `text` alone when None.
    pub html: Option<String>,
    pub attachments: Vec<Attachment>,
    /// This email's own Message-ID, when it opens a thread.
    pub message_id: Option<String>,
    /// The thread's first email, when this one follows it.
    pub in_reply_to: Option<String>,
}

/// A subject is one line, and a mail client shows about eighty characters.
pub fn subject_line(s: &str) -> String {
    let one = s.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" · ");
    if one.chars().count() > 150 { format!("{}…", one.chars().take(149).collect::<String>()) } else { one }
}

/// Telegram's HTML (b, i, u, s, code, pre, a, blockquote, tg-spoiler, and
/// newlines for breaks) as an email body.
pub fn html_body(telegram_html: &str) -> String {
    let body = telegram_html
        .replace("<tg-spoiler>", "<span>")
        .replace("</tg-spoiler>", "</span>")
        .replace("<blockquote expandable>", "<blockquote>")
        .replace("<blockquote>", "<blockquote style=\"margin:4px 0;padding-left:10px;border-left:3px solid #0b988c;color:#3a4a47\">")
        .replace('\n', "<br>\n");
    format!(
        "<!doctype html><html><body style=\"font-family:-apple-system,Segoe UI,Helvetica,Arial,sans-serif;font-size:15px;line-height:1.45;color:#0f1c1a\">\n{body}\n</body></html>"
    )
}

fn build(s: &Settings, m: &Mail) -> Result<Message, String> {
    let from: Mailbox = if s.from_name.trim().is_empty() {
        s.from_addr().parse()
    } else {
        format!("{} <{}>", s.from_name.trim().replace(['<', '>', '"'], ""), s.from_addr()).parse()
    }
    .map_err(|e| format!("From address: {e}"))?;
    let mut b = Message::builder().from(from).subject(subject_line(&m.subject));
    let mut any = false;
    for t in &m.to {
        match t.parse::<Mailbox>() {
            Ok(mb) => {
                b = b.to(mb);
                any = true;
            }
            Err(e) => return Err(format!("{t}: {e}")),
        }
    }
    if !any {
        return Err("no recipients".into());
    }
    if let Some(id) = &m.message_id {
        b = b.message_id(Some(id.clone()));
    }
    if let Some(root) = &m.in_reply_to {
        b = b.in_reply_to(root.clone()).references(root.clone());
    }
    let body = match &m.html {
        Some(h) => MultiPart::alternative_plain_html(m.text.clone(), h.clone()),
        None => MultiPart::mixed().singlepart(SinglePart::plain(m.text.clone())),
    };
    let body = if m.attachments.is_empty() {
        body
    } else {
        let mut mixed = MultiPart::mixed().multipart(body);
        for a in &m.attachments {
            let ct = ContentType::parse(&a.mime).unwrap_or(ContentType::parse("application/octet-stream").unwrap());
            mixed = mixed.singlepart(Attach::new(a.name.clone()).body(a.bytes.clone(), ct));
        }
        mixed
    };
    b.multipart(body).map_err(|e| format!("building the email: {e}"))
}

fn transport(s: &Settings) -> Result<SmtpTransport, String> {
    let host = s.host.trim();
    let tls = || TlsParameters::new(host.to_string()).map_err(|e| format!("TLS for {host}: {e}"));
    let b = match s.security.as_str() {
        "tls" => SmtpTransport::builder_dangerous(host).tls(Tls::Wrapper(tls()?)),
        "none" => SmtpTransport::builder_dangerous(host).tls(Tls::None),
        _ => SmtpTransport::builder_dangerous(host).tls(Tls::Required(tls()?)),
    };
    let mut b = b.port(s.port).timeout(Some(Duration::from_secs(30)));
    if !s.username.trim().is_empty() {
        let pw = crate::secrets::get(PASSWORD_KEY).unwrap_or_default();
        b = b.credentials(Credentials::new(s.username.trim().to_string(), pw));
    }
    Ok(b.build())
}

/// Send one email and give back its Message-ID. Blocking.
pub fn send(m: &Mail) -> Result<String, String> {
    if std::env::var("HS_NO_SEND").is_ok_and(|v| v == "1") {
        return Err("sending is switched off (HS_NO_SEND)".into());
    }
    let s = settings();
    if let Some(p) = s.problem() {
        return Err(p);
    }
    let msg = build(&s, m)?;
    let id = msg.headers().get_raw("Message-ID").map(str::to_string).unwrap_or_default();
    transport(&s)?.send(&msg).map_err(|e| format!("{}: {e}", s.host.trim()))?;
    Ok(id)
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct View {
    pub settings: Settings,
    pub has_password: bool,
}

#[tauri::command]
pub fn email_get() -> View {
    View { settings: settings(), has_password: crate::secrets::get(PASSWORD_KEY).is_some() }
}

#[tauri::command]
pub fn email_set(settings: Settings) -> Result<View, String> {
    let mut s = settings;
    s.host = s.host.trim().to_string();
    s.username = s.username.trim().to_string();
    s.from_addr = s.from_addr.trim().to_string();
    if !["starttls", "tls", "none"].contains(&s.security.as_str()) {
        s.security = "starttls".into();
    }
    if s.port == 0 {
        s.port = if s.security == "tls" { 465 } else { 587 };
    }
    store(&s)?;
    Ok(email_get())
}

/// Keep the SMTP password; an empty one forgets it.
#[tauri::command]
pub fn email_save_password(password: String) -> Result<(), String> {
    if password.is_empty() { crate::secrets::remove(PASSWORD_KEY) } else { crate::secrets::set(PASSWORD_KEY, &password) }
}

/// Send a test email and say what happened.
#[tauri::command]
pub async fn email_test(to: String) -> Result<String, String> {
    let to_list = recipients(&to);
    if to_list.is_empty() {
        return Err("enter an address to send the test to".into());
    }
    let s = settings();
    let mail = Mail {
        to: to_list.clone(),
        subject: "HoosierSDR can reach you by email".into(),
        text: format!("This is a test from HoosierSDR, sent through {} as {}.", s.host, s.from_addr()),
        html: None,
        ..Default::default()
    };
    tauri::async_runtime::spawn_blocking(move || send(&mail)).await.map_err(|e| e.to_string())??;
    Ok(format!("Sent to {}", to_list.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mail server in a thread: speaks just enough SMTP to take one
    /// message and hand back what it was sent.
    fn fake_server() -> (u16, std::sync::mpsc::Receiver<String>) {
        use std::io::{BufRead, BufReader, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut w = s.try_clone().unwrap();
            let mut r = BufReader::new(s);
            let mut said = String::new();
            let _ = w.write_all(b"220 fake ESMTP\r\n");
            let mut line = String::new();
            let mut data = false;
            while r.read_line(&mut line).unwrap_or(0) > 0 {
                said.push_str(&line);
                let up = line.to_ascii_uppercase();
                let reply: &[u8] = if data {
                    if line == ".\r\n" {
                        data = false;
                        b"250 queued\r\n"
                    } else {
                        b""
                    }
                } else if up.starts_with("EHLO") {
                    b"250-fake\r\n250 AUTH PLAIN LOGIN\r\n"
                } else if up.starts_with("AUTH") {
                    b"235 ok\r\n"
                } else if up.starts_with("DATA") {
                    data = true;
                    b"354 go\r\n"
                } else if up.starts_with("QUIT") {
                    let _ = w.write_all(b"221 bye\r\n");
                    break;
                } else {
                    b"250 ok\r\n"
                };
                let _ = w.write_all(reply);
                line.clear();
            }
            let _ = tx.send(said);
        });
        (port, rx)
    }

    #[test]
    fn an_email_goes_through_a_mail_server() {
        let (port, rx) = fake_server();
        *cell().write().unwrap() = Settings {
            enabled: true,
            host: "127.0.0.1".into(),
            port,
            security: "none".into(),
            username: String::new(),
            from_name: "HoosierSDR".into(),
            from_addr: "alerts@example.org".into(),
        };
        let m = Mail {
            to: vec!["ed@example.org".into(), "team@example.org".into()],
            subject: "🫀 Cardiac arrest · 6328 Brookline Drive".into(),
            text: "Working arrest".into(),
            html: Some(html_body("<b>Working arrest</b>")),
            attachments: vec![Attachment { name: "page.mp3".into(), mime: "audio/mpeg".into(), bytes: vec![0xff, 0xfb, 0x90] }],
            message_id: Some("<hs-test@example.org>".into()),
            in_reply_to: None,
        };
        assert_eq!(send(&m).unwrap(), "<hs-test@example.org>");
        let said = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(said.contains("MAIL FROM:<alerts@example.org>"), "{said}");
        assert!(said.contains("RCPT TO:<ed@example.org>") && said.contains("RCPT TO:<team@example.org>"), "{said}");
        assert!(said.contains("Message-ID: <hs-test@example.org>"), "{said}");
        assert!(said.contains("page.mp3"), "{said}");
        *cell().write().unwrap() = Settings::default();
    }

    #[test]
    fn recipients_are_split_and_checked() {
        assert_eq!(recipients("a@x.org, b@y.org; c@z.org"), vec!["a@x.org", "b@y.org", "c@z.org"]);
        assert_eq!(recipients("a@x.org a@X.org\nnot-an-address"), vec!["a@x.org"]);
        assert_eq!(recipients("ED Charge <charge@x.org>, d@x.org"), vec!["ED Charge <charge@x.org>", "d@x.org"]);
        assert!(recipients("").is_empty());
        assert_eq!(bad_recipients("a@x.org, nope"), vec!["nope"]);
    }

    #[test]
    fn a_subject_is_one_line() {
        assert_eq!(subject_line("🚨 Arrest\n\nFire · Medic 3\n"), "🚨 Arrest · Fire · Medic 3");
        assert!(subject_line(&"x".repeat(400)).chars().count() <= 150);
    }

    #[test]
    fn telegram_markup_becomes_an_email_body() {
        let h = html_body("<b>Arrest</b>\n<blockquote>said</blockquote><tg-spoiler>x</tg-spoiler>");
        assert!(h.contains("<b>Arrest</b><br>"), "{h}");
        assert!(h.contains("<blockquote style="), "{h}");
        assert!(!h.contains("tg-spoiler"), "{h}");
    }

    #[test]
    fn an_email_threads_on_its_first() {
        let s = Settings { enabled: true, host: "smtp.example.org".into(), username: "me@example.org".into(), ..Default::default() };
        let root = "<hs-1@example.org>".to_string();
        let m = Mail {
            to: vec!["a@example.org".into()],
            subject: "Re: Arrest".into(),
            text: "later".into(),
            html: Some(html_body("later")),
            in_reply_to: Some(root.clone()),
            attachments: vec![Attachment { name: "call.mp3".into(), mime: "audio/mpeg".into(), bytes: vec![1, 2, 3] }],
            ..Default::default()
        };
        let raw = String::from_utf8(build(&s, &m).unwrap().formatted()).unwrap();
        assert!(raw.contains("In-Reply-To: <hs-1@example.org>"), "{raw}");
        assert!(raw.contains("References: <hs-1@example.org>"), "{raw}");
        assert!(raw.contains("From: HoosierSDR <me@example.org>"), "{raw}");
        assert!(raw.contains("call.mp3") && raw.contains("audio/mpeg"), "{raw}");
        assert!(raw.contains("text/html"), "{raw}");
        let opened = Mail { message_id: Some(root.clone()), in_reply_to: None, ..m };
        let raw = String::from_utf8(build(&s, &opened).unwrap().formatted()).unwrap();
        assert!(raw.contains("Message-ID: <hs-1@example.org>"), "{raw}");
        assert!(Settings::default().problem().is_some());
        assert!(s.problem().is_none());
    }
}
