//! Trunking: site/system state machine, control-channel following, and
//! voice-grant tracking, plus channel→frequency mapping from IDEN_UP.

use std::collections::HashMap;

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

/// FDMA channel-plan entry from a TSBK IDEN_UP message.
#[derive(Debug, Clone, Copy)]
pub struct IdenPlan {
    pub base_freq_hz: u64,
    pub spacing_hz: u64,
    pub tx_offset_hz: i64,
}

/// A voice channel grant observed on the control channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// Site trunking model: holds the channel plan and resolves grants to
/// tunable downlink frequencies.
#[derive(Default)]
pub struct SiteModel {
    pub state: TrunkState,
    pub system: Option<SystemId>,
    idens: HashMap<u8, IdenPlan>,
}

impl SiteModel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_iden(&mut self, id: u8, plan: IdenPlan) {
        self.idens.insert(id, plan);
        if self.state == TrunkState::Searching {
            self.state = TrunkState::ControlLocked;
        }
    }

    pub fn set_system(&mut self, sys: SystemId) {
        self.system = Some(sys);
    }

    /// Resolve a 16-bit channel field (4-bit IDEN + 12-bit number) to the
    /// downlink (mobile receive) frequency in Hz, if the IDEN is known.
    pub fn channel_to_freq(&self, channel: u16) -> Option<u64> {
        let iden = (channel >> 12) as u8;
        let number = (channel & 0x0FFF) as u64;
        let plan = self.idens.get(&iden)?;
        let base = plan.base_freq_hz as i64 + (number * plan.spacing_hz) as i64;
        // Downlink = base + transmit offset (offset is signed, MHz-scale).
        Some((base + plan.tx_offset_hz) as u64)
    }

    /// Build a resolved grant, or None if the channel plan isn't known yet.
    pub fn resolve_grant(
        &mut self,
        talkgroup: u16,
        source_unit: u32,
        channel: u16,
        encrypted: bool,
    ) -> Option<Grant> {
        let freq_hz = self.channel_to_freq(channel)?;
        self.state = TrunkState::Following;
        Some(Grant {
            talkgroup,
            source_unit,
            freq_hz,
            encrypted,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_channel_to_downlink() {
        let mut site = SiteModel::new();
        // IDEN 1: base 851.0125 MHz, 12.5 kHz spacing, +45 MHz? (P25 800 uses
        // negative offset on downlink plans; sign is per-IDEN.)
        site.set_iden(
            1,
            IdenPlan {
                base_freq_hz: 851_012_500,
                spacing_hz: 12_500,
                tx_offset_hz: 0,
            },
        );
        let channel = (1u16 << 12) | 10; // iden 1, number 10
        assert_eq!(
            site.channel_to_freq(channel),
            Some(851_012_500 + 10 * 12_500)
        );
        let g = site.resolve_grant(0x2F93, 0xBEEF1, channel, false).unwrap();
        assert_eq!(g.freq_hz, 851_137_500);
        assert_eq!(site.state, TrunkState::Following);
    }

    #[test]
    fn unknown_iden_is_none() {
        let site = SiteModel::new();
        assert!(site.channel_to_freq(0x100A).is_none());
    }
}
