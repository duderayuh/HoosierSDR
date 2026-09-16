//! The settings a page keeps that steer the radio itself, shared between
//! the window on this machine and any page open over remote access.
//!
//! The page keeps its settings in the browser's own storage, and a page
//! opened from another computer has a browser of its own: it arrived with
//! no listen groups, pushed that emptiness to the radio on load, and so
//! switched off every group the listener had set up here. What steers the
//! radio lives here too, now — each page writes through it and hears every
//! change the other makes, so a remote page shows the groups this machine
//! has and switches them for it.
//!
//! Only the settings the radio acts on are shared. How a page looks —
//! theme, fonts, which panels are open, its own volume — stays with it.

use serde_json::Value;
use std::collections::BTreeMap;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::AppState;

/// The page's storage keys that are shared (`app.js` keeps the same list).
pub const SYNCED: &[&str] = &["hs.groups", "hs.grouprec", "hs.policy", "hs.lockout", "hs.prio", "hs.tgrules"];

/// A timed avoid steers the radio like a lockout, and goes out with every
/// lockout: a page that did not know the other's avoids would lift them.
/// Its key names the playlist, so it is matched by its start.
const AVOIDS: &str = "hs.avoid";

fn shared(key: &str) -> bool {
    SYNCED.contains(&key) || key == AVOIDS || key.starts_with("hs.avoid.")
}

/// A listen-group list is a few kilobytes; a value far past that is not one.
const MAX_BYTES: usize = 512 * 1024;

/// Loaded from disk on first use.
pub type Shared = std::sync::Mutex<Option<BTreeMap<String, Value>>>;

fn path(app: &AppHandle) -> Option<std::path::PathBuf> {
    app.path().app_config_dir().ok().map(|d| d.join("uistate.json"))
}

fn with<R>(app: &AppHandle, state: &AppState, f: impl FnOnce(&mut BTreeMap<String, Value>) -> R) -> R {
    let mut g = state.ui_state.lock().unwrap();
    let map = g.get_or_insert_with(|| {
        path(app)
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    });
    f(map)
}

/// Take one setting. `Ok(true)` when it changed; an unchanged value is not
/// news, which is also what stops two pages answering each other forever.
pub fn apply(map: &mut BTreeMap<String, Value>, key: &str, value: Value) -> Result<bool, String> {
    if !shared(key) {
        return Err(format!("{key} is not a shared setting"));
    }
    if serde_json::to_string(&value).map(|s| s.len()).unwrap_or(usize::MAX) > MAX_BYTES {
        return Err(format!("{key} is too large"));
    }
    if map.get(key) == Some(&value) {
        return Ok(false);
    }
    map.insert(key.to_string(), value);
    Ok(true)
}

#[tauri::command]
pub fn ui_state_get(app: AppHandle, state: State<AppState>) -> BTreeMap<String, Value> {
    with(&app, &state, |m| m.clone())
}

/// One page changed a shared setting: keep it, and tell every page.
/// `origin` names the page, so it can ignore its own change coming back.
#[tauri::command]
pub fn ui_state_set(app: AppHandle, state: State<AppState>, key: String, value: Value, origin: String) -> Result<(), String> {
    let changed = with(&app, &state, |m| {
        let kept = m.get(&key).cloned();
        let changed = apply(m, &key, value.clone())?;
        if changed {
            if let Some(p) = path(&app) {
                if let Some(d) = p.parent() {
                    let _ = std::fs::create_dir_all(d);
                }
                let written = serde_json::to_string_pretty(m)
                    .map_err(|e| e.to_string())
                    .and_then(|t| std::fs::write(&p, t).map_err(|e| format!("{}: {e}", p.display())));
                // Kept only if it was written. Otherwise no page is told,
                // while this one quietly holds the new value — and sending
                // it again would count as no change at all.
                if let Err(e) = written {
                    match kept {
                        Some(v) => m.insert(key.clone(), v),
                        None => m.remove(&key),
                    };
                    return Err(e);
                }
            }
        }
        Ok::<_, String>(changed)
    })?;
    if changed {
        let _ = app.emit("ui_state", serde_json::json!({ "key": key, "value": value, "origin": origin }));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shared_setting_is_news_once() {
        let mut m = BTreeMap::new();
        let groups = serde_json::json!([{ "id": "g1", "name": "Hospitals", "tgs": [10255], "listen": true }]);
        assert_eq!(apply(&mut m, "hs.groups", groups.clone()), Ok(true));
        assert_eq!(apply(&mut m, "hs.groups", groups), Ok(false), "the same value again is not a change");
        let off = serde_json::json!([{ "id": "g1", "name": "Hospitals", "tgs": [10255], "listen": false }]);
        assert_eq!(apply(&mut m, "hs.groups", off.clone()), Ok(true));
        assert_eq!(m.get("hs.groups"), Some(&off));
    }

    #[test]
    fn only_what_steers_the_radio_is_shared() {
        let mut m = BTreeMap::new();
        assert!(apply(&mut m, "hs.theme", serde_json::json!("dark")).is_err());
        assert_eq!(apply(&mut m, "hs.avoid.pl-1", serde_json::json!({ "10255": 1_780_000_000_i64 })), Ok(true), "a timed avoid steers the radio");
        assert!(apply(&mut m, "hs.volume", serde_json::json!(40)).is_err(), "a page's own volume stays with it");
        let huge = Value::String("x".repeat(MAX_BYTES + 1));
        assert!(apply(&mut m, "hs.policy", huge).is_err());
        assert_eq!(m.len(), 1, "only the avoid was kept");
    }
}
