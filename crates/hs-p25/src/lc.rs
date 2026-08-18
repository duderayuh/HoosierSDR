//! Link Control: the call metadata a voice channel carries about itself.
//!
//! Everything the trunking layer knows comes from the control channel — which
//! means a traffic channel decoded on its own is anonymous audio. Link Control
//! fixes that. Each LDU1 embeds a 72-bit Link Control Word naming the talkgroup
//! and the transmitting radio, so a voice channel identifies its own call with
//! no control channel present at all.
//!
//! It is also where Motorola puts the over-the-air alias — the system
//! broadcasting its own talkgroup and unit *names*, which is the one path to
//! human-readable labels that needs no external database.
//!
//! ## Extraction
//!
//! The LCW rides in the six 40-bit gaps between IMBE frames 2..8 of an LDU1:
//! 240 bits holding 24 hexbits of 10 bits each, where each hexbit is 6 data
//! bits plus a Hamming(10,6,3) parity tail. Those 24 hexbits are an
//! RS(24,12,13) codeword over GF(64) whose first 12 symbols are the 72-bit
//! LCW.
//!
//! Neither code is *corrected* here — the data bits are taken directly, as
//! `voice::ldu2_algid_raw` already does for the encryption sync. That is a
//! deliberate first step, not an oversight: it costs nothing on a strong
//! signal, and the result is checked for self-consistency instead. Adding
//! Hamming and Reed–Solomon correction is a contained upgrade that would only
//! extend the range over which this works.

/// Bit offsets of the six link-control slots inside an LDU payload.
pub const LC_SLOTS: [usize; 6] = [288, 472, 656, 840, 1024, 1208];
/// Bits per slot.
pub const LC_SLOT_BITS: usize = 40;

/// A decoded Link Control Word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lcw {
    /// Protected (encrypted) flag.
    pub protected: bool,
    /// Link Control Opcode.
    pub lco: u8,
    /// Manufacturer ID; non-zero means the opcode is vendor-defined.
    pub mfid: u8,
    /// The seven argument octets, unparsed.
    pub args: [u8; 7],
}

impl Lcw {
    /// Standard LCO for Group Voice Channel User — who is talking, and on
    /// which talkgroup.
    pub const LCO_GROUP_VOICE_USER: u8 = 0x00;

    /// Talkgroup and source unit, for a Group Voice Channel User word.
    ///
    /// Returns None for any other opcode, or for a vendor-defined one, since
    /// the argument layout is only known for the standard message.
    pub fn group_voice_user(&self) -> Option<(u16, u32)> {
        if self.lco != Self::LCO_GROUP_VOICE_USER || !self.is_standard() {
            return None;
        }
        let talkgroup = u16::from_be_bytes([self.args[2], self.args[3]]);
        let source = u32::from_be_bytes([0, self.args[4], self.args[5], self.args[6]]);
        // A call has both a talkgroup and a radio; a word missing either is a
        // corrupted read, not a transmission.
        (talkgroup != 0 && source != 0).then_some((talkgroup, source))
    }

    /// True when the opcode carries its standard meaning.
    pub fn is_standard(&self) -> bool {
        self.mfid == 0x00 || self.mfid == 0x01
    }

    /// Emergency flag from the service options octet of a group voice word.
    pub fn emergency(&self) -> bool {
        self.lco == Self::LCO_GROUP_VOICE_USER && self.is_standard() && self.args[0] & 0x80 != 0
    }
}

/// Pull the Link Control Word out of an LDU1 payload.
///
/// Returns None when the payload is too short or the word is self-evidently
/// not one — an all-zero LCW is idle padding, not a call.
pub fn extract_lcw(payload_bits: &[u8]) -> Option<Lcw> {
    if payload_bits.len() < crate::voice::LDU_PAYLOAD_BITS {
        return None;
    }
    // Gather the 240 slot bits, then take the 6 data bits of each 10-bit
    // hexbit and keep the first 12 hexbits: the RS data symbols.
    let mut octets = [0u8; 9];
    let mut written = 0usize;
    let mut acc = 0u32;
    let mut acc_bits = 0u32;

    'outer: for (s, &slot) in LC_SLOTS.iter().enumerate() {
        for h in 0..4 {
            // Hexbit index across the whole word: 4 per slot.
            if s * 4 + h >= 12 {
                break 'outer;
            }
            let base = slot + h * 10;
            for b in 0..6 {
                acc = (acc << 1) | payload_bits[base + b] as u32;
                acc_bits += 1;
                if acc_bits == 8 {
                    octets[written] = acc as u8;
                    written += 1;
                    acc = 0;
                    acc_bits = 0;
                    if written == 9 {
                        break 'outer;
                    }
                }
            }
        }
    }
    if written < 9 {
        return None;
    }
    if octets.iter().all(|&o| o == 0) {
        return None;
    }

    let mut args = [0u8; 7];
    args.copy_from_slice(&octets[2..9]);
    Some(Lcw {
        protected: octets[0] & 0x80 != 0,
        lco: octets[0] & 0x3F,
        mfid: octets[1],
        args,
    })
}

/// Confirms Link Control words by repetition.
///
/// The words extracted here are not error-corrected, so individual reads are
/// unreliable — on a real traffic channel most come back damaged, and a damaged
/// word can still parse into a plausible talkgroup and radio. Repetition is the
/// defence available without implementing the codes: a transmitter sends the
/// same Link Control Word in every LDU1 of a transmission, so a reading that
/// appears twice is almost certainly right, while noise rarely produces the
/// same wrong answer twice.
///
/// This is a weaker guarantee than Hamming and Reed–Solomon correction would
/// give, and it costs one frame of latency, but it needs no knowledge of the
/// codes and turns an unreliable stream into a trustworthy one.
#[derive(Debug, Default)]
pub struct LcConfirmer {
    seen: Vec<((u16, u32), u32)>,
}

impl LcConfirmer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Offer a decoded word; returns the call once it has been seen enough
    /// times to trust, and only on the reading that confirms it.
    pub fn observe(&mut self, lcw: &Lcw) -> Option<(u16, u32)> {
        let key = lcw.group_voice_user()?;
        match self.seen.iter_mut().find(|(k, _)| *k == key) {
            Some((_, n)) => {
                *n += 1;
                // Report exactly once, on the second sighting.
                (*n == 2).then_some(key)
            }
            None => {
                self.seen.push((key, 1));
                None
            }
        }
    }

    /// Every confirmed call, with how many times each was seen.
    pub fn confirmed(&self) -> Vec<((u16, u32), u32)> {
        self.seen.iter().filter(|(_, n)| *n >= 2).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lay a 72-bit LCW into an LDU payload the way a transmitter would, so
    /// extraction can be checked against a known word.
    fn embed(octets: &[u8; 9]) -> Vec<u8> {
        let mut payload = vec![0u8; crate::voice::LDU_PAYLOAD_BITS];
        let bits: Vec<u8> = octets
            .iter()
            .flat_map(|o| (0..8).rev().map(move |k| (o >> k) & 1))
            .collect();
        let mut i = 0;
        'outer: for &slot in LC_SLOTS.iter() {
            for h in 0..4 {
                let base = slot + h * 10;
                for b in 0..6 {
                    if i >= bits.len() {
                        break 'outer;
                    }
                    payload[base + b] = bits[i];
                    i += 1;
                }
            }
        }
        payload
    }

    #[test]
    fn extracts_a_group_voice_word() {
        // Synthetic values: nothing observed off air is committed here.
        let (tg, src) = (0x2F93u16, 0x0B_EEF1u32);
        let mut o = [0u8; 9];
        o[0] = Lcw::LCO_GROUP_VOICE_USER;
        o[1] = 0x00; // standard MFID
        o[4] = (tg >> 8) as u8;
        o[5] = tg as u8;
        o[6] = (src >> 16) as u8;
        o[7] = (src >> 8) as u8;
        o[8] = src as u8;

        let lcw = extract_lcw(&embed(&o)).expect("word extracts");
        assert_eq!(lcw.lco, Lcw::LCO_GROUP_VOICE_USER);
        assert!(lcw.is_standard());
        assert_eq!(lcw.group_voice_user(), Some((tg, src)));
        assert!(!lcw.emergency());
    }

    #[test]
    fn reads_the_emergency_flag() {
        let mut o = [0u8; 9];
        o[2] = 0x80; // service options: emergency
        o[4] = 0x2F;
        o[5] = 0x93;
        let lcw = extract_lcw(&embed(&o)).expect("word extracts");
        assert!(lcw.emergency(), "emergency bit missed: {lcw:?}");
    }

    #[test]
    fn refuses_to_interpret_a_vendor_word_as_standard() {
        // Same trap as the TSBK path: a vendor opcode wearing LCO 0x00 must
        // not yield a confident talkgroup that means something else.
        let mut o = [0u8; 9];
        o[1] = 0x90; // Motorola
        o[4] = 0x2F;
        o[5] = 0x93;
        let lcw = extract_lcw(&embed(&o)).expect("word extracts");
        assert_eq!(lcw.mfid, 0x90);
        assert!(!lcw.is_standard());
        assert_eq!(lcw.group_voice_user(), None);
    }

    #[test]
    fn a_call_is_confirmed_only_when_it_repeats() {
        let word = |tg: u16, src: u32| Lcw {
            protected: false,
            lco: Lcw::LCO_GROUP_VOICE_USER,
            mfid: 0,
            args: [
                0,
                0,
                (tg >> 8) as u8,
                tg as u8,
                (src >> 16) as u8,
                (src >> 8) as u8,
                src as u8,
            ],
        };
        let mut c = LcConfirmer::new();
        // A one-off reading is not trusted: a corrupted word parses just as
        // cleanly as a real one.
        assert_eq!(c.observe(&word(0x2F93, 100)), None);
        assert_eq!(c.observe(&word(0x1111, 999)), None, "noise, seen once");
        // The repeat confirms it, and reports only once.
        assert_eq!(c.observe(&word(0x2F93, 100)), Some((0x2F93, 100)));
        assert_eq!(c.observe(&word(0x2F93, 100)), None, "already reported");
        assert_eq!(c.confirmed(), vec![((0x2F93, 100), 3)]);
    }

    #[test]
    fn a_word_without_a_radio_is_a_corrupted_read() {
        let mut o = [0u8; 9];
        o[4] = 0x2F;
        o[5] = 0x93; // talkgroup but no source unit
        let lcw = extract_lcw(&embed(&o)).expect("word extracts");
        assert_eq!(lcw.group_voice_user(), None);
    }

    #[test]
    fn an_idle_payload_is_not_a_link_control_word() {
        let payload = vec![0u8; crate::voice::LDU_PAYLOAD_BITS];
        assert_eq!(extract_lcw(&payload), None);
        assert_eq!(extract_lcw(&[0u8; 10]), None, "short payload");
    }
}
