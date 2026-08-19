//! Dibit-stream framer: frame sync search, status-symbol removal, NID
//! decode, and per-DUID payload collection.
//!
//! Protocol facts: a status dibit is inserted every 35 payload dibits
//! (every 70 bits) counted from the start of the frame sync; the FSW itself
//! (24 dibits) is never interrupted.

use crate::nid::{Nid, NidCodec};
use crate::soft::{SoftDibit, CERTAIN};
use crate::tsbk::{self, TsbkBlock};
use crate::voice::{extract_imbe_frames, ldu2_algid_raw, ImbeFrame, LDU_PAYLOAD_BITS};
use crate::{Duid, FRAME_SYNC, FRAME_SYNC_BITS};

/// Max bit errors tolerated in the 48-bit sync correlation.
const SYNC_ERR_MAX: u32 = 2;

/// Soft-correlation threshold for frame sync, as a fraction of the total
/// confidence in the 48-bit window.
///
/// The hard correlator can only ask "how many bits differ", and must reject at
/// 3 because 3 hard errors in 48 is already close to what noise produces by
/// chance. That is a blunt rule: it discards a window with four *barely*
/// decided bits while accepting one with two confidently wrong bits, even
/// though the first is far more likely to be a real sync word.
///
/// The soft correlator weighs each disagreeing bit by how much the
/// demodulator trusted it, so a marginal symbol costs little and a confident
/// contradiction costs a lot. That lets the threshold sit where it belongs:
/// accept when the disagreeing bits account for less than this share of the
/// window's total confidence. Frames whose sync word was previously missed
/// outright — the dominant cause of dropped audio on a real capture — are
/// recovered here, without lowering the bar for noise.
const SYNC_SOFT_MAX_FRACTION: f32 = 0.16;

#[derive(Debug)]
pub enum FramerEvent {
    /// FSW just matched — the previous 24 dibits were the known sync word.
    /// hs-core uses this to train the pre-detection equalizer.
    Sync {
        bit_errors: u32,
    },
    Nid {
        nid: Nid,
        bch_errors: u32,
    },
    Tsdu {
        nac: u16,
        blocks: Vec<TsbkBlock>,
    },
    Ldu {
        nac: u16,
        duid: Duid,
        imbe: Box<[ImbeFrame; 9]>,
        /// Raw ALGID from LDU2 encryption sync (None for LDU1).
        algid: Option<u8>,
    },
    /// The undecoded Link Control slot bits from an LDU1, for studying the
    /// codes that protect them.
    LinkControlRaw {
        raw: [u8; 30],
    },
    /// Link Control from an LDU1: the call's own account of itself.
    LinkControl {
        nac: u16,
        lcw: crate::lc::Lcw,
    },
    /// A packet data unit completed: header plus reassembled payload.
    PacketData {
        nac: u16,
        packet: crate::pdu::Packet,
    },
    /// Frame type we don't decode yet — returned to sync search.
    Skipped {
        nac: u16,
        duid: Duid,
    },
}

#[derive(Clone, Copy)]
enum State {
    Search,
    Nid,
    Payload { nid: Nid, needed: usize },
}

pub struct Framer {
    state: State,
    shift: u64,
    /// Dibits since start of FSW (including status dibits) for status timing.
    since_fs: usize,
    /// Collected payload dibits (status removed), each with the
    /// demodulator's per-bit confidence so the trellis decoder can use it.
    buf: Vec<SoftDibit>,
    nid_codec: NidCodec,
    /// Per-bit confidence for the bits currently in `shift`, oldest first.
    conf: [u8; FRAME_SYNC_BITS as usize],
    /// Reassembles multi-block packet data across frames.
    pdu: crate::pdu::PduAssembler,
}

impl Default for Framer {
    fn default() -> Self {
        Self::new()
    }
}

impl Framer {
    pub fn new() -> Self {
        Self {
            state: State::Search,
            shift: 0,
            since_fs: 0,
            buf: Vec::new(),
            nid_codec: NidCodec::new(),
            conf: [CERTAIN; FRAME_SYNC_BITS as usize],
            pdu: crate::pdu::PduAssembler::new(),
        }
    }

    /// Decide whether the current window is the Frame Sync Word.
    ///
    /// Accepts on the hard rule first, so a clean sync is never rejected and
    /// behaviour on confident input is unchanged. Beyond that it falls back to
    /// the soft rule, which recovers a window whose disagreements are confined
    /// to bits the demodulator did not trust.
    fn sync_matches(&self, window: u64, errs: u32) -> bool {
        if errs <= SYNC_ERR_MAX {
            return true;
        }
        // Beyond about a sixth of the window differing, no weighting makes it
        // a sync word; bail out before doing the arithmetic.
        if errs > FRAME_SYNC_BITS / 6 {
            return false;
        }
        let diff = window ^ FRAME_SYNC;
        let mut total = 0u32;
        let mut bad = 0u32;
        for i in 0..FRAME_SYNC_BITS as usize {
            let c = self.conf[i] as u32;
            total += c;
            // Bit i of the window, counting from the oldest (MSB of the 48).
            let shift = FRAME_SYNC_BITS as usize - 1 - i;
            if (diff >> shift) & 1 != 0 {
                bad += c;
            }
        }
        total > 0 && (bad as f32) < SYNC_SOFT_MAX_FRACTION * total as f32
    }

    /// Push one sliced dibit; may emit events.
    ///
    /// Equivalent to [`Framer::push_soft`] with full confidence, so
    /// hard-decision callers behave exactly as before.
    pub fn push(&mut self, dibit: u8, events: &mut Vec<FramerEvent>) {
        self.push_soft(SoftDibit::hard(dibit), events);
    }

    /// Push one dibit with per-bit confidence; may emit events.
    pub fn push_soft(&mut self, sd: SoftDibit, events: &mut Vec<FramerEvent>) {
        let dibit = sd.bits;
        match self.state {
            State::Search => {
                self.shift = (self.shift << 2) | dibit as u64;
                self.conf.rotate_left(2);
                self.conf[FRAME_SYNC_BITS as usize - 2] = sd.conf[0];
                self.conf[FRAME_SYNC_BITS as usize - 1] = sd.conf[1];
                let window = self.shift & ((1u64 << FRAME_SYNC_BITS) - 1);
                let errs = (window ^ FRAME_SYNC).count_ones();
                if self.sync_matches(window, errs) {
                    events.push(FramerEvent::Sync { bit_errors: errs });
                    self.since_fs = 24;
                    self.buf.clear();
                    self.state = State::Nid;
                }
            }
            State::Nid => {
                if !self.status_dibit() {
                    self.buf.push(sd);
                }
                if self.buf.len() == 32 {
                    let mut w = 0u64;
                    for d in &self.buf {
                        w = (w << 2) | d.bits as u64;
                    }
                    self.buf.clear();
                    match self.nid_codec.decode(w) {
                        Some((nid, errs)) => {
                            events.push(FramerEvent::Nid {
                                nid,
                                bch_errors: errs,
                            });
                            let needed = match nid.duid {
                                Duid::TrunkSignalBlock => 98, // first block; extended as needed
                                // Packet data blocks are the same size as a
                                // TSBK block; how many follow is stated in the
                                // header, so take one block at a time.
                                Duid::PacketDataUnit => crate::pdu::BLOCK_DIBITS,
                                Duid::LogicalLinkDataUnit1 | Duid::LogicalLinkDataUnit2 => {
                                    LDU_PAYLOAD_BITS / 2
                                }
                                _ => {
                                    events.push(FramerEvent::Skipped {
                                        nac: nid.nac,
                                        duid: nid.duid,
                                    });
                                    self.state = State::Search;
                                    return;
                                }
                            };
                            self.state = State::Payload { nid, needed };
                        }
                        None => self.state = State::Search,
                    }
                }
            }
            State::Payload { nid, needed } => {
                if !self.status_dibit() {
                    self.buf.push(sd);
                }
                if self.buf.len() < needed {
                    return;
                }
                match nid.duid {
                    Duid::TrunkSignalBlock => self.tsdu_block(nid, events),
                    Duid::PacketDataUnit => self.pdu_block(nid, events),
                    Duid::LogicalLinkDataUnit1 | Duid::LogicalLinkDataUnit2 => {
                        let hard: Vec<u8> = self.buf.iter().map(|d| d.bits).collect();
                        let bits = crate::bits::dibits_to_bits(&hard);
                        if nid.duid == Duid::LogicalLinkDataUnit1 {
                            if let Some(raw) = crate::lc::raw_slots(&bits) {
                                events.push(FramerEvent::LinkControlRaw { raw });
                            }
                            if let Some(lcw) = crate::lc::extract_lcw(&bits) {
                                events.push(FramerEvent::LinkControl { nac: nid.nac, lcw });
                            }
                        }
                        if let Some(frames) = extract_imbe_frames(&bits) {
                            let algid = if nid.duid == Duid::LogicalLinkDataUnit2 {
                                ldu2_algid_raw(&bits)
                            } else {
                                None
                            };
                            events.push(FramerEvent::Ldu {
                                nac: nid.nac,
                                duid: nid.duid,
                                imbe: Box::new(frames),
                                algid,
                            });
                        }
                        self.buf.clear();
                        self.state = State::Search;
                    }
                    _ => {
                        self.buf.clear();
                        self.state = State::Search;
                    }
                }
            }
        }
    }

    /// Advance the status-symbol counter; true if this dibit is a status
    /// symbol (position ≡ 35 mod 36 from FSW start) and must be dropped.
    fn status_dibit(&mut self) -> bool {
        let is_status = self.since_fs % 36 == 35;
        self.since_fs += 1;
        is_status
    }

    /// Handle one 98-dibit packet-data block, staying in Payload state until
    /// the header's declared block count is satisfied.
    fn pdu_block(&mut self, nid: Nid, events: &mut Vec<FramerEvent>) {
        let n = crate::pdu::BLOCK_DIBITS;
        let block: [SoftDibit; crate::pdu::BLOCK_DIBITS] =
            match self.buf[self.buf.len() - n..].try_into() {
                Ok(b) => b,
                Err(_) => {
                    self.buf.clear();
                    self.state = State::Search;
                    return;
                }
            };
        match self.pdu.push_block(&block) {
            Some(packet) => {
                events.push(FramerEvent::PacketData {
                    nac: nid.nac,
                    packet,
                });
                self.buf.clear();
                self.state = State::Search;
            }
            None if self.pdu.in_progress() => {
                // Header accepted; keep collecting the blocks it promised.
                self.state = State::Payload {
                    nid,
                    needed: self.buf.len() + n,
                };
            }
            None => {
                // The header failed its CRC, or a block was undecodable.
                // Packet data is rare enough that guessing costs more than
                // waiting for the next one.
                self.pdu.reset();
                self.buf.clear();
                self.state = State::Search;
            }
        }
    }

    /// Handle a completed 98-dibit TSBK block; may extend for chained blocks.
    fn tsdu_block(&mut self, nid: Nid, events: &mut Vec<FramerEvent>) {
        // Collect blocks until last-block flag or 3 blocks or a bad decode.
        let n_blocks = self.buf.len() / 98;
        let block: &[SoftDibit] = &self.buf[(n_blocks - 1) * 98..n_blocks * 98];
        let arr: [SoftDibit; 98] = block.try_into().unwrap();
        let decoded = trellis_to_tsbk(&arr);
        let done = match &decoded {
            Some(b) => b.last_block || n_blocks >= 3,
            // An undecodable block says nothing about whether it was the
            // last one. Keep collecting to the 3-block maximum: control
            // channels run full TSDUs back-to-back, so the blocks after a
            // corrupt one are usually intact and worth decoding — giving
            // up here silently discarded them.
            None => n_blocks >= 3,
        };
        if done {
            let mut blocks = Vec::new();
            for k in 0..n_blocks {
                let arr: [SoftDibit; 98] = self.buf[k * 98..(k + 1) * 98].try_into().unwrap();
                if let Some(b) = trellis_to_tsbk(&arr) {
                    blocks.push(b);
                }
            }
            if !blocks.is_empty() {
                events.push(FramerEvent::Tsdu {
                    nac: nid.nac,
                    blocks,
                });
            }
            self.buf.clear();
            self.state = State::Search;
        } else {
            self.state = State::Payload {
                nid,
                needed: (n_blocks + 1) * 98,
            };
        }
    }
}

/// How many list-Viterbi candidates to test against the TSBK CRC when the
/// maximum-likelihood path fails it. Chosen on the Marion County
/// control-channel capture; deeper lists stopped paying past this.
const TSBK_LIST: usize = 64;

/// Only paths at most this far (in confidence units) above the ML path are
/// worth testing. A genuine near-miss sits close to the ML cost; a block
/// that is really noise has *every* path expensive, and testing 64 CRCs
/// against noise is how a false TSBK — and a false grant — gets in.
const TSBK_LIST_COST_MARGIN: u32 = 8 * CERTAIN as u32;

/// Above this ML cost the block is noise, not a near-miss; don't go fishing.
/// ≈18 confident bit errors in 196 — beyond anything the list ever recovers.
const TSBK_LIST_COST_MAX: u32 = 18 * CERTAIN as u32;

fn parse_trellis_data(data: &[u8; 12]) -> Option<TsbkBlock> {
    let mut bits = [0u8; 96];
    for (i, b) in bits.iter_mut().enumerate() {
        *b = (data[i / 8] >> (7 - i % 8)) & 1;
    }
    tsbk::parse(&bits)
}

fn trellis_to_tsbk(dibits: &[SoftDibit; 98]) -> Option<TsbkBlock> {
    // Fast path: the maximum-likelihood decode, exactly as before.
    let (data, ml_cost) = crate::trellis::decode_soft(dibits)?;
    if let Some(b) = parse_trellis_data(&data) {
        return Some(b);
    }
    if ml_cost > TSBK_LIST_COST_MAX {
        return None;
    }
    // CRC-guided list recovery: the correct codeword is usually one of the
    // next few paths — a couple of low-confidence dibits decided the other
    // way. The CRC arbitrates (a wrong candidate passes at ~2⁻¹⁶ per try),
    // and the cost bounds keep the search among genuine near-misses.
    for (data, cost) in crate::trellis::decode_list_soft(dibits, TSBK_LIST)
        .into_iter()
        .skip(1)
    {
        if cost > ml_cost + TSBK_LIST_COST_MARGIN {
            break;
        }
        if let Some(b) = parse_trellis_data(&data) {
            return Some(b);
        }
    }
    None
}
