//! Single-channel P25 decode pipeline: IQ samples → C4FM receiver →
//! adaptive equalizer → slicer → framer → trunking + voice events.
//!
//! This is where the project thesis is realized end to end: the equalizer
//! filters the recovered symbol stream and its output feeds the slicer
//! *before* the dibit decision, so ISI is removed ahead of detection, not
//! after. It adapts decision-directed on every symbol, which across the
//! 24-symbol Frame Sync Word amounts to training against a known reference.
//! `EqMode::Bypass` disables it for A/B comparison on the bench.

use hs_dsp::c4fm::slice;
use hs_dsp::cqpsk::CqpskReceiver;
use hs_dsp::decimate::{DecimationPlan, Decimator, TARGET_SPS};
use hs_dsp::equalizer::RealLmsEq;
use hs_dsp::receiver::C4fmReceiver;
use hs_dsp::C32;
use hs_p25::framer::{Framer, FramerEvent};
use hs_p25::moto::MotoRegroup;
use hs_p25::tsbk::Tsbk;
use hs_p25::{AlgId, Duid};
use hs_trunk::{Grant, IdenPlan, MobilityEvent, Neighbour, PatchTracker, SiteModel, SystemId};
use hs_vocoder::imbe::ImbeDecoder;
use hs_vocoder::Vocoder;

/// P25 Phase I modulation of the incoming signal.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum Modulation {
    /// C4FM (frequency): the FM-discriminator path with the symbol-domain
    /// equalizer. Used on non-simulcast sites.
    #[default]
    C4fm,
    /// CQPSK / LSM (linear, π/4-DQPSK): the coherent front end with carrier +
    /// timing recovery and the phase-blind CMA equalizer **before**
    /// differential detection. Used on simulcast sites — the project thesis.
    Cqpsk,
}

/// A composite per-voice-frame quality signal, combining sources that used
/// to live in isolation: FEC syndrome errors (mbelib's `errs2`, corrected
/// *after* the fact) and the demodulator's own amplitude-margin confidence
/// (before FEC — see `hs_p25::soft`) and carrier lock (CQPSK only). Any one
/// alone is a partial picture: a frame can pass FEC cleanly (`fec_errors` at
/// or near 0) while every symbol sat right on a decision boundary the whole
/// time (`confidence` low) — a channel visibly *about* to drop, that a purely
/// error-count-based signal only notices after it already has.
#[derive(Clone, Copy, Debug)]
pub struct VoiceQuality {
    /// Mean per-bit demodulator confidence across the frame's protected bits
    /// (`hs_p25::soft`'s 0..255 scale), normalized to 0.0..1.0.
    pub confidence: f32,
    /// mbelib's post-correction error count for this frame (`errs2`).
    pub fec_errors: u32,
    /// CQPSK carrier-lock quality at decode time (`ChannelDecoder::cqpsk_lock`),
    /// `None` on the C4FM path, which has no equivalent metric.
    pub lock: Option<f32>,
}

/// Mean per-bit confidence across an IMBE frame's real (protected or not)
/// bit positions, normalized to 0.0..1.0. Positions beyond a codeword row's
/// own width (see `hs_p25::voice::IMBE_CODEWORD_WIDTHS`) are unused storage,
/// not real bits, and must be excluded or they'd silently dilute the mean
/// with `CERTAIN`-defaulted padding that was never actually demodulated.
fn mean_confidence(conf: &hs_p25::voice::ImbeConf) -> f32 {
    use hs_p25::voice::IMBE_CODEWORD_WIDTHS;
    let mut sum = 0u64;
    let mut n = 0u64;
    for (row, &width) in conf.iter().zip(IMBE_CODEWORD_WIDTHS.iter()) {
        for &c in &row[..width] {
            sum += c as u64;
            n += 1;
        }
    }
    if n == 0 {
        return 1.0;
    }
    (sum as f32 / n as f32) / 255.0
}

impl VoiceQuality {
    /// Above this many FEC corrections a frame is graded as if it had none
    /// left to spend — matches the FEC-only threshold `voice_frames_holding`
    /// used before this existed, so the two signals agree at the edges.
    const FEC_ERROR_SATURATION: f32 = 10.0;

    /// Combine the three signals into one 0.0 (drop it) .. 1.0 (solid) score.
    /// Weighted 40% confidence / 40% FEC / 20% lock when lock is available;
    /// confidence and FEC split the lock's share evenly on C4FM (no lock
    /// metric exists there, and a missing signal should never silently
    /// downgrade every C4FM frame relative to CQPSK).
    pub fn score(&self) -> f32 {
        let fec_frac = (1.0 - self.fec_errors as f32 / Self::FEC_ERROR_SATURATION).clamp(0.0, 1.0);
        match self.lock {
            Some(lock) => 0.4 * self.confidence + 0.4 * fec_frac + 0.2 * lock.clamp(0.0, 1.0),
            None => 0.5 * self.confidence + 0.5 * fec_frac,
        }
    }
}

/// Output of the decoder for one processed IQ block.
#[derive(Default)]
pub struct DecodeOutput {
    /// Resolved voice grants seen this block (clear only; encrypted flagged).
    pub grants: Vec<Grant>,
    /// PCM samples (8 kHz mono i16) decoded from clear voice frames.
    pub pcm: Vec<i16>,
    /// Quality signal for each voice frame decoded this block, index-aligned
    /// with 160-sample chunks of `pcm` in order (one entry per IMBE frame).
    pub voice_quality: Vec<VoiceQuality>,
    /// Talkgroups skipped this block because they were encrypted.
    pub encrypted_skips: Vec<u16>,
    /// Frame-sync detections (for diagnostics / bench metrics).
    pub syncs: u32,
    /// Terminator frames (TDU) seen this block: the channel explicitly
    /// ending a transmission.
    pub terminators: u32,
    /// Radio position reports decoded from packet data this block.
    pub locations: Vec<hs_p25::lrrp::LrrpReport>,
    /// Affiliation / registration messages heard this block.
    pub mobility: Vec<MobilityEvent>,
    /// An over-the-air talker alias confirmed this block (traffic channels).
    pub talker_alias: Option<String>,
}

/// Recent decision-stage symbols kept for a constellation display.
pub const SYMBOL_RING: usize = 512;

/// Whether the equalizer sits in the symbol path.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum EqMode {
    /// Experimental: FSW-trained real symbol-domain LMS equalizer before the
    /// slicer. Non-harmful on clean channels but does not yet beat the
    /// baseline on multipath (see hs-bench and docs/ARCHITECTURE.md §4); the
    /// complex pre-discriminator FSE is the path to the Phase 1 gate. Opt in
    /// for A/B measurement.
    Enabled,
    /// CQPSK only: decision-feedback equalizer before differential detection,
    /// which cancels the deep-null simulcast echo the linear CMA leaves (see
    /// `hs_dsp::equalizer::CmaDfe`). On the C4FM path this behaves as `Bypass`.
    Dfe,
    /// Shipping default: slice the receiver output directly. This is the
    /// proven decode path.
    #[default]
    Bypass,
}

pub struct ChannelDecoder {
    modulation: Modulation,
    /// Front-end resampler: native SDR rates (240 kHz on an RTL-SDR) down to
    /// the ~10 samples/symbol the demodulators are tuned for.
    decim: Decimator,
    plan: DecimationPlan,
    rx: C4fmReceiver,
    cqpsk: Option<CqpskReceiver>,
    /// Resolves the π/2 rotation ambiguity of blind CQPSK acquisition.
    derot: crate::derotate::Derotator,
    eq: RealLmsEq,
    eq_mode: EqMode,
    framer: Framer,
    site: SiteModel,
    /// Dynamic talkgroup patches (Motorola Group Regroup).
    patches: PatchTracker,
    /// Confirms Link Control readings by repetition (they are not FEC-corrected).
    lc_confirm: hs_p25::lc::LcConfirmer,
    vocoder: ImbeDecoder,
    /// Rolling buffer of recent RAW receiver symbols (pre-equalizer), used to
    /// train the equalizer on the Frame Sync Word once the framer confirms
    /// one. Sized to hold the 24 FSW symbols plus filter context.
    raw_hist: Vec<f32>,
    fsw_levels: Vec<f32>,
    /// Talkgroup of the call currently on this channel (for voice routing).
    active_tg: Option<u16>,
    active_enc: bool,
    /// Rolling diagnostics for real-signal export (see `diag`).
    diag: crate::diag::Diagnostics,
    /// The last [`SYMBOL_RING`] decision-stage symbols: (I, Q) for CQPSK,
    /// (previous level, level) for C4FM — what a constellation view draws.
    symbols: std::collections::VecDeque<(f32, f32)>,
    prev_level: f32,
    /// Over-the-air alias words on a traffic channel.
    talker: hs_p25::talker_alias::TalkerAliasAssembler,
    /// Composite quality of the most recently decoded voice frame — see
    /// [`VoiceQuality`]. Surfaced for a live UI meter and for concealment
    /// decisions downstream (`app::player`).
    last_voice_quality: Option<VoiceQuality>,
}

impl ChannelDecoder {
    /// C4FM decoder with the symbol-domain equalizer mode. `sample_rate` is
    /// the **capture** rate; anything above ~10 samples/symbol is decimated
    /// down internally, so native SDR rates can be passed straight in.
    pub fn new(sample_rate: f64, eq_mode: EqMode) -> Self {
        Self::build(sample_rate, Modulation::C4fm, eq_mode, 0.0)
    }

    /// CQPSK / LSM decoder: carrier + timing recovery with the CMA equalizer
    /// before differential detection (simulcast sites). `sample_rate` must be
    /// an integer multiple of the 4800-baud symbol rate.
    pub fn new_cqpsk(sample_rate: f64) -> Self {
        Self::build(sample_rate, Modulation::Cqpsk, EqMode::Enabled, 0.0)
    }

    /// Decode the channel `offset_hz` away from the capture centre. A
    /// wideband capture holds many 12.5 kHz P25 channels; this selects one
    /// without re-tuning the radio, which also rescues a recording made on the
    /// wrong channel.
    pub fn with_offset(
        sample_rate: f64,
        modulation: Modulation,
        eq_mode: EqMode,
        offset_hz: f64,
    ) -> Self {
        Self::build(sample_rate, modulation, eq_mode, offset_hz)
    }

    fn build(sample_rate: f64, modulation: Modulation, eq_mode: EqMode, offset_hz: f64) -> Self {
        let fsw_levels = hs_p25::synth::sync_dibits()
            .into_iter()
            .map(hs_dsp::c4fm::dibit_to_level)
            .collect();
        let plan = DecimationPlan::for_rate(sample_rate, TARGET_SPS);
        // On the CQPSK path `EqMode` selects the thesis A/B directly: the
        // equalizer is the CMA stage ahead of differential detection, and
        // bypassing it gives exactly the conventional receiver every other
        // open-source P25 decoder implements. That makes the comparison
        // measurable on field captures, not just synthetic ones.
        let cqpsk = match modulation {
            Modulation::Cqpsk => Some(match eq_mode {
                EqMode::Enabled => CqpskReceiver::new(plan.sps, 0.2),
                EqMode::Dfe => CqpskReceiver::new_dfe(plan.sps, 0.2),
                EqMode::Bypass => CqpskReceiver::new_bare(plan.sps, 0.2),
            }),
            Modulation::C4fm => None,
        };
        let mut diag = crate::diag::Diagnostics::new(sample_rate, eq_mode != EqMode::Bypass);
        diag.modulation = modulation;
        Self {
            modulation,
            decim: Decimator::with_offset(sample_rate, TARGET_SPS, offset_hz),
            plan,
            rx: C4fmReceiver::new(plan.working_rate),
            cqpsk,
            derot: crate::derotate::Derotator::default(),
            eq: RealLmsEq::new(7, 0.5),
            eq_mode,
            framer: Framer::new(),
            site: SiteModel::new(),
            patches: PatchTracker::new(),
            lc_confirm: hs_p25::lc::LcConfirmer::new(),
            vocoder: ImbeDecoder::new(),
            raw_hist: Vec::with_capacity(48),
            fsw_levels,
            active_tg: None,
            active_enc: false,
            diag,
            symbols: std::collections::VecDeque::with_capacity(SYMBOL_RING),
            prev_level: 0.0,
            talker: hs_p25::talker_alias::TalkerAliasAssembler::new(),
            last_voice_quality: None,
        }
    }

    /// Composite quality of the most recently decoded voice frame (see
    /// [`VoiceQuality`]), `None` before the first one. Combines what used to
    /// be three separate signals — FEC error count, demodulator confidence,
    /// and (CQPSK only) carrier lock — that a UI or concealment stage would
    /// otherwise have to reconcile itself, or (as before this existed) not
    /// reconcile at all and rely on FEC error count alone.
    pub fn last_voice_quality(&self) -> Option<VoiceQuality> {
        self.last_voice_quality
    }

    /// Recent decision-stage symbols, oldest first. CQPSK: equalized (I, Q),
    /// an 8-point π/4-DQPSK ring when locked. C4FM: (previous, current)
    /// symbol level in units of the nominal ±1/±3 levels, a 16-point
    /// transition grid when the eye is open.
    pub fn recent_symbols(&self) -> impl Iterator<Item = (f32, f32)> + '_ {
        self.symbols.iter().copied()
    }

    fn push_symbol(&mut self, p: (f32, f32)) {
        if self.symbols.len() == SYMBOL_RING {
            self.symbols.pop_front();
        }
        self.symbols.push_back(p);
    }

    /// The talker alias the traffic channel broadcast for this transmission,
    /// once confirmed.
    pub fn talker_alias(&self) -> Option<&str> {
        self.talker.alias()
    }

    pub fn modulation(&self) -> Modulation {
        self.modulation
    }

    /// CQPSK carrier-lock quality in 0..1 (1 = solidly locked), once the
    /// blind acquisition has completed; 0 before it acquires. `None` on the
    /// C4FM path, which has no equivalent decision-directed lock metric.
    /// Surfaced so a live UI can show a lock meter.
    pub fn cqpsk_lock(&self) -> Option<f32> {
        self.cqpsk.as_ref().map(|r| {
            if !r.acquired() {
                0.0
            } else {
                // lock_error is bounded by ~0.39 on noise; map that to 0 and a
                // solid lock (error → 0) to 1.
                (1.0 - r.lock_error() / 0.39).clamp(0.0, 1.0)
            }
        })
    }

    /// The echo structure the CQPSK equalizer has learned — a live simulcast-
    /// distortion severity readout (see [`hs_dsp::cqpsk::EchoProfile`]).
    /// `None` on the C4FM path, with the equalizer bypassed, or before the
    /// receiver acquires.
    pub fn cqpsk_echo(&self) -> Option<hs_dsp::cqpsk::EchoProfile> {
        self.cqpsk.as_ref().and_then(|r| r.echo_profile())
    }

    /// Current signal power estimated by the AGC (dBFS).
    pub fn power_dbfs(&self) -> Option<f32> {
        // We use the baseband signal power from the CQPSK path if it's there.
        // `gain()` is the multiplier applied *to* the signal to normalize it,
        // so actual signal power is inversely proportional to AGC gain.
        self.cqpsk
            .as_ref()
            .map(|c| -10.0 * c.agc.gain().max(1e-12).log10())
    }

    /// How the capture rate is reduced before demodulation.
    pub fn decimation(&self) -> DecimationPlan {
        self.plan
    }

    /// Talkgroup patches observed so far. Traffic for a patched talkgroup can
    /// appear under any of its members, so this is needed to attribute calls
    /// correctly.
    pub fn patches(&self) -> &PatchTracker {
        &self.patches
    }

    pub fn site(&self) -> &SiteModel {
        &self.site
    }

    /// Adopt another decoder's accumulated trunking state — channel plans,
    /// system identity, secondary control channels, and talkgroup patches.
    /// Used when the control channel moves: the new channel belongs to the
    /// same site, and waiting for the plans to be re-broadcast would drop
    /// every grant issued in between.
    pub fn adopt_trunk_state(&mut self, other: &ChannelDecoder) {
        self.site = other.site.clone();
        self.patches = other.patches.clone();
    }

    /// Accumulated decode diagnostics for real-signal export.
    pub fn diagnostics(&self) -> &crate::diag::Diagnostics {
        &self.diag
    }

    /// Process a slice of interleaved-IQ f32 samples.
    pub fn process(&mut self, iq: &[f32]) -> DecodeOutput {
        let mut out = DecodeOutput::default();
        self.diag.trim(50_000);
        let mut derot_buf: Vec<u8> = Vec::new();
        let mut i = 0;
        while i + 1 < iq.len() {
            let s = C32::new(iq[i], iq[i + 1]);
            i += 2;
            // Resample to the working rate first; at 1× this is a no-op.
            let Some(s) = self.decim.push(s) else {
                continue;
            };
            match self.modulation {
                Modulation::C4fm => {
                    if let Some(sym) = self.rx.push(s) {
                        self.on_symbol(sym, &mut out);
                    }
                }
                Modulation::Cqpsk => {
                    // The CQPSK front end (DC block → AGC → matched filter →
                    // Gardner → CMA → differential detection) emits dibits
                    // directly, but blind carrier acquisition leaves them
                    // rotated by an unknown quarter turn; the derotator pins
                    // that against the Frame Sync Word before the framer.
                    if let Some((raw, dphi)) = self.cqpsk.as_mut().unwrap().push_phase(s) {
                        if let Some(sym) = self.cqpsk.as_ref().unwrap().last_symbol() {
                            self.push_symbol((sym.re, sym.im));
                        }
                        derot_buf.clear();
                        self.derot.push(raw, &mut derot_buf);
                        // Confidence comes from the differential phase's
                        // distance to its decision boundaries. Derotation
                        // permutes which dibit is meant but not how well the
                        // symbol was resolved, so the confidences carry over.
                        let conf = hs_p25::soft::soft_slice_cqpsk(dphi).conf;
                        for &d in &derot_buf {
                            self.feed_dibit(hs_p25::soft::SoftDibit::new(d, conf), None, &mut out);
                        }
                    }
                }
            }
        }
        out
    }

    fn on_symbol(&mut self, sym: f32, out: &mut DecodeOutput) {
        // Buffer the raw (pre-equalizer) symbol so we can train on the Frame
        // Sync Word once the framer confirms one.
        if self.eq_mode == EqMode::Enabled {
            self.raw_hist.push(sym);
            if self.raw_hist.len() > 40 {
                self.raw_hist.remove(0);
            }
        }

        // Equalize (or bypass) to get the decision-stage symbol. In Enabled
        // mode the equalizer filters the raw symbol stream and its output —
        // not the raw sample — is what the slicer decides on. This is the
        // project thesis in one line: ISI removal ahead of detection. The
        // equalizer is FROZEN between syncs (never adapts on its own
        // decisions), so it cannot cold-start into instability; it only
        // updates on the known FSW via train_sequence() below.
        let eq_sym = match self.eq_mode {
            EqMode::Enabled => self.eq.push(sym),
            // The DFE is a complex pre-differential-detection stage on the
            // CQPSK path; on this real-symbol C4FM path there is nothing for it
            // to do, so it slices the receiver output directly like Bypass.
            EqMode::Dfe | EqMode::Bypass => sym,
        };
        // Soft-slice: keep how far the symbol sits from each decision
        // threshold, so the sync correlator and the trellis decoder can weigh
        // a marginal symbol differently from a confident one.
        let sd = hs_p25::soft::soft_slice_c4fm(eq_sym);
        debug_assert_eq!(sd.bits, slice(eq_sym));
        self.push_symbol((self.prev_level, eq_sym));
        self.prev_level = eq_sym;
        self.feed_dibit(sd, Some(eq_sym), out);
    }

    /// Shared dibit-domain path: diagnostics + framer + event handling. Both
    /// front ends (C4FM symbol slicing, CQPSK differential detection) converge
    /// here. `soft` is the real soft-symbol value for eye diagnostics when the
    /// front end has one (C4FM); the CQPSK path passes None.
    fn feed_dibit(
        &mut self,
        sd: hs_p25::soft::SoftDibit,
        soft: Option<f32>,
        out: &mut DecodeOutput,
    ) {
        self.diag.symbols_processed += 1;
        if let Some(s) = soft {
            self.diag.health.observe(s, sd.bits);
        }
        let mut events = Vec::new();
        self.framer.push_soft(sd, &mut events);
        for ev in events {
            self.on_event(ev, out);
        }
    }

    fn on_event(&mut self, ev: FramerEvent, out: &mut DecodeOutput) {
        match ev {
            FramerEvent::Sync { bit_errors } => {
                out.syncs += 1;
                self.diag.syncs.push(crate::diag::SyncStat {
                    at_symbol: self.diag.symbols_processed,
                    bit_errors,
                });
                // Train the equalizer on the FSW we just decoded. The framer
                // syncs on the EQUALIZED stream, which lags the raw symbols by
                // the equalizer's group delay `c`, so the raw FSW symbols end
                // `c` samples back from the tail of raw_hist. Slice with that
                // offset and prepend `ctx` context symbols to prime the filter.
                let c = self.eq.delay_syms();
                let ctx = 6;
                let need = 24 + ctx + c;
                if self.eq_mode == EqMode::Enabled && self.raw_hist.len() >= need {
                    let n = self.raw_hist.len();
                    let end = n - c; // one past the last raw FSW symbol
                    let start = end - 24 - ctx;
                    let raw = &self.raw_hist[start..end];
                    let mut desired = vec![f32::NAN; ctx]; // no ground truth
                    desired.extend_from_slice(&self.fsw_levels);
                    self.eq.train_sequence(raw, &desired);
                }
            }
            FramerEvent::PacketData { packet, .. } => {
                self.diag.packets += 1;
                // Packet data carries IP; a location report is one particular
                // UDP payload inside it. Anything else — and anything
                // encrypted — simply fails to parse and is dropped.
                if let Some(r) =
                    hs_p25::lrrp::report_from_packet(packet.header.llid, &packet.payload)
                {
                    self.diag.locations.push(crate::diag::LocationStat {
                        llid: r.llid,
                        lat: r.lat,
                        lon: r.lon,
                    });
                    out.locations.push(r);
                }
            }
            FramerEvent::LinkControlRaw { raw } => {
                if self.diag.lc_raw.len() < 4000 {
                    self.diag.lc_raw.push(raw);
                }
            }
            FramerEvent::LinkControl { lcw, .. } => {
                // A voice channel naming its own call: this is what lets a
                // traffic channel be identified without the control channel.
                if let Some((tg, src)) = self.lc_confirm.observe(&lcw) {
                    self.diag.link_control.push(crate::diag::LcStat {
                        talkgroup: tg,
                        source_unit: src,
                        emergency: lcw.emergency(),
                    });
                    self.active_tg = Some(tg);
                } else if !lcw.is_standard() {
                    if let Some(alias) = self.talker.observe(&lcw) {
                        self.diag
                            .talker_aliases
                            .push((self.active_tg.unwrap_or(0), alias.clone()));
                        out.talker_alias = Some(alias);
                    }
                    let key = (lcw.mfid, lcw.lco);
                    match self
                        .diag
                        .vendor_lc
                        .iter_mut()
                        .find(|(m, o, _)| (*m, *o) == key)
                    {
                        Some((_, _, n)) => *n += 1,
                        None => self.diag.vendor_lc.push((lcw.mfid, lcw.lco, 1)),
                    }
                    if self.diag.vendor_lc_samples.len() < 64 {
                        self.diag
                            .vendor_lc_samples
                            .push((lcw.mfid, lcw.lco, lcw.args));
                    }
                }
            }
            FramerEvent::Nid { nid, bch_errors } => {
                self.diag.nids.push(crate::diag::NidStat {
                    nac: nid.nac,
                    duid: nid.duid.code(),
                    bch_errors,
                });
            }
            FramerEvent::Tsdu { blocks, .. } => {
                self.diag.tsbks += blocks.len() as u64;
                for b in blocks {
                    self.on_tsbk(b.tsbk, out);
                }
            }
            FramerEvent::Ldu {
                imbe, conf, algid, duid, ..
            } => {
                let encrypted = match duid {
                    Duid::LogicalLinkDataUnit2 => algid
                        .map(|a| !AlgId::from(a).is_decodable())
                        .unwrap_or(false),
                    _ => self.active_enc,
                };
                if encrypted {
                    if let Some(tg) = self.active_tg {
                        out.encrypted_skips.push(tg);
                        self.diag.encrypted_skips.push(tg);
                    }
                    self.active_enc = true;
                    return;
                }
                // Clear voice: synthesize audio for all nine IMBE frames,
                // using the demodulator's per-bit confidence (amplitude
                // margin from the decision boundary — see hs_p25::soft) to
                // guide FEC correction ahead of the vocoder, rather than
                // handing it hard-sliced bits alone.
                for (frame, frame_conf) in imbe.iter().zip(conf.iter()) {
                    let pcm = self.vocoder.decode_soft(frame, frame_conf);
                    self.diag.voice_frames += 1;
                    self.diag.pcm_samples += pcm.len() as u64;
                    let errs = self.vocoder.last_errs.max(0) as u32;
                    self.diag.voice_frame_errors += errs as u64;
                    if errs > 5 {
                        self.diag.voice_frames_holding += 1;
                    }
                    if errs > self.diag.voice_error_max {
                        self.diag.voice_error_max = errs;
                    }
                    let quality = VoiceQuality {
                        confidence: mean_confidence(frame_conf),
                        fec_errors: errs,
                        lock: self.cqpsk_lock(),
                    };
                    self.diag.record_voice_quality(quality);
                    self.last_voice_quality = Some(quality);
                    out.voice_quality.push(quality);
                    out.pcm.extend_from_slice(&pcm);
                }
            }
            FramerEvent::Skipped {
                duid: Duid::TerminatorNoLc | Duid::TerminatorWithLc,
                ..
            } => {
                // The channel saying its transmission is over. The frame
                // carries nothing to decode (the with-LC variant's link
                // control repeats what LDU1 already said), but the *event*
                // matters: it is the explicit end of a transmission, seconds
                // sooner than a quiet-channel timeout can conclude the same.
                out.terminators += 1;
                self.active_tg = None;
                self.active_enc = false;
            }
            _ => {}
        }
    }

    fn on_tsbk(&mut self, tsbk: Tsbk, out: &mut DecodeOutput) {
        match tsbk {
            // Motorola Group Regroup: a patch (supergroup) call's voice
            // channel is announced *only* here — the standard grant never
            // names a supergroup. These must start calls exactly like the
            // standard messages they mirror, or every patch call on the
            // system is silently skipped (a metro county's NORTH/SOUTH
            // dispatch supergroups are granted exclusively this way).
            Tsbk::MotoRegroup(r) => match r {
                MotoRegroup::GrgChannelGrant {
                    opts,
                    channel,
                    supergroup,
                    source_unit,
                } => {
                    let encrypted = opts & 0x40 != 0; // 'E' bit, as standard
                    if let Some(g) =
                        self.site
                            .resolve_grant(supergroup, source_unit, channel, encrypted)
                    {
                        self.active_tg = Some(supergroup);
                        self.active_enc = encrypted;
                        if encrypted {
                            out.encrypted_skips.push(supergroup);
                        }
                        self.diag.grants.push(crate::diag::GrantStat {
                            talkgroup: g.talkgroup,
                            source_unit: g.source_unit,
                            freq_hz: g.freq_hz,
                            encrypted: g.encrypted,
                        });
                        out.grants.push(g);
                    }
                }
                MotoRegroup::GrgChannelUpdate { pairs } => {
                    // A padded slot is normalized to (0, 0) by the parser.
                    for (channel, supergroup) in pairs.into_iter().filter(|&(ch, _)| ch != 0) {
                        if let Some(g) = self.site.resolve_grant(supergroup, 0, channel, false) {
                            self.diag.grants.push(crate::diag::GrantStat {
                                talkgroup: g.talkgroup,
                                source_unit: g.source_unit,
                                freq_hz: g.freq_hz,
                                encrypted: g.encrypted,
                            });
                            out.grants.push(g);
                        }
                    }
                }
                // Four talkgroup IDs with no channel and no confirmed
                // supergroup position: records no association (see hs-p25
                // moto.rs provenance notes).
                MotoRegroup::RegroupAdd { .. } => {}
            },
            Tsbk::VendorSpecific { mfid, opcode, args } => {
                // A few raw examples per vendor opcode are enough to work out
                // its structure offline; the counts above carry the rest.
                if self.diag.vendor_samples.len() < 64 {
                    self.diag.vendor_samples.push((mfid, opcode, args));
                }
                match self
                    .diag
                    .vendor_tsbks
                    .iter_mut()
                    .find(|(m, o, _)| *m == mfid && *o == opcode)
                {
                    Some((_, _, n)) => *n += 1,
                    None => self.diag.vendor_tsbks.push((mfid, opcode, 1)),
                }
            }
            Tsbk::IdenUp {
                iden,
                spacing_khz,
                tx_offset_mhz,
                base_freq_hz,
                ..
            } => {
                if !self.diag.idens.iter().any(|(i, _, _)| *i == iden) {
                    self.diag
                        .idens
                        .push((iden, base_freq_hz, (spacing_khz * 1000.0) as u64));
                }
                self.site.set_iden(
                    iden,
                    IdenPlan {
                        base_freq_hz,
                        spacing_hz: (spacing_khz * 1000.0) as u64,
                        tx_offset_hz: (tx_offset_mhz * 1_000_000.0) as i64,
                    },
                );
            }
            Tsbk::SecondaryControl {
                channel_a,
                channel_b,
                ..
            } => {
                // The site naming its alternate control channels. Kept in the
                // site model so a follower that loses this channel knows where
                // the control channel can reappear. A zero channel is an
                // unused slot in the broadcast, not channel 0 of IDEN 0.
                for ch in [channel_a, channel_b] {
                    if ch != 0 {
                        self.site.add_secondary_cc(ch);
                    }
                }
            }
            Tsbk::GroupVoiceGrant {
                opts,
                channel,
                group,
                source,
            } => {
                let encrypted = opts & 0x40 != 0; // 'E' bit in service options
                if let Some(g) = self.site.resolve_grant(group, source, channel, encrypted) {
                    self.active_tg = Some(group);
                    self.active_enc = encrypted;
                    if encrypted {
                        out.encrypted_skips.push(group);
                    }
                    self.diag.grants.push(crate::diag::GrantStat {
                        talkgroup: g.talkgroup,
                        source_unit: g.source_unit,
                        freq_hz: g.freq_hz,
                        encrypted: g.encrypted,
                    });
                    out.grants.push(g);
                }
            }
            Tsbk::NetworkStatus { wacn, sys_id, .. } => {
                self.site.set_system(SystemId { wacn, sys_id });
            }
            Tsbk::RfssStatus { rfss, site, .. } => {
                self.site.set_rfss_site(rfss, site);
            }
            Tsbk::AdjacentStatus {
                sys_id,
                rfss,
                site,
                channel,
                ..
            } => {
                self.site.add_neighbour(Neighbour {
                    sys_id,
                    rfss,
                    site,
                    channel,
                });
            }
            Tsbk::GroupAffiliationResponse {
                accepted,
                group,
                target,
                ..
            } => out.mobility.push(MobilityEvent::Affiliated {
                unit: target,
                group,
                accepted,
            }),
            Tsbk::UnitRegistrationResponse {
                status, source_id, ..
            } => out.mobility.push(MobilityEvent::Registered {
                unit: source_id,
                status,
            }),
            Tsbk::LocationRegistrationResponse {
                status,
                group,
                target,
                ..
            } => {
                if status == 0 {
                    out.mobility.push(MobilityEvent::Located {
                        unit: target,
                        group,
                    });
                }
            }
            Tsbk::DeregistrationAck { source_id, .. } => {
                out.mobility
                    .push(MobilityEvent::Deregistered { unit: source_id });
            }
            Tsbk::GroupVoiceGrantUpdate {
                channel_a,
                group_a,
                channel_b,
                group_b,
            } => {
                // Both slots are grants; an unused B slot is zeros (or a
                // repeat of A, which the follower already treats as the same
                // call). Dropping B silently skipped every second announced
                // call.
                for (channel, group) in [(channel_a, group_a), (channel_b, group_b)] {
                    if channel == 0 || group == 0 || group == 0xFFFF {
                        continue;
                    }
                    if let Some(g) = self.site.resolve_grant(group, 0, channel, false) {
                        self.diag.grants.push(crate::diag::GrantStat {
                            talkgroup: g.talkgroup,
                            source_unit: g.source_unit,
                            freq_hz: g.freq_hz,
                            encrypted: g.encrypted,
                        });
                        out.grants.push(g);
                    }
                }
            }
            _ => {}
        }
    }

    /// Unvoiced synthesis quality passed to the vocoder (1-64). Higher gives
    /// fricatives more high-frequency detail; it changes only how decoded
    /// parameters are rendered to audio, never what was decoded.
    pub fn set_uv_quality(&mut self, q: i32) {
        self.vocoder.set_uv_quality(q);
    }

    pub fn vocoder_name(&self) -> &'static str {
        self.vocoder.name()
    }
}

#[cfg(test)]
mod voice_quality_tests {
    use super::*;
    use hs_p25::soft::CERTAIN;

    fn conf_at(level: u8) -> hs_p25::voice::ImbeConf {
        [[level; 23]; 8]
    }

    #[test]
    fn mean_confidence_ignores_padding_beyond_each_codewords_real_width() {
        // Real bits certain, padding beyond each row's width set to 0 (the
        // lowest possible confidence) — if padding leaked into the mean it
        // would pull this well under 1.0.
        let mut conf = conf_at(CERTAIN);
        for (row, &width) in conf.iter_mut().zip(hs_p25::voice::IMBE_CODEWORD_WIDTHS.iter()) {
            for c in &mut row[width..] {
                *c = 0;
            }
        }
        assert_eq!(mean_confidence(&conf), 1.0);
    }

    #[test]
    fn mean_confidence_is_zero_for_a_totally_uncertain_frame() {
        assert_eq!(mean_confidence(&conf_at(0)), 0.0);
    }

    #[test]
    fn score_rewards_confidence_fec_and_lock_independently() {
        let clean = VoiceQuality {
            confidence: 1.0,
            fec_errors: 0,
            lock: Some(1.0),
        };
        assert_eq!(clean.score(), 1.0);

        let low_confidence = VoiceQuality {
            confidence: 0.0,
            ..clean
        };
        let high_fec = VoiceQuality {
            fec_errors: 20,
            ..clean
        };
        let no_lock = VoiceQuality {
            lock: Some(0.0),
            ..clean
        };
        // Each degraded signal, alone, must pull the score down from a
        // perfect frame — a signal that never moves the score would be
        // dead weight in the formula, exactly the "ignoring amplitude" bug
        // this replaces (confidence used to be exactly such dead weight).
        assert!(low_confidence.score() < clean.score());
        assert!(high_fec.score() < clean.score());
        assert!(no_lock.score() < clean.score());
    }

    #[test]
    fn score_does_not_penalize_c4fm_for_lacking_a_lock_metric() {
        // No lock signal exists on C4FM; a clean C4FM frame must still score
        // a perfect 1.0, not be capped below CQPSK for missing a metric that
        // doesn't apply to it.
        let c4fm_clean = VoiceQuality {
            confidence: 1.0,
            fec_errors: 0,
            lock: None,
        };
        assert_eq!(c4fm_clean.score(), 1.0);
    }

    #[test]
    fn record_voice_quality_tracks_a_running_mean_and_low_quality_count() {
        let mut diag = crate::diag::Diagnostics::new(48_000.0, false);
        // Frame 1: perfect.
        diag.voice_frames = 1;
        diag.record_voice_quality(VoiceQuality {
            confidence: 1.0,
            fec_errors: 0,
            lock: None,
        });
        assert_eq!(diag.mean_voice_quality(), 1.0);
        assert_eq!(diag.voice_frames_low_quality, 0);

        // Frame 2: bad enough to count as low-quality even though FEC alone
        // (fec_errors: 1, barely nonzero) would call this frame nearly clean.
        diag.voice_frames = 2;
        diag.record_voice_quality(VoiceQuality {
            confidence: 0.0,
            fec_errors: 1,
            lock: None,
        });
        assert_eq!(diag.voice_frames_low_quality, 1);
        assert!(diag.mean_voice_quality() < 1.0);
    }
}
