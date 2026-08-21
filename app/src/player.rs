//! Plays completed calls through the default output device, in priority
//! order, with skip and replay-last — the scanner's speaker.
//!
//! One cpal output stream runs for the app's lifetime on its own thread (the
//! stream is not `Send`). Calls are queued as clips; a clip with a better
//! (lower) priority number is inserted ahead of worse ones but never
//! interrupts the clip already playing. The device's own rate is used and
//! the 8 kHz audio is linearly interpolated up to it.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};

/// Roughly 90 s of 8 kHz audio kept for replay.
const HISTORY_SAMPLES: usize = 8000 * 90;

struct Clip {
    pcm: Vec<i16>,
    priority: u8,
}

#[derive(Default)]
struct Queue {
    clips: VecDeque<Clip>,
    /// The clip being played and the read position in it.
    current: Option<(Vec<i16>, usize)>,
    /// Recently played clips, newest last, bounded by `HISTORY_SAMPLES`.
    history: VecDeque<Vec<i16>>,
    history_samples: usize,
}

impl Queue {
    fn push(&mut self, pcm: Vec<i16>, priority: u8) {
        let at = self
            .clips
            .iter()
            .position(|c| c.priority > priority)
            .unwrap_or(self.clips.len());
        self.clips.insert(at, Clip { pcm, priority });
    }

    fn skip(&mut self) {
        self.current = None;
    }

    fn replay_last(&mut self) {
        if let Some(last) = self.history.back().cloned() {
            self.clips.push_front(Clip {
                pcm: last,
                priority: 0,
            });
            self.current = None;
        }
    }

    fn remember(&mut self, pcm: &[i16]) {
        self.history_samples += pcm.len();
        self.history.push_back(pcm.to_vec());
        while self.history_samples > HISTORY_SAMPLES && self.history.len() > 1 {
            if let Some(old) = self.history.pop_front() {
                self.history_samples -= old.len();
            }
        }
    }

    /// Next 8 kHz sample, or silence when idle.
    fn next_sample(&mut self) -> f32 {
        loop {
            if let Some((pcm, pos)) = self.current.as_mut() {
                if *pos < pcm.len() {
                    let v = pcm[*pos] as f32 / 32768.0;
                    *pos += 1;
                    return v;
                }
                self.current = None;
            }
            match self.clips.pop_front() {
                Some(c) => {
                    self.remember(&c.pcm);
                    // A short gap between calls so they don't run together.
                    let mut pcm = c.pcm;
                    pcm.extend(std::iter::repeat_n(0i16, 1600));
                    self.current = Some((pcm, 0));
                }
                None => return 0.0,
            }
        }
    }

    fn queued(&self) -> usize {
        self.clips.len() + usize::from(self.current.is_some())
    }
}

enum Cmd {
    Play(Vec<i16>, u8),
    Skip,
    ReplayLast,
}

/// A handle to the audio thread; cheap to clone.
#[derive(Clone)]
pub struct Audio {
    tx: Sender<Cmd>,
    queue: Arc<Mutex<Queue>>,
}

impl Audio {
    /// Queue a call (8 kHz mono) at `priority` (1 = first, 99 = last).
    pub fn play(&self, pcm: Vec<i16>, priority: u8) {
        let _ = self.tx.send(Cmd::Play(pcm, priority));
    }
    /// Drop whatever is playing and move on.
    pub fn skip(&self) {
        let _ = self.tx.send(Cmd::Skip);
    }
    /// Play the most recently played call again, now.
    pub fn replay_last(&self) {
        let _ = self.tx.send(Cmd::ReplayLast);
    }
    pub fn queued(&self) -> usize {
        self.queue.lock().map(|q| q.queued()).unwrap_or(0)
    }
}

/// Start the app's audio thread. `None` if there is no output device.
pub fn spawn() -> Option<Audio> {
    let (tx, rx) = channel::<Cmd>();
    let (ready_tx, ready_rx) = channel::<Option<Arc<Mutex<Queue>>>>();
    std::thread::spawn(move || {
        let queue: Arc<Mutex<Queue>> = Arc::new(Mutex::new(Queue::default()));
        let Some(_stream) = open_stream(Arc::clone(&queue)) else {
            let _ = ready_tx.send(None);
            return;
        };
        let _ = ready_tx.send(Some(Arc::clone(&queue)));
        while let Ok(cmd) = rx.recv() {
            let mut q = queue.lock().unwrap();
            match cmd {
                Cmd::Play(pcm, pri) => q.push(pcm, pri),
                Cmd::Skip => q.skip(),
                Cmd::ReplayLast => q.replay_last(),
            }
        }
    });
    let queue = ready_rx.recv().ok().flatten()?;
    Some(Audio { tx, queue })
}

fn open_stream(queue: Arc<Mutex<Queue>>) -> Option<cpal::Stream> {
    let host = cpal::default_host();
    let device = host.default_output_device()?;
    let config = device.default_output_config().ok()?;
    let out_rate = config.sample_rate().0 as f32;
    let channels = config.channels() as usize;
    let step = 8000.0 / out_rate;
    let (mut frac, mut prev, mut next) = (0.0f32, 0.0f32, 0.0f32);
    let stream = device
        .build_output_stream(
            &config.config(),
            move |data: &mut [f32], _| {
                let mut q = queue.lock().unwrap();
                for frame in data.chunks_mut(channels) {
                    frac += step;
                    while frac >= 1.0 {
                        frac -= 1.0;
                        prev = next;
                        next = q.next_sample();
                    }
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
    Some(stream)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_round_trips() {
        let pcm: Vec<i16> = (0..16000)
            .map(|i| ((i * 37) % 2000) as i16 - 1000)
            .collect();
        let path = std::env::temp_dir().join("hs_roundtrip.wav");
        hs_core::wav::write_wav(path.to_str().unwrap(), 8000, &pcm).unwrap();
        assert_eq!(read_wav(path.to_str().unwrap()).unwrap(), pcm);
        assert!(read_wav("/etc/hosts").is_err());
    }

    /// Higher priority plays first but never interrupts; skip drops the
    /// current clip; replay-last re-queues what just played.
    #[test]
    fn queue_orders_by_priority_and_supports_skip_and_replay() {
        let mut q = Queue::default();
        q.push(vec![1; 4], 50);
        q.push(vec![2; 4], 50);
        q.push(vec![3; 4], 10);
        // Nothing was playing, so the priority-10 clip goes first.
        assert_eq!(q.next_sample(), 3.0 / 32768.0);
        // Queued behind it, in arrival order: 1 then 2. A later high-priority
        // push goes ahead of them but never displaces the playing clip.
        q.push(vec![4; 4], 10);
        assert_eq!(
            q.clips.iter().map(|c| c.pcm[0]).collect::<Vec<_>>(),
            vec![4, 1, 2]
        );
        q.skip();
        assert_eq!(q.next_sample(), 4.0 / 32768.0);
        q.skip();
        assert_eq!(q.next_sample(), 1.0 / 32768.0);
        q.skip();
        assert_eq!(q.next_sample(), 2.0 / 32768.0);
        q.skip();
        assert_eq!(q.next_sample(), 0.0); // idle
        q.replay_last();
        assert_eq!(q.next_sample(), 2.0 / 32768.0);
        assert_eq!(q.history.len(), 5); // 3, 4, 1, 2, 2(replayed)
                                        // 1, 3, 2, 2(replayed)
    }
}
