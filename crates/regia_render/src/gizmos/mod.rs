// SPDX-License-Identifier: MPL-2.0
//! Immediate-mode gizmos: debug lines and shapes drawn for one frame from any system.
//!
//! Ported from Bevy's `bevy_gizmos` / `bevy_gizmos_render` (MIT OR Apache-2.0, commit `fd98063`,
//! attribution in `docs/ported-sources.toml` and each file's header) and adapted to Regia Engine:
//! `regia_core` maths and colours, no asset server, Regia Engine's single frame renderer.
//!
//! # How the pieces fit
//!
//! A system takes a [`Gizmos`] parameter and calls `line`, `arrow`, `circle`, `sphere`,
//! `capsule`, `grid`, `aabb_3d`, `frustum`, `text`, … Those calls append line-list and
//! line-strip vertices to the parameter's [`GizmoBuffer`], which flushes into the group's
//! [`GizmoStorage`] when the system's deferred commands apply. Each frame, `Render` drains every
//! group's storage into a [`GizmoFrame`] (one [`GizmoBatch`] per enabled group, plus one per
//! retained [`Gizmo`] component) on [`crate::ExtractedFrame::gizmos`]. The frame renderer draws
//! it in its own pass after the scene, post stack and translucent layers, before the UI overlay,
//! at every quality preset.
//!
//! Groups ([`GizmoConfigGroup`]) carry the styling: line width, perspective, dotted/dashed
//! style, strip joints, depth bias and on-top drawing. Four debug overlay groups ship disabled:
//! [`PhysicsGizmos`], [`NavMeshGizmos`], [`AudioGizmos`] and [`AiPerceptionGizmos`]; the agent
//! method `debug.set_gizmos` toggles any group by name.
//!
//! See `book/user-guide/gizmos.md` and `book/agents/gizmos.md`.

mod arcs;
mod arrows;
mod buffer;
mod circles;
mod config;
mod curves;
mod extract;
mod grid;
mod primitives;
pub(crate) mod render;
mod retained;
mod rounded_box;
mod simplex_stroke_font;
mod stroke_text;

use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::math::{Color, Mat4, Vec3};
use regia_scene::GlobalTransform;

pub use arcs::Arc3dBuilder;
pub use arrows::ArrowBuilder;
pub use buffer::{GizmoBuffer, GizmoBufferView, GizmoStorage, Gizmos, IntoIsometry, TransformPoint};
pub use circles::{DEFAULT_CIRCLE_RESOLUTION, EllipseBuilder, SphereBuilder};
pub use config::{
    AiDebugGizmos, AiPerceptionGizmos, AudioGizmos, CameraRigGizmos, DefaultGizmoConfigGroup,
    ErasedGizmoConfigGroup, FormationGizmos, GizmoConfig, GizmoConfigGroup, GizmoConfigStore,
    GizmoLineConfig, GizmoLineJoint, GizmoLineStyle, MotionMatchingGizmos, NavMeshGizmos, PhysicsGizmos,
    PostVolumeGizmos, SmartObjectGizmos,
};
pub use extract::{GizmoBatch, GizmoFrame, GizmoJoint, GizmoLabelBatch, GizmoLabelVertex, GizmoSegment};
pub use grid::{GridBuilder2d, GridBuilder3d};
pub use primitives::{
    Capsule3dBuilder, Cone3dBuilder, ConicalFrustum3dBuilder, Plane3dBuilder, Torus3dBuilder,
};
pub use retained::{Gizmo, GizmoAsset};
pub use rounded_box::{RoundedCuboidBuilder, RoundedRectBuilder};
pub use stroke_text::stroke_text_segments;

/// System sets of the gizmo extraction (`Render`).
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GizmoSystems {
    /// Each group's storage becomes a batch.
    Collect,
    /// The batches and retained gizmos become [`crate::ExtractedFrame::gizmos`].
    Finish,
}

/// Batches collected this frame, waiting for [`GizmoSystems::Finish`].
#[derive(Resource, Default)]
struct PendingGizmoBatches(Vec<GizmoBatch>);

/// Atlas-drawn debug labels collected this frame (ADR-7254), waiting for [`GizmoSystems::Finish`].
/// One entry per (on top, page): the page and its glyph vertices so far.
#[derive(Resource, Default)]
struct PendingGizmoLabels(Vec<(bool, u32, crate::DebugLabelPage, Vec<GizmoLabelVertex>)>);

/// Expanded retained gizmos. An unchanged asset reuses its segment and joint slices.
///
/// The key is the entity, the asset allocation (`Arc::as_ptr`), and the line settings. A
/// transform change rewrites only `world_from_local`. Replacing the asset `Arc`, or editing the
/// buffer through a unique `Arc::get_mut`, is what rebuilds the slices. An in-place edit keeps
/// the pointer, so a [`Gizmo`] changed this frame (any `&mut Gizmo` access, which `get_mut`
/// needs) always re-expands; a shared `Arc` cannot be mutated in place.
#[derive(Resource, Default)]
pub(crate) struct RetainedGizmoCache {
    /// Expansions since the cache was created. A repeated frame of an unchanged asset does not
    /// increment this.
    pub(crate) expands: u64,
    entries: Vec<RetainedGizmoEntry>,
    /// Frame stamp. Entries not touched this frame are dropped.
    stamp: u64,
}

struct RetainedGizmoEntry {
    entity: Entity,
    asset: usize,
    line: GizmoLineConfig,
    depth_bias: f32,
    on_top: bool,
    segments: Arc<[GizmoSegment]>,
    joints: Arc<[GizmoJoint]>,
    seen: u64,
}

/// Registers custom gizmo config groups on an [`App`].
pub trait AppGizmoBuilder {
    /// Registers group `G` (its config starts as [`GizmoConfigGroup::default_config`]) and the
    /// system that hands its gizmos to the renderer. Idempotent.
    fn init_gizmo_group<G: GizmoConfigGroup>(&mut self) -> &mut Self;

    /// Registers group `G` with an explicit value and config (replacing any existing one).
    fn insert_gizmo_config<G: GizmoConfigGroup>(&mut self, group: G, config: GizmoConfig) -> &mut Self;
}

impl AppGizmoBuilder for App {
    fn init_gizmo_group<G: GizmoConfigGroup>(&mut self) -> &mut Self {
        if self.world().contains_resource::<GizmoStorage<G>>() {
            return self;
        }
        self.world_mut()
            .get_resource_or_init::<GizmoConfigStore>()
            .register::<G>();
        self.init_resource::<GizmoStorage<G>>()
            .add_systems(Render, collect_gizmos::<G>.in_set(GizmoSystems::Collect))
    }

    fn insert_gizmo_config<G: GizmoConfigGroup>(&mut self, group: G, config: GizmoConfig) -> &mut Self {
        self.init_gizmo_group::<G>();
        self.world_mut()
            .get_resource_or_init::<GizmoConfigStore>()
            .insert(config, group);
        self
    }
}

/// Run condition: group `G` is registered and enabled (false without a renderer). Use it on
/// systems that only draw gizmos so a disabled overlay costs nothing.
pub fn gizmo_group_enabled<G: GizmoConfigGroup>(store: Option<Res<GizmoConfigStore>>) -> bool {
    store.is_some_and(|store| store.is_enabled::<G>())
}

/// Installs gizmos: the config store, the default and debug-overlay groups, retained gizmos and
/// the per-frame extraction. Added by [`crate::RenderPlugin`].
#[derive(Default)]
pub struct GizmoPlugin;

impl Plugin for GizmoPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<GizmoConfigStore>()
            .init_resource::<PendingGizmoBatches>()
            .init_resource::<PendingGizmoLabels>()
            .init_resource::<crate::DebugLabelGlyphs>()
            .init_resource::<RetainedGizmoCache>()
            .init_gizmo_group::<DefaultGizmoConfigGroup>()
            .init_gizmo_group::<PhysicsGizmos>()
            .init_gizmo_group::<NavMeshGizmos>()
            .init_gizmo_group::<AudioGizmos>()
            .init_gizmo_group::<AiPerceptionGizmos>()
            .init_gizmo_group::<SmartObjectGizmos>()
            .init_gizmo_group::<CameraRigGizmos>()
            .init_gizmo_group::<MotionMatchingGizmos>()
            .init_gizmo_group::<PostVolumeGizmos>()
            .init_gizmo_group::<FormationGizmos>()
            .init_gizmo_group::<AiDebugGizmos>()
            .configure_sets(
                Render,
                (GizmoSystems::Collect, GizmoSystems::Finish)
                    .chain()
                    .after(crate::runtime::extract_frame)
                    .before(crate::runtime::render_texture_views),
            )
            .add_systems(
                Render,
                (collect_debug_draw, finish_gizmo_frame)
                    .chain()
                    .in_set(GizmoSystems::Finish),
            )
            // The post-volume overlay (ADR-2323); its system does not run while the group is off.
            .add_systems(
                regia_app::PostUpdate,
                crate::post_volume_gizmos::draw_post_volumes
                    .run_if(gizmo_group_enabled::<PostVolumeGizmos>)
                    .after(regia_app::PostUpdateSet::TransformPropagate),
            );
    }
}

fn collect_gizmos<G: GizmoConfigGroup>(
    mut storage: ResMut<GizmoStorage<G>>,
    store: Res<GizmoConfigStore>,
    mut pending: ResMut<PendingGizmoBatches>,
) {
    let _span = tracing::debug_span!("collect_gizmos").entered();
    let Some((config, _)) = store.config::<G>() else {
        storage.clear();
        return;
    };
    if config.enabled && !storage.is_empty() {
        let batch = GizmoBatch::from_view(storage.view(), Mat4::IDENTITY, config);
        if !batch.is_empty() {
            pending.0.push(batch);
        }
    }
    storage.clear();
}

/// The [`crate::DebugDraw`] shim: its queued lines and labels draw through the default group, the
/// labels facing the primary camera; each item leaves the queue when its duration is over
/// (ADR-7211).
fn collect_debug_draw(
    debug: Option<ResMut<crate::DebugDraw>>,
    store: Res<GizmoConfigStore>,
    extracted: Res<crate::ExtractedFrame>,
    mut pending: ResMut<PendingGizmoBatches>,
    mut pending_labels: ResMut<PendingGizmoLabels>,
    mut glyphs: Option<ResMut<crate::DebugLabelGlyphs>>,
) {
    let _span = tracing::debug_span!("collect_debug_draw").entered();
    let Some(mut debug) = debug else { return };
    if debug.is_empty() {
        return;
    }
    let default = store
        .config::<DefaultGizmoConfigGroup>()
        .map(|(config, _)| config.clone())
        .unwrap_or_default();
    if default.enabled {
        let eye = extracted.primary_view().map(|v| Vec3::from_array(v.camera.eye));
        for (on_top, depth_test) in [(false, true), (true, false)] {
            let mut buffer = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
            let mut any = false;
            for l in debug.lines.iter().filter(|l| l.depth_test == depth_test) {
                buffer.line(
                    Vec3::from_array(l.from),
                    Vec3::from_array(l.to),
                    Color::from(l.color),
                );
                any = true;
            }
            for t in debug.texts.iter().filter(|t| t.depth_test == depth_test) {
                let at = Vec3::from_array(t.position);
                let facing = eye.map_or(regia_core::math::Quat::IDENTITY, |eye| facing_camera(at, eye));
                // The installed font atlas first (ADR-7254); the stroke font without one.
                if let Some((quads, pages)) = glyphs.as_mut().and_then(|g| g.layout(&t.text)) {
                    push_label_quads(&mut pending_labels.0, on_top, at, facing, t, &quads, pages);
                    continue;
                }
                buffer.text(
                    (at, facing),
                    &t.text,
                    t.size,
                    regia_core::math::Vec2::ZERO,
                    Color::from(t.color),
                );
                any = true;
            }
            if !any {
                continue;
            }
            let config = GizmoConfig {
                on_top,
                ..default.clone()
            };
            pending
                .0
                .push(GizmoBatch::from_view(buffer.buffer(), Mat4::IDENTITY, &config));
        }
    }
    debug.advance_frame();
}

/// The rotation that turns the local XY plane (text) to face `eye` from `at`, local +Y kept up.
fn facing_camera(at: Vec3, eye: Vec3) -> regia_core::math::Quat {
    let z = (eye - at).normalize_or_zero();
    let x = Vec3::Y.cross(z).normalize_or_zero();
    if z == Vec3::ZERO || x == Vec3::ZERO {
        return regia_core::math::Quat::IDENTITY;
    }
    let y = z.cross(x);
    regia_core::math::Quat::from_mat3(&regia_core::math::Mat3::from_cols(x, y, z))
}

/// Appends the glyph quads of label `t` at `at` (rotated by `facing`) to the batch of their page.
fn push_label_quads(
    out: &mut Vec<(bool, u32, crate::DebugLabelPage, Vec<GizmoLabelVertex>)>,
    on_top: bool,
    at: Vec3,
    facing: regia_core::math::Quat,
    t: &crate::DebugText,
    quads: &[crate::DebugLabelGlyph],
    pages: Vec<(u32, crate::DebugLabelPage)>,
) {
    let (right, up) = (facing * Vec3::X * t.size, facing * Vec3::Y * t.size);
    for (index, page) in pages {
        let mut vertices = Vec::new();
        for q in quads.iter().filter(|q| q.page == index) {
            let [x0, y0, x1, y1] = q.rect;
            let [u0, v0, u1, v1] = q.uv;
            let corner = |x: f32, y: f32, u: f32, v: f32| GizmoLabelVertex {
                position: (at + right * x + up * y).to_array(),
                uv: [u, v],
                color: t.color,
            };
            // Bottom-left, bottom-right, top-right; bottom-left, top-right, top-left (V down).
            let (bl, br, tr, tl) = (
                corner(x0, y0, u0, v1),
                corner(x1, y0, u1, v1),
                corner(x1, y1, u1, v0),
                corner(x0, y1, u0, v0),
            );
            vertices.extend_from_slice(&[bl, br, tr, bl, tr, tl]);
        }
        if vertices.is_empty() {
            continue;
        }
        match out
            .iter_mut()
            .find(|(top, i, p, _)| *top == on_top && *i == index && *p == page)
        {
            Some((_, _, _, batch)) => batch.extend_from_slice(&vertices),
            None => out.push((on_top, index, page, vertices)),
        }
    }
}

fn finish_gizmo_frame(
    mut pending: ResMut<PendingGizmoBatches>,
    mut pending_labels: ResMut<PendingGizmoLabels>,
    retained: Query<(Entity, Ref<Gizmo>, Option<&GlobalTransform>)>,
    mut cache: ResMut<RetainedGizmoCache>,
    mut extracted: ResMut<crate::ExtractedFrame>,
) {
    let _span = tracing::info_span!("gizmo_extract").entered();
    let mut batches = std::mem::take(&mut pending.0);
    cache.stamp = cache.stamp.wrapping_add(1);
    let stamp = cache.stamp;
    for (entity, gizmo, transform) in &retained {
        if gizmo.asset.is_empty() {
            continue;
        }
        let world_from_local = transform.map_or(Mat4::IDENTITY, |t| Mat4::from(t.0));
        let asset = Arc::as_ptr(&gizmo.asset) as usize;
        // A changed component may hold an asset edited in place under the same pointer.
        let changed = gizmo.is_changed();
        let hit = cache.entries.iter().position(|entry| {
            !changed
                && entry.entity == entity
                && entry.asset == asset
                && entry.line == gizmo.line
                && entry.depth_bias == gizmo.depth_bias
                && entry.on_top == gizmo.on_top
        });
        if let Some(index) = hit {
            cache.entries[index].seen = stamp;
            batches.push(GizmoBatch {
                world_from_local,
                line: gizmo.line.clone(),
                depth_bias: gizmo.depth_bias,
                on_top: gizmo.on_top,
                segments: Arc::clone(&cache.entries[index].segments),
                joints: Arc::clone(&cache.entries[index].joints),
            });
            continue;
        }
        let config = GizmoConfig {
            enabled: true,
            line: gizmo.line.clone(),
            depth_bias: gizmo.depth_bias,
            on_top: gizmo.on_top,
        };
        let batch = GizmoBatch::from_view(gizmo.asset.buffer(), world_from_local, &config);
        if batch.is_empty() {
            continue;
        }
        cache.expands += 1;
        cache.entries.push(RetainedGizmoEntry {
            entity,
            asset,
            line: gizmo.line.clone(),
            depth_bias: gizmo.depth_bias,
            on_top: gizmo.on_top,
            segments: Arc::clone(&batch.segments),
            joints: Arc::clone(&batch.joints),
            seen: stamp,
        });
        batches.push(batch);
    }
    cache.entries.retain(|entry| entry.seen == stamp);
    let labels = std::mem::take(&mut pending_labels.0)
        .into_iter()
        .map(|(on_top, index, page, vertices)| GizmoLabelBatch {
            on_top,
            page: (index, page),
            vertices: vertices.into(),
        })
        .collect();
    extracted.gizmos = Arc::new(GizmoFrame { batches, labels });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draw_lines(mut gizmos: Gizmos) {
        gizmos.line(Vec3::ZERO, Vec3::X, Color::RED);
        gizmos.circle(Vec3::ZERO, 1.0, Color::GREEN).resolution(8);
    }

    fn draw_physics(mut gizmos: Gizmos<PhysicsGizmos>) {
        gizmos.cuboid(Vec3::ZERO, Vec3::ONE, Color::WHITE);
    }

    fn app() -> App {
        let mut app = App::new();
        app.init_resource::<crate::ExtractedFrame>()
            .init_resource::<crate::runtime::RuntimeRenderer>()
            .add_systems(Render, crate::runtime::extract_frame)
            .add_systems(Render, crate::runtime::render_texture_views)
            .add_plugins(GizmoPlugin)
            .add_systems(Update, (draw_lines, draw_physics));
        app
    }

    #[test]
    fn immediate_gizmos_reach_the_extracted_frame_once() {
        let mut app = app();
        app.update();
        let frame = app.world().resource::<crate::ExtractedFrame>().gizmos.clone();
        // The physics group is disabled by default: only the default group's batch.
        assert_eq!(frame.batches.len(), 1);
        assert_eq!(frame.segment_count(), 1 + 8);
        assert!(
            app.world()
                .resource::<GizmoStorage<DefaultGizmoConfigGroup>>()
                .is_empty()
        );

        app.world_mut()
            .resource_mut::<GizmoConfigStore>()
            .set_enabled(PhysicsGizmos::NAME, true);
        app.update();
        let frame = app.world().resource::<crate::ExtractedFrame>().gizmos.clone();
        assert_eq!(frame.batches.len(), 2);
        assert_eq!(frame.segment_count(), 1 + 8 + 12);
    }

    #[test]
    fn disabled_default_group_draws_nothing() {
        let mut app = app();
        app.world_mut()
            .resource_mut::<GizmoConfigStore>()
            .set_enabled("default", false);
        app.update();
        assert!(app.world().resource::<crate::ExtractedFrame>().gizmos.is_empty());
    }

    #[test]
    fn retained_gizmos_follow_their_entity() {
        let mut app = app();
        let mut asset = GizmoAsset::new();
        asset.line(Vec3::ZERO, Vec3::Y, Color::WHITE);
        app.world_mut().spawn((
            Gizmo::new(Arc::new(asset)),
            GlobalTransform(regia_core::math::Affine3A::from_translation(Vec3::new(
                5.0, 0.0, 0.0,
            ))),
        ));
        app.update();
        let frame = app.world().resource::<crate::ExtractedFrame>().gizmos.clone();
        let retained = frame
            .batches
            .iter()
            .find(|b| b.world_from_local != Mat4::IDENTITY)
            .expect("retained batch");
        assert_eq!(retained.segments.len(), 1);
        assert_eq!(retained.world_from_local.w_axis.x, 5.0);
    }

    #[test]
    fn debug_draw_shim_draws_through_the_default_group() {
        let mut app = app();
        let mut debug = crate::DebugDraw::default();
        debug.line([0.0; 3], [1.0, 0.0, 0.0], [1.0, 1.0, 0.0, 1.0]);
        debug.lines.push(crate::DebugLine {
            from: [0.0; 3],
            to: [0.0, 1.0, 0.0],
            color: [1.0; 4],
            depth_test: false,
            duration: 0,
        });
        app.insert_resource(debug);
        app.update();
        let frame = app.world().resource::<crate::ExtractedFrame>().gizmos.clone();
        assert_eq!(frame.segment_count(), 1 + 8 + 2);
        assert!(frame.batches.iter().any(|b| b.on_top));
        assert!(app.world().resource::<crate::DebugDraw>().lines.is_empty());
    }

    fn segments(app: &App) -> usize {
        app.world()
            .resource::<crate::ExtractedFrame>()
            .gizmos
            .segment_count()
    }

    #[test]
    fn a_duration_two_debug_line_draws_for_exactly_two_frames() {
        let mut app = app();
        app.update();
        let base = segments(&app);
        let mut debug = crate::DebugDraw::default();
        debug.line_for([0.0; 3], [1.0, 0.0, 0.0], [1.0, 0.0, 0.0, 1.0], 2);
        debug.line([0.0; 3], [0.0, 0.0, 1.0], [0.0, 0.0, 1.0, 1.0]);
        app.insert_resource(debug);
        let counts: Vec<usize> = (0..4)
            .map(|_| {
                app.update();
                segments(&app)
            })
            .collect();
        // Frame 1: both lines; frame 2: the duration-2 line alone; then nothing.
        assert_eq!(counts, vec![base + 2, base + 1, base, base]);
        assert!(app.world().resource::<crate::DebugDraw>().is_empty());
    }

    #[test]
    fn debug_text_draws_stroke_glyphs_facing_the_camera_for_its_duration() {
        let mut app = app();
        app.update();
        let base = segments(&app);
        let mut debug = crate::DebugDraw::default();
        debug.text([0.0, 1.0, 0.0], "HI", 0.25, [1.0; 4], 3);
        debug.texts.push(crate::DebugText {
            position: [0.0; 3],
            text: "X".into(),
            size: 0.1,
            color: [1.0; 4],
            depth_test: false,
            duration: 1,
        });
        app.insert_resource(debug);
        app.update();
        let frame = app.world().resource::<crate::ExtractedFrame>().gizmos.clone();
        let first = frame.segment_count();
        assert!(first > base + 3, "glyph strokes: {first} vs {base}");
        assert!(frame.batches.iter().any(|b| b.on_top), "the on-top label");
        app.update();
        let second = segments(&app);
        assert!(
            second > base && second < first,
            "the X expired: {second} (first {first}, base {base})"
        );
        app.update();
        app.update();
        assert_eq!(segments(&app), base, "both labels expired");
        // A label faces the camera: its plane normal points at the eye.
        let q = facing_camera(Vec3::ZERO, Vec3::new(3.0, 0.0, 0.0));
        assert!((q * Vec3::Z - Vec3::X).length() < 1e-5);
        assert!((q * Vec3::Y - Vec3::Y).length() < 1e-5);
    }

    #[test]
    fn with_a_glyph_source_a_label_draws_atlas_quads_and_without_it_stroke_glyphs() {
        // ADR-7254: the installed source's quads sample its atlas page; no source, stroke font.
        let label = |app: &mut App| {
            let mut debug = crate::DebugDraw::default();
            debug.text([0.0, 1.0, 0.0], "HI", 0.5, [1.0, 0.5, 0.25, 1.0], 1);
            app.insert_resource(debug);
            app.update();
            app.world().resource::<crate::ExtractedFrame>().gizmos.clone()
        };
        let mut stroke_app = app();
        stroke_app.update();
        let base = segments(&stroke_app);
        let stroke = label(&mut stroke_app);
        assert!(stroke.labels.is_empty(), "no source: no atlas quads");
        assert!(stroke.segment_count() > base + 3, "no source: stroke glyphs");

        let mut atlas_app = app();
        crate::install_debug_label_glyphs(
            &mut atlas_app,
            Box::new(crate::debug_labels::tests::QuarterPage::new(255)),
        );
        atlas_app.update();
        let base = segments(&atlas_app);
        let atlas = label(&mut atlas_app);
        assert_eq!(
            atlas.segment_count(),
            base,
            "the source draws the label: no stroke glyphs"
        );
        assert_eq!(atlas.labels.len(), 1, "one page");
        let batch = &atlas.labels[0];
        assert_eq!(batch.page.0, 0);
        assert_eq!(batch.page.1.size, 8);
        assert_eq!(batch.vertices.len(), 2 * 6, "two glyphs, two triangles each");
        for v in batch.vertices.iter() {
            // Every corner samples the glyph's rectangle on the page (the quarter).
            assert!(
                (0.0..=0.5).contains(&v.uv[0]) && (0.0..=0.5).contains(&v.uv[1]),
                "{v:?}"
            );
            assert_eq!(v.color, [1.0, 0.5, 0.25, 1.0]);
            // The label plane: y in 1 ± 0.5 × size.
            assert!((v.position[1] - 1.0).abs() <= 0.25 + 1e-5, "{v:?}");
        }
        assert!(!atlas.is_empty());
        // The label leaves with its duration.
        atlas_app.update();
        assert!(
            atlas_app
                .world()
                .resource::<crate::ExtractedFrame>()
                .gizmos
                .labels
                .is_empty()
        );
    }

    #[test]
    fn audit_g3_unchanged_retained_gizmo_reuses_its_expanded_batch() {
        let mut app = app();
        let mut asset = GizmoAsset::new();
        asset.line(Vec3::ZERO, Vec3::Y, Color::WHITE);
        let entity = app
            .world_mut()
            .spawn((
                Gizmo::new(Arc::new(asset)),
                GlobalTransform(regia_core::math::Affine3A::from_translation(Vec3::new(
                    5.0, 0.0, 0.0,
                ))),
            ))
            .id();
        app.update();
        let seg = retained_segments(&app);
        assert_eq!(app.world().resource::<RetainedGizmoCache>().expands, 1);

        app.update();
        let again = retained_segments(&app);
        assert!(
            Arc::ptr_eq(&seg, &again),
            "second frame expanded the retained gizmo again"
        );
        assert_eq!(app.world().resource::<RetainedGizmoCache>().expands, 1);

        app.world_mut()
            .get_mut::<GlobalTransform>(entity)
            .expect("spawned")
            .0 = regia_core::math::Affine3A::from_translation(Vec3::new(9.0, 0.0, 0.0));
        app.update();
        let moved = retained_batch(&app);
        assert_eq!(moved.world_from_local.w_axis.x, 9.0);
        assert!(Arc::ptr_eq(&seg, &moved.segments));
        assert_eq!(app.world().resource::<RetainedGizmoCache>().expands, 1);

        app.world_mut()
            .get_mut::<Gizmo>(entity)
            .expect("spawned")
            .line
            .width = 7.0;
        app.update();
        let restyled = retained_batch(&app);
        assert_eq!(restyled.line.width, 7.0);
        assert!(!Arc::ptr_eq(&seg, &restyled.segments));
        assert_eq!(app.world().resource::<RetainedGizmoCache>().expands, 2);
    }

    fn retained_batch(app: &App) -> GizmoBatch {
        app.world()
            .resource::<crate::ExtractedFrame>()
            .gizmos
            .batches
            .iter()
            .find(|batch| batch.world_from_local != Mat4::IDENTITY)
            .expect("retained batch")
            .clone()
    }

    /// An asset edited in place through `Arc::get_mut` keeps its pointer; the next frame must
    /// still draw the new segments, not the cached expansion.
    #[test]
    fn a_retained_asset_edited_in_place_re_expands() {
        let mut app = app();
        let mut asset = GizmoAsset::new();
        asset.line(Vec3::ZERO, Vec3::Y, Color::WHITE);
        let entity = app
            .world_mut()
            .spawn((
                Gizmo::new(Arc::new(asset)),
                GlobalTransform(regia_core::math::Affine3A::from_translation(Vec3::new(
                    5.0, 0.0, 0.0,
                ))),
            ))
            .id();
        app.update();
        let before = retained_segments(&app).len();
        app.update();
        assert_eq!(app.world().resource::<RetainedGizmoCache>().expands, 1);
        {
            let mut gizmo = app.world_mut().get_mut::<Gizmo>(entity).expect("spawned");
            let edited = Arc::get_mut(&mut gizmo.asset).expect("the cache keeps no asset Arc");
            edited.line(Vec3::ZERO, Vec3::X, Color::WHITE);
            edited.line(Vec3::ZERO, Vec3::Z, Color::WHITE);
        }
        app.update();
        let after = retained_segments(&app).len();
        assert!(
            after > before,
            "the in-place edit drew the old {before} segments ({after})"
        );
        assert_eq!(app.world().resource::<RetainedGizmoCache>().expands, 2);
        // Unchanged again: the new expansion is reused.
        app.update();
        assert_eq!(retained_segments(&app).len(), after);
        assert_eq!(app.world().resource::<RetainedGizmoCache>().expands, 2);
    }

    fn retained_segments(app: &App) -> Arc<[GizmoSegment]> {
        Arc::clone(&retained_batch(app).segments)
    }
}
