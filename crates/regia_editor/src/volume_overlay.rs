// SPDX-License-Identifier: MPL-2.0
//! Show ▸ Post volumes (lane w9-viewport-flags-nav, ADR-7593): every post volume's outline in
//! the viewport, the editor-side view of the runtime's `post_volumes` debug overlay (ADR-2323).
//!
//! The segments are [`regia_render::post_volume_outline`], the data `render.post_volume_gizmos`
//! hands to agents and the runtime overlay draws: the shape in its priority color
//! ([`regia_render::post_volume_gizmos::priority_color`]) and the outer edge of its blend band
//! dimmed by [`regia_render::post_volume_gizmos::BAND_DIM`]. Disabled, unbound, newer-format and
//! degenerate volumes draw nothing, as in the runtime overlay; editor-internal entities
//! (`visibility::is_hidden`) draw nothing, as their icons. The flag is [`crate::views::ShowFlags::post_volumes`] (`editor.set_viewport
//! { post_volumes }`), off by default and hidden in game view.
//!
//! The live panel adds [`post_volume_shapes`] to its overlay shapes; captures paint them with
//! [`paint_capture_post_volumes`] after the other overlays.

use regia_core::math::{Vec2, Vec3};
use regia_ecs::prelude::*;
use regia_render::post_volume_gizmos::{BAND_DIM, priority_color};
use regia_render::{PostVolume, PostVolumeShape};
use regia_scene::GlobalTransform;

use crate::overlays::OverlayShape;
use crate::render::PaneFrame;
use crate::views::{ViewCamera, ViewportLayout};

/// Most post-volume segments one pane draws (a scene full of convex hulls draws its first ones).
pub const MAX_POST_VOLUME_SEGMENTS: usize = 8192;

fn color32(c: regia_core::math::Color, scale: f32) -> egui::Color32 {
    let to = |v: f32| ((v * scale).clamp(0.0, 1.0) * 255.0).round() as u8;
    egui::Color32::from_rgb(to(c.r), to(c.g), to(c.b))
}

/// The shapes the runtime overlay draws too (its `drawable` rule): an unbound volume covers the
/// world, and a zero or non-finite box or sphere and an invalid hull contribute nothing.
fn drawable(shape: &PostVolumeShape) -> bool {
    match shape {
        PostVolumeShape::Unbound => false,
        PostVolumeShape::Box { half_extents } => half_extents.iter().all(|h| h.is_finite() && *h != 0.0),
        PostVolumeShape::Sphere { radius } => radius.is_finite() && *radius != 0.0,
        PostVolumeShape::ConvexHull { .. } => shape.is_valid(),
    }
}

/// Every drawable post volume's outline as world segments with their colors: the shape in its
/// priority color, the blend band's outer edge dimmed. Sorted by entity so the cap
/// ([`MAX_POST_VOLUME_SEGMENTS`]) is stable.
#[must_use]
pub fn post_volume_segments(world: &World) -> Vec<(Vec3, Vec3, egui::Color32)> {
    let _span = tracing::debug_span!(target: "regia_editor", "post_volume_overlay").entered();
    let Some(mut query) = world.try_query::<(Entity, &PostVolume, &GlobalTransform)>() else {
        return Vec::new();
    };
    let mut volumes: Vec<(Entity, &PostVolume, &GlobalTransform)> = query
        .iter(world)
        .filter(|(entity, volume, _)| {
            volume.enabled
                && volume.is_supported()
                && drawable(&volume.shape)
                && !crate::visibility::is_hidden(world, *entity)
        })
        .collect();
    volumes.sort_by_key(|(entity, _, _)| *entity);
    let mut out = Vec::new();
    for (_, volume, world_from_local) in volumes {
        let Some(outline) = regia_render::post_volume_outline(volume, world_from_local) else {
            continue;
        };
        let base = priority_color(volume.priority);
        let shape = color32(base, 1.0);
        let band = color32(base, BAND_DIM);
        let lines = outline
            .shape
            .iter()
            .map(|s| (s, shape))
            .chain(outline.band.iter().map(|s| (s, band)));
        for ([a, b], color) in lines {
            if out.len() >= MAX_POST_VOLUME_SEGMENTS {
                return out;
            }
            out.push((Vec3::from_array(*a), Vec3::from_array(*b), color));
        }
    }
    out
}

/// A world segment clipped in front of the eye and projected into the pane (the rule the other
/// viewport helpers use).
fn project_segment(view: &ViewCamera, size: Vec2, a: Vec3, b: Vec3) -> Option<(Vec2, Vec2)> {
    let near = if view.ortho {
        view.near * 0.5
    } else {
        view.near * 2.5
    };
    let (a, b) = crate::viewport::clip_segment_to_near(view.eye, view.forward(), near, a, b)?;
    Some((view.project(a, size)?, view.project(b, size)?))
}

/// [`post_volume_segments`] as pane lines for a view of `size` pixels.
#[must_use]
pub fn post_volume_shapes(world: &World, view: &ViewCamera, size: Vec2) -> Vec<OverlayShape> {
    post_volume_segments(world)
        .into_iter()
        .filter_map(|(a, b, color)| {
            project_segment(view, size, a, b).map(|(a, b)| OverlayShape::Line {
                a,
                b,
                color,
                thickness: 0,
            })
        })
        .collect()
}

/// Paints the post-volume outlines into a captured pane when `layout`'s effective show flags
/// ask for them (never through a possessed game camera). `None` reads the world's
/// [`ViewportLayout`].
pub fn paint_capture_post_volumes(frame: &mut PaneFrame, world: &World, layout: Option<&ViewportLayout>) {
    if frame.info.possessed {
        return;
    }
    let show = match layout {
        Some(layout) => layout.effective_show(),
        None => world
            .get_resource::<ViewportLayout>()
            .map(ViewportLayout::effective_show)
            .unwrap_or_default(),
    };
    if !show.post_volumes {
        return;
    }
    let size = Vec2::new(frame.image.width() as f32, frame.image.height() as f32);
    let shapes = post_volume_shapes(world, &frame.view, size);
    crate::overlays::paint_shapes(&mut frame.image, &shapes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::viewport::EditorCamera;
    use crate::views::{ShowFlags, ViewKind};
    use regia_scene::prelude::Transform;

    fn spawn_volume(world: &mut World, at: Vec3, volume: PostVolume) -> Entity {
        let transform = Transform::from_translation(at);
        world
            .spawn((transform, GlobalTransform(transform.compute_affine()), volume))
            .id()
    }

    fn boxed(blend: f32) -> PostVolume {
        PostVolume {
            shape: PostVolumeShape::Box {
                half_extents: [1.0, 1.0, 1.0],
            },
            blend_distance: blend,
            ..PostVolume::default()
        }
    }

    #[test]
    fn a_box_is_twelve_edges_and_its_band_twelve_more() {
        let mut world = World::new();
        spawn_volume(&mut world, Vec3::ZERO, boxed(0.0));
        let hard = post_volume_segments(&world);
        assert_eq!(hard.len(), 12, "a box is twelve edges");
        assert!(hard.iter().all(|(_, _, c)| *c == color32(priority_color(0), 1.0)));

        let mut world = World::new();
        spawn_volume(&mut world, Vec3::ZERO, boxed(0.5));
        let banded = post_volume_segments(&world);
        assert_eq!(banded.len(), 24, "shape plus band");
        let reach = banded
            .iter()
            .flat_map(|(a, b, _)| [a.x, b.x])
            .fold(f32::MIN, f32::max);
        assert!(
            (reach - 1.5).abs() < 1e-4,
            "the band reaches blend_distance out: {reach}"
        );
    }

    #[test]
    fn disabled_unbound_and_editor_internal_volumes_draw_nothing() {
        let mut world = World::new();
        spawn_volume(
            &mut world,
            Vec3::ZERO,
            PostVolume {
                enabled: false,
                ..boxed(0.0)
            },
        );
        spawn_volume(
            &mut world,
            Vec3::ZERO,
            PostVolume {
                shape: PostVolumeShape::Unbound,
                ..PostVolume::default()
            },
        );
        assert!(
            post_volume_segments(&world).is_empty(),
            "disabled and unbound volumes draw nothing"
        );
        let shown = spawn_volume(&mut world, Vec3::ZERO, boxed(0.0));
        assert_eq!(post_volume_segments(&world).len(), 12);
        world.entity_mut(shown).insert(crate::visibility::EditorHidden);
        assert!(
            post_volume_segments(&world).is_empty(),
            "an editor-internal volume draws nothing"
        );
    }

    #[test]
    fn the_flag_is_off_by_default_and_game_view_hides_it() {
        assert!(!ShowFlags::default().post_volumes);
        let layout = ViewportLayout {
            show: ShowFlags {
                post_volumes: true,
                ..ShowFlags::default()
            },
            game_view: true,
            ..ViewportLayout::default()
        };
        assert!(!layout.effective_show().post_volumes);
    }

    #[test]
    fn the_outline_projects_into_the_pane() {
        let mut world = World::new();
        spawn_volume(&mut world, Vec3::new(0.0, 1.0, 0.0), boxed(0.25));
        let view = ViewCamera::of(ViewKind::Perspective, &EditorCamera::default());
        let size = Vec2::new(160.0, 120.0);
        let shapes = post_volume_shapes(&world, &view, size);
        assert_eq!(shapes.len(), 24);
        let mut image = egui::ColorImage::new([160, 120], vec![egui::Color32::BLACK; 160 * 120]);
        crate::overlays::paint_shapes(&mut image, &shapes);
        let lit = image
            .pixels
            .iter()
            .filter(|p| **p == color32(priority_color(0), 1.0))
            .count();
        assert!(lit > 40, "the box's edges paint: {lit}");
    }
}
