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
//! **The control channel does not stay put.** A site rotates its control
//! channel among a set of frequencies — for maintenance, or on its own
//! schedule — and announces the alternates over SCCB while it runs. A control
//! channel transmits continuously, so silence on it means it moved (or
//! faded), not that the system went idle. The follower watches for that
//! silence and hunts the announced alternates that fall inside the capture,
//! carrying the site's channel plans across so no grant is lost re-learning
//! them; alternates outside the capture are reported so a live front end can
//! retune the radio instead.
//!
//! **Tuner error is larger than the receiver's tolerance.** The demodulators
//! hold lock within roughly ±1 kHz, and an uncalibrated dongle can sit 6 kHz
//! off — enough that tuning a granted frequency by its nominal value finds
//! nothing. Rather than ask for a ppm figure, the follower takes the control
//! channel's *measured* frequency alongside its nominal one and applies the
//! difference to every channel it tunes afterwards. The control channel has to
//! be found before anything can be followed anyway, so the correction is free.

use crate::decoder::{ChannelDecoder, EqMode, Modulation};

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
    /// Grants the control channel issued that this capture cannot follow
    /// because the traffic channel is outside the tuned band. Reported so a
    /// live listener sees the system *is* active — and learns the band it would
    /// need to widen to reach those calls — rather than facing silence.
    pub grants_out_of_band: Vec<(u16, u64)>,
    /// Grants skipped because the call is encrypted.
    pub grants_encrypted: Vec<(u16, u64)>,
    /// The control channel went quiet and the follower retuned to an
    /// alternate: (old nominal Hz, new nominal Hz).
    pub control_moved: Option<(u64, u64)>,
    /// The control channel went quiet with nothing reachable to move to.
    /// Any known alternates outside the captured band are listed, so a live
    /// front end — or its operator — can retune the radio to one. Empty means
    /// the site never announced an alternate.
    pub control_lost: Option<Vec<u64>>,
}

pub struct TrunkFollower {
    /// Capture rate. Each channel is decimated straight out of the wideband
    /// stream at this rate — see the note on `new`.
    sample_rate: f64,
    control: ChannelDecoder,
    active: Vec<ActiveCall>,
    center_hz: f64,
    /// Added to every nominal frequency to find where it really is.
    correction_hz: f64,
    /// Blocks without a sync before a call is considered over (~1 s).
    quiet_limit: u32,
    /// Most calls the channelizer will follow at once.
    max_calls: usize,
    /// The control channel's modulation, which is also what a replacement
    /// control channel on the same site is decoded with.
    modulation: Modulation,
    /// Nominal frequency of the control channel currently being decoded.
    control_nominal_hz: u64,
    /// Nominal frequency the follower was started on — kept in the hunt
    /// rotation so a fade on the primary eventually retries it.
    primary_hz: u64,
    /// Consecutive blocks in which the control channel produced no frame
    /// sync. A control channel transmits continuously, so silence here means
    /// it is gone, not idle.
    control_quiet: u32,
    /// Quiet blocks before the control channel is declared lost (~2 s).
    control_loss_limit: u32,
    /// Rotation position over the known control channels, so repeated losses
    /// hunt through all of them rather than retrying one forever.
    hunt_next: usize,
    /// `control_lost` has been reported and nothing has changed since: the
    /// hunt keeps running quietly, but the caller is not told again until the
    /// control channel is actually heard.
    lost_reported: bool,
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
    ///
    /// Each channel is pulled out of the wideband stream by its own decimator
    /// inside a [`ChannelDecoder`], not by a shared FFT channelizer. A
    /// channelizer transforms the whole band every block, a cost paid whether
    /// one channel is wanted or a hundred; it wins only past several channels.
    /// A trunk follower watches one control channel and a handful of calls, so
    /// direct decimation is far cheaper — measured at ~6x on one channel — and
    /// that headroom is what lets the follower keep up with a live radio.
    pub fn new(
        sample_rate: f64,
        center_hz: f64,
        control_nominal_hz: f64,
        control_measured_hz: f64,
        modulation: Modulation,
    ) -> Self {
        let correction_hz = control_measured_hz - control_nominal_hz;
        let control_offset = control_measured_hz - center_hz;
        Self {
            sample_rate,
            // A control channel is continuous, so the CQPSK front end's blind
            // acquisition always has something to lock to. Each decoder
            // decimates its own channel straight out of the wideband stream.
            control: ChannelDecoder::with_offset(
                sample_rate,
                modulation,
                eq_for(modulation),
                control_offset,
            ),
            active: Vec::new(),
            center_hz,
            correction_hz,
            quiet_limit: 20,
            max_calls: 6,
            modulation,
            control_nominal_hz: control_nominal_hz as u64,
            primary_hz: control_nominal_hz as u64,
            control_quiet: 0,
            control_loss_limit: 20,
            hunt_next: 0,
            lost_reported: false,
        }
    }

    /// The tuner error being compensated for.
    pub fn correction_hz(&self) -> f64 {
        self.correction_hz
    }

    /// Nominal frequency of the control channel currently being followed.
    /// Starts where `new` pointed it; changes when the follower hunts onto an
    /// alternate control channel.
    pub fn control_hz(&self) -> u64 {
        self.control_nominal_hz
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
        // Choose on BCH-clean network identifiers, not on decoded audio.
        // Both C4FM and CQPSK partially decode each other's signals — a C4FM
        // discriminator on a CQPSK channel still emits audio, of similar
        // length — so audio length does not tell the correct modulation from
        // the incorrect one on a borderline channel. A clean NID has passed
        // its BCH check, so it is decode correctness measured directly; the
        // modulation that produces more of them is the one actually locked.
        // Audio length breaks a tie only when neither is cleaner.
        let clean = |d: &crate::diag::Diagnostics| -> usize {
            d.nids.iter().filter(|n| n.bch_errors == 0).count()
        };
        let (n_c4, n_cq) = (clean(c.c4fm.diagnostics()), clean(c.cqpsk.diagnostics()));
        let pick_c4fm = match n_c4.cmp(&n_cq) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => c.pcm_c4fm.len() >= c.pcm_cqpsk.len(),
        };
        let (modulation, pcm, lc) = if c.pcm_c4fm.is_empty() && c.pcm_cqpsk.is_empty() {
            (None, Vec::new(), None)
        } else if pick_c4fm {
            (
                Some(Modulation::C4fm),
                c.pcm_c4fm,
                c.c4fm.diagnostics().link_control.first().cloned(),
            )
        } else {
            (
                Some(Modulation::Cqpsk),
                c.pcm_cqpsk,
                c.cqpsk.diagnostics().link_control.first().cloned(),
            )
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

        // The control channel decimates itself out of the wideband stream.
        let control_out = self.control.process(iq);
        out.control_syncs = control_out.syncs;

        // A control channel transmits continuously, so a stretch with no
        // frame sync means it is gone — the site rotated it onto one of the
        // alternates it has been announcing (SCCB), or the signal faded. Hunt
        // the known control channels rather than sitting deaf forever.
        if control_out.syncs == 0 {
            self.control_quiet += 1;
            if self.control_quiet >= self.control_loss_limit {
                self.control_quiet = 0;
                self.hunt(&mut out);
            }
        } else {
            self.control_quiet = 0;
            self.lost_reported = false;
        }

        // Each active call does the same, at its own offset.
        for call in self.active.iter_mut() {
            let samples: &[f32] = iq;
            // Both modulations run for the whole call, and `retire` picks the
            // winner on total decoded audio at the end. Deciding early is
            // tempting for the CPU it would save, but it cannot be done on
            // accumulated audio: CQPSK's blind carrier acquisition takes about
            // half a second during which it emits nothing, while the C4FM
            // discriminator locks in one frame, so early audio always favours
            // C4FM even on a CQPSK signal. A mis-detected control channel plus
            // an early drop killed the correct modulation exactly this way. The
            // per-channel decimation this follower now uses is cheap enough
            // that running both to the end is affordable, and it is the only
            // unbiased comparison.
            let a = call.c4fm.process(samples);
            let b = call.cqpsk.process(samples);
            call.syncs_c4fm += a.syncs;
            call.syncs_cqpsk += b.syncs;
            call.pcm_c4fm.extend_from_slice(&a.pcm);
            call.pcm_cqpsk.extend_from_slice(&b.pcm);
            if a.syncs.max(b.syncs) == 0 {
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
            self.active.push(ActiveCall {
                freq_hz: g.freq_hz,
                talkgroup: g.talkgroup,
                source_unit: g.source_unit,
                c4fm: ChannelDecoder::with_offset(
                    self.sample_rate,
                    Modulation::C4fm,
                    EqMode::Bypass,
                    offset,
                ),
                cqpsk: ChannelDecoder::with_offset(
                    self.sample_rate,
                    Modulation::Cqpsk,
                    EqMode::Enabled,
                    offset,
                ),
                pcm_c4fm: Vec::new(),
                pcm_cqpsk: Vec::new(),
                syncs_c4fm: 0,
                syncs_cqpsk: 0,
                quiet: 0,
            });
            out.started.push((g.talkgroup, g.freq_hz));
        }

        out
    }

    /// The control channel has been silent past the loss limit: move to the
    /// next known control channel this capture can reach.
    ///
    /// The candidates are the alternates the site announced over SCCB plus
    /// the primary the follower started on, since a loss can also be a fade
    /// and the rotation should eventually retry it. The replacement decoder
    /// adopts the old one's site model, so the channel plans, patches, and
    /// the alternate list itself survive the move — waiting for them to be
    /// re-broadcast would drop every grant in between.
    fn hunt(&mut self, out: &mut FollowOutput) {
        let mut candidates: Vec<u64> = self.control.site().secondary_cc_freqs();
        if !candidates.contains(&self.primary_hz) {
            candidates.push(self.primary_hz);
        }
        candidates.retain(|&hz| hz != self.control_nominal_hz);

        let in_band: Vec<u64> = candidates
            .iter()
            .copied()
            .filter(|&hz| {
                let offset = hz as f64 + self.correction_hz - self.center_hz;
                offset.abs() < self.nyquist()
            })
            .collect();

        let Some(&next_hz) = in_band.get(self.hunt_next % in_band.len().max(1)) else {
            // Nothing reachable. Report the alternates this capture cannot
            // see, so a live front end can retune the radio to one — once,
            // not every couple of seconds while nothing changes.
            if !self.lost_reported {
                self.lost_reported = true;
                out.control_lost = Some(candidates);
            }
            return;
        };
        self.hunt_next += 1;

        let offset = next_hz as f64 + self.correction_hz - self.center_hz;
        let mut next = ChannelDecoder::with_offset(
            self.sample_rate,
            self.modulation,
            eq_for(self.modulation),
            offset,
        );
        next.adopt_trunk_state(&self.control);
        self.control = next;
        out.control_moved = Some((self.control_nominal_hz, next_hz));
        self.control_nominal_hz = next_hz;
    }

    fn nyquist(&self) -> f64 {
        // Leave a channel's width of margin from the band edge, where the
        // decimator's filter rolls off.
        self.sample_rate / 2.0 - 12_500.0
    }
}
