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

/// Explicit aliases. Radio IDs are only unique within a system (MESA's
/// 790041 and SAFE-T's 790041 are different radios), so the table is kept
/// per RadioReference system (`units_<sid>.json`) with one unscoped table
/// (`units.json`, the pre-multi-system file) that applies everywhere a
/// system has no row of its own.
#[derive(Default, Clone, Debug)]
pub struct UnitTable {
    pub global: HashMap<u32, String>,
    pub by_sid: HashMap<u32, HashMap<u32, String>>,
}

impl UnitTable {
    /// The alias for radio `id` on system `sid` (`None` = system unknown).
    pub fn get(&self, sid: Option<u32>, id: u32) -> Option<&String> {
        sid.and_then(|s| self.by_sid.get(&s))
            .and_then(|m| m.get(&id))
            .or_else(|| self.global.get(&id))
    }

    /// Is `id` named on exactly this scope (not by fallback)?
    fn has(&self, sid: Option<u32>, id: u32) -> bool {
        match sid {
            Some(s) => self.by_sid.get(&s).is_some_and(|m| m.contains_key(&id)),
            None => self.global.contains_key(&id),
        }
    }

    /// The table for one scope, created on demand.
    fn scope_mut(&mut self, sid: Option<u32>) -> &mut HashMap<u32, String> {
        match sid {
            Some(s) => self.by_sid.entry(s).or_default(),
            None => &mut self.global,
        }
    }

    fn scope(&self, sid: Option<u32>) -> Option<&HashMap<u32, String>> {
        match sid {
            Some(s) => self.by_sid.get(&s),
            None => Some(&self.global),
        }
    }

    pub fn len(&self) -> usize {
        self.global.len() + self.by_sid.values().map(HashMap::len).sum::<usize>()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub type Units = Arc<Mutex<UnitTable>>;

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

/// Explicit alias (the system's own, else the unscoped one) first, then
/// the rules.
pub fn name_for(units: &UnitTable, rules: &[Rule], sid: Option<u32>, id: u32) -> Option<String> {
    units
        .get(sid, id)
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

/// What a given radio would be called right now (explicit alias or rule)
/// on system `sid`.
#[tauri::command]
pub fn unit_resolve(state: State<AppState>, id: u32, sid: Option<u32>) -> Option<String> {
    name_for(
        &state.units.lock().unwrap(),
        &state.unit_rules.lock().unwrap(),
        sid,
        id,
    )
}

/// Remember an alias the system broadcast over the air, unless the radio
/// already has a name on that system. Returns true when something was
/// written.
pub fn learn(app: &AppHandle, state: &AppState, sid: Option<u32>, id: u32, alias: &str) -> bool {
    let alias = alias.trim();
    if alias.is_empty() {
        return false;
    }
    let mut t = state.units.lock().unwrap();
    if t.get(sid, id).is_some() {
        return false;
    }
    t.scope_mut(sid).insert(id, alias.to_string());
    store(app, sid, &t).is_ok()
}

fn config_dir(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let d = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("config dir: {e}"))?;
    std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    Ok(d)
}

fn path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    Ok(config_dir(app)?.join("units.json"))
}

/// `units.json` (any system) and every `units_<sid>.json`.
fn scope_path(app: &AppHandle, sid: Option<u32>) -> Result<std::path::PathBuf, String> {
    match sid {
        Some(s) => Ok(config_dir(app)?.join(format!("units_{s}.json"))),
        None => path(app),
    }
}

fn read_map(p: &std::path::Path) -> HashMap<u32, String> {
    std::fs::read_to_string(p)
        .ok()
        .and_then(|t| serde_json::from_str::<HashMap<u32, String>>(&t).ok())
        .unwrap_or_default()
}

pub fn load(app: &AppHandle) -> UnitTable {
    let mut t = UnitTable::default();
    let Ok(d) = config_dir(app) else {
        return t;
    };
    t.global = read_map(&d.join("units.json"));
    for e in std::fs::read_dir(&d)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
    {
        let p = e.path();
        let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if p.extension().is_some_and(|x| x == "json") {
            if let Some(sid) = stem
                .strip_prefix("units_")
                .and_then(|n| n.parse::<u32>().ok())
            {
                t.by_sid.insert(sid, read_map(&p));
            }
        }
    }
    t
}

/// Write one scope's table (an empty per-system table removes its file).
fn store(app: &AppHandle, sid: Option<u32>, t: &UnitTable) -> Result<(), String> {
    let p = scope_path(app, sid)?;
    let m = t.scope(sid).cloned().unwrap_or_default();
    if sid.is_some() && m.is_empty() {
        let _ = std::fs::remove_file(&p);
        return Ok(());
    }
    std::fs::write(
        &p,
        serde_json::to_string_pretty(&m).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("{}: {e}", p.display()))
}

#[derive(Serialize, Deserialize)]
pub struct UnitRow {
    pub id: u32,
    pub name: String,
    /// RadioReference system the alias belongs to; `None` = any system.
    #[serde(default)]
    pub sid: Option<u32>,
}

#[tauri::command]
pub fn units_list(state: State<AppState>) -> Vec<UnitRow> {
    let t = state.units.lock().unwrap();
    let mut v: Vec<UnitRow> = t
        .global
        .iter()
        .map(|(id, name)| UnitRow {
            id: *id,
            name: name.clone(),
            sid: None,
        })
        .collect();
    for (sid, m) in &t.by_sid {
        v.extend(m.iter().map(|(id, name)| UnitRow {
            id: *id,
            name: name.clone(),
            sid: Some(*sid),
        }));
    }
    v.sort_by_key(|u| (u.id, u.sid));
    v
}

/// Set (or, with an empty name, remove) one alias, on system `sid` or (with
/// none) for every system.
#[tauri::command]
pub fn unit_set(
    app: AppHandle,
    state: State<AppState>,
    id: u32,
    name: String,
    sid: Option<u32>,
) -> Result<usize, String> {
    let mut t = state.units.lock().unwrap();
    if name.trim().is_empty() {
        if t.has(sid, id) {
            t.scope_mut(sid).remove(&id);
        }
    } else {
        t.scope_mut(sid).insert(id, name.trim().to_string());
    }
    store(&app, sid, &t)?;
    Ok(t.len())
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
/// rows merge into the table for system `sid` (or the unscoped one), regex
/// rows into the rules. Returns the total number of aliases after import.
#[tauri::command]
pub fn units_import(
    app: AppHandle,
    state: State<AppState>,
    path: String,
    sid: Option<u32>,
) -> Result<usize, String> {
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
    let mut t = state.units.lock().unwrap();
    t.scope_mut(sid).extend(rows);
    store(&app, sid, &t)?;
    Ok(t.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_trunk_recorder_unit_tags() {
        let text =
            "Unit ID,Unit Tag\n4900165,\"Car 12\"\n^49001(\\d\\d)$,Fleet $1\n790062,Engine 3\n";
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
        let mut t = UnitTable::default();
        t.global.insert(4900165, "Car 12".to_string());
        assert_eq!(name_for(&t, &rules, None, 4900165), Some("Car 12".into()));
        assert_eq!(name_for(&t, &rules, None, 4900101), Some("Fleet 01".into()));
    }

    #[test]
    fn aliases_are_kept_per_system() {
        let mut t = UnitTable::default();
        t.global.insert(790041, "EMS Control".to_string());
        t.scope_mut(Some(5737))
            .insert(790041, "Indy EMS Control".to_string());
        t.scope_mut(Some(8084))
            .insert(790099, "State Trooper 99".to_string());
        // The system's own row wins; the unscoped row covers the rest.
        assert_eq!(
            t.get(Some(5737), 790041).map(String::as_str),
            Some("Indy EMS Control")
        );
        assert_eq!(
            t.get(Some(8084), 790041).map(String::as_str),
            Some("EMS Control")
        );
        assert_eq!(t.get(None, 790041).map(String::as_str), Some("EMS Control"));
        // Another system's alias never leaks.
        assert_eq!(t.get(Some(5737), 790099), None);
        assert_eq!(t.get(None, 790099), None);
        assert!(t.has(Some(8084), 790099));
        assert!(!t.has(Some(5737), 790099));
        assert!(t.has(None, 790041));
        assert_eq!(t.len(), 3);
    }

    #[test]
    fn rules_match_the_whole_id_and_first_wins() {
        let rules = vec![
            Rule {
                pattern: "79.*".into(),
                name: "Police {$0}".into(),
            },
            Rule {
                pattern: "7900(\\d+)".into(),
                name: "Never $1".into(),
            },
        ];
        assert_eq!(apply_rules(&rules, 790065), Some("Police {790065}".into()));
        assert_eq!(apply_rules(&rules, 1790065), None, "not anchored");
    }
}
