//! Scheduler for a single "hopping" voice radio.
//!
//! In dual-SDR operation one radio locks the control channel and decodes
//! grants; a second, narrow radio hops between voice channels, covering one
//! call at a time. This is the pure decision engine for that second radio:
//! given grants (talkgroup → frequency) and their priorities, it decides
//! which channel to tune and when a higher-priority call preempts the one
//! being followed. No DSP or hardware — feed it events, read [`HopAction`]s,
//! and let the front end execute the tunes.

use std::collections::HashMap;

use crate::priority::{PriorityMap, DEFAULT_PRIORITY};

/// How long a call must run before a higher-priority grant may preempt it.
/// Two calls flipping rapidly would otherwise thrash the radio between them.
const DEFAULT_GRACE_SECS: f64 = 2.0;

/// How long a grant is remembered without being re-announced before it is
/// considered over. Covers skipped calls whose release we never decoded
/// (we were not listening), since the control channel re-announces grants
/// periodically.
const DEFAULT_HOLD_SECS: f64 = 6.0;

/// A grant currently believed open on the system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenCall {
    pub talkgroup: u16,
    pub freq_hz: u64,
}

/// What the scheduler wants the radio to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HopAction {
    /// Retune the radio to this frequency and decode this call.
    Tune { freq_hz: u64, talkgroup: u16 },
    /// Nothing worth following; park (typically back on the control channel).
    Park,
    /// Keep decoding the current call (a lower/equal-priority grant arrived,
    /// or a call we were not following ended).
    Stay,
}

/// An open grant and its rank, tracked internally.
struct Active {
    talkgroup: u16,
    priority: u8,
    last_seen: f64,
}

pub struct HoppingScheduler {
    priority: PriorityMap,
    grace_secs: f64,
    hold_secs: f64,
    /// Grants believed open, keyed by frequency.
    active: HashMap<u64, Active>,
    /// The call the radio is decoding now, and when it tuned there.
    current: Option<(u64, f64)>,
}

impl HoppingScheduler {
    pub fn new(priority: PriorityMap) -> Self {
        Self::with_timing(priority, DEFAULT_GRACE_SECS, DEFAULT_HOLD_SECS)
    }

    pub fn with_timing(priority: PriorityMap, grace_secs: f64, hold_secs: f64) -> Self {
        Self {
            priority,
            grace_secs,
            hold_secs,
            active: HashMap::new(),
            current: None,
        }
    }

    /// The call the radio is currently following, if any.
    pub fn current(&self) -> Option<OpenCall> {
        self.current.map(|(freq, _)| OpenCall {
            freq_hz: freq,
            talkgroup: self.active.get(&freq).map(|a| a.talkgroup).unwrap_or(0),
        })
    }

    /// A grant was decoded on the control channel. `now` is a monotonic
    /// clock in seconds (any origin) shared across all calls.
    pub fn on_grant(&mut self, talkgroup: u16, freq_hz: u64, now: f64) -> HopAction {
        let priority = self.priority.lookup(talkgroup);
        self.active.insert(
            freq_hz,
            Active {
                talkgroup,
                priority,
                last_seen: now,
            },
        );

        match self.current {
            None => {
                self.current = Some((freq_hz, now));
                HopAction::Tune { freq_hz, talkgroup }
            }
            Some((cur_freq, tuned_at)) => {
                let cur_priority = self
                    .active
                    .get(&cur_freq)
                    .map(|a| a.priority)
                    .unwrap_or(DEFAULT_PRIORITY);
                if PriorityMap::beats(priority, cur_priority) && now - tuned_at >= self.grace_secs {
                    self.current = Some((freq_hz, now));
                    HopAction::Tune { freq_hz, talkgroup }
                } else {
                    HopAction::Stay
                }
            }
        }
    }

    /// A call ended (control-channel release, or the radio heard its
    /// terminator). If it was the call being followed, promote the next
    /// highest-priority open call (or park).
    pub fn on_end(&mut self, freq_hz: u64, now: f64) -> HopAction {
        self.active.remove(&freq_hz);
        if self.current.map(|(f, _)| f) != Some(freq_hz) {
            // A call we were not following ended; nothing to retune.
            return HopAction::Stay;
        }
        self.promote(now)
    }

    /// Drop skipped grants not re-announced within the hold window. The call
    /// being followed is never pruned — its end comes from [`Self::on_end`]
    /// (a terminator or the radio's quiet timeout), not the re-announcement
    /// timer. Returns how many grants were forgotten.
    pub fn prune(&mut self, now: f64) -> usize {
        let cur = self.current.map(|(f, _)| f);
        let before = self.active.len();
        self.active
            .retain(|freq, a| Some(*freq) == cur || now - a.last_seen < self.hold_secs);
        before - self.active.len()
    }

    /// Tune to the highest-priority open call, or park if none are open.
    fn promote(&mut self, now: f64) -> HopAction {
        let best = self
            .active
            .iter()
            .min_by(|(fa, a), (fb, b)| a.priority.cmp(&b.priority).then(fa.cmp(fb)));
        match best {
            Some((&freq, a)) => {
                self.current = Some((freq, now));
                HopAction::Tune {
                    freq_hz: freq,
                    talkgroup: a.talkgroup,
                }
            }
            None => {
                self.current = None;
                HopAction::Park
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(overrides: &[(u16, u8)]) -> PriorityMap {
        let mut m = PriorityMap::new();
        for (tg, p) in overrides {
            m.set_base(*tg, *p);
        }
        m
    }

    #[test]
    fn first_grant_tunes_and_idle_park() {
        let mut s = HoppingScheduler::new(map(&[(101, 50)]));
        assert_eq!(
            s.on_grant(101, 850_000_000, 0.0),
            HopAction::Tune {
                freq_hz: 850_000_000,
                talkgroup: 101
            }
        );
        assert_eq!(
            s.on_end(850_000_000, 5.0),
            HopAction::Park,
            "no open calls left → park"
        );
        assert!(s.current().is_none());
    }

    #[test]
    fn lower_priority_grant_does_not_preempt() {
        let mut s = HoppingScheduler::new(map(&[(10, 10), (20, 90)]));
        s.on_grant(10, 850_000_000, 0.0);
        // A lower-priority (90) grant while on priority-10: stay.
        assert_eq!(s.on_grant(20, 851_000_000, 1.0), HopAction::Stay);
        assert_eq!(s.current().unwrap().freq_hz, 850_000_000);
    }

    #[test]
    fn higher_priority_preempts_after_grace() {
        let mut s = HoppingScheduler::new(map(&[(10, 90), (20, 10)]));
        s.on_grant(10, 850_000_000, 0.0);
        // Inside the grace window: no preempt.
        assert_eq!(s.on_grant(20, 851_000_000, 1.0), HopAction::Stay);
        // After the grace window: preempt to the higher-priority call.
        assert_eq!(
            s.on_grant(20, 851_000_000, 3.0),
            HopAction::Tune {
                freq_hz: 851_000_000,
                talkgroup: 20
            }
        );
        assert_eq!(s.current().unwrap().freq_hz, 851_000_000);
    }

    #[test]
    fn end_of_current_promotes_next_highest() {
        let mut s = HoppingScheduler::new(map(&[(10, 80), (20, 10), (30, 50)]));
        s.on_grant(20, 851_000_000, 0.0); // priority 10, wins
                                          // Two lower calls also open while we follow #20.
        assert_eq!(s.on_grant(30, 852_000_000, 1.0), HopAction::Stay);
        assert_eq!(s.on_grant(10, 850_000_000, 1.0), HopAction::Stay);
        // #20 ends → promote the best remaining: #30 (priority 50) over #10 (80).
        assert_eq!(
            s.on_end(851_000_000, 5.0),
            HopAction::Tune {
                freq_hz: 852_000_000,
                talkgroup: 30
            }
        );
    }

    #[test]
    fn end_of_noncurrent_call_is_noop() {
        let mut s = HoppingScheduler::new(map(&[(10, 10), (20, 90)]));
        s.on_grant(10, 850_000_000, 0.0);
        s.on_grant(20, 851_000_000, 1.0); // skipped (lower priority)
        assert_eq!(s.on_end(851_000_000, 2.0), HopAction::Stay);
        assert_eq!(s.current().unwrap().freq_hz, 850_000_000);
    }

    #[test]
    fn prune_forgets_stale_skipped_but_not_current() {
        let mut s = HoppingScheduler::with_timing(map(&[(10, 10), (20, 90)]), 2.0, 6.0);
        s.on_grant(10, 850_000_000, 0.0); // current
        s.on_grant(20, 851_000_000, 1.0); // skipped
        let pruned = s.prune(10.0);
        assert_eq!(pruned, 1, "the skipped grant lapsed");
        assert_eq!(s.active.len(), 1);
        assert_eq!(s.current().unwrap().freq_hz, 850_000_000);
    }

    #[test]
    fn equal_priority_does_not_preempt() {
        let mut s = HoppingScheduler::new(map(&[(10, 50), (20, 50)]));
        s.on_grant(10, 850_000_000, 0.0);
        assert_eq!(s.on_grant(20, 851_000_000, 5.0), HopAction::Stay);
    }
}
