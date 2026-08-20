//! Plays completed calls' 8 kHz PCM through the default output device.
//!
//! One cpal output stream runs for the app's lifetime; calls are queued and
//! played back to back. The device's own rate is used and the 8 kHz audio is
//! linearly interpolated up to it — good enough for vocoded speech, and it
//! avoids asking the device for a rate it may not offer.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

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
