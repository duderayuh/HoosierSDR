//! Playlists and sites.
//!
//! A *playlist* is one system's talkgroup selection together with the
//! listening choices made on it: locked-out talkgroups and priorities. A
//! *site* is what the radio tunes (control channel, band centre, rate) and
//! names the playlist it follows. Several sites of one system — a simulcast
//! per county — share one playlist, so a talkgroup locked out on it is
//! locked on all of them; another system reusing the same talkgroup number
//! has its own playlist and is untouched.
//!
//! Both are user data, kept as JSON in the app's config directory
//! (`playlists.json`, `sites.json`). Earlier versions kept one file whose
//! entries were a site and a talkgroup selection in one; the first load
//! splits such a file into the two (backed up as `playlists.v1.json`).

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Manager, State};

use crate::AppState;

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    pub sid: u32,
    pub system_name: String,
    /// Talkgroups to follow; empty means every talkgroup on the system.
    pub tgs: Vec<u16>,
    /// Talkgroups locked out on this playlist (every site following it).
    #[serde(default)]
    pub lockout: Vec<u16>,
    /// Talkgroup priorities, 1 (highest) … 99; absent = 50.
    #[serde(default)]
    pub priorities: Vec<(u16, u8)>,

    // Fields of the one-file format, read for migration only.
    #[serde(default, skip_serializing)]
    site_id: u32,
    #[serde(default, skip_serializing)]
    site_name: String,
    #[serde(default, skip_serializing)]
    nac: Option<u16>,
    #[serde(default, skip_serializing)]
    control_mhz: f64,
    #[serde(default, skip_serializing)]
    center_mhz: f64,
    #[serde(default, skip_serializing)]
    rate: f64,
    #[serde(default, skip_serializing)]
    span_mhz: Option<(f64, f64)>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Site {
    pub id: String,
    /// The listener's label ("MESA System 1"); the top bar and run chips.
    pub name: String,
    pub sid: u32,
    pub system_name: String,
    pub site_id: u32,
    /// RadioReference's name for the site.
    pub site_name: String,
    pub nac: Option<u16>,
    pub control_mhz: f64,
    pub center_mhz: f64,
    /// Sample rate that covers the site's span.
    pub rate: f64,
    /// The site's frequency span (MHz), so extra radios can be planned to
    /// cover what the primary cannot.
    #[serde(default)]
    pub span_mhz: Option<(f64, f64)>,
    /// The playlist followed from this site; `None` = every talkgroup, with
    /// the unscoped lockout and priorities.
    #[serde(default)]
    pub playlist: Option<String>,
}

/// What the follower reads live for one playlist (or, under the empty key,
/// for runs started without one). Sites sharing a playlist share one of
/// these, so a change reaches every run at once.
#[derive(Default, Clone, Debug, PartialEq)]
pub struct Filters {
    pub lockout: HashSet<u16>,
    pub lockout_ranges: Vec<(u16, u16)>,
    /// Talkgroups to follow (`None` = all): the playlist's, unless the UI
    /// narrowed it further (service-tag filter).
    pub allowlist: Option<HashSet<u16>>,
    /// Hold: follow only this talkgroup until released.
    pub hold: Option<u16>,
    pub priorities: HashMap<u16, u8>,
    pub priority_ranges: Vec<(u16, u16, u8)>,
}

pub type FiltersRef = Arc<Mutex<Filters>>;
/// Playlist id → live filters; `""` is the unscoped set.
pub type FilterTable = Mutex<HashMap<String, FiltersRef>>;

fn config_dir(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let d = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("config dir: {e}"))?;
    std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    Ok(d)
}

fn read_json<T: serde::de::DeserializeOwned + Default>(p: &std::path::Path) -> T {
    std::fs::read_to_string(p)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn write_json<T: Serialize>(p: &std::path::Path, v: &T) -> Result<(), String> {
    std::fs::write(
        p,
        serde_json::to_string_pretty(v).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("{}: {e}", p.display()))
}

/// Split a one-file playlist list (each entry a site with its own talkgroup
/// selection) into sites and playlists: one site per entry, keeping its id
/// so saved preferences still resolve, and one playlist per distinct
/// (system, talkgroup set), so a system's simulcast sites end up sharing.
pub fn split_legacy(old: &[Playlist]) -> (Vec<Playlist>, Vec<Site>) {
    let mut playlists: Vec<Playlist> = Vec::new();
    let mut sites = Vec::new();
    // (sid, tgs) → index into `playlists`, plus how many entries share it.
    let mut groups: HashMap<(u32, Vec<u16>), usize> = HashMap::new();
    let mut per_system: HashMap<u32, usize> = HashMap::new();
    for e in old {
        let mut tgs = e.tgs.clone();
        tgs.sort_unstable();
        tgs.dedup();
        let idx = *groups.entry((e.sid, tgs.clone())).or_insert_with(|| {
            let n = per_system.entry(e.sid).or_insert(0);
            *n += 1;
            playlists.push(Playlist {
                id: format!("pl-{}-{}", e.sid, n),
                name: e.name.clone(),
                sid: e.sid,
                system_name: e.system_name.clone(),
                tgs,
                lockout: e.lockout.clone(),
                priorities: e.priorities.clone(),
                ..Default::default()
            });
            playlists.len() - 1
        });
        sites.push(Site {
            id: e.id.clone(),
            name: e.name.clone(),
            sid: e.sid,
            system_name: e.system_name.clone(),
            site_id: e.site_id,
            site_name: e.site_name.clone(),
            nac: e.nac,
            control_mhz: e.control_mhz,
            center_mhz: e.center_mhz,
            rate: e.rate,
            span_mhz: e.span_mhz,
            playlist: Some(playlists[idx].id.clone()),
        });
    }
    // A playlist several sites share is the system's, not one site's: name
    // it after the system (its acronym when the name carries one).
    for p in playlists.iter_mut() {
        let users = sites
            .iter()
            .filter(|s| s.playlist.as_deref() == Some(&p.id))
            .count();
        if users > 1 {
            p.name = system_short_name(&p.system_name);
        }
    }
    (playlists, sites)
}

/// "Metropolitan Emergency Services Agency (MESA) (Formerly IDPS)" → "MESA".
pub fn system_short_name(name: &str) -> String {
    let mut rest = name;
    while let Some(i) = rest.find('(') {
        let after = &rest[i + 1..];
        if let Some(j) = after.find(')') {
            let inner = after[..j].trim();
            if (2..=8).contains(&inner.len())
                && inner
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '-')
            {
                return inner.to_string();
            }
            rest = &after[j + 1..];
        } else {
            break;
        }
    }
    let t = name.trim();
    if t.is_empty() {
        "Playlist".into()
    } else {
        t.to_string()
    }
}

static MIGRATED: std::sync::Once = std::sync::Once::new();

/// Convert a one-file `playlists.json` into `playlists.json` + `sites.json`
/// the first time this process looks at either.
fn ensure_migrated(app: &AppHandle) {
    MIGRATED.call_once(|| {
        let Ok(d) = config_dir(app) else { return };
        let pl_path = d.join("playlists.json");
        if d.join("sites.json").exists() {
            return;
        }
        let old: Vec<Playlist> = read_json(&pl_path);
        if old.is_empty() || !old.iter().any(|p| p.control_mhz > 0.0) {
            return;
        }
        let (playlists, sites) = split_legacy(&old);
        if !d.join("playlists.v1.json").exists() {
            let _ = std::fs::copy(&pl_path, d.join("playlists.v1.json"));
        }
        if write_json(&d.join("sites.json"), &sites).is_ok() {
            let _ = write_json(&pl_path, &playlists);
            eprintln!(
                "[playlists] split {} one-file playlists into {} playlists and {} sites",
                old.len(),
                playlists.len(),
                sites.len()
            );
        }
    });
}

pub fn load(app: &AppHandle) -> Vec<Playlist> {
    ensure_migrated(app);
    config_dir(app)
        .map(|d| read_json(&d.join("playlists.json")))
        .unwrap_or_default()
}

fn store(app: &AppHandle, v: &[Playlist]) -> Result<(), String> {
    write_json(&config_dir(app)?.join("playlists.json"), &v)
}

pub fn load_sites(app: &AppHandle) -> Vec<Site> {
    ensure_migrated(app);
    config_dir(app)
        .map(|d| read_json(&d.join("sites.json")))
        .unwrap_or_default()
}

fn store_sites(app: &AppHandle, v: &[Site]) -> Result<(), String> {
    write_json(&config_dir(app)?.join("sites.json"), &v)
}

/// The unscoped lockout and priorities (runs started without a playlist),
/// kept in `filters.json`.
#[derive(Serialize, Deserialize, Default)]
struct Unscoped {
    lockout: Vec<u16>,
    priorities: Vec<(u16, u8)>,
}

fn unscoped_path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    Ok(config_dir(app)?.join("filters.json"))
}

/// The RadioReference system a control channel belongs to, from the saved
/// sites — so a run started by hand on a site's control channel still
/// names talkgroups from the right system.
pub fn sid_for_control(app: &AppHandle, control_hz: f64) -> Option<u32> {
    load_sites(app)
        .into_iter()
        .find(|s| (s.control_mhz * 1e6 - control_hz).abs() < 1.0)
        .map(|s| s.sid)
}

/// System name (as written into library rows and sidecars) → system id,
/// for rows recorded before the id was stored.
pub fn sids_by_system_name(app: &AppHandle) -> HashMap<String, u32> {
    let mut m: HashMap<String, u32> = load(app)
        .into_iter()
        .filter(|p| !p.system_name.trim().is_empty())
        .map(|p| (p.system_name, p.sid))
        .collect();
    for s in load_sites(app) {
        if !s.system_name.trim().is_empty() {
            m.entry(s.system_name).or_insert(s.sid);
        }
    }
    m
}

/// The live filters for a playlist (`None` = the unscoped set), created on
/// first use from what is saved.
pub fn filters_for(app: &AppHandle, state: &AppState, playlist: Option<&str>) -> FiltersRef {
    let key = playlist.unwrap_or("").to_string();
    let mut table = state.filters.lock().unwrap();
    if let Some(f) = table.get(&key) {
        return f.clone();
    }
    let mut f = Filters::default();
    // Ranges are global: every set carries the same ones.
    if let Some(d) = table.get("") {
        let d = d.lock().unwrap();
        f.lockout_ranges = d.lockout_ranges.clone();
        f.priority_ranges = d.priority_ranges.clone();
    }
    if key.is_empty() {
        let u: Unscoped = unscoped_path(app)
            .map(|p| read_json(&p))
            .unwrap_or_default();
        f.lockout = u.lockout.into_iter().collect();
        f.priorities = u.priorities.into_iter().collect();
    } else if let Some(p) = load(app).into_iter().find(|p| p.id == key) {
        f.lockout = p.lockout.iter().copied().collect();
        f.priorities = p.priorities.iter().copied().collect();
        f.allowlist = (!p.tgs.is_empty()).then(|| p.tgs.iter().copied().collect());
    }
    let r = Arc::new(Mutex::new(f));
    table.insert(key, r.clone());
    r
}

/// Write a playlist's lockout and priorities back to disk from its live set.
fn persist_filters(app: &AppHandle, playlist: Option<&str>, f: &Filters) -> Result<(), String> {
    let mut lockout: Vec<u16> = f.lockout.iter().copied().collect();
    lockout.sort_unstable();
    let mut priorities: Vec<(u16, u8)> = f.priorities.iter().map(|(t, p)| (*t, *p)).collect();
    priorities.sort_unstable();
    match playlist {
        None | Some("") => write_json(
            &unscoped_path(app)?,
            &Unscoped {
                lockout,
                priorities,
            },
        ),
        Some(id) => {
            let mut all = load(app);
            if let Some(p) = all.iter_mut().find(|p| p.id == id) {
                p.lockout = lockout;
                p.priorities = priorities;
                store(app, &all)?;
            }
            Ok(())
        }
    }
}

// ---------------- commands ----------------

#[tauri::command]
pub fn playlists_list(app: AppHandle) -> Vec<Playlist> {
    load(&app)
}

/// Create or update (by id) a playlist's name and talkgroups; a stored
/// playlist keeps its lockout and priorities. Returns the stored list.
#[tauri::command]
pub fn playlist_save(
    app: AppHandle,
    state: State<AppState>,
    mut playlist: Playlist,
) -> Result<Vec<Playlist>, String> {
    if playlist.name.trim().is_empty() {
        return Err("give the playlist a name".into());
    }
    playlist.tgs.sort_unstable();
    playlist.tgs.dedup();
    if playlist.id.is_empty() {
        playlist.id = format!(
            "pl-{}-{}",
            playlist.sid,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        );
    }
    let mut all = load(&app);
    match all.iter_mut().find(|p| p.id == playlist.id) {
        Some(slot) => {
            playlist.lockout = slot.lockout.clone();
            playlist.priorities = slot.priorities.clone();
            *slot = playlist.clone();
        }
        None => all.push(playlist.clone()),
    }
    store(&app, &all)?;
    // Runs on it follow the new selection at once.
    if let Some(f) = state.filters.lock().unwrap().get(&playlist.id) {
        f.lock().unwrap().allowlist =
            (!playlist.tgs.is_empty()).then(|| playlist.tgs.iter().copied().collect());
    }
    Ok(all)
}

/// Delete a playlist; sites following it fall back to every talkgroup.
#[tauri::command]
pub fn playlist_delete(
    app: AppHandle,
    state: State<AppState>,
    id: String,
) -> Result<Vec<Playlist>, String> {
    let mut all = load(&app);
    all.retain(|p| p.id != id);
    store(&app, &all)?;
    let mut sites = load_sites(&app);
    let mut changed = false;
    for s in sites.iter_mut() {
        if s.playlist.as_deref() == Some(&id) {
            s.playlist = None;
            changed = true;
        }
    }
    if changed {
        store_sites(&app, &sites)?;
    }
    state.filters.lock().unwrap().remove(&id);
    Ok(all)
}

#[tauri::command]
pub fn sites_list(app: AppHandle) -> Vec<Site> {
    load_sites(&app)
}

/// Create or update (by id) a site; returns the stored list.
#[tauri::command]
pub fn site_save(app: AppHandle, mut site: Site) -> Result<Vec<Site>, String> {
    if site.name.trim().is_empty() {
        site.name = if site.site_name.trim().is_empty() {
            format!("site {}", site.site_id)
        } else {
            site.site_name.clone()
        };
    }
    if site.control_mhz <= 0.0 {
        return Err("the site needs a control channel".into());
    }
    if site.id.is_empty() {
        site.id = format!(
            "{}-{}",
            site.sid,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        );
    }
    if let Some(pl) = site.playlist.as_deref().filter(|s| !s.is_empty()) {
        if !load(&app).iter().any(|p| p.id == pl) {
            return Err("no such playlist".into());
        }
    } else {
        site.playlist = None;
    }
    let mut all = load_sites(&app);
    match all.iter_mut().find(|s| s.id == site.id) {
        Some(slot) => *slot = site,
        None => all.push(site),
    }
    store_sites(&app, &all)?;
    Ok(all)
}

#[tauri::command]
pub fn site_delete(app: AppHandle, id: String) -> Result<Vec<Site>, String> {
    let mut all = load_sites(&app);
    all.retain(|s| s.id != id);
    store_sites(&app, &all)?;
    Ok(all)
}

/// Replace a playlist's locked-out talkgroups (`None` = the unscoped set):
/// `tgs` are the listener's lockouts, saved with the playlist; `extra` are
/// locked for now only (timed avoids, muted groups). Takes effect on the
/// follower's next block — a call of a newly locked talkgroup already up
/// is dropped.
#[tauri::command]
pub fn set_lockout(
    app: AppHandle,
    state: State<AppState>,
    tgs: Vec<u16>,
    playlist: Option<String>,
    extra: Option<Vec<u16>>,
) -> Result<(), String> {
    let f = filters_for(&app, &state, playlist.as_deref());
    let mut saved = f.lock().unwrap().clone();
    saved.lockout = tgs.iter().copied().collect();
    let mut live = saved.clone();
    live.lockout.extend(extra.unwrap_or_default());
    *f.lock().unwrap() = live;
    persist_filters(&app, playlist.as_deref(), &saved)
}

/// Replace a playlist's talkgroup priority table.
#[tauri::command]
pub fn set_priorities(
    app: AppHandle,
    state: State<AppState>,
    entries: Vec<(u16, u8)>,
    playlist: Option<String>,
) -> Result<(), String> {
    let f = filters_for(&app, &state, playlist.as_deref());
    let snapshot = {
        let mut g = f.lock().unwrap();
        g.priorities = entries.into_iter().collect();
        g.clone()
    };
    persist_filters(&app, playlist.as_deref(), &snapshot)
}

/// Hold on one talkgroup of a playlist (`None` releases). Every site
/// following that playlist narrows to it while set; other systems carry on.
#[tauri::command]
pub fn set_hold(app: AppHandle, state: State<AppState>, tg: Option<u16>, playlist: Option<String>) {
    let f = filters_for(&app, &state, playlist.as_deref());
    f.lock().unwrap().hold = tg;
}

/// Narrow a playlist's runs to these talkgroups for now (`None` = back to
/// the playlist's own selection). Not saved.
#[tauri::command]
pub fn set_allowlist(
    app: AppHandle,
    state: State<AppState>,
    tgs: Option<Vec<u16>>,
    playlist: Option<String>,
) {
    let f = filters_for(&app, &state, playlist.as_deref());
    let own: Option<HashSet<u16>> = playlist
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(|id| load(&app).into_iter().find(|p| p.id == id))
        .filter(|p| !p.tgs.is_empty())
        .map(|p| p.tgs.iter().copied().collect());
    f.lock().unwrap().allowlist = match tgs {
        Some(t) => Some(t.into_iter().collect()),
        None => own,
    };
}

/// Locked-out talkgroup ranges (inclusive), alongside every playlist's
/// explicit lockout. Ranges are global.
#[tauri::command]
pub fn set_lockout_ranges(app: AppHandle, state: State<AppState>, ranges: Vec<(u16, u16)>) {
    filters_for(&app, &state, None);
    for f in state.filters.lock().unwrap().values() {
        f.lock().unwrap().lockout_ranges = ranges.clone();
    }
}

/// Priority ranges (inclusive, 1 high … 99 low); explicit entries win.
#[tauri::command]
pub fn set_priority_ranges(app: AppHandle, state: State<AppState>, ranges: Vec<(u16, u16, u8)>) {
    filters_for(&app, &state, None);
    for f in state.filters.lock().unwrap().values() {
        f.lock().unwrap().priority_ranges = ranges.clone();
    }
}

/// Release every hold (the last run stopped).
pub fn clear_holds(state: &AppState) {
    for f in state.filters.lock().unwrap().values() {
        f.lock().unwrap().hold = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy(
        id: &str,
        name: &str,
        sid: u32,
        site_id: u32,
        control: f64,
        tgs: Vec<u16>,
    ) -> Playlist {
        Playlist {
            id: id.into(),
            name: name.into(),
            sid,
            system_name: "Metropolitan Emergency Services Agency (MESA) (Formerly IDPS)".into(),
            tgs,
            site_id,
            site_name: format!("Site {site_id}"),
            control_mhz: control,
            center_mhz: control - 0.2,
            rate: 10e6,
            ..Default::default()
        }
    }

    #[test]
    fn legacy_entries_split_into_sites_sharing_one_playlist() {
        let old = vec![
            legacy(
                "5737-1",
                "MESA System 1",
                5737,
                13059,
                857.6625,
                vec![3, 1, 2],
            ),
            legacy(
                "5737-2",
                "MESA System 2",
                5737,
                13102,
                852.1125,
                vec![1, 2, 3],
            ),
            legacy("5737-3", "MESA Fire only", 5737, 24437, 859.8125, vec![2]),
            {
                let mut p = legacy("8084-1", "SAFE-T", 8084, 28177, 859.7375, vec![1, 2, 3]);
                p.system_name = "Indiana Project Hoosier SAFE-T".into();
                p
            },
        ];
        let (pl, sites) = split_legacy(&old);
        assert_eq!(sites.len(), 4);
        assert_eq!(pl.len(), 3);
        // Same system, same selection → one playlist, named after the system.
        assert_eq!(sites[0].playlist, sites[1].playlist);
        let shared = pl
            .iter()
            .find(|p| Some(&p.id) == sites[0].playlist.as_ref())
            .unwrap();
        assert_eq!(shared.name, "MESA");
        assert_eq!(shared.tgs, vec![1, 2, 3]);
        // A different selection on the same system keeps its own name.
        let fire = pl
            .iter()
            .find(|p| Some(&p.id) == sites[2].playlist.as_ref())
            .unwrap();
        assert_eq!(fire.name, "MESA Fire only");
        // Another system never shares, even with the same talkgroup numbers.
        assert_ne!(sites[3].playlist, sites[0].playlist);
        // Sites keep their ids (saved preferences point at them) and tuning.
        assert_eq!(sites[0].id, "5737-1");
        assert_eq!(sites[0].control_mhz, 857.6625);
        assert_eq!(sites[0].name, "MESA System 1");
    }

    #[test]
    fn short_names() {
        assert_eq!(
            system_short_name("Metropolitan Emergency Services Agency (MESA) (Formerly IDPS)"),
            "MESA"
        );
        assert_eq!(
            system_short_name("Indiana Project Hoosier SAFE-T"),
            "Indiana Project Hoosier SAFE-T"
        );
        assert_eq!(system_short_name("Statewide (P25)"), "P25");
        assert_eq!(system_short_name(""), "Playlist");
    }

    #[test]
    fn migrated_playlists_serialise_without_site_fields() {
        let (pl, _) = split_legacy(&[legacy("a", "A", 1, 2, 851.0, vec![])]);
        let text = serde_json::to_string(&pl[0]).unwrap();
        assert!(!text.contains("control_mhz"), "{text}");
        assert!(text.contains("\"lockout\""));
    }
}
