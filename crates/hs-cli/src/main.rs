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

use hs_core::decoder::{ChannelDecoder, DecodeOutput, EqMode, Modulation};

#[cfg(any(feature = "rtlsdr", feature = "airspy"))]
mod dual;
mod follow;

#[cfg(feature = "radioreference")]
mod rr;
use std::io::Read;

const DEFAULT_RATE: f64 = 48000.0;
const VOICE_RATE: u32 = 8000;

#[derive(Clone)]
struct Args {
    input: Option<String>,
    rate: f64,
    wav_out: Option<String>,
    log_out: Option<String>,
    save_iq: Option<String>,
    equalizer: bool,
    dfe: bool,
    cqpsk: bool,
    play: bool,
    demo: bool,
    sdr: bool,
    source: String,
    serial: Option<u64>,
    secs: Option<f64>,
    catalog: Option<String>,
    freq: f64,
    gain: Option<f64>,
    offset: f64,
    no_equalizer: bool,
    follow: bool,
    control: f64,
    control_measured: Option<f64>,
    dual: bool,
    voice_source: String,
    voice_serial: Option<u64>,
    voice_rate: f64,
    priorities: Vec<(u16, u8)>,
    rr_system: Option<u32>,
    rr_dump: Option<String>,
    scan: bool,
    scan_secs: f64,
    uv_quality: Option<i32>,
}

fn parse_args() -> Args {
    let mut a = Args {
        input: None,
        rate: DEFAULT_RATE,
        wav_out: Some("hoosier_out.wav".into()),
        log_out: None,
        save_iq: None,
        equalizer: false,
        dfe: false,
        cqpsk: false,
        play: false,
        demo: false,
        sdr: false,
        source: String::new(),
        serial: None,
        secs: None,
        catalog: None,
        freq: 851_000_000.0,
        gain: None,
        offset: 0.0,
        no_equalizer: false,
        follow: false,
        control: 0.0,
        control_measured: None,
        dual: false,
        voice_source: String::new(),
        voice_serial: None,
        voice_rate: 0.0,
        priorities: Vec::new(),
        rr_system: None,
        rr_dump: None,
        scan: false,
        scan_secs: 4.0,
        uv_quality: None,
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
            "--dfe" => a.dfe = true,
            "--no-equalizer" => a.no_equalizer = true,
            "--rr-system" => a.rr_system = it.next().and_then(|s| s.parse().ok()),
            "--rr-dump" => a.rr_dump = it.next(),
            "--uv-quality" => a.uv_quality = it.next().and_then(|s| s.parse().ok()),
            "--follow" => a.follow = true,
            "--control" => a.control = it.next().and_then(|s| parse_freq(&s)).unwrap_or(0.0),
            "--control-measured" => a.control_measured = it.next().and_then(|s| parse_freq(&s)),
            "--dual" => a.dual = true,
            "--voice-source" => a.voice_source = it.next().unwrap_or_default(),
            "--voice-serial" => {
                a.voice_serial = it
                    .next()
                    .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
            }
            "--voice-rate" => a.voice_rate = it.next().and_then(|s| s.parse().ok()).unwrap_or(0.0),
            "--priority" => {
                let s = it.next().unwrap_or_default();
                if let Some((tg, pr)) = s.split_once('=') {
                    if let (Ok(t), Ok(p)) = (tg.parse::<u16>(), pr.parse::<u8>()) {
                        a.priorities.push((t, p.clamp(1, 99)));
                    }
                }
            }
            "--scan" => a.scan = true,
            "--scan-secs" => a.scan_secs = it.next().and_then(|s| s.parse().ok()).unwrap_or(4.0),
            "--cqpsk" => a.cqpsk = true,
            "--offset" => a.offset = it.next().and_then(|s| parse_freq(&s)).unwrap_or(0.0),
            "--play" => a.play = true,
            "--demo" => a.demo = true,
            "--sdr" => a.sdr = true,
            "--source" => a.source = it.next().unwrap_or_default(),
            "--secs" => a.secs = it.next().and_then(|s| s.parse().ok()),
            "--serial" => {
                a.serial = it
                    .next()
                    .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
            }
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
             --no-equalizer Bypass the CMA equalizer on the CQPSK path, giving the\n\
             \x20              conventional detect-first receiver — the thesis A/B\n\
             --dfe          CQPSK decision-feedback equalizer before differential\n\
             \x20              detection: cancels the deep-null simulcast echo the\n\
             \x20              linear CMA leaves. Experimental; A/B against default.\n\
             --offset <HZ>  Decode the channel this far from the capture centre\n\
             \x20              (e.g. 50k). A wideband capture holds many 12.5 kHz\n\
             \x20              channels; this picks one without re-recording.\n\
             --cqpsk        Decode CQPSK/LSM (simulcast) instead of C4FM: carrier +\n\
                            timing recovery + CMA equalizer before differential detection\n\
             --sdr          Capture live from a radio (build --features rtlsdr,airspy)
--source <S>   Which radio: rtlsdr or airspy (default: whichever this
               build has; rtlsdr when it has both). An Airspy R2 runs at
               --rate 2500000 or 10000000; the stream is normalized to
               2.4/9.6 MSPS on the fly. Its firmware takes no gain setting.
--serial <HEX> Pick one of several Airspys by serial (see airspy_info)
--secs <S>     Stop a live capture after S seconds and print the summary
               (otherwise it runs until Ctrl-C)\n\
             --freq <HZ>    SDR center frequency (accepts 851M, 851.0125e6; default 851M)\n\
             --gain <DB>    SDR manual gain in dB (omit for hardware AGC)\n\
             --catalog <P>  RadioReference talkgroup CSV: show names instead of TG numbers\n\
             --rr-system <N> Download a trunked system\'s sites, control channels and\n\
             \x20              talkgroups from RadioReference and print where to tune.\n\
             \x20              Needs RR_APP_KEY, RR_USERNAME, RR_PASSWORD and a premium\n\
             \x20              account. Writes a talkgroup CSV (--catalog reads it).\n\
             \x20              Build with --features radioreference.\n\
             --rr-dump <D>  Save raw RadioReference XML responses to <D>/ for\n\
             \x20              diagnosing an unexpected response schema.\n\
             --uv-quality <N> Vocoder unvoiced synthesis detail, 1-64 (default 3).\n\
             \x20              Affects only how audio is rendered, never what is\n\
             \x20              decoded. Higher is smoother but not brighter; A/B by ear.\n\
             --follow       Trunk-follow: decode the control channel and every call\n\
             \x20              it grants, from one wideband capture. Needs --control\n\
             \x20              and --freq (the capture centre).\n\
             --control <HZ> Nominal control-channel frequency to follow.\n\
             --control-measured <HZ>  Where it actually is, if the tuner is far\n\
             \x20              enough off that auto-detection struggles.\n\
             --dual         Dual-SDR priority follow: one radio locks the control\n\
             \x20              channel, a second narrow radio hops voice channels by\n\
             \x20              talkgroup priority (1 = highest, 99 = lowest). Needs\n\
             \x20              --control. --source/--rate/--gain set the control radio;\n\
             \x20              --voice-source/--voice-serial/--voice-rate the voice\n\
             \x20              radio. --priority <TG=1..99> (repeatable) overrides the\n\
             \x20              catalog's Priority column.\n\
             --scan         Sweep the whole captured band and report which channels\n\
             \x20              actually carry P25 — by decoding, not by signal power.\n\
             \x20              Marks control vs voice channels and reports each NAC.\n\
             \x20              Pass --freq <centre> to get absolute frequencies.\n\
             --scan-secs <S> Seconds of capture to test per channel (default 4).\n\
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
    // `.cu8` is the RTL-SDR's native format: interleaved unsigned 8-bit, DC at
    // 127.5. Load it directly so a raw `rtl_sdr` capture can be decoded without
    // a conversion step.
    if path.ends_with(".cu8") || path.ends_with(".u8") {
        return bytes.iter().map(|&b| (b as f32 - 127.5) / 127.5).collect();
    }
    // `.cs16` is `airspy_rx -t 2` (INT16_IQ): interleaved signed 16-bit
    // little-endian, centred at 0. This is the Airspy R2's reliable output
    // format — its old firmware hangs when asked for float32 — and it is
    // native 12-bit data promoted to 16-bit, so it loses nothing to f32 at
    // half the file size.
    if path.ends_with(".cs16") || path.ends_with(".s16") {
        let n = bytes.len() / 2;
        return (0..n)
            .map(|i| {
                let s = i16::from_le_bytes([bytes[i * 2], bytes[i * 2 + 1]]);
                s as f32 / 32768.0
            })
            .collect();
    }
    // Everything else is interleaved little-endian f32 (`.cf32`), the
    // project's working format.
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
    println!("TSBKs decoded:    {}", dec.diagnostics().tsbks);
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

#[cfg(feature = "radioreference")]
fn run_rr(sys_id: u32, cache: Option<&str>, dump: Option<&str>) -> i32 {
    rr::run(sys_id, cache, dump)
}

#[cfg(not(feature = "radioreference"))]
fn run_rr(_sys_id: u32, _cache: Option<&str>, _dump: Option<&str>) -> i32 {
    eprintln!(
        "--rr-system needs the `radioreference` feature:\n\
         \n\
         \x20   cargo run -p hs-cli --features radioreference -- --rr-system <N>\n\
         \n\
         It also needs an application key registered at\n\
         https://www.radioreference.com/apps/account/?tab=api and a RadioReference\n\
         login with an active premium subscription, supplied as RR_APP_KEY,\n\
         RR_USERNAME and RR_PASSWORD.\n\
         \n\
         Without it, export your system's talkgroup CSV from the RadioReference\n\
         website and pass it with --catalog <file.csv>."
    );
    2
}

/// Sweep the captured band and report every channel that actually decodes.
fn run_scan(iq: &[f32], args: &Args) {
    let mut cfg = hs_core::scan::ScanConfig::new(args.rate).secs(args.scan_secs);
    // --freq doubles as the capture centre here, so results can be reported as
    // absolute frequencies rather than offsets.
    if args.freq > 0.0 {
        cfg = cfg.center(args.freq);
    }
    let channels = ((args.rate / 12_500.0) as u64).max(1);
    println!(
        "Scanning {:.0} kHz (~{channels} P25 channels), {:.0}s per channel…\n",
        args.rate / 1000.0,
        args.scan_secs
    );
    let found = hs_core::scan::scan(iq, &cfg);
    if found.is_empty() {
        println!("No P25 found in this capture.");
        println!(
            "\nIf you expected a signal here: the tuned frequency may be outside \n\
             the recording entirely, or the site may be Phase II TDMA (not yet \n\
             decoded). Widen the capture or check the frequency against \n\
             RadioReference with --rr-system."
        );
        return;
    }
    println!("Found {} P25 channel(s):\n", found.len());
    for f in &found {
        println!("  {}", f.summary());
    }
    if let Some(cc) = found.iter().find(|f| f.control_channel) {
        println!("\nControl channel — decode it with:");
        println!(
            "  hoosier-sdr --sdr --freq {} {}",
            cc.freq_hz.unwrap_or(0.0) as u64,
            if cc.modulation == Modulation::Cqpsk {
                "--cqpsk"
            } else {
                ""
            }
        );
    } else {
        println!(
            "\nNo control channel in this capture — these are traffic channels \n\
             (voice only, no trunking signalling). The control channel is elsewhere in the band."
        );
    }
}

fn build_decoder(args: &Args) -> ChannelDecoder {
    let modulation = if args.cqpsk {
        Modulation::Cqpsk
    } else {
        Modulation::C4fm
    };
    // C4FM: the symbol-domain equalizer is experimental and opt-in.
    // CQPSK: the CMA equalizer before differential detection IS the shipping
    // path (the project thesis), so it is on unless explicitly disabled.
    let mode = if args.dfe {
        EqMode::Dfe
    } else if args.no_equalizer {
        EqMode::Bypass
    } else if args.cqpsk || args.equalizer {
        EqMode::Enabled
    } else {
        EqMode::Bypass
    };
    ChannelDecoder::with_offset(args.rate, modulation, mode, args.offset)
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

/// Warn about a sample rate that decodes a continuous carrier but mangles
/// voice.
///
/// The RTL-SDR's 225–300 kHz range is its lowest-quality mode, with aggressive
/// internal decimation. Measured against a live P25 system, a control channel
/// captured there decoded cleanly (its carrier is continuous) while a voice
/// call on the same capture produced 0.2 s of audio out of 7 s — the framer
/// starved of usable symbols — that at 1.2 MHz decoded in full. The rate is not
/// rejected, because control-only work there is fine, but voice needs headroom.
fn warn_low_rate(rate: f64) {
    if (225_001.0..=300_000.0).contains(&rate) {
        eprintln!(
            "note: {:.0} kHz is the RTL-SDR's lowest-quality mode. A control channel\n             decodes there, but voice frames are marginal — capture voice at >=900 kHz.",
            rate / 1000.0
        );
    }
}

fn main() {
    let mut args = parse_args();
    warn_low_rate(args.rate);

    if let Some(sys_id) = args.rr_system {
        std::process::exit(run_rr(
            sys_id,
            args.catalog.as_deref(),
            args.rr_dump.as_deref(),
        ));
    }

    if args.sdr {
        run_sdr(&args);
        return;
    }

    #[cfg(any(feature = "rtlsdr", feature = "airspy"))]
    if args.dual {
        if args.control <= 0.0 {
            eprintln!("--dual needs --control <HZ> (the control-channel frequency)");
            std::process::exit(2);
        }
        dual::run(dual::DualArgs {
            control_source: args.source.clone(),
            control_serial: args.serial,
            control_hz: args.control,
            control_rate: args.rate,
            voice_source: args.voice_source.clone(),
            voice_serial: args.voice_serial,
            voice_rate: if args.voice_rate > 0.0 {
                args.voice_rate
            } else {
                args.rate
            },
            gain: args.gain,
            cqpsk: args.cqpsk,
            priorities: args.priorities.clone(),
            catalog: args.catalog.as_deref().and_then(load_catalog),
            secs: args.secs,
            wav_out: args.wav_out.clone(),
        });
        return;
    }
    #[cfg(not(any(feature = "rtlsdr", feature = "airspy")))]
    if args.dual {
        eprintln!("--dual needs a live radio (build --features rtlsdr,airspy)");
        std::process::exit(2);
    }

    if args.input.is_none() && !args.demo {
        eprintln!("no input file (use --demo to decode a synthesized transmission)\n");
        print_help();
        std::process::exit(2);
    }

    let mut iq = if args.demo {
        println!("Decoding a synthesized P25 transmission (demo mode)…\n");
        demo_iq(args.rate, args.cqpsk)
    } else {
        load_iq(args.input.as_ref().unwrap())
    };

    // Normalize a hardware rate that isn't a multiple of the symbol rate (an
    // Airspy R2's 10 or 2.5 MSPS) to the nearest clean rate, once, before any
    // channel processing. Everything downstream then treats it like a native
    // capture — scan, offset tuning and the channelizer all assume rate % 4800.
    if !args.demo {
        if let Some((up, down, out_rate)) = hs_dsp::resample::normalize_ratio(args.rate) {
            println!(
                "normalizing {:.3} MSPS → {:.3} MSPS (×{up}/{down}) so the decoder's \
                 front end can lock…\n",
                args.rate / 1e6,
                out_rate / 1e6
            );
            iq = hs_dsp::resample::resample_iq(&iq, up, down, args.rate);
            args.rate = out_rate;
        }
    }

    if args.scan {
        run_scan(&iq, &args);
        return;
    }

    if args.follow {
        if args.control <= 0.0 {
            eprintln!("--follow needs --control <HZ> (the control-channel frequency)");
            std::process::exit(2);
        }
        let catalog = args.catalog.as_deref().and_then(load_catalog);
        follow::run_file(
            &iq,
            args.rate,
            args.freq,
            args.control,
            args.control_measured,
            catalog.as_ref(),
        );
        return;
    }

    let catalog = args.catalog.as_deref().and_then(load_catalog);
    let mut dec = build_decoder(&args);
    if let Some(q) = args.uv_quality {
        dec.set_uv_quality(q);
    }
    let out = dec.process(&iq);
    report(&out, &dec, catalog.as_ref());

    let lc = &dec.diagnostics().link_control;
    if !lc.is_empty() {
        println!("\ncalls identified from the voice channel itself (Link Control):");
        for l in lc {
            let em = if l.emergency { "  [EMERGENCY]" } else { "" };
            println!("  TG {:<7} unit {:<9}{em}", l.talkgroup, l.source_unit);
        }
    }

    let vendors = &dec.diagnostics().vendor_tsbks;
    if !vendors.is_empty() {
        println!("\nmanufacturer-specific messages seen (not acted on):");
        let mut v: Vec<_> = vendors.iter().collect();
        v.sort_by_key(|(_, _, n)| std::cmp::Reverse(*n));
        for (mfid, opcode, n) in v {
            let name = if *mfid == hs_p25::moto::MFID_MOTOROLA {
                hs_p25::moto::describe(*opcode).unwrap_or("unidentified")
            } else {
                "unidentified"
            };
            println!("  MFID 0x{mfid:02X} opcode 0x{opcode:02X}  x{n:<5} {name}");
        }
    }

    let patches = dec.patches();
    if !patches.is_empty() {
        println!("\ntalkgroup patches (Motorola Group Regroup):");
        for (sg, members) in patches.patches() {
            let list: Vec<String> = members.iter().map(|m| m.to_string()).collect();
            println!("  patch {sg:<6} <- TG {}", list.join(", "));
        }
        println!("  (audio for any member can appear under the others)");
    }

    if !out.locations.is_empty() {
        println!("\nradio positions (LRRP):");
        for l in &out.locations {
            println!(
                "  unit {:<8} {:.5}, {:.5}   https://maps.google.com/?q={:.5},{:.5}",
                l.llid, l.lat, l.lon, l.lat, l.lon
            );
        }
    }

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
#[cfg(any(feature = "rtlsdr", feature = "airspy"))]
fn run_sdr(args: &Args) {
    use hs_core::stream::Normalized;

    // An unnamed source means "the radio this build knows"; with both
    // compiled in, the RTL-SDR keeps its historical default.
    let source = if args.source.is_empty() {
        if cfg!(feature = "rtlsdr") {
            "rtlsdr"
        } else {
            "airspy"
        }
    } else {
        args.source.as_str()
    };
    match source {
        #[cfg(feature = "rtlsdr")]
        "rtlsdr" => {
            use hs_source::rtlsdr::RtlSdrSource;
            let src = match RtlSdrSource::open("driver=rtlsdr", args.freq, args.rate, args.gain) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("could not open RTL-SDR: {e:?}");
                    std::process::exit(1);
                }
            };
            run_sdr_with(Normalized::new(src), args);
        }
        #[cfg(feature = "airspy")]
        "airspy" => {
            use hs_source::airspy::AirspySource;
            let src = match AirspySource::open(args.serial, args.freq, args.rate, args.gain) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("could not open Airspy: {e:?}");
                    std::process::exit(1);
                }
            };
            if let Some(g) = src.gain_ignored() {
                eprintln!(
                    "note: --gain {g} ignored — the Airspy R2 firmware hangs on gain \
                     commands, so it runs at its default gain"
                );
            }
            let src = Normalized::new(src);
            if src.is_resampling() {
                use hs_source::SdrSource;
                println!(
                    "normalizing {:.3} MSPS → {:.3} MSPS (×24/25) on the fly",
                    args.rate / 1e6,
                    src.sample_rate() / 1e6
                );
            }
            run_sdr_with(src, args);
        }
        other => {
            eprintln!(
                "unknown --source {other:?} (or not compiled in); this build supports:{}{}",
                if cfg!(feature = "rtlsdr") {
                    " rtlsdr"
                } else {
                    ""
                },
                if cfg!(feature = "airspy") {
                    " airspy"
                } else {
                    ""
                },
            );
            std::process::exit(2);
        }
    }
}

/// Live capture from an already-open, rate-normalized source: follow a trunk
/// or decode one channel until interrupted, printing grants as they resolve
/// and accumulating decoded voice to a WAV.
#[cfg(any(feature = "rtlsdr", feature = "airspy"))]
fn run_sdr_with<S: hs_source::SdrSource + Send + 'static>(src: S, args: &Args) {
    use hs_core::stream;
    use hs_source::SdrSource;

    let src = Timed::new(src, args.secs);
    // The decoder must be built for the rate the source *delivers*, which
    // for an Airspy is the normalized one, not the hardware one.
    let rate = src.sample_rate();
    if args.follow {
        if args.control <= 0.0 {
            eprintln!("--follow needs --control <HZ> (the control-channel frequency)");
            std::process::exit(2);
        }
        let catalog = args.catalog.as_deref().and_then(load_catalog);
        follow::run_live(
            src,
            rate,
            args.freq,
            args.control,
            args.control_measured,
            catalog.as_ref(),
            args.save_iq.as_deref(),
        );
        return;
    }

    println!(
        "Capturing {} at {:.4} MHz, {} Hz{}… Ctrl-C to stop.",
        if args.cqpsk { "CQPSK/LSM" } else { "C4FM" },
        args.freq / 1e6,
        rate as u64,
        match args.gain {
            Some(g) => format!(", gain {g} dB"),
            None => " (AGC)".into(),
        },
    );

    let mut dec = build_decoder(&Args {
        rate,
        ..args.clone()
    });
    // Drain the radio on its own thread (see `stream::Buffered`): read
    // synchronously from the decode loop, an RTL-SDR loses samples at every
    // block boundary and the control channel syncs but never yields a TSBK.
    let src = stream::Buffered::new(src, 65536);
    // --save-iq records exactly what the decoder sees — post-normalization,
    // as .cf32 at `rate` — so `hoosier-sdr --rate <rate> <file>` replays it.
    let mut src = Recorded::new(src, args.save_iq.as_deref());
    if let Some(p) = src.path() {
        println!("recording IQ to {p} ({:.1} MB/s)", rate * 8.0 / 1e6);
    }
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
            "\nstopped: {} blocks, {} syncs, {} grants, {:.1}s voice, {} dropped",
            s.blocks,
            s.syncs,
            s.grants,
            s.pcm_samples as f64 / VOICE_RATE as f64,
            src.dropped()
        ),
        Err(e) => eprintln!("capture error: {e:?}"),
    }
    if let (Some(path), false) = (&args.wav_out, pcm.is_empty()) {
        let _ = wav::write_wav(path, VOICE_RATE, &pcm);
        println!("wrote {path}");
    }
}

/// A source that reports end-of-stream after a deadline, so `--secs` ends a
/// live run the same way a file does — cleanly, with the summary printed.
#[cfg(any(feature = "rtlsdr", feature = "airspy"))]
struct Timed<S> {
    inner: S,
    deadline: Option<std::time::Instant>,
}

#[cfg(any(feature = "rtlsdr", feature = "airspy"))]
impl<S> Timed<S> {
    fn new(inner: S, secs: Option<f64>) -> Self {
        Self {
            inner,
            deadline: secs
                .map(|s| std::time::Instant::now() + std::time::Duration::from_secs_f64(s)),
        }
    }
}

#[cfg(any(feature = "rtlsdr", feature = "airspy"))]
impl<S: hs_source::SdrSource> hs_source::SdrSource for Timed<S> {
    fn sample_rate(&self) -> f64 {
        self.inner.sample_rate()
    }
    fn center_freq(&self) -> f64 {
        self.inner.center_freq()
    }
    fn dropped(&self) -> u64 {
        self.inner.dropped()
    }
    fn read(&mut self, buf: &mut [f32]) -> Result<usize, hs_source::SourceError> {
        if self
            .deadline
            .is_some_and(|d| std::time::Instant::now() >= d)
        {
            return Err(hs_source::SourceError::Eof);
        }
        self.inner.read(buf)
    }
}

/// A source that tees everything it delivers into a `.cf32` file.
#[cfg(any(feature = "rtlsdr", feature = "airspy"))]
struct Recorded<S> {
    inner: S,
    out: Option<(String, std::io::BufWriter<std::fs::File>)>,
}

#[cfg(any(feature = "rtlsdr", feature = "airspy"))]
impl<S> Recorded<S> {
    fn new(inner: S, path: Option<&str>) -> Self {
        let out = path.and_then(|p| {
            let p = if p.ends_with(".cf32") {
                p.to_string()
            } else {
                format!("{p}.cf32")
            };
            match std::fs::File::create(&p) {
                Ok(f) => Some((p, std::io::BufWriter::new(f))),
                Err(e) => {
                    eprintln!("cannot record IQ to {p}: {e}");
                    None
                }
            }
        });
        Self { inner, out }
    }

    fn path(&self) -> Option<&str> {
        self.out.as_ref().map(|(p, _)| p.as_str())
    }
}

#[cfg(any(feature = "rtlsdr", feature = "airspy"))]
impl<S: hs_source::SdrSource> hs_source::SdrSource for Recorded<S> {
    fn sample_rate(&self) -> f64 {
        self.inner.sample_rate()
    }
    fn center_freq(&self) -> f64 {
        self.inner.center_freq()
    }
    fn dropped(&self) -> u64 {
        self.inner.dropped()
    }
    fn read(&mut self, buf: &mut [f32]) -> Result<usize, hs_source::SourceError> {
        let n = self.inner.read(buf)?;
        if let Some((_, w)) = self.out.as_mut() {
            use std::io::Write;
            let bytes: Vec<u8> = buf[..n].iter().flat_map(|v| v.to_le_bytes()).collect();
            if let Err(e) = w.write_all(&bytes) {
                eprintln!("IQ recording stopped: {e}");
                self.out = None;
            }
        }
        Ok(n)
    }
}

#[cfg(not(any(feature = "rtlsdr", feature = "airspy")))]
fn run_sdr(_args: &Args) {
    eprintln!(
        "Live SDR capture needs a build with the rtlsdr and/or airspy feature:\n\
         \n    RUSTFLAGS=\"-C target-cpu=native\" \\\n\
         \n      cargo run -p hs-cli --release --features rtlsdr,airspy -- --sdr --freq 851.0125M\n\
         \n(rtlsdr pulls Seify + libusb; airspy links libairspy. On macOS: `brew install libusb airspy`.)\n\
         \nThe target-cpu=native flag matters for --follow at 2.4 MHz: without it\n\
         the pipeline can run just under real time and the radio drops samples.\n\
         Pass a fixed --gain (e.g. 40) rather than relying on the tuner's AGC."
    );
    std::process::exit(2);
}
