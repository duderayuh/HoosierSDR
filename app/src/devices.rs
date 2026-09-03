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
    /// RTL-SDR tuner gain in dB; `None` = a fixed 40 dB default (the tuner's
    /// hardware AGC proved unreliable and garbled voice).
    pub gain: Option<f64>,
    /// Preferred sample rate, Hz (0 = the radio's default for the mode).
    pub rate: f64,
    /// Airspy: customize the gain instead of the weak-signal sensitivity
    /// default `AirspySource::open` now applies on its own
    /// (`DEFAULT_SENSITIVITY_GAIN`). Off by default — most sites don't need
    /// hand-tuning; turn this on to pick linearity/manual mode, or a
    /// different sensitivity level, e.g. to back off from ADC clipping near
    /// a strong tower.
    #[serde(default)]
    pub airspy_gain: bool,
    /// Airspy mode: "agc" (front-end AGCs on), "linearity", "sensitivity",
    /// "manual".
    #[serde(default)]
    pub airspy_mode: String,
    /// Preset 0–21 for linearity / sensitivity.
    #[serde(default)]
    pub airspy_preset: u8,
    #[serde(default)]
    pub airspy_lna: u8,
    #[serde(default)]
    pub airspy_mixer: u8,
    #[serde(default)]
    pub airspy_vga: u8,
    #[serde(default)]
    pub airspy_lna_agc: bool,
    #[serde(default)]
    pub airspy_mixer_agc: bool,
}

impl DeviceSettings {
    /// The gain to apply for a radio of `kind`, or `None` to leave it at its
    /// own default (Airspy: `AirspySource::open`'s weak-signal sensitivity
    /// default; RTL-SDR: see the `gain` field).
    pub fn gain_setting(&self, kind: &str) -> Option<hs_source::GainSetting> {
        use hs_source::GainSetting as G;
        if kind == "airspy" {
            if !self.airspy_gain {
                return None;
            }
            return Some(match self.airspy_mode.as_str() {
                "linearity" => G::AirspyLinearity(self.airspy_preset.min(21)),
                "sensitivity" => G::AirspySensitivity(self.airspy_preset.min(21)),
                "manual" => G::AirspyManual {
                    lna: self.airspy_lna.min(14),
                    mixer: self.airspy_mixer.min(15),
                    vga: self.airspy_vga.min(15),
                    lna_agc: self.airspy_lna_agc,
                    mixer_agc: self.airspy_mixer_agc,
                },
                _ => G::Agc,
            });
        }
        if kind == "soapy" {
            // RTL-SDR through SoapySDR. The tuner reports its own gain ladder
            // at open() (R820T2 0–49.6 dB, E4000 −1–42 dB) and SoapyRtlSource
            // clamps the value into that range, so pass the raw dB through and
            // let the device decide. R820T/R820T2 hardware AGC is unreliable —
            // it garbled voice and, at floor gain, could not lock the control
            // channel — so a fresh device defaults to a fixed manual gain like
            // the pure-Rust path, never AGC.
            return Some(match self.gain {
                Some(db) => G::Manual(db),
                None => G::Manual(hs_source::RTL_DEFAULT_GAIN_DB),
            });
        }
        Some(match self.gain {
            Some(db) => G::Manual(hs_source::clamp_rtl_gain(db)),
            None => G::Manual(hs_source::RTL_DEFAULT_GAIN_DB),
        })
    }
}

/// Settings for one radio by its picker key ("kind|id"), or the defaults.
pub fn settings_for(app: &AppHandle, kind: &str, id: Option<&str>) -> DeviceSettings {
    let all = load(app);
    id.and_then(|i| all.get(&format!("{kind}|{i}")).cloned())
        .unwrap_or_default()
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
    // RTL-SDRs: route by tuner. The pure-Rust `rtlsdr` driver applies manual
    // gain correctly and decodes the control channel; the SoapySDR path leaves
    // R820T-family tuners at AGC/floor gain (SoapyRTLSDR never switches them
    // out of auto gain mode) so the control channel is ~35 dB weak and cannot
    // be found. The pure-Rust driver can't open an E4000, so those (and any
    // other tuner it doesn't support) stay on the SoapySDR path.
    let rtl_rates = vec![2_400_000.0, 1_024_000.0, 250_000.0];
    let soapy_devs = hs_source::soapy::SoapyRtlSource::list(); // (args, label, tuner)
                                                               // The pure-Rust enumeration reports no tuner, so map serial → tuner from
                                                               // SoapySDR (which does report it) to decide which path each dongle takes.
    let mut tuner_by_serial: HashMap<String, String> = HashMap::new();
    for (args, _, tuner) in &soapy_devs {
        if let Some(serial) = serial_from_args(args) {
            tuner_by_serial.insert(serial, tuner.clone());
        }
    }
    let is_rtl_tuner = |t: &str| t.contains("R820T") || t.contains("R828D");
    for (args, label) in hs_source::rtlsdr::RtlSdrSource::list() {
        let tuner = serial_from_args(&args).and_then(|s| tuner_by_serial.get(&s).cloned());
        if tuner.as_deref().map(is_rtl_tuner).unwrap_or(true) {
            v.push(Device {
                kind: "rtlsdr".into(),
                id: args,
                label,
                rates: rtl_rates.clone(),
            });
        }
    }
    for (args, label, tuner) in soapy_devs {
        if is_rtl_tuner(&tuner) {
            continue; // already listed above via the pure-Rust driver
        }
        v.push(Device {
            kind: "soapy".into(),
            id: args,
            label,
            rates: rtl_rates.clone(),
        });
    }
    v
}

/// Pull the `serial=` value out of a seify/SoapySDR args string, if present.
fn serial_from_args(args: &str) -> Option<String> {
    args.split(',').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k.trim() == "serial" && !v.trim().is_empty()).then(|| v.trim().to_string())
    })
}

#[derive(Serialize)]
pub struct View {
    pub devices: Vec<Device>,
    pub settings: HashMap<String, DeviceSettings>,
    /// The RTL-SDR tuner's gain steps, for the picker.
    pub rtl_gains_db: Vec<f64>,
    /// The E4000 tuner's gain steps (Smartee XTR), for the picker.
    pub e4000_gains_db: Vec<f64>,
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
        rtl_gains_db: hs_source::RTL_TUNER_GAINS_DB.to_vec(),
        e4000_gains_db: hs_source::E4000_TUNER_GAINS_DB.to_vec(),
    }
}

/// Change a streaming radio's gain now. `key` is the picker key of the
/// radio ("kind|id") — it must be one of the radios the current run opened.
#[tauri::command]
pub fn gain_live(
    app: AppHandle,
    state: tauri::State<crate::AppState>,
    key: String,
    settings: DeviceSettings,
) -> Result<String, String> {
    let kind = key.split('|').next().unwrap_or("").to_string();
    let g = settings
        .gain_setting(&kind)
        .ok_or("gain control is opted out for this Airspy (tick “apply gain” first)")?;
    let handles = state.gain_handles.lock().unwrap();
    let h = handles
        .get(&key)
        .ok_or("that radio is not part of the current run — Start first")?;
    h.request(g.clone());
    // Remember it for the next start too.
    let mut all = load(&app);
    all.insert(key.clone(), settings);
    if let Ok(p) = path(&app) {
        let _ = std::fs::write(&p, serde_json::to_string_pretty(&all).unwrap_or_default());
    }
    Ok(format!("applied {g:?}"))
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
