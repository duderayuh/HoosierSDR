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
/// `async` so it can drive the async commands (`devices_list`, `library_play`)
/// without blocking the web server's runtime.
pub async fn dispatch(app: &AppHandle, cmd: &str, args: &Value) -> Result<Value, String> {
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
        "replay_last" => crate::replay_last(state)
            .map(|_| Value::Null)
            .map_err(|e| e),
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

        // ---- devices / live gain ----
        "devices_list" => jv(crate::devices::devices_list(app.clone()).await),
        "gain_live" => {
            let key = arg::<String>(args, "key")?;
            let settings: crate::devices::DeviceSettings = arg(args, "settings")?;
            jv(crate::devices::gain_live(
                app.clone(),
                state,
                key,
                settings,
            )?)
        }

        // ---- library: browse + replay ----
        "library_search" => {
            let q: crate::library::Query =
                serde_json::from_value(args.clone()).map_err(|e| e.to_string())?;
            jv(crate::library_search(state, q)?)
        }
        "library_stats" => {
            let (count, seconds, transcribed, dir) = crate::library_stats(state)?;
            Ok(serde_json::json!({
                "count": count,
                "seconds": seconds,
                "transcribed": transcribed,
                "dir": dir,
            }))
        }
        "library_get" => jv(crate::library_get(state, arg(args, "id")?)?),
        "library_play" => {
            let id = arg(args, "id")?;
            crate::library_play(app.clone(), id).await?;
            Ok(Value::Null)
        }
        "library_star" => {
            let id = arg(args, "id")?;
            let on = arg(args, "on")?;
            crate::library_star(state, id, on)?;
            Ok(Value::Null)
        }
        "tg_latest_call" => jv(crate::tg_latest_call(state, arg(args, "tg")?)?),

        // ---- control extras ----
        "set_archive_mode" => {
            crate::set_archive_mode(state, arg(args, "on")?);
            Ok(Value::Null)
        }

        // ---- settings: names / format / policies ----
        "names_get" => jv(crate::names_get(state)),
        "names_set" => jv(crate::names_set(
            app.clone(),
            state,
            arg(args, "template")?,
        )?),
        "names_preview" => jv(crate::names_preview(arg(args, "template")?)),
        "format_get" => jv(crate::format_get(state)),
        "format_set" => {
            let format: crate::encode::Format = arg(args, "format")?;
            crate::format_set(app.clone(), state, format)?;
            Ok(Value::Null)
        }
        "set_policies" => {
            let record = arg::<crate::Policy>(args, "record")?;
            let stream = arg::<crate::Policy>(args, "stream")?;
            let upload = arg::<crate::Policy>(args, "upload")?;
            crate::set_policies(state, record, stream, upload);
            Ok(Value::Null)
        }
        "set_learn_aliases" => {
            crate::set_learn_aliases(arg(args, "on")?, state);
            Ok(Value::Null)
        }
        "library_prune" => jv(crate::library_prune(state, arg(args, "days")?)?),

        // ---- settings: transcription ----
        "transcribe_probe" => jv(crate::transcribe::transcribe_probe(app.clone()).await),
        "transcribe_configure" => {
            let settings: crate::transcribe::Settings = arg(args, "settings")?;
            crate::transcribe::transcribe_configure(app.clone(), state, settings)?;
            Ok(Value::Null)
        }
        "transcribe_models" => jv(crate::transcribe::transcribe_models()),

        // ---- settings: hook / stream / uploads ----
        "hook_get" => jv(crate::hook::hook_get(app.clone(), state)),
        "hook_configure" => {
            let settings: crate::hook::Settings = arg(args, "settings")?;
            crate::hook::hook_configure(app.clone(), state, settings)?;
            Ok(Value::Null)
        }
        "stream_get" => jv(crate::stream::stream_get(app.clone(), state)),
        "stream_configure" => {
            let settings: crate::stream::Settings = arg(args, "settings")?;
            crate::stream::stream_configure(app.clone(), state, settings)?;
            Ok(Value::Null)
        }
        "uploads_get" => jv(crate::upload::uploads_get(app.clone(), state)),
        "uploads_configure" => {
            let settings: crate::upload::Settings = arg(args, "settings")?;
            crate::upload::uploads_configure(app.clone(), state, settings)?;
            Ok(Value::Null)
        }

        // ---- settings: alerts + rules (get/set full objects) ----
        "alerts_set" => {
            let settings: crate::alerts::Settings = arg(args, "settings")?;
            crate::alerts::alerts_set(app.clone(), state, settings)?;
            Ok(Value::Null)
        }
        "conversations_get" => jv(crate::conversations::conversations_get(state)),
        "conversations_set" => {
            let rules: Vec<crate::conversations::Rule> = arg(args, "rules")?;
            crate::conversations::conversations_set(app.clone(), state, rules)?;
            Ok(Value::Null)
        }
        "digests_get" => jv(crate::digest::digests_get(state)),
        "digests_set" => {
            let rules: Vec<crate::digest::DigestRule> = arg(args, "rules")?;
            crate::digest::digests_set(app.clone(), state, rules)?;
            Ok(Value::Null)
        }
        "analyzers_get" => jv(crate::analyzers::analyzers_get(state)),
        "analyzers_set" => {
            let rules: Vec<crate::analyzers::AnalyzerRule> = arg(args, "rules")?;
            crate::analyzers::analyzers_set(app.clone(), state, rules)?;
            Ok(Value::Null)
        }

        other => Err(format!("unknown command: {other}")),
    }
}
