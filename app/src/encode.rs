//! Audio formats beyond WAV, via the system `ffmpeg` (with `libmp3lame`,
//! `aac`, `libopus`). WAV stays the capture format; a derived file replaces
//! it when the user chooses another format, and the library's hash is of the
//! stored file — the manifest says which.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Format {
    /// wav | mp3 | m4a | opus
    pub codec: String,
    /// kbit/s for CBR, or the target for VBR modes.
    pub bitrate_kbps: u32,
    /// "cbr" or "vbr".
    pub mode: String,
}

impl Default for Format {
    fn default() -> Self {
        Self {
            codec: "wav".into(),
            bitrate_kbps: 32,
            mode: "vbr".into(),
        }
    }
}

pub fn ffmpeg_available() -> Option<String> {
    static PROBE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    PROBE.get_or_init(ffmpeg_probe).clone()
}

fn ffmpeg_probe() -> Option<String> {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .unwrap_or("ffmpeg")
                .to_string()
        })
}

/// Transcode `wav` to the chosen format next to it; returns the new path.
/// WAV requests return the input unchanged. On any failure the WAV stays and
/// the error is returned.
pub fn transcode(wav: &Path, f: &Format) -> Result<PathBuf, String> {
    if f.codec == "wav" {
        return Ok(wav.to_path_buf());
    }
    let out = wav.with_extension(&f.codec);
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-y", "-loglevel", "error", "-i"]).arg(wav);
    match f.codec.as_str() {
        "mp3" => {
            cmd.args(["-c:a", "libmp3lame"]);
            if f.mode == "cbr" {
                cmd.args(["-b:a", &format!("{}k", f.bitrate_kbps)]);
            } else {
                // LAME VBR quality from a bitrate target: ~32k→7, 64k→5, 128k→2.
                let q = match f.bitrate_kbps {
                    0..=40 => "7",
                    41..=72 => "5",
                    73..=112 => "3",
                    _ => "2",
                };
                cmd.args(["-q:a", q]);
            }
        }
        "m4a" => {
            cmd.args(["-c:a", "aac", "-b:a", &format!("{}k", f.bitrate_kbps)]);
            if f.mode == "vbr" {
                cmd.args(["-q:a", "1"]);
            }
            cmd.args(["-movflags", "+faststart"]);
        }
        "opus" => {
            cmd.args(["-c:a", "libopus", "-b:a", &format!("{}k", f.bitrate_kbps)]);
            cmd.args(["-vbr", if f.mode == "cbr" { "off" } else { "on" }]);
            cmd.args(["-application", "voip"]);
        }
        other => return Err(format!("unknown format {other}")),
    }
    cmd.arg(&out);
    let st = cmd
        .output()
        .map_err(|e| format!("ffmpeg: {e} (install ffmpeg for {})", f.codec))?;
    if !st.status.success() {
        let _ = std::fs::remove_file(&out);
        return Err(format!(
            "ffmpeg: {}",
            String::from_utf8_lossy(&st.stderr).trim()
        ));
    }
    Ok(out)
}

/// Decode any format ffmpeg knows back to 8 kHz mono i16 for the speaker.
pub fn decode_to_pcm(path: &Path) -> Result<Vec<i16>, String> {
    if path.extension().and_then(|e| e.to_str()) == Some("wav") {
        return crate::player::read_wav(path.to_str().ok_or("bad path")?);
    }
    let out = Command::new("ffmpeg")
        .args(["-loglevel", "error", "-i"])
        .arg(path)
        .args(["-f", "s16le", "-ac", "1", "-ar", "8000", "-"])
        .output()
        .map_err(|e| format!("ffmpeg: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "ffmpeg: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out
        .stdout
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With ffmpeg present, a WAV round-trips through every format and comes
    /// back the same length (±50 ms of encoder padding).
    #[test]
    fn transcodes_and_decodes_each_format() {
        if ffmpeg_available().is_none() {
            eprintln!("no ffmpeg; skipping");
            return;
        }
        let pcm: Vec<i16> = (0..16000)
            .map(|i| ((i as f32 * 0.3).sin() * 8000.0) as i16)
            .collect();
        let dir = std::env::temp_dir().join(format!("hs_enc_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("t.wav");
        hs_core::wav::write_wav(wav.to_str().unwrap(), 8000, &pcm).unwrap();
        for (codec, mode) in [
            ("mp3", "vbr"),
            ("mp3", "cbr"),
            ("m4a", "cbr"),
            ("opus", "vbr"),
        ] {
            let f = Format {
                codec: codec.into(),
                bitrate_kbps: 32,
                mode: mode.into(),
            };
            let out = transcode(&wav, &f).unwrap();
            assert!(
                out.exists() && std::fs::metadata(&out).unwrap().len() > 100,
                "{codec}"
            );
            let back = decode_to_pcm(&out).unwrap();
            assert!(
                (back.len() as i64 - pcm.len() as i64).abs() < 800,
                "{codec}: {} vs {}",
                back.len(),
                pcm.len()
            );
        }
        assert_eq!(transcode(&wav, &Format::default()).unwrap(), wav);
    }
}
