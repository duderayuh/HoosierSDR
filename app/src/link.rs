//! Joining a hospital report to the dispatch that started it.
//!
//! The same emergency reaches the radio twice. First a dispatcher sends a
//! crew somewhere — "Medic 41, 1217 North Whitfield Street, sick person" —
//! and twenty minutes later that crew reads a patient report to a hospital
//! on a different talkgroup. Until now those were two unrelated rows in two
//! unrelated tables, and the listener joined them in their head.
//!
//! The join is the crew. Hospital channels carry no radio aliases here — the
//! radio IDs are bare numbers — but the crew says who they are in the first
//! sentence of the report, and the dispatch already recorded which units it
//! sent. A callsign heard on a hospital channel, inside the window it takes
//! to drive there, is the same job.
//!
//! Rules decide; the model only breaks ties. One dispatch with that crew in
//! the window is a link and nothing is asked of any model. Several, and the
//! clock is tried first — a report an hour after one dispatch and four
//! minutes after another is about the second. Only a genuine tie is put to
//! the local model, and if it cannot be reached nothing is linked, because a
//! wrong join is worse than none.

use rusqlite::{params, Connection};
use serde::Serialize;
use std::collections::HashSet;

/// How long before the report a dispatch may have gone out, and how much
/// after it (a crew sometimes calls the hospital as the run is still being
/// dispatched).
pub const LOOK_BACK_SECS: i64 = 75 * 60;
pub const LOOK_AHEAD_SECS: i64 = 5 * 60;
/// A candidate this much closer in time than the next wins without asking
/// anyone.
const CLEAR_WIN_RATIO: f64 = 0.45;

/// What a hospital report says about itself.
#[derive(Clone, Debug, Default)]
pub struct Report {
    pub at: i64,
    /// Summary and transcript, which is where the callsign is said.
    pub text: String,
}

/// A dispatch that might be the same job.
#[derive(Clone, Debug)]
pub struct Dispatch {
    pub id: i64,
    pub at: i64,
    pub call_type: String,
    pub units: Vec<String>,
    pub address: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum Link {
    /// Linked, with the reason in the listener's words.
    Made { incident: i64, how: String },
    /// More than one fits and the clock cannot separate them.
    Tie { incidents: Vec<i64>, unit: String },
    /// Nothing fits.
    None,
}

/// The callsigns a report mentions, in the order they are said.
///
/// `vocabulary` is the set of words this county uses in front of a number —
/// learned from the dispatches themselves rather than written down here, so
/// an agency that runs "Rescue 4" or "Squad 12" is understood without anyone
/// teaching it.
pub fn callsigns(text: &str, vocabulary: &HashSet<String>) -> Vec<String> {
    let words: Vec<&str> = text.split(|c: char| !c.is_alphanumeric()).collect();
    let mut out: Vec<String> = Vec::new();
    for (i, w) in words.iter().enumerate() {
        let low = w.to_ascii_lowercase();
        if !vocabulary.contains(&low) {
            continue;
        }
        let Some(n) = words.get(i + 1).and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        let sign = format!("{} {}", title(&low), n);
        if !out.contains(&sign) {
            out.push(sign);
        }
    }
    out
}

fn title(w: &str) -> String {
    let mut c = w.chars();
    match c.next() {
        Some(f) => f.to_ascii_uppercase().to_string() + c.as_str(),
        None => String::new(),
    }
}

/// The words this library's dispatches put in front of a unit number.
pub fn vocabulary(c: &Connection) -> HashSet<String> {
    let mut out: HashSet<String> = HashSet::new();
    let Ok(mut q) = c.prepare("SELECT units FROM incidents WHERE units <> '[]' LIMIT 4000") else {
        return out;
    };
    let rows = q.query_map([], |r| r.get::<_, String>(0));
    let Ok(rows) = rows else { return out };
    for text in rows.flatten() {
        let list: Vec<String> = serde_json::from_str(&text).unwrap_or_default();
        for u in list {
            if let Some(word) = u.split_whitespace().next() {
                let low = word.to_ascii_lowercase();
                if low.chars().all(|c| c.is_alphabetic()) && low.len() >= 3 {
                    out.insert(low);
                }
            }
        }
    }
    out
}

fn same_unit(units: &[String], sign: &str) -> bool {
    units.iter().any(|u| u.eq_ignore_ascii_case(sign))
}

/// Decide, from the rules alone, which dispatch a report belongs to.
///
/// `open` are the dispatches inside the window, in any order.
pub fn decide(report: &Report, open: &[Dispatch], vocabulary: &HashSet<String>) -> Link {
    for sign in callsigns(&report.text, vocabulary) {
        let mut fits: Vec<&Dispatch> = open
            .iter()
            .filter(|d| {
                same_unit(&d.units, &sign)
                    && report.at >= d.at - LOOK_AHEAD_SECS
                    && report.at <= d.at + LOOK_BACK_SECS
            })
            .collect();
        if fits.is_empty() {
            continue;
        }
        if fits.len() == 1 {
            return Link::Made {
                incident: fits[0].id,
                how: format!("{sign} was sent to this run"),
            };
        }
        // Several runs with the same crew in the window: the clock usually
        // separates them. A crew is at the hospital minutes after the run,
        // not an hour.
        fits.sort_by_key(|d| (report.at - d.at).abs());
        let (best, next) = (fits[0], fits[1]);
        let (b, n) = (
            (report.at - best.at).abs() as f64,
            (report.at - next.at).abs() as f64,
        );
        if n > 0.0 && b / n <= CLEAR_WIN_RATIO {
            return Link::Made {
                incident: best.id,
                how: format!("{sign}, and this run is the closest in time"),
            };
        }
        return Link::Tie {
            incidents: fits.iter().map(|d| d.id).collect(),
            unit: sign,
        };
    }
    Link::None
}

/// What the model is asked when the clock cannot separate two runs. It is
/// given nothing but the report and the runs, and answers with an id.
pub fn tie_prompt(report: &Report, fits: &[&Dispatch], unit: &str) -> String {
    let mut s = format!(
        "A crew radioed this report to a hospital. {unit} was sent to more than one run \
         in the hour before it. Which run is this report about?\n\nReport:\n{}\n\nRuns:\n",
        report.text.trim()
    );
    for d in fits {
        let mins = (report.at - d.at) / 60;
        s.push_str(&format!(
            "- id {}: {} at {}, {} minutes before the report\n",
            d.id, d.call_type, d.address, mins
        ));
    }
    s.push_str(
        "\nAnswer with JSON only: {\"id\": <the run's id, or 0 if none of them fits>, \
         \"why\": \"<a few words>\"}",
    );
    s
}

/// Read the model's answer back. Anything that is not one of the offered
/// ids is no answer at all.
pub fn tie_answer(text: &str, offered: &[i64]) -> Option<(i64, String)> {
    // The same shape of answer the AI checks parse: JSON somewhere in
    // whatever the model felt like saying.
    let (start, end) = (text.find('{')?, text.rfind('}')?);
    let v: serde_json::Value = serde_json::from_str(text.get(start..=end)?).ok()?;
    let id = v.get("id").and_then(|x| x.as_i64())?;
    offered.contains(&id).then(|| {
        (
            id,
            v.get("why")
                .and_then(|x| x.as_str())
                .unwrap_or("the model chose this run")
                .to_string(),
        )
    })
}

// ---------------------------------------------------------------------------
// the library side
// ---------------------------------------------------------------------------

pub fn ensure_schema(c: &Connection) {
    // A conversation remembers the run it belongs to. Nullable: most
    // conversations are not hospital reports, and an unlinked one is normal.
    let _ = c.execute("ALTER TABLE conversations ADD COLUMN incident INTEGER", []);
    let _ = c.execute("ALTER TABLE conversations ADD COLUMN link_how TEXT NOT NULL DEFAULT ''", []);
    let _ = c.execute(
        "CREATE INDEX IF NOT EXISTS conversations_incident ON conversations(incident)",
        [],
    );
}

/// The dispatches a report could belong to.
pub fn open_dispatches(c: &Connection, at: i64) -> Vec<Dispatch> {
    let Ok(mut q) = c.prepare(
        "SELECT id, created, call_type, units, address FROM incidents
          WHERE created BETWEEN ?1 AND ?2",
    ) else {
        return Vec::new();
    };
    let rows = q.query_map(params![at - LOOK_BACK_SECS, at + LOOK_AHEAD_SECS], |r| {
        Ok(Dispatch {
            id: r.get(0)?,
            at: r.get(1)?,
            call_type: r.get(2)?,
            units: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(),
            address: r.get(4)?,
        })
    });
    rows.map(|r| r.flatten().collect()).unwrap_or_default()
}

/// Remember a link.
pub fn attach(c: &Connection, conversation: i64, incident: i64, how: &str) -> Result<(), String> {
    c.execute(
        "UPDATE conversations SET incident = ?1, link_how = ?2 WHERE id = ?3",
        params![incident, how, conversation],
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// The hospital reports linked to a run, oldest first.
#[derive(Serialize, Clone, Debug)]
pub struct LinkedReport {
    pub id: i64,
    pub at: i64,
    pub tg: u16,
    pub tg_name: String,
    pub tg_desc: String,
    pub place: String,
    pub summary: String,
    pub how: String,
}

pub fn reports_for(c: &Connection, incident: i64, places: &crate::places::Settings) -> Vec<LinkedReport> {
    let Ok(mut q) = c.prepare(
        "SELECT id, first_at, tg, tg_name, tg_desc, summary, link_how FROM conversations
          WHERE incident = ?1 ORDER BY first_at",
    ) else {
        return Vec::new();
    };
    let rows = q.query_map([incident], |r| {
        let tg: u16 = r.get::<_, i64>(2)? as u16;
        Ok(LinkedReport {
            id: r.get(0)?,
            at: r.get(1)?,
            tg,
            tg_name: r.get(3)?,
            tg_desc: r.get(4)?,
            place: crate::places::for_tg(places, tg, "")
                .map(|p| p.name.clone())
                .unwrap_or_default(),
            summary: r.get(5)?,
            how: r.get(6)?,
        })
    });
    rows.map(|r| r.flatten().collect()).unwrap_or_default()
}

/// Join a stored conversation to a run, if the rules can see which one.
///
/// Called after a conversation is stored. Everything here is rules; the
/// model is asked only when two runs fit equally well, and a model that
/// cannot be reached means no link rather than a guess.
pub fn try_link(app: &tauri::AppHandle, conversation: i64) -> Option<(i64, String)> {
    use tauri::Manager;
    let state = app.state::<crate::AppState>();
    let db = state.db.lock().unwrap().clone()?;

    let (report, open, vocab) = {
        let c = db.lock().unwrap();
        let (at, summary, transcript, already): (i64, String, String, Option<i64>) = c
            .query_row(
                "SELECT first_at, summary, transcript, incident FROM conversations WHERE id = ?1",
                [conversation],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .ok()?;
        if already.is_some() {
            return None;
        }
        let report = Report {
            at,
            text: format!("{summary}\n{transcript}"),
        };
        (report, open_dispatches(&c, at), vocabulary(&c))
    };

    let (incident, how) = match decide(&report, &open, &vocab) {
        Link::Made { incident, how } => (incident, how),
        Link::None => return None,
        Link::Tie { incidents, unit } => {
            let fits: Vec<&Dispatch> = open.iter().filter(|d| incidents.contains(&d.id)).collect();
            let o = crate::alerts::shared_settings(&state).1;
            let answer = crate::alerts::ollama_json(&o, &tie_prompt(&report, &fits, &unit));
            match answer.ok().and_then(|t| tie_answer(&t, &incidents)) {
                Some((id, why)) => (id, format!("{unit}; the model picked this run — {why}")),
                // A tie nobody could settle stays unjoined: a wrong join is
                // worse than none.
                None => return None,
            }
        }
    };
    {
        let c = db.lock().unwrap();
        attach(&c, conversation, incident, &how).ok()?;
    }
    let _ = tauri::Emitter::emit(
        app,
        "incident_linked",
        serde_json::json!({ "conversation": conversation, "incident": incident, "how": how }),
    );
    // A tripwire waiting for the outcome — "tell me where the arrest went" —
    // has been waiting for exactly this.
    {
        let c = db.lock().unwrap();
        if let Ok(Some(run)) = crate::dispatch::inc_get(&c, incident) {
            drop(c);
            crate::tripwires::on_incident(app, &run, true);
        }
    }
    Some((incident, how))
}

/// Go back over stored conversations and join the ones the rules can place.
/// Runs when the listener asks, since it may talk to the local model.
#[tauri::command]
pub fn incidents_relink(app: tauri::AppHandle, state: tauri::State<crate::AppState>) -> Result<(u32, u32), String> {
    let Some(db) = state.db.lock().unwrap().clone() else {
        return Err("library not open".into());
    };
    let todo: Vec<i64> = {
        let c = db.lock().unwrap();
        let mut q = c
            .prepare("SELECT id FROM conversations WHERE incident IS NULL ORDER BY id DESC LIMIT 500")
            .map_err(|e| e.to_string())?;
        let rows = q
            .query_map([], |r| r.get::<_, i64>(0))
            .map_err(|e| e.to_string())?;
        rows.flatten().collect()
    };
    let tried = todo.len() as u32;
    let mut made = 0u32;
    for id in todo {
        if try_link(&app, id).is_some() {
            made += 1;
        }
    }
    Ok((tried, made))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vocab() -> HashSet<String> {
        ["medic", "ambulance", "engine", "ladder"]
            .into_iter()
            .map(String::from)
            .collect()
    }

    fn dispatch(id: i64, at: i64, units: &[&str]) -> Dispatch {
        Dispatch {
            id,
            at,
            call_type: "Sick Person".into(),
            units: units.iter().map(|s| s.to_string()).collect(),
            address: "1 Example Street".into(),
        }
    }

    fn report(at: i64, text: &str) -> Report {
        Report {
            at,
            text: text.into(),
        }
    }

    #[test]
    fn the_crew_says_who_they_are() {
        let v = vocab();
        assert_eq!(
            callsigns("Medic 71 is inbound to the ER with a 15-year-old", &v),
            vec!["Medic 71"]
        );
        // Said twice, counted once; a second crew is kept in order.
        assert_eq!(
            callsigns("Engine 26 and Medic 74 responding, Engine 26 on scene", &v),
            vec!["Engine 26", "Medic 74"]
        );
        // A word the county does not use, and a number with nothing in
        // front of it, name nobody.
        assert!(callsigns("Unit 63 is inbound", &v).is_empty());
        assert!(callsigns("a 67-year-old male", &v).is_empty());
        assert!(callsigns("The unit is transporting an adult male", &v).is_empty());
    }

    #[test]
    fn one_run_with_that_crew_is_the_run() {
        let open = vec![
            dispatch(10, 1_000, &["Medic 71"]),
            dispatch(11, 1_100, &["Engine 26", "Medic 74"]),
        ];
        let got = decide(&report(2_400, "Medic 71 is inbound"), &open, &vocab());
        assert_eq!(
            got,
            Link::Made {
                incident: 10,
                how: "Medic 71 was sent to this run".into()
            }
        );
    }

    #[test]
    fn a_run_too_long_ago_is_a_different_job() {
        let open = vec![dispatch(10, 1_000, &["Medic 71"])];
        // Well past the drive-and-report window.
        let late = report(1_000 + LOOK_BACK_SECS + 60, "Medic 71 is inbound");
        assert_eq!(decide(&late, &open, &vocab()), Link::None);
        // And a report long before the run is not it either.
        let early = report(1_000 - LOOK_AHEAD_SECS - 60, "Medic 71 is inbound");
        assert_eq!(decide(&early, &open, &vocab()), Link::None);
    }

    #[test]
    fn the_clock_settles_the_easy_double() {
        let open = vec![
            dispatch(10, 0, &["Medic 71"]),      // an hour before
            dispatch(11, 3_300, &["Medic 71"]),  // four minutes before
        ];
        let got = decide(&report(3_540, "Medic 71 is inbound"), &open, &vocab());
        assert_eq!(
            got,
            Link::Made {
                incident: 11,
                how: "Medic 71, and this run is the closest in time".into()
            }
        );
    }

    #[test]
    fn a_real_tie_is_not_guessed() {
        let open = vec![
            dispatch(10, 1_000, &["Medic 71"]),
            dispatch(11, 1_400, &["Medic 71"]),
        ];
        let got = decide(&report(2_000, "Medic 71 is inbound"), &open, &vocab());
        assert_eq!(
            got,
            Link::Tie {
                incidents: vec![11, 10],
                unit: "Medic 71".into()
            }
        );
    }

    #[test]
    fn the_model_only_gets_to_pick_from_what_it_was_offered() {
        let d = [dispatch(10, 0, &["Medic 71"]), dispatch(11, 600, &["Medic 71"])];
        let fits: Vec<&Dispatch> = d.iter().collect();
        let p = tie_prompt(&report(1_200, "Medic 71 inbound"), &fits, "Medic 71");
        assert!(p.contains("id 10") && p.contains("id 11"));
        assert_eq!(
            tie_answer(r#"{"id": 11, "why": "same complaint"}"#, &[10, 11]),
            Some((11, "same complaint".into()))
        );
        // An id nobody offered, and "none of them", are no answer.
        assert_eq!(tie_answer(r#"{"id": 99}"#, &[10, 11]), None);
        assert_eq!(tie_answer(r#"{"id": 0}"#, &[10, 11]), None);
        assert_eq!(tie_answer("the model rambled", &[10, 11]), None);
    }

    #[test]
    fn the_vocabulary_is_learned_from_the_dispatches() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE incidents (id INTEGER PRIMARY KEY, units TEXT NOT NULL DEFAULT '[]');
             INSERT INTO incidents (units) VALUES ('[\"Medic 41\",\"Rescue 7\"]'), ('[\"Squad 12\"]'), ('[]');",
        )
        .unwrap();
        let v = vocabulary(&c);
        assert!(v.contains("medic") && v.contains("rescue") && v.contains("squad"));
        // Which means an agency nobody wrote into this file is understood.
        assert_eq!(callsigns("Squad 12 is inbound", &v), vec!["Squad 12"]);
    }
}

/// Run the rules over a real library and print what they would join.
/// `HS_LIB_DB=… cargo test link::real -- --ignored --nocapture`
#[cfg(test)]
mod real {
    use super::*;

    #[test]
    #[ignore]
    fn what_the_rules_join_in_a_real_library() {
        let Ok(path) = std::env::var("HS_LIB_DB") else {
            eprintln!("set HS_LIB_DB");
            return;
        };
        let c = Connection::open(&path).unwrap();
        let v = vocabulary(&c);
        println!("vocabulary: {} words", v.len());
        let mut q = c
            .prepare("SELECT id, first_at, tg, tg_name, summary, transcript FROM conversations ORDER BY id")
            .unwrap();
        let rows: Vec<(i64, i64, u16, String, String, String)> = q
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get::<_, i64>(2)? as u16,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })
            .unwrap()
            .flatten()
            .collect();
        let (mut made, mut tie, mut none) = (0, 0, 0);
        let (mut no_sign, mut no_runs, mut unmatched) = (0, 0, 0);
        for (id, at, tg, tg_name, summary, transcript) in &rows {
            let report = Report {
                at: *at,
                text: format!("{summary}\n{transcript}"),
            };
            let open = open_dispatches(&c, *at);
            match decide(&report, &open, &v) {
                Link::Made { incident, how } => {
                    made += 1;
                    if made <= 10 {
                        let (ct, addr): (String, String) = c
                            .query_row(
                                "SELECT call_type, address FROM incidents WHERE id = ?1",
                                [incident],
                                |r| Ok((r.get(0)?, r.get(1)?)),
                            )
                            .unwrap_or_default();
                        println!("  conv {id} ({tg_name}) → run {incident} {ct} @ {addr}  [{how}]");
                        println!("      {}", summary.lines().next().unwrap_or("").trim());
                    }
                }
                Link::Tie { incidents, unit } => {
                    tie += 1;
                    if tie <= 4 {
                        println!("  conv {id}: {unit} fits runs {incidents:?} — a tie");
                    }
                }
                Link::None => {
                    none += 1;
                    let said = callsigns(&report.text, &v);
                    if said.is_empty() {
                        no_sign += 1;
                    } else if open.is_empty() {
                        no_runs += 1;
                    } else {
                        unmatched += 1;
                        if unmatched <= 5 {
                            println!("  conv {id}: said {said:?}, {} runs in the window, none of them theirs", open.len());
                        }
                    }
                }
            }
        }
        println!(
            "{} conversations: {made} joined, {tie} ties for the model, {none} unjoined \
             ({no_sign} named no crew, {no_runs} had no dispatch recorded in the window, \
             {unmatched} named a crew no nearby run was sent)",
            rows.len()
        );
    }
}
