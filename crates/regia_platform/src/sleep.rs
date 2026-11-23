// SPDX-License-Identifier: MPL-2.0
//! System sleep detection for desktop Linux handhelds (Steam Deck, batch v020 lane `steam-deck`).
//!
//! winit raises `suspended` only on Android and iOS: when SteamOS (or any Linux desktop) puts the
//! machine to sleep, the process is frozen and nothing tells it. After the wake the Vulkan surface
//! may be outdated or lost, the audio device may have gone, and the frame clock has a gap of
//! however long the machine slept. Linux has a cheap tell: `CLOCK_MONOTONIC` stops during system
//! sleep, `CLOCK_BOOTTIME` keeps counting. If the two drift apart between two polls, the machine
//! slept for the difference. [`SleepDetector`] watches for that and the runner turns it into a
//! synthetic `Suspended` → `Resumed` pair, the same path Android's lifecycle takes (surface
//! dropped and recreated, audio output suspended and restarted, gameplay time not advanced).

use std::time::Duration;

/// A sleep shorter than this is not reported (clock read jitter, a stopped debugger).
pub const MIN_REPORTED_SLEEP: Duration = Duration::from_secs(2);

/// A pair of clock readings: time awake (monotonic) and time since boot including sleep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockReading {
    /// `CLOCK_MONOTONIC`: does not advance while the system sleeps.
    pub awake: Duration,
    /// `CLOCK_BOOTTIME`: advances while the system sleeps.
    pub boot: Duration,
}

/// Compares successive [`ClockReading`]s and reports system sleeps.
#[derive(Debug, Clone, Default)]
pub struct SleepDetector {
    last: Option<ClockReading>,
    /// Total time slept since the detector started.
    pub total_slept: Duration,
    /// Number of sleeps reported.
    pub sleeps: u32,
}

impl SleepDetector {
    /// Feeds one reading. Returns how long the machine slept since the previous reading when that
    /// is at least [`MIN_REPORTED_SLEEP`].
    pub fn observe(&mut self, now: ClockReading) -> Option<Duration> {
        let slept = self.last.map(|last| {
            let awake = now.awake.saturating_sub(last.awake);
            let boot = now.boot.saturating_sub(last.boot);
            boot.saturating_sub(awake)
        });
        self.last = Some(now);
        slept.filter(|s| *s >= MIN_REPORTED_SLEEP).inspect(|s| {
            self.total_slept += *s;
            self.sleeps += 1;
        })
    }

    /// Reads the system clocks and feeds them; `None` on platforms without the two clocks.
    pub fn poll(&mut self) -> Option<Duration> {
        read_clocks().and_then(|r| self.observe(r))
    }
}

/// The lifecycle events a wake-up after `slept` turns into: a suspend and a resume back to back,
/// through the same [`crate::LifecycleTracker`] the OS callbacks use (so surfaces drop and come back,
/// audio output restarts, gameplay time does not advance). Nothing if no sleep was seen or the app
/// is not running.
pub fn wake_events(
    tracker: &mut crate::LifecycleTracker,
    slept: Option<Duration>,
) -> Vec<crate::LifecycleEvent> {
    if slept.is_none() || tracker.state() != crate::LifecycleState::Running {
        return Vec::new();
    }
    let mut events = tracker.suspended();
    events.extend(tracker.resumed());
    events
}

#[cfg(target_os = "linux")]
fn read_clocks() -> Option<ClockReading> {
    fn clock(id: libc::clockid_t) -> Option<Duration> {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a valid, writable timespec for the duration of the call.
        let rc = unsafe { libc::clock_gettime(id, &mut ts) };
        if rc != 0 {
            return None;
        }
        Some(Duration::new(
            u64::try_from(ts.tv_sec).ok()?,
            u32::try_from(ts.tv_nsec).ok()?,
        ))
    }
    Some(ClockReading {
        awake: clock(libc::CLOCK_MONOTONIC)?,
        boot: clock(libc::CLOCK_BOOTTIME)?,
    })
}

#[cfg(not(target_os = "linux"))]
fn read_clocks() -> Option<ClockReading> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(awake: u64, boot: u64) -> ClockReading {
        ClockReading {
            awake: Duration::from_secs(awake),
            boot: Duration::from_secs(boot),
        }
    }

    #[test]
    fn steady_clocks_report_nothing() {
        let mut d = SleepDetector::default();
        assert_eq!(d.observe(r(10, 100)), None);
        assert_eq!(d.observe(r(11, 101)), None);
        assert_eq!(d.sleeps, 0);
    }

    #[test]
    fn boottime_ahead_of_monotonic_is_a_sleep() {
        let mut d = SleepDetector::default();
        d.observe(r(10, 100));
        // Awake advanced 1 s, boot advanced 1801 s: the machine slept 30 minutes.
        assert_eq!(d.observe(r(11, 1901)), Some(Duration::from_secs(1800)));
        assert_eq!((d.sleeps, d.total_slept), (1, Duration::from_secs(1800)));
        // The next poll compares against the new baseline.
        assert_eq!(d.observe(r(12, 1902)), None);
    }

    #[test]
    fn a_wake_is_a_suspend_resume_pair_and_only_while_running() {
        use crate::{LifecycleEvent::*, LifecycleTracker};
        let mut t = LifecycleTracker::default();
        let slept = Some(Duration::from_secs(600));
        assert_eq!(
            wake_events(&mut t, slept),
            [],
            "before the first resume nothing happens"
        );
        t.resumed();
        assert_eq!(wake_events(&mut t, None), []);
        assert_eq!(wake_events(&mut t, slept), [Suspended, Resumed]);
        assert_eq!(t.state(), crate::LifecycleState::Running);
        t.exiting();
        assert_eq!(wake_events(&mut t, slept), [], "not while terminating");
    }

    #[test]
    fn the_pair_pauses_gameplay_time_and_survives_repeats() {
        use regia_app::prelude::*;
        let mut app = App::new();
        app.init_resource::<crate::AppLifecycle>();
        let mut tracker = crate::LifecycleTracker::default();
        for event in tracker.resumed() {
            crate::apply_transition(&mut app, event);
        }
        for round in 1..=3u64 {
            let events = wake_events(&mut tracker, Some(Duration::from_secs(30)));
            for (i, event) in events.into_iter().enumerate() {
                crate::apply_transition(&mut app, event);
                let suspended = app.world().resource::<Time>().is_suspended();
                assert_eq!(
                    suspended,
                    i == 0,
                    "suspended between the pair only (round {round})"
                );
            }
            assert_eq!(
                app.world().resource::<crate::AppLifecycle>().resume_count,
                round + 1
            );
        }
    }

    #[test]
    fn short_gaps_are_ignored() {
        let mut d = SleepDetector::default();
        d.observe(r(0, 0));
        assert_eq!(d.observe(r(5, 6)), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn live_clocks_do_not_report_a_sleep_between_back_to_back_polls() {
        let mut d = SleepDetector::default();
        assert_eq!(d.poll(), None);
        assert_eq!(d.poll(), None);
    }
}
