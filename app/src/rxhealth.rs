//! How much of what the system grants is actually being heard.
//!
//! A trunked call arrives as a grant — the control channel says a talkgroup is
//! about to speak on a voice frequency — and the app tunes it. When the voice
//! channel does not decode, the call is still recorded, with no audio and a
//! length of zero. Nothing until now added those up.
//!
//! It is worth adding up. On a simulcast site, overlapping transmitters can
//! leave the control channel perfectly readable while individual voice
//! channels are unrecoverable, so everything looks healthy — talkgroups
//! appear, calls are listed — while a third of the traffic is silently
//! missing. That is invisible in a call list, which only shows what did
//! arrive, and it is the difference between "this hospital was quiet" and
//! "we did not hear this hospital".
//!
//! Encrypted calls are excluded rather than counted as failures: they have no
//! audio by design, and lumping them in would make a properly working radio
//! look broken.

use serde::Serialize;

/// One hour of reception.
#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub struct Hour {
    /// Start of the hour, epoch seconds.
    pub at: i64,
    pub calls: u32,
    /// Grants that produced no audio, encryption aside.
    pub silent: u32,
    /// Calls that decoded, but with damaged frames.
    pub poor: u32,
    pub pct: f64,
    pub verdict: String,
}

/// One talkgroup's reception over the window.
#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub struct Talkgroup {
    pub tg: u16,
    pub name: String,
    pub calls: u32,
    pub silent: u32,
    pub poor: u32,
    pub pct: f64,
    pub verdict: String,
}

#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub struct Health {
    /// Oldest hour first, so the row reads left to right as time.
    pub hours: Vec<Hour>,
    /// Worst first by how many grants were lost.
    pub talkgroups: Vec<Talkgroup>,
    pub calls: u32,
    pub silent: u32,
    pub poor: u32,
    /// Counted, and then left out of everything above.
    pub encrypted: u32,
    pub hours_asked: u32,
    /// Share of grants that produced nothing, and what that share means.
    /// Worked out here rather than on the page so the thresholds have one
    /// home and cannot drift apart from the ones the tests check.
    pub pct: f64,
    pub verdict: String,
}

/// Percentage of grants that produced nothing, 0 when nothing was heard.
pub fn silent_pct(calls: u32, silent: u32) -> f64 {
    if calls == 0 {
        0.0
    } else {
        silent as f64 * 100.0 / calls as f64
    }
}

/// What a rate means, for colouring. The bands are judgement, not physics:
/// a well-placed receiver on a clean site loses almost nothing, and a third
/// of the traffic going missing is worth being told about.
pub fn verdict(pct: f64) -> &'static str {
    if pct < 5.0 {
        "good"
    } else if pct < 15.0 {
        "fair"
    } else if pct < 30.0 {
        "poor"
    } else {
        "bad"
    }
}

/// Rows that are not transmissions and must not be weighed as if they were.
///
/// A system re-announces a call for a second or two after the last radio
/// releases. Older builds recorded each of those announcements as a call of
/// zero length with no radio named — 4391 of them in 72 hours on this
/// library, against 127 transmissions that genuinely failed to decode.
/// Counted as losses they put this panel's reading at 25-29% when the real
/// figure was near 1%, and sent a day of work after a receive problem that
/// was not happening.
///
/// `follow` no longer writes them, but the ones already recorded have to age
/// out, so they are excluded on the way in as well. A row with no audio, no
/// radio named and nothing decoded is not evidence that anything was said.
pub const ANNOUNCED_ONLY: &str = "(secs <= 0 AND COALESCE(unit, 0) = 0)";

pub fn read(c: &rusqlite::Connection, hours: u32, now: i64) -> Result<Health, String> {
    let hours = hours.clamp(1, 24 * 14);
    let since = now - hours as i64 * 3600;
    let mut h = Health {
        hours_asked: hours,
        ..Default::default()
    };

    let mut q = c
        .prepare(&format!(
            "SELECT COUNT(*),
                    SUM(CASE WHEN secs <= 0 AND encrypted = 0 THEN 1 ELSE 0 END),
                    SUM(CASE WHEN secs > 0 AND poor_frames > 0 THEN 1 ELSE 0 END),
                    SUM(CASE WHEN encrypted = 1 THEN 1 ELSE 0 END)
               FROM calls WHERE start >= ?1 AND NOT {ANNOUNCED_ONLY}"
        ))
        .map_err(|e| e.to_string())?;
    let (calls, silent, poor, enc) = q
        .query_row([since], |r| {
            Ok((
                r.get::<_, i64>(0)? as u32,
                r.get::<_, Option<i64>>(1)?.unwrap_or(0) as u32,
                r.get::<_, Option<i64>>(2)?.unwrap_or(0) as u32,
                r.get::<_, Option<i64>>(3)?.unwrap_or(0) as u32,
            ))
        })
        .map_err(|e| e.to_string())?;
    // An encrypted call is not a failure and not a chance to fail, so it is
    // out of the denominator as well as the numerator.
    h.calls = calls.saturating_sub(enc);
    h.silent = silent;
    h.poor = poor;
    h.encrypted = enc;
    h.pct = silent_pct(h.calls, h.silent);
    h.verdict = verdict(h.pct).into();

    let mut q = c
        .prepare(&format!(
            "SELECT CAST(start / 3600 AS INTEGER) * 3600 AS hr,
                    COUNT(*),
                    SUM(CASE WHEN secs <= 0 AND encrypted = 0 THEN 1 ELSE 0 END),
                    SUM(CASE WHEN secs > 0 AND poor_frames > 0 THEN 1 ELSE 0 END)
               FROM calls WHERE start >= ?1 AND encrypted = 0 AND NOT {ANNOUNCED_ONLY}
              GROUP BY hr ORDER BY hr"
        ))
        .map_err(|e| e.to_string())?;
    h.hours = q
        .query_map([since], |r| {
            let calls = r.get::<_, i64>(1)? as u32;
            let silent = r.get::<_, Option<i64>>(2)?.unwrap_or(0) as u32;
            let pct = silent_pct(calls, silent);
            Ok(Hour {
                at: r.get(0)?,
                calls,
                silent,
                poor: r.get::<_, Option<i64>>(3)?.unwrap_or(0) as u32,
                pct,
                verdict: verdict(pct).into(),
            })
        })
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .collect();

    let mut q = c
        .prepare(&format!(
            "SELECT tg, MAX(tg_name), COUNT(*),
                    SUM(CASE WHEN secs <= 0 THEN 1 ELSE 0 END),
                    SUM(CASE WHEN secs > 0 AND poor_frames > 0 THEN 1 ELSE 0 END)
               FROM calls WHERE start >= ?1 AND encrypted = 0 AND NOT {ANNOUNCED_ONLY}
              GROUP BY tg ORDER BY 4 DESC, 3 DESC LIMIT 12"
        ))
        .map_err(|e| e.to_string())?;
    h.talkgroups = q
        .query_map([since], |r| {
            let calls = r.get::<_, i64>(2)? as u32;
            let silent = r.get::<_, Option<i64>>(3)?.unwrap_or(0) as u32;
            let pct = silent_pct(calls, silent);
            Ok(Talkgroup {
                tg: r.get::<_, i64>(0)? as u16,
                name: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                calls,
                silent,
                poor: r.get::<_, Option<i64>>(4)?.unwrap_or(0) as u32,
                pct,
                verdict: verdict(pct).into(),
            })
        })
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .filter(|t| t.silent > 0)
        .collect();
    Ok(h)
}

#[tauri::command]
pub fn rx_health(state: tauri::State<crate::AppState>, hours: u32) -> Result<Health, String> {
    let db = state
        .db
        .lock()
        .unwrap()
        .clone()
        .ok_or("no call library open")?;
    let c = db.lock().unwrap();
    read(&c, hours, crate::library::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE calls (id INTEGER PRIMARY KEY, start INTEGER, secs REAL, tg INTEGER,
             tg_name TEXT, encrypted INTEGER DEFAULT 0, poor_frames INTEGER DEFAULT 0,
             unit INTEGER DEFAULT 0);",
        )
        .unwrap();
        c
    }

    /// A transmission: some radio keyed up, whether or not it decoded.
    fn add(c: &Connection, start: i64, tg: u16, name: &str, secs: f64, enc: i64, poor: i64) {
        add_from(c, start, tg, name, secs, enc, poor, 4242);
    }

    fn add_from(
        c: &Connection,
        start: i64,
        tg: u16,
        name: &str,
        secs: f64,
        enc: i64,
        poor: i64,
        unit: u32,
    ) {
        c.execute(
            "INSERT INTO calls (start, secs, tg, tg_name, encrypted, poor_frames, unit)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            rusqlite::params![start, secs, tg, name, enc, poor, unit],
        )
        .unwrap();
    }

    /// The control channel re-announcing a call after the last radio released:
    /// no audio, and no radio named, because none was transmitting.
    fn announce(c: &Connection, start: i64, tg: u16, name: &str) {
        add_from(c, start, tg, name, 0.0, 0, 0, 0);
    }

    #[test]
    fn an_announcement_of_a_call_nobody_made_is_not_a_loss() {
        // A system re-announces a call for a second or two after the last
        // radio releases. Weighing those as failed transmissions is what had
        // this panel reporting a quarter of the traffic missing when the real
        // figure was near one percent.
        let c = db();
        add(&c, 1_000_000, 10256, "49M-M03", 5.0, 0, 0);
        for k in 0..8 {
            announce(&c, 1_000_010 + k, 10256, "49M-M03");
        }
        let h = read(&c, 24, 1_000_200).unwrap();
        assert_eq!(
            (h.calls, h.silent),
            (1, 0),
            "an announcement is neither a transmission nor a lost one"
        );
        assert_eq!(h.verdict, "good");
        assert!(h.talkgroups.is_empty(), "nothing was lost to report");
    }

    #[test]
    fn a_grant_that_named_a_radio_and_decoded_nothing_is_still_a_loss() {
        // The real thing this panel is for: a radio keyed up and none of it
        // was heard.
        let c = db();
        add_from(&c, 1_000_000, 10256, "49M-M03", 0.0, 0, 0, 4242);
        add_from(&c, 1_000_100, 10256, "49M-M03", 5.0, 0, 0, 4243);
        let h = read(&c, 24, 1_000_200).unwrap();
        assert_eq!((h.calls, h.silent), (2, 1));
    }

    #[test]
    fn a_grant_that_decoded_nothing_counts_as_lost() {
        let c = db();
        add(&c, 1_000_000, 10256, "49M-M03", 0.0, 0, 0);
        add(&c, 1_000_100, 10256, "49M-M03", 5.0, 0, 0);
        let h = read(&c, 24, 1_000_200).unwrap();
        assert_eq!((h.calls, h.silent), (2, 1));
        assert_eq!(silent_pct(h.calls, h.silent), 50.0);
    }

    #[test]
    fn an_encrypted_call_is_not_a_receive_failure() {
        // It has no audio by design. Counting it as a loss would make a radio
        // that is working perfectly look like one that is missing a third of
        // the traffic — which is the very thing this readout exists to show.
        let c = db();
        add(&c, 1_000_000, 10300, "SECURE", 0.0, 1, 0);
        add(&c, 1_000_100, 10300, "SECURE", 0.0, 1, 0);
        add(&c, 1_000_200, 10256, "49M-M03", 5.0, 0, 0);
        let h = read(&c, 24, 1_000_300).unwrap();
        assert_eq!(h.encrypted, 2);
        assert_eq!(h.silent, 0, "encrypted calls are not losses");
        assert_eq!(h.calls, 1, "nor are they chances to fail");
        assert_eq!(silent_pct(h.calls, h.silent), 0.0);
        assert!(h.talkgroups.is_empty(), "and they are not worst offenders");
    }

    #[test]
    fn the_worst_talkgroup_comes_first() {
        // One talkgroup losing hundreds of grants would otherwise hide inside
        // a single overall percentage, and the overall figure would look like
        // a site-wide fault rather than one talkgroup's.
        let c = db();
        for i in 0..20 {
            add(&c, 1_000_000 + i, 10203, "49F-NORTH", 0.0, 0, 0);
        }
        add(&c, 1_000_100, 10256, "49M-M03", 0.0, 0, 0);
        add(&c, 1_000_200, 10256, "49M-M03", 5.0, 0, 0);
        let h = read(&c, 24, 1_000_300).unwrap();
        assert_eq!(h.talkgroups[0].tg, 10203);
        assert_eq!(h.talkgroups[0].silent, 20);
        assert_eq!(h.talkgroups[1].tg, 10256);
    }

    #[test]
    fn a_talkgroup_losing_nothing_is_not_listed() {
        let c = db();
        add(&c, 1_000_000, 10256, "49M-M03", 5.0, 0, 0);
        let h = read(&c, 24, 1_000_100).unwrap();
        assert!(h.talkgroups.is_empty());
    }

    #[test]
    fn hours_come_back_oldest_first_and_bucketed() {
        let c = db();
        add(&c, 7_200, 10256, "A", 0.0, 0, 0); // hour 2
        add(&c, 7_300, 10256, "A", 5.0, 0, 0); // hour 2
        add(&c, 3_600, 10256, "A", 5.0, 0, 0); // hour 1
        let h = read(&c, 24, 10_000).unwrap();
        assert_eq!(h.hours.len(), 2);
        assert_eq!(h.hours[0].at, 3_600, "oldest first");
        assert_eq!(h.hours[1].at, 7_200);
        assert_eq!((h.hours[1].calls, h.hours[1].silent), (2, 1));
    }

    #[test]
    fn damaged_audio_is_counted_apart_from_silence() {
        // A call that decoded badly still told you something; one that
        // decoded not at all did not. They are different problems.
        let c = db();
        add(&c, 1_000_000, 10256, "A", 5.0, 0, 40);
        add(&c, 1_000_100, 10256, "A", 0.0, 0, 0);
        let h = read(&c, 24, 1_000_200).unwrap();
        assert_eq!((h.silent, h.poor), (1, 1));
    }

    #[test]
    fn an_empty_window_reports_nothing_rather_than_dividing_by_zero() {
        let c = db();
        let h = read(&c, 6, 1_000_000).unwrap();
        assert_eq!((h.calls, h.silent), (0, 0));
        assert_eq!(silent_pct(0, 0), 0.0);
        assert_eq!(verdict(silent_pct(0, 0)), "good");
    }

    #[test]
    fn the_verdict_bands_are_ordered() {
        assert_eq!(verdict(0.0), "good");
        assert_eq!(verdict(9.0), "fair");
        assert_eq!(verdict(22.0), "poor");
        assert_eq!(verdict(29.5), "poor");
        assert_eq!(verdict(30.0), "bad");
    }
}
