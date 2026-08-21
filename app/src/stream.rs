//! Live audio feed to an Icecast / Shoutcast-compatible server (Broadcastify
//! feeds speak Icecast). ffmpeg does the encoding and the connection; this
//! side writes a continuous 8 kHz PCM stream — calls as they are spoken,
//! silence between — so the mount never starves.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager, State};

use crate::AppState;

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct Settings {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub mount: String,
    pub user: String,
    pub password: String,
    /// mp3 | aac | opus
    pub codec: String,
    pub bitrate_kbps: u32,
    pub name: String,
    pub description: String,
    pub tls: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            host: "audio1.broadcastify.com".into(),
            port: 80,
            mount: "/mount".into(),
            user: "source".into(),
            password: String::new(),
            codec: "mp3".into(),
            bitrate_kbps: 16,
            name: "HoosierSDR".into(),
            description: "P25 trunked scanner".into(),
            tls: false,
        }
    }
}

enum Cmd {
    Pcm(Vec<i16>),
    Reconfigure(Settings),
    Stop,
}

#[derive(Default, Clone, Serialize)]
pub struct Status {
    pub running: bool,
    pub connected: bool,
    pub bytes_sent: u64,
    pub last_error: Option<String>,
}

pub struct Streamer {
    tx: Sender<Cmd>,
    pub status: Arc<Mutex<Status>>,
    pub settings: Arc<Mutex<Settings>>,
}

pub type Shared = Arc<Mutex<Option<Streamer>>>;

fn settings_path(app: &AppHandle) -> Option<std::path::PathBuf> {
    app.path()
        .app_config_dir()
        .ok()
        .map(|d| d.join("stream.json"))
}

pub fn load_settings(app: &AppHandle) -> Settings {
    settings_path(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn spawn_ffmpeg(s: &Settings) -> Result<(Child, ChildStdin), String> {
    let scheme = if s.tls { "icecast+tls" } else { "icecast" };
    let url = format!(
        "{scheme}://{}:{}@{}:{}{}",
        s.user,
        s.password,
        s.host,
        s.port,
        if s.mount.starts_with('/') {
            s.mount.clone()
        } else {
            format!("/{}", s.mount)
        }
    );
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-loglevel",
        "error",
        "-re",
        "-f",
        "s16le",
        "-ar",
        "8000",
        "-ac",
        "1",
        "-i",
        "pipe:0",
    ]);
    let (codec, ctype, fmt) = match s.codec.as_str() {
        "aac" => ("aac", "audio/aac", "adts"),
        "opus" => ("libopus", "audio/ogg", "ogg"),
        _ => ("libmp3lame", "audio/mpeg", "mp3"),
    };
    cmd.args([
        "-c:a",
        codec,
        "-b:a",
        &format!("{}k", s.bitrate_kbps),
        "-ar",
        "22050",
    ]);
    cmd.args([
        "-content_type",
        ctype,
        "-ice_name",
        &s.name,
        "-ice_description",
        &s.description,
        "-ice_public",
        "0",
    ]);
    cmd.args(["-f", fmt]).arg(&url);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("ffmpeg: {e}"))?;
    let stdin = child.stdin.take().ok_or("ffmpeg stdin")?;
    Ok((child, stdin))
}

/// Feeder thread: real-time paced PCM (calls or silence) into ffmpeg, with
/// reconnects on failure.
fn feeder(rx: Receiver<Cmd>, status: Arc<Mutex<Status>>, settings: Arc<Mutex<Settings>>) {
    let mut proc: Option<(Child, ChildStdin)> = None;
    let mut queue: std::collections::VecDeque<i16> = Default::default();
    let mut next_retry = Instant::now();
    let chunk = 800usize; // 100 ms at 8 kHz
    let mut silence_in_row = 0u32;
    loop {
        // Drain commands without blocking.
        loop {
            match rx.try_recv() {
                Ok(Cmd::Pcm(p)) => queue.extend(p),
                Ok(Cmd::Reconfigure(s)) => {
                    *settings.lock().unwrap() = s;
                    if let Some((mut c, _)) = proc.take() {
                        let _ = c.kill();
                        let _ = c.wait();
                    }
                    next_retry = Instant::now();
                }
                Ok(Cmd::Stop) => {
                    if let Some((mut c, _)) = proc.take() {
                        let _ = c.kill();
                        let _ = c.wait();
                    }
                    let mut st = status.lock().unwrap();
                    st.running = false;
                    st.connected = false;
                    return;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
            }
        }
        if proc.is_none() && Instant::now() >= next_retry {
            let s = settings.lock().unwrap().clone();
            match spawn_ffmpeg(&s) {
                Ok(p) => {
                    proc = Some(p);
                    let mut st = status.lock().unwrap();
                    st.connected = true;
                    st.last_error = None;
                }
                Err(e) => {
                    status.lock().unwrap().last_error = Some(e);
                    next_retry = Instant::now() + Duration::from_secs(15);
                }
            }
        }
        // One 100 ms frame: from the queue, else silence.
        let mut frame = Vec::with_capacity(chunk * 2);
        for _ in 0..chunk {
            let v = queue.pop_front().unwrap_or(0);
            frame.extend_from_slice(&v.to_le_bytes());
        }
        silence_in_row = if queue.is_empty() {
            silence_in_row.saturating_add(1)
        } else {
            0
        };
        if let Some((child, stdin)) = proc.as_mut() {
            if let Err(e) = stdin.write_all(&frame) {
                let err = format!("ffmpeg/icecast connection ended: {e}");
                let _ = child.kill();
                let _ = child.wait();
                proc = None;
                let mut st = status.lock().unwrap();
                st.connected = false;
                st.last_error = Some(err.trim().to_string());
                next_retry = Instant::now() + Duration::from_secs(10);
            } else {
                status.lock().unwrap().bytes_sent += frame.len() as u64;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub fn start(settings: Settings) -> Streamer {
    let (tx, rx) = channel();
    let status = Arc::new(Mutex::new(Status {
        running: true,
        ..Default::default()
    }));
    let shared = Arc::new(Mutex::new(settings));
    let (st2, se2) = (Arc::clone(&status), Arc::clone(&shared));
    std::thread::spawn(move || feeder(rx, st2, se2));
    Streamer {
        tx,
        status,
        settings: shared,
    }
}

impl Streamer {
    pub fn feed(&self, pcm: &[i16]) {
        let _ = self.tx.send(Cmd::Pcm(pcm.to_vec()));
    }
    pub fn reconfigure(&self, s: Settings) {
        let _ = self.tx.send(Cmd::Reconfigure(s));
    }
    pub fn stop(&self) {
        let _ = self.tx.send(Cmd::Stop);
    }
}

#[derive(Serialize)]
pub struct View {
    settings: Settings,
    status: Status,
    ffmpeg: bool,
}

#[tauri::command]
pub fn stream_get(app: AppHandle, state: State<AppState>) -> View {
    let guard = state.streamer.lock().unwrap();
    let (settings, status) = match guard.as_ref() {
        Some(s) => (
            s.settings.lock().unwrap().clone(),
            s.status.lock().unwrap().clone(),
        ),
        None => (load_settings(&app), Status::default()),
    };
    View {
        settings,
        status,
        ffmpeg: crate::encode::ffmpeg_available().is_some(),
    }
}

/// Save settings and start / reconfigure / stop the feed accordingly.
#[tauri::command]
pub fn stream_configure(
    app: AppHandle,
    state: State<AppState>,
    settings: Settings,
) -> Result<(), String> {
    if let Some(p) = settings_path(&app) {
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        // Never log the password; it is stored here because ffmpeg needs it
        // on the command line anyway.
        std::fs::write(
            &p,
            serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())?,
        )
        .map_err(|e| format!("{}: {e}", p.display()))?;
    }
    let mut guard = state.streamer.lock().unwrap();
    match (guard.as_ref(), settings.enabled) {
        (Some(s), true) => {
            if *s.settings.lock().unwrap() != settings {
                s.reconfigure(settings);
            }
        }
        (Some(s), false) => {
            s.stop();
            *guard = None;
        }
        (None, true) => {
            if crate::encode::ffmpeg_available().is_none() {
                return Err("ffmpeg is required for streaming (brew install ffmpeg)".into());
            }
            *guard = Some(start(settings));
        }
        (None, false) => {}
    }
    Ok(())
}

/// Start the feed at launch if it was left enabled.
pub fn autostart(app: &AppHandle) {
    let s = load_settings(app);
    if s.enabled && crate::encode::ffmpeg_available().is_some() {
        *app.state::<AppState>().streamer.lock().unwrap() = Some(start(s));
    }
}
