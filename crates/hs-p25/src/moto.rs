//! Motorola manufacturer-specific TSBKs (MFID 0x90).
//!
//! TIA-102 lets a manufacturer define its own opcode space, and on a Motorola
//! P25 system that space carries more traffic than the standard one. A 60
//! second reference control channel held 599 Motorola blocks against 208
//! standard voice grants — so ignoring them means ignoring most of what the
//! system is saying.
//!
//! The messages decoded here are **Group Regroup (GRG)**: dynamic talkgroup
//! patching, where dispatch merges several talkgroups into a *supergroup*
//! that shares audio. The part that matters to a scanner is that **a patch
//! call's voice channel assignment is announced only here** — the standard
//! Group Voice Channel Grant never names the supergroup. A receiver that
//! files these messages away as bookkeeping plays none of the patch's calls,
//! while consumer scanners (which implement GRG natively) play them all. On
//! a metro county's dispatch, the NORTH / SOUTH dispatch supergroups are
//! granted exclusively through these opcodes.
//!
//! ## Cross-checked against public reporting
//!
//! Enthusiast documentation names the family: 0x00 Group Regroup Add Command,
//! 0x01 Delete, **0x02 GRG Channel Grant — "assign supergroup to channel"**,
//! **0x03 GRG Channel Grant Update**. The grant messages mirror their
//! standard-opcode counterparts field for field:
//!
//! * **0x02** = `opts(8) | channel(16) | supergroup(16) | source unit(24)` —
//!   the exact shape of the standard 0x00 Group Voice Channel Grant.
//! * **0x03** = two `(channel, supergroup)` pairs — the exact shape of the
//!   standard 0x02 Group Voice Channel Grant Update.
//!
//! ## Provenance and confidence
//!
//! There is no public specification for these opcodes, and an earlier
//! revision of this decoder read them the other way around — 0x02 as
//! `supergroup | member-talkgroup | unit` and 0x03 as `(supergroup, member)`
//! pairs — on the reasoning that the second field of 0x02, read as a channel,
//! named iden 2, which the observed system never announced. That reasoning
//! tested the wrong field: it is the *first* field that carries the channel.
//! The evidence that settled it:
//!
//! * The values observed in the **first** field (a small set in the
//!   hundreds, e.g. 949 and 957) resolve through the site's announced iden 0
//!   plan (base 851.00625 MHz, 6.25 kHz spacing) to **856.9375 and
//!   856.9875 MHz — both active voice channels of the same site**, granted
//!   to ordinary talkgroups by standard messages in the same captures. Two
//!   exact integer hits on the site's own channel plan are not a coincidence
//!   a wrong split would produce.
//! * The **second** field's values sit inside the system's talkgroup block —
//!   because it is the supergroup ID, which lives in the same numbering
//!   space as ordinary talkgroups (dispatch patches are commonly named by a
//!   dispatch talkgroup). This is also why those values appear in the 0x00
//!   talkgroup lists.
//! * The trailing 24 bits of 0x02 held values in the unit-ID ranges this
//!   system's standard grants independently reported — the source radio,
//!   exactly where the standard grant puts it.
//! * Six days of continuous monitoring produced **zero** standard voice
//!   grants for the known supergroups, while a consumer scanner on the same
//!   air played their calls — so the channel assignment must arrive in the
//!   messages the scanner implements and this decoder previously shelved.
//!
//! What 0x00 asks the radios to do remains inferred: it carries four
//! talkgroup-block IDs and no obvious channel, and public reporting calls it
//! an "add command". Whether one of its IDs is the supergroup and the rest
//! members (which would recover true patch membership) is not yet confirmed
//! against captured traffic, so it records no association. Everything else
//! stays [`Tsbk::VendorSpecific`](crate::tsbk::Tsbk::VendorSpecific) with raw
//! arguments, ready to be identified from a shared diagnostics log.

/// Motorola's manufacturer ID.
pub const MFID_MOTOROLA: u8 = 0x90;

/// A Motorola Group Regroup (talkgroup patch) message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MotoRegroup {
    /// 0x03 — GRG Channel Grant Update: patch calls in progress, as
    /// `(channel, supergroup)` pairs. A block often repeats one pair twice;
    /// an implausible pair is normalized to `(0, 0)` so the caller can skip
    /// it without re-deriving plausibility.
    GrgChannelUpdate {
        /// (channel, supergroup) pairs.
        pairs: [(u16, u16); 2],
    },
    /// 0x02 — GRG Channel Grant: a supergroup (patch) call assigned to a
    /// voice channel, with the granting radio. Mirrors the standard Group
    /// Voice Channel Grant.
    GrgChannelGrant {
        /// Service options, as in the standard grant (0x40 is the E bit).
        opts: u8,
        channel: u16,
        supergroup: u16,
        source_unit: u32,
    },
    /// 0x00 — Group Regroup add command: four talkgroup IDs being regrouped.
    ///
    /// No channel appears in the block, and which ID (if any) is the
    /// supergroup is unconfirmed, so this records no association on its own.
    RegroupAdd { talkgroups: [u16; 4] },
}

/// Parse a Motorola TSBK. Returns None for opcodes we do not recognize, or
/// when the fields are not plausible for the meaning claimed — in both cases
/// the caller keeps the block as vendor-specific with raw arguments rather
/// than inventing a decode.
pub fn parse(opcode: u8, args: u64) -> Option<MotoRegroup> {
    match opcode {
        0x00 => {
            let talkgroups = [
                (args >> 48) as u16,
                (args >> 32) as u16,
                (args >> 16) as u16,
                args as u16,
            ];
            // All four must look like talkgroups. An all-zero or all-ones
            // block is padding or a different message wearing this opcode.
            talkgroups
                .iter()
                .all(|&t| plausible_talkgroup(t))
                .then_some(MotoRegroup::RegroupAdd { talkgroups })
        }
        0x02 => {
            let opts = (args >> 56) as u8;
            let channel = ((args >> 40) & 0xFFFF) as u16;
            let supergroup = ((args >> 24) & 0xFFFF) as u16;
            let source_unit = (args & 0xFF_FFFF) as u32;
            (channel != 0 && plausible_talkgroup(supergroup) && source_unit != 0).then_some(
                MotoRegroup::GrgChannelGrant {
                    opts,
                    channel,
                    supergroup,
                    source_unit,
                },
            )
        }
        0x03 => {
            let raw = [
                (
                    ((args >> 48) & 0xFFFF) as u16,
                    ((args >> 32) & 0xFFFF) as u16,
                ),
                (((args >> 16) & 0xFFFF) as u16, (args & 0xFFFF) as u16),
            ];
            let pairs = raw.map(|(ch, sg)| {
                if ch != 0 && plausible_talkgroup(sg) {
                    (ch, sg)
                } else {
                    (0, 0)
                }
            });
            // At least one pair must be a real assignment; an all-padding
            // block is a different message wearing this opcode.
            pairs
                .iter()
                .any(|&(ch, _)| ch != 0)
                .then_some(MotoRegroup::GrgChannelUpdate { pairs })
        }
        _ => None,
    }
}

/// Name a Motorola opcode whose purpose is known even where its fields are
/// not. Naming a message is worth something on its own: it tells a reader that
/// unparsed traffic is understood-but-unused rather than a decode failure.
pub fn describe(opcode: u8) -> Option<&'static str> {
    match opcode {
        0x00 => Some("Group Regroup Add"),
        0x01 => Some("Group Regroup Delete"),
        0x02 => Some("Group Regroup Channel Grant"),
        0x03 => Some("Group Regroup Channel Grant Update"),
        0x05 => Some("System Broadcast"),
        0x09 => Some("Scan Marker"),
        0x0A => Some("Emergency Alarm"),
        0x0E => Some("Control Channel Shutdown"),
        _ => None,
    }
}

/// A talkgroup or supergroup ID that could be real. Zero is the null group and
/// 0xFFFF is the all-call/broadcast value; neither names a patch.
fn plausible_talkgroup(id: u16) -> bool {
    id != 0 && id != 0xFFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic values throughout: no observed talkgroup or unit ID from any
    // real system is committed to this repository.
    const CH: u16 = 0x03BD;
    const SG: u16 = 0x2001;
    const UNIT: u32 = 0x4A_0001;

    #[test]
    fn parses_a_channel_grant_update() {
        let args = ((CH as u64) << 48) | ((SG as u64) << 32) | ((CH as u64) << 16) | SG as u64;
        assert_eq!(
            parse(0x03, args),
            Some(MotoRegroup::GrgChannelUpdate {
                pairs: [(CH, SG), (CH, SG)]
            })
        );
    }

    #[test]
    fn update_keeps_the_real_pair_and_blanks_padding() {
        // One live assignment plus a padded slot: the block parses and the
        // padding comes back as (0, 0) rather than a confident pair.
        let args = ((CH as u64) << 48) | ((SG as u64) << 32);
        assert_eq!(
            parse(0x03, args),
            Some(MotoRegroup::GrgChannelUpdate {
                pairs: [(CH, SG), (0, 0)]
            })
        );
    }

    #[test]
    fn parses_a_channel_grant_with_its_unit() {
        let args = ((CH as u64) << 40) | ((SG as u64) << 24) | UNIT as u64;
        assert_eq!(
            parse(0x02, args),
            Some(MotoRegroup::GrgChannelGrant {
                opts: 0,
                channel: CH,
                supergroup: SG,
                source_unit: UNIT
            })
        );
    }

    #[test]
    fn grant_carries_the_encryption_bit() {
        let args = (0x40u64 << 56) | ((CH as u64) << 40) | ((SG as u64) << 24) | UNIT as u64;
        match parse(0x02, args) {
            Some(MotoRegroup::GrgChannelGrant { opts, .. }) => assert_eq!(opts & 0x40, 0x40),
            other => panic!("expected a channel grant, got {other:?}"),
        }
    }

    #[test]
    fn parses_a_patch_status_list() {
        let args =
            ((SG as u64) << 48) | (((SG + 1) as u64) << 32) | (((SG + 1) as u64) << 16) | SG as u64;
        assert_eq!(
            parse(0x00, args),
            Some(MotoRegroup::RegroupAdd {
                talkgroups: [SG, SG + 1, SG + 1, SG]
            })
        );
    }

    #[test]
    fn rejects_null_and_broadcast_group_ids() {
        // Padding and all-call values must not become grants; without this an
        // idle or unrelated block decodes as a confident patch call.
        assert_eq!(parse(0x00, 0), None);
        assert_eq!(parse(0x00, u64::MAX), None);
        assert_eq!(parse(0x03, 0), None);
        // A grant with no source radio is not a grant.
        let args = ((CH as u64) << 40) | ((SG as u64) << 24);
        assert_eq!(parse(0x02, args), None);
        // A grant to channel 0 is padding, not an assignment.
        let args = ((SG as u64) << 24) | UNIT as u64;
        assert_eq!(parse(0x02, args), None);
    }

    #[test]
    fn names_opcodes_whose_purpose_is_known_but_layout_is_not() {
        // Being able to name a message without parsing it keeps unparsed
        // traffic legible: "System Broadcast, not decoded" is a very different
        // report from "unknown opcode".
        assert_eq!(describe(0x05), Some("System Broadcast"));
        assert_eq!(describe(0x09), Some("Scan Marker"));
        // Still unidentified, here and publicly.
        assert_eq!(describe(0x0B), None);
        assert_eq!(describe(0x16), None);
    }

    #[test]
    fn leaves_unknown_opcodes_alone() {
        // Opcodes whose meaning has not been established must stay raw rather
        // than being forced into the nearest known shape.
        for op in [0x05u8, 0x09, 0x0B, 0x16, 0x3F] {
            assert_eq!(parse(op, 0x0123_4567_89AB_CDEF), None, "opcode {op:#04X}");
        }
    }
}
