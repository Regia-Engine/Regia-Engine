// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from nannou (https://github.com/nannou-org/nannou, commit bffa988),
// nannou/src/app.rs (Builder, ModelFn/UpdateFn/ViewFn, sketch). Copyright (c) 2017 MindBuffer, Copyright (c) 2019 nannou-org; dual-licensed
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
//! The quick app layer: [`sketch`] (one `view` function) and [`app`] (`model` → state, `update`,
//! `view`), built as plain systems on a normal Regia Engine [`App`].
//!
//! A quick app is a [`Plugin`]. `regia::quick::app(model).update(update).view(view).run()` runs it
//! with the engine's default plugins. The same builder can be added to any app (`app.add_plugins(...)`)
//! or registered from a `GamePlugin` (`export_game!`) for the editor and hot reload.
//!
//! The model is stored as the [`Model<M>`] resource. `M` must be [`Reflect`]: hot reload carries it
//! across a code edit, and the inspector and agent protocol (`world.query`, `expect`) can read it.
//!
//! **Order within a frame.** In `Update`, set [`QuickSet`]: [`plan_quick_frame`] decides whether this
//! frame runs (see [`RunMode`]), then `update`, then `view`. Systems of your own that read or write the
//! model can be ordered against [`QuickSet`].
//!
//! **The `model` function runs while the app is built**, before the window opens. [`Quick::window_rect`]
//! then reports the size the window was asked for (the builder's [`Builder::size`]).

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use regia_app::prelude::*;
use regia_ecs::ecs::system::SystemState;
use regia_platform::{
    CursorMoved, KeyboardInput, MouseButtonInput, MouseWheel, TouchInput, WindowResized,
    WindowScaleFactorChanged, WindowSettings,
};

use crate::run_mode::{QuickLoop, RunMode, plan_quick_frame, quick_should_run};
use crate::{DrawFrozen, Quick};

/// The user function that creates the model, once, when the app is built.
pub type ModelFn<M> = Box<dyn FnOnce(&Quick) -> M + Send + Sync>;
/// The user function that advances the model, once per frame that runs.
pub type UpdateFn<M> = Box<dyn FnMut(&Quick, &mut M) + Send + Sync>;
/// The user function that draws the model, once per frame that runs, after `update`.
pub type ViewFn<M> = Box<dyn FnMut(&Quick, &M) + Send + Sync>;
/// A sketch's view: no model.
pub type SketchViewFn = Box<dyn FnMut(&Quick) + Send + Sync>;

/// What a quick app's model must be: reflected (so hot reload and the agent protocol see it) and
/// shareable between threads. `#[derive(Reflect)]` on a plain struct is enough.
pub trait QuickModel:
    Reflect + FromReflect + TypePath + GetTypeRegistration + Typed + Send + Sync + 'static
{
}

impl<T> QuickModel for T where
    T: Reflect + FromReflect + TypePath + GetTypeRegistration + Typed + Send + Sync + 'static
{
}

/// The quick app's model, as a reflected resource.
///
/// Read it from your own systems with `Res<Model<MyModel>>`, or through the agent protocol as the
/// resource `regia_quick::app::Model<my_game::MyModel>` (field `model`).
#[derive(Resource, Reflect, Debug, Clone, PartialEq)]
#[reflect(Resource)]
pub struct Model<M: QuickModel> {
    /// The user's model.
    pub model: M,
}

/// The system set holding a quick app's `plan → update → view` chain, in `Update`.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct QuickSet;

/// The user functions, taken out of the builder when it is built (a plugin builds through `&self`).
struct Parts<M> {
    model: ModelFn<M>,
    update: Option<UpdateFn<M>>,
    view: Option<ViewFn<M>>,
}

/// Window and loop configuration of a quick app.
#[derive(Debug, Clone, Default)]
struct Config {
    title: Option<String>,
    size: Option<(u32, u32)>,
    run_mode: RunMode,
}

/// A quick app builder: a [`Plugin`] made of a `model`, an optional `update` and an optional `view`.
///
/// Created by [`app`] or [`sketch`]. Every function may be a `fn` item or a closure.
pub struct Builder<M: QuickModel> {
    parts: Mutex<Option<Parts<M>>>,
    config: Config,
    /// Unique per builder: a game that registers its quick app again after a hot reload must not be
    /// skipped as an already-added plugin.
    name: String,
}

/// Starts a quick app whose state is created by `model`.
///
/// ```
/// use regia_app::prelude::*;
/// use regia_quick::{Quick, app};
///
/// #[derive(Reflect, Default)]
/// struct Model {
///     x: f32,
/// }
///
/// let mut game = App::new();
/// game.add_plugins(
///     app(|_q: &Quick| Model::default())
///         .update(|q: &Quick, m: &mut Model| m.x += 60.0 * q.delta())
///         .view(|q: &Quick, m: &Model| {
///             q.draw().ellipse().x(m.x).radius(20.0);
///         }),
/// );
/// game.update();
/// ```
pub fn app<M: QuickModel>(model: impl FnOnce(&Quick) -> M + Send + Sync + 'static) -> Builder<M> {
    Builder {
        parts: Mutex::new(Some(Parts {
            model: Box::new(model),
            update: None,
            view: None,
        })),
        config: Config::default(),
        name: format!(
            "{}#{}",
            std::any::type_name::<Builder<M>>(),
            NEXT_BUILDER.fetch_add(1, Ordering::Relaxed)
        ),
    }
}

/// Numbers builders for their plugin names.
static NEXT_BUILDER: AtomicU64 = AtomicU64::new(0);

/// Starts a sketch: one `view` function and no model, for drawing something quickly.
///
/// ```
/// use regia_app::prelude::*;
/// use regia_quick::{Quick, sketch};
///
/// let mut game = App::new();
/// game.add_plugins(sketch(|q: &Quick| {
///     q.draw().ellipse().xy(q.mouse()).radius(30.0);
/// }));
/// game.update();
/// ```
pub fn sketch(mut view: impl FnMut(&Quick) + Send + Sync + 'static) -> Builder<()> {
    app(|_: &Quick| ()).view(move |q: &Quick, _: &()| view(q))
}

impl<M: QuickModel> Builder<M> {
    fn with_parts(self, f: impl FnOnce(&mut Parts<M>)) -> Self {
        if let Some(parts) = self.parts.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
            f(parts);
        }
        self
    }

    /// The function that advances the model, once per frame that runs (see [`RunMode`]).
    pub fn update(self, update: impl FnMut(&Quick, &mut M) + Send + Sync + 'static) -> Self {
        self.with_parts(|parts| parts.update = Some(Box::new(update)))
    }

    /// The function that draws the model, after `update`, once per frame that runs.
    pub fn view(self, view: impl FnMut(&Quick, &M) + Send + Sync + 'static) -> Self {
        self.with_parts(|parts| parts.view = Some(Box::new(view)))
    }

    /// The window's title.
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.config.title = Some(title.into());
        self
    }

    /// The window's size in logical units.
    pub fn size(mut self, width: u32, height: u32) -> Self {
        self.config.size = Some((width, height));
        self
    }

    /// The behaviour of the loop (default [`RunMode::Loop`]).
    pub fn run_mode(mut self, mode: RunMode) -> Self {
        self.config.run_mode = mode;
        self
    }

    /// Run `update`/`view` once, then hold the frame ([`RunMode::loop_once`]).
    pub fn loop_once(self) -> Self {
        self.run_mode(RunMode::loop_once())
    }

    /// Run `update`/`view` `n` times, then hold the last frame ([`RunMode::loop_ntimes`]).
    pub fn loop_ntimes(self, n: u64) -> Self {
        self.run_mode(RunMode::loop_ntimes(n))
    }

    /// Run `update`/`view` on the first frame, then only when input arrives or the window is
    /// resized or rescaled ([`RunMode::Wait`]).
    pub fn wait(self) -> Self {
        self.run_mode(RunMode::Wait)
    }

    /// Applies the window configuration to the app's [`WindowSettings`] (inserted if absent).
    fn configure_window(&self, app: &mut App) {
        if self.config.title.is_none() && self.config.size.is_none() {
            return;
        }
        let world = app.world_mut();
        if !world.contains_resource::<WindowSettings>() {
            world.insert_resource(WindowSettings::default());
        }
        let mut settings = world.resource_mut::<WindowSettings>();
        if let Some(title) = &self.config.title {
            settings.window.title.clone_from(title);
        }
        if let Some((w, h)) = self.config.size {
            settings.window.logical_size = (f64::from(w), f64::from(h));
        }
    }
}

/// Runs `model` with a [`Quick`] built on `world`, applying the commands it queued.
fn create_model<M>(world: &mut World, model: ModelFn<M>) -> M {
    let mut state = SystemState::<Quick>::new(world);
    let value = {
        let quick = state
            .get_mut(world)
            .expect("Quick's parameters are all optional, so it always validates");
        model(&quick)
    };
    state.apply(world);
    value
}

impl<M: QuickModel> Plugin for Builder<M> {
    fn build(&self, app: &mut App) {
        let Some(parts) = self.parts.lock().unwrap_or_else(|p| p.into_inner()).take() else {
            tracing::warn!(target: "regia_quick", "a quick app builder was added twice; ignoring the second");
            return;
        };
        self.configure_window(app);
        // The messages that wake `RunMode::Wait` (idempotent; the platform layer registers them too).
        app.add_message::<KeyboardInput>()
            .add_message::<MouseButtonInput>()
            .add_message::<CursorMoved>()
            .add_message::<MouseWheel>()
            .add_message::<TouchInput>()
            .add_message::<WindowResized>()
            .add_message::<WindowScaleFactorChanged>()
            .init_resource::<DrawFrozen>()
            .init_resource::<QuickLoop>()
            .insert_resource(self.config.run_mode)
            .register_type::<RunMode>()
            .register_type::<Model<M>>();

        let model = create_model(app.world_mut(), parts.model);
        app.insert_resource(Model { model });

        // One chain, no set-based ordering: hot-reload teardown removes these systems with their
        // owner set, and ordering edges through other sets would outlive them.
        let mut update = parts.update;
        let mut view = parts.view;
        app.add_systems(
            Update,
            (
                plan_quick_frame,
                (move |q: Quick, mut model: ResMut<Model<M>>| {
                    if let Some(update) = update.as_mut() {
                        let _span = tracing::trace_span!(target: "regia_quick", "quick_update").entered();
                        update(&q, &mut model.model);
                    }
                })
                .run_if(quick_should_run),
                (move |q: Quick, model: Res<Model<M>>| {
                    if let Some(view) = view.as_mut() {
                        let _span = tracing::trace_span!(target: "regia_quick", "quick_view").entered();
                        view(&q, &model.model);
                    }
                })
                .run_if(quick_should_run),
            )
                .chain()
                .in_set(QuickSet),
        );
    }

    fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU32;

    #[derive(Reflect, Default, Debug, Clone, PartialEq)]
    struct Counter {
        updates: u32,
        seen_by_view: u32,
    }

    fn counting_app(mode: RunMode, views: Arc<AtomicU32>) -> App {
        let mut game = App::new();
        game.add_plugins(
            app(|_: &Quick| Counter::default())
                .update(|_: &Quick, c: &mut Counter| c.updates += 1)
                .view(move |_: &Quick, c: &Counter| {
                    views.store(c.updates, Ordering::Relaxed);
                })
                .run_mode(mode),
        );
        game
    }

    #[test]
    fn update_then_view_every_frame_in_loop_mode() {
        let views = Arc::new(AtomicU32::new(0));
        let mut game = counting_app(RunMode::Loop, views.clone());
        for _ in 0..4 {
            game.update();
        }
        assert_eq!(game.world().resource::<Model<Counter>>().model.updates, 4);
        // `view` ran after `update` in the same frame: it saw the fourth update.
        assert_eq!(views.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn loop_n_times_freezes_and_stays_closable() {
        let views = Arc::new(AtomicU32::new(0));
        let mut game = counting_app(RunMode::loop_ntimes(2), views.clone());
        for _ in 0..8 {
            game.update();
        }
        assert_eq!(game.world().resource::<Model<Counter>>().model.updates, 2);
        assert_eq!(views.load(Ordering::Relaxed), 2);
        assert!(game.world().resource::<DrawFrozen>().0, "the last frame is held");
        assert!(game.world().resource::<QuickLoop>().frozen);
        // Frames keep running while frozen, so a close request is still handled.
        game.add_plugins(regia_platform::PlatformPlugin);
        let window = game
            .world_mut()
            .spawn((regia_platform::Window::default(), regia_platform::PrimaryWindow))
            .id();
        game.world_mut()
            .write_message(regia_platform::WindowCloseRequested { window });
        game.update();
        assert_eq!(game.should_exit(), Some(AppExit::Success));
    }

    #[test]
    fn model_is_a_registered_reflected_resource() {
        let views = Arc::new(AtomicU32::new(0));
        let mut game = counting_app(RunMode::Loop, views);
        game.update();
        let registry = game.world().resource::<AppTypeRegistry>().clone();
        let registry = registry.read();
        let path = <Model<Counter> as TypePath>::type_path();
        let registration = registry
            .get_with_type_path(path)
            .expect("Model<Counter> is registered");
        assert!(registration.data::<ReflectResource>().is_some());
        let model = game.world().resource::<Model<Counter>>();
        let regia_ecs::reflect::ReflectRef::Struct(fields) = model.reflect_ref() else {
            panic!("Model is a struct");
        };
        assert!(fields.field("model").is_some());
    }

    #[test]
    fn builder_sets_the_window_and_model_sees_it() {
        let mut game = App::new();
        game.add_plugins(app(|q: &Quick| q.window_size().x).title("Quick").size(320, 200));
        assert_eq!(game.world().resource::<Model<f32>>().model, 320.0);
        let settings = game.world().resource::<WindowSettings>();
        assert_eq!(settings.window.title, "Quick");
        assert_eq!(settings.window.logical_size, (320.0, 200.0));
    }

    #[test]
    fn sketch_runs_its_view_each_frame() {
        let calls = Arc::new(AtomicU32::new(0));
        let seen = calls.clone();
        let mut game = App::new();
        game.add_plugins(sketch(move |_: &Quick| {
            seen.fetch_add(1, Ordering::Relaxed);
        }));
        game.update();
        game.update();
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }
}
