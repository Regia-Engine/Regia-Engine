// SPDX-License-Identifier: MPL-2.0
//! The `post_volumes` debug overlay (`docs/specs/83-prefab-profiler-volumes.md` § 3.2, ADR-2323):
//! every enabled [`PostVolume`] drawn as its shape plus the outer edge of its blend band, in a
//! colour keyed by priority. [`PostVolumeShape::Unbound`] volumes cover the world and draw nothing.
//!
//! Off by default like every debug overlay; turn it on with
//! `GizmoConfigStore::set_enabled("post_volumes", true)` or `debug.set_gizmos`.
//!
//! The lines come from [`crate::post_volume_outline`] (post-volume format v3, ADR-2323), the same
//! segments `render.post_volume_gizmos` returns: a box is placed by the full entity transform, a
//! sphere's radius scales by the mean of its X and Z scale, a convex hull draws its bounded edges.
//! The band is the shape with every face pushed out by `blend_distance` (the exact band has
//! rounded edges; the outline bounds it), exact for a sphere.

use regia_app::prelude::*;
use regia_core::math::{Color, Vec3};
use regia_scene::GlobalTransform;

use crate::gizmos::{GizmoBuffer, GizmoConfigGroup, Gizmos, PostVolumeGizmos};
use crate::post_volumes::{PostVolume, PostVolumeShape};

/// Shape colours by priority (`priority.rem_euclid(6)`), so overlapping tiers read apart.
pub const PRIORITY_COLORS: [Color; 6] = [
    Color::rgb(0.95, 0.35, 0.85),
    Color::rgb(0.3, 0.8, 1.0),
    Color::rgb(1.0, 0.75, 0.2),
    Color::rgb(0.45, 1.0, 0.45),
    Color::rgb(1.0, 0.45, 0.35),
    Color::rgb(0.7, 0.6, 1.0),
];

/// Brightness of the blend band's outline relative to the shape's.
pub const BAND_DIM: f32 = 0.45;

/// The shape colour of a volume with `priority`.
#[must_use]
pub fn priority_color(priority: i32) -> Color {
    PRIORITY_COLORS[priority.rem_euclid(PRIORITY_COLORS.len() as i32) as usize]
}

fn dimmed(color: Color) -> Color {
    Color::rgb(color.r * BAND_DIM, color.g * BAND_DIM, color.b * BAND_DIM)
}

/// Draws one volume into `gizmos`: nothing for a disabled, unbound or newer-format volume, or a
/// degenerate shape (a box with a zero or non-finite extent, a sphere with a zero radius, an
/// invalid hull).
///
/// The lines are the segments of [`crate::post_volume_outline`], the data
/// `render.post_volume_gizmos` hands to agents, so what the overlay draws and what an agent reads
/// are one set of segments (a convex hull draws its bounded edges; a hull with rays omits them).
pub fn draw_post_volume<G: GizmoConfigGroup>(
    gizmos: &mut GizmoBuffer<G>,
    volume: &PostVolume,
    world_from_local: &GlobalTransform,
) {
    if !volume.enabled || !volume.is_supported() || !drawable(&volume.shape) {
        return;
    }
    let Some(outline) = crate::post_volume_outline(volume, world_from_local) else {
        return;
    };
    let color = priority_color(volume.priority);
    let band_color = dimmed(color);
    for [a, b] in &outline.shape {
        gizmos.line(Vec3::from(*a), Vec3::from(*b), color);
    }
    for [a, b] in &outline.band {
        gizmos.line(Vec3::from(*a), Vec3::from(*b), band_color);
    }
}

/// The overlay skips shapes that contribute nothing to the blend, which the outline (an
/// authoring view) would still draw flat.
fn drawable(shape: &PostVolumeShape) -> bool {
    match shape {
        PostVolumeShape::Unbound => false,
        PostVolumeShape::Box { half_extents } => half_extents.iter().all(|h| h.is_finite() && *h != 0.0),
        PostVolumeShape::Sphere { radius } => radius.is_finite() && *radius != 0.0,
        PostVolumeShape::ConvexHull { .. } => shape.is_valid(),
    }
}

/// Draws every post volume (`PostUpdate`, after transforms, only while the group is on).
pub fn draw_post_volumes(
    volumes: Query<(&PostVolume, &GlobalTransform)>,
    mut gizmos: Gizmos<PostVolumeGizmos>,
) {
    let _span = tracing::info_span!("draw_post_volumes").entered();
    for (volume, world_from_local) in &volumes {
        draw_post_volume(&mut gizmos, volume, world_from_local);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gizmos::{DefaultGizmoConfigGroup, GizmoConfigStore, GizmoPlugin, GizmoStorage};

    fn segments<G: GizmoConfigGroup>(b: &GizmoBuffer<G>) -> usize {
        let strips: usize = b
            .strip_positions
            .split(|p| p.is_nan())
            .map(|r| r.len().saturating_sub(1))
            .sum();
        b.list_positions.len() / 2 + strips
    }

    fn extent_x<G: GizmoConfigGroup>(b: &GizmoBuffer<G>) -> f32 {
        b.strip_positions
            .iter()
            .chain(&b.list_positions)
            .filter(|p| p.is_finite())
            .map(|p| p.x)
            .fold(f32::MIN, f32::max)
    }

    fn boxed(blend: f32) -> PostVolume {
        PostVolume {
            shape: PostVolumeShape::Box {
                half_extents: [1.0, 2.0, 3.0],
            },
            blend_distance: blend,
            ..PostVolume::default()
        }
    }

    #[test]
    fn shapes_and_bands_draw_where_the_blend_reaches() {
        let at = GlobalTransform::IDENTITY;
        let mut hard = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        draw_post_volume(&mut hard, &boxed(0.0), &at);
        assert_eq!(segments(&hard), 12, "a box is twelve edges");
        assert!((extent_x(&hard) - 1.0).abs() < 1e-5);

        let mut banded = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        draw_post_volume(&mut banded, &boxed(0.5), &at);
        assert_eq!(segments(&banded), 24, "shape plus band");
        assert!(
            (extent_x(&banded) - 1.5).abs() < 1e-5,
            "the band reaches blend_distance out"
        );

        // Scale is per axis for a box.
        let scaled = GlobalTransform(regia_core::math::Affine3A::from_scale(Vec3::new(2.0, 1.0, 1.0)));
        let mut wide = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        draw_post_volume(&mut wide, &boxed(0.0), &scaled);
        assert!((extent_x(&wide) - 2.0).abs() < 1e-5);

        let sphere = PostVolume {
            shape: PostVolumeShape::Sphere { radius: 2.0 },
            blend_distance: 1.0,
            ..PostVolume::default()
        };
        let mut round = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        draw_post_volume(&mut round, &sphere, &at);
        assert!(segments(&round) > 0);
        assert!((extent_x(&round) - 3.0).abs() < 1e-3, "band at radius + blend");
    }

    #[test]
    fn unbound_disabled_newer_and_degenerate_volumes_draw_nothing() {
        let at = GlobalTransform::IDENTITY;
        let cases = [
            PostVolume {
                shape: PostVolumeShape::Unbound,
                blend_distance: 4.0,
                ..PostVolume::default()
            },
            PostVolume {
                enabled: false,
                ..boxed(1.0)
            },
            PostVolume {
                format_version: crate::post_volumes::POST_VOLUME_FORMAT_VERSION + 1,
                ..boxed(1.0)
            },
            PostVolume {
                shape: PostVolumeShape::Box {
                    half_extents: [0.0, 1.0, 1.0],
                },
                ..PostVolume::default()
            },
            PostVolume {
                shape: PostVolumeShape::Sphere { radius: f32::NAN },
                ..PostVolume::default()
            },
        ];
        for volume in &cases {
            let mut b = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
            draw_post_volume(&mut b, volume, &at);
            assert_eq!(segments(&b), 0, "{volume:?}");
        }
    }

    #[test]
    fn hulls_draw_the_outline_agents_read() {
        // A hull built from a box's planes draws the box: twelve edges, plus twelve for the band.
        let at = GlobalTransform(regia_core::math::Affine3A::from_rotation_translation(
            regia_core::math::Quat::from_rotation_y(0.4),
            Vec3::new(3.0, 0.0, -2.0),
        ));
        let hull = PostVolume {
            shape: PostVolumeShape::ConvexHull {
                planes: crate::post_volume_shapes::box_hull_planes([1.0, 2.0, 3.0]),
            },
            blend_distance: 0.5,
            ..PostVolume::default()
        };
        let mut drawn = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        draw_post_volume(&mut drawn, &hull, &at);
        let outline = crate::post_volume_outline(&hull, &at).expect("a bounded hull outlines");
        assert_eq!(outline.shape.len(), 12);
        assert_eq!(outline.band.len(), 12);
        assert_eq!(segments(&drawn), outline.shape.len() + outline.band.len());
        // The drawn points are the outline's points, in order (shape first, then band).
        let expected: Vec<Vec3> = outline
            .shape
            .iter()
            .chain(&outline.band)
            .flat_map(|[a, b]| [Vec3::from(*a), Vec3::from(*b)])
            .collect();
        assert_eq!(drawn.list_positions, expected);

        // The same box as a box shape draws the same twelve shape edges' extent.
        let mut boxed_drawn = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        draw_post_volume(&mut boxed_drawn, &boxed(0.5), &at);
        assert_eq!(segments(&boxed_drawn), 24);
        assert!((extent_x(&boxed_drawn) - extent_x(&drawn)).abs() < 1e-3);

        // An invalid hull (no planes) draws nothing.
        let empty = PostVolume {
            shape: PostVolumeShape::ConvexHull { planes: Vec::new() },
            ..PostVolume::default()
        };
        let mut none = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        draw_post_volume(&mut none, &empty, &at);
        assert_eq!(segments(&none), 0);
    }

    #[test]
    fn priorities_get_distinct_colours_and_negative_priorities_wrap() {
        assert_ne!(priority_color(0), priority_color(1));
        assert_eq!(priority_color(-1), PRIORITY_COLORS[5]);
        assert_eq!(priority_color(6), PRIORITY_COLORS[0]);
    }

    #[test]
    fn the_overlay_only_draws_when_enabled() {
        let mut app = App::new();
        app.add_plugins(GizmoPlugin);
        app.world_mut().spawn((boxed(0.0), GlobalTransform::IDENTITY));
        app.run_schedule(PostUpdate);
        assert!(
            app.world()
                .resource::<GizmoStorage<PostVolumeGizmos>>()
                .is_empty()
        );
        app.world_mut()
            .resource_mut::<GizmoConfigStore>()
            .set_enabled("post_volumes", true);
        app.run_schedule(PostUpdate);
        assert_eq!(
            app.world()
                .resource::<GizmoStorage<PostVolumeGizmos>>()
                .view()
                .list_positions
                .len(),
            24
        );
    }
}
