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
use hs_trunk::{Grant, IdenPlan, PatchTracker, SiteModel};
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

/// Output of the decoder for one processed IQ block.
#[derive(Default)]
pub struct DecodeOutput {
    /// Resolved voice grants seen this block (clear only; encrypted flagged).
    pub grants: Vec<Grant>,
    /// PCM samples (8 kHz mono i16) decoded from clear voice frames.
    pub pcm: Vec<i16>,
    /// Talkgroups skipped this block because they were encrypted.
    pub encrypted_skips: Vec<u16>,
    /// Frame-sync detections (for diagnostics / bench metrics).
    pub syncs: u32,
    /// Radio position reports decoded from packet data this block.
    pub locations: Vec<hs_p25::lrrp::LrrpReport>,
}

/// Whether the equalizer sits in the symbol path.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum EqMode {
    /// Experimental: FSW-trained real symbol-domain LMS equalizer before the
    /// slicer. Non-harmful on clean channels but does not yet beat the
    /// baseline on multipath (see hs-bench and docs/ARCHITECTURE.md §4); the
    /// complex pre-discriminator FSE is the path to the Phase 1 gate. Opt in
    /// for A/B measurement.
    Enabled,
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
            Modulation::Cqpsk if eq_mode == EqMode::Enabled => {
                Some(CqpskReceiver::new(plan.sps, 0.2))
            }
            Modulation::Cqpsk => Some(CqpskReceiver::new_bare(plan.sps, 0.2)),
            Modulation::C4fm => None,
        };
        let mut diag = crate::diag::Diagnostics::new(sample_rate, eq_mode == EqMode::Enabled);
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
            vocoder: ImbeDecoder::new(),
            raw_hist: Vec::with_capacity(48),
            fsw_levels,
            active_tg: None,
            active_enc: false,
            diag,
        }
    }

    pub fn modulation(&self) -> Modulation {
        self.modulation
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

    /// Accumulated decode diagnostics for real-signal export.
    pub fn diagnostics(&self) -> &crate::diag::Diagnostics {
        &self.diag
    }

    /// Process a slice of interleaved-IQ f32 samples.
    pub fn process(&mut self, iq: &[f32]) -> DecodeOutput {
        let mut out = DecodeOutput::default();
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
            EqMode::Bypass => sym,
        };
        // Soft-slice: keep how far the symbol sits from each decision
        // threshold, so the sync correlator and the trellis decoder can weigh
        // a marginal symbol differently from a confident one.
        let sd = hs_p25::soft::soft_slice_c4fm(eq_sym);
        debug_assert_eq!(sd.bits, slice(eq_sym));
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
            FramerEvent::Nid { nid, bch_errors } => {
                self.diag.nids.push(crate::diag::NidStat {
                    nac: nid.nac,
                    duid: nid.duid.code(),
                    bch_errors,
                });
            }
            FramerEvent::Tsdu { blocks, .. } => {
                for b in blocks {
                    self.on_tsbk(b.tsbk, out);
                }
            }
            FramerEvent::Ldu {
                imbe, algid, duid, ..
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
                // Clear voice: synthesize audio for all nine IMBE frames.
                for frame in imbe.iter() {
                    let pcm = self.vocoder.decode(frame);
                    self.diag.voice_frames += 1;
                    self.diag.pcm_samples += pcm.len() as u64;
                    out.pcm.extend_from_slice(&pcm);
                }
            }
            _ => {}
        }
    }

    fn on_tsbk(&mut self, tsbk: Tsbk, out: &mut DecodeOutput) {
        match tsbk {
            Tsbk::MotoRegroup(r) => {
                match r {
                    // A patch definition names a supergroup and its members.
                    MotoRegroup::RegroupUpdate { pairs } => {
                        for (sg, tg) in pairs {
                            self.patches.add(sg, tg);
                        }
                    }
                    // A unit operating on a patched talkgroup confirms the
                    // same association from the traffic side.
                    MotoRegroup::RegroupGrant {
                        supergroup,
                        talkgroup,
                        ..
                    } => self.patches.add(supergroup, talkgroup),
                    // The status list says which talkgroups are regrouped but
                    // not under which supergroup, so it adds no association.
                    MotoRegroup::RegroupAdd { .. } => {}
                }
                self.diag.patches = self
                    .patches
                    .patches()
                    .iter()
                    .map(|(s, m)| (*s, m.clone()))
                    .collect();
            }
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
            Tsbk::GroupVoiceGrantUpdate {
                channel_a, group_a, ..
            } => {
                if let Some(g) = self.site.resolve_grant(group_a, 0, channel_a, false) {
                    self.diag.grants.push(crate::diag::GrantStat {
                        talkgroup: g.talkgroup,
                        source_unit: g.source_unit,
                        freq_hz: g.freq_hz,
                        encrypted: g.encrypted,
                    });
                    out.grants.push(g);
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
