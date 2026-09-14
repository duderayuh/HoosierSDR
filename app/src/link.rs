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
/// A report joined later than this after it ended is recorded, but tells no
/// tripwire: "went to the hospital" forty minutes after the patient arrived
/// is noise in a phone, not news.
pub const NOTIFY_WITHIN_SECS: i64 = 20 * 60;
/// How far back a retry looks for reports that are still unjoined.
pub const RETRY_WITHIN_SECS: i64 = 6 * 3600;

/// Whether a join made `now` about a report that ended at `ended` should
/// still reach the tripwires.
pub fn still_news(now: i64, ended: i64) -> bool {
    now - ended <= NOTIFY_WITHIN_SECS
}

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

/// How many dispatches must use a title before it is a title. The model
/// that reads the dispatches writes what the transcriber heard, so "Medix
/// 21" and "Letter 14" turn up once or twice; a real title turns up every
/// hour.
const TITLE_SEEN: usize = 3;

/// The words this library's dispatches put in front of a unit number.
pub fn vocabulary(c: &Connection) -> HashSet<String> {
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let Ok(mut q) = c.prepare("SELECT units FROM incidents WHERE units <> '[]' ORDER BY id DESC LIMIT 4000") else {
        return HashSet::new();
    };
    let rows = q.query_map([], |r| r.get::<_, String>(0));
    let Ok(rows) = rows else { return HashSet::new() };
    for text in rows.flatten() {
        let list: Vec<String> = serde_json::from_str(&text).unwrap_or_default();
        for u in list {
            if let Some(word) = u.split_whitespace().next() {
                let low = word.to_ascii_lowercase();
                if low.chars().all(|c| c.is_alphabetic()) && low.len() >= 3 {
                    *seen.entry(low).or_default() += 1;
                }
            }
        }
    }
    let common: HashSet<String> = seen.iter().filter(|(_, n)| **n >= TITLE_SEEN).map(|(w, _)| w.clone()).collect();
    // A library with only a handful of dispatches has no title seen three
    // times; better every word it has than none.
    if common.is_empty() {
        seen.into_keys().collect()
    } else {
        common
    }
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

/// Decide, and when nothing the report says fits a run, try what the
/// calling radios are learned to be (`radios.rs`). The reason then says so,
/// because the crew never said it.
pub fn decide_with_learned(
    report: &Report,
    open: &[Dispatch],
    vocabulary: &HashSet<String>,
    learned: &[String],
) -> Link {
    let said = decide(report, open, vocabulary);
    if said != Link::None || learned.is_empty() {
        return said;
    }
    let by_radio = Report {
        at: report.at,
        text: learned.join(", "),
    };
    match decide(&by_radio, open, vocabulary) {
        Link::Made { incident, how } => Link::Made {
            incident,
            how: format!("{how} (the radio that called is learned as {})", learned.join(", ")),
        },
        other => other,
    }
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

/// The callsigns the crew radios in a conversation are learned to be.
fn learned_crew(c: &Connection, pieces: &[crate::conversations::Piece]) -> Vec<String> {
    let system = pieces
        .iter()
        .find_map(|p| p.id)
        .and_then(|id| c.query_row("SELECT system FROM calls WHERE id = ?1", [id], |r| r.get::<_, String>(0)).ok())
        .unwrap_or_default();
    let radios: Vec<(String, u32)> = pieces
        .iter()
        .filter(|p| !p.fixed && p.unit != 0)
        .map(|p| (system.clone(), p.unit))
        .collect();
    let mut out: Vec<String> = crate::radios::identities(c, &radios)
        .into_values()
        .filter(|i| i.usable() && i.role == crate::radios::UNIT)
        .map(|i| i.callsign)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Remember a link. False when the report was already joined — two retries
/// can reach the same report, and only the one that writes the link may
/// announce it.
pub fn attach(c: &Connection, conversation: i64, incident: i64, how: &str) -> Result<bool, String> {
    c.execute(
        "UPDATE conversations SET incident = ?1, link_how = ?2 WHERE id = ?3 AND incident IS NULL",
        params![incident, how, conversation],
    )
    .map(|n| n == 1)
    .map_err(|e| e.to_string())
}

/// Reports from the last `RETRY_WITHIN_SECS` that are still unjoined,
/// optionally only those a given radio took part in.
pub fn unjoined(c: &Connection, now: i64, radio: Option<u32>) -> Vec<i64> {
    let since = now - RETRY_WITHIN_SECS;
    let sql = match radio {
        Some(_) => "SELECT id FROM conversations WHERE incident IS NULL AND first_at >= ?1
                     AND EXISTS (SELECT 1 FROM json_each(conversations.participants) WHERE value = ?2)
                     ORDER BY first_at",
        None => "SELECT id FROM conversations WHERE incident IS NULL AND first_at >= ?1 AND ?2 IS NULL ORDER BY first_at",
    };
    let Ok(mut q) = c.prepare(sql) else { return Vec::new() };
    q.query_map(params![since, radio], |r| r.get(0))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
}

/// Try again to join the recent reports a radio took part in, now that it
/// has a name. The model may break a tie: this runs once per radio, when it
/// is learned or confirmed, not on a timer.
pub fn retry_for_radio(app: &tauri::AppHandle, radio: u32) -> usize {
    use tauri::Manager;
    let state = app.state::<crate::AppState>();
    let Some(db) = state.db.lock().unwrap().clone() else { return 0 };
    let ids = unjoined(&db.lock().unwrap(), crate::library::now(), Some(radio));
    ids.into_iter().filter(|id| join(app, *id, true).is_some()).count()
}

/// Try again to join every recent unjoined report, by rules alone: a timer
/// that asked the model about the same tie every few minutes would only
/// keep it busy.
pub fn retry_recent(app: &tauri::AppHandle) -> usize {
    use tauri::Manager;
    let state = app.state::<crate::AppState>();
    let Some(db) = state.db.lock().unwrap().clone() else { return 0 };
    let ids = unjoined(&db.lock().unwrap(), crate::library::now(), None);
    ids.into_iter().filter(|id| join(app, *id, false).is_some()).count()
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
    /// The findings line the summary call named, when there is one.
    pub headline: String,
    pub summary: String,
    pub how: String,
}

pub fn reports_for(c: &Connection, incident: i64, places: &crate::places::Settings) -> Vec<LinkedReport> {
    let Ok(mut q) = c.prepare(
        "SELECT id, first_at, tg, tg_name, tg_desc, summary, link_how, headline FROM conversations
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
            headline: r.get::<_, Option<String>>(7)?.unwrap_or_default(),
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
    join(app, conversation, true)
}

/// Join one report, asking the model to break a tie only when `ask_model`.
fn join(app: &tauri::AppHandle, conversation: i64, ask_model: bool) -> Option<(i64, String)> {
    use tauri::Manager;
    let state = app.state::<crate::AppState>();
    let db = state.db.lock().unwrap().clone()?;

    let (report, open, vocab, learned, ended) = {
        let c = db.lock().unwrap();
        let (at, summary, transcript, already, pieces, ended): (i64, String, String, Option<i64>, String, i64) = c
            .query_row(
                "SELECT first_at, summary, transcript, incident, pieces, last_at FROM conversations WHERE id = ?1",
                [conversation],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .ok()?;
        if already.is_some() {
            return None;
        }
        let report = Report {
            at,
            text: format!("{summary}\n{transcript}"),
        };
        let pieces: Vec<crate::conversations::Piece> = serde_json::from_str(&pieces).unwrap_or_default();
        (report, open_dispatches(&c, at), vocabulary(&c), learned_crew(&c, &pieces), ended)
    };

    let (incident, how) = match decide_with_learned(&report, &open, &vocab, &learned) {
        Link::Made { incident, how } => (incident, how),
        Link::None => return None,
        Link::Tie { .. } if !ask_model => return None,
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
        if !attach(&c, conversation, incident, &how).ok()? {
            return None;
        }
    }
    let _ = tauri::Emitter::emit(
        app,
        "incident_linked",
        serde_json::json!({ "conversation": conversation, "incident": incident, "how": how }),
    );
    // A tripwire waiting for the outcome — "tell me where the arrest went" —
    // has been waiting for exactly this, unless it is old news by now.
    if !still_news(crate::library::now(), ended) {
        println!("[link] conversation {conversation} joined late; no tripwire told");
        return Some((incident, how));
    }
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
    fn a_crew_that_never_says_its_name_is_joined_by_its_radio_and_says_so() {
        let open = vec![dispatch(10, 1_000, &["Medic 71"]), dispatch(11, 1_100, &["Medic 74"])];
        let quiet = report(2_400, "71, this is the ER, go ahead. Coming in with a 60 year old male.");
        assert_eq!(decide(&quiet, &open, &vocab()), Link::None);
        let got = decide_with_learned(&quiet, &open, &vocab(), &["Medic 71".to_string()]);
        assert_eq!(
            got,
            Link::Made {
                incident: 10,
                how: "Medic 71 was sent to this run (the radio that called is learned as Medic 71)".into()
            }
        );
        // What the crew says still wins over what the radio is learned to be.
        let said = report(2_400, "Medic 74 is inbound");
        assert_eq!(
            decide_with_learned(&said, &open, &vocab(), &["Medic 71".to_string()]),
            Link::Made { incident: 11, how: "Medic 74 was sent to this run".into() }
        );
        // Nothing learned, nothing joined.
        assert_eq!(decide_with_learned(&quiet, &open, &vocab(), &[]), Link::None);
    }

    #[test]
    fn a_late_join_is_recorded_but_is_not_news() {
        assert!(still_news(10_000, 10_000 - NOTIFY_WITHIN_SECS));
        assert!(!still_news(10_000, 10_000 - NOTIFY_WITHIN_SECS - 1));
    }

    #[test]
    fn only_the_first_join_counts_and_retries_find_what_is_left() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE conversations (id INTEGER PRIMARY KEY, first_at INTEGER, participants TEXT, incident INTEGER, link_how TEXT);
             INSERT INTO conversations VALUES (1, 100000, '[900777]', NULL, ''), (2, 100000, '[900888]', NULL, ''),
                                              (3, 1, '[900777]', NULL, ''), (4, 100000, '[900777]', 55, 'x');",
        )
        .unwrap();
        let now = 100000 + 60;
        assert_eq!(unjoined(&c, now, None), vec![1, 2]);
        assert_eq!(unjoined(&c, now, Some(900777)), vec![1], "only that radio's, only recent, only unjoined");
        assert!(attach(&c, 1, 10, "first").unwrap());
        assert!(!attach(&c, 1, 11, "second").unwrap(), "a report already joined is not joined again");
        let how: String = c.query_row("SELECT link_how FROM conversations WHERE id = 1", [], |r| r.get(0)).unwrap();
        assert_eq!(how, "first");
        assert_eq!(unjoined(&c, now, None), vec![2]);
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
             INSERT INTO incidents (units) VALUES ('[\"Medic 41\",\"Rescue 7\"]'), ('[\"Squad 12\"]'), ('[]'),
               ('[\"Medic 2\",\"Rescue 1\",\"Squad 3\"]'), ('[\"Medic 9\",\"Rescue 4\",\"Squad 5\",\"Medix 9\"]');",
        )
        .unwrap();
        let v = vocabulary(&c);
        assert!(v.contains("medic") && v.contains("rescue") && v.contains("squad"));
        // A title heard once is the transcriber, not the county.
        assert!(!v.contains("medix"));
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
        let (mut no_sign, mut no_runs, mut unmatched, mut by_radio) = (0, 0, 0, 0);
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
                    let pieces: String = c
                        .query_row("SELECT pieces FROM conversations WHERE id = ?1", [id], |r| r.get(0))
                        .unwrap_or_default();
                    let pieces: Vec<crate::conversations::Piece> = serde_json::from_str(&pieces).unwrap_or_default();
                    let learned = learned_crew(&c, &pieces);
                    if let Link::Made { incident, how } = decide_with_learned(&report, &open, &v, &learned) {
                        by_radio += 1;
                        let (ct, addr): (String, String) = c
                            .query_row("SELECT call_type, address FROM incidents WHERE id = ?1", [incident], |r| {
                                Ok((r.get(0)?, r.get(1)?))
                            })
                            .unwrap_or_default();
                        println!("  BY RADIO conv {id} ({tg_name}) → run {incident} {ct} @ {addr}  [{how}]");
                        println!("      {}", summary.lines().next().unwrap_or("").trim());
                    }
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
             {unmatched} named a crew no nearby run was sent); {by_radio} of the unjoined joined by a learned radio",
            rows.len()
        );
    }
}
