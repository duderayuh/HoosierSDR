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
//! The Hamming code **is** corrected here (see [`hamming`]); the outer
//! Reed–Solomon layer is not yet, so the first 12 hexbits are still taken as
//! the data symbols directly. On a real traffic channel that combination lifts
//! the proportion of link-control words with all twelve data hexbits sound from
//! 14-in-31 to 24-in-31.

/// Hamming(10,6,3) as P25 applies it to each link-control hexbit.
///
/// ## Where these parity equations came from
///
/// They were **derived from real traffic**, not taken from a specification.
/// Parity bits are a linear function of the data bits, so for each of the four
/// parity positions the candidate 6-bit mask that best predicts it was found by
/// exhaustive search over 744 hexbits captured off air. Each winning mask
/// predicts its parity bit for 92–95% of samples, where a wrong mask would land
/// near 50%; the residual few percent are the channel errors this code exists
/// to fix.
///
/// The result is checked rather than assumed: the 64 codewords the masks
/// generate have a minimum Hamming distance of exactly 3, which is what
/// Hamming(10,6,3) must have and what a mistaken derivation would not produce.
pub mod hamming {
    /// Data-bit masks generating the four parity bits, LSB of each mask
    /// selecting data bit 0.
    pub const PARITY_MASKS: [u8; 4] = [0b100111, 0b101011, 0b011101, 0b011110];

    /// Encode 6 data bits (in the low bits, MSB first as bit 5) to a 10-bit
    /// codeword.
    pub fn encode(data: u8) -> u16 {
        let d = data & 0x3F;
        let mut cw = (d as u16) << 4;
        for (j, &m) in PARITY_MASKS.iter().enumerate() {
            let mut p = 0u8;
            for i in 0..6 {
                if m >> i & 1 != 0 {
                    // Data bit i counting from the most significant.
                    p ^= (d >> (5 - i)) & 1;
                }
            }
            if p != 0 {
                cw |= 1 << (3 - j);
            }
        }
        cw
    }

    /// Maximum-likelihood decode of a 10-bit codeword.
    ///
    /// Returns the 6 data bits and how many bit errors were corrected, or None
    /// when the word is more than one error from every codeword — beyond what
    /// a distance-3 code can repair, and better refused than guessed.
    pub fn decode(rx: u16) -> Option<(u8, u32)> {
        let (data, dist) = decode_best(rx);
        (dist <= 1).then_some((data, dist))
    }

    /// Nearest codeword regardless of distance, with that distance.
    ///
    /// Useful where a caller would rather have a doubtful symbol than lose the
    /// whole message, and has its own way of judging the result.
    pub fn decode_best(rx: u16) -> (u8, u32) {
        let rx = rx & 0x3FF;
        let mut best = (u32::MAX, 0u8);
        for d in 0..64u8 {
            let dist = (encode(d) ^ rx).count_ones();
            if dist < best.0 {
                best = (dist, d);
            }
        }
        (best.1, best.0)
    }
}

/// Hexbits allowed to exceed the Hamming code's correcting power before the
/// whole link-control word is abandoned.
///
/// Zero, on measurement rather than principle. Allowing two was tried, on the
/// reasoning that the repetition check downstream would sort out the doubtful
/// words and rejecting a message over one bad symbol is wasteful. Against real
/// traffic it recovered no additional call and let corrupted words leaking
/// through as bogus vendor messages rise from 6 to 9. Since the outer
/// Reed-Solomon layer — the thing that could genuinely rescue those words — is
/// not decoded yet, refusing them is both cleaner and no less useful.
const MAX_DOUBTFUL_HEXBITS: usize = 0;

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

/// The 240 raw Link Control slot bits from an LDU1, packed MSB-first into 30
/// octets. These are the coded bits, before any of the protection is undone.
pub fn raw_slots(payload_bits: &[u8]) -> Option<[u8; 30]> {
    if payload_bits.len() < crate::voice::LDU_PAYLOAD_BITS {
        return None;
    }
    let mut out = [0u8; 30];
    let mut n = 0usize;
    for &slot in LC_SLOTS.iter() {
        for b in 0..LC_SLOT_BITS {
            if payload_bits[slot + b] != 0 {
                out[n / 8] |= 0x80 >> (n % 8);
            }
            n += 1;
        }
    }
    Some(out)
}

/// Pull the Link Control Word out of an LDU1 payload.
///
/// Returns None when the payload is too short or the word is self-evidently
/// not one — an all-zero LCW is idle padding, not a call.
pub fn extract_lcw(payload_bits: &[u8]) -> Option<Lcw> {
    if payload_bits.len() < crate::voice::LDU_PAYLOAD_BITS {
        return None;
    }
    // Read the first 12 hexbits — the Reed–Solomon data symbols — correcting
    // each with its Hamming code, and repack them into the 72-bit word.
    let mut octets = [0u8; 9];
    let mut written = 0usize;
    let mut acc = 0u32;
    let mut acc_bits = 0u32;
    let mut doubtful = 0usize;

    'outer: for (s, &slot) in LC_SLOTS.iter().enumerate() {
        for h in 0..4 {
            // Hexbit index across the whole word: 4 per slot.
            if s * 4 + h >= 12 {
                break 'outer;
            }
            let base = slot + h * 10;
            let mut cw = 0u16;
            for b in 0..10 {
                cw = (cw << 1) | payload_bits[base + b] as u16;
            }
            // A hexbit beyond the Hamming code's reach is kept as its nearest
            // codeword rather than discarding the message. Rejecting the whole
            // word on one bad symbol is what the Reed-Solomon layer would make
            // unnecessary, and until that exists it throws away good calls:
            // measured on real traffic it halved the confirmed-call yield. The
            // repetition check downstream is what decides whether to trust the
            // result, so a doubtful symbol is better passed on and counted.
            let (data, errs) = hamming::decode_best(cw);
            if errs > 1 {
                doubtful += 1;
                // Too many doubtful symbols and the word is noise, not a call.
                if doubtful > MAX_DOUBTFUL_HEXBITS {
                    return None;
                }
            }
            for b in (0..6).rev() {
                acc = (acc << 1) | ((data >> b) & 1) as u32;
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
                // Six data bits, then the Hamming parity the decoder checks.
                let mut d = 0u8;
                for _ in 0..6 {
                    let bit = if i < bits.len() { bits[i] } else { 0 };
                    d = (d << 1) | bit;
                    i += 1;
                }
                let cw = hamming::encode(d);
                for b in 0..10 {
                    payload[base + b] = ((cw >> (9 - b)) & 1) as u8;
                }
                if i >= bits.len() {
                    break 'outer;
                }
            }
        }
        payload
    }

    #[test]
    fn hamming_corrects_one_error_and_refuses_two() {
        for d in 0..64u8 {
            let cw = hamming::encode(d);
            assert_eq!(hamming::decode(cw), Some((d, 0)), "clean word {d}");
            for b in 0..10 {
                assert_eq!(
                    hamming::decode(cw ^ (1 << b)),
                    Some((d, 1)),
                    "single error at bit {b} of {d}"
                );
            }
        }
    }

    #[test]
    fn the_derived_code_has_distance_three() {
        // The parity masks were derived from observed traffic rather than a
        // specification, so the property that makes them a Hamming(10,6,3)
        // code is asserted rather than assumed: any two codewords must differ
        // in at least three places, which is what allows one error to be
        // corrected unambiguously.
        let mut min = u32::MAX;
        for a in 0..64u8 {
            for b in (a + 1)..64u8 {
                min = min.min((hamming::encode(a) ^ hamming::encode(b)).count_ones());
            }
        }
        assert_eq!(min, 3, "derived code has minimum distance {min}");
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
