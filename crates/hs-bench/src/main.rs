//! hs-bench: the measuring instrument, built before the thing being measured.
//!
//! Runs an IQ corpus through the decode chain and reports, per recording:
//! sync-loss rate, pre-FEC BER, FEC correction rate, TSBK decode rate, voice
//! frame error rate, and calls successfully audio-decoded. Baselines from
//! SDRTrunk / OP25 / GopherTrunk on the same corpus live in
//! `results/baselines.md` (checked in); the corpus itself (large IQ files)
//! is never committed — see .gitignore.

use hs_p25::{sync_bit_errors, FRAME_SYNC};

#[derive(Debug, Default)]
pub struct DecodeMetrics {
    pub sync_losses: u64,
    pub bits_total: u64,
    pub bit_errors_pre_fec: u64,
    pub tsbk_attempted: u64,
    pub tsbk_decoded: u64,
    pub voice_frames: u64,
    pub voice_frame_errors: u64,
}

impl DecodeMetrics {
    pub fn ber(&self) -> f64 {
        if self.bits_total == 0 {
            return 0.0;
        }
        self.bit_errors_pre_fec as f64 / self.bits_total as f64
    }
}

fn main() {
    // Self-check until the decode chain exists: the sync detector must see
    // its own sync word perfectly.
    assert_eq!(sync_bit_errors(FRAME_SYNC), 0);
    println!("hs-bench scaffold OK — corpus runner lands in Phase 0.");
    println!("Usage (planned): hs-bench <corpus-dir> --report results/run.md");
}
