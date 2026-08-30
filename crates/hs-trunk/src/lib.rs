//! Trunking: site/system state machine, control-channel following, and
//! voice-grant tracking, plus channel→frequency mapping from IDEN_UP.

use std::collections::HashMap;

/// Identity of a P25 trunked system as observed on the air.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemId {
    pub wacn: u32,
    pub sys_id: u16,
}

/// A statewide P25 Phase I system (Phase I FDMA; Phase II TDMA pilot in
/// limited areas only).
pub const STATEWIDE: SystemId = SystemId {
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

/// A neighbouring site announced by Adjacent Status Broadcast (0x3C): where a
/// radio — or a scanner — could go next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Neighbour {
    pub sys_id: u16,
    pub rfss: u8,
    pub site: u8,
    /// Its control channel, in raw channel form (resolve through the plans).
    pub channel: u16,
}

/// Site trunking model: holds the channel plan and resolves grants to
/// tunable downlink frequencies.
#[derive(Default, Clone)]
pub struct SiteModel {
    pub state: TrunkState,
    pub system: Option<SystemId>,
    /// This site's RFSS and site numbers, from RFSS Status Broadcast.
    pub rfss: Option<u8>,
    pub site: Option<u8>,
    idens: HashMap<u8, IdenPlan>,
    /// Alternate control channels announced by SCCB (opcode 0x39), in raw
    /// channel form so they resolve against whichever IDEN plan applies.
    secondary_ccs: Vec<u16>,
    /// Neighbouring sites, in first-heard order.
    neighbours: Vec<Neighbour>,
}

impl SiteModel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record this site's identity from an RFSS Status Broadcast.
    pub fn set_rfss_site(&mut self, rfss: u8, site: u8) {
        self.rfss = Some(rfss);
        self.site = Some(site);
    }

    /// Record a neighbouring site; repeats update the channel in place.
    pub fn add_neighbour(&mut self, n: Neighbour) {
        match self
            .neighbours
            .iter_mut()
            .find(|x| x.sys_id == n.sys_id && x.rfss == n.rfss && x.site == n.site)
        {
            Some(x) => *x = n,
            None => self.neighbours.push(n),
        }
    }

    /// Neighbouring sites with their control channels resolved to Hz where
    /// the plan is known (`None` until it is).
    pub fn neighbours(&self) -> Vec<(Neighbour, Option<u64>)> {
        self.neighbours
            .iter()
            .map(|n| (*n, self.channel_to_freq(n.channel)))
            .collect()
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

    /// Record an alternate control channel from a Secondary Control Channel
    /// Broadcast.
    pub fn add_secondary_cc(&mut self, channel: u16) {
        if !self.secondary_ccs.contains(&channel) {
            self.secondary_ccs.push(channel);
        }
    }

    /// The site's announced alternate control channels, resolved to downlink
    /// frequencies. A channel whose IDEN plan has not been heard yet is
    /// omitted — there is nothing to tune until the plan arrives.
    /// The channel plans announced so far, by identifier.
    pub fn idens(&self) -> impl Iterator<Item = (u8, &IdenPlan)> {
        let mut v: Vec<(u8, &IdenPlan)> = self.idens.iter().map(|(k, p)| (*k, p)).collect();
        v.sort_by_key(|(k, _)| *k);
        v.into_iter()
    }

    pub fn secondary_cc_freqs(&self) -> Vec<u64> {
        self.secondary_ccs
            .iter()
            .filter_map(|&c| self.channel_to_freq(c))
            .collect()
    }

    /// Resolve a 16-bit channel field (4-bit IDEN + 12-bit number) to the
    /// **downlink** (base transmit / mobile receive) frequency in Hz, if the
    /// IDEN is known. This is the frequency a receiver tunes.
    ///
    /// The channel plan's base frequency is already the downlink base, so the
    /// downlink is simply `base + number × spacing`. The IDEN_UP transmit
    /// offset is *not* part of it — that offset converts a downlink to the
    /// matching uplink (see [`SiteModel::channel_to_uplink`]), and adding it
    /// here produced frequencies in the 806–824 MHz mobile-transmit band
    /// instead of the 851–869 MHz base-transmit band.
    pub fn channel_to_freq(&self, channel: u16) -> Option<u64> {
        let iden = (channel >> 12) as u8;
        let number = (channel & 0x0FFF) as u64;
        let plan = self.idens.get(&iden)?;
        Some(plan.base_freq_hz + number * plan.spacing_hz)
    }

    /// The uplink (mobile transmit) frequency for a channel: the downlink
    /// shifted by the channel plan's signed transmit offset. Not what a
    /// scanner tunes, but the other half of the pair the plan describes.
    pub fn channel_to_uplink(&self, channel: u16) -> Option<u64> {
        let down = self.channel_to_freq(channel)? as i64;
        let iden = (channel >> 12) as u8;
        let plan = self.idens.get(&iden)?;
        let up = down + plan.tx_offset_hz;
        (up > 0).then_some(up as u64)
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

    /// A real 800 MHz plan: the downlink base sits in the 851–869 MHz
    /// base-transmit band and the transmit offset is −45 MHz, pointing at the
    /// 806–824 MHz mobile-transmit band.
    ///
    /// This case is the one that mattered. With the offset left at zero (as
    /// the test below had it) a grant resolved correctly by accident, because
    /// adding zero is harmless. Against a real control channel
    /// the same code produced 812.7625 MHz — a mobile transmit frequency no
    /// receiver should tune — where three independently-decoded voice channels
    /// said the answer was 857.7625 MHz, exactly 45 MHz up.
    #[test]
    fn downlink_excludes_the_transmit_offset() {
        let mut site = SiteModel::new();
        site.set_iden(
            1,
            IdenPlan {
                base_freq_hz: 851_006_250,
                spacing_hz: 6_250,
                tx_offset_hz: -45_000_000,
            },
        );
        let channel = (1u16 << 12) | 1080;
        let down = site.channel_to_freq(channel).unwrap();
        assert_eq!(
            down, 857_756_250,
            "downlink must ignore the transmit offset"
        );
        assert!(
            (851_000_000..=869_000_000).contains(&down),
            "downlink {down} is outside the base-transmit band"
        );
        // The offset is not wrong, just not part of the downlink: it names the
        // matching mobile transmit frequency.
        let up = site.channel_to_uplink(channel).unwrap();
        assert_eq!(up, down - 45_000_000);
        assert!(
            (806_000_000..=824_000_000).contains(&up),
            "uplink {up} is outside the mobile-transmit band"
        );
    }

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

/// Tracks dynamic talkgroup patches (Motorola Group Regroup).
///
/// Dispatch can merge talkgroups so they share audio. A scanner that ignores
/// this mis-attributes calls: traffic for a patched talkgroup shows up under
/// whichever member the grant names, so two talkgroups that a listener thinks
/// are separate are in fact one conversation.
#[derive(Debug, Default, Clone)]
pub struct PatchTracker {
    /// supergroup → member talkgroups, in first-seen order.
    patches: Vec<(u16, Vec<u16>)>,
}

impl PatchTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `talkgroup` belongs to `supergroup`.
    pub fn add(&mut self, supergroup: u16, talkgroup: u16) {
        match self.patches.iter_mut().find(|(s, _)| *s == supergroup) {
            Some((_, members)) => {
                if !members.contains(&talkgroup) {
                    members.push(talkgroup);
                }
            }
            None => self.patches.push((supergroup, vec![talkgroup])),
        }
    }

    /// The patch a talkgroup belongs to, if any.
    pub fn patch_of(&self, talkgroup: u16) -> Option<u16> {
        self.patches
            .iter()
            .find(|(_, m)| m.contains(&talkgroup))
            .map(|(s, _)| *s)
    }

    /// Talkgroups sharing a patch with this one — the other labels the same
    /// audio may appear under.
    pub fn siblings(&self, talkgroup: u16) -> Vec<u16> {
        self.patches
            .iter()
            .find(|(_, m)| m.contains(&talkgroup))
            .map(|(_, m)| m.iter().copied().filter(|&t| t != talkgroup).collect())
            .unwrap_or_default()
    }

    /// Every patch and its members.
    pub fn patches(&self) -> &[(u16, Vec<u16>)] {
        &self.patches
    }

    pub fn is_empty(&self) -> bool {
        self.patches.is_empty()
    }
}

#[cfg(test)]
mod patch_tests {
    use super::*;

    #[test]
    fn groups_talkgroups_under_a_patch() {
        let mut p = PatchTracker::new();
        p.add(957, 10204);
        p.add(957, 10203);
        p.add(949, 10118);
        assert_eq!(p.patch_of(10204), Some(957));
        assert_eq!(p.patch_of(10118), Some(949));
        assert_eq!(p.patch_of(99), None);
        assert_eq!(p.siblings(10204), vec![10203]);
        assert!(p.siblings(10118).is_empty(), "sole member has no siblings");
    }

    #[test]
    fn repeated_announcements_do_not_duplicate_members() {
        // Patch messages repeat continuously on a control channel; the same
        // pair arriving hundreds of times must not grow the member list.
        let mut p = PatchTracker::new();
        for _ in 0..100 {
            p.add(957, 10204);
        }
        assert_eq!(p.patches()[0].1, vec![10204]);
    }
}

/// What the control channel has said about where radios are.
///
/// Affiliation and registration messages are the system's own roster: which
/// radio joined which talkgroup, who registered and who left. A scanner that
/// keeps them can answer "who is on this talkgroup right now" without a
/// single word of voice. Entries are bounded because radios churn all day on
/// a busy site and the table would otherwise grow without limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MobilityEvent {
    /// A radio affiliated to a talkgroup (or was refused when `accepted` is
    /// false).
    Affiliated {
        unit: u32,
        group: u16,
        accepted: bool,
    },
    /// A radio registered on the system (`status` 0 = accepted).
    Registered { unit: u32, status: u8 },
    /// A radio registered its location for a talkgroup.
    Located { unit: u32, group: u16 },
    /// A radio de-registered (left the system).
    Deregistered { unit: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Affiliation {
    pub unit: u32,
    /// Talkgroup the radio most recently affiliated to, if known.
    pub group: Option<u16>,
    /// Whether the radio is known to be registered on this system.
    pub registered: bool,
    /// When it was last heard from, in the caller's clock (seconds).
    pub last_seen_secs: f64,
}

#[derive(Debug, Clone, Default)]
pub struct AffiliationTable {
    rows: Vec<Affiliation>,
}

/// Most radios remembered at once; the quietest is forgotten first.
pub const AFFILIATION_CAP: usize = 4096;

impl AffiliationTable {
    pub fn new() -> Self {
        Self::default()
    }

    fn row(&mut self, unit: u32, now: f64) -> &mut Affiliation {
        if let Some(i) = self.rows.iter().position(|r| r.unit == unit) {
            self.rows[i].last_seen_secs = now;
            return &mut self.rows[i];
        }
        if self.rows.len() >= AFFILIATION_CAP {
            let oldest = self
                .rows
                .iter()
                .enumerate()
                .min_by(|a, b| a.1.last_seen_secs.total_cmp(&b.1.last_seen_secs))
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.rows.swap_remove(oldest);
        }
        self.rows.push(Affiliation {
            unit,
            group: None,
            registered: false,
            last_seen_secs: now,
        });
        self.rows.last_mut().unwrap()
    }

    /// Apply one mobility message heard at `now` seconds.
    pub fn observe(&mut self, ev: MobilityEvent, now: f64) {
        match ev {
            MobilityEvent::Affiliated {
                unit,
                group,
                accepted,
            } => {
                let r = self.row(unit, now);
                if accepted {
                    r.group = Some(group);
                }
            }
            MobilityEvent::Registered { unit, status } => {
                let r = self.row(unit, now);
                r.registered = status == 0;
            }
            MobilityEvent::Located { unit, group } => {
                let r = self.row(unit, now);
                r.group = Some(group);
                r.registered = true;
            }
            MobilityEvent::Deregistered { unit } => {
                self.rows.retain(|r| r.unit != unit);
            }
        }
    }

    /// Every radio heard from, most recent first.
    pub fn rows(&self) -> Vec<Affiliation> {
        let mut v = self.rows.clone();
        v.sort_by(|a, b| b.last_seen_secs.total_cmp(&a.last_seen_secs));
        v
    }

    /// Radios affiliated to a talkgroup.
    pub fn members(&self, group: u16) -> Vec<u32> {
        self.rows
            .iter()
            .filter(|r| r.group == Some(group))
            .map(|r| r.unit)
            .collect()
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

#[cfg(test)]
mod affiliation_tests {
    use super::*;

    #[test]
    fn tracks_joins_registrations_and_departures() {
        let mut t = AffiliationTable::new();
        t.observe(
            MobilityEvent::Registered {
                unit: 790065,
                status: 0,
            },
            1.0,
        );
        t.observe(
            MobilityEvent::Affiliated {
                unit: 790065,
                group: 20308,
                accepted: true,
            },
            2.0,
        );
        // A refused affiliation is heard but does not move the radio.
        t.observe(
            MobilityEvent::Affiliated {
                unit: 790065,
                group: 1,
                accepted: false,
            },
            3.0,
        );
        assert_eq!(t.members(20308), vec![790065]);
        let r = t.rows()[0];
        assert!(r.registered && r.group == Some(20308) && r.last_seen_secs == 3.0);
        t.observe(MobilityEvent::Deregistered { unit: 790065 }, 4.0);
        assert!(t.is_empty());
    }

    #[test]
    fn the_table_is_bounded_and_forgets_the_quietest() {
        let mut t = AffiliationTable::new();
        for u in 0..(AFFILIATION_CAP as u32 + 10) {
            t.observe(MobilityEvent::Registered { unit: u, status: 0 }, u as f64);
        }
        assert_eq!(t.len(), AFFILIATION_CAP);
        assert!(t.members(0).is_empty());
        // The ten oldest are gone; the newest remain.
        assert!(t.rows().iter().all(|r| r.unit >= 10));
    }
}
