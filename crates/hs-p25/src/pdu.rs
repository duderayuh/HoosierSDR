//! P25 Packet Data Units (DUID 0xC): the data-bearing side of the air
//! interface.
//!
//! Everything decoded so far has been voice (LDU) or trunking control (TSBK).
//! Packet data is the third path, and it carries IP datagrams — which is how
//! location reporting reaches the air. A radio's GPS position is not a P25
//! field; it is an ordinary UDP payload riding inside a PDU, so reaching it
//! means assembling packet data first (see [`crate::lrrp`]).
//!
//! Structure: a header block, then `blocks_to_follow` data blocks, each
//! carried in the same 1/2-rate trellis code as a TSBK, so all of it benefits
//! from the soft-decision Viterbi decoder. The header ends in a CRC-CCITT over
//! its first ten octets, which is what makes it safe to act on a header at all
//! — packet data is far rarer than voice, so a mis-framed block that decoded
//! into a plausible-looking header would otherwise poison everything after it.
//!
//! ## Confidence in this parser
//!
//! The header layout below is implemented from public descriptions of the
//! TIA-102 packet data format, and validated by CRC rather than by having been
//! run against real traffic — this project has no packet-data capture yet.
//! The CRC is what makes that acceptable: a header whose CRC fails is
//! discarded rather than guessed at, so a wrong field offset shows up as
//! "nothing decodes" instead of as confident nonsense. Fields the format
//! reserves or that vary by packet type are kept as raw octets rather than
//! being given speculative meanings.

use crate::soft::SoftDibit;

/// Dibits in one trellis-coded block, header or data.
pub const BLOCK_DIBITS: usize = 98;

/// Octets a decoded block yields.
pub const BLOCK_OCTETS: usize = 12;

/// Format field values we recognize. The format decides how the remaining
/// header octets are laid out and whether data blocks carry their own CRC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PduFormat {
    /// Unconfirmed delivery: data blocks are 12 octets of payload.
    Unconfirmed,
    /// Confirmed delivery: each data block spends 2 octets on a sequence
    /// number and block CRC, leaving 10 for payload.
    Confirmed,
    /// Alternate multi-block trunking — control signalling, not user data.
    AltMultiBlockTrunking,
    /// Response packet (acknowledgement), carries no user payload.
    Response,
    Other(u8),
}

impl From<u8> for PduFormat {
    fn from(v: u8) -> Self {
        match v & 0x3F {
            0x15 => PduFormat::Unconfirmed,
            0x16 => PduFormat::Confirmed,
            0x17 => PduFormat::AltMultiBlockTrunking,
            0x03 => PduFormat::Response,
            other => PduFormat::Other(other),
        }
    }
}

impl PduFormat {
    /// Payload octets each data block contributes.
    pub fn payload_octets_per_block(self) -> usize {
        match self {
            // Confirmed blocks spend 2 octets on DBSN + CRC-9.
            PduFormat::Confirmed => 10,
            _ => 12,
        }
    }
}

/// A decoded PDU header block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PduHeader {
    /// Confirmed delivery requested.
    pub confirmed: bool,
    /// Outbound (base → subscriber) when true.
    pub outbound: bool,
    pub format: PduFormat,
    /// Service Access Point: which service the payload belongs to. Packet
    /// data carrying IP is what location reporting rides on.
    pub sap: u8,
    /// Manufacturer ID; 0 is standard, non-zero marks a vendor extension.
    pub mfid: u8,
    /// Logical Link ID — the radio this packet is addressed to or from.
    pub llid: u32,
    /// Number of data blocks following this header.
    pub blocks_to_follow: u8,
    /// Padding octets added to the last block.
    pub pad_octets: u8,
}

impl PduHeader {
    /// SAP value used for packet data carrying IP datagrams.
    pub const SAP_IP: u8 = 0x04;

    /// True if this header introduces user data we could parse further.
    pub fn carries_user_data(&self) -> bool {
        matches!(self.format, PduFormat::Unconfirmed | PduFormat::Confirmed)
            && self.blocks_to_follow > 0
    }
}

/// Decode a 12-octet header block, verifying its trailing CRC-CCITT.
///
/// Returns None when the CRC fails, which is the only thing standing between a
/// mis-framed block and a confident misreading of the air.
pub fn parse_header(octets: &[u8; BLOCK_OCTETS]) -> Option<PduHeader> {
    // Octets 10..12 carry CRC-CCITT over octets 0..10.
    let want = u16::from_be_bytes([octets[10], octets[11]]);
    if crc_over_octets(&octets[..10]) != want {
        return None;
    }
    Some(PduHeader {
        confirmed: octets[0] & 0x80 != 0,
        outbound: octets[0] & 0x40 != 0,
        format: PduFormat::from(octets[0] & 0x3F),
        sap: octets[1] & 0x3F,
        mfid: octets[2],
        llid: u32::from_be_bytes([0, octets[3], octets[4], octets[5]]),
        blocks_to_follow: octets[6] & 0x7F,
        pad_octets: octets[7] & 0x1F,
    })
}

/// The CRC helper works over bits, so expand the octets MSB-first.
fn crc_over_octets(octets: &[u8]) -> u16 {
    let mut bits = Vec::with_capacity(octets.len() * 8);
    for &o in octets {
        for k in (0..8).rev() {
            bits.push((o >> k) & 1);
        }
    }
    crate::crc::crc16_ccitt(&bits)
}

/// A fully assembled packet: its header and the concatenated payload of every
/// data block, with padding removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub header: PduHeader,
    pub payload: Vec<u8>,
}

/// Assembles a PDU from its trellis-coded blocks as they arrive.
#[derive(Debug, Default)]
pub struct PduAssembler {
    header: Option<PduHeader>,
    payload: Vec<u8>,
    remaining: u8,
}

impl PduAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// True while a header has been accepted and blocks are still expected.
    pub fn in_progress(&self) -> bool {
        self.header.is_some() && self.remaining > 0
    }

    /// Blocks still expected before the packet is complete.
    pub fn remaining(&self) -> u8 {
        self.remaining
    }

    pub fn reset(&mut self) {
        self.header = None;
        self.payload.clear();
        self.remaining = 0;
    }

    /// Feed one trellis-coded block. Returns a `Packet` on the block that
    /// completes it.
    ///
    /// The block is soft-decoded, so packet data gets the same coding gain the
    /// control channel does — which matters more here, not less: a voice frame
    /// lost to noise costs 20 ms of audio, while a lost data block corrupts a
    /// whole packet.
    pub fn push_block(&mut self, dibits: &[SoftDibit; BLOCK_DIBITS]) -> Option<Packet> {
        let (octets, _cost) = crate::trellis::decode_soft(dibits)?;

        if self.header.is_none() {
            let h = parse_header(&octets)?;
            // A header claiming no blocks is complete on its own; one claiming
            // more blocks than a PDU may carry is a mis-decode, not a packet.
            if h.blocks_to_follow > 64 {
                return None;
            }
            if !h.carries_user_data() {
                let packet = Packet {
                    header: h,
                    payload: Vec::new(),
                };
                self.reset();
                return Some(packet);
            }
            self.remaining = h.blocks_to_follow;
            self.header = Some(h);
            self.payload.clear();
            return None;
        }

        let header = self.header.as_ref()?;
        let take = header.format.payload_octets_per_block();
        // Confirmed blocks lead with DBSN + CRC-9; the payload follows.
        let start = BLOCK_OCTETS - take;
        self.payload.extend_from_slice(&octets[start..]);
        self.remaining = self.remaining.saturating_sub(1);

        if self.remaining > 0 {
            return None;
        }
        let header = self.header.take()?;
        let pad = header.pad_octets as usize;
        let keep = self.payload.len().saturating_sub(pad);
        let packet = Packet {
            header,
            payload: self.payload[..keep].to_vec(),
        };
        self.reset();
        Some(packet)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn soft_block(octets: &[u8; BLOCK_OCTETS]) -> [SoftDibit; BLOCK_DIBITS] {
        let tx = crate::trellis::encode(octets);
        let mut out = [SoftDibit::default(); BLOCK_DIBITS];
        for (o, &d) in out.iter_mut().zip(tx.iter()) {
            *o = SoftDibit::hard(d);
        }
        out
    }

    fn header_octets(format: u8, blocks: u8, pad: u8, llid: u32) -> [u8; BLOCK_OCTETS] {
        let mut o = [0u8; BLOCK_OCTETS];
        o[0] = format & 0x3F;
        o[1] = PduHeader::SAP_IP;
        o[2] = 0; // standard MFID
        o[3] = (llid >> 16) as u8;
        o[4] = (llid >> 8) as u8;
        o[5] = llid as u8;
        o[6] = blocks & 0x7F;
        o[7] = pad & 0x1F;
        let crc = super::crc_over_octets(&o[..10]);
        o[10] = (crc >> 8) as u8;
        o[11] = crc as u8;
        o
    }

    #[test]
    fn rejects_a_header_whose_crc_fails() {
        // The whole safety story rests on this: a mis-framed block must be
        // dropped, not read as a confident packet.
        let mut o = header_octets(0x15, 2, 0, 0x1234);
        o[3] ^= 0xFF;
        assert_eq!(parse_header(&o), None);
    }

    #[test]
    fn assembles_a_multi_block_unconfirmed_packet() {
        let mut asm = PduAssembler::new();
        // 2 blocks, 3 pad octets → 24 carried, 21 of payload.
        let hdr = header_octets(0x15, 2, 3, 0x00AB_CDEF & 0xFF_FFFF);
        assert!(asm.push_block(&soft_block(&hdr)).is_none());
        assert!(asm.in_progress());

        let b1: [u8; 12] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let b2: [u8; 12] = [13, 14, 15, 16, 17, 18, 19, 20, 21, 0, 0, 0];
        assert!(asm.push_block(&soft_block(&b1)).is_none());
        let pkt = asm.push_block(&soft_block(&b2)).expect("packet completes");

        assert_eq!(pkt.header.format, PduFormat::Unconfirmed);
        assert_eq!(pkt.header.sap, PduHeader::SAP_IP);
        assert_eq!(pkt.header.llid, 0x00AB_CDEF & 0xFF_FFFF);
        assert_eq!(pkt.payload.len(), 21, "padding must be stripped");
        assert_eq!(&pkt.payload[..12], &b1);
        assert_eq!(&pkt.payload[12..], &b2[..9]);
        assert!(!asm.in_progress(), "assembler must reset after a packet");
    }

    #[test]
    fn confirmed_blocks_reserve_two_octets_per_block() {
        let mut asm = PduAssembler::new();
        let hdr = header_octets(0x16, 1, 0, 7);
        assert!(asm.push_block(&soft_block(&hdr)).is_none());
        let b1: [u8; 12] = [0xAA, 0xBB, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        let pkt = asm.push_block(&soft_block(&b1)).unwrap();
        assert_eq!(pkt.header.format, PduFormat::Confirmed);
        assert_eq!(pkt.payload, &b1[2..], "DBSN/CRC octets are not payload");
    }

    #[test]
    fn a_header_with_no_blocks_completes_immediately() {
        let mut asm = PduAssembler::new();
        let hdr = header_octets(0x03, 0, 0, 42); // response packet
        let pkt = asm.push_block(&soft_block(&hdr)).expect("completes alone");
        assert_eq!(pkt.header.format, PduFormat::Response);
        assert!(pkt.payload.is_empty());
        assert!(!asm.in_progress());
    }

    #[test]
    fn an_absurd_block_count_is_treated_as_a_mis_decode() {
        let mut asm = PduAssembler::new();
        let hdr = header_octets(0x15, 100, 0, 1);
        assert!(asm.push_block(&soft_block(&hdr)).is_none());
        assert!(
            !asm.in_progress(),
            "must not commit to a packet that cannot exist"
        );
    }
}
