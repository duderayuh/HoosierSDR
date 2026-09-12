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
/// One argument by its Rust (snake_case) name. The desktop page sends the
/// camelCase names Tauri's IPC would convert (`callsDir`), so that spelling
/// is accepted too.
fn arg<T: DeserializeOwned>(args: &Value, key: &str) -> Result<T, String> {
    let v = args
        .get(key)
        .or_else(|| args.get(camel(key)))
        .cloned()
        .unwrap_or(Value::Null);
    serde_json::from_value(v).map_err(|e| format!("{key}: {e}"))
}

fn camel(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    let mut up = false;
    for c in key.chars() {
        if c == '_' {
            up = true;
        } else if up {
            out.extend(c.to_uppercase());
            up = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// Every top-level key of `args` in snake_case, so a struct of arguments
/// deserialises whichever spelling the client used.
fn snake_args(args: &Value) -> Value {
    match args.as_object() {
        Some(m) => Value::Object(
            m.iter()
                .map(|(k, v)| (snake(k), v.clone()))
                .collect(),
        ),
        None => args.clone(),
    }
}

fn snake(key: &str) -> String {
    let mut out = String::with_capacity(key.len() + 4);
    for c in key.chars() {
        if c.is_ascii_uppercase() {
            out.push('_');
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
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
    #[serde(default)]
    playlist: Option<String>,
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
        "conversations_state" => jv(crate::conversations::conversations_state(state)),
        "conversations_list" => jv(crate::conversations::conversations_list(
            state,
            arg(args, "q")?,
            arg(args, "tg")?,
            arg(args, "before")?,
            arg(args, "limit")?,
        )?),
        "conversation_get" => jv(crate::conversations::conversation_get(
            state,
            arg(args, "id")?,
        )?),
        "conversation_delete" => {
            crate::conversations::conversation_delete(state, arg(args, "id")?)?;
            Ok(Value::Null)
        }
        "conversations_stats" => jv(crate::conversations::conversations_stats(state)?),
        "dispatch_get" => jv(crate::dispatch::dispatch_get(state)),
        "dispatch_log" => jv(crate::dispatch::dispatch_log(state)),
        "incidents_list" => jv(crate::dispatch::incidents_list(
            state,
            arg::<Option<i64>>(args, "since")?.unwrap_or(0),
            arg(args, "limit")?,
        )?),
        "incident_get" => jv(crate::dispatch::incident_get(state, arg(args, "id")?)?),
        "events_list" => jv(crate::events::events_list(
            state,
            arg::<Option<crate::events::EventQuery>>(args, "query")?.unwrap_or_default(),
        )?),
        "channel_activity" => jv(crate::channels::channel_activity(state, arg(args, "hours")?)?),
        "retention_get" => jv(crate::retention::retention_get(state)),
        "retention_set" => jv(crate::retention::retention_set(app.clone(), state, arg(args, "settings")?)?),
        "retention_migrate" => jv(crate::retention::retention_migrate(app.clone(), state, arg(args, "days")?)?),
        "retention_preview" => jv(crate::retention::retention_preview(app.clone(), arg(args, "settings")?).await?),
        "retention_apply" => jv(crate::retention::retention_apply(app.clone(), state).await?),
        "retention_usage" => jv(crate::retention::retention_usage(app.clone()).await?),
        "channel_sets_get" => jv(crate::channels::channel_sets_get(app.clone())),
        "channel_sets_set" => jv(crate::channels::channel_sets_set(app.clone(), arg(args, "settings")?)?),
        "events_stats" => jv(crate::events::events_stats(
            state,
            arg::<Option<i64>>(args, "since")?.unwrap_or(0),
        )?),
        "units_list" => jv(crate::units::units_list(state)),
        "rr_settings" => jv(crate::rr::rr_settings(app.clone(), state)),

        // ---- control ----
        "start_follow" => {
            let a: StartArgs = serde_json::from_value(snake_args(args)).map_err(|e| e.to_string())?;
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
                a.playlist,
            )?;
            Ok(Value::Null)
        }
        "stop_capture" => {
            crate::stop_capture(state);
            Ok(Value::Null)
        }
        "set_hold" => {
            crate::playlists::set_hold(app.clone(), state, arg::<Option<u16>>(args, "tg")?, arg(args, "playlist")?);
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
            crate::playlists::set_allowlist(app.clone(), state, arg::<Option<Vec<u16>>>(args, "tgs")?, arg(args, "playlist")?);
            Ok(Value::Null)
        }
        "set_lockout" => {
            crate::playlists::set_lockout(app.clone(), state, arg::<Vec<u16>>(args, "tgs")?, arg(args, "playlist")?, arg(args, "extra")?)?;
            Ok(Value::Null)
        }
        "set_priorities" => {
            crate::playlists::set_priorities(app.clone(), state, arg::<Vec<(u16, u8)>>(args, "entries")?, arg(args, "playlist")?)?;
            Ok(Value::Null)
        }
        "set_lockout_ranges" => {
            crate::playlists::set_lockout_ranges(app.clone(), state, arg::<Vec<(u16, u16)>>(args, "ranges")?);
            Ok(Value::Null)
        }
        "set_priority_ranges" => {
            crate::playlists::set_priority_ranges(app.clone(), state, arg::<Vec<(u16, u16, u8)>>(args, "ranges")?);
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
            jv(crate::library_search(app.clone(), state, q)?)
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
        "library_get" => jv(crate::library_get(app.clone(), state, arg(args, "id")?)?),
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
        "transcribe_models" => jv(crate::models::transcribe_models(app.clone())),
        "transcribe_delete" => jv(crate::models::transcribe_delete(app.clone(), arg(args, "engine")?, arg(args, "model")?)?),

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
        "remotes_get" => jv(crate::remotes::remotes_get(app.clone()).await),
        "remotes_set" => {
            let settings: crate::remotes::Settings = arg(args, "settings")?;
            crate::remotes::remotes_set(app.clone(), settings)?;
            Ok(Value::Null)
        }
        "remotes_scan" => jv(crate::remotes::remotes_scan(app.clone()).await?),
        "conversations_get" => jv(crate::conversations::conversations_get(state)),
        "tripwires_get" => jv(crate::tripwires::tripwires_get(state)),
        "tripwires_set" => jv(crate::tripwires::tripwires_set(app.clone(), state, arg(args, "tripwires")?)?),
        "tripwire_recipes" => jv(crate::tripwires::tripwire_recipes()),
        "tripwire_test" => jv(crate::tripwires::tripwire_test(app.clone(), arg(args, "id")?).await?),
        "tripwire_preview" => jv(crate::backtest::tripwire_preview(app.clone(), arg(args, "tripwire")?, arg(args, "days")?).await?),
        "tripwire_try" => jv(crate::backtest::tripwire_try(app.clone(), arg(args, "tripwire")?, arg(args, "ids")?).await?),
        "tripwire_draft" => jv(crate::backtest::tripwire_draft(app.clone(), arg(args, "id")?)?),
        "tripwires_import" => jv(crate::tripwires::tripwires_import(arg(args, "text")?)?),
        "tripwires_export" => jv(crate::tripwires::tripwires_export(
            state,
            arg::<Option<Vec<String>>>(args, "ids")?.unwrap_or_default(),
            arg::<Option<String>>(args, "name")?.unwrap_or_default(),
            arg::<Option<String>>(args, "author")?.unwrap_or_default(),
            arg::<Option<String>>(args, "description")?.unwrap_or_default(),
        )?),
        "ollama_capabilities" => {
            jv(crate::alerts::ollama_capabilities(arg(args, "url")?, arg(args, "model")?).await?)
        }

        // ---- the rest of the desktop's commands, so the remote desktop page
        // can do everything the local one can ----
        "telegram_save" => jv(crate::alerts::telegram_save(arg(args, "token")?)?),
        "telegram_verify" => jv(crate::connections::telegram_verify().await?),
        "telegram_discover" => jv(crate::connections::telegram_discover(app.clone()).await?),
        "telegram_test_destination" => jv(crate::connections::telegram_test_destination(arg(args, "destination")?).await?),
        "ollama_models" => jv(crate::alerts::ollama_models(arg(args, "url")?).await?),
        "analyzer_cloud_get" => jv(crate::analyzers::analyzer_cloud_get(state)),
        "analyzer_cloud_save" => jv(crate::analyzers::analyzer_cloud_save(
            app.clone(),
            state,
            arg(args, "cloud")?,
            arg(args, "key")?,
        )?),
        "analyzer_cloud_clear_key" => jv(crate::analyzers::analyzer_cloud_clear_key()?),
        "conversation_test" => {
            jv(crate::conversations::conversation_test(app.clone(), arg(args, "id")?).await?)
        }
        "conversation_resend" => jv(crate::conversations::conversation_resend(
            app.clone(),
            state,
            arg(args, "key")?,
        )?),
        "devices_set" => jv(crate::devices::devices_set(
            app.clone(),
            arg(args, "id")?,
            arg(args, "settings")?,
        )?),
        "dispatch_set" => jv(crate::dispatch::dispatch_set(app.clone(), state, arg(args, "settings")?)?),
        "incident_delete" => jv(crate::dispatch::incident_delete(app.clone(), state, arg(args, "id")?)?),
        "incident_locate" => jv(crate::dispatch::incident_locate(
            app.clone(),
            state,
            arg(args, "id")?,
            arg(args, "address")?,
            arg(args, "lat")?,
            arg(args, "lon")?,
        )
        .await?),
        "dispatch_regeocode" => jv(crate::dispatch::dispatch_regeocode(app.clone(), state).await?),
        "dispatch_calibrate" => jv(crate::dispatch::dispatch_calibrate(app.clone(), state)?),
        "incidents_relink" => jv(crate::link::incidents_relink(app.clone(), state)?),
        "routing_get" => jv(crate::routing::routing_get(state)),
        "routing_set" => jv(crate::routing::routing_set(app.clone(), state, arg(args, "settings")?)?),
        "routing_status" => jv(crate::routing::routing_status(app.clone(), state)),
        "mapdata_prepare" => jv(crate::routing::mapdata_prepare(app.clone(), state, arg(args, "region")?)?),
        "mapdata_start" => jv(crate::routing::mapdata_start(app.clone(), state)?),
        "mapdata_stop" => jv(crate::routing::mapdata_stop()?),
        "mapdata_region" => jv(crate::routing::mapdata_region(state)?),
        "places_get" => jv(crate::places::places_get(state)),
        "places_set" => jv(crate::places::places_set(app.clone(), state, arg(args, "settings")?)?),
        "places_suggest" => jv(crate::places::places_suggest(app.clone(), arg(args, "sid")?)),
        "place_features" => jv(crate::places::place_features()),
        "place_locate" => jv(crate::places::place_locate(state, arg(args, "query")?).await?),
        "dispatch_geocode" => jv(crate::dispatch::dispatch_geocode(state, arg(args, "q")?).await?),
        "dispatch_test" => jv(crate::dispatch::dispatch_test(app.clone(), state, arg(args, "tg")?).await?),
        "dispatch_backfill" => jv(crate::dispatch::dispatch_backfill(app.clone(), state, arg(args, "hours")?)?),
        "dual_start" => {
            let a: DualArgs = serde_json::from_value(snake_args(args)).map_err(|e| e.to_string())?;
            crate::dual::dual_start(
                app.clone(),
                state,
                a.control_source,
                a.control_device,
                a.control_rate,
                a.voice_source,
                a.voice_device,
                a.voice_rate,
                a.gain,
                a.control,
                a.cqpsk,
                a.play,
            )?;
            Ok(Value::Null)
        }
        "hook_test" => jv(crate::hook::hook_test(arg(args, "settings")?).await?),
        "corrections_get" => jv(crate::corrections_get(state)),
        "corrections_set" => jv(crate::corrections_set(app.clone(), state, arg(args, "entries")?)),
        "ui_log" => jv(crate::ui_log(arg(args, "msg")?)),
        "library_reencode" => jv(crate::library_reencode(app.clone(), state)?),
        "library_set_edited" => jv(crate::library_set_edited(state, arg(args, "id")?, arg(args, "text")?)?),
        "library_export" => jv(crate::library_export(
            app.clone(),
            state,
            arg(args, "ids")?,
            arg(args, "dest")?,
        )?),
        "play_wav" => jv(crate::play_wav(app.clone(), arg(args, "path")?).await?),
        "load_catalog" => jv(crate::load_catalog(app.clone(), arg(args, "path")?, state)?),
        "start_capture" => {
            let a: CaptureArgs = serde_json::from_value(snake_args(args)).map_err(|e| e.to_string())?;
            crate::start_capture(
                app.clone(),
                state,
                a.source,
                a.freq,
                a.rate,
                a.gain,
                a.cqpsk,
                a.eq,
                a.record_iq,
                a.record_log,
                a.ppm,
                a.device,
            )?;
            Ok(Value::Null)
        }
        "survey_capture" => jv(crate::survey_capture(app.clone(), state, arg(args, "spec")?)?),
        "survey_delete" => jv(crate::survey_delete(arg(args, "spec")?)?),
        "decode_file" => jv(crate::decode_file(
            app.clone(),
            arg(args, "path")?,
            arg(args, "rate")?,
            arg(args, "cqpsk")?,
            arg(args, "eq")?,
        )
        .await?),
        "decode_file_analog" => jv(crate::decode_file_analog(
            app.clone(),
            arg(args, "path")?,
            arg(args, "rate")?,
            arg(args, "decoder")?,
            arg(args, "squelch")?,
        )
        .await?),
        "playlist_save" => jv(crate::playlists::playlist_save(app.clone(), state, arg(args, "playlist")?)?),
        "playlist_delete" => jv(crate::playlists::playlist_delete(app.clone(), state, arg(args, "id")?)?),
        "sites_list" => jv(crate::playlists::sites_list(app.clone())),
        "site_save" => jv(crate::playlists::site_save(app.clone(), arg(args, "site")?)?),
        "site_delete" => jv(crate::playlists::site_delete(app.clone(), arg(args, "id")?)?),
        "remote_token_set" => jv(crate::remotes::remote_token_set(
            arg(args, "dns")?,
            arg(args, "token")?,
        )?),
        // A remote page navigates to the other instance itself (see shim.js);
        // opening a window here would put it on the far machine's screen.
        "remote_open" => Err("open remote instances from the desktop".into()),
        "catalogs_list" => jv(crate::rr::catalogs_list(app.clone())),
        "catalog_lookup" => jv(crate::rr::catalog_lookup(app.clone(), arg(args, "tg")?)),
        "catalog_remove" => jv(crate::rr::catalog_remove(app.clone(), state, arg(args, "name")?)?),
        "catalog_user_set" => jv(crate::rr::catalog_user_set(
            app.clone(),
            state,
            arg(args, "tg")?,
            arg(args, "alias")?,
            arg(args, "category")?,
            arg(args, "sid")?,
        )?),
        "save_text" => jv(crate::rr::save_text(arg(args, "path")?, arg(args, "text")?)?),
        "rr_save" => jv(crate::rr::rr_save(
            app.clone(),
            arg(args, "username")?,
            arg(args, "password")?,
            arg(args, "sid")?,
        )?),
        "rr_download" => jv(crate::rr::rr_download(app.clone(), arg(args, "sid")?).await?),
        "rr_states" => jv(crate::rr::rr_states(app.clone(), arg(args, "refresh")?).await?),
        "rr_state" => jv(crate::rr::rr_state(app.clone(), arg(args, "stid")?, arg(args, "refresh")?).await?),
        "rr_county" => jv(crate::rr::rr_county(app.clone(), arg(args, "ctid")?, arg(args, "refresh")?).await?),
        "rr_zip" => jv(crate::rr::rr_zip(app.clone(), arg(args, "zip")?).await?),
        "transcribe_download" => jv(crate::transcribe::transcribe_download(
            app.clone(),
            arg(args, "engine")?,
            arg(args, "model")?,
        )?),
        "transcribe_call" => jv(crate::transcribe::transcribe_call(app.clone(), state, arg(args, "id")?)?),
        "unit_rules_list" => jv(crate::units::unit_rules_list(state)),
        "unit_rules_set" => jv(crate::units::unit_rules_set(app.clone(), state, arg(args, "rules")?)?),
        "unit_resolve" => jv(crate::units::unit_resolve(state, arg(args, "id")?, arg(args, "sid")?)),
        "unit_set" => jv(crate::units::unit_set(app.clone(), state, arg(args, "id")?, arg(args, "name")?, arg(args, "sid")?)?),
        "units_import" => jv(crate::units::units_import(app.clone(), state, arg(args, "path")?, arg(args, "sid")?)?),
        "uploads_test" => jv(crate::upload::uploads_test(arg(args, "service")?, arg(args, "settings")?).await?),
        "upload_call" => jv(crate::upload::upload_call(app.clone(), state, arg(args, "id")?)?),
        "web_access_get" => jv(crate::web::web_access_get(app.clone())),
        "runs_list" => jv(crate::runs_list(state)),
        "stop_run" => jv(crate::stop_run(arg(args, "id")?, state)?),

        other => Err(format!("unknown command: {other}")),
    }
}

/// Arguments for `dual_start`.
#[derive(Deserialize)]
struct DualArgs {
    control_source: String,
    #[serde(default)]
    control_device: Option<String>,
    control_rate: f64,
    voice_source: String,
    #[serde(default)]
    voice_device: Option<String>,
    voice_rate: f64,
    #[serde(default)]
    gain: Option<f64>,
    control: f64,
    #[serde(default)]
    cqpsk: bool,
    #[serde(default)]
    play: bool,
}

/// Arguments for `start_capture`.
#[derive(Deserialize)]
struct CaptureArgs {
    source: String,
    freq: f64,
    rate: f64,
    #[serde(default)]
    gain: Option<f64>,
    #[serde(default)]
    cqpsk: bool,
    #[serde(default)]
    eq: String,
    #[serde(default)]
    record_iq: Option<String>,
    #[serde(default)]
    record_log: Option<String>,
    #[serde(default)]
    ppm: Option<f64>,
    #[serde(default)]
    device: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every command the desktop registers has an arm here, so the remote
    /// desktop page can do everything the local one can.
    #[test]
    fn every_desktop_command_is_mirrored() {
        let main = include_str!("../main.rs");
        let start = main.find("generate_handler![").expect("generate_handler");
        let end = main[start..].find("])").expect("end of handler list") + start;
        let registered: Vec<&str> = main[start + "generate_handler![".len()..end]
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.rsplit("::").next().unwrap())
            .collect();
        assert!(registered.len() > 100, "parsed {} commands", registered.len());
        let me = include_str!("api.rs");
        let missing: Vec<&str> = registered
            .iter()
            .copied()
            .filter(|c| !me.contains(&format!("\"{c}\"")))
            .collect();
        assert!(missing.is_empty(), "commands without a web mirror: {missing:?}");
    }

    #[test]
    fn argument_names_accept_both_spellings() {
        let a = serde_json::json!({ "hangMs": 250, "calls_dir": "x" });
        assert_eq!(arg::<u32>(&a, "hang_ms").unwrap(), 250);
        assert_eq!(arg::<String>(&a, "calls_dir").unwrap(), "x");
        assert_eq!(snake_args(&a)["hang_ms"], 250);
        assert_eq!(camel("system_name"), "systemName");
        assert_eq!(snake("systemName"), "system_name");
    }
}
