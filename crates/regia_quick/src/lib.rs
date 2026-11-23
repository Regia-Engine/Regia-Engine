// SPDX-License-Identifier: MPL-2.0
//! Quick immediate drawing: a [`Draw`] resource for 2D shapes, paths, meshes and text drawn over
//! the frame, and a [`Draw3d`] resource for lit 3D primitives.
//!
//! The 2D API is nannou's `Draw` (ported from `nannou_draw`, MIT OR Apache-2.0; attribution in
//! `docs/ported-sources.toml` and each ported file's header), on Regia Engine's maths
//! ([`regia_core::math::geom`]), colours ([`regia_core::math::Color`]), text shaping
//! (`regia_ui`'s cosmic-text [`regia_ui::text::TextSystem`]) and immediate renderer
//! ([`regia_render::immediate`]); fill and stroke tessellation is `lyon`, plain shapes take
//! allocation-free fast paths ([`regia_render::primitives::triangulation`]).
//!
//! # How the pieces fit
//!
//! Add [`QuickPlugin`] after `RenderPlugin`. Any system draws through `Res<Draw>`:
//!
//! ```
//! use regia_quick::prelude::*;
//!
//! fn view(draw: Res<Draw>) {
//!     draw.background().color(Color::BLACK);
//!     draw.ellipse().x_y(-100.0, 0.0).radius(40.0).color(Color::RED);
//!     draw.rect().w_h(120.0, 60.0).rotate(0.3).color(Color::BLUE).stroke(Color::WHITE).stroke_weight(4.0);
//!     draw.text("hello").font_size(32).y(100.0);
//! }
//! # let _ = view;
//! ```
//!
//! Coordinates are logical window units, origin at the window centre, +Y up. In the `Render`
//! schedule, [`QuickSystems::Extract`] drains the frame's drawings into an
//! [`regia_render::ImmediateBatch`] drawn by a window overlay layer after tonemapping and
//! under the runtime UI (4× anti-aliased by default, [`QuickSettings`]). [`Draw3d`] drawings
//! become mesh instances of the forward path (lit, shadowed) and gizmo lines. With nothing
//! drawn, the layer records no GPU work. [`DrawFrozen`] keeps re-submitting the last frame.
//! [`QuickDrawStats`] summarises what was drawn for agents (`quick.draw_list`).
//!
//! See `book/agents/quick-draw.md`.

pub mod color;
pub mod draw;
pub mod draw2d;
pub mod draw3d;
pub mod frame;
mod overlay;

use regia_app::prelude::*;
use regia_platform::{PrimaryWindow, Window};
use regia_render::gizmos::{AppGizmoBuilder, GizmoConfig, GizmoConfigGroup, GizmoSystems, Gizmos};
use regia_render::{ExtractedFrame, ImmediateImage, WindowOverlay};
use regia_ui::Localization;
use regia_ui::text::TextSystem;

pub use draw::primitive::path::lyon_path_builds;
pub use draw::{BlendMode, Draw, DrawCommand, Drawing, QuickImage};
pub use draw3d::{Draw3d, Drawing3d, Item3d, Quick3dCache, Shape3d};
pub use frame::{FrameParams, QuickDrawStats, QuickRenderer};
pub use overlay::{QuickOverlay, QuickOverlayState};

/// Types an app needs to draw.
pub mod prelude {
    pub use crate::draw::primitive::text::{Justify, Wrap};
    pub use crate::draw::properties::{SetColor, SetDimensions, SetOrientation, SetPosition, SetStroke};
    pub use crate::draw::{BlendMode, Draw, QuickImage};
    pub use crate::draw3d::Draw3d;
    pub use crate::{DrawFrozen, QuickImages, QuickPlugin, QuickSettings};
    pub use regia_app::prelude::*;
    pub use regia_core::math::geom;
    pub use regia_core::math::{Color, Mat4, Quat, Vec2, Vec3};
}

/// The maths and colour types the ported draw modules use.
pub(crate) mod prelude_internal {
    pub use regia_app::prelude::{Entity, Resource};
    // The `Resource` derive names `bevy_ecs::…`.
    pub use regia_core::math::{Color, EulerRot, Mat4, Quat, Vec2, Vec3};
    pub use regia_ecs::prelude::bevy_ecs;
}

/// Logs a warning once per call site.
#[macro_export]
#[doc(hidden)]
macro_rules! warn_once {
    ($($arg:tt)*) => {{
        static ONCE: ::std::sync::Once = ::std::sync::Once::new();
        ONCE.call_once(|| ::tracing::warn!(target: "regia_quick", $($arg)*));
    }};
}

/// When `true`, extraction neither drains nor resets [`Draw`] / [`Draw3d`] and the overlay keeps
/// submitting the last frame (nannou's `DrawFrozen`: a run mode can hold its final frame on
/// screen without re-running the view).
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DrawFrozen(pub bool);

/// Settings of the quick layer.
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct QuickSettings {
    /// Anti-aliasing samples of the 2D layer (1 = none, 4 = default).
    pub samples: u32,
    /// Window size (logical units) and pixel density assumed without a primary window.
    pub fallback: FrameParams,
}

impl Default for QuickSettings {
    fn default() -> Self {
        Self {
            samples: 4,
            fallback: FrameParams {
                size: regia_core::math::Vec2::new(1280.0, 720.0),
                scale_factor: 1.0,
            },
        }
    }
}

/// Images drawings can be textured with ([`Drawing::texture`]).
#[derive(Resource, Default)]
pub struct QuickImages {
    next: u64,
    pending: Vec<(u64, Option<ImmediateImage>)>,
}

impl QuickImages {
    /// Registers an image (premultiplied sRGB RGBA8 or an R8 mask); it is uploaded before the
    /// next frame draws.
    pub fn add(&mut self, image: ImmediateImage) -> QuickImage {
        self.next += 1;
        self.pending.push((self.next, Some(image)));
        QuickImage(self.next)
    }

    /// Replaces the pixels of `image`.
    pub fn update(&mut self, image: QuickImage, pixels: ImmediateImage) {
        self.pending.push((image.0, Some(pixels)));
    }

    /// Unregisters `image`.
    pub fn remove(&mut self, image: QuickImage) {
        self.pending.push((image.0, None));
    }
}

/// Gizmo group of [`Draw3d`] lines (`quick`; agents can restyle it with `debug.set_gizmos`).
#[derive(Default, Debug, Clone, Copy)]
pub struct QuickLines;

impl GizmoConfigGroup for QuickLines {
    const NAME: &'static str = "quick";

    fn default_config() -> GizmoConfig {
        GizmoConfig::default()
    }
}

/// System sets of the quick layer (`Render` schedule).
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QuickSystems {
    /// Draw lists become the overlay batch and forward-path meshes.
    Extract,
}

/// Installs [`Draw`], [`Draw3d`], the overlay layer and the extraction. Add after
/// `RenderPlugin`; uses `regia_ui`'s [`TextSystem`] when `UiPlugin` installed one, else an
/// embedded-fonts-only one.
#[derive(Default)]
pub struct QuickPlugin;

/// Order of the quick layer among window overlays (under the UI).
pub const OVERLAY_ORDER: i32 = WindowOverlay::UI_ORDER - 10;

impl Plugin for QuickPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Draw>()
            .init_resource::<Draw3d>()
            .init_resource::<DrawFrozen>()
            .init_resource::<QuickSettings>()
            .init_resource::<QuickImages>()
            .init_resource::<QuickDrawStats>()
            .init_resource::<QuickOverlay>()
            .init_resource::<Quick3dCache>()
            .init_resource::<QuickBuilder>()
            .init_gizmo_group::<QuickLines>()
            .configure_sets(
                Render,
                QuickSystems::Extract
                    .after(regia_render::runtime::extract_frame)
                    .before(GizmoSystems::Collect),
            )
            .add_systems(
                Render,
                (extract_quick_draw, extract_quick_draw_3d).in_set(QuickSystems::Extract),
            );
    }

    fn finish(&self, app: &mut App) {
        if !app.world().contains_resource::<TextSystem>() {
            app.insert_resource(TextSystem::embedded_only());
        }
        let handle = app.world().resource::<QuickOverlay>().clone();
        match app.world_mut().get_resource_mut::<WindowOverlay>() {
            Some(mut overlay) => {
                overlay.insert(
                    OVERLAY_ORDER,
                    "quick",
                    Box::new(overlay::QuickOverlayPass(handle)),
                );
            }
            None => tracing::warn!(
                target: "regia_quick",
                "WindowOverlay missing (add RenderPlugin before QuickPlugin); quick 2D drawing will not show"
            ),
        }
    }
}

/// The frame builder resource (tessellators, glyph atlas).
#[derive(Resource, Default)]
struct QuickBuilder(QuickRenderer);

#[allow(clippy::too_many_arguments)] // one extraction step's inputs
fn extract_quick_draw(
    draw: Res<Draw>,
    frozen: Res<DrawFrozen>,
    settings: Res<QuickSettings>,
    windows: Query<&Window, With<PrimaryWindow>>,
    fonts: Option<ResMut<TextSystem>>,
    localization: Option<Res<Localization>>,
    mut fallback_localization: Local<Option<Localization>>,
    mut builder: ResMut<QuickBuilder>,
    mut images: ResMut<QuickImages>,
    overlay: Res<QuickOverlay>,
    mut stats: ResMut<QuickDrawStats>,
) {
    let _span = tracing::info_span!("quick_draw_extract").entered();
    let mut state = overlay.lock();
    for (id, image) in images.pending.drain(..) {
        match image {
            Some(image) => state.renderer.set_image(id, image),
            None => state.renderer.remove_image(id),
        }
    }
    state.samples = settings.samples.max(1);
    if frozen.0 {
        return;
    }
    let window = windows
        .iter()
        .next()
        .filter(|w| w.physical_size.0 > 0 && w.physical_size.1 > 0);
    let (params, physical) = window.map_or_else(
        || (settings.fallback, physical_pixels(settings.fallback)),
        |w| {
            let scale = (w.scale_factor as f32).max(1e-3);
            let params = FrameParams {
                size: regia_core::math::Vec2::new(w.physical_size.0 as f32, w.physical_size.1 as f32) / scale,
                scale_factor: scale,
            };
            (params, (w.physical_size.0, w.physical_size.1))
        },
    );
    state.scale_factor = params.scale_factor;
    state.logical_size = params.size;
    state.built_physical = physical;
    let mut fonts = fonts;
    let localization: &Localization = match &localization {
        Some(l) => l,
        None => fallback_localization.get_or_insert_with(Localization::default),
    };
    let text = fonts.as_deref_mut().map(|f| (f, localization));
    let frame = stats.frame;
    state.batch_generation = state.batch_generation.wrapping_add(1).max(1);
    let mut built = builder.0.build(&draw, params, text, &mut state.batch);
    for (id, image) in builder.0.take_images() {
        state.renderer.set_image(id, image);
    }
    draw.reset();
    built.frame = frame + 1;
    built.primitives_3d = stats.primitives_3d;
    built.lines_3d = stats.lines_3d;
    *stats = built;
}

/// Physical pixels of a logical frame (the fallback path, when no window supplied a size).
fn physical_pixels(params: FrameParams) -> (u32, u32) {
    let axis = |v: f32| v.round().max(0.0).min(u32::MAX as f32) as u32;
    (
        axis(params.size.x * params.scale_factor),
        axis(params.size.y * params.scale_factor),
    )
}

fn extract_quick_draw_3d(
    draw: Res<Draw3d>,
    frozen: Res<DrawFrozen>,
    mut cache: ResMut<Quick3dCache>,
    mut extracted: ResMut<ExtractedFrame>,
    mut gizmos: Gizmos<QuickLines>,
    mut stats: ResMut<QuickDrawStats>,
) {
    // Frozen frames resubmit the last drained lists and leave `draw` alone, matching the 2D
    // path: `view` does not run in `RunMode::loop_once` / a spent `LoopNTimes`, and drawings
    // made while held wait for the next live frame.
    cache.take_frame(&draw, frozen.0);
    let n_items = cache.items().len();
    let n_lines = cache.lines().len();
    stats.primitives_3d = n_items as u32;
    stats.lines_3d = n_lines as u32;
    if n_items == 0 && n_lines == 0 {
        return;
    }
    let _span = tracing::info_span!("quick_draw_extract_3d").entered();
    cache.extend_extracted(&mut extracted.meshes);
    for &(a, b, color) in cache.lines() {
        gizmos.line(a, b, color);
    }
}

// The quick app layer (`sketch`, `app`, the `Quick` system parameter, run modes, bytes loading):
// survey catalogue #5 and #19, `book/user-guide/quick-start.md`, `book/agents/quick-app.md`.
pub mod app;
pub mod context;
pub mod load;
pub mod run_mode;

pub use app::{Builder, Model, QuickModel, QuickSet, app, sketch};
pub use context::{Quick, QuickCamera, QuickSun, QuickTouch};
pub use load::BytesRequest;
pub use run_mode::{QuickLoop, RunMode};
