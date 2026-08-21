//! Radio-ID (unit) aliases: a local table, editable from the UI, importable
//! from a CSV in trunk-recorder's `unitTagsFile` shape (`id, name`), plus
//! wildcard rules — regular expressions over the decimal ID with `$1`-style
//! captures in the name, exactly the rows trunk-recorder's file allows
//! (`^49001(\d\d)$,Fleet $1`). RadioReference's API has no unit roster, so
//! this is user data, kept as JSON in the app's config directory.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Manager, State};

use crate::AppState;

pub type Units = Arc<Mutex<HashMap<u32, String>>>;

/// A wildcard rule: `pattern` is a regular expression matched against the
/// whole decimal radio ID; `name` may use `$1`… for captures.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub pattern: String,
    pub name: String,
}

pub type Rules = Arc<Mutex<Vec<Rule>>>;

fn rules_path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    Ok(path(app)?.with_file_name("unit_rules.json"))
}

pub fn load_rules(app: &AppHandle) -> Vec<Rule> {
    rules_path(app)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn store_rules(app: &AppHandle, r: &[Rule]) -> Result<(), String> {
    let p = rules_path(app)?;
    std::fs::write(
        &p,
        serde_json::to_string_pretty(r).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("{}: {e}", p.display()))
}

/// Name a radio from the rules: the first whose pattern matches the whole
/// decimal ID, with captures substituted. Explicit aliases are consulted by
/// the caller first.
pub fn apply_rules(rules: &[Rule], id: u32) -> Option<String> {
    let text = id.to_string();
    for r in rules {
        let anchored = format!("^(?:{})$", r.pattern);
        let Ok(re) = regex::Regex::new(&anchored) else {
            continue;
        };
        if let Some(c) = re.captures(&text) {
            let mut out = String::new();
            c.expand(&r.name, &mut out);
            let out = out.trim().to_string();
            if !out.is_empty() {
                return Some(out);
            }
        }
    }
    None
}

/// Explicit alias first, then the rules.
pub fn name_for(units: &HashMap<u32, String>, rules: &[Rule], id: u32) -> Option<String> {
    units
        .get(&id)
        .cloned()
        .or_else(|| apply_rules(rules, id))
}

#[tauri::command]
pub fn unit_rules_list(state: State<AppState>) -> Vec<Rule> {
    state.unit_rules.lock().unwrap().clone()
}

/// Replace the rule list (order matters: first match wins). Every pattern
/// must compile.
#[tauri::command]
pub fn unit_rules_set(
    app: AppHandle,
    state: State<AppState>,
    rules: Vec<Rule>,
) -> Result<usize, String> {
    for r in &rules {
        regex::Regex::new(&format!("^(?:{})$", r.pattern))
            .map_err(|e| format!("pattern `{}`: {e}", r.pattern))?;
    }
    let rules: Vec<Rule> = rules
        .into_iter()
        .filter(|r| !r.pattern.trim().is_empty())
        .collect();
    store_rules(&app, &rules)?;
    let n = rules.len();
    *state.unit_rules.lock().unwrap() = rules;
    Ok(n)
}

/// What a given radio would be called right now (explicit alias or rule).
#[tauri::command]
pub fn unit_resolve(state: State<AppState>, id: u32) -> Option<String> {
    name_for(
        &state.units.lock().unwrap(),
        &state.unit_rules.lock().unwrap(),
        id,
    )
}

/// Remember an alias the system broadcast over the air, unless the radio
/// already has a name. Returns true when something was written.
pub fn learn(app: &AppHandle, state: &AppState, id: u32, alias: &str) -> bool {
    let alias = alias.trim();
    if alias.is_empty() {
        return false;
    }
    let mut m = state.units.lock().unwrap();
    if m.contains_key(&id) {
        return false;
    }
    m.insert(id, alias.to_string());
    store(app, &m).is_ok()
}

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

/// Parse `id,name` lines (header skipped); returns the rows.
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

/// The wildcard rows of the same file: a first column that is not a plain
/// number is taken as a regular expression (trunk-recorder's convention).
pub fn parse_csv_rules(text: &str) -> Vec<Rule> {
    text.lines()
        .filter_map(|l| {
            let mut it = l.splitn(2, ',');
            let pat = it.next()?.trim().trim_matches('"');
            let name = it.next()?.trim().trim_matches('"');
            if pat.is_empty() || name.is_empty() || pat.parse::<u32>().is_ok() {
                return None;
            }
            regex::Regex::new(&format!("^(?:{pat})$")).ok()?;
            // The header line is not a rule.
            if pat.eq_ignore_ascii_case("unit id") || pat.eq_ignore_ascii_case("id") {
                return None;
            }
            Some(Rule {
                pattern: pat.to_string(),
                name: name.to_string(),
            })
        })
        .collect()
}

/// Import a CSV (`Unit ID, Name`, trunk-recorder unitTagsFile shape); plain
/// rows merge into the table, regex rows into the rules. Returns the total
/// number of aliases after import.
#[tauri::command]
pub fn units_import(app: AppHandle, state: State<AppState>, path: String) -> Result<usize, String> {
    let text = std::fs::read_to_string(crate::shellexpand_home(&path))
        .map_err(|e| format!("{path}: {e}"))?;
    let rows = parse_csv(&text);
    let rules = parse_csv_rules(&text);
    if rows.is_empty() && rules.is_empty() {
        return Err("no `id,name` rows found".into());
    }
    if !rules.is_empty() {
        let mut r = state.unit_rules.lock().unwrap();
        for rule in rules {
            if !r.contains(&rule) {
                r.push(rule);
            }
        }
        store_rules(&app, &r)?;
    }
    let mut m = state.units.lock().unwrap();
    m.extend(rows);
    store(&app, &m)?;
    Ok(m.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_trunk_recorder_unit_tags() {
        let text = "Unit ID,Unit Tag\n4900165,\"Car 12\"\n^49001(\\d\\d)$,Fleet $1\n790062,Engine 3\n";
        assert_eq!(
            parse_csv(text),
            vec![(4900165, "Car 12".into()), (790062, "Engine 3".into())]
        );
        // The regex row is a wildcard rule, not skipped.
        let rules = parse_csv_rules(text);
        assert_eq!(
            rules,
            vec![Rule {
                pattern: "^49001(\\d\\d)$".into(),
                name: "Fleet $1".into()
            }]
        );
        assert_eq!(apply_rules(&rules, 4900142), Some("Fleet 42".into()));
        assert_eq!(apply_rules(&rules, 4900165), Some("Fleet 65".into()));
        assert_eq!(apply_rules(&rules, 5000142), None);
        // An explicit alias beats a rule.
        let mut m = HashMap::new();
        m.insert(4900165, "Car 12".to_string());
        assert_eq!(name_for(&m, &rules, 4900165), Some("Car 12".into()));
        assert_eq!(name_for(&m, &rules, 4900101), Some("Fleet 01".into()));
    }

    #[test]
    fn rules_match_the_whole_id_and_first_wins() {
        let rules = vec![
            Rule { pattern: "79.*".into(), name: "IMPD {$0}".into() },
            Rule { pattern: "7900(\\d+)".into(), name: "Never $1".into() },
        ];
        assert_eq!(apply_rules(&rules, 790065), Some("IMPD {790065}".into()));
        assert_eq!(apply_rules(&rules, 1790065), None, "not anchored");
    }
}
