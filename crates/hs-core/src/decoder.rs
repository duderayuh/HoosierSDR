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
use hs_dsp::equalizer::RealLmsEq;
use hs_dsp::receiver::C4fmReceiver;
use hs_dsp::C32;
use hs_p25::framer::{Framer, FramerEvent};
use hs_p25::tsbk::Tsbk;
use hs_p25::{AlgId, Duid};
use hs_trunk::{Grant, IdenPlan, SiteModel};
use hs_vocoder::imbe::ImbeDecoder;
use hs_vocoder::Vocoder;

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
    rx: C4fmReceiver,
    eq: RealLmsEq,
    eq_mode: EqMode,
    framer: Framer,
    site: SiteModel,
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
    pub fn new(sample_rate: f64, eq_mode: EqMode) -> Self {
        let fsw_levels = hs_p25::synth::sync_dibits()
            .into_iter()
            .map(hs_dsp::c4fm::dibit_to_level)
            .collect();
        Self {
            rx: C4fmReceiver::new(sample_rate),
            eq: RealLmsEq::new(7, 0.5),
            eq_mode,
            framer: Framer::new(),
            site: SiteModel::new(),
            vocoder: ImbeDecoder::new(),
            raw_hist: Vec::with_capacity(48),
            fsw_levels,
            active_tg: None,
            active_enc: false,
            diag: crate::diag::Diagnostics::new(sample_rate, eq_mode == EqMode::Enabled),
        }
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
        let mut i = 0;
        while i + 1 < iq.len() {
            let s = C32::new(iq[i], iq[i + 1]);
            i += 2;
            if let Some(sym) = self.rx.push(s) {
                self.on_symbol(sym, &mut out);
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
        let dibit = slice(eq_sym);

        // Diagnostics: symbol-level health, the cheapest demod-health window.
        self.diag.symbols_processed += 1;
        self.diag.health.observe(eq_sym, dibit);

        let mut events = Vec::new();
        self.framer.push(dibit, &mut events);
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
            Tsbk::IdenUp {
                iden,
                spacing_khz,
                tx_offset_mhz,
                base_freq_hz,
                ..
            } => {
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

    pub fn vocoder_name(&self) -> &'static str {
        self.vocoder.name()
    }
}
