//! Find the P25 channels inside a wideband capture by **decoding** it, not by
//! measuring its power.
//!
//! Power is not evidence. The first field capture for this project was tuned
//! to the strongest signal an `rtl_power` sweep could find, and that signal
//! was not P25 at all — the real carrier sat 50 kHz away, well down the slope
//! of the "wrong" one. A spectrum plot cannot tell a P25 control channel from
//! an analog repeater or a data link; only a decoder can.
//!
//! So this sweeps every channel position inside the captured band, runs the
//! real decoder at each, and reports the offsets where P25 frame syncs
//! actually appear — with the NAC, the modulation, and the frame types seen.
//! An RTL-SDR at 240 kHz covers nineteen 12.5 kHz channels at once, so one
//! recording is enough to survey a whole slice of spectrum.

use crate::decoder::{ChannelDecoder, EqMode, Modulation};

/// How far above the band's noise floor a channel must sit before it is worth
/// running the decoder on it.
///
/// This is a screening threshold, not a detection threshold — the decoder
/// still decides what is P25. Its only job is to skip empty air, so it is set
/// low enough that a weak-but-real signal survives screening and only truly
/// vacant spectrum is discarded.
const SCREEN_MARGIN_DB: f32 = 4.0;

/// FFT size for the screening spectrum. At 2.4 MHz this gives ~600 Hz bins,
/// comfortably finer than the 12.5 kHz channel spacing being resolved.
const SCREEN_FFT: usize = 4096;

/// Channel grid step, anchored at **zero offset**, not at a band edge.
///
/// Anchoring matters more than it looks. P25 channels sit 12.5 kHz apart, so
/// if the capture is tuned to any channel centre, every other channel is an
/// exact multiple of 12.5 kHz away — a grid anchored at zero lands on all of
/// them. An edge-anchored sweep is offset by whatever the band edge happens
/// to be and can step straight over every real channel.
///
/// The step is finer than the channel spacing because the receiver tolerates
/// only about ±1 kHz of frequency error before the matched filter starts
/// throwing away signal, and a tuner with uncorrected crystal error does not
/// put the transmitter exactly where the band plan says.
const STEP_HZ: f64 = 2_500.0;

/// P25 channel centres are always an exact multiple of 6.25 kHz, in every
/// band the standard defines. The sweep steps on its own grid and so lands
/// *near* a channel rather than on it; snapping the reported frequency to the
/// channel raster turns "where the sweep happened to look" into "the frequency
/// to tune", which is what a user pastes into --freq.
const CHANNEL_RASTER_HZ: f64 = 6_250.0;

/// Round an absolute frequency to the nearest P25 channel centre.
pub fn snap_to_channel(hz: f64) -> f64 {
    (hz / CHANNEL_RASTER_HZ).round() * CHANNEL_RASTER_HZ
}

/// Leave the band edges alone: the decimator's channel filter rolls off there,
/// so a channel closer than this to the edge cannot be cleanly selected.
const EDGE_MARGIN_HZ: f64 = 12_500.0;

/// A P25 signal found at one offset.
#[derive(Debug, Clone)]
pub struct Found {
    /// Offset from the capture centre, in Hz.
    pub offset_hz: f64,
    /// Absolute frequency, when the capture's centre frequency is known,
    /// snapped to the P25 channel grid (see [`snap_to_channel`]).
    pub freq_hz: Option<f64>,
    pub modulation: Modulation,
    /// Frame syncs detected.
    pub syncs: u32,
    /// Mean sync-correlation bit errors (of 48). Lower is a stronger lock.
    pub mean_sync_errors: f64,
    /// NAC, if any NID decoded. Identifies the system, and matches the `nac`
    /// field RadioReference reports per site.
    pub nac: Option<u16>,
    /// True if any TSBK-bearing frame decoded — the marker of a **control
    /// channel**, which is the channel worth tuning.
    pub control_channel: bool,
    /// True if voice frames decoded — a traffic channel.
    pub voice: bool,
}

impl Found {
    /// One-line description for a report.
    pub fn summary(&self) -> String {
        let what = match (self.control_channel, self.voice) {
            (true, _) => "CONTROL",
            (false, true) => "voice  ",
            _ => "P25    ",
        };
        let nac = match self.nac {
            Some(n) => format!("NAC 0x{n:03X}"),
            None => "NAC ?    ".to_string(),
        };
        let modl = match self.modulation {
            Modulation::C4fm => "C4FM ",
            Modulation::Cqpsk => "CQPSK",
        };
        match self.freq_hz {
            Some(f) => format!(
                "{what}  {:.4} MHz  {modl}  {nac}  {:>4} syncs  err {:.2}",
                f / 1e6,
                self.syncs,
                self.mean_sync_errors
            ),
            None => format!(
                "{what}  {:+8.1} kHz  {modl}  {nac}  {:>4} syncs  err {:.2}",
                self.offset_hz / 1e3,
                self.syncs,
                self.mean_sync_errors
            ),
        }
    }
}

/// How to run a sweep.
pub struct ScanConfig {
    pub sample_rate: f64,
    /// Capture centre frequency, so results can be reported absolutely.
    pub center_hz: Option<f64>,
    /// Ignore an offset that produces fewer syncs than this. One or two syncs
    /// can appear by chance in noise across a long sweep; a real channel
    /// transmits one every 180 ms.
    pub min_syncs: u32,
    /// Seconds of the capture to analyse at each offset. A sweep decodes the
    /// band once per offset per modulation, so the whole cost scales with this
    /// — and a few seconds already carries tens of frame syncs, which is
    /// plenty to tell a channel from noise.
    pub secs: f64,
}

impl ScanConfig {
    pub fn new(sample_rate: f64) -> Self {
        Self {
            sample_rate,
            center_hz: None,
            min_syncs: 4,
            secs: 4.0,
        }
    }

    pub fn secs(mut self, s: f64) -> Self {
        self.secs = s;
        self
    }

    /// Offsets to test, walking outward from zero so the strongest candidates
    /// (usually near the tuned frequency) are reported first on a partial run.
    fn offsets(&self) -> Vec<f64> {
        let half = self.sample_rate / 2.0 - EDGE_MARGIN_HZ;
        let steps = (half / STEP_HZ).floor() as i64;
        let mut v = vec![0.0];
        for k in 1..=steps {
            v.push(k as f64 * STEP_HZ);
            v.push(-(k as f64) * STEP_HZ);
        }
        v
    }

    /// Keep only the offsets where the spectrum shows something above the
    /// noise floor.
    ///
    /// Decoding is expensive — a wideband capture has hundreds of channel
    /// positions and each costs a long channel filter over seconds of samples
    /// — while a spectrum costs one pass over the data. Most of a band is
    /// empty at any instant, so screening first is the difference between a
    /// sweep that takes minutes and one that takes hours.
    fn screen(&self, iq: &[f32], offsets: Vec<f64>) -> Vec<f64> {
        let psd = hs_dsp::fft::power_spectrum_db(iq, SCREEN_FFT);
        let floor = hs_dsp::fft::median(&psd);
        let bin_hz = self.sample_rate / SCREEN_FFT as f64;

        offsets
            .into_iter()
            .filter(|&off| {
                // Peak power anywhere inside this channel's occupied width.
                let lo = off - 6_250.0;
                let hi = off + 6_250.0;
                let idx = |f: f64| {
                    ((f / bin_hz) + SCREEN_FFT as f64 / 2.0)
                        .round()
                        .clamp(0.0, SCREEN_FFT as f64 - 1.0) as usize
                };
                let (a, b) = (idx(lo), idx(hi));
                psd[a..=b.max(a)].iter().fold(f32::MIN, |m, &v| m.max(v)) > floor + SCREEN_MARGIN_DB
            })
            .collect()
    }

    pub fn center(mut self, hz: f64) -> Self {
        self.center_hz = Some(hz);
        self
    }
}

/// Sweep `iq` (interleaved f32) across the captured band and return every
/// offset where P25 decodes, best lock first.
///
/// Both modulations are tried at each offset because a site's modulation is
/// not knowable in advance — simulcast sites run CQPSK/LSM, others C4FM — and
/// the wrong one simply produces no syncs, which is itself the answer.
pub fn scan(iq: &[f32], cfg: &ScanConfig) -> Vec<Found> {
    // Analysing the whole of a long capture at every offset is pure waste;
    // a few seconds already holds tens of frame syncs.
    let want = (cfg.secs * cfg.sample_rate * 2.0) as usize;
    let iq = &iq[..want.min(iq.len())];

    let candidates = cfg.screen(iq, cfg.offsets());
    let mut out: Vec<Found> = Vec::new();
    for offset in candidates {
        let mut best: Option<Found> = None;
        for modulation in [Modulation::Cqpsk, Modulation::C4fm] {
            if let Some(f) = try_offset(iq, cfg, offset, modulation) {
                let better = match &best {
                    Some(b) => f.syncs > b.syncs,
                    None => true,
                };
                if better {
                    best = Some(f);
                }
            }
        }
        if let Some(f) = best {
            out.push(f);
        }
    }

    // A single transmitter shows up at several adjacent grid steps (the
    // channel is wider than the step). Keep only the strongest of each
    // cluster, so the report names channels rather than sweep positions.
    out.sort_by(|a, b| b.syncs.cmp(&a.syncs));
    dedupe_adjacent(&mut out);
    out.sort_by(|a, b| b.syncs.cmp(&a.syncs));
    out
}

fn try_offset(iq: &[f32], cfg: &ScanConfig, offset: f64, modulation: Modulation) -> Option<Found> {
    let mut dec = ChannelDecoder::with_offset(cfg.sample_rate, modulation, EqMode::Enabled, offset);
    let out = dec.process(iq);
    if out.syncs < cfg.min_syncs {
        return None;
    }
    let diag = dec.diagnostics();
    Some(Found {
        offset_hz: offset,
        freq_hz: cfg.center_hz.map(|c| snap_to_channel(c + offset)),
        modulation,
        syncs: out.syncs,
        mean_sync_errors: diag.mean_sync_errors(),
        nac: dominant_nac(diag),
        // A resolved grant, or any trunking traffic, marks the control channel.
        control_channel: !out.grants.is_empty() || !diag.grants.is_empty(),
        voice: !out.pcm.is_empty(),
    })
}

/// The most frequently seen NAC. A stray NID error can invent a value, so the
/// mode across the capture is more trustworthy than the first one decoded.
fn dominant_nac(diag: &crate::diag::Diagnostics) -> Option<u16> {
    let mut counts: Vec<(u16, u32)> = Vec::new();
    for n in &diag.nids {
        match counts.iter_mut().find(|(v, _)| *v == n.nac) {
            Some((_, c)) => *c += 1,
            None => counts.push((n.nac, 1)),
        }
    }
    counts.into_iter().max_by_key(|&(_, c)| c).map(|(v, _)| v)
}

/// Collapse runs of adjacent offsets belonging to one transmitter, keeping the
/// entry with the most syncs.
fn dedupe_adjacent(found: &mut Vec<Found>) {
    // A P25 channel is 12.5 kHz wide, so hits within that of each other are
    // the same signal seen from neighbouring sweep positions.
    const CLUSTER_HZ: f64 = 12_500.0;
    let mut kept: Vec<Found> = Vec::new();
    // `found` arrives strongest-first, so the first entry of each cluster is
    // the one to keep and later neighbours are absorbed into it.
    for f in found.drain(..) {
        let dup = kept
            .iter()
            .any(|k| (k.offset_hz - f.offset_hz).abs() < CLUSTER_HZ);
        if !dup {
            kept.push(f);
        }
    }
    *found = kept;
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_dsp::cqpsk::modulate_iq;
    use hs_p25::synth::build_tsdu;

    /// Place a synthesized P25 control channel at a known offset inside a
    /// wideband capture and confirm the sweep finds it there — the exact
    /// situation the Marion County recording presented.
    #[test]
    fn finds_a_control_channel_at_an_offset() {
        const RATE: f64 = 240_000.0;
        const SPS: usize = 50;
        const OFFSET: f64 = 50_000.0;

        let iden_args: u64 = {
            let iden = 1u64 << 60;
            let bw = 100u64 << 51;
            let sign = 1u64 << 50;
            let spacing = 100u64 << 32;
            iden | bw | sign | spacing | (851_012_500u64 / 5)
        };
        let grant_args: u64 = (((1u64 << 12) | 10) << 40) | (0x2F93u64 << 24) | 0xBEEF1;

        // Preamble long enough for blind acquisition, then repeated TSDUs so
        // several syncs land inside the capture.
        let mut dibits: Vec<u8> = (0..900).map(|i| ((i * 5 + i / 3) % 4) as u8).collect();
        for _ in 0..6 {
            dibits.extend(build_tsdu(
                0x293,
                &[(0x3D, 0, iden_args), (0x00, 0, grant_args)],
            ));
            dibits.extend((0..40).map(|i| ((i * 5 + i / 3) % 4) as u8));
        }

        // Modulate at baseband, then shift up to OFFSET so the signal sits
        // where the sweep has to find it rather than at DC.
        let base = modulate_iq(&dibits, SPS, 0.2);
        let mut iq = Vec::with_capacity(base.len() * 2);
        for (n, s) in base.iter().enumerate() {
            let w = 2.0 * std::f64::consts::PI * OFFSET * n as f64 / RATE;
            let (sin, cos) = (w.sin() as f32, w.cos() as f32);
            iq.push(s.re * cos - s.im * sin);
            iq.push(s.re * sin + s.im * cos);
        }

        let cfg = ScanConfig::new(RATE).center(858_937_500.0);
        let found = scan(&iq, &cfg);

        assert!(!found.is_empty(), "sweep found no P25 signal");
        let best = &found[0];
        assert!(
            (best.offset_hz - OFFSET).abs() <= 6_250.0,
            "found at {:+.0} Hz, expected near {OFFSET:+.0}",
            best.offset_hz
        );
        assert_eq!(best.modulation, Modulation::Cqpsk);
        assert_eq!(best.nac, Some(0x293));
        assert!(
            best.control_channel,
            "TSBK traffic should mark a control channel"
        );
        assert_eq!(
            best.freq_hz,
            Some(858_987_500.0),
            "frequency should snap to the P25 channel raster, not report the \
             sweep position"
        );
        // The one transmitter must be reported once, not once per sweep step.
        assert_eq!(
            found.len(),
            1,
            "adjacent hits were not collapsed: {found:?}"
        );
    }

    #[test]
    fn snaps_reported_frequencies_onto_the_channel_raster() {
        // The sweep lands near a channel, not on it; every reported frequency
        // must be a real P25 channel centre.
        assert_eq!(snap_to_channel(858_990_000.0), 858_987_500.0);
        assert_eq!(snap_to_channel(858_985_000.0), 858_987_500.0);
        assert_eq!(snap_to_channel(851_012_500.0), 851_012_500.0);
    }

    #[test]
    fn reports_nothing_for_noise() {
        // Guards the threshold: a sweep over noise must stay silent, or every
        // report becomes untrustworthy.
        let mut s = 0x1234_5678u64;
        let iq: Vec<f32> = (0..600_000)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                ((s >> 40) as f32 / (1u64 << 23) as f32) - 1.0
            })
            .collect();
        let found = scan(&iq, &ScanConfig::new(240_000.0));
        assert!(found.is_empty(), "sweep hallucinated signals: {found:?}");
    }
}
