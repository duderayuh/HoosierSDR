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
//! **Modulation belongs to the site, not to the channel's job.** C4FM and
//! CQPSK are not "voice" and "control" modulations: a scan of one 2.4 MHz
//! slice found a CQPSK control channel and a C4FM one, and traffic channels of
//! both kinds. What actually decides is simulcast — several towers radiating
//! the same signal on the same frequency need a linear modulation, so
//! simulcast sites run CQPSK and others run C4FM — which is a property of the
//! transmitter. Every channel of one site therefore tends to agree, and
//! measurement bears that out: across the traffic channels of NAC 0x261 the
//! winner was CQPSK every time, matching its CQPSK control channel, while
//! NAC 0x6B6's traffic channel was C4FM, matching its C4FM control channel.
//!
//! So a call is decoded with the modulation its control channel uses. The
//! other one is started alongside it and dropped as soon as the first has
//! produced real audio, because the site rule is a strong guide rather than a
//! guarantee and the wrong guess would otherwise cost the whole call. This is
//! not a cosmetic saving: decoding every call twice for its full duration
//! doubled the per-call cost, and the answer is settled within a fraction of a
//! second.
//!
//! An earlier version of this comment claimed the opposite — control CQPSK,
//! traffic C4FM — and it was simply wrong. What misled it is worth recording:
//! a C4FM demodulator on a CQPSK signal does not fail cleanly. The two share a
//! symbol rate and a frame structure, so it syncs and yields *some* audio
//! (29 syncs and 2.34 s against CQPSK's 33 and 2.88 s on the same recording).
//! Frame syncs alone do not separate them; decoded audio does.
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

/// The equalizer is the point of the project on CQPSK, where a simulcast
/// channel is what it exists to correct; on C4FM it has nothing to do.
fn eq_for(m: Modulation) -> EqMode {
    match m {
        Modulation::Cqpsk => EqMode::Enabled,
        Modulation::C4fm => EqMode::Bypass,
    }
}

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
    /// Modulation inherited from the control channel, tried first.
    primary: Modulation,
    /// Whether the other modulation is still being decoded as a hedge. Cleared
    /// once `primary` has produced enough audio to have proven itself.
    hedging: bool,
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
    /// Calls that confirmed the site's modulation this block and stopped
    /// decoding the alternative. Reported so the saving can be tested rather
    /// than assumed.
    pub hedges_dropped: u32,
    /// Grants the control channel issued that this capture cannot follow
    /// because the traffic channel is outside the tuned band. Reported so a
    /// live listener sees the system *is* active — and learns the band it would
    /// need to widen to reach those calls — rather than facing silence.
    pub grants_out_of_band: Vec<(u16, u64)>,
    /// Grants skipped because the call is encrypted.
    pub grants_encrypted: Vec<(u16, u64)>,
}

pub struct TrunkFollower {
    chan: Channelizer,
    /// Modulation this site uses, and hence what each call is decoded with.
    modulation: Modulation,
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
    /// `modulation` is what the control channel uses, which is also what its
    /// traffic channels are decoded with — see the note on simulcast above.
    pub fn new(
        sample_rate: f64,
        center_hz: f64,
        control_nominal_hz: f64,
        control_measured_hz: f64,
        modulation: Modulation,
    ) -> Self {
        let correction_hz = control_measured_hz - control_nominal_hz;
        let control_offset = control_measured_hz - center_hz;
        let chan = Channelizer::new(sample_rate, &[control_offset]);
        let rate = chan.output_rate();
        Self {
            chan,
            modulation,
            // A control channel is continuous, so the CQPSK front end's blind
            // acquisition always has something to lock to.
            control: ChannelDecoder::with_offset(rate, modulation, eq_for(modulation), 0.0),
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

    /// Close out an in-progress call at end of stream.
    ///
    /// A live radio runs until stopped, so a call always ends by going quiet.
    /// A recording does not: the file simply stops, usually mid-transmission,
    /// and without this the call in flight — often the only one — is discarded
    /// along with every second of audio already decoded from it. Draining here
    /// means a short capture reports what it actually heard.
    pub fn finish(&mut self) -> Vec<Call> {
        let active = core::mem::take(&mut self.active);
        active.into_iter().map(|c| self.retire(c)).collect()
    }

    /// Turn a finished call into its reported form: pick the modulation that
    /// produced audio, and name the radio from Link Control when the grant
    /// did not.
    fn retire(&mut self, c: ActiveCall) -> Call {
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
        Call {
            syncs_c4fm: c.syncs_c4fm,
            syncs_cqpsk: c.syncs_cqpsk,
            talkgroup: c.talkgroup,
            source_unit,
            freq_hz: c.freq_hz,
            modulation,
            patched_with: self.control.patches().siblings(c.talkgroup),
            pcm,
        }
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
            // The site's modulation always runs. The other one runs only
            // until the first has proven itself.
            let want_c4fm = call.hedging || call.primary == Modulation::C4fm;
            let want_cqpsk = call.hedging || call.primary == Modulation::Cqpsk;
            let mut syncs = 0;
            if want_c4fm {
                let a = call.c4fm.process(samples);
                syncs = syncs.max(a.syncs);
                call.syncs_c4fm += a.syncs;
                call.pcm_c4fm.extend_from_slice(&a.pcm);
            }
            if want_cqpsk {
                let b = call.cqpsk.process(samples);
                syncs = syncs.max(b.syncs);
                call.syncs_cqpsk += b.syncs;
                call.pcm_cqpsk.extend_from_slice(&b.pcm);
            }
            if syncs == 0 {
                call.quiet += 1;
            } else {
                call.quiet = 0;
            }

            // Two voice frames is past any accident: the inherited modulation
            // is decoding, so stop paying for the alternative. Dropping the
            // hedge only ever happens *after* the primary is confirmed
            // working, so it cannot cost audio — if the primary never
            // confirms, both keep running and the end-of-call comparison
            // decides exactly as it did before.
            const CONFIRM_SAMPLES: usize = 2 * 1440;
            if call.hedging {
                let (kept, dropped) = match call.primary {
                    Modulation::C4fm => (call.pcm_c4fm.len(), &mut call.pcm_cqpsk),
                    Modulation::Cqpsk => (call.pcm_cqpsk.len(), &mut call.pcm_c4fm),
                };
                if kept >= CONFIRM_SAMPLES {
                    dropped.clear();
                    dropped.shrink_to_fit();
                    call.hedging = false;
                    out.hedges_dropped += 1;
                }
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
            out.completed.push(self.retire(c));
        }

        // Start calls the control channel just granted.
        for g in &control_out.grants {
            if g.encrypted {
                out.grants_encrypted.push((g.talkgroup, g.freq_hz));
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
                out.grants_out_of_band.push((g.talkgroup, g.freq_hz));
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
                primary: self.modulation,
                hedging: true,
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
