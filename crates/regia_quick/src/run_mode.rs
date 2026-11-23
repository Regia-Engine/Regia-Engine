// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from nannou (https://github.com/nannou-org/nannou, commit bffa988),
// nannou/src/app.rs (RunMode, apply_loop_once). Copyright (c) 2017 MindBuffer, Copyright (c) 2019 nannou-org; dual-licensed
// MIT OR Apache-2.0, used here under the MIT licence (see docs/ported-sources.toml).
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software
// and associated documentation files (the "Software"), to deal in the Software without restriction,
// including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all copies or substantial
// portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT
// LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
// NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
// WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
// SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
//! How often a quick app runs its `update` and `view`: [`RunMode`] and the per-frame plan.
//!
//! Regia Engine's runner drives frames continuously (it returns to the OS each frame), so a run mode does not
//! change the event loop. It gates the quick app's `update`/`view` systems and, on frames where they do
//! not run, sets [`DrawFrozen`] so the last frame's drawing stays on screen and lowers [`FramePacing`]
//! so the idle window costs almost nothing. The window keeps processing events, so it stays closable
//! and resizable in every mode.

use regia_app::prelude::*;
use regia_ecs::ecs::system::SystemParam;
use regia_platform::{
    CursorMoved, KeyboardInput, MouseButtonInput, MouseWheel, TouchInput, WindowResized,
    WindowScaleFactorChanged,
};

use crate::DrawFrozen;

/// Controls the behaviour of a quick app's loop.
#[derive(Resource, Reflect, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[reflect(Resource, Default)]
pub enum RunMode {
    /// Run `update` and `view` every frame until the app exits.
    #[default]
    Loop,
    /// Run `update` and `view` on the first frame and then only on frames that received input
    /// (keys, mouse buttons, cursor motion, wheel, touch), a window resize, or a scale-factor
    /// change. In between, the last frame stays on screen and the loop idles at [`WAIT_IDLE_FPS`].
    Wait,
    /// Run `update` and `view` a fixed number of times, then hold the last frame on screen while the
    /// window idles at [`FROZEN_IDLE_FPS`].
    ///
    /// Input and window events keep being processed (the window stays closable and resizable); they
    /// just no longer run `update`/`view`. Because `view` runs a fixed number of times, a sketch whose
    /// view reads the live time or mouse is genuinely frozen. To run `n` frames and then *quit*, call
    /// [`Quick::quit`](crate::Quick::quit) from `update` instead.
    LoopNTimes(u64),
}

impl RunMode {
    /// Run `update` and `view` once, then hold that frame on screen.
    pub fn loop_once() -> Self {
        RunMode::LoopNTimes(1)
    }

    /// Run `update` and `view` `n` times, then hold the last frame on screen.
    pub fn loop_ntimes(n: u64) -> Self {
        RunMode::LoopNTimes(n)
    }
}

/// Frame rate a [`RunMode::Wait`] app idles at between inputs (bounds input latency to one idle frame).
pub const WAIT_IDLE_FPS: f32 = 30.0;
/// Frame rate a frozen [`RunMode::LoopNTimes`] app idles at (enough to stay responsive to closing).
pub const FROZEN_IDLE_FPS: f32 = 10.0;

/// What the quick loop decided this frame, and what it has done so far.
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct QuickLoop {
    /// Frames on which `update`/`view` ran since the run mode was last set.
    pub runs: u64,
    /// `update`/`view` run this frame.
    pub run_this_frame: bool,
    /// `LoopNTimes` budget spent: the drawing is held and nothing runs until the mode changes.
    pub frozen: bool,
    /// The run mode the counters belong to; a different mode resets them.
    mode: Option<RunMode>,
    /// The app's own frame pacing while idling is in effect, restored when frames run again.
    saved_pacing: Option<Option<f32>>,
}

impl QuickLoop {
    /// Forgets the runs counted for the current mode (the next plan starts the mode afresh).
    pub(crate) fn restart(&mut self) {
        *self = QuickLoop {
            saved_pacing: self.saved_pacing.take(),
            ..QuickLoop::default()
        };
    }
}

/// True when the quick app's `update`/`view` run this frame (a run condition).
pub fn quick_should_run(state: Res<QuickLoop>) -> bool {
    state.run_this_frame
}

/// Window and input messages that wake a [`RunMode::Wait`] app.
#[derive(SystemParam)]
pub struct InputActivity<'w, 's> {
    keys: MessageReader<'w, 's, KeyboardInput>,
    buttons: MessageReader<'w, 's, MouseButtonInput>,
    cursor: MessageReader<'w, 's, CursorMoved>,
    wheel: MessageReader<'w, 's, MouseWheel>,
    touch: MessageReader<'w, 's, TouchInput>,
    resized: MessageReader<'w, 's, WindowResized>,
    scale: MessageReader<'w, 's, WindowScaleFactorChanged>,
}

impl InputActivity<'_, '_> {
    /// True if any wake-up message arrived since this system last ran (all readers are drained).
    fn any(&mut self) -> bool {
        // Count every reader so each one's cursor advances.
        let n = self.keys.read().count()
            + self.buttons.read().count()
            + self.cursor.read().count()
            + self.wheel.read().count()
            + self.touch.read().count()
            + self.resized.read().count()
            + self.scale.read().count();
        n > 0
    }
}

/// `Update`, before the quick app's systems: decides whether `update`/`view` run this frame, holds the
/// drawing on frames where they do not, and idles the loop while nothing runs.
#[tracing::instrument(target = "regia_quick", level = "trace", skip_all)]
pub fn plan_quick_frame(
    mode: Res<RunMode>,
    mut state: ResMut<QuickLoop>,
    mut activity: InputActivity,
    mut frozen: ResMut<DrawFrozen>,
    pacing: Option<ResMut<FramePacing>>,
) {
    let mode = *mode;
    if state.mode != Some(mode) {
        state.mode = Some(mode);
        state.runs = 0;
        state.frozen = false;
    }
    let woke = activity.any();
    let run = match mode {
        RunMode::Loop => true,
        RunMode::Wait => state.runs == 0 || woke,
        RunMode::LoopNTimes(n) => state.runs < n,
    };
    state.run_this_frame = run;
    if run {
        state.runs += 1;
    }
    if let RunMode::LoopNTimes(n) = mode {
        state.frozen = state.runs >= n && !run;
    }
    // Keep the last frame's drawing on screen whenever `view` does not run.
    if frozen.0 == run {
        frozen.0 = !run;
    }
    let idle_fps = match mode {
        RunMode::Loop => None,
        RunMode::Wait => Some(WAIT_IDLE_FPS),
        RunMode::LoopNTimes(_) => Some(FROZEN_IDLE_FPS),
    };
    if let Some(mut pacing) = pacing {
        match (run, idle_fps) {
            (false, Some(fps)) => {
                if state.saved_pacing.is_none() {
                    state.saved_pacing = Some(pacing.target_fps);
                    let idle = pacing.target_fps.map_or(fps, |own| own.min(fps));
                    *pacing = FramePacing::new(Some(idle));
                }
            }
            _ => {
                if let Some(own) = state.saved_pacing.take() {
                    *pacing = FramePacing::new(own);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(mode: RunMode) -> App {
        let mut app = App::new();
        app.add_message::<KeyboardInput>()
            .add_message::<MouseButtonInput>()
            .add_message::<CursorMoved>()
            .add_message::<MouseWheel>()
            .add_message::<TouchInput>()
            .add_message::<WindowResized>()
            .add_message::<WindowScaleFactorChanged>()
            .insert_resource(mode)
            .init_resource::<QuickLoop>()
            .init_resource::<DrawFrozen>()
            .insert_resource(FramePacing::new(None))
            .add_systems(Update, plan_quick_frame);
        app
    }

    fn ran(app: &App) -> bool {
        app.world().resource::<QuickLoop>().run_this_frame
    }

    #[test]
    fn loop_runs_every_frame_and_never_freezes() {
        let mut app = app(RunMode::Loop);
        for _ in 0..5 {
            app.update();
            assert!(ran(&app));
            assert!(!app.world().resource::<DrawFrozen>().0);
        }
        assert_eq!(app.world().resource::<FramePacing>().target_fps, None);
    }

    #[test]
    fn loop_n_times_runs_n_frames_then_holds_and_idles() {
        let mut app = app(RunMode::loop_ntimes(3));
        let mut runs = 0;
        for _ in 0..10 {
            app.update();
            runs += u32::from(ran(&app));
        }
        assert_eq!(runs, 3);
        let state = app.world().resource::<QuickLoop>();
        assert!(state.frozen);
        assert!(app.world().resource::<DrawFrozen>().0);
        assert_eq!(
            app.world().resource::<FramePacing>().target_fps,
            Some(FROZEN_IDLE_FPS)
        );
        // Setting a new mode resets the budget and restores the app's pacing.
        app.insert_resource(RunMode::Loop);
        app.update();
        assert!(ran(&app));
        assert!(!app.world().resource::<DrawFrozen>().0);
        assert_eq!(app.world().resource::<FramePacing>().target_fps, None);
    }

    #[test]
    fn wait_runs_on_the_first_frame_and_on_input_only() {
        let mut app = app(RunMode::Wait);
        app.update();
        assert!(ran(&app), "first frame always runs");
        app.update();
        assert!(!ran(&app));
        assert!(app.world().resource::<DrawFrozen>().0, "the last frame is held");
        app.world_mut().write_message(CursorMoved {
            window: Entity::PLACEHOLDER,
            position: (3.0, 4.0),
            timestamp: regia_platform::InputTimestamp::ZERO,
        });
        app.update();
        assert!(ran(&app), "input wakes the app");
        assert!(!app.world().resource::<DrawFrozen>().0);
        app.update();
        assert!(!ran(&app));
        app.world_mut().write_message(WindowScaleFactorChanged {
            window: Entity::PLACEHOLDER,
            scale_factor: 2.0,
        });
        app.update();
        assert!(ran(&app), "a scale-factor change wakes the app");
        app.update();
        assert!(!ran(&app));
    }

    #[test]
    fn idling_never_raises_a_slower_app_frame_cap() {
        let mut app = app(RunMode::loop_once());
        app.insert_resource(FramePacing::new(Some(5.0)));
        app.update();
        app.update();
        assert_eq!(app.world().resource::<FramePacing>().target_fps, Some(5.0));
    }

    fn cursor_moved() -> CursorMoved {
        CursorMoved {
            window: Entity::PLACEHOLDER,
            position: (1.0, 2.0),
            timestamp: regia_platform::InputTimestamp::ZERO,
        }
    }

    fn key_pressed() -> KeyboardInput {
        KeyboardInput {
            window: Entity::PLACEHOLDER,
            physical_key: "KeyA".to_string(),
            logical_key: "a".to_string(),
            state: regia_platform::ButtonState::Pressed,
            repeat: false,
            timestamp: regia_platform::InputTimestamp::ZERO,
        }
    }

    fn fps(app: &App) -> Option<f32> {
        app.world().resource::<FramePacing>().target_fps
    }

    #[test]
    fn frozen_idle_caps_an_uncapped_or_faster_app_and_restores_it() {
        for own in [None, Some(60.0)] {
            let mut app = app(RunMode::loop_once());
            app.insert_resource(FramePacing::new(own));
            app.update();
            assert!(ran(&app));
            assert_eq!(fps(&app), own, "a running frame leaves the app's pacing alone");
            app.update();
            assert!(!ran(&app));
            assert_eq!(
                fps(&app),
                Some(FROZEN_IDLE_FPS),
                "own cap {own:?} idles at the frozen rate"
            );
            // A new mode runs again and gives the app its own cap back.
            app.insert_resource(RunMode::Loop);
            app.update();
            assert!(ran(&app));
            assert_eq!(fps(&app), own);
        }
    }

    #[test]
    fn wait_idles_at_its_own_rate_between_inputs() {
        let mut app = app(RunMode::Wait);
        app.update();
        assert_eq!(fps(&app), None, "the first frame runs");
        app.update();
        assert!(!ran(&app));
        assert_eq!(fps(&app), Some(WAIT_IDLE_FPS));
        app.world_mut().write_message(cursor_moved());
        app.update();
        assert!(ran(&app));
        assert_eq!(fps(&app), None, "input restores the app's pacing");
    }

    #[test]
    fn input_does_not_wake_a_spent_loop_n_times() {
        let mut app = app(RunMode::loop_ntimes(2));
        for _ in 0..4 {
            app.update();
        }
        assert!(!ran(&app));
        app.world_mut().write_message(cursor_moved());
        app.update();
        assert!(!ran(&app), "cursor motion after the budget runs nothing");
        app.world_mut().write_message(key_pressed());
        app.update();
        assert!(!ran(&app), "a key press after the budget runs nothing");
        assert_eq!(app.world().resource::<QuickLoop>().runs, 2);
    }

    #[test]
    fn wait_wakes_on_a_key_press_too() {
        let mut app = app(RunMode::Wait);
        app.update();
        app.update();
        assert!(!ran(&app));
        app.world_mut().write_message(key_pressed());
        app.update();
        assert!(ran(&app), "keyboard input wakes the app");
        app.update();
        assert!(!ran(&app), "one wake runs one frame");
    }
}
