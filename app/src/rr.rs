//! RadioReference from inside the app: credentials saved on this machine,
//! one click to download a system's talkgroups and control channels.
//!
//! The password and app key go in the OS credential store (macOS Keychain,
//! Windows Credential Manager) via `keyring`; the username and system id —
//! not secrets — in a small JSON file in the app's config directory, next to
//! the downloaded talkgroup CSV, which is loaded again on every start so the
//! catalog works offline afterwards.

use hs_catalog::radioreference::{Credentials, RrClient};
use hs_catalog::CsvCatalog;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

use crate::AppState;

const SERVICE: &str = "HoosierSDR RadioReference";

include!(concat!(env!("OUT_DIR"), "/rr_key.rs"));
const MASK: [u8; 16] = [
    0x5a, 0xc3, 0x91, 0x2e, 0x77, 0xb8, 0x04, 0xe5, 0x3c, 0x6f, 0xd2, 0x19, 0x8b, 0x40, 0xa7, 0xf1,
];

fn unmask(masked: &[u8]) -> String {
    masked
        .iter()
        .enumerate()
        .map(|(i, b)| (b ^ MASK[i % MASK.len()]) as char)
        .collect()
}

/// The key compiled into this build, if any (see `build.rs`).
fn embedded_key() -> Option<String> {
    let k = unmask(RR_KEY_MASKED);
    (!k.is_empty()).then_some(k)
}

/// The app key to use: this build's embedded key, else one the user saved.
fn app_key() -> Option<String> {
    embedded_key().or_else(|| get_secret("app_key").filter(|k| !k.is_empty()))
}

fn client(app: &AppHandle) -> Result<RrClient, String> {
    let p = load_prefs(app);
    let key = app_key()
        .ok_or("no RadioReference app key: this build has none embedded, so enter one in Config")?;
    if p.username.is_empty() {
        return Err("save your RadioReference username and password first".into());
    }
    let password = get_secret(&format!("password:{}", p.username))
        .filter(|k| !k.is_empty())
        .ok_or("save your RadioReference password first")?;
    Ok(RrClient::new(Credentials::new(key, p.username, password)))
}

/// Small on-disk cache for browse responses: the geography tree barely
/// changes and RadioReference rate-limits, so each lookup is fetched once
/// unless the user asks to refresh.
fn cached<T: Serialize + serde::de::DeserializeOwned>(
    app: &AppHandle,
    name: &str,
    refresh: bool,
    fetch: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let dir = config_dir(app)?.join("rr-cache");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{name}.json"));
    if !refresh {
        if let Some(v) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<T>(&t).ok())
        {
            return Ok(v);
        }
    }
    let v = fetch()?;
    if let Ok(t) = serde_json::to_string(&v) {
        let _ = std::fs::write(&path, t);
    }
    Ok(v)
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct Prefs {
    username: String,
    sid: Option<u32>,
    /// Name of the last system downloaded, for the UI.
    system_name: Option<String>,
}

fn config_dir(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let d = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("config dir: {e}"))?;
    std::fs::create_dir_all(&d).map_err(|e| format!("config dir {}: {e}", d.display()))?;
    Ok(d)
}

fn prefs_path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    Ok(config_dir(app)?.join("radioreference.json"))
}

/// One CSV per source under `catalogs/`; all are merged into the live
/// catalog, so several systems can be named at once.
pub fn catalogs_dir(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let d = config_dir(app)?.join("catalogs");
    std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    Ok(d)
}

/// Merge every saved catalog file (and the legacy single file, if present).
pub fn merged_catalog(app: &AppHandle) -> Option<CsvCatalog> {
    let mut cat = CsvCatalog::default();
    if let Ok(d) = catalogs_dir(app) {
        let mut files: Vec<_> = std::fs::read_dir(&d)
            .ok()?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "csv"))
            .collect();
        files.sort();
        for f in files {
            if let Ok(t) = std::fs::read_to_string(&f) {
                cat.merge(&CsvCatalog::parse(&t));
            }
        }
    }
    if let Ok(legacy) = config_dir(app).map(|d| d.join("talkgroups.csv")) {
        if let Ok(t) = std::fs::read_to_string(legacy) {
            cat.merge(&CsvCatalog::parse(&t));
        }
    }
    (!cat.is_empty()).then_some(cat)
}

/// Saved catalog sources, for the UI.
#[derive(Serialize)]
pub struct CatalogFile {
    name: String,
    talkgroups: usize,
}

#[tauri::command]
pub fn catalogs_list(app: AppHandle) -> Vec<CatalogFile> {
    let Ok(d) = catalogs_dir(&app) else {
        return vec![];
    };
    let mut v: Vec<CatalogFile> = std::fs::read_dir(d)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "csv"))
        .filter_map(|e| {
            let t = std::fs::read_to_string(e.path()).ok()?;
            Some(CatalogFile {
                name: e
                    .file_name()
                    .to_string_lossy()
                    .trim_end_matches(".csv")
                    .to_string(),
                talkgroups: CsvCatalog::parse(&t).len(),
            })
        })
        .collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

#[derive(Serialize)]
pub struct AliasRow {
    pub id: u16,
    pub alias: String,
    pub description: String,
    pub category: String,
    pub encrypted: bool,
    pub source: String,
}

/// Every named talkgroup across all saved catalogs, with its source.
#[tauri::command]
pub fn catalog_rows(app: AppHandle) -> Vec<AliasRow> {
    let Ok(d) = catalogs_dir(&app) else {
        return vec![];
    };
    let mut rows: Vec<AliasRow> = Vec::new();
    let mut files: Vec<_> = std::fs::read_dir(d)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "csv"))
        .collect();
    files.sort();
    for f in files {
        let source = f
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Ok(t) = std::fs::read_to_string(&f) else {
            continue;
        };
        let cat = CsvCatalog::parse(&t);
        if let Ok(tgs) = hs_catalog::Catalog::talkgroups(&cat, 0) {
            for tg in tgs {
                rows.push(AliasRow {
                    id: tg.id,
                    alias: tg.alias.unwrap_or_default(),
                    description: tg.description.unwrap_or_default(),
                    category: tg.category.unwrap_or_default(),
                    encrypted: tg.encrypted,
                    source: source.clone(),
                });
            }
        }
    }
    rows.sort_by_key(|r| (r.id, r.source.clone()));
    rows
}

/// Is this talkgroup named? Which source names it?
#[tauri::command]
pub fn catalog_lookup(app: AppHandle, tg: u16) -> Vec<AliasRow> {
    catalog_rows(app)
        .into_iter()
        .filter(|r| r.id == tg)
        .collect()
}

#[tauri::command]
pub fn catalog_remove(
    app: AppHandle,
    state: State<AppState>,
    name: String,
) -> Result<usize, String> {
    let p = catalogs_dir(&app)?.join(format!("{}.csv", name.replace(['/', '\\'], "_")));
    std::fs::remove_file(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    let cat = merged_catalog(&app);
    let n = cat.as_ref().map_or(0, |c| c.len());
    *state.catalog.lock().unwrap() = cat;
    Ok(n)
}

/// Name a talkgroup by hand (discovery: "I heard TG 20308, call it Sheriff
/// Patrol"). Rows go into `csv_user.csv` in the catalogs folder — a plain
/// RadioReference-shaped CSV the merge already reads, newest source wins —
/// and the live catalog is reloaded. An empty alias removes the row.
#[tauri::command]
pub fn catalog_user_set(
    app: AppHandle,
    state: State<AppState>,
    tg: u16,
    alias: String,
    category: Option<String>,
) -> Result<usize, String> {
    let p = catalogs_dir(&app)?.join("csv_user.csv");
    let mut rows: Vec<(u16, String, String)> = std::fs::read_to_string(&p)
        .ok()
        .map(|t| {
            t.lines()
                .skip(1)
                .filter_map(|l| {
                    let f: Vec<&str> = l.split(',').collect();
                    let id = f.first()?.trim().parse().ok()?;
                    let alias = f.get(2).map(|s| s.trim_matches('"').to_string()).unwrap_or_default();
                    let cat = f.get(6).map(|s| s.trim_matches('"').to_string()).unwrap_or_default();
                    Some((id, alias, cat))
                })
                .collect()
        })
        .unwrap_or_default();
    rows.retain(|r| r.0 != tg);
    let alias = alias.trim().replace(['"', ','], " ");
    if !alias.is_empty() {
        rows.push((
            tg,
            alias,
            category.unwrap_or_default().trim().replace(['"', ','], " "),
        ));
    }
    rows.sort_by_key(|r| r.0);
    let mut text = String::from("Decimal,Hex,Alpha Tag,Mode,Description,Tag,Category,Priority\n");
    for (id, a, c) in &rows {
        text.push_str(&format!("{id},{id:X},\"{a}\",D,\"{a}\",,\"{c}\",\n"));
    }
    if rows.is_empty() {
        let _ = std::fs::remove_file(&p);
    } else {
        std::fs::write(&p, text).map_err(|e| format!("{}: {e}", p.display()))?;
    }
    let cat = merged_catalog(&app);
    let n = cat.as_ref().map_or(0, |c| c.len());
    *state.catalog.lock().unwrap() = cat;
    Ok(n)
}

/// Write text the UI produced (a discovery CSV, a report) to a file the
/// listener named. `~` expands; parent folders are created.
#[tauri::command]
pub fn save_text(path: String, text: String) -> Result<String, String> {
    let p = std::path::PathBuf::from(crate::shellexpand_home(&path));
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    std::fs::write(&p, text).map_err(|e| format!("{}: {e}", p.display()))?;
    Ok(p.to_string_lossy().into_owned())
}

fn load_prefs(app: &AppHandle) -> Prefs {
    prefs_path(app)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_prefs(app: &AppHandle, p: &Prefs) -> Result<(), String> {
    let path = prefs_path(app)?;
    std::fs::write(
        &path,
        serde_json::to_string_pretty(p).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("{}: {e}", path.display()))
}

fn secret(user: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, user).map_err(|e| format!("credential store: {e}"))
}

fn get_secret(user: &str) -> Option<String> {
    secret(user).ok()?.get_password().ok()
}

/// The catalogs saved on previous runs, merged.
pub fn saved_catalog(app: &AppHandle) -> Option<CsvCatalog> {
    merged_catalog(app)
}

#[derive(Serialize)]
pub struct RrSettings {
    username: String,
    sid: Option<u32>,
    has_app_key: bool,
    /// The key is built in; the app-key field is not needed.
    embedded_key: bool,
    has_password: bool,
    system_name: Option<String>,
    /// Talkgroups in the catalog currently loaded (from disk at start).
    catalog_len: usize,
}

/// What the app has saved — never the secrets themselves.
#[tauri::command]
pub fn rr_settings(app: AppHandle, state: State<AppState>) -> RrSettings {
    let p = load_prefs(&app);
    RrSettings {
        has_app_key: app_key().is_some(),
        embedded_key: embedded_key().is_some(),
        has_password: !p.username.is_empty()
            && get_secret(&format!("password:{}", p.username)).is_some_and(|k| !k.is_empty()),
        username: p.username,
        sid: p.sid,
        system_name: p.system_name,
        catalog_len: state
            .catalog
            .lock()
            .unwrap()
            .as_ref()
            .map_or(0, |c| c.len()),
    }
}

/// Save credentials. Empty `app_key` / `password` keep what is stored.
#[tauri::command]
pub fn rr_save(
    app: AppHandle,
    app_key: String,
    username: String,
    password: String,
    sid: Option<u32>,
) -> Result<(), String> {
    let mut p = load_prefs(&app);
    p.username = username.trim().to_string();
    p.sid = sid;
    if !app_key.trim().is_empty() {
        secret("app_key")?
            .set_password(app_key.trim())
            .map_err(|e| format!("save app key: {e}"))?;
    }
    if !password.is_empty() {
        if p.username.is_empty() {
            return Err("enter the username the password belongs to".into());
        }
        secret(&format!("password:{}", p.username))?
            .set_password(&password)
            .map_err(|e| format!("save password: {e}"))?;
    }
    save_prefs(&app, &p)
}

#[derive(Serialize)]
pub struct RrSiteInfo {
    site_id: u32,
    name: String,
    nac: Option<u16>,
    tdma_control: bool,
    control_mhz: Vec<f64>,
    /// Lowest and highest frequency on the site — the span a radio must cover
    /// to follow every call.
    span_mhz: Option<(f64, f64)>,
}

#[derive(Serialize)]
pub struct RrTalkgroup {
    id: u16,
    alias: String,
    description: String,
    category: String,
    encrypted: bool,
}

#[derive(Serialize)]
pub struct RrDownload {
    sid: u32,
    name: String,
    talkgroups: usize,
    sites: Vec<RrSiteInfo>,
    tgs: Vec<RrTalkgroup>,
}

/// Download a system with the saved credentials: its talkgroups become the
/// live catalog (and are saved for next start), and its sites come back with
/// the control channels to tune.
#[tauri::command]
pub async fn rr_download(app: AppHandle, sid: u32) -> Result<RrDownload, String> {
    tauri::async_runtime::spawn_blocking(move || rr_download_blocking(app, sid))
        .await
        .map_err(|e| e.to_string())?
}

fn rr_download_blocking(app: AppHandle, sid: u32) -> Result<RrDownload, String> {
    let state = app.state::<AppState>();
    let mut p = load_prefs(&app);
    let client = client(&app)?;
    use tauri::Emitter;
    let mut report = |step: &str, done: usize, total: usize| {
        let _ = app.emit(
            "rr_progress",
            serde_json::json!({ "sid": sid, "step": step, "done": done, "total": total }),
        );
    };
    let sys = client.system_with_progress(sid, &mut report).map_err(|e| {
        eprintln!("[rr] download {sid} failed: {e}");
        let _ = app.emit("rr_progress", serde_json::json!({ "sid": sid, "step": "failed", "done": 0, "total": 0 }));
        e.to_string()
    })?;
    report("saving", 3, 3);

    let csv = sys.talkgroup_csv();
    let n = CsvCatalog::parse(&csv).len();
    // Saved per system and merged with whatever else is loaded.
    if let Ok(d) = catalogs_dir(&app) {
        let _ = std::fs::write(d.join(format!("rr_{sid}.csv")), &csv);
    }
    *state.catalog.lock().unwrap() = merged_catalog(&app);
    report("done", 3, 3);

    p.sid = Some(sid);
    p.system_name = sys.name.clone();
    let _ = save_prefs(&app, &p);

    let sites = sys
        .sites
        .iter()
        .filter(|s| !s.control_channels_hz.is_empty())
        .map(|s| {
            let lo = s.frequencies_hz.iter().min().copied();
            let hi = s.frequencies_hz.iter().max().copied();
            RrSiteInfo {
                site_id: s.site_id,
                name: s
                    .description
                    .clone()
                    .or_else(|| s.county.clone())
                    .unwrap_or_else(|| format!("site {}", s.site_id)),
                nac: s.nac,
                tdma_control: s.tdma_control,
                control_mhz: s
                    .control_channels_hz
                    .iter()
                    .map(|h| *h as f64 / 1e6)
                    .collect(),
                span_mhz: lo.zip(hi).map(|(a, b)| (a as f64 / 1e6, b as f64 / 1e6)),
            }
        })
        .collect();
    let tgs = sys
        .talkgroups
        .iter()
        .map(|t| RrTalkgroup {
            id: t.id,
            alias: t.alias.clone().unwrap_or_default(),
            description: t.description.clone().unwrap_or_default(),
            category: t.category.clone().unwrap_or_default(),
            encrypted: t.encrypted,
        })
        .collect();
    Ok(RrDownload {
        sid,
        name: sys.name.clone().unwrap_or_else(|| format!("system {sid}")),
        talkgroups: n,
        sites,
        tgs,
    })
}

// ---------------------------------------------------------------------------
// Browsing: state → county → systems, or straight from a ZIP code.
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone)]
pub struct StateRow {
    stid: u32,
    name: String,
    code: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct CountyRow {
    ctid: u32,
    name: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct SystemRow {
    sid: u32,
    name: String,
    stype: Option<u32>,
    city: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct StateView {
    counties: Vec<CountyRow>,
    systems: Vec<SystemRow>,
}

#[derive(Serialize)]
pub struct ZipView {
    city: Option<String>,
    stid: u32,
    ctid: u32,
}

fn sys_rows(v: Vec<hs_catalog::radioreference::RrSystemRef>) -> Vec<SystemRow> {
    v.into_iter()
        .map(|s| SystemRow {
            sid: s.sid,
            name: s.name,
            stype: s.stype,
            city: s.city,
        })
        .collect()
}

#[tauri::command]
pub async fn rr_states(app: AppHandle, refresh: Option<bool>) -> Result<Vec<StateRow>, String> {
    tauri::async_runtime::spawn_blocking(move || rr_states_blocking(app, refresh))
        .await
        .map_err(|e| e.to_string())?
}

fn rr_states_blocking(app: AppHandle, refresh: Option<bool>) -> Result<Vec<StateRow>, String> {
    cached(&app, "states", refresh.unwrap_or(false), || {
        let c = client(&app)?;
        Ok(c.states()
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|s| StateRow {
                stid: s.stid,
                name: s.name,
                code: s.code,
            })
            .collect())
    })
}

#[tauri::command]
pub async fn rr_state(
    app: AppHandle,
    stid: u32,
    refresh: Option<bool>,
) -> Result<StateView, String> {
    tauri::async_runtime::spawn_blocking(move || rr_state_blocking(app, stid, refresh))
        .await
        .map_err(|e| e.to_string())?
}

fn rr_state_blocking(
    app: AppHandle,
    stid: u32,
    refresh: Option<bool>,
) -> Result<StateView, String> {
    cached(
        &app,
        &format!("state_{stid}"),
        refresh.unwrap_or(false),
        || {
            let c = client(&app)?;
            let info = c.state_info(stid).map_err(|e| e.to_string())?;
            Ok(StateView {
                counties: info
                    .counties
                    .into_iter()
                    .map(|c| CountyRow {
                        ctid: c.ctid,
                        name: c.name,
                    })
                    .collect(),
                systems: sys_rows(info.systems),
            })
        },
    )
}

#[tauri::command]
pub async fn rr_county(
    app: AppHandle,
    ctid: u32,
    refresh: Option<bool>,
) -> Result<Vec<SystemRow>, String> {
    tauri::async_runtime::spawn_blocking(move || rr_county_blocking(app, ctid, refresh))
        .await
        .map_err(|e| e.to_string())?
}

fn rr_county_blocking(
    app: AppHandle,
    ctid: u32,
    refresh: Option<bool>,
) -> Result<Vec<SystemRow>, String> {
    cached(
        &app,
        &format!("county_{ctid}"),
        refresh.unwrap_or(false),
        || {
            let c = client(&app)?;
            Ok(sys_rows(c.county_systems(ctid).map_err(|e| e.to_string())?))
        },
    )
}

#[tauri::command]
pub async fn rr_zip(app: AppHandle, zip: u32) -> Result<ZipView, String> {
    tauri::async_runtime::spawn_blocking(move || rr_zip_blocking(app, zip))
        .await
        .map_err(|e| e.to_string())?
}

fn rr_zip_blocking(app: AppHandle, zip: u32) -> Result<ZipView, String> {
    let c = client(&app)?;
    let z = c.zipcode(zip).map_err(|e| e.to_string())?;
    Ok(ZipView {
        city: z.city,
        stid: z.stid,
        ctid: z.ctid,
    })
}

#[cfg(test)]
mod tests {
    /// The build-time masking round-trips (checked with a dummy key — the
    /// real one never appears in tests or the repo).
    /// Only meaningful on a machine that builds with a key; reports, never
    /// prints, the key.
    #[test]
    fn this_build_embeds_a_key_when_configured() {
        let configured =
            std::env::var("HS_RR_APP_KEY").is_ok() || std::path::Path::new(".rr_app_key").exists();
        if configured {
            assert!(super::embedded_key().is_some_and(|k| k.len() >= 8));
        }
    }

    #[test]
    fn key_mask_round_trips() {
        let key = "dummy-app-key-0123456789";
        let masked: Vec<u8> = key
            .bytes()
            .enumerate()
            .map(|(i, b)| b ^ super::MASK[i % super::MASK.len()])
            .collect();
        assert_eq!(super::unmask(&masked), key);
        assert_ne!(masked, key.as_bytes());
        assert_eq!(super::unmask(&[]), "");
    }
}
