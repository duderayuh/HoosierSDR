//! Which hospital a report is to.
//!
//! On a hospital's own MED channel the channel says it. On a channel many
//! hospitals answer — IHERN — it has to be worked out for each exchange:
//!
//! 1. A radio the hospital is known to answer on. Methodist answers IHERN
//!    on a radio of its own (9412088) that never talks anywhere else, so an
//!    exchange that radio speaks in is Methodist's.
//! 2. What is said, where it is said. The call-up names who is called
//!    ("Riley ER, Air Vac 145 on IHERN"), the dispatcher's hand-off names
//!    who they are connected to ("Stand by for Methodist"), and the answer
//!    names who answered ("This is Riley, go ahead"). A hospital merely
//!    mentioned — where the patient is coming *from* — counts for little.
//!
//! Most IHERN traffic arrives with no radio ID at all (it comes onto the
//! trunk through a link from the statewide channel), so the words carry
//! most of the weight, and an exchange that names no hospital clearly is
//! left unattributed rather than guessed.

use crate::places::{Place, Settings};

/// Lower case, apostrophes and punctuation gone, "saint" read as "st".
pub fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .replace("'s ", " ")
        .replace("’s ", " ")
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .map(|w| if w == "saint" { "st".to_string() } else { w.to_string() })
        .collect()
}

/// Words that name nothing on their own: a hospital called "North" or
/// "Heart" on the air could be any of several.
const GENERIC: &[&str] = &[
    "north", "south", "east", "west", "heart", "center", "centre", "women", "infants", "children", "childrens", "hospital",
    "medical", "regional", "health", "the", "and", "of", "er", "ed", "st", "street", "vascular", "neighborhood",
];

/// Words a system puts before its hospitals' names.
const PREFIX: &[&str] = &["iu", "health", "st", "vincent", "vincents", "community", "the"];

/// Every way each place is said on the air, as words: its name, the name
/// without the system in front ("Methodist", "86th Street"), "IU" without
/// "Health" ("IU North"), its first distinctive word, and whatever the
/// listener added. A short form two places share is dropped from both.
pub fn aliases(s: &Settings) -> Vec<(usize, Vec<String>)> {
    let mut derived: Vec<(usize, Vec<String>)> = Vec::new();
    let mut given: Vec<(usize, Vec<String>)> = Vec::new();
    for (i, p) in s.places.iter().enumerate().filter(|(_, p)| p.enabled && p.kind == "hospital") {
        let full = words(&p.name);
        for a in &p.aliases {
            let w = words(a);
            if !w.is_empty() {
                given.push((i, w));
            }
        }
        if full.is_empty() {
            continue;
        }
        derived.push((i, full.clone()));
        let no_health: Vec<String> = full.iter().filter(|w| *w != "health").cloned().collect();
        derived.push((i, no_health));
        let mut rest: Vec<String> = full.iter().skip_while(|w| PREFIX.contains(&w.as_str())).cloned().collect();
        while rest.last().is_some_and(|w| ["hospital", "medical", "center"].contains(&w.as_str())) {
            rest.pop();
        }
        // A place known by a number ("86th Street") is also called by the
        // number alone: "86, go ahead", "traffic for 86".
        for w in &rest {
            let digits: String = w.chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.len() >= 2 {
                derived.push((i, vec![w.clone()]));
                derived.push((i, vec![digits]));
            }
        }
        if !rest.is_empty() {
            derived.push((i, rest.clone()));
            let first = &rest[0];
            if first.len() >= 5 && !GENERIC.contains(&first.as_str()) && first.chars().any(|c| c.is_alphabetic()) {
                derived.push((i, vec![first.clone()]));
            }
        }
    }
    let owners = |w: &Vec<String>| {
        let mut o: Vec<usize> = derived.iter().chain(given.iter()).filter(|(_, x)| x == w).map(|(i, _)| *i).collect();
        o.sort_unstable();
        o.dedup();
        o.len()
    };
    let mut out: Vec<(usize, Vec<String>)> = given.clone();
    for (i, w) in &derived {
        let chars: usize = w.iter().map(|x| x.len()).sum();
        let number = w.len() == 1 && w[0].chars().next().is_some_and(|c| c.is_ascii_digit());
        if (chars >= 4 || number) && !(w.len() == 1 && GENERIC.contains(&w[0].as_str())) && owners(w) == 1 && !out.contains(&(*i, w.clone())) {
            out.push((*i, w.clone()));
        }
    }
    // Longest first, so "community north" is read before "north" would be.
    out.sort_by_key(|(_, w)| std::cmp::Reverse(w.len()));
    out
}

/// Where in `ws` an alias is said: its first word's index and its length.
/// A one-word name of six letters or more may be a letter or two off
/// ("Ekanazi" for Eskenazi); shorter names must be exact.
fn find(ws: &[String], alias: &[String]) -> Vec<usize> {
    if alias.is_empty() || ws.len() < alias.len() {
        return Vec::new();
    }
    (0..=ws.len() - alias.len())
        .filter(|&i| {
            if alias.len() == 1 && alias[0].len() >= 6 {
                crate::fuzzy::alike(&ws[i], &alias[0])
            } else {
                ws[i..i + alias.len()] == *alias
            }
        })
        .collect()
}

fn has(ws: &[String], phrase: &str) -> Option<usize> {
    let p = words(phrase);
    (0..ws.len().saturating_sub(p.len() - 1)).find(|&i| ws[i..].starts_with(&p))
}

/// How a hospital's name was said in one transmission.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Said {
    /// Named where the patient is coming from, or in passing.
    Mentioned = 1,
    /// Called: "Riley ER, Air Vac 145", or handed to: "stand by for Methodist".
    Called = 2,
    /// Answered as: "This is Riley, go ahead".
    Answered = 3,
}

/// Which hospitals one transmission names, and how.
pub fn named(text: &str, names: &[(usize, Vec<String>)]) -> Vec<(usize, Said)> {
    let ws = words(text);
    let mut taken = vec![false; ws.len()];
    let mut out: Vec<(usize, Said)> = Vec::new();
    for (place, alias) in names {
        for at in find(&ws, alias) {
            if taken[at..at + alias.len()].iter().any(|t| *t) {
                continue;
            }
            taken[at..at + alias.len()].iter_mut().for_each(|t| *t = true);
            let before = &ws[at.saturating_sub(4)..at];
            let after = &ws[at + alias.len()..(at + alias.len() + 6).min(ws.len())];
            let before_s = before.join(" ");
            let after_s = after.join(" ");
            let said = if before_s.ends_with("this is") || before_s.ends_with("i am") || before_s.ends_with("it s") || before_s.ends_with("this is the") {
                Said::Answered
            } else if after.first().is_some_and(|w| w == "clear")
                || (at <= 1 && (after_s.contains("go ahead") || after_s.contains("clear") || after_s.contains("go for")) && !after_s.contains("this is"))
            {
                Said::Answered
            } else if at <= 1
                || ["good morning", "good afternoon", "good evening", "good night", "hi", "hey", "hello"].iter().any(|g| before_s.ends_with(g))
                || ["stand by for", "standby for", "connect", "patch", "report for", "coming to", "inbound to", "en route to", "enroute to", "headed to", "transport to", "going to", "get a hold of", "reach", "calling"]
                    .iter()
                    .any(|p| before_s.contains(p))
            {
                Said::Called
            } else {
                Said::Mentioned
            };
            out.push((*place, said));
        }
    }
    out
}

/// A transmission that opens a new exchange: someone hailing a hospital or
/// dispatch ("Indianapolis EMS, Indianapolis EMS, Star Medic 1 on the
/// IHERN"; "Riley ER, Air Vac 145"), not a hospital answering.
pub fn is_call_up(text: &str, names: &[(usize, Vec<String>)]) -> bool {
    let ws = words(text);
    if ws.len() < 3 {
        return false;
    }
    let lead = ws[..ws.len().min(8)].join(" ");
    // Dispatch is hailed as "Indianapolis" with whatever the transcriber
    // makes of the rest ("EMS", "DMS", "VMS", "Medical").
    let dispatch = ["indianapolis", "indy ems"].iter().any(|d| lead.starts_with(d));
    let hospital_first = named(text, names).iter().any(|(_, s)| *s == Said::Called)
        && names.iter().any(|(_, a)| ws.starts_with(a) || (a.len() == 1 && a[0].len() >= 6 && crate::fuzzy::alike(&ws[0], &a[0])));
    let answers = named(text, names).iter().any(|(_, s)| *s == Said::Answered);
    let doubled = ws.len() >= 4 && (1..=3).any(|n| ws.len() >= 2 * n && ws[..n] == ws[n..2 * n]);
    let who = has(&ws, "this is").is_some() || has(&ws, "from").is_some() || has(&ws, "on the").is_some() || ws.iter().any(|w| ["medic", "flight", "lifeline", "ambulance", "ground", "air", "aerovac", "rescue"].contains(&w.as_str()))
        // A unit's number, or the channel named ("on IHERN", however heard).
        || ws.iter().any(|w| w.chars().all(|c| c.is_ascii_digit()) || w.starts_with("ih"));
    !answers && ((dispatch && (who || doubled)) || (hospital_first && who) || (doubled && who))
}

/// What the transcriber writes for a carrier, a tone or dead air: the
/// thousands of "Thank you." on IHERN are not anyone speaking.
pub fn is_noise(text: &str) -> bool {
    let t: String = text.trim().trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
    matches!(t.as_str(), "" | "thank you" | "you" | "bye" | "bye bye" | "bye-bye" | "pfft" | "thanks for watching" | "thank you for watching")
}

/// Which hospital an exchange is with, and why.
#[derive(Clone, Debug, PartialEq)]
pub struct Whose {
    pub place_id: String,
    pub name: String,
    pub how: String,
}

/// The hospital a report on `tg` is to, from its transmissions as (radio,
/// what was said). A channel a single place claims is that place's; on a
/// shared channel it is worked out as above, or None when nothing says.
pub fn whose(s: &Settings, tg: u16, pieces: &[(u32, String)]) -> Option<Whose> {
    if !s.shared_tgs.contains(&tg) {
        return crate::places::for_tg(s, tg, "").map(|p| Whose { place_id: p.id.clone(), name: p.name.clone(), how: "its own channel".into() });
    }
    let by_radio = |p: &Place| pieces.iter().any(|(u, _)| *u != 0 && p.radios.contains(u));
    if let Some(p) = s.places.iter().find(|p| p.enabled && by_radio(p)) {
        return Some(Whose { place_id: p.id.clone(), name: p.name.clone(), how: "its own radio answered".into() });
    }
    let names = aliases(s);
    let mut score: std::collections::BTreeMap<usize, (u32, Said)> = std::collections::BTreeMap::new();
    for (_, text) in pieces.iter().filter(|(_, t)| !is_noise(t)) {
        // In a call-up, a hospital named is the one asked for — unless it
        // is where the patient is coming from.
        let up = is_call_up(text, &names);
        let ws = words(text);
        for (place, said) in named(text, &names) {
            let from = names.iter().filter(|(p, _)| *p == place).flat_map(|(_, a)| find(&ws, a)).any(|at| {
                let b = ws[at.saturating_sub(3)..at].join(" ");
                b.ends_with("from") || b.ends_with("out of") || b.ends_with("transfer from")
            });
            let said = if up && said == Said::Mentioned && !from { Said::Called } else { said };
            let e = score.entry(place).or_insert((0, Said::Mentioned));
            e.0 += said as u32;
            e.1 = e.1.max(said);
        }
    }
    let mut ranked: Vec<(usize, u32, Said)> = score.into_iter().map(|(p, (n, s))| (p, n, s)).collect();
    ranked.sort_by_key(|(_, n, s)| std::cmp::Reverse((*s, *n)));
    let (best, n, said) = *ranked.first()?;
    // Called or answered, and clearly ahead of anyone else named.
    if said < Said::Called || ranked.get(1).is_some_and(|(_, m, s2)| *s2 == said && *m * 2 > n) {
        return None;
    }
    let p = &s.places[best];
    let how = match said {
        Said::Answered => "answered by name",
        _ => "called by name",
    };
    Some(Whose { place_id: p.id.clone(), name: p.name.clone(), how: how.into() })
}

/// The transmissions of one quiet-bounded stretch of a shared channel, cut
/// into exchanges: a call-up starts a new one once the current one has had
/// its say. The transcriber's filler is left out. Each item is the index
/// of a transmission in `texts`.
pub fn exchanges(texts: &[&str], names: &[(usize, Vec<String>)]) -> Vec<Vec<usize>> {
    let mut out: Vec<Vec<usize>> = Vec::new();
    let mut cur: Vec<usize> = Vec::new();
    let mut last_was_call_up = false;
    for (i, t) in texts.iter().enumerate() {
        if is_noise(t) {
            continue;
        }
        let up = is_call_up(t, names);
        // A hail said twice is one call-up; a call-up after real talk is a
        // new exchange.
        if up && !last_was_call_up && cur.len() >= 2 {
            out.push(std::mem::take(&mut cur));
        }
        cur.push(i);
        last_was_call_up = up;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// A radio the library has heard answering as a place.
#[derive(serde::Serialize, Clone, Debug, PartialEq)]
pub struct HeardRadio {
    pub place_id: String,
    pub place: String,
    pub radio: u32,
    /// Its transmissions, and how many said it was this place.
    pub calls: u32,
    pub answered: u32,
    /// It talks on a shared channel (IHERN), not the place's own.
    pub shared: bool,
    pub example: String,
}

/// The radios each place answers on, read off the library: a radio that
/// keeps to hospital channels (its own and the shared ones) and says it is
/// this place, again and again. A flight crew keeps to hospital channels
/// too, but says it is the crew; a dispatcher's console says it is
/// Indianapolis and works everywhere.
pub fn heard_radios(c: &rusqlite::Connection, s: &Settings) -> Vec<HeardRadio> {
    let names = aliases(s);
    let home: std::collections::HashSet<u16> = s.places.iter().filter(|p| p.enabled).flat_map(|p| p.tgs.iter().copied()).chain(s.shared_tgs.iter().copied()).collect();
    if home.is_empty() {
        return Vec::new();
    }
    let list = home.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(",");
    let totals: std::collections::HashMap<u32, u32> = c
        .prepare(&format!("SELECT unit, COUNT(*) FROM calls WHERE unit IN (SELECT DISTINCT unit FROM calls WHERE tg IN ({list}) AND unit <> 0) GROUP BY unit"))
        .and_then(|mut q| q.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map(|rows| rows.flatten().collect()))
        .unwrap_or_default();
    let rows: Vec<(u32, u16, String)> = c
        .prepare(&format!("SELECT unit, tg, COALESCE(transcript_edited, transcript, '') FROM calls WHERE tg IN ({list}) AND unit <> 0"))
        .and_then(|mut q| q.query_map([], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u16, r.get(2)?))).map(|rows| rows.flatten().collect()))
        .unwrap_or_default();
    struct Tally {
        home: u32,
        shared: bool,
        answered: std::collections::BTreeMap<usize, (u32, String)>,
    }
    let mut by: std::collections::BTreeMap<u32, Tally> = Default::default();
    for (unit, tg, text) in rows {
        let t = by.entry(unit).or_insert(Tally { home: 0, shared: false, answered: Default::default() });
        t.home += 1;
        t.shared |= s.shared_tgs.contains(&tg);
        for (place, said) in named(&text, &names) {
            if said == Said::Answered && words(&text).len() <= 20 {
                let e = t.answered.entry(place).or_insert((0, String::new()));
                e.0 += 1;
                if e.1.is_empty() {
                    e.1 = text.trim().chars().take(80).collect();
                }
            }
        }
    }
    let mut out = Vec::new();
    for (radio, t) in by {
        let total = totals.get(&radio).copied().unwrap_or(t.home).max(t.home);
        let said_any: u32 = t.answered.values().map(|(n, _)| n).sum();
        let Some((&place, (n, example))) = t.answered.iter().max_by_key(|(_, (n, _))| *n) else { continue };
        // Keeps to hospital channels, and is this place far more than any other.
        if t.home * 5 < total * 4 || *n < 3 || n * 5 < said_any * 3 {
            continue;
        }
        let p = &s.places[place];
        if p.radios.contains(&radio) {
            continue;
        }
        out.push(HeardRadio { place_id: p.id.clone(), place: p.name.clone(), radio, calls: total, answered: *n, shared: t.shared, example: example.clone() });
    }
    out.sort_by(|a, b| a.place.cmp(&b.place).then(b.answered.cmp(&a.answered)));
    out
}

#[tauri::command]
pub async fn places_heard_radios(app: tauri::AppHandle) -> Result<Vec<HeardRadio>, String> {
    use tauri::Manager;
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<crate::AppState>();
        let s = state.places.lock().unwrap().settings.clone();
        let db = state.db.lock().unwrap().clone().ok_or("the call library is not open")?;
        let c = db.lock().unwrap();
        Ok(heard_radios(&c, &s))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Replays the library's IHERN traffic: what each exchange would be
    /// credited to. `cargo test ihern_replay -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn ihern_replay() {
        let home = std::env::var("HOME").unwrap();
        let dir = format!("{home}/Library/Application Support/com.hoosiersdr.app");
        let mut s: Settings = serde_json::from_str(&std::fs::read_to_string(format!("{dir}/places.json")).unwrap()).unwrap();
        crate::places::sanitize(&mut s);
        s.shared_tgs = vec![10254];
        for p in s.places.iter_mut() {
            if p.name == "IU Health Methodist" {
                p.radios = vec![9412088];
            }
            if p.name == "IU Health - Fishers" {
                p.aliases = vec!["Riley Fishers".into(), "IU Fishers".into()];
            }
        }
        let c = rusqlite::Connection::open_with_flags(format!("{dir}/library/calls.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let rows: Vec<(i64, u32, f64, String)> = c
            .prepare("SELECT start, unit, secs, COALESCE(transcript_edited, transcript, '') FROM calls WHERE tg = 10254 ORDER BY start")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .flatten()
            .collect();
        let names = aliases(&s);
        // Quiet-bounded stretches, as the conversation engine ends them.
        let mut stretches: Vec<Vec<usize>> = Vec::new();
        for (i, r) in rows.iter().enumerate() {
            match stretches.last_mut() {
                Some(st) if r.0 - (rows[*st.last().unwrap()].0 + rows[*st.last().unwrap()].2 as i64) <= 90 => st.push(i),
                _ => stretches.push(vec![i]),
            }
        }
        let mut tally: std::collections::BTreeMap<String, usize> = Default::default();
        let (mut n, mut split) = (0, 0);
        for st in &stretches {
            let texts: Vec<&str> = st.iter().map(|i| rows[*i].3.as_str()).collect();
            let ex = exchanges(&texts, &names);
            if ex.len() > 1 {
                split += 1;
            }
            for e in ex {
                let pieces: Vec<(u32, String)> = e.iter().map(|j| (rows[st[*j]].1, rows[st[*j]].3.clone())).collect();
                if pieces.iter().map(|(_, t)| t.len()).sum::<usize>() < 40 {
                    continue;
                }
                n += 1;
                let w = whose(&s, 10254, &pieces);
                let chars: usize = pieces.iter().filter(|(_, t)| !is_noise(t)).map(|(_, t)| t.len()).sum();
                let key = w.as_ref().map(|w| format!("{} ({})", w.name, w.how)).unwrap_or(if chars >= 300 { "— nobody clear, a real report".into() } else { "— nobody clear, chatter".into() });
                *tally.entry(key.clone()).or_default() += 1;
                let when = crate::library::local_fmt(rows[st[e[0]]].0, "%m-%d %H:%M");
                let first: Vec<String> = pieces.iter().take(if w.is_some() { 12 } else { 3 }).map(|(u, t)| format!("[{u}] {}", t.chars().take(70).collect::<String>())).collect();
                println!("{when} → {key}\n    {}", first.join("\n    "));
            }
        }
        println!("\n{n} exchanges ({split} stretches split in more than one)");
        for (k, v) in tally {
            println!("{v:4}  {k}");
        }
    }

    #[test]
    fn a_place_is_known_by_the_radio_that_keeps_saying_it_is_it() {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE calls (id INTEGER PRIMARY KEY, unit INTEGER, tg INTEGER, transcript TEXT, transcript_edited TEXT);").unwrap();
        let mut add = |unit: u32, tg: u16, text: &str, n: usize| {
            for _ in 0..n {
                c.execute("INSERT INTO calls (unit, tg, transcript) VALUES (?1, ?2, ?3)", rusqlite::params![unit, tg, text]).unwrap();
            }
        };
        add(9412088, 10254, "This is Methodist, go ahead.", 4); // Methodist's IHERN radio
        add(31709, 10256, "Medic 21, this is Methodist.", 5); // its MED 3 console
        add(9399208, 10256, "Methodist Hospital, Lifeline 2 on Med Channel 3", 6); // a flight crew calling
        add(790042, 10254, "This is Indianapolis EMS, go ahead.", 3); // the dispatcher…
        add(790042, 10203, "Engine 5 respond", 40); // …who works everywhere
        add(4911300, 10258, "This is Riley.", 2); // said too few times to be sure
        let mut s = book();
        s.places[0].radios.clear();
        let got: Vec<(String, u32, bool)> = heard_radios(&c, &s).into_iter().map(|h| (h.place_id, h.radio, h.shared)).collect();
        assert_eq!(got, vec![("m".into(), 31709, false), ("m".into(), 9412088, true)]);
        // A radio already on the place is not suggested again.
        s.places[0].radios = vec![9412088];
        assert_eq!(heard_radios(&c, &s).len(), 1);
    }

    /// The radios the real library would suggest.
    /// `cargo test heard_radios_replay -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn heard_radios_replay() {
        let home = std::env::var("HOME").unwrap();
        let dir = format!("{home}/Library/Application Support/com.hoosiersdr.app");
        let mut s: Settings = serde_json::from_str(&std::fs::read_to_string(format!("{dir}/places.json")).unwrap()).unwrap();
        crate::places::sanitize(&mut s);
        s.shared_tgs = vec![10254];
        let c = rusqlite::Connection::open_with_flags(format!("{dir}/library/calls.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        for h in heard_radios(&c, &s) {
            println!("{:<45} {:>8} {:>5} calls {:>3} said {} {}", h.place, h.radio, h.calls, h.answered, if h.shared { "IHERN" } else { "     " }, h.example);
        }
    }

    fn place(id: &str, name: &str, tgs: &[u16]) -> Place {
        Place { id: id.into(), name: name.into(), kind: "hospital".into(), enabled: true, tgs: tgs.to_vec(), ..Default::default() }
    }

    fn book() -> Settings {
        let mut methodist = place("m", "IU Health Methodist", &[10256]);
        methodist.radios = vec![9412088];
        let mut fishers = place("f", "IU Health - Fishers", &[10272]);
        fishers.aliases = vec!["Riley Fishers".into()];
        Settings {
            places: vec![
                methodist,
                place("r", "IU Health Riley Children's", &[10258]),
                place("e", "Eskenazi", &[10255]),
                place("v86", "St. Vincent's - 86th Street", &[10259]),
                place("cn", "Community North", &[10261]),
                place("iun", "IU Health North", &[10267]),
                place("fs", "Franciscan Health South", &[10266]),
                place("fh", "Franciscan Health", &[10268]),
                fishers,
            ],
            shared_tgs: vec![10254],
        }
    }

    fn said(texts: &[(u32, &str)]) -> Vec<(u32, String)> {
        texts.iter().map(|(u, t)| (*u, t.to_string())).collect()
    }

    #[test]
    fn a_hospital_is_known_by_its_short_names_and_not_by_a_shared_one() {
        let s = book();
        let a = aliases(&s);
        let has = |i: usize, w: &str| a.contains(&(i, words(w)));
        assert!(has(0, "methodist") && has(0, "iu methodist"));
        assert!(has(1, "riley"));
        assert!(has(3, "86th street"));
        assert!(has(5, "iu north") && has(4, "community north"));
        assert!(has(6, "franciscan south"));
        // "North" and "Franciscan" each belong to two places.
        assert!(!a.iter().any(|(_, w)| w == &words("north")));
        assert!(!a.iter().any(|(_, w)| w == &words("franciscan")));
        assert!(has(8, "riley fishers"));
    }

    #[test]
    fn how_a_name_is_said() {
        let a = aliases(&book());
        assert_eq!(named("This is Methodist. Go ahead.", &a), vec![(0, Said::Answered)]);
        assert_eq!(named("Riley ER, Air Vac 145 on IHERN.", &a), vec![(1, Said::Called)]);
        assert_eq!(named("Stand by for Methodist, please.", &a), vec![(0, Said::Called)]);
        assert_eq!(named("We require nothing further. Riley, clear.", &a), vec![(1, Said::Answered)]);
        assert_eq!(named("Medic 18, this is Ekanazi.", &a), vec![(2, Said::Answered)]);
        assert_eq!(named("a direct admit coming out of Eskenazi", &a), vec![(2, Said::Mentioned)]);
        assert_eq!(named("Good evening, Methodist. We are in the facility", &a), vec![(0, Said::Called)]);
        assert_eq!(named("we will see you on arrival, 86 clear.", &a), vec![(3, Said::Answered)]);
    }

    #[test]
    fn a_call_up_opens_an_exchange_and_an_answer_does_not() {
        let a = aliases(&book());
        assert!(is_call_up("Indianapolis EMS, Indianapolis EMS, Star Medic 1 on the IHERN.", &a));
        assert!(is_call_up("Riley ER, Airy Bag 145 on IHART.", &a));
        assert!(is_call_up("Indianapolis EMS, this is stat flight six with a patient report for St. Vincent.", &a));
        assert!(!is_call_up("This is Methodist. Go ahead.", &a));
        assert!(!is_call_up("Transcare, this is Riley. Go ahead.", &a));
        assert!(!is_call_up("Stand by for Riley.", &a));
        assert!(!is_call_up("Thank you.", &a));
    }

    #[test]
    fn the_transcribers_filler_is_noise() {
        assert!(is_noise("Thank you.") && is_noise("  ") && is_noise("Bye-bye.") && is_noise("!"));
        assert!(!is_noise("Go ahead.") && !is_noise("Thank you, Riley clear."));
    }

    #[test]
    fn an_ihern_exchange_is_credited_to_the_hospital_it_was_with() {
        let s = book();
        // Methodist's own IHERN radio answered.
        let w = whose(&s, 10254, &said(&[(0, "Indianapolis EMS, Lifeline 2, can you connect us with Riley?"), (9412088, "This is Methodist, go ahead."), (0, "Good evening, 54 year old female")])).unwrap();
        assert_eq!((w.place_id.as_str(), w.how.as_str()), ("m", "its own radio answered"));
        // By name: called, handed to, answered.
        let w = whose(&s, 10254, &said(&[(0, "Riley ER, Air Vac 145 on IHERN."), (0, "Air Vac 145, this is Riley, go ahead."), (0, "Coming out of Eskenazi with a 5 year old")])).unwrap();
        assert_eq!((w.place_id.as_str(), w.how.as_str()), ("r", "answered by name"));
        let w = whose(&s, 10254, &said(&[(790043, "Stand by for Methodist."), (0, "Thank you."), (0, "Good morning, 29 year old female from South Bend")])).unwrap();
        assert_eq!(w.place_id, "m");
        // Asked for in the call-up; where the patient comes from is not.
        let w = whose(&s, 10254, &said(&[(0, "Indianapolis, AMC 2092, for 86th Street."), (0, "Go ahead.")])).unwrap();
        assert_eq!(w.place_id, "v86");
        assert_eq!(whose(&s, 10254, &said(&[(0, "Indianapolis EMS, Lutheran Ground 1, transfer from Eskenazi.")])), None);
        // A hospital only mentioned, or two called equally, is nobody's.
        assert_eq!(whose(&s, 10254, &said(&[(0, "Indianapolis EMS, Medic 3, transfer out of Eskenazi")])), None);
        assert_eq!(whose(&s, 10254, &said(&[(0, "Thank you."), (0, "Thank you.")])), None);
        // A place's own channel is that place, whatever is said on it.
        assert_eq!(whose(&s, 10258, &said(&[(0, "This is Methodist")])).unwrap().place_id, "r");
        assert_eq!(whose(&s, 10272, &said(&[(50658, "Riley Fishers, go ahead")])).unwrap().place_id, "f");
    }
}
