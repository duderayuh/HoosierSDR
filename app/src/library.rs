//! The call library: every call the app hears, with its audio file, a
//! capture-time SHA-256, full-text search, and the machine and human
//! transcripts kept apart. SQLite (bundled) in the app's config directory.
//!
//! Evidence-minded by design: the audio hash is computed the moment the file
//! is written, the machine transcript is never overwritten by an edit, and an
//! export re-hashes every file and signs the manifest with its own hash.

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct CallRow {
    pub id: i64,
    pub start: i64,
    pub secs: f64,
    pub tg: u16,
    pub tg_name: String,
    pub unit: u32,
    pub unit_name: Option<String>,
    pub freq_hz: u64,
    pub modulation: String,
    pub emergency: bool,
    pub patched_with: Vec<u16>,
    pub system: String,
    pub site: String,
    pub audio: Option<String>,
    pub sha256: Option<String>,
    pub transcript: Option<String>,
    pub transcript_model: Option<String>,
    pub transcript_edited: Option<String>,
    pub edited_at: Option<i64>,
    pub starred: bool,
}

pub fn open(dir: &Path) -> Result<Connection, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let c = Connection::open(dir.join("calls.db")).map_err(|e| format!("open calls.db: {e}"))?;
    c.execute_batch(
        r#"
        PRAGMA journal_mode = WAL;
        CREATE TABLE IF NOT EXISTS calls (
            id INTEGER PRIMARY KEY,
            start INTEGER NOT NULL,
            secs REAL NOT NULL,
            tg INTEGER NOT NULL,
            tg_name TEXT NOT NULL,
            unit INTEGER NOT NULL,
            unit_name TEXT,
            freq_hz INTEGER NOT NULL,
            modulation TEXT NOT NULL,
            emergency INTEGER NOT NULL DEFAULT 0,
            patched_with TEXT NOT NULL DEFAULT '',
            system TEXT NOT NULL DEFAULT '',
            site TEXT NOT NULL DEFAULT '',
            audio TEXT,
            sha256 TEXT,
            transcript TEXT,
            transcript_model TEXT,
            transcribed_at INTEGER,
            transcript_edited TEXT,
            edited_at INTEGER,
            starred INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS calls_start ON calls(start);
        CREATE INDEX IF NOT EXISTS calls_tg ON calls(tg, start);
        CREATE INDEX IF NOT EXISTS calls_unit ON calls(unit, start);
        CREATE VIRTUAL TABLE IF NOT EXISTS calls_fts USING fts5(
            tg_name, unit_name, transcript, transcript_edited, content='calls', content_rowid='id'
        );
        CREATE TRIGGER IF NOT EXISTS calls_ai AFTER INSERT ON calls BEGIN
            INSERT INTO calls_fts(rowid, tg_name, unit_name, transcript, transcript_edited)
            VALUES (new.id, new.tg_name, new.unit_name, new.transcript, new.transcript_edited);
        END;
        CREATE TRIGGER IF NOT EXISTS calls_ad AFTER DELETE ON calls BEGIN
            INSERT INTO calls_fts(calls_fts, rowid, tg_name, unit_name, transcript, transcript_edited)
            VALUES ('delete', old.id, old.tg_name, old.unit_name, old.transcript, old.transcript_edited);
        END;
        CREATE TRIGGER IF NOT EXISTS calls_au AFTER UPDATE ON calls BEGIN
            INSERT INTO calls_fts(calls_fts, rowid, tg_name, unit_name, transcript, transcript_edited)
            VALUES ('delete', old.id, old.tg_name, old.unit_name, old.transcript, old.transcript_edited);
            INSERT INTO calls_fts(rowid, tg_name, unit_name, transcript, transcript_edited)
            VALUES (new.id, new.tg_name, new.unit_name, new.transcript, new.transcript_edited);
        END;
        "#,
    )
    .map_err(|e| format!("schema: {e}"))?;
    Ok(c)
}

pub fn sha256_file(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(hex(&Sha256::digest(&bytes)))
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Insert a call; `audio` is the file already written (hash computed here,
/// at capture time). Returns the new id.
pub fn insert(c: &Connection, r: &CallRow) -> Result<i64, String> {
    let sha = match &r.audio {
        Some(a) => Some(sha256_file(Path::new(a))?),
        None => None,
    };
    c.execute(
        "INSERT INTO calls (start, secs, tg, tg_name, unit, unit_name, freq_hz, modulation, emergency, patched_with, system, site, audio, sha256)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            r.start,
            r.secs,
            r.tg,
            r.tg_name,
            r.unit,
            r.unit_name,
            r.freq_hz as i64,
            r.modulation,
            r.emergency,
            r.patched_with.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(" "),
            r.system,
            r.site,
            r.audio,
            sha,
        ],
    )
    .map_err(|e| format!("insert call: {e}"))?;
    Ok(c.last_insert_rowid())
}

#[derive(Deserialize, Default, Clone)]
pub struct Query {
    /// FTS5 query over talkgroup name, unit name and both transcripts.
    pub text: Option<String>,
    pub from: Option<i64>,
    pub to: Option<i64>,
    pub tg: Option<u16>,
    pub unit: Option<u32>,
    pub starred: Option<bool>,
    pub emergency: Option<bool>,
    pub with_audio: Option<bool>,
    pub limit: Option<u32>,
    pub before_id: Option<i64>,
    /// Calls newer than this id (live tail).
    pub after_id: Option<i64>,
}

const COLS: &str = "id, start, secs, tg, tg_name, unit, unit_name, freq_hz, modulation, emergency, patched_with, system, site, audio, sha256, transcript, transcript_model, transcript_edited, edited_at, starred";

fn row(r: &rusqlite::Row) -> rusqlite::Result<CallRow> {
    let patched: String = r.get(10)?;
    Ok(CallRow {
        id: r.get(0)?,
        start: r.get(1)?,
        secs: r.get(2)?,
        tg: r.get::<_, i64>(3)? as u16,
        tg_name: r.get(4)?,
        unit: r.get::<_, i64>(5)? as u32,
        unit_name: r.get(6)?,
        freq_hz: r.get::<_, i64>(7)? as u64,
        modulation: r.get(8)?,
        emergency: r.get::<_, i64>(9)? != 0,
        patched_with: patched
            .split_whitespace()
            .filter_map(|t| t.parse().ok())
            .collect(),
        system: r.get(11)?,
        site: r.get(12)?,
        audio: r.get(13)?,
        sha256: r.get(14)?,
        transcript: r.get(15)?,
        transcript_model: r.get(16)?,
        transcript_edited: r.get(17)?,
        edited_at: r.get(18)?,
        starred: r.get::<_, i64>(19)? != 0,
    })
}

/// Search, newest first (or oldest first when tailing with `after_id`).
pub fn search(c: &Connection, q: &Query) -> Result<Vec<CallRow>, String> {
    let mut sql = format!("SELECT {COLS} FROM calls WHERE 1=1");
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(t) = q.text.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        sql.push_str(" AND id IN (SELECT rowid FROM calls_fts WHERE calls_fts MATCH ?)");
        args.push(Box::new(fts_query(t)));
    }
    if let Some(v) = q.from {
        sql.push_str(" AND start >= ?");
        args.push(Box::new(v));
    }
    if let Some(v) = q.to {
        sql.push_str(" AND start <= ?");
        args.push(Box::new(v));
    }
    if let Some(v) = q.tg {
        sql.push_str(" AND tg = ?");
        args.push(Box::new(v as i64));
    }
    if let Some(v) = q.unit {
        sql.push_str(" AND unit = ?");
        args.push(Box::new(v as i64));
    }
    if let Some(true) = q.starred {
        sql.push_str(" AND starred = 1");
    }
    if let Some(true) = q.emergency {
        sql.push_str(" AND emergency = 1");
    }
    if let Some(true) = q.with_audio {
        sql.push_str(" AND audio IS NOT NULL");
    }
    if let Some(v) = q.before_id {
        sql.push_str(" AND id < ?");
        args.push(Box::new(v));
    }
    if let Some(v) = q.after_id {
        sql.push_str(" AND id > ?");
        args.push(Box::new(v));
    }
    sql.push_str(if q.after_id.is_some() {
        " ORDER BY id ASC"
    } else {
        " ORDER BY id DESC"
    });
    sql.push_str(" LIMIT ?");
    args.push(Box::new(q.limit.unwrap_or(200).min(2000) as i64));
    let mut st = c.prepare(&sql).map_err(|e| format!("search: {e}"))?;
    let rows = st
        .query_map(
            rusqlite::params_from_iter(args.iter().map(|a| a.as_ref())),
            row,
        )
        .map_err(|e| format!("search: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("search: {e}"))?;
    Ok(rows)
}

/// Make a forgiving FTS5 query: each word becomes a prefix term, ANDed.
fn fts_query(t: &str) -> String {
    t.split_whitespace()
        .map(|w| format!("\"{}\"*", w.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn get(c: &Connection, id: i64) -> Result<Option<CallRow>, String> {
    c.query_row(
        &format!("SELECT {COLS} FROM calls WHERE id = ?1"),
        params![id],
        row,
    )
    .optional()
    .map_err(|e| format!("get: {e}"))
}

pub fn set_transcript(c: &Connection, id: i64, text: &str, model: &str) -> Result<(), String> {
    c.execute(
        "UPDATE calls SET transcript = ?2, transcript_model = ?3, transcribed_at = ?4 WHERE id = ?1",
        params![id, text, model, now()],
    )
    .map_err(|e| format!("set transcript: {e}"))?;
    Ok(())
}

/// A human correction — stored beside the machine text, never over it.
pub fn set_edited(c: &Connection, id: i64, text: Option<&str>) -> Result<(), String> {
    c.execute(
        "UPDATE calls SET transcript_edited = ?2, edited_at = ?3 WHERE id = ?1",
        params![id, text, text.map(|_| now())],
    )
    .map_err(|e| format!("set edited: {e}"))?;
    Ok(())
}

pub fn set_starred(c: &Connection, id: i64, on: bool) -> Result<(), String> {
    c.execute(
        "UPDATE calls SET starred = ?2 WHERE id = ?1",
        params![id, on],
    )
    .map_err(|e| format!("star: {e}"))?;
    Ok(())
}

/// Calls with audio and no machine transcript yet, oldest first.
pub fn untranscribed(c: &Connection, limit: u32) -> Result<Vec<CallRow>, String> {
    let mut st = c
        .prepare(&format!("SELECT {COLS} FROM calls WHERE audio IS NOT NULL AND transcript IS NULL ORDER BY id ASC LIMIT ?1"))
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map(params![limit], row)
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(rows)
}

/// Delete calls (and their audio) older than `days`. Starred calls stay.
pub fn prune(c: &Connection, days: u32) -> Result<usize, String> {
    let cutoff = now() - days as i64 * 86400;
    let mut st = c
        .prepare("SELECT id, audio FROM calls WHERE start < ?1 AND starred = 0")
        .map_err(|e| e.to_string())?;
    let victims: Vec<(i64, Option<String>)> = st
        .query_map(params![cutoff], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .collect();
    for (id, audio) in &victims {
        if let Some(a) = audio {
            let _ = std::fs::remove_file(a);
            let _ = std::fs::remove_file(Path::new(a).with_extension("json"));
        }
        let _ = c.execute("DELETE FROM calls WHERE id = ?1", params![id]);
    }
    Ok(victims.len())
}

pub fn stats(c: &Connection) -> Result<(i64, f64, i64), String> {
    c.query_row(
        "SELECT COUNT(*), COALESCE(SUM(secs),0), COALESCE(SUM(transcript IS NOT NULL),0) FROM calls",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .map_err(|e| e.to_string())
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Export with chain of custody.
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ManifestCall {
    id: i64,
    captured_at: i64,
    captured_at_utc: String,
    seconds: f64,
    talkgroup: u16,
    talkgroup_name: String,
    unit: u32,
    unit_name: Option<String>,
    freq_hz: u64,
    modulation: String,
    emergency: bool,
    patched_with: Vec<u16>,
    system: String,
    site: String,
    exported_file: Option<String>,
    original_path: Option<String>,
    sha256_at_capture: Option<String>,
    sha256_at_export: Option<String>,
    hash_matches_capture: Option<bool>,
    machine_transcript: Option<String>,
    transcript_model: Option<String>,
    edited_transcript: Option<String>,
    transcript_edited_by_human: bool,
    edited_at: Option<i64>,
}

#[derive(Serialize)]
struct Manifest {
    format: &'static str,
    app: String,
    exported_at: i64,
    exported_at_utc: String,
    exported_by: String,
    host: String,
    calls: Vec<ManifestCall>,
}

/// Copy the selected calls' audio byte-for-byte into `dest`, write a
/// transcript text file per call, and a `manifest.json` with capture-time and
/// export-time hashes, plus `manifest.sha256`. Returns the manifest path.
pub fn export(
    c: &Connection,
    ids: &[i64],
    dest: &Path,
    app_version: &str,
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    let mut calls = Vec::new();
    for id in ids {
        let Some(r) = get(c, *id)? else { continue };
        let mut exported_file = None;
        let mut sha_export = None;
        if let Some(a) = &r.audio {
            let src = Path::new(a);
            if src.exists() {
                let name = src
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| format!("call_{id}.wav"));
                let dst = dest.join(&name);
                std::fs::copy(src, &dst).map_err(|e| format!("copy {}: {e}", src.display()))?;
                sha_export = Some(sha256_file(&dst)?);
                let txt = dst.with_extension("txt");
                let body = format!(
                    "HoosierSDR call {id}\ncaptured {} UTC\ntalkgroup {} ({})\nunit {}{}\nfrequency {:.4} MHz\n\n[machine transcript{}]\n{}\n\n[edited transcript]\n{}\n",
                    utc(r.start),
                    r.tg,
                    r.tg_name,
                    r.unit,
                    r.unit_name.as_deref().map(|n| format!(" ({n})")).unwrap_or_default(),
                    r.freq_hz as f64 / 1e6,
                    r.transcript_model.as_deref().map(|m| format!(", {m}")).unwrap_or_default(),
                    r.transcript.as_deref().unwrap_or("(none)"),
                    r.transcript_edited.as_deref().unwrap_or("(none)")
                );
                let _ = std::fs::write(&txt, body);
                exported_file = Some(name);
            }
        }
        calls.push(ManifestCall {
            id: r.id,
            captured_at: r.start,
            captured_at_utc: utc(r.start),
            seconds: r.secs,
            talkgroup: r.tg,
            talkgroup_name: r.tg_name.clone(),
            unit: r.unit,
            unit_name: r.unit_name.clone(),
            freq_hz: r.freq_hz,
            modulation: r.modulation.clone(),
            emergency: r.emergency,
            patched_with: r.patched_with.clone(),
            system: r.system.clone(),
            site: r.site.clone(),
            exported_file,
            original_path: r.audio.clone(),
            hash_matches_capture: match (&r.sha256, &sha_export) {
                (Some(a), Some(b)) => Some(a == b),
                _ => None,
            },
            sha256_at_capture: r.sha256.clone(),
            sha256_at_export: sha_export,
            machine_transcript: r.transcript.clone(),
            transcript_model: r.transcript_model.clone(),
            transcript_edited_by_human: r.transcript_edited.is_some(),
            edited_transcript: r.transcript_edited.clone(),
            edited_at: r.edited_at,
        });
    }
    let m = Manifest {
        format: "hoosiersdr-export/1",
        app: app_version.to_string(),
        exported_at: now(),
        exported_at_utc: utc(now()),
        exported_by: std::env::var("USER").unwrap_or_else(|_| "unknown".into()),
        host: hostname(),
        calls,
    };
    let path = dest.join("manifest.json");
    let json = serde_json::to_string_pretty(&m).map_err(|e| e.to_string())?;
    std::fs::write(&path, &json).map_err(|e| format!("{}: {e}", path.display()))?;
    let digest = hex(&Sha256::digest(json.as_bytes()));
    std::fs::write(
        dest.join("manifest.sha256"),
        format!("{digest}  manifest.json\n"),
    )
    .map_err(|e| e.to_string())?;
    Ok(path)
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

/// `YYYY-MM-DD HH:MM:SS` in UTC.
pub fn utc(t: i64) -> String {
    let t = t.max(0) as u64;
    let days = (t / 86400) as i64;
    let secs = t % 86400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("hs_lib_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn call(dir: &Path, tg: u16, name: &str, secs: f64, start: i64) -> CallRow {
        let pcm: Vec<i16> = (0..(8000.0 * secs) as usize)
            .map(|i| (i % 100) as i16)
            .collect();
        let audio = dir.join(format!("c_{tg}_{start}.wav"));
        hs_core::wav::write_wav(audio.to_str().unwrap(), 8000, &pcm).unwrap();
        CallRow {
            start,
            secs,
            tg,
            tg_name: name.into(),
            unit: 4900165,
            unit_name: Some("Car 12".into()),
            freq_hz: 857_387_500,
            modulation: "CQPSK".into(),
            system: "Test".into(),
            audio: Some(audio.to_string_lossy().into_owned()),
            ..Default::default()
        }
    }

    #[test]
    fn inserts_hashes_searches_and_exports_with_custody() {
        let d = tmp();
        let c = open(&d).unwrap();
        let a = insert(
            &c,
            &call(&d, 10103, "IMPD North Dispatch", 2.0, 1_700_000_000),
        )
        .unwrap();
        let b = insert(
            &c,
            &call(&d, 10147, "IFD Fire Dispatch", 1.0, 1_700_000_100),
        )
        .unwrap();
        set_transcript(
            &c,
            a,
            "engine twelve respond to a structure fire",
            "faster-whisper/base",
        )
        .unwrap();
        set_edited(&c, a, Some("Engine 12 respond to a structure fire")).unwrap();

        // Hash recorded at capture.
        let ra = get(&c, a).unwrap().unwrap();
        assert_eq!(ra.sha256.as_deref().map(str::len), Some(64));
        assert_eq!(
            ra.transcript.as_deref(),
            Some("engine twelve respond to a structure fire")
        );
        assert_eq!(
            ra.transcript_edited.as_deref(),
            Some("Engine 12 respond to a structure fire")
        );

        // Text search over names and both transcripts; time and tg filters.
        let q = |text: &str| Query {
            text: Some(text.into()),
            ..Default::default()
        };
        assert_eq!(search(&c, &q("fire")).unwrap().len(), 2); // "Fire Dispatch" + transcript
        assert_eq!(search(&c, &q("structure")).unwrap().len(), 1);
        assert_eq!(search(&c, &q("Engine 12")).unwrap().len(), 1); // edited transcript
        assert_eq!(search(&c, &q("car")).unwrap().len(), 2); // unit name
        assert_eq!(
            search(
                &c,
                &Query {
                    tg: Some(10147),
                    ..Default::default()
                }
            )
            .unwrap()[0]
                .id,
            b
        );
        assert_eq!(
            search(
                &c,
                &Query {
                    from: Some(1_700_000_050),
                    ..Default::default()
                }
            )
            .unwrap()
            .len(),
            1
        );
        assert_eq!(
            search(
                &c,
                &Query {
                    after_id: Some(a),
                    ..Default::default()
                }
            )
            .unwrap()[0]
                .id,
            b
        );

        // Export: byte-identical copies, matching hashes, manifest + its hash.
        let out = d.join("export");
        let manifest = export(&c, &[a, b], &out, "test").unwrap();
        let m: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&manifest).unwrap()).unwrap();
        assert_eq!(m["calls"].as_array().unwrap().len(), 2);
        assert_eq!(m["calls"][0]["hash_matches_capture"], true);
        assert_eq!(m["calls"][0]["transcript_edited_by_human"], true);
        assert_eq!(
            m["calls"][0]["machine_transcript"],
            "engine twelve respond to a structure fire"
        );
        let sig = std::fs::read_to_string(out.join("manifest.sha256")).unwrap();
        assert_eq!(
            &sig[..64],
            hex(&Sha256::digest(std::fs::read(&manifest).unwrap()))
        );
        assert!(out.join("c_10103_1700000000.txt").exists());

        // Prune keeps starred calls.
        set_starred(&c, b, true).unwrap();
        assert_eq!(prune(&c, 0).unwrap(), 1);
        assert!(get(&c, a).unwrap().is_none() && get(&c, b).unwrap().is_some());
        assert_eq!(stats(&c).unwrap().0, 1);
        assert_eq!(utc(0), "1970-01-01 00:00:00");
    }
}
