//! A control channel granting a call, and the follower decoding both at once.
//!
//! This is the scanner in miniature: one wideband stream carrying a control
//! channel and a traffic channel, where the follower must read the grant, tune
//! the frequency it names, and come back with that call's audio — without ever
//! being told where the traffic channel is.

use hs_core::decoder::Modulation;
use hs_core::follow::TrunkFollower;
use hs_dsp::cqpsk::modulate_iq;
use hs_p25::synth::{build_ldu1, build_tdu, build_tsdu};
use hs_p25::voice::ImbeFrame;

const RATE: f64 = 288_000.0;
const CENTER: f64 = 857_650_000.0;
const CONTROL: f64 = 857_662_500.0;
/// Deliberately not zero: an uncalibrated tuner puts every channel off its
/// nominal frequency, and the follower has to carry that across from the
/// control channel to the ones it is told about.
const TUNER_ERROR: f64 = 6_000.0;
/// Channel-plan base that puts the granted channel inside this capture.
const PLAN_BASE: u64 = 857_575_000;
/// Where channel 10 of that plan lands: 50 kHz above centre, comfortably
/// inside the 288 kHz window.
const TRAFFIC: f64 = (PLAN_BASE + 10 * 12_500) as f64;
/// A second plan whose channels sit megahertz away — a real system grants
/// across a whole band, most of which one tuner cannot see.
const FAR_PLAN_BASE: u64 = 851_012_500;
const TALKGROUP: u16 = 0x2F93;

fn preamble(n: usize) -> Vec<u8> {
    (0..n).map(|i| ((i * 5 + i / 3) % 4) as u8).collect()
}

/// IDEN_UP announcing the 12.5 kHz plan rooted at `plan_base` as IDEN 1.
fn iden_args(plan_base: u64) -> u64 {
    let iden = 1u64 << 60;
    let bw = 100u64 << 51;
    let sign = 1u64 << 50;
    let spacing = 100u64 << 32;
    iden | bw | sign | spacing | (plan_base / 5)
}

/// Grant of `talkgroup` on channel 10 of IDEN 1.
fn grant_args(talkgroup: u16) -> u64 {
    (((1u64 << 12) | 10) << 40) | ((talkgroup as u64) << 24) | 0xBEEF1
}

/// A control-channel stream repeating the given TSBKs `reps` times.
fn tsdu_stream_n(tsbks: &[(u8, u8, u64)], reps: usize) -> Vec<u8> {
    let mut d = preamble(900);
    for _ in 0..reps {
        d.extend(build_tsdu(0x293, tsbks));
        d.extend(preamble(40));
    }
    d
}

/// A control-channel stream repeating the given TSBKs.
fn tsdu_stream(tsbks: &[(u8, u8, u64)]) -> Vec<u8> {
    tsdu_stream_n(tsbks, 40)
}

/// Control channel: announce the channel plan, then grant a call on it.
fn control_dibits(plan_base: u64) -> Vec<u8> {
    tsdu_stream(&[
        (0x3D, 0, iden_args(plan_base)),
        (0x00, 0, grant_args(TALKGROUP)),
    ])
}

fn voice_frames() -> [ImbeFrame; 9] {
    let mut frames: [ImbeFrame; 9] = [[[0u8; 23]; 8]; 9];
    let widths = [23usize, 23, 23, 23, 15, 15, 15, 7];
    for (k, fr) in frames.iter_mut().enumerate() {
        for (w, row) in fr.iter_mut().enumerate() {
            for (x, cell) in row.iter_mut().enumerate().take(widths[w]) {
                *cell = (((k + 1) * (w + 2) * (x + 5)) % 2) as u8;
            }
        }
    }
    frames
}

/// Traffic channel: `reps` voice frames after a preamble.
fn traffic_dibits_n(reps: usize) -> Vec<u8> {
    let frames = voice_frames();
    let mut d = preamble(900);
    for _ in 0..reps {
        d.extend(build_ldu1(0x293, &frames));
        d.extend(preamble(40));
    }
    d
}

/// Traffic channel: silence, then voice frames.
fn traffic_dibits() -> Vec<u8> {
    traffic_dibits_n(20)
}

/// Modulate dibits and shift to `freq`, summing into a shared band.
fn add_to_band(band: &mut Vec<f32>, dibits: &[u8], freq: f64) {
    let sps = (RATE / 4800.0) as usize;
    let base = modulate_iq(dibits, sps, 0.2);
    let offset = freq - CENTER;
    if band.len() < base.len() * 2 {
        band.resize(base.len() * 2, 0.0);
    }
    for (n, s) in base.iter().enumerate() {
        let w = 2.0 * std::f64::consts::PI * offset * n as f64 / RATE;
        let (sin, cos) = (w.sin() as f32, w.cos() as f32);
        band[n * 2] += s.re * cos - s.im * sin;
        band[n * 2 + 1] += s.re * sin + s.im * cos;
    }
}

#[test]
fn follows_a_grant_onto_its_traffic_channel() {
    let mut band = Vec::new();
    // Both channels transmit at once, each offset by the tuner's error.
    add_to_band(&mut band, &control_dibits(PLAN_BASE), CONTROL + TUNER_ERROR);
    add_to_band(&mut band, &traffic_dibits(), TRAFFIC + TUNER_ERROR);

    // The follower is told where the control channel *actually* is; every
    // other frequency it works out for itself.
    let mut f = TrunkFollower::new(
        RATE,
        CENTER,
        CONTROL,
        CONTROL + TUNER_ERROR,
        Modulation::Cqpsk,
    );
    assert_eq!(f.correction_hz(), TUNER_ERROR);

    let block = (RATE as usize / 10) * 2;
    let mut started = Vec::new();
    let mut completed = Vec::new();
    let mut control_syncs = 0;
    for chunk in band.chunks(block) {
        let out = f.process(chunk);
        control_syncs += out.control_syncs;
        started.extend(out.started);
        completed.extend(out.completed);
    }

    assert!(control_syncs > 0, "control channel never decoded");

    // The traffic channel here is CQPSK, so a completed call must report CQPSK
    // — the follower runs both modulations and keeps whichever decodes cleanly.
    if let Some(call) = completed.iter().find(|c| c.talkgroup == TALKGROUP) {
        assert_eq!(
            call.modulation,
            Some(Modulation::Cqpsk),
            "CQPSK traffic decoded as the wrong modulation"
        );
    }
    assert!(
        started
            .iter()
            .any(|(tg, hz)| *tg == TALKGROUP && *hz == TRAFFIC as u64),
        "grant not followed: started {started:?}"
    );

    // The call may still be open at end of stream; either way it must have
    // been tuned, which the start event above proves.
    let audio: usize = completed.iter().map(|c| c.pcm.len()).sum();
    if !completed.is_empty() {
        assert!(
            completed.iter().any(|c| c.talkgroup == TALKGROUP),
            "a call completed but not the one granted"
        );
        assert!(audio > 0, "call completed with no audio");
    }
}

#[test]
fn a_terminator_ends_the_call_without_waiting_for_silence() {
    // A traffic channel closes every transmission with a terminator (TDU).
    // The call must retire a short hang after it — not a couple of seconds of
    // quiet-timeout later — so this stream is sized to make the difference
    // observable: after the traffic ends, the capture keeps running for less
    // time than the quiet timeout needs, so only the terminator path can
    // complete the call before the recording ends.
    let mut traffic = traffic_dibits_n(6);
    for _ in 0..4 {
        traffic.extend(build_tdu(0x293));
        traffic.extend(preamble(40));
    }

    let mut band = Vec::new();
    add_to_band(&mut band, &control_dibits(PLAN_BASE), CONTROL + TUNER_ERROR);
    add_to_band(&mut band, &traffic, TRAFFIC + TUNER_ERROR);

    let mut f = TrunkFollower::new(
        RATE,
        CENTER,
        CONTROL,
        CONTROL + TUNER_ERROR,
        Modulation::Cqpsk,
    );
    let block = (RATE as usize / 10) * 2;
    let mut completed_in_loop = Vec::new();
    for chunk in band.chunks(block) {
        completed_in_loop.extend(f.process(chunk).completed);
    }

    assert!(
        completed_in_loop
            .iter()
            .any(|c| c.talkgroup == TALKGROUP && !c.pcm.is_empty()),
        "terminator did not retire the call before the stream ended: {:?}",
        completed_in_loop
            .iter()
            .map(|c| (c.talkgroup, c.pcm.len()))
            .collect::<Vec<_>>()
    );
}

#[test]
fn a_regrant_for_another_talkgroup_splits_the_calls() {
    // Two back-to-back transmissions on one traffic channel, granted to two
    // different talkgroups. Skipping the second grant because the frequency
    // was already active merged both into the first call — wrong talkgroup,
    // wrong unit — so a reassignment must retire the old call on the spot.
    const TALKGROUP2: u16 = 0x1111;
    let mut control = tsdu_stream_n(
        &[
            (0x3D, 0, iden_args(PLAN_BASE)),
            (0x00, 0, grant_args(TALKGROUP)),
        ],
        15,
    );
    control.extend(tsdu_stream_n(&[(0x00, 0, grant_args(TALKGROUP2))], 15));

    let mut band = Vec::new();
    add_to_band(&mut band, &control, CONTROL + TUNER_ERROR);
    // The voice runs continuously across the reassignment — exactly the case
    // where a quiet timeout can never separate the two calls.
    add_to_band(&mut band, &traffic_dibits_n(12), TRAFFIC + TUNER_ERROR);

    let mut f = TrunkFollower::new(
        RATE,
        CENTER,
        CONTROL,
        CONTROL + TUNER_ERROR,
        Modulation::Cqpsk,
    );
    let block = (RATE as usize / 10) * 2;
    let mut started = Vec::new();
    let mut completed = Vec::new();
    for chunk in band.chunks(block) {
        let out = f.process(chunk);
        started.extend(out.started);
        completed.extend(out.completed);
    }
    let in_loop = completed.len();
    completed.extend(f.finish());

    assert!(
        started.iter().any(|(tg, _)| *tg == TALKGROUP)
            && started.iter().any(|(tg, _)| *tg == TALKGROUP2),
        "both grants must open calls: started {started:?}"
    );
    let first = completed
        .iter()
        .find(|c| c.talkgroup == TALKGROUP)
        .expect("first call never completed");
    assert!(
        !first.pcm.is_empty(),
        "first call lost its audio in the reassignment"
    );
    assert!(
        in_loop >= 1,
        "the reassignment must retire the first call mid-stream, not at finish()"
    );
    assert!(
        completed.iter().any(|c| c.talkgroup == TALKGROUP2),
        "second call missing: {:?}",
        completed.iter().map(|c| c.talkgroup).collect::<Vec<_>>()
    );
}

#[test]
fn hunts_onto_the_announced_alternate_when_the_control_channel_moves() {
    // A site rotates its control channel and announces the alternates over
    // SCCB while it runs. Here the primary broadcasts the channel plan and an
    // SCCB naming channel 4 as an alternate, then goes silent; the alternate
    // then starts issuing grants — *without* re-broadcasting the plan, so a
    // grant only resolves if the follower carried the plan across the move.
    const ALT_CHANNEL: u16 = (1 << 12) | 4;
    const ALT: f64 = (PLAN_BASE + 4 * 12_500) as f64; // 25 kHz below centre
    let sccb_args: u64 =
        (1u64 << 56) | (1u64 << 48) | ((ALT_CHANNEL as u64) << 32) | (0x70u64 << 24);

    let block = (RATE as usize / 10) * 2;
    let mut band = Vec::new();
    // Phase A: the primary announces the plan and the alternate.
    add_to_band(
        &mut band,
        &tsdu_stream(&[(0x3D, 0, iden_args(PLAN_BASE)), (0x39, 0, sccb_args)]),
        CONTROL + TUNER_ERROR,
    );
    // Silence: past the loss limit (20 blocks), with headroom.
    band.extend(std::iter::repeat_n(0.0f32, block * 24));
    // Phase B: the alternate takes over, granting a call with no IDEN_UP.
    // Built in its own buffer because add_to_band sums from sample 0.
    let mut phase_b = Vec::new();
    add_to_band(
        &mut phase_b,
        &tsdu_stream(&[(0x00, 0, grant_args(TALKGROUP))]),
        ALT + TUNER_ERROR,
    );
    band.extend(phase_b);

    let mut f = TrunkFollower::new(
        RATE,
        CENTER,
        CONTROL,
        CONTROL + TUNER_ERROR,
        Modulation::Cqpsk,
    );
    let mut moved = Vec::new();
    let mut started = Vec::new();
    for chunk in band.chunks(block) {
        let out = f.process(chunk);
        moved.extend(out.control_moved);
        started.extend(out.started);
    }

    assert_eq!(
        moved,
        vec![(CONTROL as u64, ALT as u64)],
        "follower did not move to the announced alternate exactly once"
    );
    assert_eq!(f.control_hz(), ALT as u64);
    assert!(
        started
            .iter()
            .any(|(tg, hz)| *tg == TALKGROUP && *hz == TRAFFIC as u64),
        "grant from the alternate control channel not followed \
         (the channel plan was lost in the move): started {started:?}"
    );
}

#[test]
fn ignores_grants_outside_the_captured_band() {
    // A trunked system grants across its whole band; a single tuner sees a
    // slice. Following a frequency that is not in the capture would decode
    // whatever aliases into its place.
    let mut band = Vec::new();
    add_to_band(
        &mut band,
        &control_dibits(FAR_PLAN_BASE),
        CONTROL + TUNER_ERROR,
    );

    let mut f = TrunkFollower::new(
        RATE,
        CENTER,
        CONTROL,
        CONTROL + TUNER_ERROR,
        Modulation::Cqpsk,
    );
    let block = (RATE as usize / 10) * 2;
    for chunk in band.chunks(block) {
        // The granted channel sits ~6.5 MHz away, far outside a 288 kHz capture.
        for (_, hz) in f.process(chunk).started {
            let offset = (hz as f64 - CENTER).abs();
            assert!(
                offset < RATE / 2.0,
                "followed {hz} Hz, which is outside the captured band"
            );
        }
    }
}

/// Strict version: the granted call must actually yield CQPSK audio through
/// the channelizer path, draining it at end of stream rather than hoping it
/// completed. Guards the traffic path against a channelizer regression.
#[test]
fn traffic_audio_decodes_through_the_channelizer() {
    let mut band = Vec::new();
    add_to_band(&mut band, &control_dibits(PLAN_BASE), CONTROL + TUNER_ERROR);
    add_to_band(&mut band, &traffic_dibits_n(6), TRAFFIC + TUNER_ERROR);
    let mut f = TrunkFollower::new(RATE, CENTER, CONTROL, CONTROL + TUNER_ERROR, Modulation::Cqpsk);
    let block = (RATE as usize / 10) * 2;
    let mut completed = Vec::new();
    let mut started = 0;
    for chunk in band.chunks(block) {
        let out = f.process(chunk);
        started += out.started.len();
        completed.extend(out.completed);
    }
    completed.extend(f.finish());
    assert!(started >= 1, "grant not followed");
    let call = completed
        .iter()
        .find(|c| c.talkgroup == TALKGROUP)
        .expect("granted call reported");
    eprintln!(
        "call: {} samples, syncs c4fm {} cqpsk {}, mod {:?}",
        call.pcm.len(),
        call.syncs_c4fm,
        call.syncs_cqpsk,
        call.modulation
    );
    assert!(call.syncs_cqpsk > 0, "no CQPSK frame sync on the traffic channel");
    assert!(!call.pcm.is_empty(), "call completed with no audio");
    assert_eq!(call.modulation, Some(Modulation::Cqpsk));
}

/// A neighbour one channel over (12.5 kHz, three times the amplitude) must
/// not stop the granted call decoding, with either extraction method. (Four
/// such neighbours at once defeat the synthetic CQPSK chain classically as
/// well, so this is a smoke test, not an adjacent-channel specification.)
fn neighbour_call(classic: bool) -> hs_core::follow::Call {
    let mut band = Vec::new();
    add_to_band(&mut band, &control_dibits(PLAN_BASE), CONTROL + TUNER_ERROR);
    add_to_band(&mut band, &traffic_dibits_n(6), TRAFFIC + TUNER_ERROR);
    let mut neighbour = Vec::new();
    add_to_band(&mut neighbour, &traffic_dibits_n(6), TRAFFIC + 12_500.0 + TUNER_ERROR);
    for (b, n) in band.iter_mut().zip(neighbour.iter()) {
        *b += 3.0 * n;
    }
    let mut f = TrunkFollower::new(RATE, CENTER, CONTROL, CONTROL + TUNER_ERROR, Modulation::Cqpsk);
    f.set_channelizer(!classic);
    let block = (RATE as usize / 10) * 2;
    let mut completed = Vec::new();
    for chunk in band.chunks(block) {
        completed.extend(f.process(chunk).completed);
    }
    completed.extend(f.finish());
    let call = completed
        .into_iter()
        .find(|c| c.talkgroup == TALKGROUP)
        .expect("granted call reported");
    eprintln!(
        "with neighbour ({}): {} samples, syncs c4fm {} cqpsk {}, mod {:?}",
        if classic { "classic" } else { "channelizer" },
        call.pcm.len(),
        call.syncs_c4fm,
        call.syncs_cqpsk,
        call.modulation
    );
    call
}

#[test]
fn a_loud_adjacent_channel_does_not_garble_the_call() {
    let call = neighbour_call(false);
    assert!(call.syncs_cqpsk > 0, "traffic channel lost to its neighbour");
    assert!(!call.pcm.is_empty(), "no audio with a loud neighbour");
    assert_eq!(call.modulation, Some(Modulation::Cqpsk));
}

#[test]
fn a_loud_adjacent_channel_does_not_garble_the_call_classically() {
    let call = neighbour_call(true);
    assert!(call.syncs_cqpsk > 0, "traffic channel lost to its neighbour");
    assert!(!call.pcm.is_empty(), "no audio with a loud neighbour");
}

/// A second radio parked elsewhere on the site's span: a grant outside the
/// primary band is routed to the extra band and decodes there, while the
/// primary band's own call still decodes. This is the "use the other
/// tuners to cover the rest of the band" behaviour.
#[test]
fn a_grant_outside_the_primary_band_decodes_on_an_extra_radio() {
    const TG2: u16 = 0x2F94;
    // Channel 40 of IDEN 1: 857.575 + 40 × 0.0125 = 858.075 MHz, +425 kHz
    // from the primary centre — outside its ±131.5 kHz.
    let traffic2 = (PLAN_BASE + 40 * 12_500) as f64;
    let center2 = 858_050_000.0;
    let grant2 = (((1u64 << 12) | 40) << 40) | ((TG2 as u64) << 24) | 0xBEEF2;
    let control = tsdu_stream(&[
        (0x3D, 0, iden_args(PLAN_BASE)),
        (0x00, 0, grant_args(TALKGROUP)),
        (0x00, 0, grant2),
    ]);
    let mut band1 = Vec::new();
    add_to_band(&mut band1, &control, CONTROL + TUNER_ERROR);
    add_to_band(&mut band1, &traffic_dibits_n(6), TRAFFIC + TUNER_ERROR);
    // The extra radio's own IQ, relative to its own centre.
    let mut band2 = Vec::new();
    {
        let sps = (RATE / 4800.0) as usize;
        let base = modulate_iq(&traffic_dibits_n(6), sps, 0.2);
        let offset = traffic2 + TUNER_ERROR - center2;
        band2.resize(base.len() * 2, 0.0);
        for (n, s) in base.iter().enumerate() {
            let w = 2.0 * std::f64::consts::PI * offset * n as f64 / RATE;
            let (sin, cos) = (w.sin() as f32, w.cos() as f32);
            band2[n * 2] += s.re * cos - s.im * sin;
            band2[n * 2 + 1] += s.re * sin + s.im * cos;
        }
    }
    let mut f = TrunkFollower::new(RATE, CENTER, CONTROL, CONTROL + TUNER_ERROR, Modulation::Cqpsk);
    let extra = f.add_band(center2, RATE);
    assert_eq!(f.bands().len(), 2);
    let block = (RATE as usize / 10) * 2;
    let mut started = Vec::new();
    let mut completed = Vec::new();
    let mut oob = 0;
    let n = band1.len().max(band2.len());
    let mut i = 0;
    while i < n {
        let end = (i + block).min(n);
        if i < band1.len() {
            let out = f.process(&band1[i..end.min(band1.len())]);
            started.extend(out.started);
            completed.extend(out.completed);
            oob += out.grants_out_of_band.len();
        }
        if i < band2.len() {
            let out = f.process_band(extra, &band2[i..end.min(band2.len())]);
            completed.extend(out.completed);
        }
        i += block;
    }
    completed.extend(f.finish());
    assert!(
        started.iter().any(|(tg, hz)| *tg == TG2 && *hz == traffic2 as u64),
        "grant on the extra band not followed: started {started:?}, out of band {oob}"
    );
    assert!(started.iter().any(|(tg, _)| *tg == TALKGROUP), "primary band's call not followed");
    let c2 = completed.iter().find(|c| c.talkgroup == TG2).expect("extra-band call reported");
    eprintln!("extra band: {} samples, syncs c4fm {} cqpsk {}", c2.pcm.len(), c2.syncs_c4fm, c2.syncs_cqpsk);
    assert!(c2.syncs_cqpsk > 0, "no sync on the extra band's call");
    assert!(!c2.pcm.is_empty(), "extra band's call produced no audio");
}
