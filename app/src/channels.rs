//! Channels: what the talkgroup picker needs from the backend — how busy
//! each talkgroup has been lately, and the named sets of talkgroups the
//! listener saves ("Hospitals", "Fire dispatch") to reuse across rules.
//!
//! Sets live here rather than in the page's local storage because rules
//! run in the backend and the phone edits them too.

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

use crate::AppState;

/// Calls heard on one talkgroup in a window.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Activity {
    pub tg: u16,
    pub system: String,
    pub calls: u32,
    pub transcribed: u32,
    pub last_at: i64,
}

pub fn activity(c: &Connection, since: i64) -> Result<Vec<Activity>, String> {
    let mut st = c
        .prepare(
            "SELECT tg, system, COUNT(*), SUM(transcript IS NOT NULL AND transcript <> ''), MAX(start)
             FROM calls WHERE start >= ?1 GROUP BY tg, system ORDER BY COUNT(*) DESC",
        )
        .map_err(|e| format!("activity: {e}"))?;
    let rows = st
        .query_map(params![since], |r| {
            Ok(Activity {
                tg: r.get::<_, i64>(0)? as u16,
                system: r.get(1)?,
                calls: r.get::<_, i64>(2)? as u32,
                transcribed: r.get::<_, i64>(3)? as u32,
                last_at: r.get(4)?,
            })
        })
        .map_err(|e| format!("activity: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("activity: {e}"))
}

/// A named set of talkgroups.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ChannelSet {
    pub id: String,
    pub name: String,
    pub tgs: Vec<u16>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Settings {
    #[serde(default)]
    pub sets: Vec<ChannelSet>,
}

fn path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let d = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("config dir: {e}"))?;
    std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    Ok(d.join("channel_sets.json"))
}

pub fn load(app: &AppHandle) -> Settings {
    path(app)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Clean what the page (or phone) sends: names, ids, sorted unique TGs.
pub fn sanitize(s: &mut Settings) {
    s.sets.truncate(200);
    let mut ids = std::collections::HashSet::new();
    for (i, set) in s.sets.iter_mut().enumerate() {
        set.name = crate::analyzers::clean_line(&set.name, 60);
        if set.name.is_empty() {
            set.name = format!("Set {}", i + 1);
        }
        set.id = crate::analyzers::clean_line(&set.id, 40);
        if set.id.is_empty()
            || !set
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            || !ids.insert(set.id.clone())
        {
            set.id = format!("s{}-{i}", crate::library::now());
            ids.insert(set.id.clone());
        }
        set.tgs.sort_unstable();
        set.tgs.dedup();
        set.tgs.truncate(2000);
    }
}

#[tauri::command]
pub fn channel_activity(
    state: State<AppState>,
    hours: Option<u32>,
) -> Result<Vec<Activity>, String> {
    let db = state
        .db
        .lock()
        .unwrap()
        .clone()
        .ok_or("the call library is not open")?;
    let c = db.lock().unwrap();
    let since = crate::library::now() - hours.unwrap_or(24).clamp(1, 24 * 90) as i64 * 3600;
    activity(&c, since)
}

#[tauri::command]
pub fn channel_sets_get(app: AppHandle) -> Settings {
    load(&app)
}

#[tauri::command]
pub fn channel_sets_set(app: AppHandle, settings: Settings) -> Result<Settings, String> {
    let mut settings = settings;
    sanitize(&mut settings);
    let p = path(&app)?;
    std::fs::write(
        &p,
        serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("{}: {e}", p.display()))?;
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_counts_calls_per_talkgroup_in_the_window() {
        let d = std::env::temp_dir().join(format!("hs_chan_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let c = crate::library::open(&d).unwrap();
        for (tg, start, tr) in [
            (1001u16, 100i64, true),
            (1001, 200, false),
            (1002, 150, true),
            (1003, 10, true),
        ] {
            let id = crate::library::insert(
                &c,
                &crate::library::CallRow {
                    start,
                    tg,
                    tg_name: format!("TG {tg}"),
                    system: "Test".into(),
                    ..Default::default()
                },
            )
            .unwrap();
            if tr {
                crate::library::set_transcript(&c, id, "hello", "test").unwrap();
            }
        }
        let a = activity(&c, 50).unwrap();
        assert_eq!(a.len(), 2, "1003 is before the window");
        assert_eq!(
            (a[0].tg, a[0].calls, a[0].transcribed, a[0].last_at),
            (1001, 2, 1, 200)
        );
        assert_eq!((a[1].tg, a[1].calls), (1002, 1));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn sets_are_tidied() {
        let mut s = Settings {
            sets: vec![
                ChannelSet {
                    id: "a".into(),
                    name: " Hospitals ".into(),
                    tgs: vec![3, 1, 3, 2],
                },
                ChannelSet {
                    id: "a".into(),
                    name: "".into(),
                    tgs: vec![],
                },
            ],
        };
        sanitize(&mut s);
        assert_eq!(s.sets[0].name, "Hospitals");
        assert_eq!(s.sets[0].tgs, vec![1, 2, 3]);
        assert_eq!(s.sets[1].name, "Set 2");
        assert_ne!(s.sets[0].id, s.sets[1].id);
    }
}
