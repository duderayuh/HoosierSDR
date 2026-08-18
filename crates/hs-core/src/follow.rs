//! Trunk following: watch a control channel, and decode the calls it grants.
//!
//! Everything else in this crate decodes *a* channel. This assembles those
//! pieces into a scanner. A trunked system announces every call on its control
//! channel and then carries the audio somewhere else, so listening means
//! tracking those announcements and decoding several frequencies at once —
//! which is what the [`Channelizer`] makes possible from a single radio. The
//! receiver never retunes, so no call is missed while it moves.
//!
//! ## Two things real captures forced into this design
//!
//! **Modulation is per channel, not per system.** On the observed system the
//! control channel is CQPSK while the traffic channel carrying its calls is
//! C4FM. A follower that assumed one modulation would hear the announcements
//! and none of the audio, so each call is decoded both ways and whichever
//! produces frame syncs is believed.
//!
//! **Tuner error is larger than the receiver's tolerance.** The demodulators
//! hold lock within roughly ±1 kHz, and an uncalibrated dongle can sit 6 kHz
//! off — enough that tuning a granted frequency by its nominal value finds
//! nothing. Rather than ask for a ppm figure, the follower takes the control
//! channel's *measured* frequency alongside its nominal one and applies the
//! difference to every channel it tunes afterwards. The control channel has to
//! be found before anything can be followed anyway, so the correction is free.

use crate::decoder::{ChannelDecoder, EqMode, Modulation};
use hs_dsp::channelizer::Channelizer;

/// A call in progress on a traffic channel.
struct ActiveCall {
    freq_hz: u64,
    talkgroup: u16,
    source_unit: u32,
    /// Both modulations, because only the channel knows which it uses.
    c4fm: ChannelDecoder,
    cqpsk: ChannelDecoder,
    /// Audio from each modulation, kept separately so the choice between them
    /// can be made on the thing that matters rather than guessed early.
    pcm_c4fm: Vec<i16>,
    pcm_cqpsk: Vec<i16>,
    syncs_c4fm: u32,
    syncs_cqpsk: u32,
    /// Blocks seen with no frame sync, used to retire a finished call.
    quiet: u32,
}

/// A call the follower has finished with.
#[derive(Debug, Clone)]
pub struct Call {
    pub talkgroup: u16,
    pub source_unit: u32,
    pub freq_hz: u64,
    /// Modulation that actually decoded, once known.
    pub modulation: Option<Modulation>,
    /// Frame syncs each modulation achieved, the evidence for that choice.
    pub syncs_c4fm: u32,
    pub syncs_cqpsk: u32,
    /// Talkgroups patched to this one; audio may be shared with them.
    pub patched_with: Vec<u16>,
    /// 8 kHz mono audio.
    pub pcm: Vec<i16>,
}

/// What one processed block produced.
#[derive(Default)]
pub struct FollowOutput {
    /// Calls that began this block.
    pub started: Vec<(u16, u64)>,
    /// Calls that finished, with their audio.
    pub completed: Vec<Call>,
    /// Frame syncs on the control channel, a health signal.
    pub control_syncs: u32,
}

pub struct TrunkFollower {
    chan: Channelizer,
    control: ChannelDecoder,
    active: Vec<ActiveCall>,
    center_hz: f64,
    /// Added to every nominal frequency to find where it really is.
    correction_hz: f64,
    /// Blocks without a sync before a call is considered over (~1 s).
    quiet_limit: u32,
    /// Most calls the channelizer will follow at once.
    max_calls: usize,
}

impl TrunkFollower {
    /// Follow the system whose control channel is at `control_nominal_hz`.
    ///
    /// `control_measured_hz` is where that channel actually appears in the
    /// capture — from `scan`, or from a spectrum peak. The difference between
    /// the two is the tuner's error, and it is applied to every frequency the
    /// control channel later names.
    pub fn new(
        sample_rate: f64,
        center_hz: f64,
        control_nominal_hz: f64,
        control_measured_hz: f64,
    ) -> Self {
        let correction_hz = control_measured_hz - control_nominal_hz;
        let control_offset = control_measured_hz - center_hz;
        let chan = Channelizer::new(sample_rate, &[control_offset]);
        let rate = chan.output_rate();
        Self {
            chan,
            // The control channel of a trunked system is continuous, so the
            // CQPSK front end's blind acquisition has something to lock to.
            control: ChannelDecoder::with_offset(rate, Modulation::Cqpsk, EqMode::Enabled, 0.0),
            active: Vec::new(),
            center_hz,
            correction_hz,
            quiet_limit: 20,
            max_calls: 8,
        }
    }

    /// The tuner error being compensated for.
    pub fn correction_hz(&self) -> f64 {
        self.correction_hz
    }

    /// Calls currently being decoded.
    pub fn active_calls(&self) -> Vec<(u16, u64)> {
        self.active
            .iter()
            .map(|c| (c.talkgroup, c.freq_hz))
            .collect()
    }

    /// Diagnostics from the control channel.
    pub fn control_diagnostics(&self) -> &crate::diag::Diagnostics {
        self.control.diagnostics()
    }

    /// Feed wideband IQ; returns the calls that started and finished.
    pub fn process(&mut self, iq: &[f32]) -> FollowOutput {
        let mut out = FollowOutput::default();
        let chans = self.chan.process(iq);
        if chans.is_empty() {
            return out;
        }

        // Channel 0 is always the control channel.
        let control_out = self.control.process(&chans[0]);
        out.control_syncs = control_out.syncs;

        // Traffic channels follow, in the order `retune` laid them out.
        for (i, call) in self.active.iter_mut().enumerate() {
            let Some(samples) = chans.get(i + 1) else {
                continue;
            };
            let a = call.c4fm.process(samples);
            let b = call.cqpsk.process(samples);
            call.syncs_c4fm += a.syncs;
            call.syncs_cqpsk += b.syncs;
            call.pcm_c4fm.extend_from_slice(&a.pcm);
            call.pcm_cqpsk.extend_from_slice(&b.pcm);
            let syncs = a.syncs.max(b.syncs);
            if syncs == 0 {
                call.quiet += 1;
            } else {
                call.quiet = 0;
            }
        }

        // Retire finished calls.
        let limit = self.quiet_limit;
        let mut finished = Vec::new();
        let mut i = 0;
        while i < self.active.len() {
            if self.active[i].quiet >= limit {
                finished.push(self.active.remove(i));
            } else {
                i += 1;
            }
        }
        for c in finished {
            // Choose on decoded audio, not on frame syncs. The two counts run
            // close on a strong channel — 110 against 116 on the first real
            // capture — so syncs do not separate them, and the modulation that
            // syncs marginally more often can still produce visibly less
            // audio. Audio is what a scanner is for, so it decides.
            let (modulation, pcm, lc) = match (c.pcm_c4fm.len(), c.pcm_cqpsk.len()) {
                (0, 0) => (None, Vec::new(), None),
                (a, b) if a >= b => (
                    Some(Modulation::C4fm),
                    c.pcm_c4fm,
                    c.c4fm.diagnostics().link_control.first().cloned(),
                ),
                _ => (
                    Some(Modulation::Cqpsk),
                    c.pcm_cqpsk,
                    c.cqpsk.diagnostics().link_control.first().cloned(),
                ),
            };
            // A grant does not always name the radio; Link Control, which the
            // traffic channel sends about itself, usually does. Take the first
            // confirmed word — the radio that opened the transmission — rather
            // than the last, which on a shared talkgroup may be someone else.
            let source_unit = match (c.source_unit, lc.as_ref()) {
                (0, Some(l)) => l.source_unit,
                (s, _) => s,
            };
            out.completed.push(Call {
                syncs_c4fm: c.syncs_c4fm,
                syncs_cqpsk: c.syncs_cqpsk,
                talkgroup: c.talkgroup,
                source_unit,
                freq_hz: c.freq_hz,
                modulation,
                patched_with: self.control.patches().siblings(c.talkgroup),
                pcm,
            });
        }

        // Start calls the control channel just granted.
        for g in &control_out.grants {
            if g.encrypted {
                continue;
            }
            if self.active.iter().any(|c| c.freq_hz == g.freq_hz) {
                continue;
            }
            if self.active.len() >= self.max_calls {
                continue;
            }
            // Only follow a channel that is actually inside this capture. A
            // trunked system grants across its whole band, most of which a
            // single tuner cannot see.
            let offset = g.freq_hz as f64 + self.correction_hz - self.center_hz;
            if offset.abs() >= self.nyquist() {
                continue;
            }
            let rate = self.chan.output_rate();
            self.active.push(ActiveCall {
                freq_hz: g.freq_hz,
                talkgroup: g.talkgroup,
                source_unit: g.source_unit,
                c4fm: ChannelDecoder::with_offset(rate, Modulation::C4fm, EqMode::Bypass, 0.0),
                cqpsk: ChannelDecoder::with_offset(rate, Modulation::Cqpsk, EqMode::Enabled, 0.0),
                pcm_c4fm: Vec::new(),
                pcm_cqpsk: Vec::new(),
                syncs_c4fm: 0,
                syncs_cqpsk: 0,
                quiet: 0,
            });
            out.started.push((g.talkgroup, g.freq_hz));
        }

        self.retune();
        out
    }

    fn nyquist(&self) -> f64 {
        // The channelizer refuses offsets at or beyond this.
        self.chan.sample_rate() / 2.0 - 12_500.0
    }

    /// Point the channelizer at the control channel plus every active call.
    fn retune(&mut self) {
        let mut offsets = vec![self.control_offset()];
        for c in &self.active {
            offsets.push(c.freq_hz as f64 + self.correction_hz - self.center_hz);
        }
        self.chan.set_channels(&offsets);
    }

    fn control_offset(&self) -> f64 {
        self.chan.actual_offsets_hz()[0]
    }
}
