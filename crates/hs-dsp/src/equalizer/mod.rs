//! Adaptive equalizers — the core differentiator of HoosierSDR.
//!
//! These run on complex baseband **before** differential detection, which is
//! the placement no other open-source P25 decoder uses (see
//! docs/ARCHITECTURE.md §1). Training reference is the 24-symbol P25 Frame
//! Sync Word, available every 180 ms.
//!
//! Build order (§4): LMS FSE → CMA fallback → DFE → MLSE.

pub mod lms;
pub mod real_lms;

// Planned: pub mod cma;  — blind fallback when sync is lost
// Planned: pub mod dfe;  — feedforward + feedback taps
// Planned: pub mod mlse; — Viterbi equalization over 2–3 symbol memory

pub use lms::LmsFse;
pub use real_lms::RealLmsEq;
