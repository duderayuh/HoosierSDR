//! Transcription models on disk: which are downloaded, how much room each
//! takes, and deleting the ones no longer wanted.
//!
//! Each engine keeps its weights in its own cache — faster-whisper and
//! mlx-whisper in the Hugging Face hub cache (one directory per repo),
//! openai-whisper as one `.pt` file per model. The paths are derived from
//! the engine and model name here and nowhere else; a delete request names
//! a model, never a path, so it can only ever remove a whisper model.

use serde::Serialize;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};

use crate::AppState;

/// The sizes the Transcription page offers.
pub const MODELS: &[&str] = &[
    "tiny",
    "base",
    "small",
    "medium",
    "large-v3",
    "distil-large-v3",
    "turbo",
];
pub const ENGINES: &[&str] = &["faster-whisper", "openai-whisper", "mlx-whisper"];

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

fn hub() -> PathBuf {
    // HF_HOME / HF_HUB_CACHE move the cache; honour them as the libraries do.
    if let Ok(p) = std::env::var("HF_HUB_CACHE") {
        return PathBuf::from(p);
    }
    if let Ok(p) = std::env::var("HF_HOME") {
        return PathBuf::from(p).join("hub");
    }
    home().join(".cache/huggingface/hub")
}

/// The Hugging Face repo an engine loads a model size from (mirrors
/// `scripts/transcribe.py` and the faster-whisper defaults).
pub fn hf_repo(engine: &str, model: &str) -> Option<String> {
    match engine {
        "faster-whisper" => Some(if let Some(rest) = model.strip_prefix("distil-") {
            format!("Systran/faster-distil-whisper-{rest}")
        } else if model == "turbo" || model == "large-v3-turbo" {
            "mobiuslabsgmbh/faster-whisper-large-v3-turbo".into()
        } else {
            format!("Systran/faster-whisper-{model}")
        }),
        "mlx-whisper" => Some(if model == "turbo" || model == "large-v3-turbo" {
            "mlx-community/whisper-turbo".into()
        } else if model.starts_with("distil-") {
            format!("mlx-community/{model}-mlx")
        } else {
            format!("mlx-community/whisper-{model}-mlx")
        }),
        _ => None,
    }
}

/// Where a model lives on disk.
pub fn model_path(engine: &str, model: &str) -> Option<PathBuf> {
    if !MODELS.contains(&model) || !ENGINES.contains(&engine) {
        return None;
    }
    match engine {
        // openai-whisper names its turbo file after the full model name.
        "openai-whisper" => Some(home().join(".cache/whisper").join(format!(
            "{}.pt",
            if model == "turbo" { "large-v3-turbo" } else { model }
        ))),
        _ => hf_repo(engine, model).map(|r| hub().join(format!("models--{}", r.replace('/', "--")))),
    }
}

/// Bytes a file or directory takes, counting each file once (the hub cache
/// links snapshot files to its blobs; links are not followed).
pub fn size_of(p: &Path) -> u64 {
    let Ok(m) = std::fs::symlink_metadata(p) else { return 0 };
    if m.is_file() {
        return m.len();
    }
    if !m.is_dir() {
        return 0;
    }
    std::fs::read_dir(p)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| size_of(&e.path()))
        .sum()
}

fn present(engine: &str, p: &Path) -> bool {
    if engine == "openai-whisper" {
        p.is_file()
    } else {
        p.join("snapshots").exists()
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct ModelInfo {
    pub engine: String,
    pub model: String,
    pub downloaded: bool,
    pub path: Option<String>,
    pub bytes: u64,
    /// The model the transcriber is set to use.
    pub in_use: bool,
}

/// Which model files are already on this machine, per engine, with sizes.
#[tauri::command]
pub fn transcribe_models(app: AppHandle) -> Vec<ModelInfo> {
    let (cur_engine, cur_model) = {
        let st = app.state::<AppState>();
        let w = st.transcriber.lock().unwrap();
        (w.settings.engine.clone(), w.settings.model.clone())
    };
    let mut out = Vec::new();
    for m in MODELS {
        for e in ENGINES {
            let Some(p) = model_path(e, m) else { continue };
            let done = present(e, &p);
            out.push(ModelInfo {
                engine: (*e).into(),
                model: (*m).into(),
                downloaded: done,
                path: done.then(|| p.to_string_lossy().into_owned()),
                bytes: if done { size_of(&p) } else { 0 },
                in_use: *e == cur_engine && *m == cur_model,
            });
        }
    }
    out
}

/// Delete one downloaded model. The model the transcriber is set to use
/// is refused while transcription is on — it would only download again on
/// the next call.
#[tauri::command]
pub fn transcribe_delete(app: AppHandle, engine: String, model: String) -> Result<u64, String> {
    let p = model_path(&engine, &model).ok_or("not a known transcription model")?;
    {
        let st = app.state::<AppState>();
        let w = st.transcriber.lock().unwrap();
        if w.settings.enabled && w.settings.engine == engine && w.settings.model == model {
            return Err(format!(
                "{engine} {model} is the model transcription is using — pick another model (or turn transcription off) first"
            ));
        }
    }
    if !present(&engine, &p) {
        return Err("that model is not downloaded".into());
    }
    let freed = size_of(&p);
    let res = if p.is_dir() {
        std::fs::remove_dir_all(&p)
    } else {
        std::fs::remove_file(&p)
    };
    res.map_err(|e| format!("{}: {e}", p.display()))?;
    Ok(freed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_follow_each_engines_cache_layout() {
        let fw = model_path("faster-whisper", "turbo").unwrap();
        assert!(fw.ends_with("models--mobiuslabsgmbh--faster-whisper-large-v3-turbo"), "{fw:?}");
        assert!(model_path("faster-whisper", "base").unwrap().ends_with("models--Systran--faster-whisper-base"));
        assert!(model_path("faster-whisper", "distil-large-v3").unwrap().ends_with("models--Systran--faster-distil-whisper-large-v3"));
        assert!(model_path("mlx-whisper", "turbo").unwrap().ends_with("models--mlx-community--whisper-turbo"));
        assert!(model_path("mlx-whisper", "large-v3").unwrap().ends_with("models--mlx-community--whisper-large-v3-mlx"));
        assert!(model_path("openai-whisper", "turbo").unwrap().ends_with(".cache/whisper/large-v3-turbo.pt"));
        assert!(model_path("openai-whisper", "medium").unwrap().ends_with(".cache/whisper/medium.pt"));
    }

    #[test]
    fn nothing_outside_the_list_can_be_named() {
        assert!(model_path("faster-whisper", "../../etc").is_none());
        assert!(model_path("rm -rf", "base").is_none());
        assert!(model_path("openai-whisper", "tiny/../../x").is_none());
    }

    #[test]
    fn sizes_count_files_once_and_skip_links() {
        let d = std::env::temp_dir().join(format!("hs_models_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("blobs")).unwrap();
        std::fs::create_dir_all(d.join("snapshots/abc")).unwrap();
        std::fs::write(d.join("blobs/x"), vec![0u8; 1000]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(d.join("blobs/x"), d.join("snapshots/abc/model.bin")).unwrap();
        assert_eq!(size_of(&d), 1000);
        let _ = std::fs::remove_dir_all(&d);
    }
}
