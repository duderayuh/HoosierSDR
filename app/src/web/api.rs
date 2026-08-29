//! JSON command dispatcher — mirrors the desktop app's Tauri commands over HTTP
//! so the mobile UI can read and drive the same live `AppState`.
//!
//! Each command name maps straight onto the existing `#[tauri::command]`
//! function (now `pub(crate)` where needed), so there is no logic duplication
//! and no drift: one source of truth.

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;
use tauri::{AppHandle, Manager};

use crate::{AppState, ExtraSpec};

/// Pull one named argument out of the JSON args object (missing → `null`).
fn arg<T: DeserializeOwned>(args: &Value, key: &str) -> Result<T, String> {
    let v = args.get(key).cloned().unwrap_or(Value::Null);
    serde_json::from_value(v).map_err(|e| format!("{key}: {e}"))
}

fn jv<T: serde::Serialize>(v: T) -> Result<Value, String> {
    serde_json::to_value(v).map_err(|e| e.to_string())
}

/// Arguments for `start_follow` (everything after `app` and `state`).
#[derive(Deserialize)]
struct StartArgs {
    source: String,
    freq: f64,
    rate: f64,
    #[serde(default)]
    gain: Option<f64>,
    control: f64,
    #[serde(default)]
    calls_dir: Option<String>,
    #[serde(default)]
    play: bool,
    #[serde(default)]
    hang_ms: Option<u32>,
    #[serde(default)]
    system_name: Option<String>,
    #[serde(default)]
    site_name: Option<String>,
    #[serde(default)]
    ppm: Option<f64>,
    #[serde(default)]
    device: Option<String>,
    #[serde(default)]
    modulation: Option<String>,
    #[serde(default)]
    extra: Option<Vec<ExtraSpec>>,
}

/// Dispatch one command. `args` is the JSON object the client sent.
pub fn dispatch(app: &AppHandle, cmd: &str, args: &Value) -> Result<Value, String> {
    let state = app.state::<AppState>();

    match cmd {
        // ---- status / monitoring ----
        "sys_status" => jv(crate::sysstat::sys_status(state)),
        "audio_queued" => jv(crate::audio_queued(state)),
        "catalog_rows" => jv(crate::rr::catalog_rows(app.clone())),
        "playlists_list" => jv(crate::playlists::playlists_list(app.clone())),
        "devices_get" => jv(crate::devices::devices_get(app.clone())),
        "alerts_get" => jv(crate::alerts::alerts_get(state)),
        "alerts_log" => jv(crate::alerts::alerts_log(state)),
        "conversations_state" => jv(crate::conversations::conversations_state(state)),
        "digests_log" => jv(crate::digest::digests_log(state)),
        "analyzers_log" => jv(crate::analyzers::analyzers_log(state)),
        "units_list" => jv(crate::units::units_list(state)),
        "rr_settings" => jv(crate::rr::rr_settings(app.clone(), state)),

        // ---- control ----
        "start_follow" => {
            let a: StartArgs = serde_json::from_value(args.clone()).map_err(|e| e.to_string())?;
            crate::start_follow(
                app.clone(),
                state,
                a.source,
                a.freq,
                a.rate,
                a.gain,
                a.control,
                a.calls_dir,
                a.play,
                a.hang_ms,
                a.system_name,
                a.site_name,
                a.ppm,
                a.device,
                a.modulation,
                a.extra,
            )
            .map_err(|e| e)?;
            Ok(Value::Null)
        }
        "stop_capture" => {
            crate::stop_capture(state);
            Ok(Value::Null)
        }
        "set_hold" => {
            crate::set_hold(arg::<Option<u16>>(args, "tg")?, state);
            Ok(Value::Null)
        }
        "skip_call" => {
            crate::skip_call(state);
            Ok(Value::Null)
        }
        "replay_last" => crate::replay_last(state).map(|_| Value::Null).map_err(|e| e),
        "clear_queue" => {
            crate::clear_queue(state);
            Ok(Value::Null)
        }
        "set_volume" => {
            crate::set_volume(arg(args, "gain")?, state);
            Ok(Value::Null)
        }
        "get_volume" => jv(crate::get_volume(state)),
        "set_allowlist" => {
            crate::set_allowlist(arg::<Option<Vec<u16>>>(args, "tgs")?, state);
            Ok(Value::Null)
        }
        "set_lockout" => {
            crate::set_lockout(arg::<Vec<u16>>(args, "tgs")?, state);
            Ok(Value::Null)
        }
        "set_priorities" => {
            crate::set_priorities(arg::<Vec<(u16, u8)>>(args, "entries")?, state);
            Ok(Value::Null)
        }
        "set_lockout_ranges" => {
            crate::set_lockout_ranges(arg::<Vec<(u16, u16)>>(args, "ranges")?, state);
            Ok(Value::Null)
        }
        "set_priority_ranges" => {
            crate::set_priority_ranges(arg::<Vec<(u16, u16, u8)>>(args, "ranges")?, state);
            Ok(Value::Null)
        }
        "set_max_calls" => {
            crate::set_max_calls(arg(args, "n")?, state);
            Ok(Value::Null)
        }
        "set_queue_limit" => {
            crate::set_queue_limit(arg(args, "secs")?, state);
            Ok(Value::Null)
        }
        "set_channelizer" => {
            crate::set_channelizer(arg(args, "on")?, state);
            Ok(Value::Null)
        }
        "set_uv_quality" => {
            crate::set_uv_quality(arg(args, "q")?, state);
            Ok(Value::Null)
        }
        "spectrum_set" => {
            crate::spectrum_set(state, arg(args, "fft")?, arg(args, "average")?);
            Ok(Value::Null)
        }
        "playlist_activate" => {
            let id = arg::<Option<String>>(args, "id")?;
            crate::playlists::playlist_activate(app.clone(), state, id)
                .map(|p| jv(p).unwrap_or(Value::Null))
                .map_err(|e| e)
        }

        other => Err(format!("unknown command: {other}")),
    }
}
