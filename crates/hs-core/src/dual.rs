//! Dual-SDR priority trunk-following.
//!
//! Two radios on one site: SDR A locks the control channel and decodes
//! grants; SDR B, a narrow radio, hops between voice channels covering one
//! call at a time. [`DualSdrFollower`] is the orchestration between them —
//! feed each radio's IQ, read back which channel SDR B should tune and the
//! decoded audio. The physical retune is the front end's job: it executes
//! [`ControlEvents::retune`] / [`VoiceEvents::retune`] and confirms with
//! [`DualSdrFollower::retune_done`] so the voice decoder resets.
//!
//! End detection for the call being followed comes from the voice channel
//! itself (its terminator, or a quiet timeout); skipped calls lapse through
//! the scheduler's re-announcement hold timer. P25 Phase I sends no reliable
//! control-channel release message.

use crate::decoder::{ChannelDecoder, EqMode, Modulation};
use crate::hop::{HopAction, HoppingScheduler};
use crate::priority::PriorityMap;
use hs_trunk::Grant;

/// Seconds of silence on the followed voice channel before the call is over
/// (the fallback when its terminator is lost to noise).
const QUIET_SECS: f64 = 2.0;

/// A request to move SDR B.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retune {
    /// Retune to this frequency and decode the call there.
    Tune { freq_hz: u64, talkgroup: u16 },
    /// Nothing to follow — park SDR B (typically back on the control channel).
    Park,
}

/// What processing one SDR-A (control) block produced.
#[derive(Default)]
pub struct ControlEvents {
    /// Resolved grants decoded this block.
    pub grants: Vec<Grant>,
    /// Frame syncs on the control channel (health signal).
    pub control_syncs: u32,
    /// A requested move of SDR B, if the scheduler changed its mind.
    pub retune: Option<Retune>,
}

/// What processing one SDR-B (voice) block produced.
#[derive(Default)]
pub struct VoiceEvents {
    /// PCM samples (8 kHz mono i16) decoded from the followed call.
    pub pcm: Vec<i16>,
    /// Talkgroup of the call being followed, if any.
    pub talkgroup: Option<u16>,
    /// The followed call ended this block (terminator or quiet timeout).
    pub ended: bool,
    /// A requested move of SDR B (promotion after the call ended).
    pub retune: Option<Retune>,
    /// Frame syncs on the voice channel.
    pub syncs: u32,
}

fn eq_for(m: Modulation) -> EqMode {
    match m {
        Modulation::Cqpsk => EqMode::Enabled,
        Modulation::C4fm => EqMode::Bypass,
    }
}

fn map_action(action: HopAction) -> Option<Retune> {
    match action {
        HopAction::Tune { freq_hz, talkgroup } => Some(Retune::Tune { freq_hz, talkgroup }),
        HopAction::Park => Some(Retune::Park),
        HopAction::Stay => None,
    }
}

pub struct DualSdrFollower {
    /// Control-channel decoder (SDR A).
    control: ChannelDecoder,
    control_rate: f64,
    /// The hopped voice channel (SDR B), present while a call is followed.
    voice: Option<ChannelDecoder>,
    voice_rate: f64,
    voice_modulation: Modulation,
    /// The frequency SDR B is asked to be tuned to (None = parked).
    voice_freq: Option<u64>,
    /// Seconds the voice channel has produced no frame sync.
    voice_quiet: f64,
    scheduler: HoppingScheduler,
    /// Monotonic clock shared with the scheduler (seconds).
    elapsed_secs: f64,
}

impl DualSdrFollower {
    /// `control` is a pre-built control-channel decoder (modulation + rate
    /// chosen by the front end). `control_rate`/`voice_rate` are the two
    /// radios' capture rates, used to advance the clock.
    pub fn new(
        control: ChannelDecoder,
        control_rate: f64,
        priority: PriorityMap,
        voice_rate: f64,
    ) -> Self {
        let voice_modulation = control.modulation();
        Self {
            control,
            control_rate,
            voice: None,
            voice_rate,
            voice_modulation,
            voice_freq: None,
            voice_quiet: 0.0,
            scheduler: HoppingScheduler::new(priority),
            elapsed_secs: 0.0,
        }
    }

    /// Feed a block of SDR-A (control) IQ. Returns decoded grants plus any
    /// retune the scheduler now wants.
    pub fn process_control(&mut self, iq: &[f32]) -> ControlEvents {
        self.elapsed_secs += (iq.len() / 2) as f64 / self.control_rate;
        let out = self.control.process(iq);
        let retune = self.route_grants(&out.grants);
        ControlEvents {
            grants: out.grants,
            control_syncs: out.syncs,
            retune,
        }
    }

    fn route_grants(&mut self, grants: &[Grant]) -> Option<Retune> {
        let mut retune = None;
        for g in grants {
            if g.encrypted {
                continue;
            }
            retune = map_action(
                self.scheduler
                    .on_grant(g.talkgroup, g.freq_hz, self.elapsed_secs),
            )
            .or(retune);
        }
        retune
    }

    /// Feed a block of SDR-B (voice) IQ. Returns decoded audio, and — if the
    /// followed call ended — a retune to the next-highest-priority call.
    pub fn process_voice(&mut self, iq: &[f32]) -> VoiceEvents {
        self.elapsed_secs += (iq.len() / 2) as f64 / self.voice_rate;

        let Some(voice) = self.voice.as_mut() else {
            // Parked: nothing to decode.
            return VoiceEvents::default();
        };

        let out = voice.process(iq);
        if out.syncs > 0 {
            self.voice_quiet = 0.0;
        } else {
            self.voice_quiet += (iq.len() / 2) as f64 / self.voice_rate;
        }

        let ended = out.terminators > 0 || self.voice_quiet >= QUIET_SECS;
        let retune = if ended {
            self.voice_freq
                .map(|freq| map_action(self.scheduler.on_end(freq, self.elapsed_secs)))
                .flatten()
        } else {
            None
        };

        VoiceEvents {
            pcm: out.pcm,
            talkgroup: self.scheduler.current().map(|c| c.talkgroup),
            ended,
            retune,
            syncs: out.syncs,
        }
    }

    /// The front end executed a retune: tell the follower where SDR B now
    /// sits (`None` = parked), resetting the voice decoder for the new channel.
    pub fn retune_done(&mut self, freq: Option<u64>) {
        self.voice_freq = freq;
        self.voice_quiet = 0.0;
        self.voice = freq.map(|_| {
            ChannelDecoder::with_offset(
                self.voice_rate,
                self.voice_modulation,
                eq_for(self.voice_modulation),
                0.0,
            )
        });
    }

    /// The talkgroup SDR B is following, if any.
    pub fn voice_talkgroup(&self) -> Option<u16> {
        self.scheduler.current().map(|c| c.talkgroup)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(overrides: &[(u16, u8)]) -> PriorityMap {
        let mut m = PriorityMap::new();
        for (tg, p) in overrides {
            m.set_base(*tg, *p);
        }
        m
    }

    #[test]
    fn maps_actions_to_retunes() {
        assert_eq!(
            map_action(HopAction::Tune {
                freq_hz: 851_000_000,
                talkgroup: 7
            }),
            Some(Retune::Tune {
                freq_hz: 851_000_000,
                talkgroup: 7
            })
        );
        assert_eq!(map_action(HopAction::Park), Some(Retune::Park));
        assert_eq!(map_action(HopAction::Stay), None);
    }

    #[test]
    fn parked_voice_returns_nothing() {
        let control = ChannelDecoder::new(240_000.0, EqMode::Bypass);
        let mut f = DualSdrFollower::new(control, 240_000.0, map(&[]), 240_000.0);
        let ev = f.process_voice(&[0.0f32; 4800]);
        assert!(ev.pcm.is_empty());
        assert!(!ev.ended);
        assert_eq!(ev.retune, None);
        assert_eq!(ev.syncs, 0);
    }

    #[test]
    fn retune_done_builds_a_voice_decoder_and_park_clears_it() {
        let control = ChannelDecoder::new(240_000.0, EqMode::Bypass);
        let mut f = DualSdrFollower::new(control, 240_000.0, map(&[]), 240_000.0);
        f.retune_done(Some(851_000_000));
        assert_eq!(f.voice_freq, Some(851_000_000));
        assert!(f.voice.is_some());
        f.retune_done(None);
        assert_eq!(f.voice_freq, None);
        assert!(f.voice.is_none());
    }
}
