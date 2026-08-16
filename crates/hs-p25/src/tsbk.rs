//! Trunking Signal Block parsing (TIA-102.AABC opcodes).
//!
//! A TSBK is 12 octets: LB(1) P(1) Opcode(6) | MFID(8) | args(64) | CRC(16).

use crate::bits::read_bits;
use crate::crc::crc16_ccitt;

#[derive(Debug, Clone, PartialEq)]
pub enum Tsbk {
    /// 0x00 — Group Voice Channel Grant.
    GroupVoiceGrant {
        opts: u8,
        channel: u16,
        group: u16,
        source: u32,
    },
    /// 0x02 — Group Voice Channel Grant Update (two grants per block).
    GroupVoiceGrantUpdate {
        channel_a: u16,
        group_a: u16,
        channel_b: u16,
        group_b: u16,
    },
    /// 0x3D — Channel Identifier Update (FDMA).
    IdenUp {
        iden: u8,
        bw_khz: f64,
        tx_offset_mhz: f64,
        spacing_khz: f64,
        base_freq_hz: u64,
    },
    /// 0x3B — Network Status Broadcast.
    NetworkStatus {
        wacn: u32,
        sys_id: u16,
        channel: u16,
    },
    /// 0x3A — RFSS Status Broadcast.
    RfssStatus {
        rfss: u8,
        site: u8,
        channel: u16,
    },
    Unknown {
        opcode: u8,
        mfid: u8,
        args: u64,
    },
}

#[derive(Debug)]
pub struct TsbkBlock {
    pub last_block: bool,
    pub tsbk: Tsbk,
}

/// Parse one 12-octet block (96 bits, MSB-first). Verifies CRC.
pub fn parse(bits96: &[u8]) -> Option<TsbkBlock> {
    assert_eq!(bits96.len(), 96);
    let crc_rx = read_bits(bits96, 80, 16) as u16;
    if crc16_ccitt(&bits96[..80]) != crc_rx {
        return None;
    }
    let last_block = bits96[0] == 1;
    let opcode = read_bits(bits96, 2, 6) as u8;
    let mfid = read_bits(bits96, 8, 8) as u8;
    let args = read_bits(bits96, 16, 64);

    let tsbk = match opcode {
        0x00 => Tsbk::GroupVoiceGrant {
            opts: (args >> 56) as u8,
            channel: ((args >> 40) & 0xFFFF) as u16,
            group: ((args >> 24) & 0xFFFF) as u16,
            source: (args & 0xFF_FFFF) as u32,
        },
        0x02 => Tsbk::GroupVoiceGrantUpdate {
            channel_a: ((args >> 48) & 0xFFFF) as u16,
            group_a: ((args >> 32) & 0xFFFF) as u16,
            channel_b: ((args >> 16) & 0xFFFF) as u16,
            group_b: (args & 0xFFFF) as u16,
        },
        0x3D => {
            let iden = ((args >> 60) & 0xF) as u8;
            let bw = ((args >> 51) & 0x1FF) as f64 * 0.125; // 125 Hz units
            let sign = (args >> 50) & 1;
            let off = ((args >> 42) & 0xFF) as f64 * 0.25; // 250 kHz units
            let spacing = ((args >> 32) & 0x3FF) as f64 * 0.125;
            let base = (args & 0xFFFF_FFFF) * 5; // 5 Hz units
            Tsbk::IdenUp {
                iden,
                bw_khz: bw,
                tx_offset_mhz: if sign == 1 { off } else { -off },
                spacing_khz: spacing,
                base_freq_hz: base,
            }
        }
        0x3B => Tsbk::NetworkStatus {
            wacn: ((args >> 24) & 0xF_FFFF) as u32,
            sys_id: ((args >> 12) & 0xFFF) as u16,
            channel: ((args) & 0xFFF) as u16,
        },
        0x3A => Tsbk::RfssStatus {
            rfss: ((args >> 32) & 0xFF) as u8,
            site: ((args >> 24) & 0xFF) as u8,
            channel: ((args >> 8) & 0xFFFF) as u16,
        },
        _ => Tsbk::Unknown { opcode, mfid, args },
    };
    Some(TsbkBlock { last_block, tsbk })
}

/// Build the 96 bits (with CRC) for a TSBK — used by tests and hs-bench.
pub fn build(last_block: bool, opcode: u8, mfid: u8, args: u64) -> [u8; 96] {
    use crate::bits::write_bits;
    let mut bits = [0u8; 96];
    bits[0] = last_block as u8;
    write_bits(&mut bits, 2, 6, opcode as u64);
    write_bits(&mut bits, 8, 8, mfid as u64);
    write_bits(&mut bits, 16, 64, args);
    let crc = crc16_ccitt(&bits[..80]);
    write_bits(&mut bits, 80, 16, crc as u64);
    bits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_roundtrip() {
        // channel 0x100A, group 0x2F93, source 0xBEEF1
        let args: u64 = (0x100Au64 << 40) | (0x2F93u64 << 24) | 0xBEEF1;
        let bits = build(true, 0x00, 0, args);
        let b = parse(&bits).unwrap();
        assert!(b.last_block);
        assert_eq!(
            b.tsbk,
            Tsbk::GroupVoiceGrant {
                opts: 0,
                channel: 0x100A,
                group: 0x2F93,
                source: 0xBEEF1
            }
        );
        // CRC must reject corruption.
        let mut bad = bits;
        bad[30] ^= 1;
        assert!(parse(&bad).is_none());
    }
}
