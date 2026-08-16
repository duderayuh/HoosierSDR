//! Dibit-stream framer: frame sync search, status-symbol removal, NID
//! decode, and per-DUID payload collection.
//!
//! Protocol facts: a status dibit is inserted every 35 payload dibits
//! (every 70 bits) counted from the start of the frame sync; the FSW itself
//! (24 dibits) is never interrupted.

use crate::nid::{Nid, NidCodec};
use crate::tsbk::{self, TsbkBlock};
use crate::voice::{extract_imbe_frames, ldu2_algid_raw, ImbeFrame, LDU_PAYLOAD_BITS};
use crate::{Duid, FRAME_SYNC, FRAME_SYNC_BITS};

/// Max bit errors tolerated in the 48-bit sync correlation.
const SYNC_ERR_MAX: u32 = 2;

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
    buf: Vec<u8>, // collected payload dibits (status removed)
    nid_codec: NidCodec,
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
        }
    }

    /// Push one sliced dibit; may emit events.
    pub fn push(&mut self, dibit: u8, events: &mut Vec<FramerEvent>) {
        match self.state {
            State::Search => {
                self.shift = (self.shift << 2) | dibit as u64;
                let window = self.shift & ((1u64 << FRAME_SYNC_BITS) - 1);
                let errs = (window ^ FRAME_SYNC).count_ones();
                if errs <= SYNC_ERR_MAX {
                    events.push(FramerEvent::Sync { bit_errors: errs });
                    self.since_fs = 24;
                    self.buf.clear();
                    self.state = State::Nid;
                }
            }
            State::Nid => {
                if !self.status_dibit() {
                    self.buf.push(dibit);
                }
                if self.buf.len() == 32 {
                    let mut w = 0u64;
                    for &d in &self.buf {
                        w = (w << 2) | d as u64;
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
                    self.buf.push(dibit);
                }
                if self.buf.len() < needed {
                    return;
                }
                match nid.duid {
                    Duid::TrunkSignalBlock => self.tsdu_block(nid, events),
                    Duid::LogicalLinkDataUnit1 | Duid::LogicalLinkDataUnit2 => {
                        let bits = crate::bits::dibits_to_bits(&self.buf);
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

    /// Handle a completed 98-dibit TSBK block; may extend for chained blocks.
    fn tsdu_block(&mut self, nid: Nid, events: &mut Vec<FramerEvent>) {
        // Collect blocks until last-block flag or 3 blocks or a bad decode.
        let n_blocks = self.buf.len() / 98;
        let block: &[u8] = &self.buf[(n_blocks - 1) * 98..n_blocks * 98];
        let arr: [u8; 98] = block.try_into().unwrap();
        let decoded = trellis_to_tsbk(&arr);
        let done = match &decoded {
            Some(b) => b.last_block || n_blocks >= 3,
            None => true,
        };
        if done {
            let mut blocks = Vec::new();
            for k in 0..n_blocks {
                let arr: [u8; 98] = self.buf[k * 98..(k + 1) * 98].try_into().unwrap();
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

fn trellis_to_tsbk(dibits: &[u8; 98]) -> Option<TsbkBlock> {
    let (data, _cost) = crate::trellis::decode(dibits)?;
    let mut bits = [0u8; 96];
    for (i, b) in bits.iter_mut().enumerate() {
        *b = (data[i / 8] >> (7 - i % 8)) & 1;
    }
    tsbk::parse(&bits)
}
