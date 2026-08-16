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
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EqMode {
    /// Thesis mode: adaptive LMS equalizer before the slicer.
    Enabled,
    /// Baseline mode: slice the receiver output directly (for A/B on bench).
    Bypass,
}

pub struct ChannelDecoder {
    rx: C4fmReceiver,
    eq: RealLmsEq,
    eq_mode: EqMode,
    framer: Framer,
    site: SiteModel,
    vocoder: ImbeDecoder,
    /// Talkgroup of the call currently on this channel (for voice routing).
    active_tg: Option<u16>,
    active_enc: bool,
}

impl ChannelDecoder {
    pub fn new(sample_rate: f64, eq_mode: EqMode) -> Self {
        Self {
            rx: C4fmReceiver::new(sample_rate),
            eq: RealLmsEq::new(7, 0.005),
            eq_mode,
            framer: Framer::new(),
            site: SiteModel::new(),
            vocoder: ImbeDecoder::new(),
            active_tg: None,
            active_enc: false,
        }
    }

    pub fn site(&self) -> &SiteModel {
        &self.site
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
        // Equalize (or bypass) to get the decision-stage symbol. In Enabled
        // mode the equalizer filters the raw symbol stream and its output —
        // not the raw sample — is what the slicer decides on. This is the
        // project thesis in one line: ISI removal ahead of detection.
        let eq_sym = match self.eq_mode {
            EqMode::Enabled => self.eq.push(sym),
            EqMode::Bypass => sym,
        };
        let dibit = slice(eq_sym);

        // Decision-directed LMS: adapt taps toward the nominal level of the
        // decided symbol, using the live delay line already advanced by
        // push() — no replay, no corruption of the sample stream. Across the
        // 24-symbol FSW the decisions ARE the known reference, so this is
        // exactly sync-anchored training during those symbols and DD-LMS
        // elsewhere; both keep the equalizer ahead of the slicer.
        if self.eq_mode == EqMode::Enabled {
            let target = hs_dsp::c4fm::dibit_to_level(dibit);
            self.eq.train(target);
        }

        let mut events = Vec::new();
        self.framer.push(dibit, &mut events);
        for ev in events {
            self.on_event(ev, out);
        }
    }

    fn on_event(&mut self, ev: FramerEvent, out: &mut DecodeOutput) {
        match ev {
            FramerEvent::Sync { .. } => {
                out.syncs += 1;
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
                    }
                    self.active_enc = true;
                    return;
                }
                // Clear voice: synthesize audio for all nine IMBE frames.
                for frame in imbe.iter() {
                    out.pcm.extend_from_slice(&self.vocoder.decode(frame));
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
                    out.grants.push(g);
                }
            }
            Tsbk::GroupVoiceGrantUpdate {
                channel_a, group_a, ..
            } => {
                if let Some(g) = self.site.resolve_grant(group_a, 0, channel_a, false) {
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
