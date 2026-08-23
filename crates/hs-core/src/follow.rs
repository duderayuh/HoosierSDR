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
use hs_dsp::channelizer::Channelizer;

/// Rate the channelizer delivers every traffic channel at (10 samples per
/// symbol), and hence the rate the call decoders are built for.
const CHANNEL_RATE: f64 = 48_000.0;

/// Calls that must agree with the control channel's modulation before the
/// second decoder is dropped (when single-modulation decoding is enabled),
/// and how often to re-probe afterwards.
const CONFIRM_CALLS: u32 = 2;
const REPROBE_EVERY: u32 = 10;

/// Channel filter applied to each channelizer output before its decoders:
/// the same 8 kHz passband / 24 kHz stopband the per-channel decimator used.
/// The channelizer's slice is a brick wall at ±24 kHz, which lets a
/// neighbouring channel 12.5 kHz away straight into the demodulator — the
/// first live run without this filter garbled every call on a busy site.
const CHANNEL_PASSBAND_HZ: f64 = 8_000.0;

fn channel_filter() -> hs_dsp::fir::FirC {
    let cutoff = CHANNEL_PASSBAND_HZ / CHANNEL_RATE;
    let stop = 0.5;
    let transition = stop - cutoff;
    let mut n = (3.3 / transition).ceil() as usize;
    n = n.clamp(31, 255);
    if n.is_multiple_of(2) {
        n += 1;
    }
    hs_dsp::fir::FirC::new(hs_dsp::fir::lowpass_taps(n, cutoff + transition / 2.0), 1)
}

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
    /// Offset of the channel from the capture centre (corrected), Hz — what
    /// the channelizer is asked for.
    offset_hz: f64,
    /// Both modulations, because only the channel knows which it uses —
    /// but only one of them runs once the site's modulation is confirmed
    /// (`dual`); the other sits idle.
    c4fm: ChannelDecoder,
    cqpsk: ChannelDecoder,
    dual: bool,
    /// Seconds of IQ this call has been fed.
    age: f64,
    /// Channel filter on the channelizer's slice (see `channel_filter`).
    filter: hs_dsp::fir::FirC,
    /// Classic extraction: decoders built at the capture rate with their
    /// own decimators; fed the wideband stream, not a channelizer slice.
    wideband: bool,
    /// Audio from each modulation, kept separately so the choice between them
    /// can be made on the thing that matters rather than guessed early.
    pcm_c4fm: Vec<i16>,
    pcm_cqpsk: Vec<i16>,
    syncs_c4fm: u32,
    syncs_cqpsk: u32,
    /// Seconds of IQ seen with no frame sync, used to retire a finished call.
    quiet: f64,
    /// Nonzero once a terminator (TDU) has been seen: the channel said the
    /// transmission is over, and this counts the hang blocks since. New voice
    /// clears it — the conversation continued — and the decoders stay alive
    /// through the hang, so a continuation costs no re-acquisition.
    ending: Option<f64>,
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
    /// A radio signalled emergency during the call (link-control service
    /// option bit).
    pub emergency: bool,
    /// The radio's over-the-air alias, when the system broadcast one (see
    /// `hs_p25::talker_alias`).
    pub talker_alias: Option<String>,
    /// 8 kHz mono audio.
    pub pcm: Vec<i16>,
}

/// What the control channel has said about its site so far.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SiteInfo {
    /// Most-seen NAC among BCH-clean NIDs.
    pub nac: Option<u16>,
    pub wacn: Option<u32>,
    pub sys_id: Option<u16>,
    /// Control channel currently followed (nominal Hz).
    pub control_hz: u64,
    /// Alternate control channels the site announced (Hz).
    pub alternates_hz: Vec<u64>,
    /// Channel plans: (iden, base Hz, spacing Hz).
    pub idens: Vec<(u8, u64, u64)>,
    /// Active patches: (supergroup, members).
    pub patches: Vec<(u16, Vec<u16>)>,
    /// This site's RFSS and site numbers, once broadcast.
    pub rfss: Option<u8>,
    pub site: Option<u8>,
    /// Neighbouring sites: (system, RFSS, site, control channel Hz if the
    /// plan is known).
    pub neighbours: Vec<(u16, u8, u8, Option<u64>)>,
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
    /// Grants not followed because every decoder slot was busy with a call
    /// of equal or higher priority.
    pub grants_busy: Vec<(u16, u64)>,
    /// The control channel went quiet and the follower retuned to an
    /// alternate: (old nominal Hz, new nominal Hz).
    pub control_moved: Option<(u64, u64)>,
    /// The control channel went quiet with nothing reachable to move to.
    /// Any known alternates outside the captured band are listed, so a live
    /// front end — or its operator — can retune the radio to one. Empty means
    /// the site never announced an alternate.
    pub control_lost: Option<Vec<u64>>,
    /// Every grant the control channel issued this block, before any
    /// filtering or de-duplication — the raw material for discovery
    /// (which talkgroups and frequencies a site actually uses).
    pub grants: Vec<hs_trunk::Grant>,
    /// Affiliation / registration messages heard this block, already applied
    /// to [`TrunkFollower::affiliations`].
    pub mobility: Vec<hs_trunk::MobilityEvent>,
    /// Radio position reports from packet data: (unit, lat, lon).
    pub locations: Vec<(u32, f64, f64)>,
    /// Over-the-air talker aliases confirmed this block: (talkgroup, alias).
    pub talker_aliases: Vec<(u16, String)>,
}

/// One radio's worth of spectrum in which calls are decoded: the primary
/// band (which also carries the control channel) or an extra radio parked
/// on another part of the site's span. A call lives in the band that covers
/// its frequency; the follower routes each grant to whichever band can.
pub struct Band {
    pub center_hz: f64,
    pub sample_rate: f64,
    chan: Option<Channelizer>,
    chan_offsets: Vec<f64>,
    active: Vec<ActiveCall>,
}

impl Band {
    fn new(center_hz: f64, sample_rate: f64) -> Self {
        Self {
            center_hz,
            sample_rate,
            chan: None,
            chan_offsets: Vec::new(),
            active: Vec::new(),
        }
    }

    /// Half the usable width: a channel's margin inside the band edge,
    /// where the decimator's filter rolls off.
    fn nyquist(&self) -> f64 {
        self.sample_rate / 2.0 - 12_500.0
    }

    /// Offset of a (corrected) frequency from this band's centre, if inside.
    fn offset_of(&self, hz: f64) -> Option<f64> {
        let off = hz - self.center_hz;
        (off.abs() < self.nyquist()).then_some(off)
    }
}

pub struct TrunkFollower {
    /// Capture rate. The control channel is decimated straight out of the
    /// wideband stream at this rate; traffic channels come out of the
    /// channelizer — see the note on `new`.
    sample_rate: f64,
    control: ChannelDecoder,
    /// The radio the control channel is on, and the calls inside its span.
    band: Band,
    /// Further radios covering other parts of the site's span; each is fed
    /// by [`process_band`](Self::process_band).
    extra: Vec<Band>,
    /// Calls in a row whose winning modulation matched the control
    /// channel's; past `CONFIRM_CALLS`, new calls run one decoder.
    mod_confirmed: u32,
    /// Calls started, for the periodic re-probe.
    calls_started: u32,
    /// Decode new calls with the site's modulation alone once confirmed.
    /// Off by default: a C4FM discriminator on a CQPSK signal still syncs
    /// and emits audio — garbled — so a wrong site measurement would turn
    /// every call to noise with nothing to catch it. Dual decoding with
    /// clean-NID arbitration is the safety net, and the channelizer makes
    /// it affordable.
    single_modulation: bool,
    /// Extract traffic channels with the shared channelizer (default) or,
    /// classically, with one decimator per channel straight from the
    /// wideband stream — kept as an on-air A/B switch.
    use_channelizer: bool,
    /// Decode every call with this modulation only, whatever was measured:
    /// the listener knows the site (RadioReference says simulcast → CQPSK)
    /// and a C4FM discriminator on a CQPSK signal still syncs and emits
    /// garbled audio that can win the arbitration.
    forced: Option<Modulation>,
    /// Unvoiced-synthesis quality handed to each call's vocoder (1–64).
    uv_quality: i32,
    center_hz: f64,
    /// Tuner error in parts per million; every nominal frequency is multiplied
    /// by `1 + ppm/1e6` to find where it really appears.
    correction_ppm: f64,
    /// Seconds without a sync before a call is considered over. The fallback
    /// for when the terminator is lost to noise. Measured in seconds of IQ,
    /// not blocks: callers feed blocks of wildly different lengths (the CLI
    /// 100 ms, the app 6.8 ms at 9.6 MSPS), and a block count that was ~2 s
    /// for one was 136 ms for the other — shorter than CQPSK acquisition, so
    /// every call was retired before it produced a sample of audio.
    quiet_secs: f64,
    /// Seconds a call lingers after its terminator before it is retired. Long
    /// enough to bridge the gap to a continuation transmission of the same
    /// conversation; short enough that the call reports promptly.
    hang_secs: f64,
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
    /// Seconds in which the control channel produced no frame sync. A control
    /// channel transmits continuously, so silence here means it is gone.
    control_quiet: f64,
    /// Quiet seconds before the control channel is declared lost.
    control_loss_secs: f64,
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
    /// Talkgroup priorities, 1 (highest) … 99; unlisted = 50. Decides which
    /// call gives way when every decoder slot is busy.
    priority: std::collections::HashMap<u16, u8>,
    /// Locked-out talkgroup ranges (inclusive), alongside the explicit set.
    lockout_ranges: Vec<(u16, u16)>,
    /// Priority ranges (inclusive); an explicit entry wins over a range.
    priority_ranges: Vec<(u16, u16, u8)>,
    /// Who is where, from the control channel's mobility messages.
    affiliations: hs_trunk::AffiliationTable,
    /// Seconds of IQ processed, the clock affiliations are stamped with.
    elapsed_secs: f64,
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
    /// The control channel is pulled out of the wideband stream by its own
    /// decimator inside a [`ChannelDecoder`]: it is always there, and direct
    /// decimation is far cheaper for a single channel (~6x, measured). Traffic
    /// channels go through the shared FFT [`Channelizer`] instead. Its cost
    /// is one transform of the band per block however many calls are up,
    /// which is what lets a busy site — ten or twenty calls at once — decode
    /// in real time; the per-channel decimators that preceded it scaled
    /// linearly and ran a laptop out of CPU at six, garbling every call at
    /// once when the USB queue overflowed. (This is also how SDRTrunk's
    /// polyphase channelizer handles the same load.)
    pub fn new(
        sample_rate: f64,
        center_hz: f64,
        control_nominal_hz: f64,
        control_measured_hz: f64,
        modulation: Modulation,
    ) -> Self {
        let correction_ppm = (control_measured_hz - control_nominal_hz) / control_nominal_hz * 1e6;
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
            band: Band::new(center_hz, sample_rate),
            extra: Vec::new(),
            mod_confirmed: 0,
            calls_started: 0,
            single_modulation: false,
            use_channelizer: true,
            forced: None,
            uv_quality: hs_vocoder::imbe::DEFAULT_UV_QUALITY,
            center_hz,
            correction_ppm,
            quiet_secs: 2.0,
            hang_secs: 0.3,
            max_calls: 12,
            modulation,
            control_nominal_hz: control_nominal_hz as u64,
            primary_hz: control_nominal_hz as u64,
            control_quiet: 0.0,
            control_loss_secs: 2.0,
            hunt_next: 0,
            lost_reported: false,
            lockout: std::collections::HashSet::new(),
            allowlist: None,
            priority: std::collections::HashMap::new(),
            lockout_ranges: Vec::new(),
            priority_ranges: Vec::new(),
            affiliations: hs_trunk::AffiliationTable::new(),
            elapsed_secs: 0.0,
        }
    }

    /// Lock out whole talkgroup ranges (inclusive) in addition to the
    /// explicit set — the way alias lists express "everything from 10000 to
    /// 10999". Takes effect on the next block.
    pub fn set_lockout_ranges(&mut self, ranges: impl IntoIterator<Item = (u16, u16)>) {
        self.lockout_ranges = ranges
            .into_iter()
            .map(|(a, b)| (a.min(b), a.max(b)))
            .collect();
    }

    pub fn lockout_ranges(&self) -> &[(u16, u16)] {
        &self.lockout_ranges
    }

    /// Priority for whole ranges (inclusive); an explicit per-talkgroup
    /// priority still wins, and the first matching range after that.
    pub fn set_priority_ranges(&mut self, ranges: impl IntoIterator<Item = (u16, u16, u8)>) {
        self.priority_ranges = ranges
            .into_iter()
            .map(|(a, b, p)| (a.min(b), a.max(b), p.clamp(1, 99)))
            .collect();
    }

    pub fn priority_ranges(&self) -> &[(u16, u16, u8)] {
        &self.priority_ranges
    }

    /// The radios the control channel has placed on talkgroups.
    pub fn affiliations(&self) -> &hs_trunk::AffiliationTable {
        &self.affiliations
    }

    /// Recent decision-stage symbols from the control channel, oldest
    /// first — see [`ChannelDecoder::recent_symbols`].
    pub fn control_symbols(&self) -> Vec<(f32, f32)> {
        self.control.recent_symbols().collect()
    }

    /// Talkgroup priorities (1 = highest, 99 = lowest; unlisted = 50). When
    /// the decoder slots are full, a grant for a higher-priority talkgroup
    /// retires the lowest-priority call in progress; equal priority waits.
    pub fn set_priorities(&mut self, p: impl IntoIterator<Item = (u16, u8)>) {
        self.priority = p
            .into_iter()
            .map(|(tg, pr)| (tg, pr.clamp(1, 99)))
            .collect();
    }

    pub fn priority_of(&self, tg: u16) -> u8 {
        self.priority.get(&tg).copied().unwrap_or_else(|| {
            self.priority_ranges
                .iter()
                .find(|(lo, hi, _)| (*lo..=*hi).contains(&tg))
                .map(|(_, _, p)| *p)
                .unwrap_or(50)
        })
    }

    /// How long a call lingers, in seconds: `hang` after its terminator, and
    /// `quiet` with no frame sync before it is considered over (the fallback
    /// when the terminator is lost). Independent of block size.
    pub fn set_hang(&mut self, hang: f64, quiet: f64) {
        self.hang_secs = hang.max(0.05);
        self.quiet_secs = quiet.max(0.5);
    }

    pub fn hang(&self) -> (f64, f64) {
        (self.hang_secs, self.quiet_secs)
    }

    /// Add a radio covering another part of the site's span (its centre
    /// and rate). Feed it with [`process_band`](Self::process_band); grants
    /// whose frequency falls inside it are decoded there. Returns its index.
    pub fn add_band(&mut self, center_hz: f64, sample_rate: f64) -> usize {
        self.extra.push(Band::new(center_hz, sample_rate));
        self.extra.len() - 1
    }

    /// Every band: (centre Hz, sample rate), primary first.
    pub fn bands(&self) -> Vec<(f64, f64)> {
        std::iter::once((self.band.center_hz, self.band.sample_rate))
            .chain(self.extra.iter().map(|b| (b.center_hz, b.sample_rate)))
            .collect()
    }

    /// Feed a block of IQ from extra band `idx`. Only calls routed to that
    /// band advance; the control channel is the primary band's business.
    pub fn process_band(&mut self, idx: usize, iq: &[f32]) -> FollowOutput {
        let mut out = FollowOutput::default();
        if idx >= self.extra.len() {
            return out;
        }
        let secs = (iq.len() / 2) as f64 / self.extra[idx].sample_rate;
        self.decode_band(Some(idx), iq, secs, &mut out);
        out
    }

    fn take_band(&mut self, which: Option<usize>) -> Band {
        match which {
            None => {
                let (c, r) = (self.band.center_hz, self.band.sample_rate);
                std::mem::replace(&mut self.band, Band::new(c, r))
            }
            Some(i) => {
                let (c, r) = (self.extra[i].center_hz, self.extra[i].sample_rate);
                std::mem::replace(&mut self.extra[i], Band::new(c, r))
            }
        }
    }

    fn put_band(&mut self, which: Option<usize>, b: Band) {
        match which {
            None => self.band = b,
            Some(i) => self.extra[i] = b,
        }
    }

    /// Calls in progress across every band.
    fn active_count(&self) -> usize {
        self.band.active.len() + self.extra.iter().map(|b| b.active.len()).sum::<usize>()
    }

    /// Vocoder unvoiced-synthesis quality for calls started afterwards
    /// (see `hs_vocoder::imbe::DEFAULT_UV_QUALITY`): higher is smoother,
    /// less granular unvoiced sound — the texture listeners call metallic.
    pub fn set_uv_quality(&mut self, q: i32) {
        self.uv_quality = q.clamp(1, 64);
    }

    /// Force the traffic-channel modulation (`None` = arbitrate per call).
    pub fn set_forced_modulation(&mut self, m: Option<Modulation>) {
        self.forced = m;
    }

    /// Channelizer (true, default) or classic per-channel decimation (false)
    /// for traffic channels. Applies to calls started afterwards.
    pub fn set_channelizer(&mut self, on: bool) {
        self.use_channelizer = on;
    }

    /// Decode with the site's modulation alone once a couple of calls have
    /// confirmed it (halves the per-call work; see the field note on the
    /// struct for why it is off by default).
    pub fn set_single_modulation(&mut self, on: bool) {
        self.single_modulation = on;
    }

    /// Most decoder slots used at once.
    pub fn set_max_calls(&mut self, n: usize) {
        self.max_calls = n.max(1);
    }

    /// Which active call should give way to a grant of priority `pri`, if any:
    /// the worst-priority call, and only if it is strictly worse than `pri`.
    fn contention_victim(&self, pri: u8) -> Option<(Option<usize>, usize)> {
        let mut worst: Option<((Option<usize>, usize), u8)> = None;
        let mut consider = |which: Option<usize>, active: &Vec<ActiveCall>| {
            for (i, c) in active.iter().enumerate() {
                let p = self.priority_of(c.talkgroup);
                if worst.is_none_or(|(_, wp)| p > wp) {
                    worst = Some(((which, i), p));
                }
            }
        };
        consider(None, &self.band.active);
        for (bi, b) in self.extra.iter().enumerate() {
            consider(Some(bi), &b.active);
        }
        let (loc, p) = worst?;
        (p > pri).then_some(loc)
    }

    fn remove_call(&mut self, loc: (Option<usize>, usize)) -> ActiveCall {
        match loc.0 {
            None => self.band.active.remove(loc.1),
            Some(bi) => self.extra[bi].active.remove(loc.1),
        }
    }

    /// Where a call on this frequency is being decoded, if anywhere.
    fn find_by_freq(&self, freq_hz: u64) -> Option<(Option<usize>, usize)> {
        if let Some(i) = self.band.active.iter().position(|c| c.freq_hz == freq_hz) {
            return Some((None, i));
        }
        for (bi, b) in self.extra.iter().enumerate() {
            if let Some(i) = b.active.iter().position(|c| c.freq_hz == freq_hz) {
                return Some((Some(bi), i));
            }
        }
        None
    }

    fn call_talkgroup(&self, loc: (Option<usize>, usize)) -> u16 {
        match loc.0 {
            None => self.band.active[loc.1].talkgroup,
            Some(bi) => self.extra[bi].active[loc.1].talkgroup,
        }
    }

    /// What the control channel has announced about this site.
    pub fn site_info(&self) -> SiteInfo {
        let d = self.control.diagnostics();
        let mut counts: std::collections::HashMap<u16, usize> = std::collections::HashMap::new();
        for n in d.nids.iter().filter(|n| n.bch_errors == 0) {
            *counts.entry(n.nac).or_default() += 1;
        }
        let nac = counts.into_iter().max_by_key(|(_, c)| *c).map(|(n, _)| n);
        let site = self.control.site();
        SiteInfo {
            nac,
            wacn: site.system.map(|s| s.wacn),
            sys_id: site.system.map(|s| s.sys_id),
            control_hz: self.control_nominal_hz,
            alternates_hz: site.secondary_cc_freqs(),
            idens: site
                .idens()
                .map(|(id, p)| (id, p.base_freq_hz, p.spacing_hz))
                .collect(),
            patches: self.control.patches().patches().to_vec(),
            rfss: site.rfss,
            site: site.site,
            neighbours: site
                .neighbours()
                .into_iter()
                .map(|(n, hz)| (n.sys_id, n.rfss, n.site, hz))
                .collect(),
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
        self.allowlist.as_ref().is_none_or(|a| a.contains(&tg))
            && !self.lockout.contains(&tg)
            && !self
                .lockout_ranges
                .iter()
                .any(|(lo, hi)| (*lo..=*hi).contains(&tg))
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

    /// The tuner error being compensated for, as parts per million.
    pub fn correction_ppm(&self) -> f64 {
        self.correction_ppm
    }

    /// The tuner error as a Hz offset at the current control channel, for
    /// display ("tuner error +123 Hz").
    pub fn correction_hz(&self) -> f64 {
        self.correction_ppm * self.control_nominal_hz as f64 / 1e6
    }

    /// Nominal frequency of the control channel currently being followed.
    /// Starts where `new` pointed it; changes when the follower hunts onto an
    /// alternate control channel.
    pub fn control_hz(&self) -> u64 {
        self.control_nominal_hz
    }

    /// Calls currently being decoded.
    pub fn active_calls(&self) -> Vec<(u16, u64)> {
        self.band
            .active
            .iter()
            .chain(self.extra.iter().flat_map(|b| b.active.iter()))
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
        let mut active = core::mem::take(&mut self.band.active);
        for b in self.extra.iter_mut() {
            active.extend(core::mem::take(&mut b.active));
        }
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
        let (mut n_c4, mut n_cq) = (clean(c.c4fm.diagnostics()), clean(c.cqpsk.diagnostics()));
        if !c.dual {
            // Only one decoder ran; the other has nothing to say.
            match self.forced.unwrap_or(self.modulation) {
                Modulation::C4fm => n_cq = 0,
                Modulation::Cqpsk => n_c4 = 0,
            }
        }
        let emergency = c
            .c4fm
            .diagnostics()
            .link_control
            .iter()
            .chain(c.cqpsk.diagnostics().link_control.iter())
            .any(|l| l.emergency);
        let pick_c4fm = match self.forced {
            Some(Modulation::C4fm) => true,
            Some(Modulation::Cqpsk) => false,
            None => match n_c4.cmp(&n_cq) {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Less => false,
                std::cmp::Ordering::Equal => c.pcm_c4fm.len() >= c.pcm_cqpsk.len(),
            },
        };
        // A dual-decoded call with clean frames is evidence about the site:
        // agreement with the control channel's modulation counts toward
        // dropping the second decoder; disagreement resets it.
        if c.dual && n_c4 + n_cq > 0 {
            let winner = if pick_c4fm {
                Modulation::C4fm
            } else {
                Modulation::Cqpsk
            };
            if winner == self.modulation {
                self.mod_confirmed += 1;
            } else {
                self.mod_confirmed = 0;
            }
        }
        // Whichever decoder was locked heard the alias words, if any.
        let talker_alias = if pick_c4fm {
            c.c4fm.talker_alias().or(c.cqpsk.talker_alias())
        } else {
            c.cqpsk.talker_alias().or(c.c4fm.talker_alias())
        }
        .map(str::to_string);
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
            emergency,
            talker_alias,
            pcm,
        }
    }

    /// Feed wideband IQ; returns the calls that started and finished.
    pub fn process(&mut self, iq: &[f32]) -> FollowOutput {
        let mut out = FollowOutput::default();

        let secs = (iq.len() / 2) as f64 / self.sample_rate;
        self.elapsed_secs += secs;

        // The control channel decimates itself out of the wideband stream.
        let control_out = self.control.process(iq);
        out.control_syncs = control_out.syncs;
        out.grants = control_out.grants.clone();
        for ev in &control_out.mobility {
            self.affiliations.observe(*ev, self.elapsed_secs);
        }
        out.mobility = control_out.mobility.clone();
        out.locations
            .extend(control_out.locations.iter().map(|l| (l.llid, l.lat, l.lon)));

        // A control channel transmits continuously, so a stretch with no
        // frame sync means it is gone — the site rotated it onto one of the
        // alternates it has been announcing (SCCB), or the signal faded. Hunt
        // the known control channels rather than sitting deaf forever.
        if control_out.syncs == 0 {
            self.control_quiet += secs;
            if self.control_quiet >= self.control_loss_secs {
                self.control_quiet = 0.0;
                self.hunt(&mut out);
            }
        } else {
            self.control_quiet = 0.0;
            self.lost_reported = false;
        }

        self.decode_band(None, iq, secs, &mut out);

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
            if let Some(loc) = self.find_by_freq(g.freq_hz) {
                if self.call_talkgroup(loc) == g.talkgroup {
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
                let old = self.remove_call(loc);
                let done = self.retire(old);
                out.completed.push(done);
            }
            if self.active_count() >= self.max_calls {
                match self.contention_victim(self.priority_of(g.talkgroup)) {
                    Some(loc) => {
                        let old = self.remove_call(loc);
                        let done = self.retire(old);
                        out.completed.push(done);
                    }
                    None => {
                        out.grants_busy.push((g.talkgroup, g.freq_hz));
                        continue;
                    }
                }
            }
            // Follow the channel in whichever band covers it: the primary
            // radio's, or an extra radio parked elsewhere on the site's span.
            // A trunked system grants across its whole band, most of which a
            // single tuner cannot see — that is what the extra radios are for.
            let corrected = g.freq_hz as f64 * (1.0 + self.correction_ppm / 1e6);
            let (which, offset) = if let Some(off) = self.band.offset_of(corrected) {
                (None, off)
            } else if let Some((bi, off)) = self
                .extra
                .iter()
                .enumerate()
                .find_map(|(bi, b)| b.offset_of(corrected).map(|o| (bi, o)))
            {
                (Some(bi), off)
            } else {
                out.grants_out_of_band.push((g.talkgroup, g.freq_hz));
                continue;
            };
            let band_rate = match which {
                None => self.band.sample_rate,
                Some(bi) => self.extra[bi].sample_rate,
            };
            self.calls_started += 1;
            let dual = self.forced.is_none()
                && (!self.single_modulation
                    || self.mod_confirmed < CONFIRM_CALLS
                    || self.calls_started.is_multiple_of(REPROBE_EVERY));
            let mut call = ActiveCall {
                freq_hz: g.freq_hz,
                talkgroup: g.talkgroup,
                source_unit: g.source_unit,
                offset_hz: offset,
                // The channelizer delivers the channel at baseband, 48 kHz;
                // a classic call decimates it out of the wideband stream.
                c4fm: ChannelDecoder::with_offset(
                    if self.use_channelizer {
                        CHANNEL_RATE
                    } else {
                        band_rate
                    },
                    Modulation::C4fm,
                    EqMode::Bypass,
                    if self.use_channelizer { 0.0 } else { offset },
                ),
                cqpsk: ChannelDecoder::with_offset(
                    if self.use_channelizer {
                        CHANNEL_RATE
                    } else {
                        band_rate
                    },
                    Modulation::Cqpsk,
                    EqMode::Enabled,
                    if self.use_channelizer { 0.0 } else { offset },
                ),
                dual,
                age: 0.0,
                filter: channel_filter(),
                wideband: !self.use_channelizer,
                pcm_c4fm: Vec::new(),
                pcm_cqpsk: Vec::new(),
                syncs_c4fm: 0,
                syncs_cqpsk: 0,
                quiet: 0.0,
                ending: None,
            };
            call.c4fm.set_uv_quality(self.uv_quality);
            call.cqpsk.set_uv_quality(self.uv_quality);
            match which {
                None => self.band.active.push(call),
                Some(bi) => self.extra[bi].active.push(call),
            }
            out.started.push((g.talkgroup, g.freq_hz));
        }

        out
    }

    /// One block of one band: drop locked-out calls, slice the channelizer,
    /// run each call's decoders, retire what finished.
    fn decode_band(&mut self, which: Option<usize>, iq: &[f32], secs: f64, out: &mut FollowOutput) {
        let mut band = self.take_band(which);
        // A talkgroup locked out mid-call: stop following it now, silently —
        // the listener asked not to hear it, so it gets no audio and no row.
        if !self.lockout.is_empty() || self.allowlist.is_some() {
            let keep: Vec<bool> = band
                .active
                .iter()
                .map(|c| self.wanted(c.talkgroup))
                .collect();
            let mut i = 0;
            band.active.retain(|_| {
                i += 1;
                keep[i - 1]
            });
        }

        // Every active call is sliced out of one transform of the band
        // (classic wideband calls get the raw stream instead).
        let any_sliced = band.active.iter().any(|c| !c.wideband);
        let channels: Vec<Vec<f32>> = if band.active.is_empty() || !any_sliced {
            if let Some(ch) = band.chan.as_mut() {
                ch.reset();
                band.chan_offsets.clear();
            }
            Vec::new()
        } else {
            let offsets: Vec<f64> = band.active.iter().map(|c| c.offset_hz).collect();
            let ch = band
                .chan
                .get_or_insert_with(|| Channelizer::new(band.sample_rate, &offsets));
            if offsets != band.chan_offsets {
                ch.set_channels(&offsets);
                band.chan_offsets = offsets;
            }
            ch.process(iq)
        };
        let empty: Vec<f32> = Vec::new();
        let per_call: Vec<&[f32]> = band
            .active
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if c.wideband {
                    iq
                } else {
                    channels.get(i).map(|v| v.as_slice()).unwrap_or(&empty)
                }
            })
            .collect();
        // Each call's decoders run on their own thread for this block: the
        // channelizer has already done the shared work, so the per-call part
        // is independent and a busy site can use every core. Scoped threads
        // cost tens of microseconds per block, nothing against the decode.
        let site = self.forced.unwrap_or(self.modulation);
        let results: Vec<(crate::decoder::DecodeOutput, crate::decoder::DecodeOutput)> =
            std::thread::scope(|sc| {
                let handles: Vec<_> = band
                    .active
                    .iter_mut()
                    .zip(per_call.iter())
                    .map(|(call, samples)| {
                        let samples: &[f32] = samples;
                        sc.spawn(move || {
                            // Channel-filter a channelizer slice first (both
                            // decoders share the filtered stream); a classic
                            // wideband call filters inside its own decimator.
                            let mut filtered: Vec<f32> = Vec::new();
                            if !call.wideband {
                                filtered.reserve(samples.len());
                                for pair in samples.chunks_exact(2) {
                                    if let Some(y) =
                                        call.filter.push(hs_dsp::C32::new(pair[0], pair[1]))
                                    {
                                        filtered.push(y.re);
                                        filtered.push(y.im);
                                    }
                                }
                            }
                            let samples: &[f32] = if call.wideband { samples } else { &filtered };
                            // While the site's modulation is unconfirmed both
                            // decoders run for the whole call, and `retire`
                            // picks the winner at the end. Deciding early
                            // cannot be done on accumulated audio: CQPSK's
                            // blind carrier acquisition takes about half a
                            // second during which it emits nothing, while the
                            // C4FM discriminator locks in one frame, so early
                            // audio always favours C4FM even on a CQPSK
                            // signal. Once a couple of calls have agreed with
                            // the control channel, new calls run the site's
                            // decoder alone (halving the work), with a
                            // periodic dual call to catch a mixed site.
                            let a = if call.dual || site == Modulation::C4fm {
                                call.c4fm.process(samples)
                            } else {
                                Default::default()
                            };
                            let b = if call.dual || site == Modulation::Cqpsk {
                                call.cqpsk.process(samples)
                            } else {
                                Default::default()
                            };
                            (a, b)
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().expect("call decoder"))
                    .collect()
            });
        for (call, (a, b)) in band.active.iter_mut().zip(results) {
            call.syncs_c4fm += a.syncs;
            call.syncs_cqpsk += b.syncs;
            call.age += secs;
            // Dual on doubt: a single-decoder call that has not synced at
            // all after a second gets the other decoder too. The site rule
            // is strong but not a guarantee, and the channelizer makes the
            // second decoder cheap — far cheaper than a silent call.
            if !call.dual
                && call.age > 1.0
                && call.syncs_c4fm + call.syncs_cqpsk == 0
                && self.forced.is_none()
            {
                call.dual = true;
            }
            call.pcm_c4fm.extend_from_slice(&a.pcm);
            call.pcm_cqpsk.extend_from_slice(&b.pcm);
            for l in a.locations.iter().chain(b.locations.iter()) {
                out.locations.push((l.llid, l.lat, l.lon));
            }
            if let Some(alias) = a.talker_alias.as_ref().or(b.talker_alias.as_ref()) {
                out.talker_aliases.push((call.talkgroup, alias.clone()));
            }
            if a.syncs.max(b.syncs) == 0 {
                call.quiet += secs;
            } else {
                call.quiet = 0.0;
            }
            // A terminator ends the transmission explicitly; hold the call
            // open for a short hang in case the conversation continues, then
            // retire it. Both decoders watch the same RF, so a terminator
            // from either is the channel's own word. New voice during the
            // hang means the call carried on.
            if a.terminators + b.terminators > 0 {
                call.ending.get_or_insert(0.0);
            } else if let Some(h) = call.ending.as_mut() {
                if a.pcm.is_empty() && b.pcm.is_empty() {
                    *h += secs;
                } else {
                    call.ending = None;
                }
            }
        }

        // Retire finished calls: terminated and past the hang, or — when the
        // terminator was lost — quiet past the timeout.
        let (quiet_secs, hang_secs) = (self.quiet_secs, self.hang_secs);
        let mut finished = Vec::new();
        let mut i = 0;
        while i < band.active.len() {
            if band.active[i].quiet >= quiet_secs
                || band.active[i].ending.is_some_and(|h| h > hang_secs)
            {
                finished.push(band.active.remove(i));
            } else {
                i += 1;
            }
        }
        for c in finished {
            out.completed.push(self.retire(c));
        }

        self.put_band(which, band);
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
                self.band
                    .offset_of(hz as f64 * (1.0 + self.correction_ppm / 1e6))
                    .is_some()
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

        let offset = next_hz as f64 * (1.0 + self.correction_ppm / 1e6) - self.center_hz;
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
    measure_carrier_cancellable(iq, sample_rate, center_hz, nominal_hz, &|| false)
}

/// As [`measure_carrier`], checking `cancel()` between candidates so a live
/// front end can abandon a long sweep (tens of seconds at 10 MSPS).
pub fn measure_carrier_cancellable(
    iq: &[f32],
    sample_rate: f64,
    center_hz: f64,
    nominal_hz: f64,
    cancel: &dyn Fn() -> bool,
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
        if cancel() {
            return None;
        }
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
        if cancel() {
            return None;
        }
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
    /// freq -> seconds remaining before it may be reported again.
    seen: std::collections::HashMap<u64, f64>,
    cooldown_secs: f64,
}

impl GrantGate {
    /// `cooldown_secs` between repeat reports of the same frequency.
    pub fn new(cooldown_secs: f64) -> Self {
        Self {
            seen: std::collections::HashMap::new(),
            cooldown_secs,
        }
    }
    /// True if this frequency should be reported now (and arms its cooldown).
    pub fn fresh(&mut self, freq: u64) -> bool {
        match self.seen.get_mut(&freq) {
            Some(c) if *c > 0.0 => false,
            _ => {
                self.seen.insert(freq, self.cooldown_secs);
                true
            }
        }
    }
    /// Age the cooldowns by `secs` of processed IQ.
    pub fn tick(&mut self, secs: f64) {
        for c in self.seen.values_mut() {
            *c = (*c - secs).max(0.0);
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

#[cfg(test)]
impl TrunkFollower {
    /// A placeholder active call for contention tests (decoders idle).
    fn push_fake_call(&mut self, talkgroup: u16, freq_hz: u64) {
        let offset = freq_hz as f64 - self.center_hz;
        self.band.active.push(ActiveCall {
            freq_hz,
            talkgroup,
            source_unit: 0,
            offset_hz: offset,
            c4fm: ChannelDecoder::with_offset(CHANNEL_RATE, Modulation::C4fm, EqMode::Bypass, 0.0),
            cqpsk: ChannelDecoder::with_offset(
                CHANNEL_RATE,
                Modulation::Cqpsk,
                EqMode::Enabled,
                0.0,
            ),
            dual: true,
            age: 0.0,
            filter: channel_filter(),
            wideband: false,
            pcm_c4fm: Vec::new(),
            pcm_cqpsk: Vec::new(),
            syncs_c4fm: 0,
            syncs_cqpsk: 0,
            quiet: 0.0,
            ending: None,
        });
    }
}

#[cfg(test)]
mod priority_tests {
    use super::*;

    fn follower() -> TrunkFollower {
        TrunkFollower::new(
            2_400_000.0,
            851e6,
            851_012_500.0,
            851_012_500.0,
            Modulation::Cqpsk,
        )
    }

    /// With every slot busy, a higher-priority grant evicts the lowest-priority
    /// call; an equal- or lower-priority grant waits.
    #[test]
    fn higher_priority_grant_evicts_the_lowest_priority_call() {
        let mut f = follower();
        f.set_max_calls(2);
        f.set_priorities([(100, 10), (200, 60)]);
        f.push_fake_call(100, 851_100_000);
        f.push_fake_call(200, 851_200_000);
        // Priority 50 (unlisted) beats 60 → the TG 200 call (index 1) gives way.
        assert_eq!(f.contention_victim(50), Some((None, 1)));
        // Priority 60 ties the worst call → nobody gives way.
        assert_eq!(f.contention_victim(60), None);
        // Nothing beats priority 1's claim, but 1 itself beats everyone.
        assert_eq!(f.contention_victim(1), Some((None, 1)));
        f.set_priorities([(100, 1), (200, 1)]);
        assert_eq!(f.contention_victim(1), None);
        assert_eq!(f.contention_victim(99), None);
    }

    #[test]
    fn priorities_clamp_and_default() {
        let mut f = follower();
        f.set_priorities([(7, 0), (8, 200)]);
        assert_eq!(f.priority_of(7), 1);
        assert_eq!(f.priority_of(8), 99);
        assert_eq!(f.priority_of(9), 50);
        f.set_hang(0.0, 0.0);
        assert_eq!(f.hang(), (0.05, 0.5));
    }

    /// A call with no sync is retired after the quiet time in *seconds*,
    /// whether the IQ arrives in 100 ms blocks or 6.8 ms blocks.
    #[test]
    fn quiet_timeout_is_measured_in_seconds_not_blocks() {
        for block_secs in [0.1, 0.0068] {
            let mut f = follower();
            f.set_hang(0.3, 1.0);
            f.push_fake_call(100, 851_100_000);
            let block = vec![0.0f32; (2_400_000.0 * block_secs) as usize * 2];
            let mut fed = 0.0;
            let mut completed = 0;
            while fed < 0.9 {
                completed += f.process(&block).completed.len();
                fed += block_secs;
            }
            assert_eq!(completed, 0, "retired early at {block_secs}s blocks");
            while fed < 1.3 {
                completed += f.process(&block).completed.len();
                fed += block_secs;
            }
            assert_eq!(
                completed, 1,
                "not retired after 1 s at {block_secs}s blocks"
            );
        }
    }

    /// Ranges lock out and prioritise whole blocks of talkgroups; an explicit
    /// entry still wins over a range.
    #[test]
    fn ranges_cover_blocks_of_talkgroups() {
        let mut f = follower();
        f.set_lockout_ranges([(10000, 10999), (30000, 20000)]);
        assert!(!f.wanted(10500) && !f.wanted(25000));
        assert!(f.wanted(9999) && f.wanted(11000));
        f.set_priority_ranges([(20000, 20999, 10)]);
        f.set_priorities([(20308, 90)]);
        assert_eq!(f.priority_of(20500), 10, "range priority");
        assert_eq!(f.priority_of(20308), 90, "explicit beats range");
        assert_eq!(f.priority_of(1), 50);
    }

    #[test]
    fn site_info_starts_empty_and_names_the_control() {
        let f = follower();
        let s = f.site_info();
        assert_eq!(s.control_hz, 851_012_500);
        assert!(s.nac.is_none() && s.idens.is_empty() && s.patches.is_empty());
    }
}

#[cfg(test)]
mod capture_tests {
    use super::*;

    /// What the real NAC 0x260 control channel says about its site — the
    /// values the decoder-state panel shows. The recording is the Marion
    /// County (MESA) site: WACN 0xBEE00, system 0x262, whose Adjacent Status
    /// messages name the same system id independently, so the NetworkStatus
    /// decode is checked against ground truth, not just a round trip. This
    /// is the test that caught the WACN/system field split being wrong.
    #[test]
    fn site_info_from_the_recorded_airspy_site() {
        let path =
            std::env::var("HOME").unwrap() + "/hoosier-field/live_airspy_851M_2500k_nac260.cs16";
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!("no {path}; skipping");
            return;
        };
        let mut f32s: Vec<u8> = Vec::with_capacity(bytes.len() * 2);
        for c in bytes.chunks_exact(2) {
            let v = i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0;
            f32s.extend_from_slice(&v.to_le_bytes());
        }
        // 2.5 MSPS → the decoder's 2.4 MSPS front-end rate, as the app does.
        use hs_source::SdrSource;
        let mut src = crate::stream::Normalized::new(hs_source::IqFileSource::new(
            std::io::Cursor::new(f32s),
            2_500_000.0,
            851e6,
        ));
        let rate = src.sample_rate();
        let mut iq = Vec::new();
        let mut buf = vec![0.0f32; 1 << 17];
        while let Ok(n) = src.read(&mut buf) {
            if n == 0 {
                break;
            }
            iq.extend_from_slice(&buf[..n]);
        }
        let (measured, m) = measure_carrier(&iq, rate, 851e6, 851_537_500.0).expect("control");
        let mut f = TrunkFollower::new(rate, 851e6, 851_537_500.0, measured, m);
        for chunk in iq.chunks(rate as usize / 5) {
            f.process(chunk);
        }
        let s = f.site_info();
        eprintln!("site: {s:?}");
        eprintln!("affiliations: {}", f.affiliations().len());
        assert_eq!(s.wacn, Some(0xBEE00), "WACN from NetworkStatus");
        assert_eq!(s.sys_id, Some(0x262), "system id from NetworkStatus");
        assert!(
            s.neighbours.iter().all(|(sys, ..)| *sys == 0x262),
            "neighbours name another system: {:?}",
            s.neighbours
        );
        assert!(
            s.rfss.is_some() && s.site.is_some(),
            "RFSS/site from RfssStatus"
        );
        for (sys, r, st, hz) in &s.neighbours {
            eprintln!("neighbour sys {sys:#x} rfss {r} site {st} {hz:?}");
            if let Some(hz) = hz {
                assert!(
                    (851_000_000..=869_000_000).contains(hz),
                    "neighbour {hz} out of band"
                );
            }
        }
    }
}

#[cfg(test)]
mod load_tests {
    use super::*;

    /// Throughput with a busy site: 12 calls at once over 9.6 MSPS noise.
    /// Prints the real-time factor; run with `--release -- --ignored`.
    #[test]
    #[ignore]
    fn twelve_calls_at_once_realtime_factor() {
        let rate = 9_600_000.0;
        let mut f =
            TrunkFollower::new(rate, 855e6, 851_537_500.0, 851_537_500.0, Modulation::Cqpsk);
        f.set_max_calls(12);
        for k in 0..12u64 {
            f.push_fake_call(100 + k as u16, 852_000_000 + k * 250_000);
        }
        let secs = 2.0;
        let mut seed = 0x9E37_79B9u32;
        let block: Vec<f32> = (0..(rate * 0.1) as usize * 2)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                (seed as f32 / u32::MAX as f32 - 0.5) * 0.1
            })
            .collect();
        let start = std::time::Instant::now();
        let mut fed = 0.0;
        while fed < secs {
            f.process(&block);
            fed += 0.1;
        }
        let el = start.elapsed().as_secs_f64();
        eprintln!(
            "12 calls: {secs} s of IQ in {el:.2} s → {:.2}x real time",
            secs / el
        );
    }
}
