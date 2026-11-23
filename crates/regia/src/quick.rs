// SPDX-License-Identifier: MPL-2.0
//! The quick layer: a game or sketch in one file (`regia_quick`, re-exported).
//!
//! ```no_run
//! use regia::quick::prelude::*;
//!
//! #[derive(Reflect, Default)]
//! struct Model {
//!     x: f32,
//! }
//!
//! fn main() {
//!     regia::quick::app(|_q: &Quick| Model::default())
//!         .update(|q: &Quick, m: &mut Model| m.x = (m.x + 100.0 * q.delta()) % 300.0)
//!         .view(|q: &Quick, m: &Model| {
//!             q.draw().background().color(Color::BLACK);
//!             q.draw().ellipse().x(m.x).radius(30.0).color(Color::WHITE);
//!         })
//!         .run();
//! }
//! ```
//!
//! [`RunQuick::run`] adds the engine's [`DefaultPlugins`](crate::DefaultPlugins), the input layer and
//! [`QuickPlugin`] around the builder. To grow past one file, add the builder to an ordinary app or
//! `GamePlugin` instead (`app.add_plugins(builder)`), and use [`Quick`] in normal systems
//! (`book/user-guide/sketch-to-project.md`).

pub use regia_quick::*;

use regia_app::prelude::*;

/// Runs a quick app builder as a desktop game (`.run()` on [`app`](fn@app) / [`sketch`](fn@sketch)).
pub trait RunQuick {
    /// Runs the app with the engine's default plugins until it exits.
    fn run(self) -> AppExit;
}

impl<M: QuickModel> RunQuick for Builder<M> {
    fn run(self) -> AppExit {
        crate::run((QuickEngine, self))
    }
}

/// The engine plugins a quick app needs on top of [`DefaultPlugins`](crate::DefaultPlugins): the input
/// layer (keys, mouse, touch for [`Quick`]) and [`QuickPlugin`] (the draw layer). Each is skipped when
/// the app already has it.
#[derive(Default)]
pub struct QuickEngine;

impl Plugin for QuickEngine {
    fn build(&self, app: &mut App) {
        if !app.world().contains_resource::<regia_input::Keyboard>() {
            app.add_plugins(regia_input::InputPlugin::default());
        }
        if !app.world().contains_resource::<Draw>() {
            app.add_plugins(QuickPlugin);
        }
    }
}

/// Everything a quick app imports: `use regia::quick::prelude::*;`.
pub mod prelude {
    pub use super::RunQuick;
    pub use regia_core::math::Rng;
    pub use regia_platform::{MouseButton, TouchPhase};
    pub use regia_quick::color::*;
    pub use regia_quick::prelude::*;
    pub use regia_quick::{Model, Quick, QuickSet, QuickTouch, RunMode, app, sketch};
}
