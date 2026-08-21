//! Radio-ID (unit) aliases: a local table, editable from the UI, importable
//! from a CSV in trunk-recorder's `unitTagsFile` shape (`id, name`).
//! RadioReference's API has no unit roster, so this is user data, kept as
//! JSON in the app's config directory.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Manager, State};

use crate::AppState;

pub type Units = Arc<Mutex<HashMap<u32, String>>>;

fn path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let d = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("config dir: {e}"))?;
    std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    Ok(d.join("units.json"))
}

pub fn load(app: &AppHandle) -> HashMap<u32, String> {
    path(app)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str::<HashMap<u32, String>>(&t).ok())
        .unwrap_or_default()
}

fn store(app: &AppHandle, m: &HashMap<u32, String>) -> Result<(), String> {
    let p = path(app)?;
    std::fs::write(
        &p,
        serde_json::to_string_pretty(m).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("{}: {e}", p.display()))
}

#[derive(Serialize, Deserialize)]
pub struct UnitRow {
    pub id: u32,
    pub name: String,
}

#[tauri::command]
pub fn units_list(state: State<AppState>) -> Vec<UnitRow> {
    let mut v: Vec<UnitRow> = state
        .units
        .lock()
        .unwrap()
        .iter()
        .map(|(id, name)| UnitRow {
            id: *id,
            name: name.clone(),
        })
        .collect();
    v.sort_by_key(|u| u.id);
    v
}

/// Set (or, with an empty name, remove) one alias.
#[tauri::command]
pub fn unit_set(
    app: AppHandle,
    state: State<AppState>,
    id: u32,
    name: String,
) -> Result<usize, String> {
    let mut m = state.units.lock().unwrap();
    if name.trim().is_empty() {
        m.remove(&id);
    } else {
        m.insert(id, name.trim().to_string());
    }
    store(&app, &m)?;
    Ok(m.len())
}

/// Parse `id,name` lines (header and regex rows skipped); returns the rows.
pub fn parse_csv(text: &str) -> Vec<(u32, String)> {
    text.lines()
        .filter_map(|l| {
            let mut it = l.splitn(2, ',');
            let id = it.next()?.trim().trim_matches('"');
            let name = it.next()?.trim().trim_matches('"');
            let id: u32 = id.parse().ok()?;
            (!name.is_empty()).then(|| (id, name.to_string()))
        })
        .collect()
}

/// Import a CSV (`Unit ID, Name`, trunk-recorder unitTagsFile shape); merges
/// into the table. Returns the total after import.
#[tauri::command]
pub fn units_import(app: AppHandle, state: State<AppState>, path: String) -> Result<usize, String> {
    let text = std::fs::read_to_string(crate::shellexpand_home(&path))
        .map_err(|e| format!("{path}: {e}"))?;
    let rows = parse_csv(&text);
    if rows.is_empty() {
        return Err("no `id,name` rows found".into());
    }
    let mut m = state.units.lock().unwrap();
    m.extend(rows);
    store(&app, &m)?;
    Ok(m.len())
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_trunk_recorder_unit_tags() {
        let rows = super::parse_csv(
            "Unit ID,Unit Tag\n4900165,\"Car 12\"\n^49001(\\d\\d)$,Fleet $1\n790062,Engine 3\n",
        );
        assert_eq!(
            rows,
            vec![(4900165, "Car 12".into()), (790062, "Engine 3".into())]
        );
    }
}
