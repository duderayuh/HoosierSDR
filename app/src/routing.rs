//! How far away something is, by road, without asking anyone.
//!
//! A straight line is an honest first answer and a poor second one: a river,
//! an interstate with no exit, or a one-way grid can double the drive. OSRM
//! answers properly, and it runs on this machine — a container, a map of one
//! state, and nothing leaves the house.
//!
//! Nothing here requires any of that. With no server configured, or with one
//! that does not answer, every distance falls back to the straight line and
//! says so, which is what the app did before.

use crate::AppState;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, State};

/// A distance, and how it was worked out.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Distance {
    pub meters: f64,
    /// Drive time in seconds; 0 when only the straight line is known.
    pub secs: f64,
    /// `road` — a route; `straight` — as the crow flies.
    pub how: &'static str,
}

impl Distance {
    pub fn km(&self) -> f64 {
        self.meters / 1000.0
    }
    pub fn mins(&self) -> f64 {
        self.secs / 60.0
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Ask the router at all. Off = straight lines everywhere.
    pub enabled: bool,
    pub url: String,
    /// The Geofabrik extract this machine has prepared, e.g. "indiana".
    pub region: String,
    /// The OSRM container image, so a listener can pin or change it.
    pub image: String,
    /// `--platform` for Docker, for an image with no build for this chip.
    pub platform: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            url: "http://127.0.0.1:5000".into(),
            region: String::new(),
            image: "ghcr.io/project-osrm/osrm-backend".into(),
            platform: String::new(),
        }
    }
}

#[derive(Default)]
pub struct RouteState {
    pub settings: Settings,
    /// Set when the router has just failed, so a dead server is not dialled
    /// for every place on every run.
    quiet_until: i64,
}

pub type Shared = Mutex<RouteState>;

/// After a failure, leave the router alone this long.
const SULK_SECS: i64 = 60;
const TIMEOUT_SECS: u64 = 2;

fn path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let d = app.path().app_config_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    Ok(d.join("routing.json"))
}

pub fn load(app: &AppHandle) -> RouteState {
    let settings = path(app)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    RouteState {
        settings,
        quiet_until: 0,
    }
}

fn store(app: &AppHandle, s: &Settings) -> Result<(), String> {
    let p = path(app)?;
    std::fs::write(
        p,
        serde_json::to_string_pretty(s).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

/// The straight line, always available.
pub fn straight(from: (f64, f64), to: (f64, f64)) -> Distance {
    Distance {
        meters: crate::dispatch::haversine_m(from.0, from.1, to.0, to.1),
        secs: 0.0,
        how: "straight",
    }
}

/// How far `to` is from `from`, by road when a router is there.
pub fn distance(state: &State<AppState>, from: (f64, f64), to: (f64, f64)) -> Distance {
    let (url, quiet) = {
        let st = state.routing.lock().unwrap();
        if !st.settings.enabled {
            return straight(from, to);
        }
        (st.settings.url.clone(), st.quiet_until)
    };
    if crate::library::now() < quiet {
        return straight(from, to);
    }
    match route(&url, from, to) {
        Ok(d) => d,
        Err(_) => {
            state.routing.lock().unwrap().quiet_until = crate::library::now() + SULK_SECS;
            straight(from, to)
        }
    }
}

/// OSRM wants longitude first, which is the opposite of every other line in
/// this app, so the swap happens here and nowhere else.
fn route(url: &str, from: (f64, f64), to: (f64, f64)) -> Result<Distance, String> {
    let u = format!(
        "{}/route/v1/driving/{:.6},{:.6};{:.6},{:.6}?overview=false",
        url.trim_end_matches('/'),
        from.1,
        from.0,
        to.1,
        to.0
    );
    let body = get(&u)?;
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    let r = v["routes"].get(0).ok_or("no route")?;
    Ok(Distance {
        meters: r["distance"].as_f64().ok_or("no distance")?,
        secs: r["duration"].as_f64().unwrap_or(0.0),
        how: "road",
    })
}

fn get(url: &str) -> Result<String, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(TIMEOUT_SECS)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut r = agent.get(url).call().map_err(|e| e.to_string())?;
    let status = r.status().as_u16();
    let text = r.body_mut().read_to_string().unwrap_or_default();
    if status != 200 {
        return Err(format!("osrm HTTP {status}"));
    }
    Ok(text)
}

// ---------------------------------------------------------------------------
// preparing the map, which is a Docker job
// ---------------------------------------------------------------------------

/// Which `docker` to run. An env var so the pipeline can be tested without
/// Docker, and so a listener with podman can point at it.
fn docker() -> String {
    std::env::var("HS_DOCKER").unwrap_or_else(|_| "docker".into())
}

/// The container the app starts, named so it can be found again.
const CONTAINER: &str = "hoosier-osrm";

fn data_dir(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let d = app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?
        .join("osrm");
    std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    Ok(d)
}

/// The three commands that turn a downloaded extract into a routable graph,
/// in order. Pure, so the pipeline can be checked without running anything.
pub fn prepare_commands(image: &str, platform: &str, dir: &str, region: &str) -> Vec<Vec<String>> {
    let base = |args: &[&str]| -> Vec<String> {
        let mut v: Vec<String> = vec!["run".into(), "--rm".into(), "-t".into()];
        if !platform.trim().is_empty() {
            v.push("--platform".into());
            v.push(platform.trim().into());
        }
        v.push("-v".into());
        v.push(format!("{dir}:/data"));
        v.push(image.into());
        v.extend(args.iter().map(|s| s.to_string()));
        v
    };
    let pbf = format!("/data/{region}.osm.pbf");
    let osrm = format!("/data/{region}.osrm");
    vec![
        base(&["osrm-extract", "-p", "/opt/car.lua", &pbf]),
        base(&["osrm-partition", &osrm]),
        base(&["osrm-customize", &osrm]),
    ]
}

/// The command that starts the router.
pub fn serve_command(image: &str, platform: &str, dir: &str, region: &str, port: u16) -> Vec<String> {
    let mut v: Vec<String> = vec!["run".into(), "-d".into(), "--name".into(), CONTAINER.into()];
    if !platform.trim().is_empty() {
        v.push("--platform".into());
        v.push(platform.trim().into());
    }
    v.push("-p".into());
    v.push(format!("{port}:5000"));
    v.push("-v".into());
    v.push(format!("{dir}:/data"));
    v.push(image.into());
    v.extend(
        [
            "osrm-routed",
            "--algorithm",
            "mld",
            &format!("/data/{region}.osrm"),
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    v
}

/// Where a Geofabrik extract for `region` lives. Regions are named the way
/// Geofabrik names them ("us/indiana"), which is also what the file is
/// called.
pub fn extract_url(region: &str) -> String {
    format!(
        "https://download.geofabrik.de/north-america/{}-latest.osm.pbf",
        region.trim_matches('/')
    )
}

fn run_docker(args: &[String]) -> Result<String, String> {
    let out = std::process::Command::new(docker())
        .args(args)
        .output()
        .map_err(|e| format!("could not run docker: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "docker {}: {}",
            args.first().cloned().unwrap_or_default(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Status {
    pub docker: bool,
    pub docker_error: String,
    pub container: bool,
    pub answering: bool,
    pub region: String,
    pub have_graph: bool,
    pub size_bytes: u64,
}

#[tauri::command]
pub fn routing_get(state: State<AppState>) -> Settings {
    state.routing.lock().unwrap().settings.clone()
}

#[tauri::command]
pub fn routing_set(
    app: AppHandle,
    state: State<AppState>,
    settings: Settings,
) -> Result<Settings, String> {
    let mut s = settings;
    s.url = crate::analyzers::clean_line(&s.url, 200);
    s.region = crate::analyzers::clean_line(&s.region, 60);
    s.image = crate::analyzers::clean_line(&s.image, 200);
    s.platform = crate::analyzers::clean_line(&s.platform, 40);
    if s.url.trim().is_empty() {
        s.url = Settings::default().url;
    }
    {
        let mut st = state.routing.lock().unwrap();
        st.settings = s.clone();
        st.quiet_until = 0;
    }
    store(&app, &s)?;
    Ok(s)
}

#[tauri::command]
pub fn routing_status(app: AppHandle, state: State<AppState>) -> Status {
    let s = routing_get(state.clone());
    let mut out = Status {
        region: s.region.clone(),
        ..Default::default()
    };
    match run_docker(&["ps".into(), "--format".into(), "{{.Names}}".into()]) {
        Ok(names) => {
            out.docker = true;
            out.container = names.lines().any(|n| n.trim() == CONTAINER);
        }
        Err(e) => out.docker_error = e,
    }
    if let Ok(dir) = data_dir(&app) {
        let graph = dir.join(format!("{}.osrm.mldgr", short_region(&s.region)));
        out.have_graph = graph.exists();
        out.size_bytes = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| e.metadata().ok())
                    .map(|m| m.len())
                    .sum()
            })
            .unwrap_or(0);
    }
    out.answering = route(
        &s.url,
        (39.7684, -86.1581),
        (39.7784, -86.1481),
    )
    .is_ok();
    out
}

/// "us/indiana" → "indiana": the file Geofabrik hands back is named for the
/// last part only.
pub fn short_region(region: &str) -> String {
    region.rsplit('/').next().unwrap_or(region).to_string()
}

/// Download the extract and build the graph, reporting progress as it goes.
/// Long: the download is a few hundred megabytes and the build takes
/// minutes, so it runs on its own thread and talks through `mapdata` events.
#[tauri::command]
pub fn mapdata_prepare(app: AppHandle, state: State<AppState>, region: String) -> Result<(), String> {
    let s = routing_get(state);
    let region = crate::analyzers::clean_line(&region, 60);
    if region.is_empty() {
        return Err("choose a region first".into());
    }
    let dir = data_dir(&app)?;
    std::thread::spawn(move || {
        let say = |step: &str, detail: String, done: bool| {
            let _ = app.emit(
                "mapdata",
                serde_json::json!({ "step": step, "detail": detail, "done": done }),
            );
        };
        let short = short_region(&region);
        let pbf = dir.join(format!("{short}.osm.pbf"));
        if !pbf.exists() {
            say("download", extract_url(&region), false);
            if let Err(e) = download(&extract_url(&region), &pbf, &app) {
                say("error", format!("download failed: {e}"), true);
                return;
            }
        }
        let dir_s = dir.to_string_lossy().to_string();
        for (i, cmd) in prepare_commands(&s.image, &s.platform, &dir_s, &short)
            .into_iter()
            .enumerate()
        {
            say("build", format!("step {} of 3", i + 1), false);
            if let Err(e) = run_docker(&cmd) {
                say("error", e, true);
                return;
            }
        }
        say("ready", "the map is prepared".into(), true);
    });
    Ok(())
}

/// Start the router on the prepared map.
#[tauri::command]
pub fn mapdata_start(app: AppHandle, state: State<AppState>) -> Result<String, String> {
    let s = routing_get(state);
    let dir = data_dir(&app)?.to_string_lossy().to_string();
    let port = s
        .url
        .rsplit(':')
        .next()
        .and_then(|p| p.trim_end_matches('/').parse::<u16>().ok())
        .unwrap_or(5000);
    let _ = run_docker(&["rm".into(), "-f".into(), CONTAINER.into()]);
    run_docker(&serve_command(
        &s.image,
        &s.platform,
        &dir,
        &short_region(&s.region),
        port,
    ))
}

/// Which Geofabrik extract this listener needs, worked out from addresses
/// their own geocoder has already confirmed.
///
/// Every answer ends "…, Marion County, Indiana, 46218, United States", so
/// the state is in there hundreds of times over. Reading it from the
/// library beats keeping a table of states in the app, and it is right for
/// whoever is listening rather than right for one county.
pub fn region_from(validated: &[String]) -> Option<String> {
    let mut count: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    for v in validated {
        let parts: Vec<&str> = v.split(',').map(str::trim).collect();
        // …, <state>, <postcode>, <country>
        if parts.len() < 4 {
            continue;
        }
        let state = parts[parts.len() - 3];
        if state.len() < 4 || !state.chars().all(|c| c.is_alphabetic() || c == ' ') {
            continue;
        }
        *count.entry(state.to_string()).or_default() += 1;
    }
    let (state, n) = count.into_iter().max_by_key(|(_, n)| *n)?;
    (n >= 3).then(|| format!("us/{}", state.to_lowercase().replace(' ', "-")))
}

/// The region this library suggests, for the "From home" button.
#[tauri::command]
pub fn mapdata_region(state: State<AppState>) -> Result<String, String> {
    let db = state
        .db
        .lock()
        .unwrap()
        .clone()
        .ok_or("the call library is not open")?;
    let c = db.lock().unwrap();
    let mut q = c
        .prepare("SELECT validated FROM incidents WHERE geocode = 'ok' AND validated <> '' LIMIT 500")
        .map_err(|e| e.to_string())?;
    let rows: Vec<String> = q
        .query_map([], |r| r.get(0))
        .map_err(|e| e.to_string())?
        .flatten()
        .collect();
    region_from(&rows).ok_or_else(|| {
        "not enough placed addresses yet — type the region (a Geofabrik path like us/indiana)".into()
    })
}

#[tauri::command]
pub fn mapdata_stop() -> Result<String, String> {
    run_docker(&["rm".into(), "-f".into(), CONTAINER.into()])
}

/// Fetch a large file, saying how it is going. Written beside the target and
/// renamed at the end, so an interrupted download is never mistaken for a
/// finished one.
fn download(url: &str, to: &std::path::Path, app: &AppHandle) -> Result<(), String> {
    let part = to.with_extension("part");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(None)
        .http_status_as_error(false)
        .build()
        .into();
    let mut r = agent.get(url).call().map_err(|e| e.to_string())?;
    if r.status().as_u16() != 200 {
        return Err(format!("HTTP {}", r.status().as_u16()));
    }
    let total: u64 = r
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = r.body_mut().as_reader();
    let mut f = std::fs::File::create(&part).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 1 << 20];
    let mut got: u64 = 0;
    let mut said = 0u64;
    loop {
        let n = std::io::Read::read(&mut body, &mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        std::io::Write::write_all(&mut f, &buf[..n]).map_err(|e| e.to_string())?;
        got += n as u64;
        if got - said > 16 << 20 {
            said = got;
            let _ = app.emit(
                "mapdata",
                serde_json::json!({ "step": "download", "got": got, "total": total, "done": false }),
            );
        }
    }
    drop(f);
    std::fs::rename(&part, to).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_straight_line_is_always_available() {
        let d = straight((39.7684, -86.1581), (39.8684, -86.1581));
        assert_eq!(d.how, "straight");
        assert!((d.km() - 11.1).abs() < 0.2, "{} km", d.km());
        assert_eq!(d.secs, 0.0);
    }

    #[test]
    fn the_pipeline_is_three_commands_over_one_mounted_folder() {
        let cmds = prepare_commands("osrm/backend", "", "/tmp/maps", "indiana");
        assert_eq!(cmds.len(), 3);
        assert_eq!(
            cmds[0],
            vec![
                "run", "--rm", "-t", "-v", "/tmp/maps:/data", "osrm/backend",
                "osrm-extract", "-p", "/opt/car.lua", "/data/indiana.osm.pbf"
            ]
        );
        assert!(cmds[1].contains(&"osrm-partition".to_string()));
        assert!(cmds[2].contains(&"osrm-customize".to_string()));
        // Every step works on the same file, in the order OSRM requires.
        for c in &cmds[1..] {
            assert!(c.last().unwrap().ends_with("indiana.osrm"));
        }
    }

    #[test]
    fn a_chip_the_image_was_not_built_for_gets_a_platform() {
        let cmds = prepare_commands("osrm/backend", "linux/amd64", "/tmp/maps", "indiana");
        assert!(cmds
            .iter()
            .all(|c| c.windows(2).any(|w| w == ["--platform", "linux/amd64"])));
        let serve = serve_command("osrm/backend", "linux/amd64", "/tmp/maps", "indiana", 5000);
        assert!(serve.windows(2).any(|w| w == ["--platform", "linux/amd64"]));
        assert!(serve.windows(2).any(|w| w == ["-p", "5000:5000"]));
        assert!(serve.contains(&"--algorithm".to_string()) && serve.contains(&"mld".to_string()));
        assert!(serve.windows(2).any(|w| w == ["--name", CONTAINER]));
    }

    #[test]
    fn the_region_comes_from_addresses_already_placed() {
        let v: Vec<String> = [
            "5863, East 16th Street, Testville, Example County, Example State, 46218, United States",
            "100, Lamont Lane, Testville, Example County, Example State, 46218, United States",
            "7, Beech Grove Road, Testville, Example County, Example State, 46219, United States",
            "1, Elsewhere Road, Othertown, Other County, Other State, 12345, United States",
            "short",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(region_from(&v).as_deref(), Some("us/example-state"));
        // One or two placed addresses is not enough to go on.
        assert_eq!(region_from(&v[3..]), None);
        assert_eq!(region_from(&[]), None);
    }

    #[test]
    fn regions_name_their_file_and_their_download() {
        assert_eq!(short_region("us/indiana"), "indiana");
        assert_eq!(short_region("indiana"), "indiana");
        assert_eq!(
            extract_url("us/indiana"),
            "https://download.geofabrik.de/north-america/us/indiana-latest.osm.pbf"
        );
    }
}

/// Drive the whole Docker pipeline against a stand-in binary, so the
/// commands are checked without Docker running.
/// `HS_DOCKER=…/fakedocker.sh FAKE_DOCKER_LOG=… cargo test routing::pipeline -- --ignored --nocapture`
#[cfg(test)]
mod pipeline {
    use super::*;

    #[test]
    #[ignore]
    fn the_commands_docker_would_be_given() {
        let Ok(log) = std::env::var("FAKE_DOCKER_LOG") else {
            eprintln!("set FAKE_DOCKER_LOG");
            return;
        };
        let _ = std::fs::remove_file(&log);
        let dir = "/tmp/hs-osrm-test";
        for cmd in prepare_commands(
            "ghcr.io/project-osrm/osrm-backend",
            "linux/amd64",
            dir,
            "indiana",
        ) {
            run_docker(&cmd).expect("the stand-in always succeeds");
        }
        run_docker(&serve_command(
            "ghcr.io/project-osrm/osrm-backend",
            "linux/amd64",
            dir,
            "indiana",
            5000,
        ))
        .unwrap();
        let text = std::fs::read_to_string(&log).unwrap();
        println!("{text}");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4);
        assert!(lines[0].contains("osrm-extract") && lines[0].contains("/data/indiana.osm.pbf"));
        assert!(lines[1].contains("osrm-partition"));
        assert!(lines[2].contains("osrm-customize"));
        assert!(lines[3].contains("osrm-routed") && lines[3].contains("5000:5000"));
        assert!(lines.iter().all(|l| l.contains("--platform linux/amd64")));
        assert!(lines.iter().all(|l| l.contains(&format!("{dir}:/data"))));
    }
}
