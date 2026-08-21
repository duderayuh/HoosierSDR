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
    /// 0x39 — Secondary Control Channel Broadcast: the site's alternate
    /// control channels. A site announces where else its control channel can
    /// appear, so a receiver that loses the primary knows where to look
    /// instead of scanning blind.
    SecondaryControl {
        rfss: u8,
        site: u8,
        /// Up to two (channel, service class) pairs; a pair of zeros is an
        /// unused slot.
        channel_a: u16,
        class_a: u8,
        channel_b: u16,
        class_b: u8,
    },
    /// 0x3C — Adjacent Status Broadcast: a neighbouring site of the same
    /// system, with the channel its control channel sits on. Same layout as
    /// RFSS Status Broadcast, describing another site instead of this one.
    AdjacentStatus {
        sys_id: u16,
        rfss: u8,
        site: u8,
        channel: u16,
        service_class: u8,
    },
    /// 0x28 — Group Affiliation Response: the site telling a radio which
    /// talkgroup it now belongs to (or refusing). `accepted` is the GAV field
    /// being zero ("affiliation accepted").
    GroupAffiliationResponse {
        accepted: bool,
        /// Announcement group the talkgroup is a member of.
        announcement_group: u16,
        group: u16,
        target: u32,
    },
    /// 0x2C — Unit Registration Response: a radio has registered on this
    /// system (or was refused / told to go elsewhere).
    UnitRegistrationResponse {
        /// RV field: 0 accepted, 1 fail, 2 denied, 3 refused.
        status: u8,
        sys_id: u16,
        source_id: u32,
        source_address: u32,
    },
    /// 0x2B — Location Registration Response: a radio registered its
    /// location on this RFSS/site for a talkgroup.
    LocationRegistrationResponse {
        status: u8,
        group: u16,
        rfss: u8,
        site: u8,
        target: u32,
    },
    /// 0x2F — De-Registration Acknowledge: a radio has left the system.
    DeregistrationAck {
        wacn: u32,
        sys_id: u16,
        source_id: u32,
    },
    /// A Motorola Group Regroup (talkgroup patch) message.
    MotoRegroup(crate::moto::MotoRegroup),
    /// A manufacturer-specific block. The opcode space belongs to the vendor,
    /// so the standard meanings do not apply and the arguments are left raw.
    VendorSpecific {
        mfid: u8,
        opcode: u8,
        args: u64,
    },
    Unknown {
        opcode: u8,
        mfid: u8,
        args: u64,
    },
}

/// Manufacturer IDs whose blocks follow the standard opcode assignments.
/// Everything else redefines the opcode space for its own use.
const MFID_STANDARD: u8 = 0x00;
const MFID_STANDARD_ALT: u8 = 0x01;

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

    // Opcodes are only standard when the manufacturer ID says so. A vendor
    // block carries a perfectly valid CRC -- it is a real message -- but its
    // arguments mean something else entirely, so interpreting one as a
    // standard grant yields a confident, plausible-looking lie: a real
    // talkgroup number pointing at a frequency nothing is transmitting on.
    if mfid != MFID_STANDARD && mfid != MFID_STANDARD_ALT {
        // Motorola's Group Regroup messages are understood well enough to
        // decode (see `moto`); every other vendor block keeps its raw
        // arguments so it can be identified later from a shared log.
        let tsbk = match (mfid == crate::moto::MFID_MOTOROLA)
            .then(|| crate::moto::parse(opcode, args))
            .flatten()
        {
            Some(r) => Tsbk::MotoRegroup(r),
            None => Tsbk::VendorSpecific { mfid, opcode, args },
        };
        return Some(TsbkBlock { last_block, tsbk });
    }

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
        // LRA(8) | WACN(20) | SysID(12) | Channel(16) | Service class(8).
        // An earlier split read WACN from bits 43..24 and reported the
        // Marion County control channel as WACN 0x262, system 5; the real
        // values — WACN 0xBEE00, system 0x262 — fell out once the fields
        // were placed where the standard puts them.
        0x3B => Tsbk::NetworkStatus {
            wacn: ((args >> 36) & 0xF_FFFF) as u32,
            sys_id: ((args >> 24) & 0xFFF) as u16,
            channel: ((args >> 8) & 0xFFFF) as u16,
        },
        0x39 => Tsbk::SecondaryControl {
            rfss: (args >> 56) as u8,
            site: ((args >> 48) & 0xFF) as u8,
            channel_a: ((args >> 32) & 0xFFFF) as u16,
            class_a: ((args >> 24) & 0xFF) as u8,
            channel_b: ((args >> 8) & 0xFFFF) as u16,
            class_b: (args & 0xFF) as u8,
        },
        0x3A => Tsbk::RfssStatus {
            rfss: ((args >> 32) & 0xFF) as u8,
            site: ((args >> 24) & 0xFF) as u8,
            channel: ((args >> 8) & 0xFFFF) as u16,
        },
        // LRA(8) | reserved(4) SysID(12) | RFSS(8) | Site(8) | Channel(16) | class(8)
        0x3C => Tsbk::AdjacentStatus {
            sys_id: ((args >> 40) & 0xFFF) as u16,
            rfss: ((args >> 32) & 0xFF) as u8,
            site: ((args >> 24) & 0xFF) as u8,
            channel: ((args >> 8) & 0xFFFF) as u16,
            service_class: (args & 0xFF) as u8,
        },
        // LG(1) GAV(2) reserved(5) | Announcement Group(16) | Group(16) | Target(24)
        0x28 => Tsbk::GroupAffiliationResponse {
            accepted: (args >> 61) & 0x3 == 0,
            announcement_group: ((args >> 40) & 0xFFFF) as u16,
            group: ((args >> 24) & 0xFFFF) as u16,
            target: (args & 0xFF_FFFF) as u32,
        },
        // reserved(2) RV(2) SysID(12) | Source ID(24) | Source Address(24)
        0x2C => Tsbk::UnitRegistrationResponse {
            status: ((args >> 60) & 0x3) as u8,
            sys_id: ((args >> 48) & 0xFFF) as u16,
            source_id: ((args >> 24) & 0xFF_FFFF) as u32,
            source_address: (args & 0xFF_FFFF) as u32,
        },
        // reserved(6) RV(2) | Group(16) | RFSS(8) | Site(8) | Target(24)
        0x2B => Tsbk::LocationRegistrationResponse {
            status: ((args >> 56) & 0x3) as u8,
            group: ((args >> 40) & 0xFFFF) as u16,
            rfss: ((args >> 32) & 0xFF) as u8,
            site: ((args >> 24) & 0xFF) as u8,
            target: (args & 0xFF_FFFF) as u32,
        },
        // reserved(8) | WACN(20) | SysID(12) | Source ID(24)
        0x2F => Tsbk::DeregistrationAck {
            wacn: ((args >> 36) & 0xF_FFFF) as u32,
            sys_id: ((args >> 24) & 0xFFF) as u16,
            source_id: (args & 0xFF_FFFF) as u32,
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
    /// Build a TSBK with a valid CRC so it reaches the opcode dispatch.
    fn tsbk_bits(opcode: u8, mfid: u8, args: u64) -> Vec<u8> {
        let mut bits = vec![0u8; 96];
        bits[0] = 1; // last block
        for k in 0..6 {
            bits[2 + k] = (opcode >> (5 - k)) & 1;
        }
        for k in 0..8 {
            bits[8 + k] = (mfid >> (7 - k)) & 1;
        }
        for k in 0..64 {
            bits[16 + k] = ((args >> (63 - k)) & 1) as u8;
        }
        let crc = crc16_ccitt(&bits[..80]);
        for k in 0..16 {
            bits[80 + k] = ((crc >> (15 - k)) & 1) as u8;
        }
        bits
    }

    #[test]
    fn a_vendor_block_is_not_read_as_a_standard_grant() {
        // Same opcode and arguments, two manufacturer IDs. Under the standard
        // MFID this is a group voice grant; under a vendor MFID the opcode
        // space belongs to the vendor and these bits mean something else.
        //
        // This mattered on a real control channel: vendor blocks carry valid
        // CRCs, so they were parsed as grants and produced real-looking
        // talkgroups pointing at frequencies measured to be below the noise
        // floor -- nothing was transmitting there at all.
        let args: u64 = (0x1234u64 << 40) | (0x2F93u64 << 24) | 0xBEEF1;

        let std = parse(&tsbk_bits(0x00, 0x00, args)).expect("standard parses");
        assert!(
            matches!(std.tsbk, Tsbk::GroupVoiceGrant { group: 0x2F93, .. }),
            "standard MFID must still decode a grant: {:?}",
            std.tsbk
        );

        // Under Motorola's MFID the same opcode is a Group Regroup message.
        // What matters is that it is not a grant: the talkgroup and frequency
        // a standard read would have produced were fiction.
        let moto = parse(&tsbk_bits(0x00, 0x90, args)).expect("motorola block parses");
        assert!(
            matches!(moto.tsbk, Tsbk::MotoRegroup(_)),
            "motorola block should decode as regroup, got {:?}",
            moto.tsbk
        );
        assert!(
            !matches!(moto.tsbk, Tsbk::GroupVoiceGrant { .. }),
            "a vendor block must never be read as a standard grant"
        );

        // A manufacturer we have no parser for keeps its arguments raw rather
        // than being forced through the nearest known layout.
        let other = parse(&tsbk_bits(0x00, 0xA4, args)).expect("vendor block parses");
        assert!(
            matches!(
                other.tsbk,
                Tsbk::VendorSpecific {
                    mfid: 0xA4,
                    opcode: 0x00,
                    ..
                }
            ),
            "unknown vendor MFID must stay raw: {:?}",
            other.tsbk
        );
    }

    use super::*;

    #[test]
    fn secondary_control_roundtrip() {
        // RFSS 1, site 5, channel A 0x100A class 0x70, channel B unused.
        let args: u64 = (1u64 << 56) | (5u64 << 48) | (0x100Au64 << 32) | (0x70u64 << 24);
        let b = parse(&build(true, 0x39, 0, args)).unwrap();
        assert_eq!(
            b.tsbk,
            Tsbk::SecondaryControl {
                rfss: 1,
                site: 5,
                channel_a: 0x100A,
                class_a: 0x70,
                channel_b: 0,
                class_b: 0,
            }
        );
    }

    /// The mobility messages a scanner turns into an affiliation view: each
    /// field lands where TIA-102.AABC puts it.
    #[test]
    fn affiliation_and_registration_roundtrips() {
        // Group Affiliation Response: accepted (GAV 0), announcement group
        // 0x0101, group 0x2F93, target 0xBEEF1.
        let args = (0x0101u64 << 40) | (0x2F93u64 << 24) | 0xBEEF1;
        assert_eq!(
            parse(&build(true, 0x28, 0, args)).unwrap().tsbk,
            Tsbk::GroupAffiliationResponse {
                accepted: true,
                announcement_group: 0x0101,
                group: 0x2F93,
                target: 0xBEEF1,
            }
        );
        // GAV = 2 (denied) sets bits 62..61.
        assert!(matches!(
            parse(&build(true, 0x28, 0, args | (2u64 << 61)))
                .unwrap()
                .tsbk,
            Tsbk::GroupAffiliationResponse {
                accepted: false,
                ..
            }
        ));
        // Unit Registration Response: RV 0, system 0x6BD, source id and
        // address both 0xBEEF1.
        let args = (0x6BDu64 << 48) | (0xBEEF1u64 << 24) | 0xBEEF1;
        assert_eq!(
            parse(&build(true, 0x2C, 0, args)).unwrap().tsbk,
            Tsbk::UnitRegistrationResponse {
                status: 0,
                sys_id: 0x6BD,
                source_id: 0xBEEF1,
                source_address: 0xBEEF1,
            }
        );
        // Location Registration Response: RV 1, group 0x2F93, RFSS 1 site 5.
        let args = (1u64 << 56) | (0x2F93u64 << 40) | (1u64 << 32) | (5u64 << 24) | 0xBEEF1;
        assert_eq!(
            parse(&build(true, 0x2B, 0, args)).unwrap().tsbk,
            Tsbk::LocationRegistrationResponse {
                status: 1,
                group: 0x2F93,
                rfss: 1,
                site: 5,
                target: 0xBEEF1,
            }
        );
        // De-Registration Acknowledge: WACN 0xBEE00, system 0x6BD.
        let args = (0xBEE00u64 << 36) | (0x6BDu64 << 24) | 0xBEEF1;
        assert_eq!(
            parse(&build(true, 0x2F, 0, args)).unwrap().tsbk,
            Tsbk::DeregistrationAck {
                wacn: 0xBEE00,
                sys_id: 0x6BD,
                source_id: 0xBEEF1,
            }
        );
        // Adjacent Status Broadcast: system 0x6BD, RFSS 1, site 7, channel
        // 0x100A, service class 0x70.
        let args = (0x6BDu64 << 40) | (1u64 << 32) | (7u64 << 24) | (0x100Au64 << 8) | 0x70;
        assert_eq!(
            parse(&build(true, 0x3C, 0, args)).unwrap().tsbk,
            Tsbk::AdjacentStatus {
                sys_id: 0x6BD,
                rfss: 1,
                site: 7,
                channel: 0x100A,
                service_class: 0x70,
            }
        );
    }

    #[test]
    fn network_status_roundtrip() {
        // WACN 0xBEE00, system 0x262, channel 0x100A, class 0x70.
        let args = (0xBEE00u64 << 36) | (0x262u64 << 24) | (0x100Au64 << 8) | 0x70;
        assert_eq!(
            parse(&build(true, 0x3B, 0, args)).unwrap().tsbk,
            Tsbk::NetworkStatus {
                wacn: 0xBEE00,
                sys_id: 0x262,
                channel: 0x100A,
            }
        );
    }

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
