//! A control channel granting a call, and the follower decoding both at once.
//!
//! This is the scanner in miniature: one wideband stream carrying a control
//! channel and a traffic channel, where the follower must read the grant, tune
//! the frequency it names, and come back with that call's audio — without ever
//! being told where the traffic channel is.

use hs_core::decoder::Modulation;
use hs_core::follow::TrunkFollower;
use hs_dsp::cqpsk::modulate_iq;
use hs_p25::synth::{build_ldu1, build_tsdu};
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

/// Grant of TALKGROUP on channel 10 of IDEN 1.
fn grant_args() -> u64 {
    (((1u64 << 12) | 10) << 40) | ((TALKGROUP as u64) << 24) | 0xBEEF1
}

/// A control-channel stream repeating the given TSBKs.
fn tsdu_stream(tsbks: &[(u8, u8, u64)]) -> Vec<u8> {
    let mut d = preamble(900);
    for _ in 0..40 {
        d.extend(build_tsdu(0x293, tsbks));
        d.extend(preamble(40));
    }
    d
}

/// Control channel: announce the channel plan, then grant a call on it.
fn control_dibits(plan_base: u64) -> Vec<u8> {
    tsdu_stream(&[(0x3D, 0, iden_args(plan_base)), (0x00, 0, grant_args())])
}

/// Traffic channel: silence, then voice frames.
fn traffic_dibits() -> Vec<u8> {
    let mut frames: [ImbeFrame; 9] = [[[0u8; 23]; 8]; 9];
    let widths = [23usize, 23, 23, 23, 15, 15, 15, 7];
    for (k, fr) in frames.iter_mut().enumerate() {
        for (w, row) in fr.iter_mut().enumerate() {
            for (x, cell) in row.iter_mut().enumerate().take(widths[w]) {
                *cell = (((k + 1) * (w + 2) * (x + 5)) % 2) as u8;
            }
        }
    }
    let mut d = preamble(900);
    for _ in 0..20 {
        d.extend(build_ldu1(0x293, &frames));
        d.extend(preamble(40));
    }
    d
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
        &tsdu_stream(&[(0x00, 0, grant_args())]),
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
