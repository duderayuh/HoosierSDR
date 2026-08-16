//! Trunking: site/system state machine, control-channel following, and
//! voice-grant tracking. Full implementation lands in Phase 2.

/// Identity of a P25 trunked system as observed on the air.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemId {
    pub wacn: u32,
    pub sys_id: u16,
}

/// Hoosier SAFE-T, the primary target system (Phase I FDMA; Phase II TDMA
/// pilot at Fort Wayne and Westville only).
pub const SAFE_T: SystemId = SystemId {
    wacn: 0xBEE00,
    sys_id: 0x6BD,
};

/// A voice channel grant observed on the control channel.
#[derive(Debug, Clone, Copy)]
pub struct Grant {
    pub talkgroup: u16,
    pub source_unit: u32,
    pub freq_hz: u64,
    /// Encrypted grants are tracked for display but never audio-decoded.
    pub encrypted: bool,
}

/// Control-channel follower state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrunkState {
    #[default]
    Searching,
    ControlLocked,
    Following,
}
