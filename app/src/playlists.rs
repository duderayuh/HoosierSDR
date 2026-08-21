//! Playlists: a saved choice of system, site and talkgroups that tunes the
//! receiver and restricts the feed in one click. User data, kept as JSON in
//! the app's config directory.

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

use crate::AppState;

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    pub sid: u32,
    pub system_name: String,
    pub site_id: u32,
    pub site_name: String,
    pub nac: Option<u16>,
    pub control_mhz: f64,
    pub center_mhz: f64,
    /// Sample rate that covers the site's span.
    pub rate: f64,
    /// Talkgroups to follow; empty means every talkgroup on the system.
    pub tgs: Vec<u16>,
}

fn path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let d = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("config dir: {e}"))?;
    std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    Ok(d.join("playlists.json"))
}

fn load(app: &AppHandle) -> Vec<Playlist> {
    path(app)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn store(app: &AppHandle, v: &[Playlist]) -> Result<(), String> {
    let p = path(app)?;
    std::fs::write(
        &p,
        serde_json::to_string_pretty(v).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("{}: {e}", p.display()))
}

#[tauri::command]
pub fn playlists_list(app: AppHandle) -> Vec<Playlist> {
    load(&app)
}

/// Create or update (by id) a playlist; returns the stored list.
#[tauri::command]
pub fn playlist_save(app: AppHandle, mut playlist: Playlist) -> Result<Vec<Playlist>, String> {
    if playlist.name.trim().is_empty() {
        return Err("give the playlist a name".into());
    }
    if playlist.id.is_empty() {
        playlist.id = format!(
            "{}-{}",
            playlist.sid,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        );
    }
    let mut all = load(&app);
    match all.iter_mut().find(|p| p.id == playlist.id) {
        Some(slot) => *slot = playlist,
        None => all.push(playlist),
    }
    store(&app, &all)?;
    Ok(all)
}

#[tauri::command]
pub fn playlist_delete(app: AppHandle, id: String) -> Result<Vec<Playlist>, String> {
    let mut all = load(&app);
    all.retain(|p| p.id != id);
    store(&app, &all)?;
    Ok(all)
}

/// Make a playlist current: its talkgroups become the follower's allowlist
/// (live, if following) and its tuning comes back for the UI to apply.
/// `None` clears the restriction.
#[tauri::command]
pub fn playlist_activate(
    app: AppHandle,
    state: State<AppState>,
    id: Option<String>,
) -> Result<Option<Playlist>, String> {
    let pick = id.and_then(|id| load(&app).into_iter().find(|p| p.id == id));
    *state.allowlist.lock().unwrap() = pick
        .as_ref()
        .filter(|p| !p.tgs.is_empty())
        .map(|p| p.tgs.iter().copied().collect());
    Ok(pick)
}
