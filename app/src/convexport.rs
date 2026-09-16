//! One hospital conversation, and everything about it, as a zip to keep.
//!
//! What a listener looks at on a conversation's page is spread over the
//! library: the row, its transmissions' recordings, the run it was joined
//! to. The archive gathers it in one file that opens anywhere — words first,
//! so it reads without the app, then the audio, each transmission on its own
//! and all of them as one clip.

use rusqlite::{Connection, OptionalExtension};
use std::io::Write;

use crate::conversations::Stored;

/// A finished archive.
pub struct Archive {
    pub name: String,
    pub bytes: Vec<u8>,
}

fn when(epoch: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(epoch, 0)
        .single()
        .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

/// Letters, digits and dashes: safe in a file name on every system.
fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-').chars().take(48).collect()
}

/// The archive's file name: when, the incident number of the run it was
/// joined to (the one its case messages carry), where, and which
/// conversation.
pub fn file_name(r: &Stored, incident: Option<i64>) -> String {
    use chrono::TimeZone;
    let at = chrono::Local
        .timestamp_opt(r.first_at, 0)
        .single()
        .map(|t| t.format("%Y-%m-%d_%H%M").to_string())
        .unwrap_or_default();
    let place = slug(if r.tg_desc.is_empty() { &r.tg_name } else { &r.tg_desc });
    let number = incident.map(|i| format!("incident-{i}")).unwrap_or_default();
    [at, number, place, slug(&r.conv_id)].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("_") + ".zip"
}

/// Who spoke a transmission, for its file name and the overview.
fn who(p: &crate::conversations::Piece, r: &Stored) -> String {
    p.unit_name
        .clone()
        .or_else(|| r.learned.get(&p.unit).cloned())
        .unwrap_or_else(|| if p.fixed { "hospital".into() } else { format!("radio {}", p.unit) })
}

fn overview(r: &Stored, incident: Option<&crate::dispatch::Incident>) -> String {
    let mut o = String::new();
    let line = |o: &mut String, k: &str, v: &str| {
        if !v.trim().is_empty() {
            o.push_str(&format!("{k:<16}{v}\n"));
        }
    };
    o.push_str(&format!("{}\n\n", if r.headline.is_empty() { r.conv_id.as_str() } else { r.headline.as_str() }));
    line(&mut o, "Conversation", &r.conv_id);
    line(&mut o, "Hospital", if r.tg_desc.is_empty() { &r.tg_name } else { &r.tg_desc });
    line(&mut o, "Talkgroup", &format!("{} · TG {}", r.tg_name, r.tg));
    line(&mut o, "Started", &when(r.first_at));
    line(&mut o, "Ended", &format!("{} ({} s)", when(r.last_at), (r.last_at - r.first_at).max(0)));
    line(&mut o, "Units", &r.units.join(", "));
    line(&mut o, "Expected", r.eta.as_deref().unwrap_or(""));
    line(&mut o, "Rule", &r.rule_name);
    line(&mut o, "Status", &format!("{}{}", r.status, if r.detail.is_empty() { String::new() } else { format!(" — {}", r.detail) }));
    if r.sent_at > 0 {
        line(&mut o, "Sent", &when(r.sent_at));
    }
    if let Some(i) = incident {
        o.push_str("\nThe run it was joined to\n");
        line(&mut o, "Incident", &format!("#{}", i.id));
        line(&mut o, "Call type", &i.call_type);
        line(&mut o, "Address", &i.address);
        line(&mut o, "Units", &i.units.join(", "));
        line(&mut o, "Dispatched", &when(i.created));
        line(&mut o, "Summary", &i.summary);
    }
    if !r.summary.is_empty() {
        o.push_str(&format!("\nSummary\n{}\n", r.summary));
    }
    if !r.facts.is_empty() {
        o.push_str("\nWhat the report said\n");
        for f in &r.facts {
            o.push_str(&format!("  {}: {}\n", f.key, f.value));
        }
    }
    o.push_str("\nTransmissions\n");
    for (n, p) in r.pieces.iter().enumerate() {
        // An announcement row carries neither sound nor words; numbers still
        // follow the recordings' own, so the list and audio/ agree.
        if p.secs == 0.0 && p.audio.is_none() && p.transcript.is_none() {
            continue;
        }
        o.push_str(&format!(
            "  {:>2}. {}  {:>5.1} s  {}{}\n",
            n + 1,
            crate::library::local_hms(p.at),
            p.secs,
            who(p, r),
            p.transcript.as_deref().map(|t| format!(": {t}")).unwrap_or_default()
        ));
    }
    o.push_str("\nIn this archive: summary.txt, transcript.txt, message.txt (what was sent), prompt.txt (what the model was asked), conversation.json (the whole record), incident.json (the run, when there is one), and audio/ — each transmission, and all of them as one clip.\n");
    o
}

/// Build the archive. `combine` joins the recordings into one clip (the
/// app's own `combine_clips`); `None` from it leaves the joined clip out.
pub fn build(h: &Held, combine: &dyn Fn(&[String]) -> Option<(std::path::PathBuf, bool)>) -> Result<Archive, String> {
    let (r, incident) = (&h.row, &h.incident);
    let incident_id = incident.as_ref().map(|i| i.id);

    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    // Every file dated when the conversation began, rather than 1980.
    let dated = {
        use chrono::{Datelike, TimeZone, Timelike};
        let base = zip::write::SimpleFileOptions::default();
        match chrono::Local.timestamp_opt(r.first_at, 0).single().and_then(|t| {
            zip::DateTime::from_date_and_time(t.year() as u16, t.month() as u8, t.day() as u8, t.hour() as u8, t.minute() as u8, t.second() as u8).ok()
        }) {
            Some(dt) => base.last_modified_time(dt),
            None => base,
        }
    };
    let text = dated.compression_method(zip::CompressionMethod::Deflated);
    // Recordings are compressed already.
    let stored = dated.compression_method(zip::CompressionMethod::Stored);
    let put = |zip: &mut zip::ZipWriter<std::io::Cursor<Vec<u8>>>, name: &str, bytes: &[u8], opts| -> Result<(), String> {
        zip.start_file(name, opts).map_err(|e| e.to_string())?;
        zip.write_all(bytes).map_err(|e| e.to_string())
    };

    put(&mut zip, "README.txt", overview(r, incident.as_ref()).as_bytes(), text)?;
    let mut summary = String::new();
    if !r.headline.is_empty() {
        summary.push_str(&format!("{}\n\n", r.headline));
    }
    summary.push_str(if r.summary.is_empty() { "(no summary)" } else { &r.summary });
    summary.push('\n');
    if let Some(eta) = &r.eta {
        summary.push_str(&format!("\nExpected: {eta}\n"));
    }
    for f in &r.facts {
        summary.push_str(&format!("{}: {}\n", f.key, f.value));
    }
    put(&mut zip, "summary.txt", summary.as_bytes(), text)?;
    put(&mut zip, "transcript.txt", r.transcript.as_bytes(), text)?;
    put(&mut zip, "message.txt", r.message.as_bytes(), text)?;
    put(&mut zip, "prompt.txt", r.prompt.as_bytes(), text)?;
    let record = serde_json::to_vec_pretty(r).map_err(|e| e.to_string())?;
    put(&mut zip, "conversation.json", &record, text)?;
    if let Some(i) = incident {
        put(&mut zip, "incident.json", &serde_json::to_vec_pretty(i).map_err(|e| e.to_string())?, text)?;
    }

    let mut files = Vec::new();
    for (n, p) in r.pieces.iter().enumerate() {
        let Some(path) = p.audio.as_deref().filter(|a| !a.is_empty()) else { continue };
        let Ok(bytes) = std::fs::read(path) else { continue };
        let ext = std::path::Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("wav");
        let clock = crate::library::local_hms(p.at).replace(':', "");
        let name = format!("audio/{:02}-{clock}-{}.{ext}", n + 1, slug(&who(p, r)));
        put(&mut zip, &name, &bytes, stored)?;
        files.push(path.to_string());
    }
    if files.len() > 1 {
        if let Some((joined, mp3)) = combine(&files) {
            if let Ok(bytes) = std::fs::read(&joined) {
                put(&mut zip, if mp3 { "audio/all.mp3" } else { "audio/all.wav" }, &bytes, stored)?;
            }
            let _ = std::fs::remove_file(&joined);
        }
    }

    let bytes = zip.finish().map_err(|e| e.to_string())?.into_inner();
    Ok(Archive { name: file_name(r, incident_id), bytes })
}

/// Everything the library holds about a conversation, read in one go. The
/// archive is then put together — clips read, joined through ffmpeg, the
/// whole thing deflated — with the library's lock let go, since every live
/// path (dispatch, cases, tripwires) waits on it.
pub struct Held {
    pub row: Stored,
    pub incident: Option<crate::dispatch::Incident>,
}

pub fn hold(c: &Connection, id: i64) -> Result<Held, String> {
    let row = crate::conversations::stored_with_names(c, id)?;
    let incident_id: Option<i64> = c
        .query_row("SELECT incident FROM conversations WHERE id = ?1", [id], |r| r.get(0))
        .optional()
        .map_err(|e| e.to_string())?
        .flatten();
    let incident = match incident_id {
        Some(id) => crate::dispatch::inc_get(c, id)?,
        None => None,
    };
    Ok(Held { row, incident })
}

/// The archive for a conversation in the library, with the clips joined by
/// the app's own encoder.
pub fn archive(held: &Held) -> Result<Archive, String> {
    let id = held.row.id;
    build(held, &|files| crate::alerts::combine_clips(files, &format!("conv_export_{id}")).ok())
}

/// Save a conversation's archive in the Downloads folder; the path it went to.
#[tauri::command]
pub fn conversation_export(state: tauri::State<crate::AppState>, id: i64) -> Result<String, String> {
    let held = crate::with_db(&state, |c| hold(c, id))?;
    let a = archive(&held)?;
    let dir = std::path::PathBuf::from(crate::shellexpand_home("~/Downloads"));
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut path = dir.join(&a.name);
    let stem = a.name.trim_end_matches(".zip").to_string();
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{stem}-{n}.zip"));
        n += 1;
    }
    std::fs::write(&path, &a.bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversations::Piece;
    use std::io::Read;

    fn row(dir: &std::path::Path) -> Stored {
        let a = dir.join("a.m4a");
        let b = dir.join("b.m4a");
        std::fs::write(&a, b"AAAA").unwrap();
        std::fs::write(&b, b"BBBB").unwrap();
        let piece = |id, unit, fixed, at, audio: Option<&std::path::Path>, name: Option<&str>, text: &str| Piece {
            id: Some(id),
            unit,
            unit_name: name.map(String::from),
            fixed,
            at,
            secs: 4.0,
            audio: audio.map(|p| p.to_string_lossy().into_owned()),
            transcript: Some(text.into()),
        };
        Stored {
            id: 3,
            conv_id: "CONV-10256-1000000".into(),
            rule_id: "c1".into(),
            rule_name: "Hospitals".into(),
            tg: 10256,
            tg_name: "49M-M03".into(),
            tg_desc: "Example General ER".into(),
            first_at: 1_000_000,
            last_at: 1_000_060,
            sent_at: 1_000_090,
            revision: 0,
            status: "sent".into(),
            detail: "sent (3 pieces)".into(),
            headline: "Cardiac arrest, ROSC".into(),
            summary: "Medic 7 inbound with a patient in cardiac arrest, ROSC achieved.".into(),
            message: "🏥 Example General ER\nMedic 7 inbound.".into(),
            prompt: "Summarise this report.".into(),
            transcript: "MEDIC 7: inbound with ROSC\nHOSPITAL: copy".into(),
            chat: String::new(),
            participants: vec![5],
            pieces: vec![
                piece(1, 5, false, 1_000_000, Some(&a), Some("Medic 7"), "inbound with ROSC"),
                piece(2, 9, true, 1_000_030, Some(&b), None, "copy"),
                piece(3, 5, false, 1_000_050, None, Some("Medic 7"), "thanks"),
            ],
            calls: 3,
            source: "live".into(),
            units: vec!["Medic 7".into()],
            eta: Some("10 minutes".into()),
            facts: vec![crate::conversations::Fact { key: "rhythm".into(), value: "VF".into() }],
            learned: Default::default(),
        }
    }

    /// Against a copy of a real library: `HS_EXPORT_DB=copy.db HS_EXPORT_OUT=dir
    /// cargo test convexport::tests::real_library -- --ignored --nocapture`
    /// writes the newest conversation's archive into that folder.
    #[test]
    #[ignore]
    fn real_library() {
        let c = Connection::open(std::env::var("HS_EXPORT_DB").unwrap()).unwrap();
        let id: i64 = c.query_row("SELECT id FROM conversations WHERE calls > 2 ORDER BY last_at DESC LIMIT 1", [], |r| r.get(0)).unwrap();
        let a = archive(&hold(&c, id).unwrap()).unwrap();
        let out = std::path::PathBuf::from(std::env::var("HS_EXPORT_OUT").unwrap()).join(&a.name);
        std::fs::write(&out, &a.bytes).unwrap();
        println!("{} ({} bytes)", out.display(), a.bytes.len());
    }

    #[test]
    fn a_conversation_archive_holds_the_words_and_every_recording() {
        let dir = std::env::temp_dir().join(format!("hs-convexport-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE conversations (id INTEGER PRIMARY KEY, incident INTEGER); INSERT INTO conversations VALUES (3, NULL);").unwrap();
        let r = row(&dir);
        let joined = dir.join("joined.mp3");
        let asked = std::cell::RefCell::new(Vec::new());
        let held = Held { row: r.clone(), incident: None };
        let a = build(&held, &|files| {
            asked.borrow_mut().extend(files.iter().cloned());
            std::fs::write(&joined, b"JOINED").unwrap();
            Some((joined.clone(), true))
        })
        .unwrap();
        assert!(a.name.ends_with("_Example-General-ER_CONV-10256-1000000.zip"), "{}", a.name);
        assert!(file_name(&r, Some(11)).ends_with("_incident-11_Example-General-ER_CONV-10256-1000000.zip"), "joined to a run, it carries the run's number");

        let mut z = zip::ZipArchive::new(std::io::Cursor::new(a.bytes)).unwrap();
        let names: Vec<String> = (0..z.len()).map(|i| z.by_index(i).unwrap().name().to_string()).collect();
        for want in ["README.txt", "summary.txt", "transcript.txt", "message.txt", "prompt.txt", "conversation.json", "audio/all.mp3"] {
            assert!(names.iter().any(|n| n == want), "{want} missing from {names:?}");
        }
        let audio: Vec<&String> = names.iter().filter(|n| n.starts_with("audio/0")).collect();
        assert_eq!(audio.len(), 2, "one file per recording that exists: {names:?}");
        assert!(audio[0].ends_with("-Medic-7.m4a") && audio[1].ends_with("-hospital.m4a"), "{audio:?}");
        assert!(!names.iter().any(|n| n == "incident.json"), "no run, no incident file");
        assert_eq!(asked.borrow().len(), 2, "the joined clip is made from the recordings that exist");

        let read = |z: &mut zip::ZipArchive<std::io::Cursor<Vec<u8>>>, n: &str| {
            let mut s = String::new();
            z.by_name(n).unwrap().read_to_string(&mut s).unwrap();
            s
        };
        let readme = read(&mut z, "README.txt");
        assert!(readme.contains("Example General ER") && readme.contains("Medic 7: inbound with ROSC") && readme.contains("rhythm: VF"), "{readme}");
        assert!(read(&mut z, "transcript.txt").contains("HOSPITAL: copy"));
        let mut bytes = Vec::new();
        z.by_name(audio[0]).unwrap().read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"AAAA");
        assert!(!joined.exists(), "the joined clip is not left lying about");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
