// SPDX-License-Identifier: MPL-2.0
//! `App`: the world, its schedules, plugins and the runner (`docs/specs/10-core.md` § 3.1).

use std::collections::HashSet;
use std::num::NonZeroU8;

use regia_ecs::ecs::message::{MessageCursor, MessageRegistry, message_update_system};
use regia_ecs::ecs::observer::IntoObserver;
use regia_ecs::ecs::schedule::{InternedSystemSet, ScheduleConfigs, SystemKey};
use regia_ecs::ecs::system::ScheduleSystem;
use regia_ecs::owner::{OwnedMessage, OwnedObserver, OwnedSystem, register_type_owned};
use regia_ecs::prelude::*;
use regia_ecs::reflect::AppTypeRegistry;
use regia_ecs::state::StateTransition;

use crate::plugin::{Plugin, Plugins};
use crate::schedules::{
    self, First, FixedUpdate, Last, Lifecycle, PostUpdate, PostUpdateSet, PreUpdate, Render, Startup, Update,
};
use crate::time::{FixedTime, FramePacing, RealTime, Time, TimeUpdateStrategy, update_time};

/// Request to end the app. Write it with `MessageWriter<AppExit>` or call [`App::exit`].
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppExit {
    /// Normal exit.
    Success,
    /// Exit with a process error code.
    Error(NonZeroU8),
}

type Runner = Box<dyn FnOnce(App) -> AppExit>;

/// A deferred registration of sub/computed-state transition systems (`crate::state`).
pub(crate) type PendingStateSystems = Box<dyn FnOnce(&mut Schedule) + Send + Sync>;

/// The application: one ECS world, the frame schedules, the plugins that configured them and the
/// runner that drives frames (the platform layer installs a winit runner).
///
/// # Example
///
/// ```
/// use regia_app::prelude::*;
///
/// let mut app = App::new();
/// app.update();
/// assert_eq!(app.world().resource::<Time>().frame_count(), 1);
/// ```
pub struct App {
    world: World,
    plugins: Vec<Box<dyn Plugin>>,
    plugin_names: HashSet<String>,
    /// Names of plugins added while a game owner was current, so a hot reload can forget them.
    game_plugin_owners: Vec<(String, TypeOwner)>,
    runner: Option<Runner>,
    plugins_finished: bool,
    /// `finish_plugins` has completed: plugins added from now on are finished at once.
    plugins_done: bool,
    started: bool,
    /// Sub/computed-state transition systems waiting for the first `StateTransition` run
    /// (their source state's message buffer must exist by then; `crate::state`).
    pub(crate) pending_state_systems: Vec<PendingStateSystems>,
    exit_cursor: MessageCursor<AppExit>,
    /// Exit request latched before the message buffers rotate; `Error` wins over `Success`.
    pending_exit: Option<AppExit>,
    phase_clock: crate::phases::PhaseClock,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    /// An app with the standard schedules (and the [`PostUpdateSet`] order), time resources, the
    /// type registry ([`AppTypeRegistry`] + [`TypeOwnership`]), the [`AppExit`] message, and a
    /// runner that calls [`App::update`] until something requests exit ([`run_headless`]).
    pub fn new() -> Self {
        // Size the global task pools before any schedule runs: the multi-threaded executor
        // creates the compute pool with bevy's default (every hardware thread, no Android cap) on
        // the first non-empty schedule. Idempotent; pools created earlier are kept.
        regia_core::tasks::init(regia_core::tasks::TaskPoolOptions::default());
        let mut world = World::new();
        let mut all = Schedules::default();
        for label in schedules::all() {
            all.insert(Schedule::new(label));
        }
        world.insert_resource(all);
        // `StateTransition` with its exit → transition → enter set order (`bevy_state`); run by
        // `update` before `Startup` and after `PreUpdate`.
        regia_ecs::state::setup_state_transitions_in_world(&mut world);
        world.init_resource::<Time>();
        // With the other built-in resources, so `update` only overwrites it: a resource is an
        // entity, and inserting one mid-session is a structural change.
        world.init_resource::<crate::FramePhases>();
        world.init_resource::<RealTime>();
        world.init_resource::<FixedTime>();
        world.init_resource::<TimeUpdateStrategy>();
        world.init_resource::<FramePacing>();
        // Single-threaded on Android, multi-threaded elsewhere (ADR-4933); applied by `update`.
        world.init_resource::<crate::ScheduleExecutorSetting>();
        world.init_resource::<AppTypeRegistry>();
        world.init_resource::<TypeOwnership>();
        let mut app = Self {
            world,
            plugins: Vec::new(),
            plugin_names: HashSet::new(),
            game_plugin_owners: Vec::new(),
            runner: Some(Box::new(run_headless)),
            plugins_finished: false,
            plugins_done: false,
            started: false,
            pending_state_systems: Vec::new(),
            exit_cursor: MessageCursor::default(),
            pending_exit: None,
            phase_clock: crate::phases::PhaseClock::default(),
        };
        app.add_message::<AppExit>();
        app.add_systems(
            First,
            (
                message_update_system,
                crate::loop_settings::enforce_freeze_pause,
                update_time,
            )
                .chain(),
        );
        app.add_systems(Last, crate::loop_settings::update_loop_control);
        // Per-state caps (ADR-7889): `PacingCaps` and `PacingState` are not built in (a resource is
        // an entity); absent, no state cap applies.
        app.add_systems(Last, crate::state_pacing::sync_state_pacing);
        app.configure_sets(
            PostUpdate,
            (
                PostUpdateSet::LateScripts,
                PostUpdateSet::PhysicsSync,
                PostUpdateSet::AnimationApply,
                PostUpdateSet::TransformPropagate,
                PostUpdateSet::Visibility,
                PostUpdateSet::AudioSync,
                PostUpdateSet::UiLayout,
            )
                .chain(),
        );
        app
    }

    /// Requests the executor every schedule runs on ([`crate::ScheduleExecutor`], ADR-4933). The
    /// next [`App::update`] installs it before its first schedule. The default is
    /// [`crate::ScheduleExecutor::platform_default`].
    pub fn set_schedule_executor(&mut self, executor: crate::ScheduleExecutor) -> &mut Self {
        self.world
            .get_resource_or_insert_with(crate::ScheduleExecutorSetting::default)
            .requested = executor;
        self
    }

    /// The owner new registrations (types, systems, messages, observers) are recorded under.
    pub fn current_owner(&self) -> TypeOwner {
        self.world
            .get_resource::<TypeOwnership>()
            .map_or(TypeOwner::Engine, TypeOwnership::current)
    }

    /// Sets the owner new registrations are recorded under; returns the previous one. The game
    /// plugin loader sets [`TypeOwner::Game`] around a game plugin's `register()`.
    pub fn set_current_owner(&mut self, owner: TypeOwner) -> TypeOwner {
        self.world
            .get_resource_or_init::<TypeOwnership>()
            .set_current(owner)
    }

    /// Runs `f` with `owner` as the current owner, then restores the previous one.
    pub fn with_owner(&mut self, owner: TypeOwner, f: impl FnOnce(&mut App)) -> &mut Self {
        let previous = self.set_current_owner(owner);
        f(self);
        self.set_current_owner(previous);
        self
    }

    /// The ownership records of every registration made through this app.
    pub fn ownership(&self) -> &TypeOwnership {
        self.world.resource::<TypeOwnership>()
    }

    /// Registers a reflected type (and the types it depends on) under the current owner.
    pub fn register_type<T: GetTypeRegistration>(&mut self) -> &mut Self {
        register_type_owned::<T>(&mut self.world);
        self
    }

    /// The app's type registry.
    pub fn type_registry(&self) -> &AppTypeRegistry {
        self.world.resource::<AppTypeRegistry>()
    }

    /// Configures system sets in a schedule (ordering, run conditions).
    pub fn configure_sets<M>(
        &mut self,
        schedule: impl ScheduleLabel,
        sets: impl IntoScheduleConfigs<InternedSystemSet, M>,
    ) -> &mut Self {
        self.world
            .resource_mut::<Schedules>()
            .configure_sets(schedule, sets);
        self
    }

    /// Spawns an observer. Observers spawned while a game owner is current are recorded.
    pub fn add_observer<M>(&mut self, observer: impl IntoObserver<M>) -> &mut Self {
        let entity = self.world.add_observer(observer).id();
        let owner = self.current_owner();
        if owner != TypeOwner::Engine {
            self.world
                .resource_mut::<TypeOwnership>()
                .record_observer(OwnedObserver { owner, entity });
        }
        self
    }

    /// Runs a schedule now, outside the frame: the trigger for [`crate::OnLevelLoaded`] and
    /// [`crate::OnLevelUnloading`]. A missing schedule is logged and ignored.
    pub fn run_schedule(&mut self, schedule: impl ScheduleLabel) {
        self.flush_pending_state_systems();
        let label = schedule.intern();
        let started = std::time::Instant::now();
        if let Err(error) = self.world.try_run_schedule(label) {
            tracing::warn!(target: "regia_app", "cannot run schedule {label:?}: {error}");
        }
        self.phase_clock.host_schedules += started.elapsed();
    }

    /// Waits as needed to honour [`FramePacing::effective_fps`] (the tighter of the player's
    /// [`FramePacing::target_fps`] and the [`crate::PacingCaps`] cap of the current
    /// [`crate::PacingState`]). Runners call this once per frame, after [`App::update`]. Returns
    /// the time waited.
    ///
    /// [`crate::LoopMode::Bench`] never waits; a frozen [`crate::LoopMode::RunFrames`] loop waits for
    /// [`crate::LoopControl::throttle_period`] instead of the cap.
    pub fn pace_frame(&mut self) -> std::time::Duration {
        self.pace_frame_with(&mut crate::time::SystemPacingClock)
    }

    /// [`App::pace_frame`] on `clock` ([`crate::PacingClock`]): tests pass a clock that jumps
    /// instead of sleeping, so the computed wait is exact.
    pub fn pace_frame_with(&mut self, clock: &mut impl crate::PacingClock) -> std::time::Duration {
        let _span = tracing::trace_span!("pace_frame").entered();
        let period = match self.world.get_resource::<crate::LoopControl>() {
            Some(control) if control.mode() == crate::LoopMode::Bench => None,
            Some(control) if control.is_frozen() => Some(control.throttle_period()),
            _ => self
                .world
                .get_resource::<FramePacing>()
                .and_then(FramePacing::period),
        };
        let waited = self
            .world
            .get_resource_mut::<FramePacing>()
            .map_or(std::time::Duration::ZERO, |mut pacing| {
                pacing.wait_for_with(period, clock)
            });
        self.phase_clock.pacing += waited;
        waited
    }

    /// Counts `waited` as frame pacing in the frame's phases, for a runner that waited for the
    /// display itself instead of calling [`App::pace_frame`].
    pub fn note_pacing_wait(&mut self, waited: std::time::Duration) {
        self.phase_clock.pacing += waited;
    }

    /// The ECS world.
    pub fn world(&self) -> &World {
        &self.world
    }

    /// The ECS world, mutably.
    pub fn world_mut(&mut self) -> &mut World {
        &mut self.world
    }

    /// Adds one plugin or a tuple of plugins; each is built immediately.
    ///
    /// Adding a plugin whose [`Plugin::name`] was already added logs a warning and skips the
    /// duplicate (the first registration wins); a game that double-adds does not fail loudly.
    pub fn add_plugins(&mut self, plugins: impl Plugins) -> &mut Self {
        plugins.add_to(self);
        self
    }

    pub(crate) fn add_boxed_plugin(&mut self, plugin: Box<dyn Plugin>) {
        let name = plugin.name().to_string();
        if !self.plugin_names.insert(name.clone()) {
            tracing::warn!(target: "regia_app", "plugin {name} added twice; ignoring the second");
            return;
        }
        let owner = self.current_owner();
        if owner != TypeOwner::Engine {
            self.game_plugin_owners.push((name, owner));
        }
        plugin.build(self);
        if self.plugins_done {
            // Added after `finish_plugins` ran (a hot reload): its `finish` and `cleanup` are due now.
            plugin.finish(self);
            plugin.cleanup(self);
        }
        self.plugins.push(plugin);
    }

    /// Forgets the names of plugins `owner` added, so a hot-reloaded `register` can add them
    /// again. Call it after tearing the owner down (its systems are already gone); without it
    /// the duplicate-plugin check skips the plugin and its systems never come back.
    pub fn forget_owner_plugins(&mut self, owner: TypeOwner) {
        let names = &mut self.plugin_names;
        let mut removed: HashSet<String> = HashSet::new();
        self.game_plugin_owners.retain(|(name, plugin_owner)| {
            if *plugin_owner == owner {
                names.remove(name);
                removed.insert(name.clone());
                false
            } else {
                true
            }
        });
        // Drop the plugin objects too, or every reload leaves one more behind.
        self.plugins.retain(|plugin| !removed.contains(plugin.name()));
    }

    /// True if a plugin with this [`Plugin::name`] has been added.
    pub fn has_plugin(&self, name: &str) -> bool {
        self.plugin_names.contains(name)
    }

    /// Adds systems to a schedule. While a [`TypeOwner::Game`] owner is current, the systems are
    /// placed in its [`OwnerSet`] and recorded in [`TypeOwnership`].
    pub fn add_systems<M>(
        &mut self,
        schedule: impl ScheduleLabel,
        systems: impl IntoScheduleConfigs<ScheduleSystem, M>,
    ) -> &mut Self {
        let owner = self.current_owner();
        if owner == TypeOwner::Engine {
            self.world
                .resource_mut::<Schedules>()
                .add_systems(schedule, systems);
            return self;
        }
        let label = schedule.intern();
        let tagged: ScheduleConfigs<ScheduleSystem> = systems.in_set(OwnerSet(owner));
        let mut schedules = self.world.resource_mut::<Schedules>();
        let before: HashSet<SystemKey> = schedules
            .get(label)
            .map(|s| s.graph().systems.iter().map(|(key, _, _)| key).collect())
            .unwrap_or_default();
        schedules.add_systems(label, tagged);
        let added: Vec<OwnedSystem> = schedules
            .get(label)
            .map(|s| {
                s.graph()
                    .systems
                    .iter()
                    .filter(|(key, _, _)| !before.contains(key))
                    .map(|(key, system, _)| OwnedSystem {
                        owner,
                        schedule: label,
                        key,
                        name: system.name().to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut ownership = self.world.resource_mut::<TypeOwnership>();
        for system in added {
            ownership.record_system(system);
        }
        self
    }

    /// Inserts (or replaces) a resource.
    pub fn insert_resource<R: Resource>(&mut self, resource: R) -> &mut Self {
        self.world.insert_resource(resource);
        self
    }

    /// Inserts a resource's default value unless it already exists.
    pub fn init_resource<R: Resource + FromWorld>(&mut self) -> &mut Self {
        self.world.init_resource::<R>();
        self
    }

    /// Registers a buffered message type (an "ECS event", specs/10 § 2.1). Idempotent.
    /// Recorded in [`TypeOwnership`] when first registered by a game owner.
    pub fn add_message<M: Message>(&mut self) -> &mut Self {
        if !self.world.contains_resource::<Messages<M>>() {
            MessageRegistry::register_message::<M>(&mut self.world);
            let owner = self.current_owner();
            if owner != TypeOwner::Engine {
                self.world
                    .resource_mut::<TypeOwnership>()
                    .record_message(OwnedMessage {
                        owner,
                        type_id: std::any::TypeId::of::<M>(),
                        type_name: std::any::type_name::<M>(),
                    });
            }
        }
        self
    }

    /// Replaces the runner, the function that drives frames (the platform layer installs one).
    pub fn set_runner(&mut self, runner: impl FnOnce(App) -> AppExit + 'static) -> &mut Self {
        self.runner = Some(Box::new(runner));
        self
    }

    /// Runs every plugin's `finish` and `cleanup` once. Called by [`App::run`]; runners that build
    /// apps differently may call it themselves.
    pub fn finish_plugins(&mut self) {
        if self.plugins_finished {
            return;
        }
        self.plugins_finished = true;
        // Plugins added from a `finish` land in `self.plugins`; they are finished in the next
        // round and kept in the registry. Every `finish` (including late ones) runs before any
        // `cleanup`, so a parent's cleanup never precedes its child's finish.
        let mut plugins = std::mem::take(&mut self.plugins);
        let mut done = 0;
        while done < plugins.len() {
            let end = plugins.len();
            for plugin in &plugins[done..end] {
                plugin.finish(self);
            }
            plugins.append(&mut self.plugins);
            done = end;
        }
        for plugin in &plugins {
            plugin.cleanup(self);
        }
        // Plugins added from a `cleanup` still get their `finish` and `cleanup`.
        let mut done = plugins.len();
        plugins.append(&mut self.plugins);
        while done < plugins.len() {
            let end = plugins.len();
            for plugin in &plugins[done..end] {
                plugin.finish(self);
            }
            for plugin in &plugins[done..end] {
                plugin.cleanup(self);
            }
            plugins.append(&mut self.plugins);
            done = end;
        }
        self.plugins = plugins;
        self.plugins_done = true;
    }

    /// Runs one frame: `StateTransition` + `Startup` (first frame only), `First`, `PreUpdate`,
    /// `StateTransition`, `FixedUpdate` ×N, `Update`, `PostUpdate`, `Extract`, `Render`, `Last`.
    ///
    /// The first-frame `StateTransition` enters every state's initial value (its `OnEnter`
    /// runs before `Startup`); the per-frame one applies `NextState` requests made last frame or
    /// in `PreUpdate`, so `FixedUpdate`/`Update` see a consistent state for the whole frame.
    ///
    /// Each schedule is timed (two clock reads) and the frame is published as
    /// [`crate::FramePhases`] once it ends.
    pub fn update(&mut self) {
        use crate::phases::{Phase, ms};
        let _span = tracing::info_span!("frame").entered();
        self.latch_exit();
        self.flush_pending_state_systems();
        regia_core::profiling::mark_main_thread();
        let start = std::time::Instant::now();
        let mut phases = crate::FramePhases {
            runner: self.world.get_resource::<crate::RunnerState>().copied(),
            ..Default::default()
        };
        self.phase_clock.begin(start, &mut phases);
        // No schedule is running here: install a changed executor choice (ADR-4933).
        crate::executor::apply_schedule_executor(&mut self.world);
        let mut lap = start;
        let mut mark = |phases: &mut crate::FramePhases, phase: Phase| {
            let now = std::time::Instant::now();
            phases.ms[phase as usize] += ms(now.saturating_duration_since(lap));
            lap = now;
        };
        if !self.started {
            self.started = true;
            self.world.run_schedule(StateTransition);
            self.world.run_schedule(Startup);
        }
        self.world.run_schedule(First);
        mark(&mut phases, Phase::First);
        self.world.run_schedule(PreUpdate);
        mark(&mut phases, Phase::PreUpdate);
        {
            let _span = tracing::trace_span!("state_transition").entered();
            self.world.run_schedule(StateTransition);
        }
        mark(&mut phases, Phase::StateTransition);
        // The fixed loop runs on the simulation clock (`Time::sim_delta`: the frame delta unless a
        // `SimStep` sets it apart, ADR-1524).
        let (view, delta, frame_elapsed) = self.world.get_resource::<Time>().map_or(
            (None, std::time::Duration::ZERO, std::time::Duration::ZERO),
            |time| (Some(time.frame_view()), time.sim_delta(), time.sim_elapsed()),
        );
        // Each FixedUpdate run sees one step as `Time::delta`, with the clock advancing a step
        // at a time; the frame's view comes back afterwards. The fixed clock starts where the
        // last run step ended, so time the substep cap discarded is never added back (spec 10
        // § 3.2). The frame's simulation start (`frame_elapsed - delta`) equals the clock
        // recorded at the last fixed loop unless `Time::set_clocks` moved the clock between
        // frames (a render-out job restarting or handing the clocks back, ADR-1881); then the
        // steps re-anchor to the frame's simulation start minus the banked sub-step phase, so a
        // forward jump no longer leaves the fixed clock lagging by the jump for the rest of the
        // session, and a rewind never starts past the frame's clock.
        let (steps, step, fixed_start) = {
            let mut fixed = self.world.get_resource_or_insert_with(FixedTime::default);
            let frame_start = frame_elapsed.saturating_sub(delta);
            let start = if frame_start == fixed.last_frame_clock() {
                fixed.last_step_end().min(frame_start)
            } else {
                frame_start.saturating_sub(fixed.banked())
            };
            let steps = fixed.accumulate(delta);
            let step = fixed.step();
            fixed.set_last_frame_clock(frame_elapsed);
            (steps, step, start)
        };
        for i in 0..steps {
            if let Some(mut time) = self.world.get_resource_mut::<Time>() {
                // Never past the frame's simulation clock (`set_clocks` can rewind it under the bank).
                let at = fixed_start
                    .saturating_add(step.saturating_mul(i + 1))
                    .min(frame_elapsed);
                time.set_fixed_view(step, at);
            }
            self.world.run_schedule(FixedUpdate);
        }
        if steps > 0
            && let Some(mut fixed) = self.world.get_resource_mut::<FixedTime>()
        {
            fixed.set_last_step_end(
                fixed_start
                    .saturating_add(step.saturating_mul(steps))
                    .min(frame_elapsed),
            );
        }
        if steps > 0
            && let Some(view) = view
            && let Some(mut time) = self.world.get_resource_mut::<Time>()
        {
            time.restore_frame_view(view);
        }
        phases.fixed_steps = steps;
        mark(&mut phases, Phase::FixedUpdate);
        self.world.run_schedule(Update);
        mark(&mut phases, Phase::Update);
        self.world.run_schedule(PostUpdate);
        mark(&mut phases, Phase::PostUpdate);
        self.world.run_schedule(crate::Extract);
        mark(&mut phases, Phase::Extract);
        self.world.run_schedule(Render);
        mark(&mut phases, Phase::Render);
        self.world.run_schedule(Last);
        mark(&mut phases, Phase::Last);
        self.world.clear_trackers();
        mark(&mut phases, Phase::ClearTrackers);
        self.phase_clock.frames += 1;
        phases.frame = self.phase_clock.frames;
        self.phase_clock.last_update_end = Some(lap);
        if let Some(mut published) = self.world.get_resource_mut::<crate::FramePhases>() {
            *published.bypass_change_detection() = phases;
        } else {
            self.world.insert_resource(phases);
        }
    }

    /// Records time the runner actually spent waiting for events, excluding callbacks and updates.
    /// The next completed frame publishes it as [`crate::Phase::EventWait`]. Multiple wakes add;
    /// the first update discards pre-start waits, and accounting is clamped to the actual interval.
    pub fn record_event_wait(&mut self, waited: std::time::Duration) {
        self.phase_clock.event_wait = self.phase_clock.event_wait.saturating_add(waited);
    }

    /// Runs the [`Lifecycle`] schedule now, outside the frame (ADR-012 § 2).
    pub fn run_lifecycle(&mut self) {
        self.world.run_schedule(Lifecycle);
    }

    /// Requests exit at the end of the current frame.
    pub fn exit(&mut self) {
        self.world.write_message(AppExit::Success);
    }

    /// The exit request written since the last call, if any.
    ///
    /// Requests are latched at the start of every [`App::update`], so one written during a frame
    /// is not lost to the message buffers rotating before a runner polls. When several arrive
    /// together an `Error` wins over `Success`, and the latest `Error` code is kept.
    pub fn should_exit(&mut self) -> Option<AppExit> {
        self.latch_exit();
        self.pending_exit.take()
    }

    fn latch_exit(&mut self) {
        let Some(messages) = self.world.get_resource::<Messages<AppExit>>() else {
            return;
        };
        let mut cursor = self.exit_cursor.clone();
        for exit in cursor.read(messages).copied() {
            // An error exit must not be hidden by a success in the same frame; the latest error
            // code wins.
            if matches!(exit, AppExit::Error(_)) || !matches!(self.pending_exit, Some(AppExit::Error(_))) {
                self.pending_exit = Some(exit);
            }
        }
        self.exit_cursor = cursor;
    }

    /// Finishes plugins and hands the app to the runner. Returns the runner's exit status.
    pub fn run(mut self) -> AppExit {
        self.finish_plugins();
        let runner = self.runner.take().unwrap_or_else(|| Box::new(run_headless));
        runner(self)
    }
}

/// Default runner: frames back to back until exit, paced by [`FramePacing`]. Used headless (tests,
/// servers, [`crate::MinimalPlugins`]).
pub fn run_headless(mut app: App) -> AppExit {
    loop {
        app.update();
        if let Some(exit) = app.should_exit() {
            return exit;
        }
        app.pace_frame();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::*;
    use crate::Extract;

    #[derive(Resource, Default)]
    struct Order(Vec<&'static str>);

    fn push(name: &'static str) -> impl FnMut(ResMut<Order>) {
        move |mut order: ResMut<Order>| order.0.push(name)
    }

    #[test]
    fn frame_phases_time_each_schedule_and_sum_to_the_frame() {
        let mut app = App::new();
        app.add_systems(Update, |_: Commands| {
            std::thread::sleep(std::time::Duration::from_millis(3))
        });
        app.add_systems(Render, |_: Commands| {
            std::thread::sleep(std::time::Duration::from_millis(2))
        });
        app.update();
        std::thread::sleep(std::time::Duration::from_millis(4));
        app.update();
        let phases = *app.world().resource::<crate::FramePhases>();
        assert_eq!(phases.frame, 2);
        assert!(phases.get(crate::Phase::Update) >= 3.0, "{phases:?}");
        assert!(phases.get(crate::Phase::Render) >= 2.0, "{phases:?}");
        assert!(phases.get(crate::Phase::OutsideUpdate) >= 4.0, "{phases:?}");
        let total = phases.total_ms();
        assert!((9.0..59.0).contains(&total), "{total}");
    }

    #[test]
    fn schedules_run_in_frame_order() {
        let mut app = App::new();
        app.init_resource::<Order>();
        app.add_systems(Last, push("last"))
            .add_systems(Render, push("render"))
            .add_systems(Extract, push("extract"))
            .add_systems(PostUpdate, push("post"))
            .add_systems(Update, push("update"))
            .add_systems(PreUpdate, push("pre"))
            .add_systems(First, push("first"))
            .add_systems(Startup, push("startup"))
            .add_systems(Lifecycle, push("lifecycle"));
        app.update();
        app.update();
        assert_eq!(
            app.world().resource::<Order>().0,
            [
                "startup", "first", "pre", "update", "post", "extract", "render", "last", "first", "pre",
                "update", "post", "extract", "render", "last"
            ]
        );
        app.run_lifecycle();
        assert_eq!(app.world().resource::<Order>().0.last(), Some(&"lifecycle"));
    }

    struct Named(&'static str, Arc<Mutex<Vec<String>>>);
    impl Plugin for Named {
        fn build(&self, _app: &mut App) {
            self.1
                .lock()
                .map(|mut l| l.push(format!("build {}", self.0)))
                .ok();
        }
        fn finish(&self, _app: &mut App) {
            self.1
                .lock()
                .map(|mut l| l.push(format!("finish {}", self.0)))
                .ok();
        }
        fn name(&self) -> &str {
            self.0
        }
    }

    #[test]
    fn plugins_build_once_and_finish_after_all_builds() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut app = App::new();
        app.add_plugins((
            Named("a", log.clone()),
            Named("b", log.clone()),
            Named("a", log.clone()),
        ));
        app.finish_plugins();
        app.finish_plugins();
        let log = log.lock().map(|l| l.clone()).unwrap_or_default();
        assert_eq!(log, ["build a", "build b", "finish a", "finish b"]);
        assert!(app.has_plugin("b"));
    }

    #[test]
    fn double_add_logs_a_warning() {
        use tracing::{Event, Level, Metadata, Subscriber};

        /// Records every event's (level, target) so tests can assert on warnings.
        #[derive(Clone, Default)]
        struct EventLog {
            events: Arc<Mutex<Vec<(Level, String)>>>,
        }

        impl Subscriber for EventLog {
            fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                // Span ids must be non-zero (bevy_ecs creates a span per system with `trace`).
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
            fn event(&self, event: &Event<'_>) {
                self.events
                    .lock()
                    .map(|mut events| {
                        events.push((*event.metadata().level(), event.metadata().target().to_string()));
                    })
                    .ok();
            }
            fn enter(&self, _span: &tracing::span::Id) {}
            fn exit(&self, _span: &tracing::span::Id) {}
        }

        let log = EventLog::default();
        let events = log.events.clone();
        let warned = |events: &Arc<Mutex<Vec<(Level, String)>>>| {
            events.lock().is_ok_and(|events| {
                events
                    .iter()
                    .any(|(level, target)| *level == Level::WARN && target == "regia_app")
            })
        };
        tracing::subscriber::with_default(log, || {
            // Parallel tests hit the same `warn!` callsite with no subscriber. A
            // callsite caches its interest when it registers, and a thread that
            // registers it concurrently can compute "never" before our dispatcher
            // exists and publish the callsite only after our rebuild ran (seen
            // ~15 % of runs on the 48-core farm). Rebuild and retry until the
            // callsite is registered under our dispatcher; a missing warning
            // still fails after the bounded retries.
            for _ in 0..200 {
                tracing::callsite::rebuild_interest_cache();
                let mut app = App::new();
                app.add_plugins((
                    Named("dup", Arc::new(Mutex::new(Vec::new()))),
                    Named("dup", Arc::new(Mutex::new(Vec::new()))),
                ));
                if warned(&events) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        });
        let events = events.lock().map(|e| e.clone()).unwrap_or_default();
        assert!(
            events
                .iter()
                .any(|(level, target)| *level == Level::WARN && target == "regia_app"),
            "double-add must log a warning: {events:?}"
        );
    }

    #[test]
    fn exit_message_stops_default_runner() {
        let mut app = App::new();
        app.add_systems(Update, |time: Res<Time>, mut exit: MessageWriter<AppExit>| {
            if time.frame_count() == 3 {
                exit.write(AppExit::Success);
            }
        });
        assert_eq!(app.run(), AppExit::Success);
    }

    #[derive(Reflect, Default)]
    struct GameComponent {
        inner: GameInner,
    }

    #[derive(Reflect, Default)]
    struct GameInner(u8);

    #[derive(Message)]
    struct GameMessage;

    #[derive(Event)]
    struct GameEvent;

    fn game_system_a() {}
    fn game_system_b() {}
    fn engine_system() {}

    #[test]
    fn registrations_record_their_owner() {
        let game = TypeOwner::Game(3);
        let mut app = App::new();
        app.add_systems(Update, engine_system);
        app.with_owner(game, |app| {
            assert_eq!(app.current_owner(), game);
            app.register_type::<GameComponent>()
                .add_systems(Update, (game_system_a, game_system_b))
                .add_message::<GameMessage>()
                .add_observer(|_: On<GameEvent>| {});
        });
        assert_eq!(app.current_owner(), TypeOwner::Engine);
        assert!(
            app.type_registry()
                .read()
                .contains(std::any::TypeId::of::<GameInner>())
        );

        let ownership = app.ownership();
        assert_eq!(ownership.types_of(game).len(), 2, "type and its new dependency");
        let systems: Vec<_> = ownership.systems_of(game).map(|s| s.name.clone()).collect();
        assert_eq!(systems.len(), 2);
        assert!(
            systems.iter().any(|n| n.ends_with("game_system_a")),
            "{systems:?}"
        );
        assert!(ownership.systems_of(game).all(|s| s.schedule == Update.intern()));
        assert_eq!(ownership.messages_of(game).count(), 1);
        assert_eq!(ownership.observers_of(game).count(), 1);
        assert_eq!(ownership.owned_count(game), 6);
        assert_eq!(ownership.owned_count(TypeOwner::Engine), 0);

        // The records are enough to remove the systems again (the M11 teardown).
        app.update();
        let removed = app
            .world_mut()
            .resource_scope::<Schedules, _>(|world, mut schedules| {
                schedules.remove_systems_in_set(
                    Update,
                    OwnerSet(game),
                    world,
                    regia_ecs::ecs::schedule::ScheduleCleanupPolicy::RemoveSystemsOnly,
                )
            });
        assert_eq!(removed.ok(), Some(2));
        app.update();
    }

    #[test]
    fn post_update_sets_run_in_order() {
        let mut app = App::new();
        app.init_resource::<Order>();
        let names = [
            "late",
            "physics",
            "anim",
            "propagate",
            "visibility",
            "audio",
            "ui",
        ];
        for (set, name) in PostUpdateSet::ORDER.iter().zip(names).rev() {
            app.add_systems(PostUpdate, push(name).in_set(*set));
        }
        app.update();
        assert_eq!(app.world().resource::<Order>().0, names);
    }

    #[test]
    fn level_schedules_run_on_demand() {
        let mut app = App::new();
        app.init_resource::<Order>();
        app.add_systems(crate::OnLevelLoaded, push("loaded"))
            .add_systems(crate::OnLevelUnloading, push("unloading"));
        app.update();
        assert!(app.world().resource::<Order>().0.is_empty());
        app.run_schedule(crate::OnLevelLoaded);
        app.run_schedule(crate::OnLevelUnloading);
        assert_eq!(app.world().resource::<Order>().0, ["loaded", "unloading"]);
    }

    #[test]
    fn manual_time_strategy_steps_exactly() {
        let step = std::time::Duration::from_millis(20);
        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(step));
        app.add_plugins(crate::MinimalPlugins);
        app.world_mut().spawn_empty();
        for _ in 0..50 {
            app.update();
        }
        let time = app.world().resource::<Time>();
        assert_eq!(time.elapsed(), step * 50);
        assert_eq!(time.frame_count(), 50);
        let diagnostics = app.world().resource::<crate::Diagnostics>();
        assert_eq!(diagnostics.frame_time_avg, step);
        assert!(diagnostics.entity_count >= 1);
    }

    #[test]
    fn an_error_exit_wins_over_an_earlier_success() {
        let mut app = App::new();
        app.world_mut().write_message(AppExit::Success);
        app.world_mut()
            .write_message(AppExit::Error(NonZeroU8::new(3).expect("non-zero")));
        assert_eq!(
            app.should_exit(),
            Some(AppExit::Error(NonZeroU8::new(3).expect("non-zero")))
        );
        assert_eq!(app.should_exit(), None);
    }

    #[test]
    fn forgetting_an_owner_lets_its_plugins_be_added_again() {
        struct ReloadedPlugin;
        impl Plugin for ReloadedPlugin {
            fn build(&self, app: &mut App) {
                app.add_systems(Update, game_system_a);
            }
        }
        let game = TypeOwner::Game(7);
        let mut app = App::new();
        app.with_owner(game, |app| {
            app.add_plugins(ReloadedPlugin);
        });
        assert!(app.has_plugin(std::any::type_name::<ReloadedPlugin>()));
        // Without forgetting, the second add is skipped as a duplicate.
        app.forget_owner_plugins(TypeOwner::Game(8));
        assert!(app.has_plugin(std::any::type_name::<ReloadedPlugin>()));
        app.forget_owner_plugins(game);
        assert!(!app.has_plugin(std::any::type_name::<ReloadedPlugin>()));
        app.with_owner(TypeOwner::Game(8), |app| {
            app.add_plugins(ReloadedPlugin);
        });
        assert!(app.has_plugin(std::any::type_name::<ReloadedPlugin>()));
    }

    #[test]
    fn forgetting_an_owner_keeps_engine_plugins() {
        struct EnginePlugin;
        impl Plugin for EnginePlugin {
            fn build(&self, _app: &mut App) {}
        }
        let mut app = App::new();
        app.add_plugins(EnginePlugin);
        app.forget_owner_plugins(TypeOwner::Engine);
        assert!(app.has_plugin(std::any::type_name::<EnginePlugin>()));
    }

    #[test]
    fn should_exit_reports_each_request_once() {
        let mut app = App::new();
        assert_eq!(app.should_exit(), None);
        app.exit();
        assert_eq!(app.should_exit(), Some(AppExit::Success));
        assert_eq!(app.should_exit(), None);
    }

    #[test]
    fn exit_survives_frames_until_polled_and_error_wins() {
        let mut app = App::new();
        app.exit();
        app.update();
        app.update();
        app.update();
        assert_eq!(app.should_exit(), Some(AppExit::Success));
        assert_eq!(app.should_exit(), None);
        let code = NonZeroU8::new(3).expect("non-zero");
        app.world_mut().write_message(AppExit::Success);
        app.world_mut().write_message(AppExit::Error(code));
        app.world_mut().write_message(AppExit::Success);
        assert_eq!(app.should_exit(), Some(AppExit::Error(code)));
    }

    #[derive(Resource, Default)]
    struct Finished(Vec<&'static str>);

    struct Parent;
    struct Child;

    impl Plugin for Parent {
        fn build(&self, _app: &mut App) {}
        fn finish(&self, app: &mut App) {
            app.add_plugins(Child);
        }
        fn cleanup(&self, app: &mut App) {
            if let Some(mut finished) = app.world_mut().get_resource_mut::<Finished>() {
                finished.0.push("parent_cleanup");
            }
        }
    }
    impl Plugin for Child {
        fn build(&self, _app: &mut App) {}
        fn finish(&self, app: &mut App) {
            app.world_mut().resource_mut::<Finished>().0.push("child_finish");
        }
        fn cleanup(&self, app: &mut App) {
            app.world_mut().resource_mut::<Finished>().0.push("child_cleanup");
        }
    }

    #[test]
    fn plugin_added_from_finish_is_finished_and_kept() {
        let mut app = App::new();
        app.init_resource::<Finished>();
        app.add_plugins(Parent);
        app.finish_plugins();
        assert_eq!(
            app.world().resource::<Finished>().0,
            ["child_finish", "parent_cleanup", "child_cleanup"],
            "every finish runs before any cleanup"
        );
        assert!(app.has_plugin(std::any::type_name::<Child>()));
        app.add_plugins(Child);
        assert_eq!(
            app.world().resource::<Finished>().0.len(),
            3,
            "duplicate still skipped"
        );
    }

    /// Time banked from an earlier frame runs as a step whose clock is behind the frame's, never
    /// ahead of it, and the fixed clock advances by exactly one step per run across frames.
    #[test]
    fn fixed_clock_never_runs_past_the_frame_clock() {
        #[derive(Resource, Default)]
        struct Seen(Vec<Duration>);
        let mut app = App::new();
        app.init_resource::<Seen>();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(30)));
        app.add_systems(FixedUpdate, |time: Res<Time>, mut seen: ResMut<Seen>| {
            seen.0.push(time.elapsed());
        });
        let step = app.world().resource::<FixedTime>().step();
        let mut checked = 0;
        for _ in 0..6 {
            app.update();
            let frame_elapsed = app.world().resource::<Time>().elapsed();
            let seen = &app.world().resource::<Seen>().0;
            for elapsed in &seen[checked..] {
                assert!(*elapsed <= frame_elapsed, "{elapsed:?} > frame {frame_elapsed:?}");
            }
            checked = seen.len();
        }
        let seen = &app.world().resource::<Seen>().0;
        assert!(seen.len() >= 6, "{seen:?}");
        for (i, elapsed) in seen.iter().enumerate() {
            let n = u32::try_from(i + 1).expect("few steps");
            assert_eq!(*elapsed, step * n, "step {i}");
        }
    }

    #[test]
    fn fixed_update_sees_one_step_as_delta() {
        #[derive(Resource, Default)]
        struct Seen(Vec<(Duration, Duration)>);
        let mut app = App::new();
        app.init_resource::<Seen>();
        // Exactly two steps: `from_secs_f64(2.0 / 60.0)` rounds to 1 ns less than two rounded
        // 1/60 s steps, which runs one step.
        let step = app.world().resource::<FixedTime>().step();
        let frame = step * 2;
        app.insert_resource(TimeUpdateStrategy::ManualDuration(frame));
        app.add_systems(FixedUpdate, |time: Res<Time>, mut seen: ResMut<Seen>| {
            seen.0.push((time.delta(), time.elapsed()));
        });
        app.update();
        let seen = &app.world().resource::<Seen>().0;
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0], (step, step));
        assert_eq!(seen[1], (step, step * 2));
        let time = app.world().resource::<Time>();
        assert_eq!(time.delta(), frame, "frame delta restored");
        assert_eq!(time.elapsed(), frame);
    }

    /// A `Time::set_clocks` forward jump between frames (a render-out job handing the clocks back
    /// after its run, ADR-1881) must re-anchor the fixed clock: from the next frame on,
    /// `FixedUpdate` and `Update` read the same `Time::elapsed()` within one step. Without the
    /// re-anchor the fixed loop kept starting from the stale `last_step_end` and lagged the frame
    /// clock by the jump for the rest of the session (fixrev2 review, finding 1).
    #[test]
    fn a_forward_set_clocks_jump_reanchors_the_fixed_clock() {
        #[derive(Resource, Default)]
        struct Seen {
            fixed: Option<Duration>,
            update: Option<Duration>,
        }
        let mut app = App::new();
        app.init_resource::<Seen>();
        let step = app.world().resource::<FixedTime>().step();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(step));
        app.add_systems(FixedUpdate, |time: Res<Time>, mut seen: ResMut<Seen>| {
            seen.fixed = Some(time.elapsed());
        });
        app.add_systems(Update, |time: Res<Time>, mut seen: ResMut<Seen>| {
            seen.update = Some(time.elapsed());
        });
        for _ in 0..3 {
            app.update();
        }
        // The job ran the clocks from zero for a few seconds while the 600 s session waited;
        // `finish` adds the saved session time back on top of the job's run.
        let jump = Duration::from_secs(600);
        let (game, sim) = {
            let time = app.world().resource::<Time>();
            (time.elapsed() + jump, time.sim_elapsed() + jump)
        };
        app.world_mut().resource_mut::<Time>().set_clocks(game, sim);
        for _ in 0..3 {
            app.update();
            let frame = app.world().resource::<Time>().elapsed();
            let seen = app.world().resource::<Seen>();
            let fixed = seen.fixed.expect("a fixed step ran after the jump");
            let update = seen.update.expect("Update ran after the jump");
            assert_eq!(update, frame, "Update reads the frame clock");
            assert!(
                frame.saturating_sub(fixed) <= step,
                "fixed clock {fixed:?} lags the frame clock {frame:?} by more than a step"
            );
        }
    }
}
