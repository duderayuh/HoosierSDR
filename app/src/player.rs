//! Plays completed calls' 8 kHz PCM through the default output device.
//!
//! One cpal output stream runs for the app's lifetime; calls are queued and
//! played back to back. The device's own rate is used and the 8 kHz audio is
//! linearly interpolated up to it — good enough for vocoded speech, and it
//! avoids asking the device for a rate it may not offer.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};

/// Start the app's audio thread: it owns the (non-`Send`) output stream and
/// plays whatever 8 kHz PCM is sent to it, in order. `None` if there is no
/// output device.
pub fn spawn() -> Option<Sender<Vec<i16>>> {
    let (tx, rx) = channel::<Vec<i16>>();
    let (ready_tx, ready_rx) = channel::<bool>();
    std::thread::spawn(move || {
        let Some(mut player) = Player::open() else {
            let _ = ready_tx.send(false);
            return;
        };
        let _ = ready_tx.send(true);
        while let Ok(pcm) = rx.recv() {
            player.play(&pcm);
        }
    });
    ready_rx.recv().ok().filter(|ok| *ok).map(|_| tx)
}

/// Read a 16-bit mono PCM WAV (as `hs_core::wav::write_wav` writes) back
/// into samples. Other layouts are rejected rather than misplayed.
pub fn read_wav(path: &str) -> Result<Vec<i16>, String> {
    let b = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return Err(format!("{path}: not a WAV file"));
    }
    let (mut pos, mut fmt_ok) = (12usize, false);
    while pos + 8 <= b.len() {
        let id = &b[pos..pos + 4];
        let len = u32::from_le_bytes([b[pos + 4], b[pos + 5], b[pos + 6], b[pos + 7]]) as usize;
        let body = &b[pos + 8..(pos + 8 + len).min(b.len())];
        if id == b"fmt " && body.len() >= 16 {
            let format = u16::from_le_bytes([body[0], body[1]]);
            let channels = u16::from_le_bytes([body[2], body[3]]);
            let bits = u16::from_le_bytes([body[14], body[15]]);
            fmt_ok = format == 1 && channels == 1 && bits == 16;
        } else if id == b"data" {
            if !fmt_ok {
                return Err(format!("{path}: only 16-bit mono PCM is supported"));
            }
            return Ok(body
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]))
                .collect());
        }
        pos += 8 + len + (len & 1);
    }
    Err(format!("{path}: no audio data"))
}

pub struct Player {
    queue: Arc<Mutex<VecDeque<f32>>>,
    _stream: cpal::Stream,
}

impl Player {
    /// `None` if there is no output device or it refuses a stream.
    pub fn open() -> Option<Self> {
        let host = cpal::default_host();
        let device = host.default_output_device()?;
        let config = device.default_output_config().ok()?;
        let out_rate = config.sample_rate().0 as f32;
        let channels = config.channels() as usize;
        let queue: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::new()));
        let q = Arc::clone(&queue);
        // Fractional position into the 8 kHz queue, for interpolation.
        let step = 8000.0 / out_rate;
        let mut frac = 0.0f32;
        let mut prev = 0.0f32;
        let stream = device
            .build_output_stream(
                &config.config(),
                move |data: &mut [f32], _| {
                    let mut q = q.lock().unwrap();
                    for frame in data.chunks_mut(channels) {
                        frac += step;
                        while frac >= 1.0 {
                            frac -= 1.0;
                            prev = q.pop_front().unwrap_or(0.0);
                        }
                        let next = q.front().copied().unwrap_or(0.0);
                        let v = prev + (next - prev) * frac;
                        for s in frame.iter_mut() {
                            *s = v;
                        }
                    }
                },
                |e| eprintln!("audio output error: {e}"),
                None,
            )
            .ok()?;
        stream.play().ok()?;
        Some(Self {
            queue,
            _stream: stream,
        })
    }

    /// Queue a call's audio (8 kHz mono i16) after whatever is playing.
    pub fn play(&mut self, pcm: &[i16]) {
        let mut q = self.queue.lock().unwrap();
        q.extend(pcm.iter().map(|&s| s as f32 / 32768.0));
        // A short gap between calls so they don't run together.
        q.extend(std::iter::repeat_n(0.0f32, 1600));
    }
}

#[cfg(test)]
mod tests {
    /// What `write_wav` writes, `read_wav` reads back sample-for-sample —
    /// including a real call from the soak, when one is on this machine.
    #[test]
    fn wav_round_trips() {
        let pcm: Vec<i16> = (0..16000).map(|i| ((i * 37) % 2000) as i16 - 1000).collect();
        let path = std::env::temp_dir().join("hs_roundtrip.wav");
        hs_core::wav::write_wav(path.to_str().unwrap(), 8000, &pcm).unwrap();
        assert_eq!(super::read_wav(path.to_str().unwrap()).unwrap(), pcm);

        let soak = std::env::var("HOME").unwrap()
            + "/hoosier-field/soak-2026-08-20/call_001_tg20309.wav";
        if let Ok(s) = super::read_wav(&soak) {
            assert!(!s.is_empty());
        }
        assert!(super::read_wav("/etc/hosts").is_err());
    }
}
