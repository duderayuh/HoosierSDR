//! ECPR: the whole ED-ECPR process in one place, so every part of it can be
//! changed from Settings → ECPR rather than in code.
//!
//! The process has four parts, and this module holds the settings for all
//! of them:
//!
//! - **The screen** — a tripwire with an *extract* check: it listens for
//!   arrest words, asks the model the screening questions and pages the
//!   team. Its words, prompt, fields, conditions, destination and message
//!   are edited on the Tripwires page like any other; here is which
//!   tripwire that is, and the names of the fields the research page reads
//!   off it (`candidate`, `criteriaMet`, …).
//! - **The score** — the survival estimate worked out in code from what the
//!   crew's report stated (see `study`). The criteria, the estimate table,
//!   the age exclusion and the wording all live here.
//! - **Reading what was said** — the word lists that turn a report's
//!   "unwitnessed" into *no*, "hospice" into *end-stage*, and "not stated"
//!   into *not said*.
//! - **The case message** — whether, and how, the score line appears in
//!   the case thread the ED reads.
//!
//! Saved as `ecpr.json` beside the other settings. The defaults reproduce
//! the program's ED-ECPR gate exactly, and *Reset to defaults* brings them
//! back.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};
use tauri::{AppHandle, Manager, State};

use crate::AppState;

// ---------------------------------------------------------------------------
// the model
// ---------------------------------------------------------------------------

/// One criterion of the score.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct CriterionRule {
    /// Short id (`time`, `witnessed`); also the key research columns use.
    pub key: String,
    /// What the reader sees: "Witnessed arrest".
    pub label: String,
    /// `time` — the age plus the minutes stated must come in under `limit`;
    /// `yes_no` — met when the fact says yes;
    /// `no_disease` — met unless the fact names an end-stage word.
    pub kind: String,
    /// The fact key it reads (for `time`, the minutes; the age comes from
    /// the score's `age_fact`).
    pub fact: String,
    /// `time` only: age + minutes must be under this.
    pub limit: u32,
    /// What to make of a fact that was not stated: `unknown` (widens the
    /// estimate), `met` (counted met, marked *assumed*) or `not met`.
    pub unstated: String,
    pub enabled: bool,
}

impl Default for CriterionRule {
    fn default() -> Self {
        Self {
            key: String::new(),
            label: String::new(),
            kind: "yes_no".into(),
            fact: String::new(),
            limit: 100,
            unstated: "unknown".into(),
            enabled: true,
        }
    }
}

/// The score: what it is called, its criteria, and how it reads a report.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct ScoreRules {
    /// By the name a methods section would give it.
    pub name: String,
    pub criteria: Vec<CriterionRule>,
    /// Favourable-outcome estimate, percent, by criteria met: `bands[n]`
    /// is `[low, high]` for *n* met. One entry more than there are
    /// criteria.
    pub bands: Vec<[f64; 2]>,
    /// The fact key holding the patient's age.
    pub age_fact: String,
    /// Oldest age the screen accepts; older is a hard exclusion. 0 = no
    /// age exclusion.
    pub max_age: u32,
    /// Add "(2 of 4 not stated)" to a range.
    pub not_stated_note: bool,
    /// Words that mean the thing was not said (not the same as "no").
    pub unstated_words: Vec<String>,
    /// A value whose first word is one of these means no …
    pub no_words: Vec<String>,
    /// … or that contains one of these anywhere.
    pub no_phrases: Vec<String>,
    /// Likewise for yes. No is checked first.
    pub yes_words: Vec<String>,
    pub yes_phrases: Vec<String>,
    /// A history containing any of these names an end-stage disease.
    pub end_stage_words: Vec<String>,
}

impl Default for ScoreRules {
    fn default() -> Self {
        let w = |xs: &[&str]| xs.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        Self {
            name: "ED-ECPR 4-criterion screen".into(),
            criteria: vec![
                CriterionRule { key: "time".into(), label: "Age + low-flow minutes < 100".into(), kind: "time".into(), fact: "downtime".into(), limit: 100, unstated: "unknown".into(), enabled: true },
                CriterionRule { key: "witnessed".into(), label: "Witnessed arrest".into(), kind: "yes_no".into(), fact: "witnessed".into(), limit: 100, unstated: "unknown".into(), enabled: true },
                CriterionRule { key: "bystander".into(), label: "Bystander CPR".into(), kind: "yes_no".into(), fact: "bystander cpr".into(), limit: 100, unstated: "unknown".into(), enabled: true },
                CriterionRule { key: "disease".into(), label: "No known end-stage disease".into(), kind: "no_disease".into(), fact: "history".into(), limit: 100, unstated: "met".into(), enabled: true },
            ],
            // 4 of 4 about 46 %, 3 of 4 about 12 %, two or fewer 0 to 5 %.
            bands: vec![[0.0, 5.0], [0.0, 5.0], [0.0, 5.0], [12.0, 12.0], [46.0, 46.0]],
            age_fact: "age".into(),
            max_age: 65,
            not_stated_note: true,
            unstated_words: w(&["not stated", "not said", "unknown", "unclear", "n/a", "na", "none stated", "not mentioned", "not reported", "?"]),
            no_words: w(&["no", "not", "none", "negative", "never", "n", "false", "0", "unwitnessed", "without"]),
            no_phrases: w(&["unwitnessed", "not witnessed", "no bystander", "no cpr", "no rosc", "no pulse", "remains in arrest", "still in arrest"]),
            yes_words: w(&["yes", "y", "true", "1", "positive", "affirmative"]),
            yes_phrases: w(&["witnessed", "bystander", "cpr", "compressions", "rosc", "pulses back", "return of", "got pulses", "pulse back"]),
            end_stage_words: w(&["end stage", "end-stage", "esrd", "dialysis", "hospice", "dnr", "metastatic", "terminal", "comfort care"]),
        }
    }
}

/// The names of the fields the screen tripwire returns, as the research
/// page reads them.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct ScreenFields {
    pub candidate: String,
    pub criteria_met: String,
    pub likelihood_pct: String,
    pub reason: String,
}

impl Default for ScreenFields {
    fn default() -> Self {
        Self {
            candidate: "candidate".into(),
            criteria_met: "criteriaMet".into(),
            likelihood_pct: "likelihoodPct".into(),
            reason: "reason".into(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// The tripwire that is the ECPR screen, by id. Blank = the first
    /// extraction on the case that returned the `candidate` field.
    pub screen_tripwire: String,
    pub screen: ScreenFields,
    pub score: ScoreRules,
    /// Put the score in the case message.
    pub case_line: bool,
    /// What the score line starts with.
    pub case_prefix: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            screen_tripwire: String::new(),
            screen: ScreenFields::default(),
            score: ScoreRules::default(),
            case_line: true,
            case_prefix: "📈".into(),
        }
    }
}

// ---------------------------------------------------------------------------
// persistence: one cell every scorer can reach
// ---------------------------------------------------------------------------

static DIR: OnceLock<PathBuf> = OnceLock::new();
static SETTINGS: OnceLock<RwLock<Settings>> = OnceLock::new();

fn cell() -> &'static RwLock<Settings> {
    SETTINGS.get_or_init(|| RwLock::new(Settings::default()))
}

/// Called once at startup: read `ecpr.json` so the case sender, the
/// research page and the study all score by the same rules.
pub fn init(app: &AppHandle) {
    if let Ok(d) = app.path().app_config_dir() {
        let _ = DIR.set(d);
    }
    let mut s: Settings = DIR
        .get()
        .and_then(|d| std::fs::read_to_string(d.join("ecpr.json")).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    sanitize(&mut s);
    *cell().write().unwrap() = s;
}

pub fn settings() -> Settings {
    cell().read().unwrap().clone()
}

fn store(s: &Settings) -> Result<(), String> {
    let d = DIR.get().ok_or("the config folder is not known yet")?;
    std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    let p = d.join("ecpr.json");
    std::fs::write(&p, serde_json::to_string_pretty(s).map_err(|e| e.to_string())?).map_err(|e| format!("{}: {e}", p.display()))?;
    *cell().write().unwrap() = s.clone();
    Ok(())
}

// ---------------------------------------------------------------------------
// keeping what is saved sound
// ---------------------------------------------------------------------------

/// A word list as typed: one per line or comma, trimmed, lower-cased, no
/// blanks, no repeats.
fn words(list: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in list.iter().flat_map(|l| l.split(['\n', ','])) {
        let w = item.trim().to_ascii_lowercase();
        if !w.is_empty() && !out.contains(&w) {
            out.push(w);
        }
    }
    out
}

fn key_of(label: &str) -> String {
    let k: String = label.trim().to_ascii_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    k.trim_matches('_').to_string()
}

/// Settle every field a page could have left odd: names, keys, kinds, the
/// band table's length, and the word lists.
pub fn sanitize(s: &mut Settings) {
    let d = Settings::default();
    s.screen_tripwire = s.screen_tripwire.trim().to_string();
    let f = |v: &str, dflt: &str| if v.trim().is_empty() { dflt.to_string() } else { v.trim().to_string() };
    s.screen.candidate = f(&s.screen.candidate, &d.screen.candidate);
    s.screen.criteria_met = f(&s.screen.criteria_met, &d.screen.criteria_met);
    s.screen.likelihood_pct = f(&s.screen.likelihood_pct, &d.screen.likelihood_pct);
    s.screen.reason = f(&s.screen.reason, &d.screen.reason);
    s.case_prefix = s.case_prefix.trim().to_string();

    let r = &mut s.score;
    r.name = f(&r.name, &d.score.name);
    r.age_fact = f(&r.age_fact, &d.score.age_fact).to_ascii_lowercase();
    r.criteria.retain(|c| !(c.label.trim().is_empty() && c.fact.trim().is_empty()));
    let mut keys: Vec<String> = Vec::new();
    for (i, c) in r.criteria.iter_mut().enumerate() {
        c.label = c.label.trim().to_string();
        c.fact = c.fact.trim().to_ascii_lowercase();
        if c.label.is_empty() {
            c.label = c.fact.clone();
        }
        if !["time", "yes_no", "no_disease"].contains(&c.kind.as_str()) {
            c.kind = "yes_no".into();
        }
        if !["unknown", "met", "not met"].contains(&c.unstated.as_str()) {
            c.unstated = "unknown".into();
        }
        if c.limit == 0 {
            c.limit = 100;
        }
        let mut k = key_of(&c.key);
        if k.is_empty() {
            k = key_of(&c.label);
        }
        if k.is_empty() {
            k = format!("c{}", i + 1);
        }
        let base = k.clone();
        let mut n = 2;
        while keys.contains(&k) {
            k = format!("{base}_{n}");
            n += 1;
        }
        keys.push(k.clone());
        c.key = k;
    }
    let n = r.criteria.iter().filter(|c| c.enabled).count() + 1;
    if r.bands.is_empty() {
        r.bands = d.score.bands.clone();
    }
    while r.bands.len() < n {
        let last = *r.bands.last().unwrap();
        r.bands.push(last);
    }
    r.bands.truncate(n);
    for b in r.bands.iter_mut() {
        b[0] = b[0].clamp(0.0, 100.0);
        b[1] = b[1].clamp(0.0, 100.0);
        if b[1] < b[0] {
            b.swap(0, 1);
        }
    }
    r.unstated_words = words(&r.unstated_words);
    r.no_words = words(&r.no_words);
    r.no_phrases = words(&r.no_phrases);
    r.yes_words = words(&r.yes_words);
    r.yes_phrases = words(&r.yes_phrases);
    r.end_stage_words = words(&r.end_stage_words);
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

/// A tripwire that extracts fields — a candidate for being the screen.
#[derive(Serialize, Clone, Debug)]
pub struct ScreenOption {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    /// The field keys it returns.
    pub keys: Vec<String>,
}

#[derive(Serialize)]
pub struct View {
    pub settings: Settings,
    pub screens: Vec<ScreenOption>,
    /// The fact keys the crew's report is asked for, for the pickers.
    pub facts: Vec<String>,
}

fn screens(state: &State<AppState>) -> Vec<ScreenOption> {
    let st = state.tripwires.lock().unwrap();
    st.settings
        .tripwires
        .iter()
        .filter(|t| t.check.kind == "extract")
        .map(|t| ScreenOption {
            id: t.id.clone(),
            name: t.name.clone(),
            enabled: t.enabled && crate::tripwires::folders_on(&st.settings.folders, &t.parent),
            keys: t.check.fields.iter().map(|f| f.key.clone()).collect(),
        })
        .collect()
}

fn view(state: &State<AppState>) -> View {
    View {
        settings: settings(),
        screens: screens(state),
        facts: crate::conversations::fact_keys(),
    }
}

#[tauri::command]
pub fn ecpr_get(state: State<AppState>) -> View {
    view(&state)
}

#[tauri::command]
pub fn ecpr_set(state: State<AppState>, settings: Settings) -> Result<View, String> {
    let mut s = settings;
    sanitize(&mut s);
    if s.score.criteria.iter().filter(|c| c.enabled).count() == 0 {
        return Err("the score needs at least one criterion switched on".into());
    }
    store(&s)?;
    Ok(view(&state))
}

#[tauri::command]
pub fn ecpr_defaults() -> Settings {
    Settings::default()
}

/// Score a set of facts by a draft's rules, without saving anything: the
/// page's "try it" box.
#[tauri::command]
pub fn ecpr_try(settings: Settings, facts: BTreeMap<String, String>) -> crate::study::Score {
    let mut s = settings;
    sanitize(&mut s);
    let facts: BTreeMap<String, String> = facts.into_iter().map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())).filter(|(k, v)| !k.is_empty() && !v.is_empty()).collect();
    crate::study::score_with(&s.score, &facts)
}

/// Add the ECPR screen from its starter recipe, switched off, and make it
/// the screen. Returns the new tripwire's id so the page can open it.
#[tauri::command]
pub fn ecpr_screen_create(app: AppHandle, state: State<AppState>) -> Result<String, String> {
    let recipe = crate::tripwires::tripwire_recipes()
        .into_iter()
        .find(|r| r.id == "screen-1")
        .ok_or("the ECPR starter is missing")?;
    let mut t = recipe.tripwire;
    t.enabled = false;
    t.id = format!("ecpr-{}", crate::library::now());
    let id = t.id.clone();
    let mut list = state.tripwires.lock().unwrap().settings.tripwires.clone();
    list.push(t);
    let saved = crate::tripwires::tripwires_set(app, state, list, None)?;
    let id = saved.iter().find(|t| t.id == id).map(|t| t.id.clone()).unwrap_or(id);
    let mut s = settings();
    s.screen_tripwire = id.clone();
    store(&s)?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_reproduce_the_gate_and_survive_a_round_trip() {
        let d = Settings::default();
        assert_eq!(d.score.criteria.len(), 4);
        assert_eq!(d.score.bands.len(), 5);
        let text = serde_json::to_string(&d).unwrap();
        let back: Settings = serde_json::from_str(&text).unwrap();
        assert_eq!(back, d);
        // An older file, missing everything new, still reads.
        let old: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(old, d);
    }

    #[test]
    fn sanitizing_settles_keys_bands_and_words() {
        let mut s = Settings::default();
        s.score.criteria.push(CriterionRule { key: String::new(), label: "Shockable rhythm".into(), kind: "odd".into(), fact: " Rhythm ".into(), unstated: "?".into(), ..Default::default() });
        s.score.criteria.push(CriterionRule { key: "time".into(), label: "Another".into(), fact: "x".into(), ..Default::default() });
        s.score.no_words = vec!["No, NOT\nnope".into(), "no".into()];
        s.score.bands = vec![[50.0, 10.0]];
        sanitize(&mut s);
        assert_eq!(s.score.criteria[4].key, "shockable_rhythm");
        assert_eq!((s.score.criteria[4].kind.as_str(), s.score.criteria[4].fact.as_str(), s.score.criteria[4].unstated.as_str()), ("yes_no", "rhythm", "unknown"));
        assert_eq!(s.score.criteria[5].key, "time_2");
        assert_eq!(s.score.no_words, vec!["no", "not", "nope"]);
        // Six criteria on: seven bands, the short table padded with its last row, low before high.
        assert_eq!(s.score.bands.len(), 7);
        assert_eq!(s.score.bands[0], [10.0, 50.0]);
        assert_eq!(s.score.bands[6], [10.0, 50.0]);
    }

    #[test]
    fn a_blank_screen_field_name_falls_back() {
        let mut s = Settings::default();
        s.screen.candidate = "  ".into();
        sanitize(&mut s);
        assert_eq!(s.screen.candidate, "candidate");
    }
}
