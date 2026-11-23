// SPDX-License-Identifier: MPL-2.0
//! App, Plugin, schedules, fixed timestep, time, frame driver (platform-agnostic).
//!
//! Specification: `docs/specs/10-core.md` § 3 and the frame loop in `docs/02-architecture.md` § 5.
//!
//! # How the pieces fit
//!
//! An [`App`] owns one ECS world, the frame `Schedules`, and the [`Plugin`]s that configured
//! them. A game builds an [`App`], calls [`App::add_plugins`], and hands it to a runner
//! ([`run_headless`] in tests and servers, or the winit runner the platform layer installs for a
//! window). Each [`App::update`] runs the schedules in frame order — `StateTransition` +
//! `Startup` (once) → `First` → `PreUpdate` → `StateTransition` → `FixedUpdate` ×N → `Update` →
//! `PostUpdate` → `Extract` → `Render` → `Last` — stepping [`FixedUpdate`] once per [`FixedTime`] tick and advancing [`Time`] each
//! frame. [`Plugin`]s add systems, resources and messages into those schedules during `build`;
//! their `finish`/`cleanup` run once the app is fully configured. [`Diagnostics`] samples frame
//! times for profiling (`book/user-guide/index.md`, `book/user-guide/gameplay.md`). Game
//! states (`App::init_state`, `OnEnter`/`OnExit`, `in_state`; `regia_ecs::state`) ride the
//! `StateTransition` schedule.
//!
//! # Getting started
//!
//! Build an [`App`], add systems to a schedule and step it. A window-less app like this one is how
//! tests and servers run; a game adds `regia::DefaultPlugins` and calls `regia::run` instead.
//!
//! ```
//! use regia_app::prelude::*;
//!
//! #[derive(Resource, Default)]
//! struct Frames(u32);
//!
//! fn count(mut frames: ResMut<Frames>) {
//!     frames.0 += 1;
//! }
//!
//! let mut app = App::new();
//! app.init_resource::<Frames>().add_systems(Update, count);
//! for _ in 0..3 {
//!     app.update();
//! }
//! assert_eq!(app.world().resource::<Frames>().0, 3);
//! assert_eq!(app.world().resource::<Time>().frame_count(), 3);
//! ```

mod app;
pub mod caption;
mod diagnostics;
mod executor;
mod loop_settings;
mod minimal;
mod phases;
mod plugin;
mod residency;
mod schedules;
pub mod shuffle;
mod state;
mod state_pacing;
mod time;

pub use app::{App, AppExit, run_headless};
pub use diagnostics::{
    Diagnostics, DiagnosticsConfig, DiagnosticsPlugin, MAX_WINDOW_FRAMES, update_diagnostics,
};
pub use executor::{ScheduleExecutor, ScheduleExecutorSetting};
pub use loop_settings::{
    DEFAULT_THROTTLED_FPS, LoopControl, LoopMode, LoopSettings, MAX_THROTTLED_FPS, MIN_THROTTLED_FPS,
};
pub use minimal::MinimalPlugins;
pub use phases::{FramePhases, PHASE_COUNT, PHASE_NAMES, Phase, RunnerState};
pub use plugin::{Plugin, Plugins};
pub use residency::{Residency, ResidencyPlugin};
pub use schedules::{
    Extract, First, FixedUpdate, FixedUpdateSet, Last, Lifecycle, OnLevelLoaded, OnLevelUnloading,
    PostUpdate, PostUpdateSet, PreUpdate, Render, Startup, Update,
};
pub use state::ReflectedState;
pub use state_pacing::{PacingCaps, PacingState};
pub use time::{
    DISCONTINUITY_GAP, FixedTime, FramePacing, MAX_DELTA, MAX_FIXED_HZ, MAX_PACING_FPS, MAX_SUBSTEPS_LIMIT,
    MAX_TIME_SCALE, MIN_PACING_FPS, RealTime, SPIN_THRESHOLD, SimStep, Time, TimeUpdateStrategy,
    enable_fixed_step, sleep_until,
};
pub use time::{PacingClock, SystemPacingClock, tighter_cap};

/// Common imports for code that builds apps and plugins.
pub mod prelude {
    pub use crate::{
        App, AppExit, Diagnostics, Extract, First, FixedTime, FixedUpdate, FixedUpdateSet, FramePacing, Last,
        Lifecycle, LoopControl, LoopMode, LoopSettings, MinimalPlugins, OnLevelLoaded, OnLevelUnloading,
        Plugin, Plugins, PostUpdate, PostUpdateSet, PreUpdate, RealTime, ReflectedState, Render, SimStep,
        Startup, Time, TimeUpdateStrategy, Update, enable_fixed_step,
    };
    pub use regia_ecs::prelude::*;
}
