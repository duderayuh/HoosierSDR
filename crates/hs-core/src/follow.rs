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
//! **A call ends when the channel says so, not when it goes quiet.** A
//! traffic channel closes every transmission with a terminator frame (TDU).
//! Retiring on a quiet timeout alone did two bad things: each call dragged a
//! couple of seconds of dead air, and a channel granted to a new talkgroup
//! inside that window had its audio merged into the previous call. A
//! terminator now retires the call after a short hang — long enough that a
//! continuation of the same conversation keeps its decoders (and loses no
//! re-acquisition time), short enough that calls report promptly — and a
//! grant that reassigns an active channel retires the old call on the spot.
//! The quiet timeout remains as the fallback for a terminator lost to noise.
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
    /// Nonzero once a terminator (TDU) has been seen: the channel said the
    /// transmission is over, and this counts the hang blocks since. New voice
    /// clears it — the conversation continued — and the decoders stay alive
    /// through the hang, so a continuation costs no re-acquisition.
    ending: u32,
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
    /// Grants skipped because the listener locked the talkgroup out.
    pub grants_locked: Vec<(u16, u64)>,
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
    /// Blocks without a sync before a call is considered over (~1 s). The
    /// fallback for when the terminator is lost to noise.
    quiet_limit: u32,
    /// Blocks a call lingers after its terminator before it is retired. Long
    /// enough to bridge the gap to a continuation transmission of the same
    /// conversation; short enough that the call reports promptly.
    hang_limit: u32,
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
    /// Talkgroups the listener does not want to hear. Their grants are
    /// reported but never followed, and a call already up is dropped.
    lockout: std::collections::HashSet<u16>,
    /// When set, the only talkgroups followed — a playlist. Grants outside it
    /// count as locked. The lockout still applies within it.
    allowlist: Option<std::collections::HashSet<u16>>,
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
            hang_limit: 3,
            max_calls: 6,
            modulation,
            control_nominal_hz: control_nominal_hz as u64,
            primary_hz: control_nominal_hz as u64,
            control_quiet: 0,
            control_loss_limit: 20,
            hunt_next: 0,
            lost_reported: false,
            lockout: std::collections::HashSet::new(),
            allowlist: None,
        }
    }

    /// Restrict following to these talkgroups (`None` = all). Takes effect
    /// on the next block, like [`set_lockout`](Self::set_lockout).
    pub fn set_allowlist(&mut self, tgs: Option<impl IntoIterator<Item = u16>>) {
        self.allowlist = tgs.map(|t| t.into_iter().collect());
    }

    /// The playlist in force, if any.
    pub fn allowlist(&self) -> Option<&std::collections::HashSet<u16>> {
        self.allowlist.as_ref()
    }

    fn wanted(&self, tg: u16) -> bool {
        self.allowlist.as_ref().is_none_or(|a| a.contains(&tg)) && !self.lockout.contains(&tg)
    }

    /// Replace the set of locked-out talkgroups. Takes effect on the next
    /// block: pending grants for them are skipped and any call of theirs
    /// already being followed is dropped without being reported.
    pub fn set_lockout(&mut self, tgs: impl IntoIterator<Item = u16>) {
        self.lockout = tgs.into_iter().collect();
    }

    /// The talkgroups currently locked out.
    pub fn lockout(&self) -> &std::collections::HashSet<u16> {
        &self.lockout
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

        // A talkgroup locked out mid-call: stop following it now, silently —
        // the listener asked not to hear it, so it gets no audio and no row.
        if !self.lockout.is_empty() || self.allowlist.is_some() {
            let keep: Vec<bool> = self
                .active
                .iter()
                .map(|c| self.wanted(c.talkgroup))
                .collect();
            let mut i = 0;
            self.active.retain(|_| {
                i += 1;
                keep[i - 1]
            });
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
            // A terminator ends the transmission explicitly; hold the call
            // open for a short hang in case the conversation continues, then
            // retire it. Both decoders watch the same RF, so a terminator
            // from either is the channel's own word. New voice during the
            // hang means the call carried on.
            if a.terminators + b.terminators > 0 {
                call.ending = call.ending.max(1);
            } else if call.ending > 0 {
                if a.pcm.is_empty() && b.pcm.is_empty() {
                    call.ending += 1;
                } else {
                    call.ending = 0;
                }
            }
        }

        // Retire finished calls: terminated and past the hang, or — when the
        // terminator was lost — quiet past the timeout.
        let (quiet_limit, hang_limit) = (self.quiet_limit, self.hang_limit);
        let mut finished = Vec::new();
        let mut i = 0;
        while i < self.active.len() {
            if self.active[i].quiet >= quiet_limit || self.active[i].ending > hang_limit {
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
            if !self.wanted(g.talkgroup) {
                out.grants_locked.push((g.talkgroup, g.freq_hz));
                continue;
            }
            if let Some(pos) = self.active.iter().position(|c| c.freq_hz == g.freq_hz) {
                if self.active[pos].talkgroup == g.talkgroup {
                    // The same grant, repeated — grants are re-broadcast for
                    // the whole life of a call.
                    continue;
                }
                // The channel was reassigned to another talkgroup: whatever
                // audio the old call still owes is over, and every frame
                // decoded from here on belongs to the new one. Without this,
                // back-to-back calls on one frequency merged into the first
                // call's talkgroup. The new call starts fresh decoders — the
                // CQPSK path re-acquires (~0.5 s) — but reusing the old ones
                // would carry the old call's Link Control and clean-NID
                // history into the new call's attribution, which is the very
                // mistake this exists to fix.
                let old = self.active.remove(pos);
                let done = self.retire(old);
                out.completed.push(done);
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
                ending: 0,
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

// ---------------------------------------------------------------------------
// Finding and qualifying the control channel. These live here rather than in
// the CLI so every front end (CLI, desktop app) measures the same way.
// ---------------------------------------------------------------------------

/// How well a candidate frequency-and-modulation decodes the control channel,
/// as a sortable score.
///
/// Grants dominate, then clean NIDs. A control channel's purpose is to issue
/// grants, and — crucially — the modulation that produces them is not always
/// the one with the most clean NIDs. At the right frequency, C4FM and CQPSK
/// tie on clean NIDs, because the network identifier's BCH check survives the
/// confusion between the two; but only the correct modulation decodes the TSBK
/// payload behind it, so only it produces grants. Scoring on clean NIDs alone
/// therefore picks the modulation by a coin toss and, half the time, follows a
/// control channel that never grants anything. Counting grants first settles
/// it. Clean NIDs still break ties and still catch a control channel that
/// happened to be idle through the probe.
pub fn control_score(f: &TrunkFollower) -> (usize, u32) {
    let d = f.control_diagnostics();
    let grants = d.grants.len();
    let clean = d.nids.iter().filter(|n| n.bch_errors == 0).count() as u32;
    (grants, clean)
}

/// Find where a channel really is, given where it should be.
///
/// An uncalibrated tuner can sit several kilohertz off, which is more than the
/// demodulators tolerate, so the follower needs the control channel's measured
/// frequency rather than its nominal one. Rather than ask the user for a ppm
/// figure they have no easy way to obtain, find it here.
///
/// The obvious way — take the strongest spectral peak near the nominal
/// frequency — does not survive contact with a real capture. Two things beat
/// it: the tuner's own DC spike sits at the centre of the band and is often the
/// tallest thing in it, and a busy traffic channel one 25 kHz slot away is
/// routinely louder than the control channel itself. Both were observed on the
/// first recording this was tried against, and both produced a confident wrong
/// answer.
///
/// So measure the thing we actually care about instead of a proxy for it: try
/// each candidate and keep whichever one lets the control channel decode.
///
/// "Decode" has to mean more than frame syncs, though. Scoring on sync count
/// picked C4FM at zero offset over the correct CQPSK at −1 kHz, and followed
/// the system to zero calls — because a C4FM demodulator on a CQPSK signal
/// still syncs. The two modulations share a symbol rate and a frame structure,
/// so the sync correlator is not what separates them. What does is the network
/// identifier behind each sync: it carries a BCH code, so a wrong modulation
/// produces syncs whose NIDs do not check out. Counting *clean* NIDs scores
/// the thing that has to be right for anything downstream to work.
/// Returns `None` when nothing decoded anywhere in the window, which the
/// caller must report as such: a failure that prints like a successful
/// detection at zero error is worse than no detection at all. This function
/// used to return the nominal frequency in that case, and three control
/// channels that were simply not on the air were duly reported as "found at
/// nominal, tuner error +0 Hz" — indistinguishable from a real lock, and
/// nearly recorded as a measurement.
pub fn measure_carrier(
    iq: &[f32],
    sample_rate: f64,
    center_hz: f64,
    nominal_hz: f64,
) -> Option<(f64, Modulation)> {
    // Half a channel either way. Sweeping that whole span at the precision the
    // demodulators want would mean a hundred trial decodes, and each one
    // channelizes the probe from scratch. Coarse-then-fine gets the same
    // answer for a fraction of the cost: a kilohertz grid is tight enough that
    // the right slot still decodes something, and the refinement only has to
    // search its neighbourhood.
    const SEARCH_HZ: f64 = 12_500.0;
    const COARSE_HZ: f64 = 1_000.0;
    const FINE_HZ: f64 = 250.0;

    // Grants are the discriminator between the two modulations (both decode the
    // network identifier, only the right one decodes the grant behind it), and
    // grants are sparse — a couple a second — so the probe has to be a few
    // seconds, not one, or it catches none and the modulation choice falls back
    // to a coin toss. Three seconds holds enough grants to be decisive while
    // keeping the whole sweep to a few seconds of work at 13x real time.
    let want = 3 * sample_rate as usize;
    let probe = &iq[..want.min(iq.len())];

    let try_at = |cand: f64, m: Modulation| -> (usize, u32) {
        if (cand - center_hz).abs() >= sample_rate / 2.0 {
            return (0, 0);
        }
        let mut f = TrunkFollower::new(sample_rate, center_hz, nominal_hz, cand, m);
        f.process(probe);
        control_score(&f)
    };

    // Modulation is swept alongside frequency rather than asked for. It is not
    // knowable from the frequency — a scan of one band found control channels
    // of both kinds — and getting it wrong looks exactly like being tuned to
    // the wrong place, so guessing would produce a confident silence.
    let mods = [Modulation::Cqpsk, Modulation::C4fm];
    let mut best = ((0usize, 0u32), nominal_hz, Modulation::Cqpsk);
    let coarse = (SEARCH_HZ / COARSE_HZ) as i32;
    for k in -coarse..=coarse {
        let cand = nominal_hz + k as f64 * COARSE_HZ;
        for m in mods {
            let score = try_at(cand, m);
            if score > best.0 {
                best = (score, cand, m);
            }
        }
    }
    if best.0 == (0, 0) {
        return None;
    }
    let (centre, m) = (best.1, best.2);
    let fine = (COARSE_HZ / FINE_HZ) as i32;
    for k in -fine..=fine {
        let cand = centre + k as f64 * FINE_HZ;
        let score = try_at(cand, m);
        if score > best.0 {
            best = (score, cand, m);
        }
    }
    Some((best.1, best.2))
}

/// De-noises repeated grant announcements. A control channel repeats a grant
/// every cycle while its call is up, so the same out-of-band call is seen many
/// times a second; this reports each frequency at most once per cooldown.
pub struct GrantGate {
    /// freq -> blocks remaining before it may be reported again.
    seen: std::collections::HashMap<u64, u32>,
    cooldown: u32,
}

impl GrantGate {
    pub fn new(cooldown: u32) -> Self {
        Self {
            seen: std::collections::HashMap::new(),
            cooldown,
        }
    }
    /// True if this frequency should be reported now (and arms its cooldown).
    pub fn fresh(&mut self, freq: u64) -> bool {
        match self.seen.get_mut(&freq) {
            Some(c) if *c > 0 => false,
            _ => {
                self.seen.insert(freq, self.cooldown);
                true
            }
        }
    }
    /// Call once per processed block to age the cooldowns.
    pub fn tick(&mut self) {
        for c in self.seen.values_mut() {
            *c = c.saturating_sub(1);
        }
    }
}

/// Decide the modulation at a frequency the user supplied.
///
/// `--control-measured` names where the channel is but not what it speaks, and
/// the two are independent, so the shorter sweep still has to run.
pub fn pick_modulation(
    iq: &[f32],
    sample_rate: f64,
    center_hz: f64,
    nominal_hz: f64,
    measured_hz: f64,
) -> Option<Modulation> {
    let probe = &iq[..(sample_rate as usize).min(iq.len())];
    let score = |m: Modulation| {
        let mut f = TrunkFollower::new(sample_rate, center_hz, nominal_hz, measured_hz, m);
        f.process(probe);
        control_score(&f)
    };
    let (c4, cq) = (score(Modulation::C4fm), score(Modulation::Cqpsk));
    if c4 == (0, 0) && cq == (0, 0) {
        None
    } else if c4 > cq {
        Some(Modulation::C4fm)
    } else {
        Some(Modulation::Cqpsk)
    }
}

/// Is the control channel inside the captured band? `Err` carries a message
/// fit to show a user, with the band the capture actually covers.
pub fn in_band(sample_rate: f64, center_hz: f64, control_hz: f64) -> Result<(), String> {
    let nyquist = sample_rate / 2.0;
    if (control_hz - center_hz).abs() >= nyquist {
        return Err(format!(
            "control {:.4} MHz is outside the capture: centered at {:.4} MHz, \
             {:.4} MHz wide (covers {:.4}–{:.4} MHz).",
            control_hz / 1e6,
            center_hz / 1e6,
            sample_rate / 1e6,
            (center_hz - nyquist) / 1e6,
            (center_hz + nyquist) / 1e6,
        ));
    }
    Ok(())
}
