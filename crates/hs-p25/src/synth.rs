//! Frame synthesis: build valid P25 dibit streams (with status symbols)
//! for loopback tests and the hs-bench corpus tools.

use crate::bits::bits_to_dibits;
use crate::nid::NidCodec;
use crate::voice::{interleave_imbe, ImbeFrame};
use crate::{trellis, tsbk, FRAME_SYNC};

/// FSW as 24 dibits.
pub fn sync_dibits() -> Vec<u8> {
    (0..24)
        .rev()
        .map(|i| ((FRAME_SYNC >> (2 * i)) & 3) as u8)
        .collect()
}

/// Insert status dibits at transmitted positions ≡ 35 (mod 36) from frame
/// start. Input: FS+NID+payload without status; output: over-the-air stream.
///
/// Call this once per frame, not once over several frames' content
/// concatenated together: `Framer` resets its own status-dibit counter
/// (`since_fs`) to the FSW length on every sync it finds, so each frame's
/// status dibits are numbered from *its own* FSW, not from a running count
/// since stream start. A multi-frame test fixture that wants back-to-back
/// frames must therefore call this per frame (as [`build_tsdu`] does) and,
/// when one frame's declared length leaves a status dibit due exactly where
/// the next frame's FSW would start, insert one deferred idle dibit (`0b01`)
/// between them by hand — the FSW itself is never interrupted, so the status
/// dibit is pushed to just before it rather than into it. See
/// `framer_e2e.rs`'s flywheel test for a worked example of the collision
/// case.
pub fn insert_status(frame: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(frame.len() + frame.len() / 35 + 1);
    let mut it = frame.iter();
    let mut pos = 0usize;
    loop {
        if pos % 36 == 35 {
            out.push(0b01); // idle status symbol
            pos += 1;
            continue;
        }
        match it.next() {
            Some(&d) => {
                out.push(d);
                pos += 1;
            }
            None => break,
        }
    }
    out
}

/// Build a complete TSDU over-the-air dibit stream for the given TSBKs
/// (each as (opcode, mfid, args); last block flagged automatically). See
/// [`insert_status`] for how to correctly chain several of these together.
pub fn build_tsdu(nac: u16, tsbks: &[(u8, u8, u64)]) -> Vec<u8> {
    assert!(!tsbks.is_empty() && tsbks.len() <= 3);
    let codec = NidCodec::new();
    let mut frame = sync_dibits();
    let nid = codec.encode(nac, 0x7);
    frame.extend((0..32).rev().map(|i| ((nid >> (2 * i)) & 3) as u8));
    for (k, &(op, mfid, args)) in tsbks.iter().enumerate() {
        let bits = tsbk::build(k == tsbks.len() - 1, op, mfid, args);
        let mut data = [0u8; 12];
        for (i, &b) in bits.iter().enumerate() {
            data[i / 8] |= b << (7 - i % 8);
        }
        frame.extend_from_slice(&trellis::encode(&data));
    }
    insert_status(&frame)
}

/// Build a terminator (TDU, no link control): sync and NID alone. This is
/// how a traffic channel says a transmission is over.
pub fn build_tdu(nac: u16) -> Vec<u8> {
    let codec = NidCodec::new();
    let mut frame = sync_dibits();
    let nid = codec.encode(nac, 0x3);
    frame.extend((0..32).rev().map(|i| ((nid >> (2 * i)) & 3) as u8));
    insert_status(&frame)
}

/// Build a complete LDU1 stream carrying the given nine IMBE frames.
/// Link-control bits are zeroed (v1 does not decode LC).
pub fn build_ldu1(nac: u16, imbe: &[ImbeFrame; 9]) -> Vec<u8> {
    let codec = NidCodec::new();
    let mut frame = sync_dibits();
    let nid = codec.encode(nac, 0x5);
    frame.extend((0..32).rev().map(|i| ((nid >> (2 * i)) & 3) as u8));

    let mut payload = vec![0u8; crate::voice::LDU_PAYLOAD_BITS];
    for (k, fr) in imbe.iter().enumerate() {
        let bits = interleave_imbe(fr);
        let off = crate::voice::IMBE_OFFSETS[k];
        payload[off..off + 144].copy_from_slice(&bits);
    }
    frame.extend(bits_to_dibits(&payload));
    insert_status(&frame)
}
