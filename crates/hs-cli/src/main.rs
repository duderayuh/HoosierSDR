//! hoosier-sdr — the HoosierSDR command-line application.
//!
//! Phase 2 CLI: decode a P25 Phase I transmission from a raw IQ file and
//! surface trunking activity, encryption status, and decoded voice. Audio is
//! written to a WAV file by default (works headless); with `--features audio`
//! it can also play live through the default output device.
//!
//! Live SDR capture (Seify RTL-SDR / Airspy) is the next integration; the
//! decode core it feeds is already complete and exercised here.

mod wav;

use hs_core::decoder::{ChannelDecoder, DecodeOutput, EqMode};
use std::io::Read;

const DEFAULT_RATE: f64 = 48000.0;
const VOICE_RATE: u32 = 8000;

struct Args {
    input: Option<String>,
    rate: f64,
    wav_out: Option<String>,
    log_out: Option<String>,
    save_iq: Option<String>,
    equalizer: bool,
    cqpsk: bool,
    play: bool,
    demo: bool,
    sdr: bool,
    catalog: Option<String>,
    freq: f64,
    gain: Option<f64>,
}

fn parse_args() -> Args {
    let mut a = Args {
        input: None,
        rate: DEFAULT_RATE,
        wav_out: Some("hoosier_out.wav".into()),
        log_out: None,
        save_iq: None,
        equalizer: false,
        cqpsk: false,
        play: false,
        demo: false,
        sdr: false,
        catalog: None,
        freq: 851_000_000.0,
        gain: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--rate" => {
                a.rate = it
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(DEFAULT_RATE)
            }
            "--wav" => a.wav_out = it.next(),
            "--no-wav" => a.wav_out = None,
            "--log" => a.log_out = it.next(),
            "--save-iq" => a.save_iq = it.next(),
            "--equalizer" => a.equalizer = true,
            "--cqpsk" => a.cqpsk = true,
            "--play" => a.play = true,
            "--demo" => a.demo = true,
            "--sdr" => a.sdr = true,
            "--catalog" => a.catalog = it.next(),
            "--freq" => a.freq = it.next().and_then(|s| parse_freq(&s)).unwrap_or(a.freq),
            "--gain" => a.gain = it.next().and_then(|s| s.parse().ok()),
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other if !other.starts_with('-') => a.input = Some(other.to_string()),
            other => {
                eprintln!("unknown option: {other}");
                print_help();
                std::process::exit(2);
            }
        }
    }
    a
}

fn print_help() {
    println!(
        "hoosier-sdr — P25 Phase I decoder\n\
         \n\
         USAGE:\n\
             hoosier-sdr [OPTIONS] <input.cf32>\n\
             hoosier-sdr --demo\n\
         \n\
         ARGS:\n\
             <input.cf32>   Raw interleaved-f32 IQ recording to decode\n\
         \n\
         OPTIONS:\n\
             --rate <HZ>    IQ sample rate (default 48000; must be a multiple of 4800)\n\
             --wav <PATH>   Write decoded voice to this WAV file (default hoosier_out.wav)\n\
             --no-wav       Do not write a WAV file\n\
             --log <PATH>   Write a JSON diagnostics log for offline refinement\n\
             --save-iq <P>  Save the decoded IQ to <P>.cf32 (share it to reproduce a decode)\n\
             --equalizer    Enable the experimental FSW-trained equalizer (C4FM)\n\
             --cqpsk        Decode CQPSK/LSM (simulcast) instead of C4FM: carrier +\n\
                            timing recovery + CMA equalizer before differential detection\n\
             --sdr          Capture live from an RTL-SDR (build --features rtlsdr)\n\
             --freq <HZ>    SDR center frequency (accepts 851M, 851.0125e6; default 851M)\n\
             --gain <DB>    SDR manual gain in dB (omit for hardware AGC)\n\
             --catalog <P>  RadioReference talkgroup CSV: show names instead of TG numbers\n\
             --play         Play decoded audio live (requires build --features audio)\n\
             --demo         Decode a synthesized transmission (no input file needed)\n\
             -h, --help     Show this help\n\
         \n\
         REFINING FROM A REAL TEST: run against your capture with --log run.json\n\
         (and optionally --save-iq run.cf32), then share those two files. The\n\
         log captures sync quality, NID/BCH stats, symbol-eye health, grants,\n\
         and encryption events; the .cf32 lets the decode be reproduced exactly.\n\
         \n\
         LEGAL: Decodes unencrypted P25 only. Encrypted talkgroups are detected\n\
         and skipped by design — HoosierSDR never decrypts. Indiana users: run\n\
         this at your dwelling or place of business only (IC 35-44.1-2-7)."
    );
}

fn load_iq(path: &str) -> Vec<f32> {
    let mut f = std::fs::File::open(path).unwrap_or_else(|e| {
        eprintln!("cannot open {path}: {e}");
        std::process::exit(1);
    });
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes).expect("read IQ");
    let n = bytes.len() / 4;
    (0..n)
        .map(|i| f32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap()))
        .collect()
}

/// Synthesize a demo transmission (control channel + clear voice) as IQ so
/// the app runs end-to-end with no capture hardware or recording. Emits C4FM
/// or, when `cqpsk` is set, π/4-DQPSK so `--cqpsk --demo` exercises that path.
fn demo_iq(rate: f64, cqpsk: bool) -> Vec<f32> {
    use hs_dsp::C32;
    use hs_p25::synth::{build_ldu1, build_tsdu};
    use hs_p25::voice::ImbeFrame;

    let iden = (1u64 << 60) | (100u64 << 51) | (1u64 << 50) | (100u64 << 32) | (851_012_500u64 / 5);
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

    let to_interleaved = |iq: Vec<C32>| {
        let mut out = Vec::with_capacity(iq.len() * 2);
        for c in iq {
            out.push(c.re);
            out.push(c.im);
        }
        out
    };

    if cqpsk {
        // Preamble + frame + trailing flush; modulate as π/4-DQPSK.
        let sps = (rate / hs_dsp::P25_SYMBOL_RATE).round() as usize;
        let mut dibits: Vec<u8> = (0..300).map(|i| ((i * 5 + i / 3) % 4) as u8).collect();
        dibits.extend(stream);
        dibits.extend((0..120).map(|i| ((i * 5) % 4) as u8));
        return to_interleaved(hs_dsp::cqpsk::modulate_iq(&dibits, sps, 0.2));
    }

    use hs_dsp::modulator::C4fmModulator;
    let mut m = C4fmModulator::new(rate);
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
    to_interleaved(iq)
}

/// Talkgroup label from the catalog, else the numeric ID.
fn tg_label(cat: Option<&hs_core::catalog::CsvCatalog>, id: u16) -> String {
    match cat {
        Some(c) => c.label(id),
        None => format!("TG {id}"),
    }
}

fn report(out: &DecodeOutput, dec: &ChannelDecoder, cat: Option<&hs_core::catalog::CsvCatalog>) {
    println!("── HoosierSDR decode summary ──");
    println!("modulation:       {:?}", dec.modulation());
    println!("vocoder:          {}", dec.vocoder_name());
    println!("frame syncs:      {}", out.syncs);
    println!("voice grants:     {}", out.grants.len());
    for g in &out.grants {
        println!(
            "   {:<20} src {:<8} → {:.4} MHz{}",
            tg_label(cat, g.talkgroup),
            g.source_unit,
            g.freq_hz as f64 / 1e6,
            if g.encrypted {
                "   [ENCRYPTED — skipped]"
            } else {
                ""
            }
        );
    }
    if !out.encrypted_skips.is_empty() {
        println!(
            "encrypted skips:  {} talkgroup(s): {:?}",
            out.encrypted_skips.len(),
            out.encrypted_skips
        );
    }
    println!(
        "decoded voice:    {} samples ({:.2}s @ 8 kHz)",
        out.pcm.len(),
        out.pcm.len() as f64 / VOICE_RATE as f64
    );
}

#[cfg(feature = "audio")]
fn play_pcm(samples: &[i16]) {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    let host = cpal::default_host();
    let Some(device) = host.default_output_device() else {
        eprintln!("no audio output device; skipping playback");
        return;
    };
    let config = cpal::StreamConfig {
        channels: 1,
        sample_rate: cpal::SampleRate(VOICE_RATE),
        buffer_size: cpal::BufferSize::Default,
    };
    let data: Vec<f32> = samples.iter().map(|&s| s as f32 / 32768.0).collect();
    let mut pos = 0usize;
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done2 = done.clone();
    let stream = device
        .build_output_stream(
            &config,
            move |buf: &mut [f32], _| {
                for out in buf.iter_mut() {
                    *out = data.get(pos).copied().unwrap_or(0.0);
                    pos += 1;
                }
                if pos >= data.len() {
                    done2.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            },
            |e| eprintln!("audio stream error: {e}"),
            None,
        )
        .expect("build output stream");
    stream.play().expect("play");
    while !done.load(std::sync::atomic::Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[cfg(not(feature = "audio"))]
fn play_pcm(_samples: &[i16]) {
    eprintln!("live playback needs a build with --features audio; wrote WAV instead");
}

/// Parse a frequency like `851M`, `851.0125e6`, or `851000000`.
fn parse_freq(s: &str) -> Option<f64> {
    let s = s.trim();
    if let Some(v) = s.strip_suffix(['M', 'm']) {
        v.parse::<f64>().ok().map(|x| x * 1e6)
    } else if let Some(v) = s.strip_suffix(['k', 'K']) {
        v.parse::<f64>().ok().map(|x| x * 1e3)
    } else {
        s.parse::<f64>().ok()
    }
}

fn build_decoder(args: &Args) -> ChannelDecoder {
    if args.cqpsk {
        ChannelDecoder::new_cqpsk(args.rate)
    } else {
        let mode = if args.equalizer {
            EqMode::Enabled
        } else {
            EqMode::Bypass
        };
        ChannelDecoder::new(args.rate, mode)
    }
}

/// Load a RadioReference talkgroup CSV, or warn and continue without it.
fn load_catalog(path: &str) -> Option<hs_core::catalog::CsvCatalog> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let cat = hs_core::catalog::CsvCatalog::parse(&text);
            println!("loaded catalog: {} talkgroups from {path}", cat.len());
            Some(cat)
        }
        Err(e) => {
            eprintln!("could not read catalog {path}: {e}");
            None
        }
    }
}

fn main() {
    let args = parse_args();

    if args.sdr {
        run_sdr(&args);
        return;
    }

    if args.input.is_none() && !args.demo {
        eprintln!("no input file (use --demo to decode a synthesized transmission)\n");
        print_help();
        std::process::exit(2);
    }

    let iq = if args.demo {
        println!("Decoding a synthesized P25 transmission (demo mode)…\n");
        demo_iq(args.rate, args.cqpsk)
    } else {
        load_iq(args.input.as_ref().unwrap())
    };

    let catalog = args.catalog.as_deref().and_then(load_catalog);
    let mut dec = build_decoder(&args);
    let out = dec.process(&iq);
    report(&out, &dec, catalog.as_ref());

    if let Some(path) = &args.wav_out {
        if out.pcm.is_empty() {
            println!("\n(no voice decoded; not writing WAV)");
        } else {
            match wav::write_wav(path, VOICE_RATE, &out.pcm) {
                Ok(()) => println!("\nwrote {path}"),
                Err(e) => eprintln!("\nfailed to write {path}: {e}"),
            }
        }
    }

    if let Some(path) = &args.log_out {
        let json = dec.diagnostics().to_json();
        match std::fs::write(path, json) {
            Ok(()) => println!("wrote diagnostics {path}"),
            Err(e) => eprintln!("failed to write {path}: {e}"),
        }
    }

    if let Some(path) = &args.save_iq {
        match save_iq(path, &iq) {
            Ok(()) => println!("wrote IQ {path} ({} samples)", iq.len() / 2),
            Err(e) => eprintln!("failed to write {path}: {e}"),
        }
    }

    if args.play && !out.pcm.is_empty() {
        play_pcm(&out.pcm);
    }
}

/// Persist interleaved-f32 IQ so a decode can be reproduced offline.
fn save_iq(path: &str, iq: &[f32]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut buf = Vec::with_capacity(iq.len() * 4);
    for &s in iq {
        buf.extend_from_slice(&s.to_le_bytes());
    }
    f.write_all(&buf)
}

/// Live capture: stream from an RTL-SDR into the decoder until interrupted,
/// printing grants as they resolve and accumulating decoded voice to a WAV.
#[cfg(feature = "rtlsdr")]
fn run_sdr(args: &Args) {
    use hs_core::stream;
    use hs_source::rtlsdr::RtlSdrSource;

    let mut src = match RtlSdrSource::open("driver=rtlsdr", args.freq, args.rate, args.gain) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("could not open RTL-SDR: {e:?}");
            std::process::exit(1);
        }
    };
    println!(
        "Capturing {} at {:.4} MHz, {} Hz{}… Ctrl-C to stop.",
        if args.cqpsk { "CQPSK/LSM" } else { "C4FM" },
        args.freq / 1e6,
        args.rate as u64,
        match args.gain {
            Some(g) => format!(", gain {g} dB"),
            None => " (AGC)".into(),
        },
    );

    let mut dec = build_decoder(args);
    let mut pcm: Vec<i16> = Vec::new();
    let stats = stream::run(&mut src, &mut dec, 65536, |out| {
        for g in &out.grants {
            println!(
                "  grant TG {:<6} src {:<8} → {:.4} MHz{}",
                g.talkgroup,
                g.source_unit,
                g.freq_hz as f64 / 1e6,
                if g.encrypted {
                    "  [ENCRYPTED — skipped]"
                } else {
                    ""
                }
            );
        }
        pcm.extend_from_slice(&out.pcm);
    });
    match stats {
        Ok(s) => println!(
            "\nstopped: {} blocks, {} syncs, {} grants, {:.1}s voice",
            s.blocks,
            s.syncs,
            s.grants,
            s.pcm_samples as f64 / VOICE_RATE as f64
        ),
        Err(e) => eprintln!("capture error: {e:?}"),
    }
    if let (Some(path), false) = (&args.wav_out, pcm.is_empty()) {
        let _ = wav::write_wav(path, VOICE_RATE, &pcm);
        println!("wrote {path}");
    }
}

#[cfg(not(feature = "rtlsdr"))]
fn run_sdr(_args: &Args) {
    eprintln!(
        "Live SDR capture needs a build with the rtlsdr feature:\n\
         \n    cargo run -p hs-cli --features rtlsdr -- --sdr --freq 851.0125M\n\
         \n(That pulls Seify + libusb. On macOS: `brew install libusb`.)"
    );
    std::process::exit(2);
}
