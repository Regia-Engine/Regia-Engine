// SPDX-License-Identifier: MPL-2.0
//! Which `bevy_ecs` executor the app's schedules run on (ADR-4933).
//!
//! The multi-threaded executor runs every `Send` system as a task on the compute pool and the
//! main thread waits for the schedule to end. Each system that finishes wakes the executor task,
//! which wakes the workers for the systems it made ready. On a desktop CPU a wake costs a few
//! microseconds. On a phone the pool threads sleep on cores in deep idle states, and many of
//! those are the slow in-order cores, so a frame of a few hundred short systems spends more time
//! handing work between threads than running it (the arena on a Pixel 9 Pro: about 22 ms of
//! systems in a 45 ms update, the main thread on a CPU a third of the frame).
//!
//! [`ScheduleExecutor::platform_default`] is therefore single-threaded on Android and
//! multi-threaded elsewhere. Systems still fan out inside themselves through
//! `regia_core::tasks` (`par_chunks`, `scope`); only the hand-off between systems moves to the
//! main thread. [`ScheduleExecutorSetting`] holds the choice; [`crate::App::update`] applies a
//! changed choice to every schedule before the frame's first schedule runs, and to schedules
//! added later, so a host or the agent (`executor.set`) can switch it between two frames.

use std::collections::HashSet;

use regia_ecs::ecs::schedule::{
    InternedScheduleLabel, MultiThreadedExecutor, Schedules, SingleThreadedExecutor,
};
use regia_ecs::prelude::*;

/// The executor kind of every schedule of the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScheduleExecutor {
    /// `bevy_ecs`'s multi-threaded executor: systems run as compute-pool tasks, in parallel where
    /// their data access allows.
    MultiThreaded,
    /// `bevy_ecs`'s single-threaded executor: systems run one after another on the thread that
    /// runs the schedule (the main thread), in the same topological order.
    SingleThreaded,
}

impl ScheduleExecutor {
    /// The default of the platform this was compiled for: single-threaded on Android (thread
    /// hand-offs cost more than the parallelism wins there, ADR-4933), multi-threaded elsewhere.
    #[must_use]
    pub const fn platform_default() -> Self {
        Self::default_for(cfg!(target_os = "android"))
    }

    /// The default for an Android (`true`) or other (`false`) target; [`Self::platform_default`]
    /// passes the compile target.
    #[must_use]
    pub const fn default_for(android: bool) -> Self {
        if android {
            Self::SingleThreaded
        } else {
            Self::MultiThreaded
        }
    }

    /// The protocol name: `multi_threaded` or `single_threaded`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MultiThreaded => "multi_threaded",
            Self::SingleThreaded => "single_threaded",
        }
    }

    /// Parses a protocol name ([`Self::as_str`]).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "multi_threaded" => Some(Self::MultiThreaded),
            "single_threaded" => Some(Self::SingleThreaded),
            _ => None,
        }
    }
}

/// The app's executor choice ([`ScheduleExecutor`]), a resource every [`crate::App`] has.
///
/// Set [`ScheduleExecutorSetting::requested`] (directly or through
/// [`crate::App::set_schedule_executor`]); the next [`crate::App::update`] installs it on every
/// schedule before `First` runs. A schedule nobody switched keeps the executor it was created
/// with (`bevy_ecs`'s multi-threaded one), so the multi-threaded default never touches a
/// schedule that a test or tool gave its own executor.
#[derive(Resource, Debug, Clone)]
pub struct ScheduleExecutorSetting {
    /// The executor the schedules should run on.
    pub requested: ScheduleExecutor,
    /// The executor installed on the schedules in `switched`.
    applied: ScheduleExecutor,
    /// Schedules this setting installed `applied` on.
    switched: HashSet<InternedScheduleLabel>,
    /// How many times a changed request was applied.
    switches: u32,
}

impl Default for ScheduleExecutorSetting {
    fn default() -> Self {
        Self::new(ScheduleExecutor::platform_default())
    }
}

impl ScheduleExecutorSetting {
    /// A setting that requests `requested`. Nothing is installed until the next update.
    #[must_use]
    pub fn new(requested: ScheduleExecutor) -> Self {
        Self {
            requested,
            // Every schedule is created with bevy's multi-threaded executor.
            applied: ScheduleExecutor::MultiThreaded,
            switched: HashSet::new(),
            switches: 0,
        }
    }

    /// The executor the schedules run on now (the last applied request).
    #[must_use]
    pub fn applied(&self) -> ScheduleExecutor {
        self.applied
    }

    /// How many times a changed request has been applied.
    #[must_use]
    pub fn switches(&self) -> u32 {
        self.switches
    }

    /// Number of schedules this setting installed its executor on.
    #[must_use]
    pub fn switched_schedules(&self) -> usize {
        self.switched.len()
    }
}

/// Installs the requested executor on every schedule that needs it; called by
/// [`crate::App::update`] before the frame's first schedule (no schedule is running then).
/// Returns how many schedules got a new executor.
pub(crate) fn apply_schedule_executor(world: &mut World) -> usize {
    let Some(setting) = world.get_resource::<ScheduleExecutorSetting>() else {
        return 0;
    };
    let requested = setting.requested;
    let changed = requested != setting.applied;
    // Nothing to do when the request is the creation default and nothing was ever switched:
    // the common desktop case costs one resource read a frame.
    if !changed && requested == ScheduleExecutor::MultiThreaded && setting.switched.is_empty() {
        return 0;
    }
    let schedule_count = world.get_resource::<Schedules>().map_or(0, |s| s.iter().count());
    // Cheap skip when every schedule is covered. A count comparison is not enough: a schedule
    // removed and another added in the same frame keeps the count equal while the new label was
    // never switched, and it kept the creation executor under a single-threaded request
    // (corr-low, ADR-5789).
    if !changed && setting.switched.len() == schedule_count {
        let all_covered = world.get_resource::<Schedules>().is_none_or(|s| {
            s.iter()
                .all(|(_, schedule)| setting.switched.contains(&schedule.label()))
        });
        if all_covered {
            return 0;
        }
    }
    let _span = tracing::info_span!("apply_schedule_executor").entered();
    world.resource_scope(|world, mut setting: Mut<ScheduleExecutorSetting>| {
        let Some(mut schedules) = world.get_resource_mut::<Schedules>() else {
            return 0;
        };
        if changed {
            setting.switched.clear();
        }
        let mut installed = 0;
        for (_, schedule) in schedules.iter_mut() {
            let label = schedule.label();
            if !changed && setting.switched.contains(&label) {
                continue;
            }
            match requested {
                ScheduleExecutor::MultiThreaded => schedule.set_executor(MultiThreadedExecutor::new()),
                ScheduleExecutor::SingleThreaded => schedule.set_executor(SingleThreadedExecutor::new()),
            };
            setting.switched.insert(label);
            installed += 1;
        }
        // Drop labels of removed schedules, so the count stays truthful and the fast skip above
        // keeps matching after a swap.
        let live: Vec<_> = schedules.iter().map(|(_, s)| s.label()).collect();
        setting.switched.retain(|label| live.contains(label));
        if changed {
            setting.applied = requested;
            setting.switches += 1;
            tracing::info!(
                target: "regia_app",
                "schedule executor: {} on {installed} schedules",
                requested.as_str()
            );
        }
        installed
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn android_defaults_to_single_threaded_and_desktop_to_multi_threaded() {
        assert_eq!(
            ScheduleExecutor::default_for(true),
            ScheduleExecutor::SingleThreaded
        );
        assert_eq!(
            ScheduleExecutor::default_for(false),
            ScheduleExecutor::MultiThreaded
        );
        assert_eq!(
            ScheduleExecutor::platform_default(),
            ScheduleExecutor::default_for(cfg!(target_os = "android"))
        );
    }

    #[test]
    fn names_round_trip() {
        for kind in [ScheduleExecutor::MultiThreaded, ScheduleExecutor::SingleThreaded] {
            assert_eq!(ScheduleExecutor::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(ScheduleExecutor::parse("parallel"), None);
    }

    use crate::prelude::*;
    use regia_ecs::ecs::schedule::ScheduleLabel;
    use std::sync::{Arc, Mutex};

    #[derive(ScheduleLabel, Debug, Clone, PartialEq, Eq, Hash)]
    struct LateSchedule;

    #[derive(ScheduleLabel, Debug, Clone, PartialEq, Eq, Hash)]
    struct SwappedSchedule;

    fn setting(app: &App) -> &ScheduleExecutorSetting {
        app.world().resource::<ScheduleExecutorSetting>()
    }

    #[test]
    fn the_multi_threaded_request_leaves_every_schedule_alone() {
        let mut app = App::new();
        app.set_schedule_executor(ScheduleExecutor::MultiThreaded);
        app.add_systems(Update, || {});
        app.update();
        app.update();
        assert_eq!(setting(&app).switched_schedules(), 0);
        assert_eq!(setting(&app).switches(), 0);
        assert_eq!(setting(&app).applied(), ScheduleExecutor::MultiThreaded);
    }

    #[test]
    fn single_threaded_runs_the_systems_on_the_updating_thread() {
        let seen: Arc<Mutex<Vec<std::thread::ThreadId>>> = Arc::default();
        let mut app = App::new();
        app.set_schedule_executor(ScheduleExecutor::SingleThreaded);
        for _ in 0..4 {
            let seen = seen.clone();
            app.add_systems(Update, move || {
                if let Ok(mut s) = seen.lock() {
                    s.push(std::thread::current().id());
                }
            });
        }
        app.update();
        let me = std::thread::current().id();
        let seen = seen.lock().map(|s| s.clone()).unwrap_or_default();
        assert_eq!(seen.len(), 4);
        assert!(
            seen.iter().all(|t| *t == me),
            "every system on the main thread: {seen:?}"
        );
        assert_eq!(setting(&app).applied(), ScheduleExecutor::SingleThreaded);
        assert_eq!(setting(&app).switches(), 1);
    }

    #[test]
    fn a_schedule_added_later_gets_the_executor_and_a_switch_back_reaches_all() {
        let mut app = App::new();
        app.set_schedule_executor(ScheduleExecutor::SingleThreaded);
        app.update();
        let before = setting(&app).switched_schedules();
        assert!(before > 5, "the standard schedules: {before}");
        app.world_mut()
            .resource_mut::<Schedules>()
            .insert(Schedule::new(LateSchedule));
        app.update();
        assert_eq!(setting(&app).switched_schedules(), before + 1);
        // The late schedule is installed without counting a new switch.
        assert_eq!(setting(&app).switches(), 1);
        app.set_schedule_executor(ScheduleExecutor::MultiThreaded);
        app.update();
        assert_eq!(setting(&app).switched_schedules(), before + 1);
        assert_eq!(setting(&app).switches(), 2);
        assert_eq!(setting(&app).applied(), ScheduleExecutor::MultiThreaded);
    }

    /// corr-low: the skip check compared the switched count with the schedule count. A schedule
    /// removed and another added in the same frame keeps the counts equal, so the new label was
    /// never switched and kept the creation (multi-threaded) executor under a single-threaded
    /// request. Only performance is affected, but the request must reach every schedule.
    #[test]
    fn a_schedule_swap_still_gets_the_requested_executor() {
        let seen: Arc<Mutex<Option<std::thread::ThreadId>>> = Arc::default();
        let mut app = App::new();
        app.set_schedule_executor(ScheduleExecutor::SingleThreaded);
        app.update();
        // Cover a custom schedule, then swap it for another between two updates: the counts stay
        // equal while the new label was never switched. The swap brings the observing system, and
        // the next update must install the requested executor on the new label.
        app.world_mut()
            .resource_mut::<Schedules>()
            .insert(Schedule::new(LateSchedule));
        app.update();
        {
            let mut schedules = app.world_mut().resource_mut::<Schedules>();
            schedules.remove(LateSchedule);
            let mut schedule = Schedule::new(SwappedSchedule);
            let seen = Arc::clone(&seen);
            schedule.add_systems(move || {
                if let Ok(mut s) = seen.lock() {
                    *s = Some(std::thread::current().id());
                }
            });
            schedules.insert(schedule);
        }
        app.update();
        app.world_mut().run_schedule(SwappedSchedule);
        let me = std::thread::current().id();
        let ran_on = seen.lock().map(|s| *s).unwrap_or_default();
        assert_eq!(
            ran_on,
            Some(me),
            "the swapped-in schedule must run on the updating thread under SingleThreaded"
        );
    }
}
