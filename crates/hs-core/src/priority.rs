//! Talkgroup priority ranking for the dual-SDR scheduler.
//!
//! SDRTrunk semantics: **1 = highest priority, 99 = lowest**. When the
//! hopping voice radio can only cover one call at a time, a call on a
//! higher-priority (lower-numbered) talkgroup preempts a lower-priority one.
//!
//! Two layers: a *base* priority seeded from the catalog (the RadioReference
//! CSV `Priority` column) and a per-user *override* that wins over it.
//! Talkgroups with neither fall back to [`DEFAULT_PRIORITY`].

use std::collections::HashMap;

/// Priority assigned to a talkgroup with no explicit value (the lowest).
pub const DEFAULT_PRIORITY: u8 = 99;

/// The highest possible priority.
pub const HIGHEST_PRIORITY: u8 = 1;

/// The lowest possible priority (same as the default).
pub const LOWEST_PRIORITY: u8 = 99;

/// Talkgroup → SDRTrunk-style priority (1 = highest, 99 = lowest).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PriorityMap {
    /// Per-user assignments; these win over the catalog base.
    overrides: HashMap<u16, u8>,
    /// Priorities read from the catalog (RR `Priority` column).
    base: HashMap<u16, u8>,
}

impl PriorityMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a catalog-sourced priority. User overrides still win.
    pub fn set_base(&mut self, talkgroup: u16, priority: u8) {
        self.base.insert(talkgroup, priority);
    }

    /// Record a per-user priority, winning over any catalog value.
    pub fn set_override(&mut self, talkgroup: u16, priority: u8) {
        self.overrides.insert(talkgroup, priority);
    }

    /// Clear a per-user override, falling back to the catalog value.
    pub fn clear_override(&mut self, talkgroup: u16) {
        self.overrides.remove(&talkgroup);
    }

    /// The effective priority for a talkgroup: override, else catalog base,
    /// else [`DEFAULT_PRIORITY`].
    pub fn lookup(&self, talkgroup: u16) -> u8 {
        self.overrides
            .get(&talkgroup)
            .copied()
            .or_else(|| self.base.get(&talkgroup).copied())
            .unwrap_or(DEFAULT_PRIORITY)
    }

    /// True if priority `a` outranks `b` (lower number wins).
    pub fn beats(a: u8, b: u8) -> bool {
        a < b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_lowest() {
        let map = PriorityMap::new();
        assert_eq!(map.lookup(1234), DEFAULT_PRIORITY);
        assert_eq!(map.lookup(0), DEFAULT_PRIORITY);
    }

    #[test]
    fn override_wins_over_base() {
        let mut map = PriorityMap::new();
        map.set_base(1234, 40);
        assert_eq!(map.lookup(1234), 40);
        map.set_override(1234, 10);
        assert_eq!(map.lookup(1234), 10);
        map.clear_override(1234);
        assert_eq!(map.lookup(1234), 40);
    }

    #[test]
    fn lower_number_outranks() {
        assert!(PriorityMap::beats(1, 99));
        assert!(PriorityMap::beats(10, 20));
        assert!(!PriorityMap::beats(99, 1));
        assert!(
            !PriorityMap::beats(50, 50),
            "equal priority does not preempt"
        );
    }
}
