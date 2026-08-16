//! Continuous streaming decode: pull IQ from any [`SdrSource`] in blocks and
//! feed the stateful [`ChannelDecoder`], emitting decode output as it arrives.
//!
//! This is the "live" path — the same loop drives a file replay or a live SDR
//! (an RTL-SDR via `hs-source`'s `rtlsdr` feature). The decoder is stateful
//! across blocks, so block boundaries are invisible to the demodulator.

use crate::decoder::{ChannelDecoder, DecodeOutput};
use hs_source::{SdrSource, SourceError};

/// Cumulative stats over a streaming session.
#[derive(Debug, Default, Clone)]
pub struct StreamStats {
    pub blocks: u64,
    pub iq_samples: u64,
    pub syncs: u32,
    pub grants: usize,
    pub pcm_samples: usize,
    pub encrypted_skips: usize,
}

/// Read from `src` in blocks of `block_pairs` IQ pairs, decode continuously,
/// and invoke `on_output` for each non-empty block result (a resolved grant,
/// decoded audio, or an encryption skip). Returns when the source reports EOF.
///
/// `on_output` is where a caller routes audio to a device, appends to a WAV,
/// or updates a UI — it runs in the capture loop, so keep it cheap.
pub fn run<S: SdrSource>(
    src: &mut S,
    dec: &mut ChannelDecoder,
    block_pairs: usize,
    mut on_output: impl FnMut(&DecodeOutput),
) -> Result<StreamStats, SourceError> {
    let mut buf = vec![0.0f32; block_pairs * 2];
    let mut stats = StreamStats::default();
    loop {
        match src.read(&mut buf) {
            Ok(0) => continue,
            Ok(n) => {
                let out = dec.process(&buf[..n]);
                stats.blocks += 1;
                stats.iq_samples += (n / 2) as u64;
                stats.syncs += out.syncs;
                stats.grants += out.grants.len();
                stats.pcm_samples += out.pcm.len();
                stats.encrypted_skips += out.encrypted_skips.len();
                if !out.grants.is_empty() || !out.pcm.is_empty() || !out.encrypted_skips.is_empty()
                {
                    on_output(&out);
                }
            }
            Err(SourceError::Eof) => break,
            Err(e) => return Err(e),
        }
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::ChannelDecoder;
    use hs_source::IqFileSource;

    /// Build a C4FM demo transmission as interleaved-f32 bytes.
    fn demo_bytes() -> Vec<u8> {
        use hs_dsp::modulator::C4fmModulator;
        use hs_dsp::C32;
        use hs_p25::synth::{build_ldu1, build_tsdu};
        use hs_p25::voice::ImbeFrame;

        let iden =
            (1u64 << 60) | (100u64 << 51) | (1u64 << 50) | (100u64 << 32) | (851_012_500u64 / 5);
        let channel = (1u64 << 12) | 10;
        let grant = (channel << 40) | (0x2F93u64 << 24) | 0xBEEF1;
        let mut stream = build_tsdu(0x293, &[(0x3D, 0, iden), (0x00, 0, grant)]);
        let mut frames: [ImbeFrame; 9] = [[[0u8; 23]; 8]; 9];
        let widths = [23usize, 23, 23, 23, 15, 15, 15, 7];
        for (k, fr) in frames.iter_mut().enumerate() {
            for (w, row) in fr.iter_mut().enumerate() {
                for (x, cell) in row.iter_mut().enumerate().take(widths[w]) {
                    *cell = (((k + 1) * (w + 2) * (x + 5)) % 2) as u8;
                }
            }
        }
        stream.extend(build_ldu1(0x293, &frames));

        let mut m = C4fmModulator::new(48000.0);
        let mut iq: Vec<C32> = Vec::new();
        for i in 0..400 {
            m.modulate(if i % 2 == 0 { 0b01 } else { 0b11 }, &mut iq);
        }
        for &d in &stream {
            m.modulate(d, &mut iq);
        }
        for _ in 0..200 {
            m.modulate(0b00, &mut iq);
        }
        let mut bytes = Vec::with_capacity(iq.len() * 8);
        for c in iq {
            bytes.extend_from_slice(&c.re.to_le_bytes());
            bytes.extend_from_slice(&c.im.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn streams_blocks_and_decodes_across_boundaries() {
        // Feed the demo through a file source in SMALL blocks, so the frame is
        // split across many block boundaries — the decoder's cross-block state
        // must still recover the grant and the voice.
        let bytes = demo_bytes();
        let cursor = std::io::Cursor::new(bytes);
        let mut src = IqFileSource::new(cursor, 48000.0, 851_000_000.0);
        let mut dec = ChannelDecoder::new(48000.0, crate::decoder::EqMode::Bypass);

        let mut grants_seen = 0;
        let stats = run(&mut src, &mut dec, 512, |out| {
            grants_seen += out.grants.len();
        })
        .expect("stream ok");

        assert!(
            stats.blocks > 1,
            "expected multiple blocks, got {}",
            stats.blocks
        );
        assert!(stats.syncs >= 1, "no sync across streamed blocks");
        assert!(grants_seen >= 1, "grant not recovered from streamed blocks");
        assert_eq!(stats.pcm_samples, 9 * 160, "voice not decoded from stream");
    }
}
