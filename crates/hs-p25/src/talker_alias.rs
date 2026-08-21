//! Over-the-air talker alias: a radio's *name*, sent by the system itself.
//!
//! Motorola systems can broadcast a unit's alias alongside its voice, as
//! manufacturer-specific Link Control words on the traffic channel (MFID
//! 0x90). Public enthusiast reporting names the opcodes — a header word
//! followed by data-block words — but not the field layout, and this project
//! ports nothing from GPL decoders. So the layout is **not** assumed here.
//!
//! What this does instead is the conservative thing that needs no layout:
//! gather the argument octets of the alias words of one transmission in the
//! order they arrive, and accept the longest run of printable ASCII in them
//! as the alias — but only after the same text has been seen twice, the
//! repetition rule the rest of the Link Control path already uses. A wrong
//! guess about the encoding therefore yields *no* alias rather than a wrong
//! one, and [`Diagnostics`](../../hs_core/diag) keeps the raw words so a
//! real capture can refine this into a proper parser.
//!
//! Status: parser-tested on synthetic words; not yet confirmed against a live
//! capture (no recording in the corpus carries alias words).

use crate::lc::Lcw;

pub const MFID_MOTOROLA: u8 = 0x90;
/// Motorola talker-alias header (public reporting; layout not assumed).
pub const LCO_MOTO_TALKER_ALIAS_HEADER: u8 = 0x15;
/// Motorola talker-alias data block.
pub const LCO_MOTO_TALKER_ALIAS_BLOCK: u8 = 0x17;

/// Shortest text accepted as an alias. Three printable bytes turn up by
/// chance in binary fields; four in a row, twice, do not.
const MIN_ALIAS_CHARS: usize = 4;
/// Most octets kept per transmission (a header plus a handful of blocks).
const MAX_BYTES: usize = 64;

#[derive(Debug, Default)]
pub struct TalkerAliasAssembler {
    bytes: Vec<u8>,
    /// Candidate alias texts and how often each has been seen.
    seen: Vec<(String, u32)>,
    confirmed: Option<String>,
}

impl TalkerAliasAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Is this word part of an alias broadcast?
    pub fn is_alias_word(lcw: &Lcw) -> bool {
        lcw.mfid == MFID_MOTOROLA
            && matches!(
                lcw.lco,
                LCO_MOTO_TALKER_ALIAS_HEADER | LCO_MOTO_TALKER_ALIAS_BLOCK
            )
    }

    /// Offer a Link Control word. Returns the alias the first time it is
    /// confirmed (seen twice); `None` otherwise, including for words that are
    /// not alias words at all.
    pub fn observe(&mut self, lcw: &Lcw) -> Option<String> {
        if !Self::is_alias_word(lcw) {
            return None;
        }
        if lcw.lco == LCO_MOTO_TALKER_ALIAS_HEADER {
            // A header restarts the sequence; the previous one is complete.
            self.bytes.clear();
        }
        if self.bytes.len() + lcw.args.len() <= MAX_BYTES {
            self.bytes.extend_from_slice(&lcw.args);
        }
        let text = longest_printable_run(&self.bytes)?;
        if text.len() < MIN_ALIAS_CHARS {
            return None;
        }
        let n = match self.seen.iter_mut().find(|(t, _)| *t == text) {
            Some((_, n)) => {
                *n += 1;
                *n
            }
            None => {
                self.seen.push((text.clone(), 1));
                1
            }
        };
        if n == 2 && self.confirmed.is_none() {
            self.confirmed = Some(text.clone());
            return Some(text);
        }
        None
    }

    /// The confirmed alias for this transmission, if any.
    pub fn alias(&self) -> Option<&str> {
        self.confirmed.as_deref()
    }

    /// The raw argument octets gathered so far (for diagnostics).
    pub fn raw(&self) -> &[u8] {
        &self.bytes
    }
}

/// The longest run of printable ASCII (space through tilde), trimmed.
fn longest_printable_run(bytes: &[u8]) -> Option<String> {
    let mut best: &[u8] = &[];
    let mut start = None;
    for (i, &b) in bytes.iter().chain(std::iter::once(&0u8)).enumerate() {
        let printable = (0x20..=0x7E).contains(&b);
        match (printable, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                if i - s > best.len() {
                    best = &bytes[s..i];
                }
                start = None;
            }
            _ => {}
        }
    }
    let text = std::str::from_utf8(best).ok()?.trim();
    (!text.is_empty()).then(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(lco: u8, args: [u8; 7]) -> Lcw {
        Lcw {
            protected: false,
            lco,
            mfid: MFID_MOTOROLA,
            args,
        }
    }

    #[test]
    fn assembles_printable_text_and_confirms_on_repeat() {
        let mut a = TalkerAliasAssembler::new();
        let header = word(LCO_MOTO_TALKER_ALIAS_HEADER, [0x01, 0x02, 0, 0, 0, b'E', b'N']);
        let block = word(LCO_MOTO_TALKER_ALIAS_BLOCK, [b'G', b' ', b'2', b'1', 0, 0, 0]);
        assert_eq!(a.observe(&header), None, "too short on its own");
        assert_eq!(a.observe(&block), None, "seen once");
        assert_eq!(a.observe(&header), None);
        assert_eq!(a.observe(&block), Some("ENG 21".into()), "confirmed");
        assert_eq!(a.alias(), Some("ENG 21"));
        // Further repeats do not report again.
        a.observe(&header);
        assert_eq!(a.observe(&block), None);
    }

    #[test]
    fn binary_fields_do_not_become_aliases() {
        let mut a = TalkerAliasAssembler::new();
        let w = word(LCO_MOTO_TALKER_ALIAS_BLOCK, [0x00, 0x41, 0x00, 0x42, 0xFF, 0x43, 0x00]);
        for _ in 0..4 {
            assert_eq!(a.observe(&w), None);
        }
        assert_eq!(a.alias(), None);
    }

    #[test]
    fn ignores_words_that_are_not_alias_words() {
        let mut a = TalkerAliasAssembler::new();
        let mut w = word(0x00, *b"ABCDEFG");
        w.mfid = 0;
        assert_eq!(a.observe(&w), None);
        assert_eq!(a.observe(&w), None);
        assert!(a.raw().is_empty());
    }

    #[test]
    fn longest_run_is_chosen_and_trimmed() {
        assert_eq!(longest_printable_run(b"\x00AB\x00 CAR 12 \x01"), Some("CAR 12".into()));
        assert_eq!(longest_printable_run(b"\x00\x01"), None);
    }
}
