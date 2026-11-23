// SPDX-License-Identifier: MPL-2.0
//! Application lifecycle (`docs/specs/11-platform-and-input.md` § 2).

use regia_app::prelude::*;

/// A lifecycle transition. Written as a message and also stored in [`AppLifecycle::last_event`] while
/// the [`Lifecycle`] schedule runs for it.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleEvent {
    /// First resume: windows exist, rendering may start. Always followed by `Resumed`.
    Started,
    /// The OS took the window away (Android: home button, screen off). Surfaces must be dropped.
    Suspended,
    /// The window is back (and on the first resume, right after `Started`).
    Resumed,
    /// The OS asks the app to free memory.
    LowMemory,
    /// The app is exiting.
    WillTerminate,
}

/// Where the app is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LifecycleState {
    /// Before the first resume.
    #[default]
    NotStarted,
    /// Windows exist and frames run.
    Running,
    /// Suspended by the OS: no surfaces, no frames.
    Suspended,
    /// Exiting.
    Terminating,
}

/// Current lifecycle state, plus the transition being processed while [`Lifecycle`] runs.
#[derive(Resource, Debug, Clone, Default)]
pub struct AppLifecycle {
    /// Current state.
    pub state: LifecycleState,
    /// The transition the [`Lifecycle`] schedule is running for (`None` outside it).
    pub last_event: Option<LifecycleEvent>,
    /// Number of `Resumed` transitions so far.
    pub resume_count: u64,
}

/// Pure state machine turning winit callbacks into [`LifecycleEvent`]s. winit may repeat
/// `resumed`/`suspended`; repeats produce nothing.
#[derive(Debug, Clone, Default)]
pub struct LifecycleTracker {
    state: LifecycleState,
}

impl LifecycleTracker {
    /// Current state.
    pub fn state(&self) -> LifecycleState {
        self.state
    }

    /// winit `resumed`.
    pub fn resumed(&mut self) -> Vec<LifecycleEvent> {
        match self.state {
            LifecycleState::NotStarted => {
                self.state = LifecycleState::Running;
                vec![LifecycleEvent::Started, LifecycleEvent::Resumed]
            }
            LifecycleState::Suspended => {
                self.state = LifecycleState::Running;
                vec![LifecycleEvent::Resumed]
            }
            LifecycleState::Running | LifecycleState::Terminating => Vec::new(),
        }
    }

    /// winit `suspended`.
    pub fn suspended(&mut self) -> Vec<LifecycleEvent> {
        if self.state == LifecycleState::Running {
            self.state = LifecycleState::Suspended;
            vec![LifecycleEvent::Suspended]
        } else {
            Vec::new()
        }
    }

    /// winit `memory_warning`.
    pub fn memory_warning(&mut self) -> Vec<LifecycleEvent> {
        if self.state == LifecycleState::Terminating {
            Vec::new()
        } else {
            vec![LifecycleEvent::LowMemory]
        }
    }

    /// winit `exiting`.
    pub fn exiting(&mut self) -> Vec<LifecycleEvent> {
        if self.state == LifecycleState::Terminating {
            return Vec::new();
        }
        let mut events = Vec::new();
        if self.state == LifecycleState::Running {
            events.push(LifecycleEvent::Suspended);
        }
        self.state = LifecycleState::Terminating;
        events.push(LifecycleEvent::WillTerminate);
        events
    }
}

/// Applies one transition to the world: state resource, gameplay-time pause, message, and the
/// [`Lifecycle`] schedule. Logs the marker line `regia lifecycle: <event>` that test tooling waits for.
pub fn apply_transition(app: &mut App, event: LifecycleEvent) {
    tracing::info!(target: "regia_platform", "regia lifecycle: {event:?}");
    let world = app.world_mut();
    {
        let mut lifecycle = world.resource_mut::<AppLifecycle>();
        lifecycle.last_event = Some(event);
        // `LowMemory` can arrive in any state (Android sends it before the first resume and while
        // suspended); it never moves the state machine.
        lifecycle.state = match event {
            LifecycleEvent::Started | LifecycleEvent::Resumed => LifecycleState::Running,
            LifecycleEvent::LowMemory => lifecycle.state,
            LifecycleEvent::Suspended => LifecycleState::Suspended,
            LifecycleEvent::WillTerminate => LifecycleState::Terminating,
        };
        if event == LifecycleEvent::Resumed {
            lifecycle.resume_count += 1;
        }
    }
    match event {
        LifecycleEvent::Suspended => world.resource_mut::<Time>().set_suspended(true),
        LifecycleEvent::Resumed => {
            world.resource_mut::<Time>().set_suspended(false);
            world.resource_mut::<RealTime>().reset_clock();
        }
        _ => {}
    }
    world.write_message(event);
    app.run_lifecycle();
    app.world_mut().resource_mut::<AppLifecycle>().last_event = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use LifecycleEvent::*;

    #[test]
    fn first_resume_emits_started_and_resumed() {
        let mut t = LifecycleTracker::default();
        assert_eq!(t.resumed(), [Started, Resumed]);
        assert_eq!(t.state(), LifecycleState::Running);
    }

    #[test]
    fn suspend_resume_cycles_and_ignores_repeats() {
        let mut t = LifecycleTracker::default();
        t.resumed();
        for _ in 0..50 {
            assert_eq!(t.suspended(), [Suspended]);
            assert_eq!(t.suspended(), [], "duplicate suspended");
            assert_eq!(t.resumed(), [Resumed]);
            assert_eq!(t.resumed(), [], "duplicate resumed");
        }
    }

    #[test]
    fn memory_warning_and_exit() {
        let mut t = LifecycleTracker::default();
        t.resumed();
        assert_eq!(t.memory_warning(), [LowMemory]);
        assert_eq!(t.exiting(), [Suspended, WillTerminate]);
        assert_eq!(t.exiting(), []);
        assert_eq!(t.resumed(), []);
    }

    #[test]
    fn low_memory_keeps_the_state_it_found() {
        let mut app = App::new();
        app.init_resource::<AppLifecycle>()
            .add_message::<LifecycleEvent>();
        app.add_systems(Lifecycle, || {});
        apply_transition(&mut app, LowMemory);
        assert_eq!(
            app.world().resource::<AppLifecycle>().state,
            LifecycleState::NotStarted
        );
    }

    #[derive(Resource, Default)]
    struct Seen(Vec<(LifecycleEvent, LifecycleState)>);

    #[test]
    fn transitions_run_lifecycle_schedule_and_pause_time() {
        let mut app = App::new();
        app.init_resource::<AppLifecycle>()
            .init_resource::<Seen>()
            .add_message::<LifecycleEvent>();
        app.add_systems(Lifecycle, |l: Res<AppLifecycle>, mut seen: ResMut<Seen>| {
            if let Some(e) = l.last_event {
                seen.0.push((e, l.state));
            }
        });
        let mut t = LifecycleTracker::default();
        for e in t.resumed().into_iter().chain(t.suspended()) {
            apply_transition(&mut app, e);
        }
        assert!(app.world().resource::<Time>().is_suspended());
        for e in t.resumed() {
            apply_transition(&mut app, e);
        }
        let world = app.world();
        assert!(!world.resource::<Time>().is_suspended());
        assert_eq!(world.resource::<AppLifecycle>().resume_count, 2);
        assert_eq!(
            world.resource::<Seen>().0,
            [
                (Started, LifecycleState::Running),
                (Resumed, LifecycleState::Running),
                (Suspended, LifecycleState::Suspended),
                (Resumed, LifecycleState::Running)
            ]
        );
    }

    #[test]
    fn low_memory_never_changes_the_state() {
        let mut app = App::new();
        app.init_resource::<AppLifecycle>()
            .add_message::<LifecycleEvent>();
        apply_transition(&mut app, LowMemory);
        assert_eq!(
            app.world().resource::<AppLifecycle>().state,
            LifecycleState::NotStarted,
            "a low-memory warning before the first resume must not start the app"
        );
        apply_transition(&mut app, Started);
        apply_transition(&mut app, Suspended);
        apply_transition(&mut app, LowMemory);
        assert_eq!(
            app.world().resource::<AppLifecycle>().state,
            LifecycleState::Suspended
        );
        apply_transition(&mut app, WillTerminate);
        apply_transition(&mut app, LowMemory);
        assert_eq!(
            app.world().resource::<AppLifecycle>().state,
            LifecycleState::Terminating
        );
    }
}
