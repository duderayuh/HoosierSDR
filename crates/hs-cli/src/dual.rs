//! Dual-SDR priority trunk-following (live): SDR A locks the control channel
//! and decodes grants; SDR B, a narrow radio, hops between voice channels,
//! always covering the highest-priority open call.

use hs_core::decoder::{ChannelDecoder, EqMode};
use hs_core::dual::{DualSdrFollower, Retune};
use hs_core::priority::PriorityMap;
use hs_core::stream::{Buffered, Normalized};
use hs_source::{FreqHandle, SdrSource};

pub struct DualArgs {
    pub control_source: String,
    pub control_serial: Option<u64>,
    /// RTL-SDR device index for the control radio (0 = first dongle).
    pub control_rtl: usize,
    /// The control channel frequency (SDR A's centre).
    pub control_hz: f64,
    pub control_rate: f64,
    pub voice_source: String,
    pub voice_serial: Option<u64>,
    /// RTL-SDR device index for the voice radio (1 = second dongle).
    pub voice_rtl: usize,
    pub voice_rate: f64,
    pub gain: Option<f64>,
    pub cqpsk: bool,
    pub priorities: Vec<(u16, u8)>,
    pub catalog: Option<hs_core::catalog::CsvCatalog>,
    pub secs: Option<f64>,
    pub wav_out: Option<String>,
}

/// The default source for this build: RTL-SDR when compiled in, else Airspy.
fn default_source() -> String {
    if cfg!(feature = "rtlsdr") {
        "rtlsdr".into()
    } else {
        "airspy".into()
    }
}

fn open(
    kind: &str,
    serial: Option<u64>,
    rtl_index: usize,
    freq: f64,
    rate: f64,
    gain: Option<f64>,
) -> (Box<dyn SdrSource + Send>, FreqHandle) {
    match kind {
        #[cfg(feature = "rtlsdr")]
        "rtlsdr" => {
            let args = format!("driver=rtlsdr,rtl={rtl_index}");
            let src = hs_source::rtlsdr::RtlSdrSource::open(&args, freq, rate, gain)
                .unwrap_or_else(|e| {
                    eprintln!("could not open RTL-SDR #{rtl_index}: {e:?}");
                    std::process::exit(1);
                });
            let fh = src.freq_handle();
            (Box::new(Normalized::new(src)), fh)
        }
        #[cfg(feature = "airspy")]
        "airspy" => {
            let src = hs_source::airspy::AirspySource::open(serial, freq, rate, gain)
                .unwrap_or_else(|e| {
                    eprintln!("could not open Airspy: {e:?}");
                    std::process::exit(1);
                });
            let fh = src.freq_handle();
            (Box::new(Normalized::new(src)), fh)
        }
        other => {
            eprintln!("unknown source {other:?} (or not compiled in)");
            std::process::exit(2);
        }
    }
}

pub fn run(args: DualArgs) {
    let control_source = if args.control_source.is_empty() {
        default_source()
    } else {
        args.control_source.clone()
    };
    let voice_source = if args.voice_source.is_empty() {
        control_source.clone()
    } else {
        args.voice_source.clone()
    };

    // Priority: catalog base (RR Priority column) + CLI `--priority` overrides.
    let mut prio = PriorityMap::new();
    if let Some(cat) = &args.catalog {
        use hs_core::catalog::Catalog;
        if let Ok(tgs) = cat.talkgroups(0) {
            for tg in tgs {
                if let Some(p) = tg.priority {
                    prio.set_base(tg.id, p);
                }
            }
        }
    }
    for (tg, p) in &args.priorities {
        prio.set_override(*tg, *p);
    }

    // Open both radios. SDR B starts parked on the control channel so it can
    // still hear control if SDR A dies.
    let (control_src, _) = open(
        &control_source,
        args.control_serial,
        args.control_rtl,
        args.control_hz,
        args.control_rate,
        args.gain,
    );
    let (voice_src, voice_fh) = open(
        &voice_source,
        args.voice_serial,
        args.voice_rtl,
        args.control_hz,
        args.voice_rate,
        args.gain,
    );

    // The control decoder runs at baseband — the radio is centred on the
    // control channel.
    let control = if args.cqpsk {
        ChannelDecoder::new_cqpsk(args.control_rate)
    } else {
        ChannelDecoder::new(args.control_rate, EqMode::Bypass)
    };

    let mut follower = DualSdrFollower::new(control, args.control_rate, prio, args.voice_rate);
    let mut control = Buffered::new(control_src, 65536);
    let mut voice = Buffered::new(voice_src, 65536);

    println!(
        "dual-SDR follow: control {} @ {:.4} MHz, voice {} hopping (Ctrl-C to stop)",
        control_source,
        args.control_hz / 1e6,
        voice_source
    );

    let mut cbuf = vec![0.0f32; 65536 * 2];
    let mut vbuf = vec![0.0f32; 65536 * 2];
    let mut pcm: Vec<i16> = Vec::new();
    let start = std::time::Instant::now();

    loop {
        if let Some(s) = args.secs {
            if start.elapsed().as_secs_f64() >= s {
                break;
            }
        }

        match control.read(&mut cbuf) {
            Ok(n) if n > 0 => {
                let ev = follower.process_control(&cbuf[..n]);
                for g in &ev.grants {
                    println!(
                        "  grant TG {:<6} src {:<8} → {:.4} MHz{}",
                        g.talkgroup,
                        g.source_unit,
                        g.freq_hz as f64 / 1e6,
                        if g.encrypted {
                            "  [ENC — skipped]"
                        } else {
                            ""
                        }
                    );
                }
                if let Some(r) = ev.retune {
                    apply_retune(&r, &voice_fh, &mut follower, args.control_hz);
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }

        match voice.read(&mut vbuf) {
            Ok(n) if n > 0 => {
                let ev = follower.process_voice(&vbuf[..n]);
                pcm.extend_from_slice(&ev.pcm);
                if let Some(r) = ev.retune {
                    apply_retune(&r, &voice_fh, &mut follower, args.control_hz);
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    if let Some(path) = &args.wav_out {
        if !pcm.is_empty() {
            let _ = crate::wav::write_wav(path, crate::VOICE_RATE, &pcm);
            println!(
                "wrote {path} ({:.1}s voice)",
                pcm.len() as f64 / crate::VOICE_RATE as f64
            );
        }
    }
}

fn apply_retune(r: &Retune, fh: &FreqHandle, follower: &mut DualSdrFollower, control_hz: f64) {
    match r {
        Retune::Tune { freq_hz, talkgroup } => {
            println!(
                "  → voice radio → {:.4} MHz (TG {talkgroup})",
                *freq_hz as f64 / 1e6
            );
            fh.request(*freq_hz as f64);
            follower.retune_done(Some(*freq_hz));
        }
        Retune::Park => {
            println!("  → voice radio parked (back to control)");
            fh.request(control_hz);
            follower.retune_done(None);
        }
    }
}
