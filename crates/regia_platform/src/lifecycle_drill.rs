// SPDX-License-Identifier: MPL-2.0
//! Suspend/resume drills and the surface audit (ADR-527).
//!
//! Android takes the window away on every home press or screen-off: the renderer must drop every
//! surface before the callback returns and recreate one per window on resume, with the device
//! kept (`docs/specs/11-platform-and-input.md` § 2). That contract used to be checked only on a
//! phone (`cargo xtask android-lifecycle`). Two pieces make it checkable anywhere:
//!
//! - [`LifecycleDrill`] queues synthetic suspend → resume cycles. The windowed runner runs one
//!   queued cycle per frame through the same [`LifecycleTracker`] the OS callbacks use (the
//!   path a Steam Deck wake already takes, `crate::sleep`), so on a desktop the real surfaces are
//!   dropped and recreated. Agents queue cycles with `lifecycle.drill`.
//! - [`SurfaceAudit`] receives one [`SurfaceObservation`] per lifecycle transition from the
//!   renderer and records every broken rule: a surface alive while suspended, a window without a
//!   surface after resume, or created − dropped ≠ live. It logs one `regia surface audit:` line
//!   per resume, which the device pass reads from logcat.

use regia_app::prelude::*;

use crate::lifecycle::{LifecycleEvent, LifecycleState, LifecycleTracker, apply_transition};

/// Most cycles a [`LifecycleDrill`] holds queued at once.
pub const MAX_DRILL_CYCLES: u32 = 1000;

/// Most violations a [`SurfaceAudit`] keeps (the count keeps going past it).
pub const MAX_KEPT_VIOLATIONS: usize = 32;

/// Queued synthetic suspend → resume cycles. Present by default (`PlatformPlugin`).
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct LifecycleDrill {
    pending: u32,
    completed: u64,
}

impl LifecycleDrill {
    /// Queues up to `cycles` more cycles (the queue is capped at [`MAX_DRILL_CYCLES`]); returns
    /// how many were queued.
    pub fn request(&mut self, cycles: u32) -> u32 {
        let queued = cycles.min(MAX_DRILL_CYCLES - self.pending);
        self.pending += queued;
        queued
    }

    /// Cycles still queued.
    #[must_use]
    pub fn pending(&self) -> u32 {
        self.pending
    }

    /// Cycles run since start.
    #[must_use]
    pub fn completed(&self) -> u64 {
        self.completed
    }

    /// Drops every queued cycle; returns how many were dropped.
    pub fn cancel(&mut self) -> u32 {
        std::mem::take(&mut self.pending)
    }
}

/// The events of the next queued drill cycle: `Suspended` then `Resumed`, from `tracker`, and
/// the cycle counted as run. Nothing when no cycle is queued or the app is not running (before
/// the first resume, while the OS has it suspended, or while exiting).
pub fn next_drill_events(tracker: &mut LifecycleTracker, drill: &mut LifecycleDrill) -> Vec<LifecycleEvent> {
    if drill.pending == 0 || tracker.state() != LifecycleState::Running {
        return Vec::new();
    }
    drill.pending -= 1;
    drill.completed += 1;
    let mut events = tracker.suspended();
    events.extend(tracker.resumed());
    events
}

/// Runs every queued drill cycle on `app` through `tracker`; returns how many ran. The runner
/// runs one per frame instead; this is the headless harness tests use.
pub fn run_queued_drill(app: &mut App, tracker: &mut LifecycleTracker) -> u32 {
    let mut ran = 0;
    loop {
        let events = match app.world_mut().get_resource_mut::<LifecycleDrill>() {
            Some(mut drill) => next_drill_events(tracker, &mut drill),
            None => Vec::new(),
        };
        if events.is_empty() {
            return ran;
        }
        let _span = tracing::info_span!("lifecycle_drill_cycle").entered();
        for event in events {
            apply_transition(app, event);
        }
        ran += 1;
    }
}

/// What the renderer reports after it handled one lifecycle transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SurfaceObservation {
    /// Surfaces alive after the transition.
    pub live: usize,
    /// OS windows that exist (each should have a surface while running).
    pub windows: usize,
    /// Surfaces created since start.
    pub created_total: u64,
    /// Surfaces dropped since start.
    pub dropped_total: u64,
}

/// Which rule a transition broke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceViolationKind {
    /// A surface was still alive after `Suspended` or `WillTerminate`.
    KeptWhileSuspended,
    /// After `Resumed` fewer surfaces were alive than windows exist.
    MissingAfterResume,
    /// Surfaces created − dropped does not match the surfaces alive (a leak or a double drop).
    CountMismatch,
}

impl SurfaceViolationKind {
    /// Stable lowercase code (agent protocol, logcat).
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::KeptWhileSuspended => "surface_kept_while_suspended",
            Self::MissingAfterResume => "surface_missing_after_resume",
            Self::CountMismatch => "surface_count_mismatch",
        }
    }
}

/// One broken rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceViolation {
    /// The rule.
    pub kind: SurfaceViolationKind,
    /// The transition it was seen after.
    pub event: LifecycleEvent,
    /// Resumes seen before it (0 = during the first start).
    pub resumes: u64,
    /// What the renderer reported.
    pub observed: SurfaceObservation,
}

/// Checks the surface contract on every lifecycle transition. Present by default
/// (`PlatformPlugin`); the renderer feeds it.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct SurfaceAudit {
    suspends: u64,
    resumes: u64,
    last: SurfaceObservation,
    violation_count: u64,
    violations: Vec<SurfaceViolation>,
}

impl SurfaceAudit {
    /// Records the renderer's report for `event` and checks it.
    pub fn observe(&mut self, event: LifecycleEvent, observed: SurfaceObservation) {
        let mut broken = Vec::new();
        match event {
            LifecycleEvent::Suspended | LifecycleEvent::WillTerminate => {
                if event == LifecycleEvent::Suspended {
                    self.suspends += 1;
                }
                if observed.live != 0 {
                    broken.push(SurfaceViolationKind::KeptWhileSuspended);
                }
            }
            LifecycleEvent::Resumed => {
                self.resumes += 1;
                if observed.live < observed.windows {
                    broken.push(SurfaceViolationKind::MissingAfterResume);
                }
            }
            LifecycleEvent::Started | LifecycleEvent::LowMemory => {}
        }
        if observed.created_total.checked_sub(observed.dropped_total) != Some(observed.live as u64) {
            broken.push(SurfaceViolationKind::CountMismatch);
        }
        for kind in broken {
            self.violation_count += 1;
            if self.violations.len() < MAX_KEPT_VIOLATIONS {
                self.violations.push(SurfaceViolation {
                    kind,
                    event,
                    resumes: self.resumes,
                    observed,
                });
            }
        }
        self.last = observed;
    }

    /// `Suspended` transitions seen.
    #[must_use]
    pub fn suspends(&self) -> u64 {
        self.suspends
    }

    /// `Resumed` transitions seen.
    #[must_use]
    pub fn resumes(&self) -> u64 {
        self.resumes
    }

    /// The latest report.
    #[must_use]
    pub fn last(&self) -> SurfaceObservation {
        self.last
    }

    /// Every broken rule so far (including those past [`MAX_KEPT_VIOLATIONS`]).
    #[must_use]
    pub fn violation_count(&self) -> u64 {
        self.violation_count
    }

    /// The first [`MAX_KEPT_VIOLATIONS`] broken rules.
    #[must_use]
    pub fn violations(&self) -> &[SurfaceViolation] {
        &self.violations
    }

    /// No rule broken so far.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.violation_count == 0
    }

    /// The `regia surface audit:` marker line (logged by the renderer on each resume and read
    /// by `cargo xtask android-lifecycle`).
    #[must_use]
    pub fn summary_line(&self) -> String {
        format!(
            "regia surface audit: suspends={} resumes={} created={} dropped={} live={} windows={} violations={}",
            self.suspends,
            self.resumes,
            self.last.created_total,
            self.last.dropped_total,
            self.last.live,
            self.last.windows,
            self.violation_count
        )
    }
}

/// Feeds `observed` for the transition the [`Lifecycle`] schedule is running, when the
/// world has a [`SurfaceAudit`]. Renderers call it at the end of their lifecycle handler; on
/// `Resumed` it logs [`SurfaceAudit::summary_line`].
pub fn record_surface_observation(world: &mut World, observed: SurfaceObservation) {
    let Some(event) = world
        .get_resource::<crate::AppLifecycle>()
        .and_then(|l| l.last_event)
    else {
        return;
    };
    let Some(mut audit) = world.get_resource_mut::<SurfaceAudit>() else {
        return;
    };
    let before = audit.violation_count();
    audit.observe(event, observed);
    if audit.violation_count() > before {
        for v in &audit.violations()[audit.violations().len().min(before as usize)..] {
            tracing::warn!(
                target: "regia_platform",
                "surface audit: {} after {:?} (live {}, windows {}, created {}, dropped {})",
                v.kind.code(),
                v.event,
                v.observed.live,
                v.observed.windows,
                v.observed.created_total,
                v.observed.dropped_total
            );
        }
    }
    if event == LifecycleEvent::Resumed {
        tracing::info!(target: "regia_platform", "{}", audit.summary_line());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use LifecycleEvent::*;

    fn obs(live: usize, windows: usize, created: u64, dropped: u64) -> SurfaceObservation {
        SurfaceObservation {
            live,
            windows,
            created_total: created,
            dropped_total: dropped,
        }
    }

    #[test]
    fn drill_requests_cap_and_cancel() {
        let mut drill = LifecycleDrill::default();
        assert_eq!(drill.request(3), 3);
        assert_eq!(drill.request(MAX_DRILL_CYCLES), MAX_DRILL_CYCLES - 3);
        assert_eq!(drill.pending(), MAX_DRILL_CYCLES);
        assert_eq!(drill.request(1), 0, "a full queue takes nothing");
        assert_eq!(drill.cancel(), MAX_DRILL_CYCLES);
        assert_eq!(drill.pending(), 0);
    }

    #[test]
    fn drill_events_only_while_running() {
        let mut tracker = LifecycleTracker::default();
        let mut drill = LifecycleDrill::default();
        drill.request(2);
        assert!(
            next_drill_events(&mut tracker, &mut drill).is_empty(),
            "not started yet"
        );
        tracker.resumed();
        assert_eq!(next_drill_events(&mut tracker, &mut drill), [Suspended, Resumed]);
        tracker.suspended();
        assert!(
            next_drill_events(&mut tracker, &mut drill).is_empty(),
            "OS suspended"
        );
        tracker.resumed();
        assert_eq!(next_drill_events(&mut tracker, &mut drill), [Suspended, Resumed]);
        assert!(
            next_drill_events(&mut tracker, &mut drill).is_empty(),
            "queue empty"
        );
        assert_eq!((drill.pending(), drill.completed()), (0, 2));
        assert_eq!(tracker.state(), LifecycleState::Running);
    }

    #[test]
    fn audit_accepts_a_clean_cycle() {
        let mut audit = SurfaceAudit::default();
        audit.observe(Started, obs(0, 1, 0, 0));
        audit.observe(Resumed, obs(1, 1, 1, 0));
        audit.observe(Suspended, obs(0, 1, 1, 1));
        audit.observe(Resumed, obs(1, 1, 2, 1));
        audit.observe(LowMemory, obs(1, 1, 2, 1));
        assert!(audit.is_clean(), "{:?}", audit.violations());
        assert_eq!((audit.suspends(), audit.resumes()), (1, 2));
        assert_eq!(
            audit.summary_line(),
            "regia surface audit: suspends=1 resumes=2 created=2 dropped=1 live=1 windows=1 violations=0"
        );
    }

    #[test]
    fn audit_flags_each_broken_rule() {
        let mut audit = SurfaceAudit::default();
        audit.observe(Resumed, obs(1, 1, 1, 0));
        audit.observe(Suspended, obs(1, 1, 1, 0));
        audit.observe(Resumed, obs(0, 1, 1, 1));
        audit.observe(WillTerminate, obs(0, 1, 3, 1));
        let kinds: Vec<_> = audit.violations().iter().map(|v| (v.kind, v.event)).collect();
        assert_eq!(
            kinds,
            [
                (SurfaceViolationKind::KeptWhileSuspended, Suspended),
                (SurfaceViolationKind::MissingAfterResume, Resumed),
                (SurfaceViolationKind::CountMismatch, WillTerminate),
            ]
        );
        assert_eq!(audit.violations()[1].resumes, 2);
        assert_eq!(
            SurfaceViolationKind::CountMismatch.code(),
            "surface_count_mismatch"
        );
    }

    #[test]
    fn audit_keeps_a_bounded_list_and_counts_the_rest() {
        let mut audit = SurfaceAudit::default();
        for _ in 0..(MAX_KEPT_VIOLATIONS + 8) {
            audit.observe(Suspended, obs(1, 1, 1, 0));
        }
        assert_eq!(audit.violations().len(), MAX_KEPT_VIOLATIONS);
        assert_eq!(audit.violation_count(), (MAX_KEPT_VIOLATIONS + 8) as u64);
    }

    /// A stand-in renderer: one surface per window, dropped on suspend, recreated on resume.
    #[derive(Resource, Default)]
    struct FakeSurfaces {
        windows: usize,
        live: usize,
        created: u64,
        dropped: u64,
        /// Leak the surface on this resume (1-based), to prove the audit sees it.
        keep_on_suspend: Option<u64>,
    }

    fn fake_renderer(world: &mut World) {
        let Some(event) = world.resource::<crate::AppLifecycle>().last_event else {
            return;
        };
        let resumes = world.resource::<SurfaceAudit>().resumes();
        let mut s = world.resource_mut::<FakeSurfaces>();
        match event {
            Suspended | WillTerminate if s.keep_on_suspend != Some(resumes) => {
                s.dropped += s.live as u64;
                s.live = 0;
            }
            Resumed => {
                let missing = s.windows - s.live;
                s.created += missing as u64;
                s.live = s.windows;
            }
            _ => {}
        }
        let observed = obs(s.live, s.windows, s.created, s.dropped);
        record_surface_observation(world, observed);
    }

    fn drill_app(keep_on_suspend: Option<u64>) -> (App, LifecycleTracker) {
        let mut app = App::new();
        app.init_resource::<crate::AppLifecycle>()
            .init_resource::<LifecycleDrill>()
            .init_resource::<SurfaceAudit>()
            .insert_resource(FakeSurfaces {
                windows: 2,
                keep_on_suspend,
                ..Default::default()
            })
            .add_message::<LifecycleEvent>()
            .add_systems(Lifecycle, fake_renderer);
        let mut tracker = LifecycleTracker::default();
        for event in tracker.resumed() {
            apply_transition(&mut app, event);
        }
        (app, tracker)
    }

    #[test]
    fn headless_drill_runs_every_cycle_through_the_lifecycle_schedule() {
        let (mut app, mut tracker) = drill_app(None);
        app.world_mut().resource_mut::<LifecycleDrill>().request(50);
        assert_eq!(run_queued_drill(&mut app, &mut tracker), 50);
        let world = app.world();
        let audit = world.resource::<SurfaceAudit>();
        assert!(audit.is_clean(), "{:?}", audit.violations());
        assert_eq!((audit.suspends(), audit.resumes()), (50, 51));
        assert_eq!(audit.last(), obs(2, 2, 102, 100));
        assert_eq!(world.resource::<crate::AppLifecycle>().resume_count, 51);
        assert_eq!(world.resource::<LifecycleDrill>().completed(), 50);
        assert!(!world.resource::<Time>().is_suspended(), "the drill ends resumed");
        assert_eq!(tracker.state(), LifecycleState::Running);
    }

    #[test]
    fn headless_drill_reports_a_surface_kept_through_a_suspend() {
        let (mut app, mut tracker) = drill_app(Some(3));
        app.world_mut().resource_mut::<LifecycleDrill>().request(5);
        run_queued_drill(&mut app, &mut tracker);
        let audit = app.world().resource::<SurfaceAudit>();
        let kinds: Vec<_> = audit.violations().iter().map(|v| (v.kind, v.resumes)).collect();
        assert_eq!(kinds, [(SurfaceViolationKind::KeptWhileSuspended, 3)]);
    }
}
