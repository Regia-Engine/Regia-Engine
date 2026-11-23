// SPDX-License-Identifier: MPL-2.0
//! Viewport gizmos for entity audio: every `AudioEmitter` gets a small speaker marker; selected
//! emitters show their `min_distance` (full volume) and `max_distance` (silence) spheres and,
//! when directional, their inner and outer cones; selected `AudioReverbZone`s show their volume.
//!
//! The geometry is built here as world-space line segments so the live egui panel and the
//! headless capture path draw the same thing (through [`crate::overlays::build_overlays`]).

use regia::audio::{AudioEmitter, AudioReverbZone, ReverbZoneShape};
use regia_core::math::{Quat, Vec3};
use regia_ecs::ecs::entity::Entity;
use regia_ecs::prelude::*;
use regia_scene::prelude::{GlobalTransform, Transform};

/// Marker and `min_distance` sphere colour.
pub const AUDIO_MIN: egui::Color32 = egui::Color32::from_rgb(80, 200, 230);
/// `max_distance` sphere colour.
pub const AUDIO_MAX: egui::Color32 = egui::Color32::from_rgb(40, 110, 160);
/// Cone colour.
pub const AUDIO_CONE: egui::Color32 = egui::Color32::from_rgb(240, 140, 220);
/// Reverb-zone volume colour.
pub const REVERB_ZONE: egui::Color32 = egui::Color32::from_rgb(170, 120, 240);

/// Segments per circle.
const CIRCLE_STEPS: usize = 32;
/// Cone length cap, metres (a 50 m cone would fill the view).
const CONE_LENGTH: f32 = 4.0;
/// Speaker-marker half size, metres.
const MARKER: f32 = 0.25;

/// One coloured world-space segment.
pub type Segment = (Vec3, Vec3, egui::Color32);

/// World pose of an entity: the propagated `GlobalTransform` (the local transform when
/// propagation has not run yet).
fn pose(world: &World, entity: Entity) -> Option<(Vec3, Quat)> {
    let local = world.get::<Transform>(entity);
    match world.get::<GlobalTransform>(entity) {
        Some(global)
            if *global != GlobalTransform::IDENTITY || local.is_none_or(|l| *l == Transform::IDENTITY) =>
        {
            let (_, rotation, translation) = global.0.to_scale_rotation_translation();
            Some((translation, rotation.normalize()))
        }
        _ => local.map(|l| (l.translation, l.rotation)),
    }
}

/// A circle of `radius` around `centre` in the plane spanned by unit `u` and `v`.
fn circle(out: &mut Vec<Segment>, centre: Vec3, u: Vec3, v: Vec3, radius: f32, color: egui::Color32) {
    let point = |i: usize| {
        let a = std::f32::consts::TAU * i as f32 / CIRCLE_STEPS as f32;
        centre + (u * a.cos() + v * a.sin()) * radius
    };
    for i in 0..CIRCLE_STEPS {
        out.push((point(i), point(i + 1), color));
    }
}

/// Three great circles of a sphere.
fn sphere(out: &mut Vec<Segment>, centre: Vec3, rotation: Quat, radius: f32, color: egui::Color32) {
    let (x, y, z) = (rotation * Vec3::X, rotation * Vec3::Y, rotation * Vec3::Z);
    circle(out, centre, x, y, radius, color);
    circle(out, centre, x, z, radius, color);
    circle(out, centre, y, z, radius, color);
}

/// A cone of half-angle `degrees` along `forward`, `length` long: rays plus the end ring.
fn cone(
    out: &mut Vec<Segment>,
    apex: Vec3,
    rotation: Quat,
    degrees: f32,
    length: f32,
    rays: usize,
    color: egui::Color32,
) {
    let half = degrees.clamp(0.0, 179.0).to_radians();
    let forward = rotation * Vec3::NEG_Z;
    let (u, v) = (rotation * Vec3::X, rotation * Vec3::Y);
    let ring_centre = apex + forward * length * half.cos();
    let ring_radius = length * half.sin();
    for i in 0..rays {
        let a = std::f32::consts::TAU * i as f32 / rays as f32;
        out.push((
            apex,
            ring_centre + (u * a.cos() + v * a.sin()) * ring_radius,
            color,
        ));
    }
    circle(out, ring_centre, u, v, ring_radius, color);
}

/// The audio gizmo segments for `world`, with full detail for the `selected` entities.
pub fn audio_gizmo_segments(world: &World, selected: &[Entity]) -> Vec<Segment> {
    let colors = &crate::theme::tokens(world).overlay;
    let mut out = Vec::new();
    // `try_query` works on a shared world (the capture path) and is `None` until the component
    // was ever registered.
    let emitters: Vec<(Entity, AudioEmitter)> = world
        .try_query::<(Entity, &AudioEmitter)>()
        .map(|mut q| q.iter(world).map(|(e, a)| (e, a.clone())).collect())
        .unwrap_or_default();
    for (entity, emitter) in emitters {
        let Some((position, rotation)) = pose(world, entity) else {
            continue;
        };
        for axis in [Vec3::X, Vec3::Y, Vec3::Z] {
            out.push((
                position - axis * MARKER,
                position + axis * MARKER,
                colors.audio_min,
            ));
        }
        if !selected.contains(&entity) {
            continue;
        }
        sphere(
            &mut out,
            position,
            rotation,
            emitter.min_distance.max(0.0),
            colors.audio_min,
        );
        sphere(
            &mut out,
            position,
            rotation,
            emitter.max_distance.max(0.0),
            colors.audio_max,
        );
        if emitter.cone_inner_degrees < 180.0 {
            let length = emitter
                .min_distance
                .max(1.0)
                .min(emitter.max_distance)
                .min(CONE_LENGTH);
            cone(
                &mut out,
                position,
                rotation,
                emitter.cone_inner_degrees,
                length,
                4,
                colors.audio_min,
            );
            cone(
                &mut out,
                position,
                rotation,
                emitter.cone_outer_degrees,
                length,
                8,
                colors.audio_cone,
            );
        }
    }
    let zones: Vec<(Entity, AudioReverbZone)> = world
        .try_query::<(Entity, &AudioReverbZone)>()
        .map(|mut q| {
            q.iter(world)
                .filter(|(e, _)| selected.contains(e))
                .map(|(e, z)| (e, z.clone()))
                .collect()
        })
        .unwrap_or_default();
    for (entity, zone) in zones {
        let Some((centre, rotation)) = pose(world, entity) else {
            continue;
        };
        match zone.shape {
            ReverbZoneShape::Sphere { radius } => {
                sphere(&mut out, centre, rotation, radius, colors.reverb_zone)
            }
            ReverbZoneShape::Box { half_extents } => {
                let corner = |sx: f32, sy: f32, sz: f32| {
                    centre
                        + rotation * Vec3::new(sx * half_extents.x, sy * half_extents.y, sz * half_extents.z)
                };
                for (a, b) in [
                    ((-1., -1., -1.), (1., -1., -1.)),
                    ((-1., 1., -1.), (1., 1., -1.)),
                    ((-1., -1., 1.), (1., -1., 1.)),
                    ((-1., 1., 1.), (1., 1., 1.)),
                    ((-1., -1., -1.), (-1., 1., -1.)),
                    ((1., -1., -1.), (1., 1., -1.)),
                    ((-1., -1., 1.), (-1., 1., 1.)),
                    ((1., -1., 1.), (1., 1., 1.)),
                    ((-1., -1., -1.), (-1., -1., 1.)),
                    ((1., -1., -1.), (1., -1., 1.)),
                    ((-1., 1., -1.), (-1., 1., 1.)),
                    ((1., 1., -1.), (1., 1., 1.)),
                ] {
                    out.push((corner(a.0, a.1, a.2), corner(b.0, b.1, b.2), colors.reverb_zone));
                }
            }
            ReverbZoneShape::Capsule { radius, half_length } => {
                // The axis is local Y: a ring at each end of the segment and at the middle, and
                // four lines joining the end rings.
                let (u, v, axis) = (rotation * Vec3::X, rotation * Vec3::Z, rotation * Vec3::Y);
                for t in [-1.0f32, 0.0, 1.0] {
                    circle(
                        &mut out,
                        centre + axis * (half_length * t),
                        u,
                        v,
                        radius,
                        colors.reverb_zone,
                    );
                }
                for dir in [u, -u, v, -v] {
                    out.push((
                        centre + axis * half_length + dir * radius,
                        centre - axis * half_length + dir * radius,
                        colors.reverb_zone,
                    ));
                }
            }
            ReverbZoneShape::Global => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(segments: &[Segment], color: egui::Color32) -> usize {
        segments.iter().filter(|s| s.2 == color).count()
    }

    #[test]
    fn unselected_emitters_get_a_marker_and_selected_ones_their_ranges() {
        let mut world = World::new();
        let emitter = world
            .spawn((
                AudioEmitter {
                    min_distance: 2.0,
                    max_distance: 30.0,
                    ..AudioEmitter::default()
                },
                Transform::from_translation(Vec3::new(5.0, 0.0, 0.0)),
            ))
            .id();
        let marker_only = audio_gizmo_segments(&world, &[]);
        assert_eq!(marker_only.len(), 3);
        let full = audio_gizmo_segments(&world, &[emitter]);
        // Omnidirectional (default cone 360°): marker + two spheres, no cone.
        assert_eq!(count(&full, AUDIO_MAX), 3 * CIRCLE_STEPS);
        assert_eq!(count(&full, AUDIO_CONE), 0);
        // The max-distance sphere's points are 30 m from the emitter.
        for (a, b, _) in full.iter().filter(|s| s.2 == AUDIO_MAX) {
            assert!(((*a - Vec3::new(5.0, 0.0, 0.0)).length() - 30.0).abs() < 1e-3);
            assert!(((*b - Vec3::new(5.0, 0.0, 0.0)).length() - 30.0).abs() < 1e-3);
        }
    }

    #[test]
    fn a_directional_emitter_shows_its_cone_along_its_facing() {
        let mut world = World::new();
        let rotation = Quat::from_rotation_y(std::f32::consts::FRAC_PI_2); // faces −X
        let emitter = world
            .spawn((
                AudioEmitter {
                    cone_inner_degrees: 20.0,
                    cone_outer_degrees: 45.0,
                    ..AudioEmitter::default()
                },
                Transform {
                    rotation,
                    ..Default::default()
                },
            ))
            .id();
        let segments = audio_gizmo_segments(&world, &[emitter]);
        let rays: Vec<&Segment> = segments
            .iter()
            .filter(|s| s.2 == AUDIO_CONE && s.0 == Vec3::ZERO)
            .collect();
        assert_eq!(rays.len(), 8);
        for (_, end, _) in rays {
            let dir = end.normalize();
            let angle = dir.dot(Vec3::NEG_X).acos().to_degrees();
            assert!((angle - 45.0).abs() < 0.1, "{angle}");
        }
    }

    #[test]
    fn selected_reverb_zones_draw_their_box() {
        let mut world = World::new();
        let zone = world
            .spawn((AudioReverbZone::default(), Transform::default()))
            .id();
        assert_eq!(count(&audio_gizmo_segments(&world, &[zone]), REVERB_ZONE), 12);
        assert_eq!(count(&audio_gizmo_segments(&world, &[]), REVERB_ZONE), 0);
    }
}
