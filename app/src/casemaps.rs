//! The map a case is shown with on a board.
//!
//! A board is polled every few seconds by every screen showing it, and a map
//! costs tiles and a route. So the picture is drawn once, when there is
//! something new to draw, and kept in the library: the scene alone while
//! nobody has said where the patient is going, and scene to hospital once a
//! crew has reported to one. The board reads what is stored.

use rusqlite::{params, Connection, OptionalExtension};
use tauri::{AppHandle, Manager};

use crate::cases::CaseView;
use crate::AppState;

/// Most pictures drawn in one pass, so a burst of cases after a restart does
/// not hold up the case loop behind a row of tile fetches.
const PER_PASS: usize = 2;

pub fn ensure_schema(c: &Connection) {
    let _ = c.execute_batch(
        "CREATE TABLE IF NOT EXISTS case_maps (
            profile TEXT NOT NULL,
            incident INTEGER NOT NULL,
            place_id TEXT NOT NULL,
            png BLOB NOT NULL,
            drive_min INTEGER,
            drive_how TEXT NOT NULL DEFAULT '',
            km REAL,
            at INTEGER NOT NULL,
            PRIMARY KEY (profile, incident, place_id)
         );",
    );
}

#[derive(Clone, Debug, PartialEq)]
pub struct CaseMap {
    pub place_id: String,
    pub png: Vec<u8>,
    /// The drive the picture shows, worked out when it was drawn.
    pub drive_min: Option<i64>,
    pub drive_how: String,
    pub km: Option<f64>,
}

/// What a case's picture should show: the hospital of its latest report
/// that has a position, else the scene alone. `None` when the scene has no
/// position.
pub fn wanted(k: &CaseView) -> Option<(String, (f64, f64), Option<(f64, f64)>)> {
    if let Some(a) = k.arrivals.iter().rev().find(|a| a.ends.is_some()) {
        let (scene, hospital) = a.ends?;
        return Some((a.place_id.clone(), scene, Some(hospital)));
    }
    Some((String::new(), (k.lat?, k.lon?), None))
}

/// The newest picture stored for a case.
pub fn latest(c: &Connection, profile: &str, incident: i64) -> Option<CaseMap> {
    c.query_row(
        "SELECT place_id, png, drive_min, drive_how, km FROM case_maps WHERE profile = ?1 AND incident = ?2 ORDER BY at DESC LIMIT 1",
        params![profile, incident],
        |r| Ok(CaseMap { place_id: r.get(0)?, png: r.get(1)?, drive_min: r.get(2)?, drive_how: r.get(3)?, km: r.get(4)? }),
    )
    .optional()
    .ok()
    .flatten()
}

fn have(c: &Connection, profile: &str, incident: i64, place_id: &str) -> bool {
    c.query_row(
        "SELECT 1 FROM case_maps WHERE profile = ?1 AND incident = ?2 AND place_id = ?3",
        params![profile, incident, place_id],
        |_| Ok(()),
    )
    .optional()
    .ok()
    .flatten()
    .is_some()
}

/// Draw what the open cases are missing. Called from the case loop.
pub fn tick(app: &AppHandle, view: &crate::cases::CasesView) {
    let state = app.state::<AppState>();
    let Some(db) = state.db.lock().unwrap().clone() else { return };
    let mut drawn = 0;
    for k in view.cases.iter().filter(|k| k.open) {
        if drawn >= PER_PASS {
            break;
        }
        let Some((place_id, scene, hospital)) = wanted(k) else { continue };
        if have(&db.lock().unwrap(), &k.profile, k.incident, &place_id) {
            continue;
        }
        drawn += 1;
        let (legs, drive) = match hospital {
            Some(h) => {
                let r = crate::routing::shape(&state, scene, h);
                let road = r.how == "road";
                let d = crate::routing::Distance {
                    meters: r.meters,
                    secs: r.secs,
                    how: if road { "road" } else { "straight" },
                };
                (vec![crate::mapshot::Leg { to: h, line: r.line, road }], Some(d))
            }
            None => (Vec::new(), None),
        };
        let shot = match crate::mapshot::draw(app, scene, &legs) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("cases: map for case {}: {e}", k.id);
                continue;
            }
        };
        let c = db.lock().unwrap();
        let _ = c.execute(
            "INSERT OR REPLACE INTO case_maps (profile, incident, place_id, png, drive_min, drive_how, km, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                k.profile,
                k.incident,
                place_id,
                shot.png,
                drive.as_ref().map(crate::cases::drive_minutes),
                match drive.map(|d| d.how) {
                    Some("road") => "by road",
                    Some(_) => "by distance",
                    None => "",
                },
                drive.map(|d| (d.km() * 10.0).round() / 10.0),
                crate::library::now()
            ],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(lat: Option<f64>, arrivals: Vec<crate::cases::Arrival>) -> CaseView {
        CaseView {
            id: 1,
            profile: "cardiac-arrest".into(),
            incident: 11,
            title: "Cardiac arrest".into(),
            call_type: String::new(),
            address: "1200 Example St".into(),
            lat,
            lon: lat.map(|_| -86.0),
            units: vec![],
            incidents: vec![11],
            opened: 0,
            updated: 0,
            state: "dispatched".into(),
            open: true,
            lines: vec![],
            arrival: arrivals.last().cloned(),
            arrivals,
        }
    }

    fn reported(place: &str, ends: Option<((f64, f64), (f64, f64))>) -> crate::cases::Arrival {
        let mut a = crate::cases::predict(3, "Example General", 100, None, None);
        a.place_id = place.into();
        a.ends = ends;
        a
    }

    #[test]
    fn the_picture_is_the_scene_until_a_crew_names_a_hospital() {
        assert_eq!(wanted(&case(Some(40.0), vec![])), Some((String::new(), (40.0, -86.0), None)));
        assert_eq!(wanted(&case(None, vec![])), None);
        let both = ((40.0, -86.0), (40.1, -86.1));
        assert_eq!(
            wanted(&case(Some(40.0), vec![reported("p-a", Some(both)), reported("p-b", None)])),
            Some(("p-a".into(), (40.0, -86.0), Some((40.1, -86.1)))),
            "the latest report that can be drawn"
        );
    }

    #[test]
    fn the_newest_picture_is_the_one_shown() {
        let c = Connection::open_in_memory().unwrap();
        ensure_schema(&c);
        assert!(latest(&c, "cardiac-arrest", 11).is_none());
        c.execute("INSERT INTO case_maps (profile, incident, place_id, png, at) VALUES ('cardiac-arrest', 11, '', x'01', 100)", []).unwrap();
        c.execute(
            "INSERT INTO case_maps (profile, incident, place_id, png, drive_min, drive_how, km, at) VALUES ('cardiac-arrest', 11, 'p-a', x'02', 9, 'by road', 6.1, 200)",
            [],
        )
        .unwrap();
        let m = latest(&c, "cardiac-arrest", 11).unwrap();
        assert_eq!((m.place_id.as_str(), m.png.as_slice(), m.drive_min), ("p-a", &[2u8][..], Some(9)));
        assert!(have(&c, "cardiac-arrest", 11, ""));
        assert!(!have(&c, "cardiac-arrest", 12, ""));
    }
}
