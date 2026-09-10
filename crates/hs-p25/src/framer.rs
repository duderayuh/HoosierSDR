//! Dibit-stream framer: frame sync search, status-symbol removal, NID
//! decode, and per-DUID payload collection.
//!
//! Protocol facts: a status dibit is inserted every 35 payload dibits
//! (every 70 bits) counted from the start of the frame sync; the FSW itself
//! (24 dibits) is never interrupted.

use crate::ess::EssDecode;
use crate::nid::{Nid, NidCodec};
use crate::soft::{SoftDibit, CERTAIN};
use crate::tsbk::{self, TsbkBlock};
use crate::voice::{extract_imbe_conf, extract_imbe_frames, ImbeConf, ImbeFrame, LDU_PAYLOAD_BITS};
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

/// Dibits from the end of a fully-consumed `Payload` to the next FSW. P25
/// frames run back-to-back with no gap, so this is exactly the 24-dibit FSW
/// length itself: the very next window that fills is where the next sync
/// word is protocol-guaranteed to start.
const FLYWHEEL_COAST_DIBITS: u32 = FRAME_SYNC_BITS / 2;

/// Relaxed sync thresholds used for exactly one check: the single
/// protocol-predicted position after `FLYWHEEL_COAST_DIBITS` (see
/// `arm_flywheel`/`sync_matches_coast`). Looser than [`SYNC_ERR_MAX`]/
/// [`SYNC_SOFT_MAX_FRACTION`] because, unlike a cold search over an
/// arbitrary bit offset, this position carries independent evidence: we know
/// a real frame of the exact declared length just ended here, and protocol
/// timing (not correlation) says the FSW is at this exact spot.
///
/// That evidence bounds the *false-accept rate*, not the *cost* of a false
/// accept: a misaligned accept here does not always fail cleanly downstream.
/// During development a coast accept at the wrong offset let a garbled NID
/// pass its BCH check with correctable errors and produced a bogus decode
/// rather than an obvious rejection — this is a real residual risk of
/// widening the tolerance, not a self-healing one. It's accepted here
/// because it is bounded to one check per coasted gap (not a search) and the
/// thresholds were tightened until real-capture TSBK/grant yield matched the
/// pre-flywheel baseline (see the commit history for the measurements).
const SYNC_ERR_MAX_COAST: u32 = 6;
const SYNC_SOFT_MAX_FRACTION_COAST: f32 = 0.35;

/// Consecutive voice frames the framer will *assume* at the protocol cadence
/// when the sync word or the NID behind it is lost mid-transmission.
///
/// P25 voice runs LDU1, LDU2, LDU1, LDU2 … back to back, every frame the
/// same length, on one NAC. Once that pattern is established, a missed sync
/// or an uncorrectable NID says nothing about the nine IMBE frames that
/// follow — each carries its own Golay/Hamming protection and the vocoder
/// judges them individually. Returning to a cold search instead threw away
/// 180 ms of audio per miss, on a signal where the miss itself was the only
/// thing wrong. So the framer coasts: it presumes the frame is there, infers
/// its type from the alternation, and lets the voice FEC decide.
///
/// Bounded, because a coast past the real end of a transmission collects
/// noise as voice. Two frames (360 ms) bridges the fades that matter; a
/// longer outage is a genuine loss, and reported as a gap instead.
const MAX_COAST_FRAMES: u32 = 2;

/// Clean NID decodes on one NAC before it is trusted as the channel's own,
/// which a coasted frame is then attributed to.
const NAC_TRACK_MIN: u32 = 3;

/// Dibits per LDU on the wire (sync + NID + payload + status symbols).
pub const LDU_WIRE_DIBITS: u32 = 864;

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
        /// Per-bit demodulator confidence for `imbe`, index-aligned
        /// (`conf[k][w][x]` is the confidence of `imbe[k][w][x]`). Lets a
        /// downstream soft-decision FEC pass (see `hs_vocoder::imbe`) use
        /// amplitude information the hard-sliced `imbe` bits alone discard.
        conf: Box<[ImbeConf; 9]>,
        /// The Encryption Sync, decoded through its Hamming and
        /// Reed–Solomon protection (LDU2 only; `None` for LDU1).
        ess: Option<EssDecode>,
        /// The frame's sync word or NID was lost and its type and NAC were
        /// inferred from the voice cadence (see [`MAX_COAST_FRAMES`]).
        inferred: bool,
    },
    /// A real sync word arrived after a stretch of unframed dibits: the
    /// channel was on the air but nothing was decoded for `dibits` symbols
    /// beyond the normal frame spacing. A voice consumer can turn this into
    /// the time that went missing, which a stream of only decoded frames
    /// would otherwise silently compress.
    Gap {
        dibits: u32,
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
    /// Collecting the NID. `presumed` means no sync word was actually seen:
    /// the flywheel said one was due and the voice cadence makes it likely.
    Nid {
        presumed: bool,
    },
    /// Collecting a payload. `inferred` means the NID was lost and the
    /// frame's identity is a guess from the voice cadence.
    Payload {
        nid: Nid,
        needed: usize,
        inferred: bool,
    },
}

/// Debug-only running dibit count, so HS_TSDU_DEBUG lines carry a timeline.
static DIBIT_CLOCK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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
    /// Countdown to the next protocol-predicted FSW position, or `None` for
    /// an ordinary cold search. Armed whenever `Payload` state exits having
    /// consumed its full declared length — see `arm_flywheel`.
    flywheel: Option<u32>,
    /// The last frame type decoded, which a coasted frame's type is inferred
    /// from (LDU1 ↔ LDU2).
    last_duid: Option<Duid>,
    /// The NAC seen on recent clean NIDs and how many times in a row; trusted
    /// once it reaches `NAC_TRACK_MIN`.
    nac_seen: Option<(u16, u32)>,
    /// Coast budget remaining before a lost frame becomes a gap.
    coast_left: u32,
    /// Raw dibits since the last frame ended, for gap reporting.
    since_payload: u32,
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
            flywheel: None,
            last_duid: None,
            nac_seen: None,
            coast_left: MAX_COAST_FRAMES,
            since_payload: 0,
        }
    }

    /// The channel's NAC, once enough clean NIDs have agreed on it.
    pub fn tracked_nac(&self) -> Option<u16> {
        self.nac_seen
            .filter(|&(_, n)| n >= NAC_TRACK_MIN)
            .map(|(nac, _)| nac)
    }

    fn track_nac(&mut self, nac: u16) {
        self.nac_seen = Some(match self.nac_seen {
            Some((seen, n)) if seen == nac => (nac, n.saturating_add(1)),
            _ => (nac, 1),
        });
    }

    /// Whether a lost sync or NID may be bridged by assuming the next voice
    /// frame: only mid-voice, on a known NAC, within the coast budget.
    fn can_coast(&self) -> bool {
        matches!(
            self.last_duid,
            Some(Duid::LogicalLinkDataUnit1 | Duid::LogicalLinkDataUnit2)
        ) && self.tracked_nac().is_some()
            && self.coast_left > 0
    }

    /// The voice frame that follows `last_duid` in the LDU1/LDU2 cadence.
    fn next_voice_duid(&self) -> Duid {
        match self.last_duid {
            Some(Duid::LogicalLinkDataUnit1) => Duid::LogicalLinkDataUnit2,
            _ => Duid::LogicalLinkDataUnit1,
        }
    }

    /// Note a frame ending: the flywheel is armed for the next sync and the
    /// gap clock restarts.
    fn frame_ended(&mut self, duid: Duid) {
        self.last_duid = Some(duid);
        self.since_payload = 0;
        self.arm_flywheel();
        self.state = State::Search;
    }

    /// Slide the sync shift register by one dibit.
    fn shift_in(&mut self, sd: SoftDibit) -> (u64, u32) {
        self.shift = (self.shift << 2) | sd.bits as u64;
        self.conf.rotate_left(2);
        self.conf[FRAME_SYNC_BITS as usize - 2] = sd.conf[0];
        self.conf[FRAME_SYNC_BITS as usize - 1] = sd.conf[1];
        let window = self.shift & ((1u64 << FRAME_SYNC_BITS) - 1);
        (window, (window ^ FRAME_SYNC).count_ones())
    }

    /// A real sync word just filled the shift register: report any stretch
    /// that went unframed before it, then start on the NID.
    fn on_real_sync(&mut self, errs: u32, events: &mut Vec<FramerEvent>) {
        // The sync word ends `FLYWHEEL_COAST_DIBITS` (plus at most one status
        // dibit) after the previous frame; anything beyond a status period
        // more than that was air nothing decoded from.
        let expected = FLYWHEEL_COAST_DIBITS + 1;
        if self.since_payload > expected + 36 {
            events.push(FramerEvent::Gap {
                dibits: self.since_payload - expected,
            });
        }
        events.push(FramerEvent::Sync { bit_errors: errs });
        self.since_fs = 24;
        self.buf.clear();
        self.state = State::Nid { presumed: false };
    }

    /// Decide whether the current window is the Frame Sync Word.
    ///
    /// Accepts on the hard rule first, so a clean sync is never rejected and
    /// behaviour on confident input is unchanged. Beyond that it falls back to
    /// the soft rule, which recovers a window whose disagreements are confined
    /// to bits the demodulator did not trust.
    fn sync_matches(&self, window: u64, errs: u32) -> bool {
        // Bail bound kept exactly as it was before the flywheel existed: a
        // cold, no-information search sees this position and only this
        // position, so its bail bound must not move just because a
        // *different* caller (`sync_matches_coast`) needs a looser one.
        self.sync_matches_at(
            window,
            errs,
            SYNC_ERR_MAX,
            SYNC_SOFT_MAX_FRACTION,
            FRAME_SYNC_BITS / 6,
        )
    }

    /// As [`Framer::sync_matches`], but with the relaxed thresholds used for
    /// the single flywheel-predicted position — see [`SYNC_ERR_MAX_COAST`].
    fn sync_matches_coast(&self, window: u64, errs: u32) -> bool {
        // Own bail bound, scaled to the fraction actually in use here —
        // sharing `sync_matches`' fixed `FRAME_SYNC_BITS / 6` would silently
        // cap the coast path's looser `SYNC_SOFT_MAX_FRACTION_COAST` at the
        // strict path's bound, defeating the point of loosening it.
        let bail = (FRAME_SYNC_BITS as f32 * SYNC_SOFT_MAX_FRACTION_COAST * 1.5) as u32;
        self.sync_matches_at(
            window,
            errs,
            SYNC_ERR_MAX_COAST,
            SYNC_SOFT_MAX_FRACTION_COAST,
            bail,
        )
    }

    fn sync_matches_at(
        &self,
        window: u64,
        errs: u32,
        err_max: u32,
        soft_max_fraction: f32,
        bail_max: u32,
    ) -> bool {
        if errs <= err_max {
            return true;
        }
        // Beyond this many raw mismatches, no confidence-weighting can bring
        // the ratio under `soft_max_fraction`; bail before doing the
        // arithmetic.
        if errs > bail_max {
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
        total > 0 && (bad as f32) < soft_max_fraction * total as f32
    }

    /// Arm the flywheel: the next FSW is protocol-predicted
    /// [`FLYWHEEL_COAST_DIBITS`] *content* dibits from here — but a status
    /// dibit is inserted into the raw wire stream every 36 counted positions
    /// (`status_dibit`), continuously across frame boundaries, independent
    /// of any particular frame's length. If one falls inside the coasted
    /// stretch it doesn't count as content, so the raw wire-dibit gap to the
    /// next FSW is 24 only when none does, and 25 when exactly one does (at
    /// most one can, since 24 < 36). Get this wrong and the single
    /// predicted-position check lands one dibit off the true FSW, which
    /// showed up on a real capture as a very consistent, non-random error
    /// count rather than the occasional miss real noise would produce —
    /// `self.since_fs` already tracks the exact absolute position needed to
    /// compute this correctly instead of assuming the common case.
    fn arm_flywheel(&mut self) {
        let mut raw = 0u32;
        let mut content = 0u32;
        let mut s = self.since_fs as u32;
        while content < FLYWHEEL_COAST_DIBITS {
            if s % 36 != 35 {
                content += 1;
            }
            s += 1;
            raw += 1;
        }
        self.flywheel = Some(raw);
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
        DIBIT_CLOCK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        match self.state {
            State::Search => {
                self.since_payload = self.since_payload.saturating_add(1);
                let (window, errs) = self.shift_in(sd);
                // The flywheel's one predicted-position check, in addition to
                // (never instead of) the ordinary check below — see
                // `arm_flywheel`. Single-shot: cleared here whether or not it
                // matches, so a miss falls through to the ordinary cold
                // sliding search from the very next dibit, exactly as before
                // this existed.
                let (coast_hit, at_predicted) = match &mut self.flywheel {
                    Some(n) if *n > 1 => {
                        *n -= 1;
                        (false, false)
                    }
                    Some(_) => {
                        self.flywheel = None;
                        (self.sync_matches_coast(window, errs), true)
                    }
                    None => (false, false),
                };
                if coast_hit || self.sync_matches(window, errs) {
                    self.on_real_sync(errs, events);
                } else if at_predicted && self.can_coast() {
                    // The sync word was due here and did not show. Mid-voice
                    // that is far more often a damaged sync than a silent
                    // channel, so presume it and let the NID (or, failing
                    // that, the voice FEC) be the judge.
                    self.since_fs = 24;
                    self.buf.clear();
                    self.state = State::Nid { presumed: true };
                }
            }
            State::Nid { presumed } => {
                self.since_payload = self.since_payload.saturating_add(1);
                if !self.status_dibit() {
                    self.buf.push(sd);
                }
                if self.buf.len() == 32 {
                    let mut w = 0u64;
                    for d in &self.buf {
                        w = (w << 2) | d.bits as u64;
                    }
                    self.buf.clear();
                    let decoded = self.nid_codec.decode(w).filter(|(nid, errs)| {
                        // Behind a presumed sync the NID is the only evidence
                        // there is a frame at all, so hold it to a higher
                        // standard: a nearly clean decode, or the NAC this
                        // channel is known to use.
                        !presumed || *errs <= 3 || Some(nid.nac) == self.tracked_nac()
                    });
                    match decoded {
                        Some((nid, errs)) => {
                            events.push(FramerEvent::Nid {
                                nid,
                                bch_errors: errs,
                            });
                            self.track_nac(nid.nac);
                            self.coast_left = MAX_COAST_FRAMES;
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
                                    self.last_duid = Some(nid.duid);
                                    self.since_payload = 0;
                                    self.state = State::Search;
                                    return;
                                }
                            };
                            self.state = State::Payload {
                                nid,
                                needed,
                                inferred: false,
                            };
                        }
                        None if self.can_coast() => {
                            // Mid-voice, on a known NAC: the frame is almost
                            // certainly the next LDU in the cadence. Collect
                            // it as such and let the voice FEC judge it.
                            self.coast_left -= 1;
                            let nid = Nid {
                                nac: self.tracked_nac().unwrap_or(0),
                                duid: self.next_voice_duid(),
                            };
                            self.state = State::Payload {
                                nid,
                                needed: LDU_PAYLOAD_BITS / 2,
                                inferred: true,
                            };
                        }
                        None => self.state = State::Search,
                    }
                }
            }
            State::Payload {
                nid,
                needed,
                inferred,
            } => {
                self.since_payload = self.since_payload.saturating_add(1);
                if inferred {
                    // A guessed frame is abandoned the moment a real sync word
                    // shows up inside it: the guess was wrong (the
                    // transmission ended, say) and the sync is what to follow.
                    // A confirmed frame never does this — its length is
                    // declared by a decoded NID, and a sync-looking pattern in
                    // the middle of it is data.
                    let (window, errs) = self.shift_in(sd);
                    if self.sync_matches(window, errs) {
                        self.buf.clear();
                        events.push(FramerEvent::Sync { bit_errors: errs });
                        self.since_fs = 24;
                        self.state = State::Nid { presumed: false };
                        return;
                    }
                }
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
                            // Same length guard as `extract_imbe_frames` above
                            // (`bits` and `conf` are both `self.buf.len() * 2`
                            // long), so this cannot fail when `frames` did not.
                            let conf_bits = crate::soft::soft_dibits_to_bit_conf(&self.buf);
                            let conf: [ImbeConf; 9] = extract_imbe_conf(&conf_bits)
                                .expect("conf and bits are the same length");
                            let ess = if nid.duid == Duid::LogicalLinkDataUnit2 {
                                crate::ess::decode_ess(&bits)
                            } else {
                                None
                            };
                            events.push(FramerEvent::Ldu {
                                nac: nid.nac,
                                duid: nid.duid,
                                imbe: Box::new(frames),
                                conf: Box::new(conf),
                                ess,
                                inferred,
                            });
                        }
                        self.buf.clear();
                        self.frame_ended(nid.duid);
                    }
                    _ => {
                        self.buf.clear();
                        self.frame_ended(nid.duid);
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
                self.frame_ended(nid.duid);
            }
            None if self.pdu.in_progress() => {
                // Header accepted; keep collecting the blocks it promised.
                self.state = State::Payload {
                    nid,
                    needed: self.buf.len() + n,
                    inferred: false,
                };
            }
            None => {
                // The header failed its CRC, or a block was undecodable.
                // Packet data is rare enough that guessing costs more than
                // waiting for the next one. Still consumed exactly one
                // fixed-size block, so the position is trustworthy even
                // though the content wasn't.
                self.pdu.reset();
                self.buf.clear();
                self.frame_ended(nid.duid);
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
                if std::env::var_os("HS_TSDU_DEBUG").is_some() {
                    let ml = crate::trellis::decode_soft(&arr);
                    let ok = trellis_to_tsbk(&arr).is_some();
                    let data_hex: String = ml
                        .as_ref()
                        .map(|(d, _)| d.iter().map(|b| format!("{b:02x}")).collect())
                        .unwrap_or_default();
                    let rx: String = arr.iter().map(|d| char::from(b'0' + d.bits)).collect();
                    let confs: Vec<u32> = arr
                        .iter()
                        .map(|d| d.conf[0] as u32 + d.conf[1] as u32)
                        .collect();
                    eprintln!(
                        "TSDU_DBG t={} blk={}/{} ok={} cost={:?} data={} rx={} confs={:?}",
                        DIBIT_CLOCK.load(std::sync::atomic::Ordering::Relaxed),
                        k,
                        n_blocks,
                        ok,
                        ml.map(|(_, c)| c),
                        data_hex,
                        rx,
                        confs
                    );
                }
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
            self.frame_ended(nid.duid);
        } else {
            self.state = State::Payload {
                nid,
                needed: (n_blocks + 1) * 98,
                inferred: false,
            };
        }
    }
}

#[cfg(test)]
mod flywheel_tests {
    use super::*;

    /// A status dibit is inserted every 36 counted positions, continuously
    /// across frame boundaries — independent of any one frame's length. If
    /// one falls inside the coasted stretch it costs an extra raw wire dibit
    /// (it isn't content); if none does, the raw gap is the plain 24. Get
    /// this wrong and the single predicted-position check lands off the true
    /// FSW — this regressed a real off-air capture (syncs 1191->900, TSBKs
    /// 3541->901) despite every synthetic test passing, because the
    /// synthetic tests' frame lengths happened not to straddle a status
    /// dibit. Exercise both cases directly against `since_fs`, not just
    /// through one synthetic scenario that got lucky.
    #[test]
    fn accounts_for_a_status_dibit_landing_inside_the_coast() {
        let mut f = Framer::new();

        // No status dibit in [s, s+24): raw gap is exactly 24.
        f.since_fs = 0;
        f.arm_flywheel();
        assert_eq!(f.flywheel, Some(24), "since_fs=0");

        // since_fs=12 -> positions 12..36 -> 35 falls inside (at s+23):
        // raw gap must be 25 to still land 24 *content* dibits later.
        f.since_fs = 12;
        f.arm_flywheel();
        assert_eq!(
            f.flywheel,
            Some(25),
            "since_fs=12, straddles a status dibit"
        );

        // since_fs=35 itself is the status position: the very next dibit
        // (s=35) is status and doesn't count, so this also needs 25.
        f.since_fs = 35;
        f.arm_flywheel();
        assert_eq!(f.flywheel, Some(25), "since_fs=35");

        // since_fs=36 (just past a status dibit): clear run of 24, back to
        // the plain case.
        f.since_fs = 36;
        f.arm_flywheel();
        assert_eq!(f.flywheel, Some(24), "since_fs=36");
    }
}

/// How many list-Viterbi candidates to test against the TSBK CRC when the
/// maximum-likelihood path fails it. Chosen on a reference
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
