//! Script hook: run a program of the listener's choosing after each call
//! completes — the escape hatch for everything this app does not do itself
//! (push notifications, home automation, a custom archive).
//!
//! The command runs on its own worker thread, never on the follow thread, so
//! a slow script cannot stall decoding; one run at a time, with a queue
//! bounded so a wedged script cannot pile up memory, and a timeout so it
//! cannot wedge forever. Call details go to the child as environment
//! variables (`HS_*`) and as one JSON object on standard input.

use serde::{Deserialize, Serialize};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::AppState;

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Settings {
    pub enabled: bool,
    /// Program to run; arguments may follow, shell-style quoting (single or
    /// double quotes) honoured. `~` expands to the home directory.
    pub command: String,
    /// Seconds before the child is killed.
    pub timeout_secs: u32,
    /// Skip calls shorter than this.
    pub min_secs: f64,
    /// Only run for emergency calls.
    pub emergency_only: bool,
}

impl Settings {
    fn defaults() -> Self {
        Self {
            enabled: false,
            command: String::new(),
            timeout_secs: 20,
            min_secs: 0.0,
            emergency_only: false,
        }
    }
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Status {
    pub runs: u64,
    pub failures: u64,
    pub last_error: Option<String>,
    pub last_output: Option<String>,
}

/// One completed call, as handed to the script.
#[derive(Serialize, Clone, Debug)]
pub struct CallInfo {
    pub id: Option<i64>,
    pub start: i64,
    pub secs: f64,
    pub tg: u16,
    pub tg_name: String,
    pub unit: u32,
    pub unit_name: Option<String>,
    pub talker_alias: Option<String>,
    pub freq_hz: u64,
    pub modulation: String,
    pub emergency: bool,
    pub patched_with: Vec<u16>,
    pub system: String,
    pub audio: Option<String>,
    pub sidecar: Option<String>,
}

pub struct Hook {
    tx: SyncSender<CallInfo>,
    pub settings: Arc<Mutex<Settings>>,
    pub status: Arc<Mutex<Status>>,
}

pub type Shared = Mutex<Option<Hook>>;

fn path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let d = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("config dir: {e}"))?;
    std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    Ok(d.join("hook.json"))
}

pub fn load_settings(app: &AppHandle) -> Settings {
    path(app)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(Settings::defaults)
}

fn store(app: &AppHandle, s: &Settings) -> Result<(), String> {
    let p = path(app)?;
    std::fs::write(
        &p,
        serde_json::to_string_pretty(s).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("{}: {e}", p.display()))
}

/// Split a command line into words, honouring single and double quotes and
/// backslash escapes — enough for `"/path with spaces/notify.sh" --loud`.
pub fn split_command(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut any = false;
    for c in line.chars() {
        if escaped {
            cur.push(c);
            escaped = false;
            any = true;
            continue;
        }
        match (c, quote) {
            ('\\', Some('"') | None) => escaped = true,
            (q, None) if q == '"' || q == '\'' => {
                quote = Some(q);
                any = true;
            }
            (q, Some(open)) if q == open => quote = None,
            (c, None) if c.is_whitespace() => {
                if any {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            (c, _) => {
                cur.push(c);
                any = true;
            }
        }
    }
    if any {
        out.push(cur);
    }
    out
}

/// Run the hook once, synchronously. Returns the child's trimmed output.
pub fn run_once(s: &Settings, call: &CallInfo) -> Result<String, String> {
    let words = split_command(&s.command);
    let Some(prog) = words.first() else {
        return Err("no command set".into());
    };
    let prog = crate::shellexpand_home(prog);
    let json = serde_json::to_string(call).map_err(|e| e.to_string())?;
    let mut cmd = std::process::Command::new(&prog);
    cmd.args(&words[1..])
        .env("HS_CALL_ID", call.id.map(|i| i.to_string()).unwrap_or_default())
        .env("HS_START", call.start.to_string())
        .env("HS_SECS", format!("{:.1}", call.secs))
        .env("HS_TG", call.tg.to_string())
        .env("HS_TG_NAME", &call.tg_name)
        .env("HS_UNIT", call.unit.to_string())
        .env("HS_UNIT_NAME", call.unit_name.clone().unwrap_or_default())
        .env("HS_TALKER_ALIAS", call.talker_alias.clone().unwrap_or_default())
        .env("HS_FREQ_HZ", call.freq_hz.to_string())
        .env("HS_MODULATION", &call.modulation)
        .env("HS_EMERGENCY", if call.emergency { "1" } else { "0" })
        .env(
            "HS_PATCHED_WITH",
            call.patched_with
                .iter()
                .map(|t| t.to_string())
                .collect::<Vec<_>>()
                .join(","),
        )
        .env("HS_SYSTEM", &call.system)
        .env("HS_AUDIO", call.audio.clone().unwrap_or_default())
        .env("HS_JSON", call.sidecar.clone().unwrap_or_default())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("{prog}: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(json.as_bytes());
        let _ = stdin.write_all(b"\n");
    }
    // Wait with a timeout, polling; kill on expiry.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(s.timeout_secs.max(1) as u64);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = child.wait_with_output().map_err(|e| e.to_string())?;
                let text = format!(
                    "{}{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                );
                let text = text.trim().chars().take(400).collect::<String>();
                if status.success() {
                    return Ok(text);
                }
                return Err(format!("exit {}: {text}", status.code().unwrap_or(-1)));
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("timed out after {} s", s.timeout_secs));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// Start the worker. Calls are queued (bounded) and run one at a time.
pub fn start(app: AppHandle, settings: Settings) -> Hook {
    let (tx, rx) = sync_channel::<CallInfo>(64);
    let settings = Arc::new(Mutex::new(settings));
    let status = Arc::new(Mutex::new(Status::default()));
    let (s2, st2) = (Arc::clone(&settings), Arc::clone(&status));
    std::thread::spawn(move || {
        while let Ok(call) = rx.recv() {
            let s = s2.lock().unwrap().clone();
            if !s.enabled || s.command.trim().is_empty() {
                continue;
            }
            let res = run_once(&s, &call);
            let mut st = st2.lock().unwrap();
            st.runs += 1;
            match res {
                Ok(out) => {
                    st.last_output = Some(out);
                    st.last_error = None;
                }
                Err(e) => {
                    st.failures += 1;
                    st.last_error = Some(e.clone());
                    let _ = app.emit("hook_error", e);
                }
            }
        }
    });
    Hook {
        tx,
        settings,
        status,
    }
}

impl Hook {
    /// Queue a call for the script. Drops (and records) when the queue is
    /// full rather than blocking the follow thread.
    pub fn submit(&self, call: CallInfo) {
        let s = self.settings.lock().unwrap();
        if !s.enabled
            || s.command.trim().is_empty()
            || call.secs < s.min_secs
            || (s.emergency_only && !call.emergency)
        {
            return;
        }
        drop(s);
        if let Err(TrySendError::Full(_)) = self.tx.try_send(call) {
            let mut st = self.status.lock().unwrap();
            st.failures += 1;
            st.last_error = Some("hook queue full — script too slow, call skipped".into());
        }
    }
}

#[derive(Serialize)]
pub struct View {
    pub settings: Settings,
    pub status: Status,
}

#[tauri::command]
pub fn hook_get(app: AppHandle, state: State<AppState>) -> View {
    let h = state.hook.lock().unwrap();
    match h.as_ref() {
        Some(h) => View {
            settings: h.settings.lock().unwrap().clone(),
            status: h.status.lock().unwrap().clone(),
        },
        None => View {
            settings: load_settings(&app),
            status: Status::default(),
        },
    }
}

#[tauri::command]
pub fn hook_configure(
    app: AppHandle,
    state: State<AppState>,
    settings: Settings,
) -> Result<(), String> {
    store(&app, &settings)?;
    let mut h = state.hook.lock().unwrap();
    match h.as_mut() {
        Some(h) => *h.settings.lock().unwrap() = settings,
        None => *h = Some(start(app.clone(), settings)),
    }
    Ok(())
}

/// Run the script once with a sample call, synchronously, and return its
/// output — so a listener can see it work before a real call arrives.
#[tauri::command]
pub async fn hook_test(settings: Settings) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let sample = CallInfo {
            id: None,
            start: crate::library::now(),
            secs: 4.2,
            tg: 20308,
            tg_name: "Test talkgroup".into(),
            unit: 790065,
            unit_name: Some("Test radio".into()),
            talker_alias: None,
            freq_hz: 851_812_500,
            modulation: "CQPSK".into(),
            emergency: false,
            patched_with: vec![],
            system: "HoosierSDR test".into(),
            audio: None,
            sidecar: None,
        };
        let mut s = settings;
        s.enabled = true;
        run_once(&s, &sample).map(|o| if o.is_empty() { "ok (no output)".into() } else { o })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_quoted_command_lines() {
        assert_eq!(
            split_command(r#""/Users/me/my scripts/notify.sh" --loud 'a b' c\ d"#),
            vec!["/Users/me/my scripts/notify.sh", "--loud", "a b", "c d"]
        );
        assert!(split_command("   ").is_empty());
        assert_eq!(split_command("''"), vec![""]);
    }

    /// The child sees the call as environment and as JSON on stdin.
    #[test]
    fn runs_a_script_with_call_environment_and_stdin() {
        let s = Settings {
            enabled: true,
            command: "/bin/sh -c 'read j; echo \"$HS_TG $HS_TG_NAME $j\"'".into(),
            timeout_secs: 5,
            min_secs: 0.0,
            emergency_only: false,
        };
        let call = CallInfo {
            id: Some(7),
            start: 0,
            secs: 1.0,
            tg: 20308,
            tg_name: "Sheriff".into(),
            unit: 1,
            unit_name: None,
            talker_alias: None,
            freq_hz: 851_812_500,
            modulation: "C4FM".into(),
            emergency: false,
            patched_with: vec![],
            system: String::new(),
            audio: None,
            sidecar: None,
        };
        let out = run_once(&s, &call).unwrap();
        assert!(out.starts_with("20308 Sheriff {"), "{out}");
        assert!(out.contains("\"tg\":20308"));
        // Failure and timeout are reported, not swallowed.
        let mut bad = s.clone();
        bad.command = "/bin/sh -c 'exit 3'".into();
        assert!(run_once(&bad, &call).unwrap_err().starts_with("exit 3"));
        let mut slow = s.clone();
        slow.command = "/bin/sh -c 'sleep 5'".into();
        slow.timeout_secs = 1;
        assert!(run_once(&slow, &call).unwrap_err().contains("timed out"));
    }
}
