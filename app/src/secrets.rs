//! File-backed secret store, replacing the OS keychain.
//!
//! The keychain was a poor fit for a frequently-rebuilt dev app: macOS ties
//! keychain access to the app's code signature, which changes on every dev
//! rebuild, so the user got re-prompted three or four times per build. Secrets
//! (the RadioReference password/app key and the Telegram bot token) now live
//! in a `secrets.json` in the app's config directory, written 0600 (owner
//! only) on Unix. This is plaintext on disk — the same trade-off as every
//! other app setting — but it never prompts.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use tauri::Manager;

static DIR: OnceLock<PathBuf> = OnceLock::new();

/// Called once at startup; sets the directory the store writes into.
pub fn init(app: &tauri::AppHandle) {
    if let Ok(d) = app.path().app_config_dir() {
        let _ = DIR.set(d);
    }
}

fn path() -> Option<PathBuf> {
    DIR.get().map(|d| d.join("secrets.json"))
}

fn load() -> HashMap<String, String> {
    path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write(p: &std::path::Path, map: &HashMap<String, String>) -> Result<(), String> {
    let s = serde_json::to_string_pretty(map).map_err(|e| e.to_string())?;
    std::fs::write(p, s).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub fn get(key: &str) -> Option<String> {
    load().get(key).cloned().filter(|v| !v.is_empty())
}

pub fn set(key: &str, value: &str) -> Result<(), String> {
    let p = path().ok_or("secret store not initialised")?;
    let mut map = load();
    map.insert(key.to_string(), value.to_string());
    write(&p, &map)
}

pub fn remove(key: &str) -> Result<(), String> {
    let p = path().ok_or("secret store not initialised")?;
    let mut map = load();
    map.remove(key);
    write(&p, &map)
}
