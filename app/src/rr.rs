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

fn catalog_path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    Ok(config_dir(app)?.join("talkgroups.csv"))
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
    std::fs::write(&path, serde_json::to_string_pretty(p).map_err(|e| e.to_string())?)
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn secret(user: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, user).map_err(|e| format!("credential store: {e}"))
}

fn get_secret(user: &str) -> Option<String> {
    secret(user).ok()?.get_password().ok()
}

/// The catalog downloaded on a previous run, if any.
pub fn saved_catalog(app: &AppHandle) -> Option<CsvCatalog> {
    let text = std::fs::read_to_string(catalog_path(app).ok()?).ok()?;
    let cat = CsvCatalog::parse(&text);
    (!cat.is_empty()).then_some(cat)
}

#[derive(Serialize)]
pub struct RrSettings {
    username: String,
    sid: Option<u32>,
    has_app_key: bool,
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
        has_app_key: get_secret("app_key").is_some_and(|k| !k.is_empty()),
        has_password: !p.username.is_empty()
            && get_secret(&format!("password:{}", p.username)).is_some_and(|k| !k.is_empty()),
        username: p.username,
        sid: p.sid,
        system_name: p.system_name,
        catalog_len: state.catalog.lock().unwrap().as_ref().map_or(0, |c| c.len()),
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
pub struct RrDownload {
    name: String,
    talkgroups: usize,
    sites: Vec<RrSiteInfo>,
}

/// Download a system with the saved credentials: its talkgroups become the
/// live catalog (and are saved for next start), and its sites come back with
/// the control channels to tune.
#[tauri::command]
pub fn rr_download(app: AppHandle, state: State<AppState>, sid: u32) -> Result<RrDownload, String> {
    let mut p = load_prefs(&app);
    let app_key = get_secret("app_key").filter(|k| !k.is_empty()).ok_or("save your RadioReference app key first")?;
    if p.username.is_empty() {
        return Err("save your RadioReference username and password first".into());
    }
    let password = get_secret(&format!("password:{}", p.username))
        .filter(|k| !k.is_empty())
        .ok_or("save your RadioReference password first")?;
    let client = RrClient::new(Credentials::new(app_key, p.username.clone(), password));
    let sys = client.system(sid).map_err(|e| e.to_string())?;

    let csv = sys.talkgroup_csv();
    let cat = CsvCatalog::parse(&csv);
    let n = cat.len();
    if let Ok(path) = catalog_path(&app) {
        let _ = std::fs::write(path, &csv);
    }
    *state.catalog.lock().unwrap() = Some(cat);

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
                control_mhz: s.control_channels_hz.iter().map(|h| *h as f64 / 1e6).collect(),
                span_mhz: lo.zip(hi).map(|(a, b)| (a as f64 / 1e6, b as f64 / 1e6)),
            }
        })
        .collect();
    Ok(RrDownload {
        name: sys.name.clone().unwrap_or_else(|| format!("system {sid}")),
        talkgroups: n,
        sites,
    })
}
