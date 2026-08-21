//! Attached radios: what is plugged in, and each one's own settings (ppm,
//! gain, nickname). Detected fresh on every request, so the UI can default
//! to whichever radio is actually present.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tauri::{AppHandle, Manager};

#[derive(Serialize, Clone, Debug)]
pub struct Device {
    /// "airspy" | "rtlsdr"
    pub kind: String,
    /// Airspy serial (hex) or the Seify args that reopen this RTL-SDR.
    pub id: String,
    pub label: String,
    /// Sample rates this radio offers, Hz.
    pub rates: Vec<f64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct DeviceSettings {
    pub nickname: String,
    /// Oscillator error, ppm (positive = runs high).
    pub ppm: f64,
    /// RTL-SDR gain in dB; `None` = AGC. Ignored by the Airspy R2.
    pub gain: Option<f64>,
    /// Preferred sample rate, Hz (0 = the radio's default for the mode).
    pub rate: f64,
}

fn path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let d = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("config dir: {e}"))?;
    std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    Ok(d.join("devices.json"))
}

pub fn load(app: &AppHandle) -> HashMap<String, DeviceSettings> {
    path(app)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Probe the USB bus for radios. Airspys first (they cover a whole site).
pub fn detect() -> Vec<Device> {
    let mut v: Vec<Device> = hs_source::airspy::AirspySource::list()
        .into_iter()
        .map(|sn| Device {
            kind: "airspy".into(),
            id: format!("{sn:016X}"),
            label: format!("Airspy · {:X}", sn),
            rates: vec![10_000_000.0, 2_500_000.0],
        })
        .collect();
    v.extend(
        hs_source::rtlsdr::RtlSdrSource::list()
            .into_iter()
            .map(|(args, label)| Device {
                kind: "rtlsdr".into(),
                id: args,
                label,
                rates: vec![2_400_000.0, 1_024_000.0, 250_000.0],
            }),
    );
    v
}

#[derive(Serialize)]
pub struct View {
    pub devices: Vec<Device>,
    pub settings: HashMap<String, DeviceSettings>,
}

/// Detect radios (runs the USB probe off the UI thread).
#[tauri::command]
pub async fn devices_list(app: AppHandle) -> View {
    let devices = tauri::async_runtime::spawn_blocking(detect)
        .await
        .unwrap_or_default();
    View {
        devices,
        settings: load(&app),
    }
}

#[tauri::command]
pub fn devices_get(app: AppHandle) -> HashMap<String, DeviceSettings> {
    load(&app)
}

/// Store one device's settings (keyed by its id).
#[tauri::command]
pub fn devices_set(app: AppHandle, id: String, settings: DeviceSettings) -> Result<(), String> {
    let mut all = load(&app);
    all.insert(id, settings);
    let p = path(&app)?;
    std::fs::write(
        &p,
        serde_json::to_string_pretty(&all).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("{}: {e}", p.display()))
}
