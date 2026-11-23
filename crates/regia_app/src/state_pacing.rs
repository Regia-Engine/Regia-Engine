// SPDX-License-Identifier: MPL-2.0
//! Frame caps per app state (ADR-7889, `book/agents/frame-pacing.md`).
//!
//! A menu, a loading page or a paused scene costs little, so without a cap it runs at hundreds of
//! frames per second and keeps the GPU busy for nothing. [`PacingCaps`] holds one optional cap per
//! [`PacingState`]; the host (or `regia_framework`'s `GameFlowPlugin`, which maps its
//! `GameFlow` states) sets the [`PacingState`]. Each frame, in `Last`, the app copies the
//! current state's cap into [`FramePacing::state_fps`]; the runner then paces at the tighter of
//! that and the player's cap ([`FramePacing::target_fps`], which the settings apply owns), so a
//! settings change never removes a state cap and a state change never overrides a lower player cap.
//!
//! Neither resource is built in: a host inserts them (`GameFlowPlugin` inserts the state on its
//! first transition, `app.set_pacing_state` inserts what it sets). Absent, or with every cap at
//! its default `None`, pacing is as before.

use regia_ecs::prelude::*;

use crate::time::FramePacing;

/// What kind of frame the app is drawing, for [`PacingCaps`]. Hosts set it on their own state
/// changes; with `regia_framework`'s `GameFlowPlugin` it follows `GameFlow` (MainMenu is
/// [`PacingState::Menu`], Boot and Loading are [`PacingState::Loading`], Paused is
/// [`PacingState::Paused`], Playing is [`PacingState::Play`]).
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PacingState {
    /// In play (default).
    #[default]
    Play,
    /// In play, paused.
    Paused,
    /// A title, options or other front-end menu.
    Menu,
    /// A loading page (also start-up).
    Loading,
}

impl PacingState {
    /// Every state, in protocol order.
    pub const ALL: [Self; 4] = [Self::Play, Self::Paused, Self::Menu, Self::Loading];

    /// Stable name: `"play"`, `"paused"`, `"menu"` or `"loading"`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Play => "play",
            Self::Paused => "paused",
            Self::Menu => "menu",
            Self::Loading => "loading",
        }
    }

    /// The state named `name` ([`PacingState::name`]), if any.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.name() == name)
    }
}

/// One optional frame cap (frames per second) per [`PacingState`]. `None` leaves that state at
/// the player's cap (or uncapped). Defaults to `None` everywhere.
///
/// The effective cap is the tighter of this state's cap and the player's
/// [`FramePacing::target_fps`]. The caps apply on every runner path the player cap applies to:
/// with vsync off, on the display pacer, and with frame generation on (the cap paces rendered
/// frames; generated frames are presented between them while the rendered rate is above frame
/// generation's floor).
#[derive(Resource, Debug, Clone, Copy, PartialEq, Default)]
pub struct PacingCaps {
    /// Cap while [`PacingState::Play`].
    pub play: Option<f32>,
    /// Cap while [`PacingState::Paused`].
    pub paused: Option<f32>,
    /// Cap while [`PacingState::Menu`].
    pub menu: Option<f32>,
    /// Cap while [`PacingState::Loading`].
    pub loading: Option<f32>,
}

impl PacingCaps {
    /// The cap for `state`.
    pub fn cap(&self, state: PacingState) -> Option<f32> {
        match state {
            PacingState::Play => self.play,
            PacingState::Paused => self.paused,
            PacingState::Menu => self.menu,
            PacingState::Loading => self.loading,
        }
    }

    /// Sets the cap for `state`.
    pub fn set(&mut self, state: PacingState, fps: Option<f32>) {
        match state {
            PacingState::Play => self.play = fps,
            PacingState::Paused => self.paused = fps,
            PacingState::Menu => self.menu = fps,
            PacingState::Loading => self.loading = fps,
        }
    }
}

/// `Last`: copies the current [`PacingState`]'s cap from [`PacingCaps`] into
/// [`FramePacing::state_fps`]. A replaced `FramePacing` (a host that inserts a new one) gets its
/// state cap back on the same frame. Writes only on a change, so `FramePacing` change detection
/// stays quiet.
pub(crate) fn sync_state_pacing(
    caps: Option<Res<PacingCaps>>,
    state: Option<Res<PacingState>>,
    pacing: Option<ResMut<FramePacing>>,
) {
    let Some(mut pacing) = pacing else {
        return;
    };
    let fps = match (caps, state) {
        (Some(caps), Some(state)) => caps.cap(*state),
        _ => None,
    };
    if pacing.state_fps() != fps {
        pacing.set_state_fps(fps);
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::time::PacingClock;
    use crate::{App, LoopSettings};

    /// A clock that only moves when told: each frame costs `work`, and a sleep jumps to its
    /// deadline exactly.
    struct FakeClock {
        now: Instant,
    }

    impl PacingClock for FakeClock {
        fn now(&mut self) -> Instant {
            self.now
        }

        fn sleep_until(&mut self, deadline: Instant) {
            self.now = self.now.max(deadline);
        }
    }

    /// Runs `frames` frames of `work` each on `clock` and returns the waits after the first frame
    /// (the first starts the deadline chain and waits nothing).
    fn paced_waits(app: &mut App, clock: &mut FakeClock, frames: usize, work: Duration) -> Vec<Duration> {
        let mut waits = Vec::new();
        for i in 0..frames {
            app.update();
            clock.now += work;
            let waited = app.pace_frame_with(clock);
            if i > 0 {
                waits.push(waited);
            }
        }
        waits
    }

    fn near(a: Duration, b: Duration) -> bool {
        a.abs_diff(b) <= Duration::from_micros(2)
    }

    #[test]
    fn a_menu_cap_paces_menu_frames_at_its_period() {
        let mut app = App::new();
        app.insert_resource(PacingCaps {
            menu: Some(60.0),
            ..PacingCaps::default()
        });
        app.insert_resource(PacingState::Menu);
        let mut clock = FakeClock { now: Instant::now() };
        let work = Duration::from_millis(3);
        let start = clock.now;
        let waits = paced_waits(&mut app, &mut clock, 61, work);
        let period = Duration::from_secs_f64(1.0 / 60.0);
        assert_eq!(app.world().resource::<FramePacing>().period(), Some(period));
        for (i, wait) in waits.iter().enumerate() {
            assert!(
                near(*wait, period - work),
                "frame {i}: waited {wait:?}, want {:?}",
                period - work
            );
        }
        // 60 periods after the first frame's end.
        let span = clock.now - start - work;
        assert!(near(span, period * 60), "60 menu frames took {span:?}");
    }

    #[test]
    fn play_keeps_the_player_cap_and_the_tighter_cap_wins() {
        let mut app = App::new();
        app.insert_resource(PacingCaps {
            menu: Some(60.0),
            paused: Some(30.0),
            ..PacingCaps::default()
        });
        app.world_mut().resource_mut::<FramePacing>().target_fps = Some(144.0);
        app.update();
        let pacing = app.world().resource::<FramePacing>();
        assert_eq!(pacing.effective_fps(), Some(144.0), "play has no state cap");
        app.insert_resource(PacingState::Paused);
        app.update();
        assert_eq!(app.world().resource::<FramePacing>().effective_fps(), Some(30.0));
        // A player cap below the state cap stays in force.
        app.world_mut().resource_mut::<FramePacing>().target_fps = Some(20.0);
        app.update();
        assert_eq!(app.world().resource::<FramePacing>().effective_fps(), Some(20.0));
        app.insert_resource(PacingState::Play);
        app.update();
        assert_eq!(app.world().resource::<FramePacing>().effective_fps(), Some(20.0));
        assert_eq!(app.world().resource::<FramePacing>().state_fps(), None);
    }

    #[test]
    fn rewriting_the_player_cap_keeps_the_state_cap() {
        let mut app = App::new();
        app.insert_resource(PacingCaps {
            menu: Some(60.0),
            ..PacingCaps::default()
        });
        app.insert_resource(PacingState::Menu);
        app.update();
        // What a settings apply with frame_cap 0 / None does, and a host that replaces the whole
        // resource.
        app.world_mut().resource_mut::<FramePacing>().target_fps = None;
        LoopSettings::read(app.world())
            .max_fps(None)
            .apply(app.world_mut());
        app.insert_resource(FramePacing::new(None));
        app.update();
        assert_eq!(app.world().resource::<FramePacing>().effective_fps(), Some(60.0));
        let mut clock = FakeClock { now: Instant::now() };
        let waits = paced_waits(&mut app, &mut clock, 5, Duration::from_millis(1));
        let period = Duration::from_secs_f64(1.0 / 60.0);
        for wait in waits {
            assert!(near(wait, period - Duration::from_millis(1)), "{wait:?}");
        }
    }

    #[test]
    fn no_caps_no_change() {
        let mut app = App::new();
        for state in PacingState::ALL {
            app.insert_resource(state);
            app.update();
            assert_eq!(app.world().resource::<FramePacing>().period(), None, "{state:?}");
        }
        let mut clock = FakeClock { now: Instant::now() };
        assert!(
            paced_waits(&mut app, &mut clock, 4, Duration::from_millis(1))
                .iter()
                .all(Duration::is_zero)
        );
    }

    #[test]
    fn invalid_state_caps_are_ignored_and_names_round_trip() {
        let mut caps = PacingCaps::default();
        for state in PacingState::ALL {
            assert_eq!(PacingState::from_name(state.name()), Some(state));
            caps.set(state, Some(f32::NAN));
            assert_eq!(crate::time::tighter_cap(Some(50.0), caps.cap(state)), Some(50.0));
            caps.set(state, Some(-3.0));
            assert_eq!(crate::time::tighter_cap(None, caps.cap(state)), None);
        }
        assert_eq!(PacingState::from_name("game"), None);
        assert_eq!(PacingState::default(), PacingState::Play);
    }
}
