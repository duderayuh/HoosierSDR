//! Motorola manufacturer-specific TSBKs (MFID 0x90).
//!
//! TIA-102 lets a manufacturer define its own opcode space, and on a Motorola
//! P25 system that space carries more traffic than the standard one. A 60
//! second Marion County control channel held 599 Motorola blocks against 208
//! standard voice grants — so ignoring them means ignoring most of what the
//! system is saying.
//!
//! The messages decoded here are **Group Regroup**: dynamic talkgroup
//! patching, where dispatch merges several talkgroups so they share audio. A
//! scanner that does not track patches mis-attributes calls, because traffic
//! for one talkgroup appears under another.
//!
//! ## Cross-checked against public reporting
//!
//! Enthusiast documentation names this opcode family **Group Regroup (GRG)**:
//! 0x00 an add command, 0x01 a delete, 0x02 and 0x03 grant and update. That
//! matches what the traffic shows and the names below follow it.
//!
//! One published description does not survive contact with the bits, and the
//! disagreement is worth recording. 0x02 is often summarised as assigning a
//! supergroup *to a channel*, which would make its middle field a channel
//! number rather than a talkgroup. On the observed system that field decodes
//! as channel iden 2 — an iden the system never announces; its IDEN_UP
//! messages define only iden 0 (851.00625 MHz) and iden 1 (762.00625 MHz).
//! Read as a talkgroup the same bits give a value inside the system's own
//! talkgroup block, and the same value appears in the 0x00 talkgroup lists.
//! So the field is treated as a talkgroup here.
//!
//! Two further opcodes are named by [`describe`] but not parsed, because their
//! *purpose* is publicly reported while their field layouts are not: 0x05 is a
//! Motorola System Broadcast of system parameters, and 0x09 a Scan Marker
//! broadcast.
//!
//! Both were cross-checked against a raw capture of a different network (Ohio
//! MARCS, NAC 0x341, posted publicly) and the agreement is close enough to
//! confirm this decoder's byte alignment independently:
//!
//! * on 0x05, six of the eight argument bytes are **identical** across the two
//!   systems, with only the leading two differing — exactly what a broadcast of
//!   mostly network-wide default parameters should look like;
//! * on 0x09, the other network's value occupies the same 9-bit field this one
//!   uses, and is likewise an exact multiple of five, which no accident of
//!   framing would reproduce.
//!
//! Opcodes 0x0B and 0x16 remain unidentified here and, as far as public
//! discussion goes, everywhere.
//!
//! ## Provenance and confidence
//!
//! There is no public specification for these opcodes. The field layouts below
//! were derived from observed traffic, and the reasoning is recorded here so a
//! future reader can judge it rather than trust it:
//!
//! * **0x02** splits as `reserved(8) | supergroup(16) | talkgroup(16) |
//!   unit(24)`. The trailing 24 bits held values in the same two unit-ID
//!   ranges that this system's *standard* grants independently reported, and
//!   the 16-bit talkgroup field matched its known talkgroup block. Two
//!   independent field types landing in the right ranges at the right offsets
//!   is not a coincidence a wrong split would produce.
//! * **0x03** splits as two `(supergroup, talkgroup)` pairs. The supergroup
//!   values observed — a small set in the hundreds — were exactly the values
//!   seen in the supergroup position of 0x02, tying the two messages together.
//! * **0x00** carries four 16-bit talkgroup IDs, all within the system's
//!   talkgroup block and clustered on adjacent values.
//!
//! What is *inferred* rather than known is the semantics: which field is the
//! patch and which the member, and what 0x00 asks the radios to do. The names
//! below reflect that, and the parsers reject implausible values rather than
//! reporting a confident guess. Everything else stays
//! [`Tsbk::VendorSpecific`](crate::tsbk::Tsbk::VendorSpecific) with raw
//! arguments, ready to be identified from a shared diagnostics log.

/// Motorola's manufacturer ID.
pub const MFID_MOTOROLA: u8 = 0x90;

/// A Motorola Group Regroup (talkgroup patch) message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MotoRegroup {
    /// 0x03 — Group Regroup update: associates talkgroups with a supergroup.
    /// Two pairs per block; a block often repeats the same pair twice.
    RegroupUpdate {
        /// (supergroup, member talkgroup) pairs.
        pairs: [(u16, u16); 2],
    },
    /// 0x02 — Group Regroup grant: a unit operating on a regrouped talkgroup.
    RegroupGrant {
        supergroup: u16,
        talkgroup: u16,
        unit: u32,
    },
    /// 0x00 — Group Regroup add command: four talkgroup IDs being regrouped.
    ///
    /// No supergroup appears in the block, so this names the members without
    /// saying which patch they join. It therefore records no association on
    /// its own; 0x02 and 0x03 supply that.
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
            let supergroup = ((args >> 40) & 0xFFFF) as u16;
            let talkgroup = ((args >> 24) & 0xFFFF) as u16;
            let unit = (args & 0xFF_FFFF) as u32;
            (plausible_talkgroup(supergroup) && plausible_talkgroup(talkgroup) && unit != 0)
                .then_some(MotoRegroup::RegroupGrant {
                    supergroup,
                    talkgroup,
                    unit,
                })
        }
        0x03 => {
            let pairs = [
                (
                    ((args >> 48) & 0xFFFF) as u16,
                    ((args >> 32) & 0xFFFF) as u16,
                ),
                (((args >> 16) & 0xFFFF) as u16, (args & 0xFFFF) as u16),
            ];
            pairs
                .iter()
                .all(|&(sg, tg)| plausible_talkgroup(sg) && plausible_talkgroup(tg))
                .then_some(MotoRegroup::RegroupUpdate { pairs })
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
        0x02 => Some("Group Regroup Grant"),
        0x03 => Some("Group Regroup Update"),
        0x05 => Some("System Broadcast"),
        0x09 => Some("Scan Marker"),
        0x0A => Some("Emergency Alarm"),
        0x0E => Some("Control Channel Shutdown"),
        _ => None,
    }
}

/// A talkgroup or supergroup ID that could be real. Zero is the null group and
/// 0xFFFF is the all-call/broadcast value; neither names a patch member.
fn plausible_talkgroup(id: u16) -> bool {
    id != 0 && id != 0xFFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic values throughout: no observed talkgroup or unit ID from any
    // real system is committed to this repository.
    const SG: u16 = 0x0301;
    const TG: u16 = 0x2001;
    const UNIT: u32 = 0x4A_0001;

    #[test]
    fn parses_a_patch_add() {
        let args = ((SG as u64) << 48) | ((TG as u64) << 32) | ((SG as u64) << 16) | TG as u64;
        assert_eq!(
            parse(0x03, args),
            Some(MotoRegroup::RegroupUpdate {
                pairs: [(SG, TG), (SG, TG)]
            })
        );
    }

    #[test]
    fn parses_a_patch_user_with_its_unit() {
        let args = ((SG as u64) << 40) | ((TG as u64) << 24) | UNIT as u64;
        assert_eq!(
            parse(0x02, args),
            Some(MotoRegroup::RegroupGrant {
                supergroup: SG,
                talkgroup: TG,
                unit: UNIT
            })
        );
    }

    #[test]
    fn parses_a_patch_status_list() {
        let args =
            ((TG as u64) << 48) | (((TG + 1) as u64) << 32) | (((TG + 1) as u64) << 16) | TG as u64;
        assert_eq!(
            parse(0x00, args),
            Some(MotoRegroup::RegroupAdd {
                talkgroups: [TG, TG + 1, TG + 1, TG]
            })
        );
    }

    #[test]
    fn rejects_null_and_broadcast_group_ids() {
        // Padding and all-call values must not become patch members; without
        // this an idle or unrelated block decodes as a confident patch.
        assert_eq!(parse(0x00, 0), None);
        assert_eq!(parse(0x00, u64::MAX), None);
        assert_eq!(parse(0x03, 0), None);
        // A user message with no unit is not a user message.
        let args = ((SG as u64) << 40) | ((TG as u64) << 24);
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
