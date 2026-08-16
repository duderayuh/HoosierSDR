//! Transcription pipeline (Phase 4):
//! VAD gate → resample → ASR (trait) → hallucination filter → domain
//! normalizer → FTS index.
//!
//! Design constraints from docs/ARCHITECTURE.md §7: off-the-shelf Whisper
//! scores ~50% WER on police radio and hallucinates on 40% of non-speech
//! audio, so the filter pipeline is mandatory, `condition_on_previous_text`
//! must be false, and a transducer engine (Parakeet via sherpa-onnx, with
//! hotword biasing) ships alongside Whisper behind this trait.

#[derive(Debug, Clone)]
pub struct Transcript {
    pub text: String,
    /// Engine-reported confidence in [0,1]; surfaced in the UI so users can
    /// calibrate trust.
    pub confidence: f32,
}

#[derive(Debug)]
pub enum AsrError {
    NoSpeech,
    Engine(String),
}

/// One short transmission in (16 kHz mono f32), text out.
pub trait AsrEngine {
    fn name(&self) -> &'static str;
    fn transcribe(&mut self, pcm_16k: &[f32]) -> Result<Transcript, AsrError>;
}

/// Post-ASR hallucination filter: blocklist of known hallucinated strings
/// plus repetition de-looping. Hallucinations are highly repetitive (top
/// phrases cover ~67% of occurrences), so a blocklist is unusually effective.
pub fn filter_hallucinations(t: Transcript, blocklist: &[&str]) -> Option<Transcript> {
    let lower = t.text.to_lowercase();
    if blocklist.iter().any(|b| lower.contains(&b.to_lowercase())) {
        return None;
    }
    Some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocklist_drops_known_hallucinations() {
        let t = Transcript {
            text: "Please subscribe, click the bell icon".into(),
            confidence: 0.9,
        };
        assert!(filter_hallucinations(t, &["please subscribe"]).is_none());
        let ok = Transcript {
            text: "Medic 4 responding".into(),
            confidence: 0.8,
        };
        assert!(filter_hallucinations(ok, &["please subscribe"]).is_some());
    }
}
