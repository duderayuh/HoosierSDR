//! hs-bench: the measuring instrument, built before the thing being measured.
//!
//! Two modes:
//!   * `synth` (default): generate a P25 transmission, pass it through a
//!     configurable two-ray + AWGN channel, and report decode metrics with
//!     the equalizer ENABLED vs BYPASSED — the A/B number that tests the
//!     project thesis without needing a field recording.
//!   * `file <path>`: run a raw interleaved-f32 IQ recording (`.cf32`) at a
//!     given sample rate through the decoder and report metrics. This is the
//!     entry point for the SAFE-T corpus once captured.
//!
//! Baselines from SDRTrunk / OP25 / GopherTrunk on the same corpus belong in
//! `results/baselines.md` (checked in); the corpus IQ itself is never
//! committed — see .gitignore.

mod channel;

use hs_core::decoder::{ChannelDecoder, EqMode};
use hs_p25::synth::{build_ldu1, build_tsdu};
use hs_p25::voice::ImbeFrame;
use std::io::Read;

const RATE: f64 = 48000.0;

#[derive(Default, Debug)]
struct Metrics {
    syncs: u32,
    grants: usize,
    pcm_samples: usize,
    #[allow(dead_code)] // reported in file mode, not the synth A/B table
    encrypted_skips: usize,
}

fn build_test_stream() -> Vec<u8> {
    // IDEN_UP + group voice grant, then one LDU of clear voice.
    let iden_args: u64 = {
        let iden = 1u64 << 60;
        let bw = 100u64 << 51;
        let sign = 1u64 << 50;
        let spacing = 100u64 << 32;
        let base = 851_012_500u64 / 5;
        iden | bw | sign | spacing | base
    };
    let channel = (1u64 << 12) | 10;
    let grant_args = (channel << 40) | (0x2F93u64 << 24) | 0xBEEF1;
    let mut stream = build_tsdu(0x293, &[(0x3D, 0, iden_args), (0x00, 0, grant_args)]);

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
    stream
}

fn modulate(dibits: &[u8]) -> Vec<f32> {
    use hs_dsp::modulator::C4fmModulator;
    use hs_dsp::C32;
    let mut m = C4fmModulator::new(RATE);
    let mut iq: Vec<C32> = Vec::new();
    for i in 0..400 {
        m.modulate(if i % 2 == 0 { 0b01 } else { 0b11 }, &mut iq);
    }
    for &d in dibits {
        m.modulate(d, &mut iq);
    }
    for _ in 0..200 {
        m.modulate(0b00, &mut iq);
    }
    let mut out = Vec::with_capacity(iq.len() * 2);
    for c in iq {
        out.push(c.re);
        out.push(c.im);
    }
    out
}

fn run_decoder(iq: &[f32], mode: EqMode) -> Metrics {
    let mut dec = ChannelDecoder::new(RATE, mode);
    let out = dec.process(iq);
    Metrics {
        syncs: out.syncs,
        grants: out.grants.len(),
        pcm_samples: out.pcm.len(),
        encrypted_skips: out.encrypted_skips.len(),
    }
}

fn synth_bench() {
    println!("HoosierSDR benchmark — synthetic P25 through two-ray + AWGN\n");
    println!(
        "{:>8}  {:>10}  {:>18}  {:>18}",
        "Es/N0", "echo", "BYPASS (sync/grant/pcm)", "EQUALIZED (sync/grant/pcm)"
    );

    let clean = modulate(&build_test_stream());
    // Sweep noise and a fixed simulcast echo (~2 symbols at 48 kHz = 20 samp).
    for &esno in &[30.0f32, 18.0, 12.0, 9.0, 6.0] {
        let echo_delay = 20;
        let echo_gain = 0.45;
        let iq = channel::impair(&clean, echo_delay, echo_gain, esno, 0x1234_5678);
        let b = run_decoder(&iq, EqMode::Bypass);
        let e = run_decoder(&iq, EqMode::Enabled);
        println!(
            "{:>6.0}dB  {:>4}smp×{:>0.2}  {:>7}/{:>2}/{:>6}      {:>7}/{:>2}/{:>6}",
            esno,
            echo_delay,
            echo_gain,
            b.syncs,
            b.grants,
            b.pcm_samples,
            e.syncs,
            e.grants,
            e.pcm_samples,
        );
    }
    println!(
        "\nMetric legend: syncs = frame-sync detections, grants = resolved voice\n\
         grants, pcm = decoded PCM samples (160/voice-frame). Higher is better.\n\
         \n\
         Reading this table: the two-ray echo here is applied at COMPLEX\n\
         baseband, i.e. BEFORE the FM discriminator. The current equalizer is\n\
         a real symbol-domain LMS placed AFTER the discriminator, so it cannot\n\
         invert this class of distortion — and the numbers show it: equalized\n\
         is non-harmful (clean decode preserved) but does not beat bypass on\n\
         pre-discriminator multipath. This is exactly the Phase 1 gate the\n\
         design doc defines, and it is currently NOT passed. The path to\n\
         passing it is the complex fractionally-spaced equalizer before\n\
         differential detection (hs-dsp::equalizer::LmsFse), which is the\n\
         project's core remaining DSP work. The field-IQ corpus runner is\n\
         `hs-bench file <path.cf32> [rate]`."
    );
}

fn file_bench(path: &str, rate: f64) {
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("cannot open {path}: {e}");
            std::process::exit(1);
        }
    };
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes).expect("read IQ file");
    let n = bytes.len() / 4;
    let mut iq = Vec::with_capacity(n);
    for i in 0..n {
        iq.push(f32::from_le_bytes(
            bytes[i * 4..i * 4 + 4].try_into().unwrap(),
        ));
    }
    println!("Running {} ({} IQ samples @ {} Hz)\n", path, n / 2, rate);
    let mut dec = ChannelDecoder::new(rate, EqMode::Enabled);
    let out = dec.process(&iq);
    println!("vocoder:          {}", dec.vocoder_name());
    println!("frame syncs:      {}", out.syncs);
    println!("voice grants:     {}", out.grants.len());
    println!("encrypted skips:  {}", out.encrypted_skips.len());
    println!(
        "decoded PCM:      {} samples ({:.1}s)",
        out.pcm.len(),
        out.pcm.len() as f64 / 8000.0
    );
    for g in out.grants.iter().take(20) {
        println!(
            "  grant TG {:<6} src {:<8} -> {:.4} MHz{}",
            g.talkgroup,
            g.source_unit,
            g.freq_hz as f64 / 1e6,
            if g.encrypted {
                "  [ENCRYPTED, skipped]"
            } else {
                ""
            }
        );
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        None | Some("synth") => synth_bench(),
        Some("file") => {
            let path = args
                .get(2)
                .expect("usage: hs-bench file <path.cf32> [rate]");
            let rate = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(48000.0);
            file_bench(path, rate);
        }
        Some(other) => {
            eprintln!("unknown mode '{other}'. usage: hs-bench [synth | file <path> [rate]]");
            std::process::exit(2);
        }
    }
}
