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
    /// 0x03 — associates talkgroups with a patch (supergroup). Two pairs per
    /// block; a block often repeats the same pair twice.
    PatchAdd {
        /// (supergroup, member talkgroup) pairs.
        pairs: [(u16, u16); 2],
    },
    /// 0x02 — a unit operating on a patched talkgroup.
    PatchUser {
        supergroup: u16,
        talkgroup: u16,
        unit: u32,
    },
    /// 0x00 — four talkgroup IDs, the currently regrouped set.
    PatchStatus { talkgroups: [u16; 4] },
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
                .then_some(MotoRegroup::PatchStatus { talkgroups })
        }
        0x02 => {
            let supergroup = ((args >> 40) & 0xFFFF) as u16;
            let talkgroup = ((args >> 24) & 0xFFFF) as u16;
            let unit = (args & 0xFF_FFFF) as u32;
            (plausible_talkgroup(supergroup) && plausible_talkgroup(talkgroup) && unit != 0)
                .then_some(MotoRegroup::PatchUser {
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
                .then_some(MotoRegroup::PatchAdd { pairs })
        }
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
            Some(MotoRegroup::PatchAdd {
                pairs: [(SG, TG), (SG, TG)]
            })
        );
    }

    #[test]
    fn parses_a_patch_user_with_its_unit() {
        let args = ((SG as u64) << 40) | ((TG as u64) << 24) | UNIT as u64;
        assert_eq!(
            parse(0x02, args),
            Some(MotoRegroup::PatchUser {
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
            Some(MotoRegroup::PatchStatus {
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
    fn leaves_unknown_opcodes_alone() {
        // Opcodes whose meaning has not been established must stay raw rather
        // than being forced into the nearest known shape.
        for op in [0x05u8, 0x09, 0x0B, 0x16, 0x3F] {
            assert_eq!(parse(op, 0x0123_4567_89AB_CDEF), None, "opcode {op:#04X}");
        }
    }
}
