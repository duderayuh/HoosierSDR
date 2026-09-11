//! Up/down announcements: a Telegram message when the app starts and when
//! it shuts down, so the listener can see from the chat whether the scanner
//! is running.
//!
//! A process that is killed outright (a crash, `kill -9`, power loss, or the
//! `cargo tauri dev` watcher restarting it) cannot say goodbye. A heartbeat
//! file covers that: it records when this run started and was last alive,
//! and whether it shut down cleanly. The next start reads it and says in its
//! "up" message that the previous run ended without shutting down, and when
//! it was last seen.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;
use tauri::{AppHandle, Manager};

use crate::alerts::Telegram;
use crate::AppState;

/// How often the heartbeat file is refreshed.
const BEAT_SECS: u64 = 60;
/// Waits between attempts at the "up" message: at login the network may not
/// be there yet.
const UP_RETRY_SECS: [u64; 5] = [5, 15, 30, 60, 120];
/// The "down" message is sent while the app is exiting; do not hold the
/// exit up for long.
const DOWN_TIMEOUT_SECS: u64 = 5;

static STARTED: AtomicI64 = AtomicI64::new(0);
static DOWN_SENT: AtomicBool = AtomicBool::new(false);

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct Beat {
    /// Epoch seconds this run started.
    pub started: i64,
    /// Epoch seconds this run was last known alive.
    pub last_seen: i64,
    /// Set when the run shut down through `on_exit`.
    pub clean: bool,
}

fn beat_path(app: &AppHandle) -> Option<PathBuf> {
    let d = app.path().app_config_dir().ok()?;
    std::fs::create_dir_all(&d).ok()?;
    Some(d.join("heartbeat.json"))
}

fn read_beat(p: &Path) -> Option<Beat> {
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

fn write_beat(p: &Path, b: &Beat) {
    if let Ok(t) = serde_json::to_string(b) {
        let _ = std::fs::write(p, t);
    }
}

/// Where status messages go, or None when they are off or no chat is set.
fn destination(tg: &Telegram) -> Option<String> {
    if !tg.announce {
        return None;
    }
    let dest = if tg.announce_chat.trim().is_empty() {
        if tg.chat_id.trim().is_empty() {
            return None;
        }
        tg.destination()
    } else {
        tg.announce_chat.trim().to_string()
    };
    (!crate::alerts::chat_parts(&dest).0.is_empty()).then_some(dest)
}

/// `3 h 12 min`, `4 min`, `40 s`.
pub fn fmt_span(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s} s")
    } else if s < 3600 {
        format!("{} min", s / 60)
    } else if s < 86_400 {
        format!("{} h {} min", s / 3600, (s % 3600) / 60)
    } else {
        format!("{} d {} h", s / 86_400, (s % 86_400) / 3600)
    }
}

/// The "up" message. `prev` is the heartbeat the last run left behind.
pub fn up_message(host: &str, version: &str, prev: Option<Beat>, now: i64) -> String {
    let mut m = format!("🟢 HoosierSDR is up on {host} · v{version}");
    if let Some(p) = prev.filter(|p| !p.clean && p.last_seen > 0) {
        m.push_str(&format!(
            "\n⚠️ The previous run stopped without shutting down (crash, force quit, \
             power loss or a dev rebuild). Last seen {} ago, after {} up.",
            fmt_span(now - p.last_seen),
            fmt_span(p.last_seen - p.started),
        ));
    }
    m
}

/// The "down" message.
pub fn down_message(host: &str, uptime_secs: i64) -> String {
    format!(
        "🔴 HoosierSDR is shutting down on {host} · up {}",
        fmt_span(uptime_secs)
    )
}

fn host() -> String {
    let h = crate::library::hostname();
    h.strip_suffix(".local").unwrap_or(&h).to_string()
}

/// Called from `setup`: start the heartbeat, announce the start, and turn
/// SIGINT/SIGTERM into a normal exit so they announce the stop.
pub fn on_start(app: &AppHandle) {
    let now = crate::library::now();
    STARTED.store(now, Ordering::SeqCst);
    let path = beat_path(app);
    let prev = path.as_deref().and_then(read_beat);
    if let Some(p) = path.clone() {
        write_beat(
            &p,
            &Beat {
                started: now,
                last_seen: now,
                clean: false,
            },
        );
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(BEAT_SECS));
            if DOWN_SENT.load(Ordering::SeqCst) {
                return;
            }
            write_beat(
                &p,
                &Beat {
                    started: now,
                    last_seen: crate::library::now(),
                    clean: false,
                },
            );
        });
    }

    let version = app.package_info().version.to_string();
    let handle = app.clone();
    std::thread::spawn(move || {
        let (tg, _) = crate::alerts::shared_settings(&handle.state::<AppState>());
        let Some(dest) = destination(&tg) else {
            return;
        };
        let text = up_message(&host(), &version, prev, crate::library::now());
        let mut last = String::new();
        for wait in std::iter::once(0).chain(UP_RETRY_SECS) {
            std::thread::sleep(Duration::from_secs(wait));
            if DOWN_SENT.load(Ordering::SeqCst) {
                return;
            }
            match crate::alerts::send_text(&dest, &text, 30) {
                Ok(_) => {
                    eprintln!("status: up message sent");
                    return;
                }
                Err(e) => last = e,
            }
        }
        eprintln!("status: up message not sent: {last}");
    });

    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};
        // SIGHUP is left alone: an app started under `nohup` must outlive
        // its terminal.
        let (Ok(mut term), Ok(mut int)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) else {
            return;
        };
        async fn either(a: &mut tokio::signal::unix::Signal, b: &mut tokio::signal::unix::Signal) {
            tokio::select! {
                _ = a.recv() => {}
                _ = b.recv() => {}
            }
        }
        either(&mut term, &mut int).await;
        eprintln!("status: signal received, shutting down");
        handle.exit(0);
        // A second signal means "now": do not let a stuck send hold it up.
        either(&mut term, &mut int).await;
        std::process::exit(130);
    });
}

/// Called once the app is exiting (Cmd-Q, last window closed, a signal):
/// mark the heartbeat clean and send the "down" message, briefly.
pub fn on_exit(app: &AppHandle) {
    if DOWN_SENT.swap(true, Ordering::SeqCst) {
        return;
    }
    let now = crate::library::now();
    let started = STARTED.load(Ordering::SeqCst);
    if let Some(p) = beat_path(app) {
        write_beat(
            &p,
            &Beat {
                started,
                last_seen: now,
                clean: true,
            },
        );
    }
    let (tg, _) = crate::alerts::shared_settings(&app.state::<AppState>());
    let Some(dest) = destination(&tg) else {
        return;
    };
    match crate::alerts::send_text(
        &dest,
        &down_message(&host(), now - started),
        DOWN_TIMEOUT_SECS,
    ) {
        Ok(_) => eprintln!("status: down message sent"),
        Err(e) => eprintln!("status: down message not sent: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans() {
        assert_eq!(fmt_span(40), "40 s");
        assert_eq!(fmt_span(4 * 60 + 59), "4 min");
        assert_eq!(fmt_span(3 * 3600 + 12 * 60), "3 h 12 min");
        assert_eq!(fmt_span(2 * 86_400 + 5 * 3600), "2 d 5 h");
        assert_eq!(fmt_span(-5), "0 s");
    }

    #[test]
    fn up_mentions_an_unclean_previous_run_only() {
        let now = 1_000_000;
        let plain = up_message("studio", "0.1.0", None, now);
        assert_eq!(plain, "🟢 HoosierSDR is up on studio · v0.1.0");
        let clean = Beat {
            started: now - 7200,
            last_seen: now - 600,
            clean: true,
        };
        assert_eq!(up_message("studio", "0.1.0", Some(clean), now), plain);
        let crashed = Beat {
            clean: false,
            ..clean
        };
        let m = up_message("studio", "0.1.0", Some(crashed), now);
        assert!(m.starts_with(&plain));
        assert!(
            m.contains("Last seen 10 min ago, after 1 h 50 min up."),
            "{m}"
        );
    }

    #[test]
    fn down_carries_uptime() {
        assert_eq!(
            down_message("studio", 3 * 3600 + 5 * 60),
            "🔴 HoosierSDR is shutting down on studio · up 3 h 5 min"
        );
    }

    #[test]
    fn destination_needs_the_switch_and_a_chat() {
        let mut tg = Telegram {
            chat_id: "123".into(),
            topic_id: "7".into(),
            announce: false,
            announce_chat: String::new(),
        };
        assert_eq!(destination(&tg), None);
        tg.announce = true;
        assert_eq!(destination(&tg).as_deref(), Some("123:7"));
        tg.announce_chat = " -100555:9 ".into();
        assert_eq!(destination(&tg).as_deref(), Some("-100555:9"));
        tg.announce_chat.clear();
        tg.chat_id.clear();
        assert_eq!(destination(&tg), None);
    }

    #[test]
    fn beat_round_trips() {
        let dir = std::env::temp_dir().join(format!("hs-beat-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("heartbeat.json");
        assert_eq!(read_beat(&p), None);
        let b = Beat {
            started: 10,
            last_seen: 70,
            clean: false,
        };
        write_beat(&p, &b);
        assert_eq!(read_beat(&p), Some(b));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
