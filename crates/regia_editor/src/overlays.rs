// SPDX-License-Identifier: MPL-2.0
//! Editor overlays on top of the rendered picture (`docs/specs/19-editor.md` §§ 2–3): the
//! reference grid, icons for lights, cameras, decals, audio emitters and player starts (with a
//! facing arrow on the floor), trigger volumes (sensor colliders, dashed; ADR-7468), the selection's bounds
//! with light ranges, spot cones and camera frustums, audio ranges, cones and reverb zones
//! ([`crate::audio_gizmo`]), the selection outline, and (captures only) the gizmo.
//!
//! [`build_overlays`] turns the world into 2D shapes once, in the pane's pixel space, through
//! the same [`ViewCamera`] the picture was rendered with. Captures rasterize them into the
//! pixels ([`paint_shapes`]); the live panel draws the same shapes with egui
//! ([`paint_shapes_egui`]). The grid ([`paint_grid`], depth-tested so geometry hides it) and
//! the outline come from the entity-id buffer and are stamped into the image in both cases.

use regia_core::math::{Quat, Vec2, Vec3};
use regia_ecs::ecs::entity::Entity;
use regia_ecs::prelude::*;
use regia_scene::prelude::Transform;

use crate::render::PaneFrame;
use crate::selection::Selection;
use crate::theme::OverlayTokens;
use crate::viewport::{GIZMO_RING, GIZMO_X, GIZMO_Y, GIZMO_Z, GRID_MAJOR, GRID_MINOR, GizmoMode, GizmoState};
use crate::views::{ShowFlags, ViewCamera, ViewKind, ViewportLayout};

/// Selection outline (from the id buffer).
pub const SELECTION_OUTLINE: egui::Color32 = egui::Color32::from_rgb(255, 150, 20);
/// Selection bounds and ranges.
pub const BOUNDS: egui::Color32 = egui::Color32::from_rgb(255, 236, 140);
/// Light icons.
pub const ICON_LIGHT: egui::Color32 = egui::Color32::from_rgb(255, 222, 70);
/// Camera icons.
pub const ICON_CAMERA: egui::Color32 = egui::Color32::from_rgb(90, 200, 255);
/// Decal icons.
pub const ICON_DECAL: egui::Color32 = egui::Color32::from_rgb(200, 120, 255);
/// Audio emitter icons.
pub const ICON_AUDIO: egui::Color32 = egui::Color32::from_rgb(110, 250, 200);
/// Player start icons and their facing arrow (ADR-7468).
pub const ICON_PLAYER_START: egui::Color32 = egui::Color32::from_rgb(150, 255, 40);
/// Trigger volumes: sensor colliders, drawn dashed with the icons (ADR-7468).
pub const TRIGGER_VOLUME: egui::Color32 = egui::Color32::from_rgb(255, 60, 200);
/// Navigation mesh polygon outlines (Show ▸ Navigation, ADR-7468).
pub const NAVIGATION: egui::Color32 = egui::Color32::from_rgb(26, 191, 255);
/// Navigation mesh off-mesh links.
pub const NAVIGATION_LINK: egui::Color32 = egui::Color32::from_rgb(255, 102, 230);

/// Most navigation segments one pane draws (a huge baked mesh draws its first polygons).
pub const MAX_NAVIGATION_SEGMENTS: usize = 50_000;

/// Every colour the overlays paint (captures skip them when measuring the picture).
pub const OVERLAY_COLORS: [egui::Color32; 16] = [
    GRID_MAJOR,
    GRID_MINOR,
    GIZMO_X,
    GIZMO_Y,
    GIZMO_Z,
    GIZMO_RING,
    SELECTION_OUTLINE,
    BOUNDS,
    ICON_LIGHT,
    ICON_CAMERA,
    ICON_DECAL,
    ICON_AUDIO,
    ICON_PLAYER_START,
    TRIGGER_VOLUME,
    NAVIGATION,
    NAVIGATION_LINK,
];

/// One 2D overlay primitive in pane pixels.
#[derive(Debug, Clone, PartialEq)]
pub enum OverlayShape {
    /// A line segment.
    Line {
        /// Start.
        a: Vec2,
        /// End.
        b: Vec2,
        /// Colour.
        color: egui::Color32,
        /// Half-thickness in pixels (0 = 1 px).
        thickness: i32,
    },
    /// A circle outline.
    Circle {
        /// Centre.
        center: Vec2,
        /// Radius, pixels.
        radius: f32,
        /// Colour.
        color: egui::Color32,
    },
    /// A text label (live panel only; captures skip text).
    Text {
        /// Anchor (left-bottom).
        at: Vec2,
        /// The text.
        text: String,
        /// Colour.
        color: egui::Color32,
    },
}

/// What kind of icon an entity gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconKind {
    /// Directional light (sun).
    Sun,
    /// Point light.
    Point,
    /// Spot light.
    Spot,
    /// Camera (render camera or framework camera rig).
    Camera,
    /// Placed decal.
    Decal,
    /// Audio emitter.
    Audio,
}

/// The icon an entity shows, if any (first match: lights, cameras, decals, audio). A player start
/// has no icon kind: it keeps its proxy cube (so it stays pickable in the viewport) and draws its
/// own flag and floor arrow (`player_start_helper`, with the Icons flag).
#[must_use]
pub fn icon_of(world: &World, entity: Entity) -> Option<IconKind> {
    if world.get::<regia_render::DirectionalLight>(entity).is_some() {
        Some(IconKind::Sun)
    } else if world.get::<regia_render::SpotLight>(entity).is_some() {
        Some(IconKind::Spot)
    } else if world.get::<regia_render::PointLight>(entity).is_some() {
        Some(IconKind::Point)
    } else if world.get::<regia_render::Camera>(entity).is_some()
        || world.get::<regia_framework::camera::CameraRig>(entity).is_some()
    {
        Some(IconKind::Camera)
    } else if world.get::<regia_vfx::StaticDecal>(entity).is_some() {
        Some(IconKind::Decal)
    } else if world.get::<regia_audio::AudioEmitter>(entity).is_some() {
        Some(IconKind::Audio)
    } else {
        None
    }
}

/// The world-space corners of an entity's bounds: its resolved mesh bounds, else the unit
/// proxy cube, transformed by its world matrix.
#[must_use]
pub fn world_bounds_corners(world: &World, entity: Entity) -> Option<[Vec3; 8]> {
    let model = crate::mesh_source::world_matrix(world, entity)?;
    let (min, max) =
        crate::mesh_source::model_bounds(world, entity).unwrap_or((Vec3::splat(-0.5), Vec3::splat(0.5)));
    let mut corners = [Vec3::ZERO; 8];
    for (i, corner) in corners.iter_mut().enumerate() {
        let local = Vec3::new(
            if i & 1 == 0 { min.x } else { max.x },
            if i & 2 == 0 { min.y } else { max.y },
            if i & 4 == 0 { min.z } else { max.z },
        );
        *corner = model.transform_point3(local);
    }
    Some(corners)
}

/// World-space AABB of an entity (see [`world_bounds_corners`]).
#[must_use]
pub fn world_aabb(world: &World, entity: Entity) -> Option<(Vec3, Vec3)> {
    let corners = world_bounds_corners(world, entity)?;
    let min = corners.iter().copied().fold(Vec3::splat(f32::MAX), Vec3::min);
    let max = corners.iter().copied().fold(Vec3::splat(f32::MIN), Vec3::max);
    Some((min, max))
}

/// A world segment, clipped in front of the eye, projected into the pane.
fn segment(view: &ViewCamera, size: Vec2, a: Vec3, b: Vec3) -> Option<(Vec2, Vec2)> {
    let near = if view.ortho {
        view.near * 0.5
    } else {
        view.near * 2.5
    };
    let (a, b) = crate::viewport::clip_segment_to_near(view.eye, view.forward(), near, a, b)?;
    Some((view.project(a, size)?, view.project(b, size)?))
}

pub(crate) fn push_line(
    out: &mut Vec<OverlayShape>,
    view: &ViewCamera,
    size: Vec2,
    a: Vec3,
    b: Vec3,
    color: egui::Color32,
) {
    if let Some((a, b)) = segment(view, size, a, b) {
        out.push(OverlayShape::Line {
            a,
            b,
            color,
            thickness: 0,
        });
    }
}

/// A world-space circle (`segments` chords) around `center` in the plane spanned by `u`, `v`.
pub(crate) fn push_world_circle(
    out: &mut Vec<OverlayShape>,
    view: &ViewCamera,
    size: Vec2,
    center: Vec3,
    u: Vec3,
    v: Vec3,
    color: egui::Color32,
) {
    let segments = 40;
    for i in 0..segments {
        let a0 = std::f32::consts::TAU * i as f32 / segments as f32;
        let a1 = std::f32::consts::TAU * (i + 1) as f32 / segments as f32;
        push_line(
            out,
            view,
            size,
            center + u * a0.cos() + v * a0.sin(),
            center + u * a1.cos() + v * a1.sin(),
            color,
        );
    }
}

/// The reference grid for a pane: the XZ plane for perspective and top panes, XY for front,
/// ZY for side; centred on the camera target, with a spacing that keeps lines ≥ 8 px apart.
fn grid(out: &mut Vec<OverlayShape>, view: &ViewCamera, kind: ViewKind, size: Vec2, colors: &OverlayTokens) {
    for (a, b, color) in grid_lines(view, kind, size, colors) {
        push_line(out, view, size, a, b, color);
    }
}

/// The grid's world-space segments and colours (see [`grid`]).
fn grid_lines(
    view: &ViewCamera,
    kind: ViewKind,
    size: Vec2,
    colors: &OverlayTokens,
) -> Vec<(Vec3, Vec3, egui::Color32)> {
    let mut out = Vec::new();
    let (u, v) = match kind {
        ViewKind::Perspective | ViewKind::Top | ViewKind::Bottom => (Vec3::X, Vec3::Z),
        ViewKind::Front | ViewKind::Back => (Vec3::X, Vec3::Y),
        ViewKind::Side | ViewKind::OppositeSide => (Vec3::Z, Vec3::Y),
    };
    let mut step = 1.0_f32;
    if view.ortho {
        let per_px = view.units_per_pixel(size.y);
        while step / per_px < 8.0 {
            step *= 10.0;
        }
        while step / per_px > 80.0 && step > 0.01 {
            step /= 10.0;
        }
    }
    let lines = 20;
    let extent = step * lines as f32;
    // Snap the grid centre to the step so lines do not swim while panning.
    let center = view.target;
    let cu = (center.dot(u) / step).round() * step;
    let cv = (center.dot(v) / step).round() * step;
    let base = if kind == ViewKind::Perspective {
        Vec3::ZERO
    } else {
        center - u * center.dot(u) - v * center.dot(v)
    };
    for i in -lines..=lines {
        let offset = i as f32 * step;
        for (axis, other, c_axis, c_other) in [(u, v, cu, cv), (v, u, cv, cu)] {
            let along = c_axis + offset;
            let major = along.abs() < step * 0.01;
            let a = base + axis * along + other * (c_other - extent);
            let b = base + axis * along + other * (c_other + extent);
            out.push((
                a,
                b,
                if major {
                    colors.grid_major
                } else {
                    colors.grid_minor
                },
            ));
        }
    }
    out
}

/// Stamps the reference grid into the picture, hidden behind geometry.
///
/// Each grid segment is walked one pixel at a time in screen space; the matching world point
/// comes from perspective-correct interpolation (1/depth is linear in screen space), and the
/// pixel is painted only where the point is no farther than the id buffer's eye distance
/// (plus a tolerance, so a ground mesh lying on the grid plane does not hide it). Without an
/// id buffer the grid is painted unoccluded.
pub fn paint_grid(frame: &mut PaneFrame, kind: ViewKind) {
    paint_grid_with(frame, kind, &OverlayTokens::regia_default());
}

/// [`paint_grid`] with the theme's grid colors.
pub fn paint_grid_with(frame: &mut PaneFrame, kind: ViewKind, colors: &OverlayTokens) {
    let width = frame.image.width();
    let height = frame.image.height();
    let size = Vec2::new(width as f32, height as f32);
    let view = frame.view;
    let lines = grid_lines(&view, kind, size, colors);
    let Some(ids) = frame
        .ids
        .as_ref()
        .filter(|ids| ids.width as usize == width && ids.height as usize == height)
    else {
        let mut shapes = Vec::new();
        for (a, b, color) in lines {
            push_line(&mut shapes, &view, size, a, b, color);
        }
        paint_shapes(&mut frame.image, &shapes);
        return;
    };
    let forward = view.forward();
    let near = if view.ortho {
        view.near * 0.5
    } else {
        view.near * 2.5
    };
    let ortho_tolerance = view.units_per_pixel(size.y) * 4.0 + 0.02;
    for (a, b, color) in lines {
        let Some((a, b)) = crate::viewport::clip_segment_to_near(view.eye, forward, near, a, b) else {
            continue;
        };
        let (Some(pa), Some(pb)) = (view.project(a, size), view.project(b, size)) else {
            continue;
        };
        let wa = (a - view.eye).dot(forward).max(1e-4);
        let wb = (b - view.eye).dot(forward).max(1e-4);
        let steps = (pb - pa).abs().max_element().ceil().clamp(1.0, 16_384.0) as u32;
        for i in 0..=steps {
            let s = i as f32 / steps as f32;
            let px = pa + (pb - pa) * s;
            if px.x < 0.0 || px.y < 0.0 || px.x >= size.x || px.y >= size.y {
                continue;
            }
            let t = (s / wb) / ((1.0 - s) / wa + s / wb);
            let distance = (a + (b - a) * t - view.eye).length();
            let index = px.y as usize * width + px.x as usize;
            let occluder = ids.distance.get(index).copied().unwrap_or(f32::INFINITY);
            let tolerance = if view.ortho {
                ortho_tolerance
            } else {
                distance * 0.02 + 0.02
            };
            if distance <= occluder + tolerance {
                frame.image.pixels[index] = color;
            }
        }
    }
}

/// Scales an icon's shapes about its anchor `p` by `scale` and multiplies their alpha by `opacity`
/// (Show ▸ Icons size and opacity, ADR-7770). The default 1 and 1 leave them untouched.
fn restyle_icon(shapes: &mut [OverlayShape], p: Vec2, scale: f32, opacity: f32) {
    if scale == 1.0 && opacity >= 1.0 {
        return;
    }
    let fade = |color: &mut egui::Color32| {
        *color = color.gamma_multiply(opacity.clamp(0.0, 1.0));
    };
    for shape in shapes {
        match shape {
            OverlayShape::Line { a, b, color, .. } => {
                *a = p + (*a - p) * scale;
                *b = p + (*b - p) * scale;
                fade(color);
            }
            OverlayShape::Circle {
                center,
                radius,
                color,
            } => {
                *center = p + (*center - p) * scale;
                *radius *= scale;
                fade(color);
            }
            OverlayShape::Text { at, color, .. } => {
                *at = p + (*at - p) * scale;
                fade(color);
            }
        }
    }
}

/// The icon glyph for `kind` at pixel `p`.
fn icon(out: &mut Vec<OverlayShape>, kind: IconKind, p: Vec2, colors: &OverlayTokens) {
    let line = |out: &mut Vec<OverlayShape>, a: Vec2, b: Vec2, color| {
        out.push(OverlayShape::Line {
            a: p + a,
            b: p + b,
            color,
            thickness: 0,
        });
    };
    match kind {
        IconKind::Sun => {
            out.push(OverlayShape::Circle {
                center: p,
                radius: 5.0,
                color: colors.icon_light,
            });
            for i in 0..8 {
                let a = std::f32::consts::TAU * i as f32 / 8.0;
                let d = Vec2::new(a.cos(), a.sin());
                line(out, d * 7.0, d * 11.0, colors.icon_light);
            }
        }
        IconKind::Point => {
            out.push(OverlayShape::Circle {
                center: p,
                radius: 6.0,
                color: colors.icon_light,
            });
            out.push(OverlayShape::Circle {
                center: p,
                radius: 2.5,
                color: colors.icon_light,
            });
            line(out, Vec2::new(-3.0, 7.0), Vec2::new(3.0, 7.0), colors.icon_light);
            line(out, Vec2::new(-2.0, 9.0), Vec2::new(2.0, 9.0), colors.icon_light);
        }
        IconKind::Spot => {
            out.push(OverlayShape::Circle {
                center: p + Vec2::new(0.0, -5.0),
                radius: 3.0,
                color: colors.icon_light,
            });
            line(
                out,
                Vec2::new(-2.0, -3.0),
                Vec2::new(-7.0, 7.0),
                colors.icon_light,
            );
            line(out, Vec2::new(2.0, -3.0), Vec2::new(7.0, 7.0), colors.icon_light);
            line(out, Vec2::new(-7.0, 7.0), Vec2::new(7.0, 7.0), colors.icon_light);
        }
        IconKind::Camera => {
            for (a, b) in [
                ((-8.0, -5.0), (4.0, -5.0)),
                ((4.0, -5.0), (4.0, 5.0)),
                ((4.0, 5.0), (-8.0, 5.0)),
                ((-8.0, 5.0), (-8.0, -5.0)),
                ((4.0, 0.0), (9.0, -4.0)),
                ((9.0, -4.0), (9.0, 4.0)),
                ((9.0, 4.0), (4.0, 0.0)),
            ] {
                line(out, Vec2::new(a.0, a.1), Vec2::new(b.0, b.1), colors.icon_camera);
            }
        }
        IconKind::Decal => {
            for (a, b) in [
                ((-6.0, -6.0), (6.0, -6.0)),
                ((6.0, -6.0), (6.0, 6.0)),
                ((6.0, 6.0), (-6.0, 6.0)),
                ((-6.0, 6.0), (-6.0, -6.0)),
                ((-6.0, 6.0), (6.0, -6.0)),
            ] {
                line(out, Vec2::new(a.0, a.1), Vec2::new(b.0, b.1), colors.icon_decal);
            }
        }
        IconKind::Audio => {
            for (a, b) in [
                ((-7.0, -3.0), (-3.0, -3.0)),
                ((-3.0, -3.0), (2.0, -7.0)),
                ((2.0, -7.0), (2.0, 7.0)),
                ((2.0, 7.0), (-3.0, 3.0)),
                ((-3.0, 3.0), (-7.0, 3.0)),
                ((-7.0, 3.0), (-7.0, -3.0)),
                ((5.0, -4.0), (7.0, 0.0)),
                ((7.0, 0.0), (5.0, 4.0)),
            ] {
                line(out, Vec2::new(a.0, a.1), Vec2::new(b.0, b.1), colors.icon_audio);
            }
        }
    }
}

/// A player start's flag icon at pixel `p` (a pole with a pennant).
fn player_start_icon(out: &mut Vec<OverlayShape>, p: Vec2) {
    for (a, b) in [
        ((-4.0, 9.0), (-4.0, -9.0)),
        ((-4.0, -9.0), (7.0, -5.0)),
        ((7.0, -5.0), (-4.0, -1.0)),
        ((-8.0, 9.0), (0.0, 9.0)),
    ] {
        out.push(OverlayShape::Line {
            a: p + Vec2::new(a.0, a.1),
            b: p + Vec2::new(b.0, b.1),
            color: ICON_PLAYER_START,
            thickness: 0,
        });
    }
}

/// Length of a player start's facing arrow, meters.
pub const PLAYER_START_ARROW: f32 = 1.2;

/// A player start's floor helper: a 0.4 m circle at its feet and an arrow along its facing
/// (local −Z flattened onto the floor), in world space.
fn player_start_helper(
    out: &mut Vec<OverlayShape>,
    world: &World,
    view: &ViewCamera,
    size: Vec2,
    entity: Entity,
) {
    let Some(position) = crate::viewport::world_position(world, entity) else {
        return;
    };
    let rotation = world_rotation(world, entity);
    let mut forward = rotation * Vec3::NEG_Z;
    forward.y = 0.0;
    let forward = forward.try_normalize().unwrap_or(Vec3::NEG_Z);
    let side = forward.cross(Vec3::Y);
    push_world_circle(
        out,
        view,
        size,
        position,
        Vec3::X * 0.4,
        Vec3::Z * 0.4,
        ICON_PLAYER_START,
    );
    let tip = position + forward * PLAYER_START_ARROW;
    push_line(out, view, size, position, tip, ICON_PLAYER_START);
    for wing in [side, -side] {
        push_line(
            out,
            view,
            size,
            tip,
            tip - forward * 0.35 + wing * 0.25,
            ICON_PLAYER_START,
        );
    }
}

/// The baked navigation mesh (the world's `regia_nav::NavMesh` resource) as world segments: every
/// polygon's boundary loop lifted 5 cm off its surface (so it sits on, not in, the floor, as the
/// runtime's nav gizmo draws it) in [`NAVIGATION`], and every off-mesh link as a line with an arrow
/// head at its end (both ends when bidirectional) in [`NAVIGATION_LINK`]. At most
/// [`MAX_NAVIGATION_SEGMENTS`]. The editor's preview of another level than the open one
/// ([`crate::nav_preview::is_stale`]) draws nothing.
#[must_use]
pub fn navigation_segments(world: &World) -> Vec<(Vec3, Vec3, egui::Color32)> {
    let _span = tracing::debug_span!(target: "regia_editor", "navigation_overlay").entered();
    let Some(mesh) = world.get_resource::<regia_nav::NavMesh>() else {
        return Vec::new();
    };
    // The editor's preview of the level that was open before (rev-claude-64 (2) finding 2).
    if crate::nav_preview::is_stale(world) {
        return Vec::new();
    }
    let nav_color = crate::debug_colors::override_color(world, "nav")
        .map(color32)
        .unwrap_or(NAVIGATION);
    let link_color = crate::debug_colors::override_color(world, "nav")
        .map(color32)
        .unwrap_or(NAVIGATION_LINK);
    let lift = Vec3::Y * 0.05;
    let mut out = Vec::new();
    'polys: for poly in &mesh.polys {
        let count = poly.vertices.len();
        for (i, vertex) in poly.vertices.iter().enumerate() {
            if out.len() >= MAX_NAVIGATION_SEGMENTS {
                break 'polys;
            }
            out.push((*vertex + lift, poly.vertices[(i + 1) % count] + lift, nav_color));
        }
    }
    for link in &mesh.off_mesh_links {
        if out.len() + 5 > MAX_NAVIGATION_SEGMENTS {
            break;
        }
        out.push((link.from + lift, link.to + lift, link_color));
        let mut heads = vec![(link.from, link.to)];
        if link.bidirectional {
            heads.push((link.to, link.from));
        }
        for (from, to) in heads {
            let along = (to - from).normalize_or_zero();
            let side = along.cross(Vec3::Y).normalize_or_zero() * 0.15;
            let back = to - along * 0.3 + lift;
            out.push((to + lift, back + side, link_color));
            out.push((to + lift, back - side, link_color));
        }
    }
    out
}

/// Every sensor collider's outline, dashed (each segment cut in six, every other piece drawn), in
/// [`TRIGGER_VOLUME`]: the trigger volumes' helper (ADR-7468). Hidden entities draw none, as their
/// icons; only sensor shapes are read (rev-claude-57 outliner finding 1, ADR-7495), so solid
/// trimeshes cost nothing and cannot use up the segment cap first.
#[must_use]
pub fn trigger_volume_segments(world: &World) -> Vec<(Vec3, Vec3)> {
    let _span = tracing::trace_span!("editor_trigger_volume_segments").entered();
    let mut out = Vec::new();
    let keep = |entity| !crate::visibility::is_hidden(world, entity);
    for segment in regia_agent::seeing_overlays::sensor_segments(world, keep) {
        let (a, b) = (Vec3::from_array(segment.a), Vec3::from_array(segment.b));
        for piece in (0..6).step_by(2) {
            let t0 = piece as f32 / 6.0;
            let t1 = (piece + 1) as f32 / 6.0;
            out.push((a.lerp(b, t0), a.lerp(b, t1)));
        }
    }
    out
}

/// The world rotation of an entity (its `GlobalTransform`, else its `Transform`).
fn world_rotation(world: &World, entity: Entity) -> Quat {
    crate::mesh_source::world_matrix(world, entity)
        .map_or(Quat::IDENTITY, |m| m.to_scale_rotation_translation().1)
}

/// Bounds, ranges, cones and frustums for one selected entity.
fn selection_helpers(
    out: &mut Vec<OverlayShape>,
    world: &World,
    view: &ViewCamera,
    size: Vec2,
    entity: Entity,
    colors: &OverlayTokens,
) {
    let Some(position) = crate::viewport::world_position(world, entity) else {
        return;
    };
    let rotation = world_rotation(world, entity);
    if let Some(light) = world.get::<regia_render::PointLight>(entity) {
        let r = light.radius.max(0.01);
        push_world_circle(out, view, size, position, Vec3::X * r, Vec3::Z * r, colors.bounds);
        push_world_circle(out, view, size, position, Vec3::X * r, Vec3::Y * r, colors.bounds);
        push_world_circle(out, view, size, position, Vec3::Z * r, Vec3::Y * r, colors.bounds);
        return;
    }
    if let Some(light) = world.get::<regia_render::SpotLight>(entity) {
        let forward = rotation * Vec3::NEG_Z;
        let (right, up) = (rotation * Vec3::X, rotation * Vec3::Y);
        let range = light.range.max(0.01);
        let radius = range * light.outer_angle.tan();
        let base = position + forward * range;
        push_world_circle(out, view, size, base, right * radius, up * radius, colors.bounds);
        for dir in [right, -right, up, -up] {
            push_line(out, view, size, position, base + dir * radius, colors.bounds);
        }
        return;
    }
    if let Some(light) = world.get::<regia_render::DirectionalLight>(entity) {
        let dir = Vec3::from_array(light.direction).normalize_or_zero();
        let tip = position + dir * 2.0;
        push_line(out, view, size, position, tip, colors.bounds);
        let side = dir.cross(Vec3::Y).normalize_or_zero() * 0.25;
        push_line(out, view, size, tip, tip - dir * 0.4 + side, colors.bounds);
        push_line(out, view, size, tip, tip - dir * 0.4 - side, colors.bounds);
        return;
    }
    if let Some(camera) = world.get::<regia_render::Camera>(entity) {
        let depth = camera.far.min(4.0);
        let half_h = depth * (camera.fov_y * 0.5).tan();
        let half_w = half_h * 16.0 / 9.0;
        let (right, up, forward) = (rotation * Vec3::X, rotation * Vec3::Y, rotation * Vec3::NEG_Z);
        let far_center = position + forward * depth;
        let corners = [
            far_center + right * half_w + up * half_h,
            far_center - right * half_w + up * half_h,
            far_center - right * half_w - up * half_h,
            far_center + right * half_w - up * half_h,
        ];
        for i in 0..4 {
            push_line(out, view, size, position, corners[i], colors.bounds);
            push_line(out, view, size, corners[i], corners[(i + 1) % 4], colors.bounds);
        }
        return;
    }
    if let Some(corners) = world_bounds_corners(world, entity) {
        for (a, b) in [
            (0, 1),
            (2, 3),
            (4, 5),
            (6, 7),
            (0, 2),
            (1, 3),
            (4, 6),
            (5, 7),
            (0, 4),
            (1, 5),
            (2, 6),
            (3, 7),
        ] {
            push_line(out, view, size, corners[a], corners[b], colors.bounds);
        }
    }
}

/// Every overlay shape of one pane (grid, icons, selection helpers, selection labels). Audio
/// gizmos also need the world's [`crate::viewport::ViewportSettings::audio`].
#[must_use]
pub fn build_overlays(
    world: &World,
    view: &ViewCamera,
    kind: ViewKind,
    size: Vec2,
    show: ShowFlags,
) -> Vec<OverlayShape> {
    let audio_on = world
        .get_resource::<crate::viewport::ViewportSettings>()
        .is_none_or(|settings| settings.audio);
    build_overlays_for(world, view, kind, size, show, audio_on)
}

/// [`build_overlays`] with the viewport's audio switch given (a
/// [`crate::viewport_instance::ViewportInstance`]'s own settings) instead of read from the world.
#[must_use]
pub fn build_overlays_for(
    world: &World,
    view: &ViewCamera,
    kind: ViewKind,
    size: Vec2,
    show: ShowFlags,
    audio_on: bool,
) -> Vec<OverlayShape> {
    let colors = &crate::theme::tokens(world).overlay;
    let mut out = Vec::new();
    out.extend(crate::debug_overlay_selected::overlays(world, view, size));
    out.extend(crate::visual_log_panel::overlays(world, view, size));
    if show.grid {
        grid(&mut out, view, kind, size, colors);
    }
    if show.icons {
        // Show ▸ Icons by kind, size and opacity (ADR-7770).
        let filter = show.icon_filter;
        // Flag bases, hill and zone boxes, capture progress and spawn heat (ADR-8738).
        if filter.match_rules {
            out.extend(crate::match_rules_play::overlays(world, view, size));
        }
        for entity_ref in world.iter_entities() {
            let entity = entity_ref.id();
            if crate::visibility::is_hidden(world, entity) || crate::view_state::is_view_hidden(world, entity)
            {
                continue;
            }
            // Live data of the DCC feed carries a text badge with its state (specs/60 § 6).
            if let Some(text) = crate::dcc_feed::outliner_badge(world, entity)
                && let Some(position) = crate::viewport::world_position(world, entity)
                && let Some(at) = view.project(position, size)
                && (view.eye - position).dot(view.forward()) < 0.0
            {
                out.push(OverlayShape::Text {
                    at,
                    text,
                    color: colors.icon_light,
                });
            }
            // A player start: its flag and floor arrow (ADR-7468).
            if filter.player_starts
                && world
                    .get::<regia_framework::player::PlayerStart>(entity)
                    .is_some()
            {
                let first = out.len();
                let mut label_at = None;
                if let Some(position) = crate::viewport::world_position(world, entity)
                    && let Some(p) = view.project(position, size)
                    && (view.eye - position).dot(view.forward()) < 0.0
                {
                    player_start_icon(&mut out, p);
                    label_at = Some(p);
                }
                player_start_helper(&mut out, world, view, size, entity);
                // Show ▸ Icons ▸ Match rules: the start in its team's color, with its label (ADR-8738).
                if filter.match_rules
                    && let Some((tint, label)) = crate::match_rules_play::player_start_tint(world, entity)
                {
                    for shape in &mut out[first..] {
                        if let OverlayShape::Line { color, .. } = shape {
                            *color = tint;
                        }
                    }
                    if let Some(p) = label_at {
                        out.push(OverlayShape::Text {
                            at: p + Vec2::new(9.0, -12.0),
                            text: label,
                            color: tint,
                        });
                    }
                }
            }
            let Some(kind) = icon_of(world, entity) else {
                continue;
            };
            let shown = match kind {
                IconKind::Sun | IconKind::Point | IconKind::Spot => filter.lights,
                IconKind::Camera => filter.cameras,
                IconKind::Decal => filter.decals,
                IconKind::Audio => filter.audio,
            };
            if shown
                && let Some(position) = crate::viewport::world_position(world, entity)
                && let Some(p) = view.project(position, size)
                && (view.eye - position).dot(view.forward()) < 0.0
            {
                let first = out.len();
                icon(&mut out, kind, p, colors);
                restyle_icon(&mut out[first..], p, filter.scale(), filter.opacity());
            }
        }
        // Trigger volumes, dashed (the Colliders flag draws them solid instead).
        if !show.colliders && filter.colliders {
            for (a, b) in trigger_volume_segments(world) {
                push_line(
                    &mut out,
                    view,
                    size,
                    a,
                    b,
                    crate::debug_colors::override_color(world, "trigger")
                        .map(color32)
                        .unwrap_or(TRIGGER_VOLUME),
                );
            }
        }
    }
    let selection = world.get_resource::<Selection>().cloned().unwrap_or_default();
    if show.bounds {
        for entity in selection.entities() {
            selection_helpers(&mut out, world, view, size, *entity, colors);
        }
    }
    if show.audio && audio_on {
        for (a, b, color) in crate::audio_gizmo::audio_gizmo_segments(world, selection.entities()) {
            push_line(&mut out, view, size, a, b, color);
        }
    }
    // Every collider and every light's reach (ADR-7217; the runtime screenshot's overlays).
    let mut shared = Vec::new();
    if show.colliders {
        shared.extend(project_collider_segments(world));
    }
    if show.light_radii {
        shared.extend(regia_agent::seeing_overlays::light_radius_segments(world));
    }
    for segment in shared {
        let [r, g, b, a] = segment.color;
        push_line(
            &mut out,
            view,
            size,
            Vec3::from_array(segment.a),
            Vec3::from_array(segment.b),
            egui::Color32::from_rgba_unmultiplied(r, g, b, a),
        );
    }
    if show.paths {
        let color = world
            .get_resource::<regia_framework::debug_gizmos::DebugColors>()
            .copied()
            .unwrap_or_default()
            .path;
        for (a, b) in regia_framework::path_debug::path_segments(world) {
            push_line(&mut out, view, size, a, b, color32(color));
        }
    }
    if show.navigation {
        for (a, b, color) in navigation_segments(world) {
            push_line(&mut out, view, size, a, b, color);
        }
    }
    if show.fog {
        for (a, b, color) in crate::fog_debug::fog_segments(world) {
            push_line(&mut out, view, size, a, b, color);
        }
    }
    // The Abilities inspector's `abilities` overlay group (state rings, live-effect ticks; off by default).
    for (a, b, color) in crate::abilities_panel::abilities_overlay_segments(world) {
        push_line(&mut out, view, size, a, b, color);
    }
    // The Ragdoll editor's bodies on its rig.
    for (a, b, color) in crate::ragdoll_panel::ragdoll_gizmo_segments(world) {
        push_line(&mut out, view, size, a, b, color);
    }
    // The Montage timeline's pose preview on its rig.
    for (a, b, color) in crate::montage_panel::montage_pose_segments(world) {
        push_line(&mut out, view, size, a, b, color);
    }
    // The cloth panel: the `cloth` gizmo group (off by default), the paint ramp, the preview.
    for (a, b, color) in crate::cloth_panel::cloth_gizmo_segments(world) {
        push_line(&mut out, view, size, a, b, color);
    }
    // The Bake panel's volume (drawn while the panel's state exists and its gizmo is on).
    if show.bounds {
        for (a, b, color) in crate::bake_panel::bake_gizmo_segments(world) {
            push_line(&mut out, view, size, a, b, color);
        }
        // The Lightmaps panel's worst chart seams (off until the panel or an agent turns them on).
        for (a, b, color) in crate::lightmap_panel::lightmap_gizmo_segments(world) {
            push_line(&mut out, view, size, a, b, color);
        }
        // The Fracture panel's preview.
        for (a, b, color) in crate::fracture_panel::fracture_gizmo_segments(world) {
            push_line(&mut out, view, size, a, b, color);
        }
        // The Smart Objects panel's slot markers.
        for (a, b, color) in crate::smart_object_panel::smart_object_gizmo_segments(world) {
            push_line(&mut out, view, size, a, b, color);
        }
        // Placed functional tests: a box with the name, a line to the viewpoint (spec 41 § 6).
        for gizmo in crate::tests_panel::test_gizmos(world) {
            for (a, b, color) in &gizmo.segments {
                push_line(&mut out, view, size, *a, *b, *color);
            }
            if let Some(at) = view.project(gizmo.anchor, size) {
                out.push(OverlayShape::Text {
                    at,
                    text: gizmo.name,
                    color: egui::Color32::from_rgb(120, 220, 120),
                });
            }
        }
    }
    // Selected reflection captures: influence box and dashed blend margin (ADR-7592).
    if show.bounds && show.icon_filter.captures {
        for (a, b, color) in crate::capture_helper::capture_helper_segments(world) {
            push_line(&mut out, view, size, a, b, color);
        }
    }
    out
}

/// Stamps the selection outline from the id buffer into the picture.
pub fn paint_outline(frame: &mut PaneFrame, world: &World) {
    crate::collab::paint(frame, world);
    let Some(ids) = frame.ids.as_ref() else {
        return;
    };
    let selection = world.get_resource::<Selection>().cloned().unwrap_or_default();
    if selection.entities().is_empty() {
        return;
    }
    let outline_color = crate::theme::tokens(world).overlay.selection_outline;
    let mask = ids.outline_mask(&|entity| selection.contains(entity), 1);
    for (pixel, outline) in frame.image.pixels.iter_mut().zip(mask) {
        if outline {
            *pixel = outline_color;
        }
    }
}

/// Paints every overlay into a captured pane: outline, shapes and the gizmo.
pub fn paint_capture_overlays(frame: &mut PaneFrame, world: &World, kind: ViewKind) {
    let layout = world
        .get_resource::<ViewportLayout>()
        .cloned()
        .unwrap_or_default();
    let audio_on = world
        .get_resource::<crate::viewport::ViewportSettings>()
        .is_none_or(|settings| settings.audio);
    paint_capture_overlays_for(frame, world, kind, &layout, audio_on);
}

/// [`paint_capture_overlays`] under a given layout and audio switch (a
/// [`crate::viewport_instance::ViewportInstance`]'s) instead of the world's singletons.
pub fn paint_capture_overlays_for(
    frame: &mut PaneFrame,
    world: &World,
    kind: ViewKind,
    layout: &ViewportLayout,
    audio_on: bool,
) {
    if frame.info.possessed {
        // Gameplay debug views are explicit opt-ins, also available in the possessed picture.
        let size = Vec2::new(frame.image.width() as f32, frame.image.height() as f32);
        let mut shapes = play_debug_shapes(world, &frame.view, size, layout);
        shapes.extend(crate::debug_overlay_selected::overlays(world, &frame.view, size));
        shapes.extend(crate::visual_log_panel::overlays(world, &frame.view, size));
        paint_shapes(&mut frame.image, &shapes);
        return;
    }
    let mut show = layout.effective_show();
    if show.grid {
        paint_grid_with(frame, kind, &crate::theme::tokens(world).overlay);
        show.grid = false;
    }
    if show.outline {
        paint_outline(frame, world);
    }
    let size = Vec2::new(frame.image.width() as f32, frame.image.height() as f32);
    let shapes = build_overlays_for(world, &frame.view, kind, size, show, audio_on);
    paint_shapes(&mut frame.image, &shapes);
    if !layout.game_view {
        paint_gizmo(&mut frame.image, world, &frame.view, size);
    }
}

/// Rasterizes shapes into an image (text is skipped).
pub fn paint_shapes(image: &mut egui::ColorImage, shapes: &[OverlayShape]) {
    for shape in shapes {
        match shape {
            OverlayShape::Line {
                a,
                b,
                color,
                thickness,
            } => {
                paint_line(image, a.x, a.y, b.x, b.y, *color, *thickness);
            }
            OverlayShape::Circle {
                center,
                radius,
                color,
            } => {
                paint_circle(image, center.x, center.y, *radius, *color, 0);
            }
            OverlayShape::Text { .. } => {}
        }
    }
}

/// Draws shapes with an egui painter over the pane `rect`.
pub fn paint_shapes_egui(painter: &egui::Painter, rect: egui::Rect, shapes: &[OverlayShape]) {
    let at = |p: Vec2| rect.left_top() + egui::vec2(p.x, p.y);
    for shape in shapes {
        match shape {
            OverlayShape::Line {
                a,
                b,
                color,
                thickness,
            } => {
                painter.line_segment(
                    [at(*a), at(*b)],
                    egui::Stroke::new(1.0 + *thickness as f32, *color),
                );
            }
            OverlayShape::Circle {
                center,
                radius,
                color,
            } => {
                painter.circle_stroke(at(*center), *radius, egui::Stroke::new(1.0, *color));
            }
            OverlayShape::Text { at: p, text, color } => {
                painter.text(
                    at(*p),
                    egui::Align2::LEFT_BOTTOM,
                    text,
                    egui::FontId::proportional(12.0),
                    *color,
                );
            }
        }
    }
}

/// The capture gizmo at the primary selection, in the active mode (the live panel uses the
/// interactive `transform_gizmo_egui` instead).
fn paint_gizmo(image: &mut egui::ColorImage, world: &World, view: &ViewCamera, size: Vec2) {
    let colors = &crate::theme::tokens(world).overlay;
    let Some(primary) = world.get_resource::<Selection>().and_then(Selection::primary) else {
        return;
    };
    let Some(transform) = world.get::<Transform>(primary).copied() else {
        return;
    };
    let origin = crate::viewport::world_position(world, primary).unwrap_or(transform.translation);
    let gizmo = world.get_resource::<GizmoState>().cloned().unwrap_or_default();
    // The select-only tool draws no gizmo (ADR-7756).
    if gizmo.mode == GizmoMode::Select {
        return;
    }
    // The axes of the reference space (ADR-7756), from world poses.
    let parent = crate::viewport_ops::parent_affine(world, primary);
    let (_, parent_rotation, _) = parent.to_scale_rotation_translation();
    let rotation = crate::viewport_ops::space_basis(
        gizmo.space,
        crate::viewport_ops::to_world(parent, &transform).rotation,
        parent_rotation,
        crate::viewport_ops::view_rotation(view),
    );
    let basis = [rotation * Vec3::X, rotation * Vec3::Y, rotation * Vec3::Z];
    // Constant on-screen length: 80 px.
    let length = view.units_per_pixel(size.y)
        * 80.0
        * ((origin - view.eye).length() / (view.target - view.eye).length().max(1e-3));
    let Some(center) = view.project(origin, size) else {
        return;
    };
    if gizmo.mode == GizmoMode::Rotate {
        paint_circle(image, center.x, center.y, 28.0, colors.gizmo_ring, 2);
        return;
    }
    for (axis, color) in [
        (basis[0], colors.gizmo_x),
        (basis[1], colors.gizmo_y),
        (basis[2], colors.gizmo_z),
    ] {
        if let Some(end) = view.project(origin + axis * length, size) {
            // An axis pointing at the camera collapses to a dot; keep a visible stub.
            let end = if (end - center).length() < 3.0 {
                center + Vec2::new(3.0, 0.0)
            } else {
                end
            };
            paint_line(image, center.x, center.y, end.x, end.y, color, 1);
        }
    }
}

/// Bresenham line, stamping a square of radius `thickness` so a 1 px line survives a scaled
/// screenshot. Pixels outside the image are skipped.
pub fn paint_line(
    image: &mut egui::ColorImage,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    color: egui::Color32,
    thickness: i32,
) {
    if !(x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite()) {
        return;
    }
    let mut x0 = x0.clamp(-1.0e6, 1.0e6).round() as i32;
    let mut y0 = y0.clamp(-1.0e6, 1.0e6).round() as i32;
    let x1 = x1.clamp(-1.0e6, 1.0e6).round() as i32;
    let y1 = y1.clamp(-1.0e6, 1.0e6).round() as i32;
    let dx = (x1 - x0).abs();
    let dy = (y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx - dy;
    // A segment that projects just in front of the camera can run for tens of thousands of
    // pixels. Cap the walk; the on-screen part is what matters.
    for _ in 0..(dx + dy + 1).min(8_000) {
        stamp(image, x0, y0, color, thickness);
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 > -dy {
            err -= dy;
            x0 += sx;
        }
        if e2 < dx {
            err += dx;
            y0 += sy;
        }
    }
}

/// A circle outline of `radius` pixels.
pub fn paint_circle(
    image: &mut egui::ColorImage,
    cx: f32,
    cy: f32,
    radius: f32,
    color: egui::Color32,
    thickness: i32,
) {
    let steps = 48;
    for i in 0..steps {
        let a0 = std::f32::consts::TAU * i as f32 / steps as f32;
        let a1 = std::f32::consts::TAU * (i + 1) as f32 / steps as f32;
        paint_line(
            image,
            cx + radius * a0.cos(),
            cy + radius * a0.sin(),
            cx + radius * a1.cos(),
            cy + radius * a1.sin(),
            color,
            thickness,
        );
    }
}

fn stamp(image: &mut egui::ColorImage, x: i32, y: i32, color: egui::Color32, thickness: i32) {
    let w = image.width() as i32;
    let h = image.height() as i32;
    let stride = image.width();
    for dy in -thickness..=thickness {
        for dx in -thickness..=thickness {
            let px = x + dx;
            let py = y + dy;
            if px >= 0 && py >= 0 && px < w && py < h {
                image.pixels[(py as usize) * stride + px as usize] = color;
            }
        }
    }
}

/// The world-space AABB enclosing `entities` (resolved mesh bounds, else proxy cubes, else
/// positions), for focus framing.
#[must_use]
pub fn selection_aabb(world: &World, entities: &[Entity]) -> Option<(Vec3, Vec3)> {
    let mut min = Vec3::splat(f32::MAX);
    let mut max = Vec3::splat(f32::MIN);
    let mut any = false;
    for entity in entities {
        let bounds = if icon_of(world, *entity).is_some() {
            crate::viewport::world_position(world, *entity)
                .map(|p| (p - Vec3::splat(0.5), p + Vec3::splat(0.5)))
        } else {
            world_aabb(world, *entity)
        };
        if let Some((lo, hi)) = bounds {
            min = min.min(lo);
            max = max.max(hi);
            any = true;
        }
    }
    any.then_some((min, max))
}

// ---------------------------------------------------------------------------
// Composition guides (spec addendum § 3.8, ADR-7768)
// ---------------------------------------------------------------------------

/// The composition guides over the main viewport's perspective pane: rule of thirds, a center
/// cross, the safe-action and safe-title boxes and a letterbox at a chosen aspect. View state:
/// never saved with the level, never on the undo stack. The thirds, cross and safe boxes sit inside
/// the letterboxed picture when a letterbox is on.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Default)]
pub struct ViewportGuides {
    /// Lines at a third and two thirds across and down.
    pub thirds: bool,
    /// A small cross at the picture's center.
    pub center: bool,
    /// The safe-action ([`SAFE_ACTION`]) and safe-title ([`SAFE_TITLE`]) boxes.
    pub safe_areas: bool,
    /// Letterbox bars down to this aspect (width / height); `None` is off.
    pub letterbox: Option<f32>,
}

impl ViewportGuides {
    /// Whether any guide is on.
    #[must_use]
    pub fn any(&self) -> bool {
        self.thirds || self.center || self.safe_areas || self.letterbox.is_some()
    }
}

/// The letterbox aspects the menu offers: (label, width / height).
pub const LETTERBOX_ASPECTS: &[(&str, f32)] = &[
    ("2.39:1", 2.39),
    ("1.85:1", 1.85),
    ("16:9", 16.0 / 9.0),
    ("4:3", 4.0 / 3.0),
    ("1:1", 1.0),
    ("9:16", 9.0 / 16.0),
];

/// The safe-action box: this fraction of the picture's width and height, centered.
pub const SAFE_ACTION: f32 = 0.93;
/// The safe-title box: this fraction of the picture's width and height, centered.
pub const SAFE_TITLE: f32 = 0.90;

/// The picture area left between letterbox bars at `aspect` inside `rect` (bars top and bottom
/// for a wider aspect, left and right for a narrower one); `rect` itself without a letterbox.
#[must_use]
pub fn letterbox_rect(rect: egui::Rect, aspect: Option<f32>) -> egui::Rect {
    let Some(aspect) = aspect.filter(|a| a.is_finite() && *a > 0.0) else {
        return rect;
    };
    let pane_aspect = rect.width() / rect.height().max(1.0);
    let size = if aspect >= pane_aspect {
        egui::vec2(rect.width(), rect.width() / aspect)
    } else {
        egui::vec2(rect.height() * aspect, rect.height())
    };
    egui::Rect::from_center_size(rect.center(), size)
}

/// The guide line segments over the picture area `active`: the thirds and the center cross.
#[must_use]
pub fn guide_lines(active: egui::Rect, guides: &ViewportGuides) -> Vec<[egui::Pos2; 2]> {
    let mut lines = Vec::new();
    if guides.thirds {
        for t in [1.0 / 3.0, 2.0 / 3.0] {
            let x = active.left() + active.width() * t;
            let y = active.top() + active.height() * t;
            lines.push([egui::pos2(x, active.top()), egui::pos2(x, active.bottom())]);
            lines.push([egui::pos2(active.left(), y), egui::pos2(active.right(), y)]);
        }
    }
    if guides.center {
        let c = active.center();
        let arm = active.width().min(active.height()) * 0.03;
        lines.push([egui::pos2(c.x - arm, c.y), egui::pos2(c.x + arm, c.y)]);
        lines.push([egui::pos2(c.x, c.y - arm), egui::pos2(c.x, c.y + arm)]);
    }
    lines
}

/// The safe-action and safe-title boxes inside `active` (empty when off).
#[must_use]
pub fn safe_rects(active: egui::Rect, guides: &ViewportGuides) -> Vec<egui::Rect> {
    if !guides.safe_areas {
        return Vec::new();
    }
    [SAFE_ACTION, SAFE_TITLE]
        .into_iter()
        .map(|f| egui::Rect::from_center_size(active.center(), active.size() * f))
        .collect()
}

/// Paints the world's [`ViewportGuides`] over the pane `rect` (theme colors: the bounds token for
/// lines, the extreme background for the bars). Nothing when every guide is off.
pub fn paint_guides(ui: &egui::Ui, rect: egui::Rect, world: &World) {
    let Some(guides) = world
        .get_resource::<ViewportGuides>()
        .copied()
        .filter(ViewportGuides::any)
    else {
        return;
    };
    let painter = ui.painter_at(rect);
    let line = egui::Stroke::new(1.0, crate::theme::tokens(world).overlay.bounds);
    let active = letterbox_rect(rect, guides.letterbox);
    if active != rect {
        let bar = ui.visuals().extreme_bg_color;
        for band in [
            egui::Rect::from_min_max(rect.left_top(), egui::pos2(rect.right(), active.top())),
            egui::Rect::from_min_max(egui::pos2(rect.left(), active.bottom()), rect.right_bottom()),
            egui::Rect::from_min_max(
                egui::pos2(rect.left(), active.top()),
                egui::pos2(active.left(), active.bottom()),
            ),
            egui::Rect::from_min_max(
                egui::pos2(active.right(), active.top()),
                egui::pos2(rect.right(), active.bottom()),
            ),
        ] {
            if band.width() > 0.0 && band.height() > 0.0 {
                painter.rect_filled(band, 0.0, bar);
            }
        }
    }
    for [a, b] in guide_lines(active, &guides) {
        painter.line_segment([a, b], line);
    }
    for safe in safe_rects(active, &guides) {
        painter.rect_stroke(safe, 0.0, line, egui::StrokeKind::Inside);
    }
}

/// The **Camera ▸ Guides** toggles and letterbox aspects (focusable buttons).
pub fn guides_menu(ui: &mut egui::Ui, world: &mut World) {
    use crate::agent_parity as parity;
    let mut guides = world
        .get_resource::<ViewportGuides>()
        .copied()
        .unwrap_or_default();
    ui.label("Composition guides");
    parity::action_toggle(ui, &parity::VIEW_GUIDE_THIRDS, &mut guides.thirds);
    parity::action_toggle(ui, &parity::VIEW_GUIDE_CENTER, &mut guides.center);
    parity::action_toggle(ui, &parity::VIEW_GUIDE_SAFE, &mut guides.safe_areas);
    ui.horizontal_wrapped(|ui| {
        let off = if guides.letterbox.is_none() {
            "No letterbox (current)"
        } else {
            "No letterbox"
        };
        if parity::action_button_text(ui, &parity::VIEW_GUIDE_LETTERBOX, off).clicked() {
            guides.letterbox = None;
        }
        for (label, aspect) in LETTERBOX_ASPECTS {
            let current = guides.letterbox.is_some_and(|a| (a - aspect).abs() < 1e-4);
            let text = if current {
                format!("{label} (current)")
            } else {
                (*label).to_owned()
            };
            if parity::action_button_text(ui, &parity::VIEW_GUIDE_LETTERBOX, text).clicked() {
                guides.letterbox = Some(*aspect);
            }
        }
    });
    if world.get_resource::<ViewportGuides>() != Some(&guides) {
        world.insert_resource(guides);
    }
}

/// Applies `editor.set_viewport`'s `guides` object (`thirds`, `center`, `safe_areas` booleans;
/// `letterbox`: an aspect label from [`LETTERBOX_ASPECTS`], a number, or null for off).
///
/// # Errors
///
/// `invalid_params` for a value of the wrong kind or an aspect that is not positive.
pub fn apply_guides_param(
    world: &mut World,
    value: &serde_json::Value,
) -> Result<(), regia_agent::protocol::RpcError> {
    use regia_agent::protocol::RpcError;
    let object = value.as_object().ok_or_else(|| {
        RpcError::invalid_params("`guides` is an object { thirds, center, safe_areas, letterbox }")
    })?;
    let mut guides = world
        .get_resource::<ViewportGuides>()
        .copied()
        .unwrap_or_default();
    for (name, slot) in [
        ("thirds", &mut guides.thirds),
        ("center", &mut guides.center),
        ("safe_areas", &mut guides.safe_areas),
    ] {
        if let Some(flag) = object.get(name) {
            *slot = flag
                .as_bool()
                .ok_or_else(|| RpcError::invalid_params(format!("`guides.{name}` is true or false")))?;
        }
    }
    if let Some(letterbox) = object.get("letterbox") {
        guides.letterbox = match letterbox {
            serde_json::Value::Null => None,
            serde_json::Value::String(label) => Some(
                LETTERBOX_ASPECTS
                    .iter()
                    .find(|(l, _)| *l == label.as_str())
                    .map(|(_, a)| *a)
                    .ok_or_else(|| {
                        RpcError::invalid_params(format!(
                            "unknown letterbox `{label}` (2.39:1, 1.85:1, 16:9, 4:3, 1:1, 9:16, a number, or null)"
                        ))
                    })?,
            ),
            other => Some(
                other
                    .as_f64()
                    .map(|a| a as f32)
                    .filter(|a| a.is_finite() && (0.1..=10.0).contains(a))
                    .ok_or_else(|| RpcError::invalid_params("`guides.letterbox` aspect is 0.1 to 10"))?,
            ),
        };
    }
    world.insert_resource(guides);
    Ok(())
}

/// The world's guides as `editor.set_viewport` answers them.
#[must_use]
pub fn guides_json(world: &World) -> serde_json::Value {
    let guides = world
        .get_resource::<ViewportGuides>()
        .copied()
        .unwrap_or_default();
    serde_json::json!({
        "thirds": guides.thirds,
        "center": guides.center,
        "safe_areas": guides.safe_areas,
        "letterbox": guides.letterbox,
    })
}

fn color32(color: regia_core::math::Color) -> egui::Color32 {
    let [r, g, b, a] = color.to_srgb_u8();
    egui::Color32::from_rgba_unmultiplied(r, g, b, a)
}

fn project_collider_segments(world: &World) -> Vec<regia_agent::seeing_overlays::OverlaySegment> {
    let _span = tracing::trace_span!("project_collider_segments").entered();
    let sleeping = if crate::debug_colors::override_color(world, "sleeping_body").is_some()
        || crate::debug_colors::override_color(world, "collider").is_some()
    {
        regia_framework::debug_gizmos::sleeping_entities(world.get_resource::<regia_physics::PhysicsWorld>())
    } else {
        std::collections::HashSet::new()
    };
    let mut segments = regia_agent::seeing_overlays::collider_segments_colored(world, |entity, sensor| {
        let name = if sensor {
            "trigger"
        } else if sleeping.contains(&entity) {
            "sleeping_body"
        } else {
            "collider"
        };
        crate::debug_colors::override_color(world, name).map_or(
            if sensor {
                regia_agent::seeing_overlays::SENSOR_COLOR
            } else {
                regia_agent::seeing_overlays::COLLIDER_COLOR
            },
            |c| c.to_srgb_u8(),
        )
    });
    if let Some(draw) = world.get_resource::<regia_physics::debug_draw::PhysicsDebugDraw>() {
        for line in draw.lines() {
            let c = if line.color == regia_physics::debug_draw::CONTACT_COLOR {
                crate::debug_colors::override_color(world, "contact").unwrap_or_else(|| {
                    regia_core::math::Color::rgb(line.color[0], line.color[1], line.color[2])
                })
            } else {
                regia_core::math::Color::rgb(line.color[0], line.color[1], line.color[2])
            };
            segments.push(regia_agent::seeing_overlays::OverlaySegment {
                a: line.from.to_array(),
                b: line.to.to_array(),
                color: c.to_srgb_u8(),
            });
        }
    }
    segments
}

fn selected_debug_segments(
    world: &World,
    show: ShowFlags,
) -> Vec<regia_agent::seeing_overlays::OverlaySegment> {
    let _span = tracing::trace_span!("play_debug_segments").entered();
    let mut out = Vec::new();
    if show.colliders {
        out.extend(project_collider_segments(world));
    }
    if show.navigation {
        out.extend(navigation_segments(world).into_iter().map(|(a, b, c)| {
            regia_agent::seeing_overlays::OverlaySegment {
                a: a.to_array(),
                b: b.to_array(),
                color: c.to_array(),
            }
        }));
    }
    if show.fog {
        out.extend(
            crate::fog_debug::fog_segments(world)
                .into_iter()
                .map(|(a, b, c)| regia_agent::seeing_overlays::OverlaySegment {
                    a: a.to_array(),
                    b: b.to_array(),
                    color: c.to_array(),
                }),
        );
    }
    if show.paths {
        let color = world
            .get_resource::<regia_framework::debug_gizmos::DebugColors>()
            .copied()
            .unwrap_or_default()
            .path
            .to_srgb_u8();
        out.extend(
            regia_framework::path_debug::path_segments(world)
                .into_iter()
                .map(|(a, b)| regia_agent::seeing_overlays::OverlaySegment {
                    a: a.to_array(),
                    b: b.to_array(),
                    color,
                }),
        );
    }
    if show.lockstep {
        out.extend(det_debug_segments(world));
    }
    out
}

/// Most segments the Lockstep overlay draws per pane (a huge capture draws its first ones).
pub const MAX_DET_DEBUG_SEGMENTS: usize = 50_000;

/// Segments of a circle on XZ in the Lockstep overlay.
const DET_DEBUG_CIRCLE_SEGMENTS: usize = 24;

/// Collects a det debug frame as world-space segments (circles as polygons, boxes as edges, text as
/// stroke segments).
struct SegmentSink<'a> {
    out: &'a mut Vec<regia_agent::seeing_overlays::OverlaySegment>,
    colors: regia_framework::det_debug::DetDebugColors,
}

impl SegmentSink<'_> {
    fn push(&mut self, kind: regia_framework::det_debug::DebugKind, a: [f32; 3], b: [f32; 3]) {
        if self.out.len() < MAX_DET_DEBUG_SEGMENTS {
            self.out.push(regia_agent::seeing_overlays::OverlaySegment {
                a,
                b,
                color: self.colors.of(kind).to_srgb_u8(),
            });
        }
    }
}

impl regia_framework::det_debug::DebugSink for SegmentSink<'_> {
    fn line(&mut self, kind: regia_framework::det_debug::DebugKind, a: [f32; 3], b: [f32; 3]) {
        self.push(kind, a, b);
    }
    fn circle(&mut self, kind: regia_framework::det_debug::DebugKind, center: [f32; 3], radius: f32) {
        let point = |i: usize| {
            let t = std::f32::consts::TAU * i as f32 / DET_DEBUG_CIRCLE_SEGMENTS as f32;
            [
                center[0] + radius * t.cos(),
                center[1],
                center[2] + radius * t.sin(),
            ]
        };
        for i in 0..DET_DEBUG_CIRCLE_SEGMENTS {
            self.push(kind, point(i), point(i + 1));
        }
    }
    fn aabb(&mut self, kind: regia_framework::det_debug::DebugKind, min: [f32; 3], max: [f32; 3]) {
        let y = min[1];
        let c = [
            [min[0], y, min[2]],
            [max[0], y, min[2]],
            [max[0], y, max[2]],
            [min[0], y, max[2]],
        ];
        for (i, &corner) in c.iter().enumerate() {
            self.push(kind, corner, c[(i + 1) % 4]);
        }
    }
    /// Text as stroke segments, in the plane facing +Z at `at` (centered), the way the running
    /// game's `lockstep` gizmo group draws it (KI-ED-019, ADR-10070).
    fn text(&mut self, kind: regia_framework::det_debug::DebugKind, at: [f32; 3], text: &str) {
        let segments =
            regia_render::gizmos::stroke_text_segments(text, DET_DEBUG_TEXT_M, regia_core::math::Vec2::ZERO);
        for [a, b] in segments {
            self.push(
                kind,
                [at[0] + a.x, at[1] + a.y, at[2]],
                [at[0] + b.x, at[1] + b.y, at[2]],
            );
        }
    }
}

/// Cap height of a Lockstep text label in the editor overlay, world units (the running game's
/// `lockstep` gizmo group uses the same 0.4).
pub const DET_DEBUG_TEXT_M: f32 = 0.4;

/// The play world's det debug frame (`regia_framework::det_debug::DetDebugDraw`, ADR-8562) as
/// world-space segments in the Lockstep colors. Empty when the world has no frame.
#[must_use]
pub fn det_debug_segments(world: &World) -> Vec<regia_agent::seeing_overlays::OverlaySegment> {
    let _span = tracing::trace_span!("det_debug_segments").entered();
    let mut out = Vec::new();
    let Some(draw) = world.get_resource::<regia_framework::det_debug::DetDebugDraw>() else {
        return out;
    };
    if draw.what.is_empty() {
        return out;
    }
    let colors = world
        .get_resource::<regia_framework::det_debug::DetDebugColors>()
        .copied()
        .unwrap_or_default();
    draw.frame.replay(&mut SegmentSink {
        out: &mut out,
        colors,
    });
    out
}

/// Debug flags explicitly selected by the user remain visible while possessed.
/// Authoring helpers (grid, selection, icons and gizmos) remain governed by the play view.
#[must_use]
pub fn play_debug_shapes(
    world: &World,
    view: &ViewCamera,
    size: Vec2,
    layout: &ViewportLayout,
) -> Vec<OverlayShape> {
    let _span = tracing::trace_span!("play_debug_shapes").entered();
    if layout.game_view {
        return Vec::new();
    }
    let mut shapes = Vec::new();
    for line in selected_debug_segments(world, layout.show) {
        let [r, g, b, a] = line.color;
        push_line(
            &mut shapes,
            view,
            size,
            Vec3::from_array(line.a),
            Vec3::from_array(line.b),
            egui::Color32::from_rgba_unmultiplied(r, g, b, a),
        );
    }
    shapes
}

/// The separate play window has no egui layer: its selected debug flags use a gizmo batch.
pub(crate) fn play_window_debug_frame(world: &World) -> regia_render::gizmos::GizmoFrame {
    use regia_core::math::{Color, Mat4};
    use regia_render::gizmos::{DefaultGizmoConfigGroup, GizmoBatch, GizmoBuffer, GizmoConfig, GizmoFrame};
    let _span = tracing::trace_span!("play_window_debug_frame").entered();
    let Some(layout) = world.get_resource::<ViewportLayout>().filter(|l| !l.game_view) else {
        return GizmoFrame::default();
    };
    let mut buffer = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
    for line in selected_debug_segments(world, layout.show) {
        let [r, g, b, a] = line.color;
        buffer.line(
            Vec3::from_array(line.a),
            Vec3::from_array(line.b),
            Color::from_srgb_u8(r, g, b, a),
        );
    }
    let mut out = GizmoFrame::default();
    if !buffer.is_empty() {
        out.batches.push(GizmoBatch::from_view(
            buffer.buffer(),
            Mat4::IDENTITY,
            &GizmoConfig {
                on_top: true,
                ..Default::default()
            },
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::viewport::EditorCamera;

    #[test]
    fn grid_spans_the_pane_in_every_view_kind() {
        let camera = EditorCamera::default();
        let size = Vec2::new(320.0, 240.0);
        for kind in ViewKind::ALL {
            let view = ViewCamera::of(kind, &camera);
            let mut shapes = Vec::new();
            grid(&mut shapes, &view, kind, size, &OverlayTokens::regia_default());
            let on_screen = shapes
                .iter()
                .filter(|s| match s {
                    OverlayShape::Line { a, .. } => a.x.is_finite(),
                    _ => false,
                })
                .count();
            assert!(on_screen > 10, "{kind:?}: {on_screen}");
        }
    }

    #[test]
    fn geometry_in_front_hides_the_grid() {
        let camera = EditorCamera::default();
        let (w, h) = (160_usize, 120_usize);
        let frame = |near_block: bool| {
            let mut distance = vec![f32::INFINITY; w * h];
            if near_block {
                // An occluder 0.5 m from the eye over the left half of the pane.
                for y in 0..h {
                    for x in 0..w / 2 {
                        distance[y * w + x] = 0.5;
                    }
                }
            }
            let mut frame = PaneFrame {
                image: egui::ColorImage::new([w, h], vec![egui::Color32::BLACK; w * h]),
                view: ViewCamera::of(ViewKind::Perspective, &camera),
                ids: Some(crate::picking::IdBuffer {
                    width: w as u32,
                    height: h as u32,
                    ids: vec![0; w * h],
                    distance,
                    entities: Vec::new(),
                }),
                info: crate::render::PaneInfo::default(),
            };
            paint_grid(&mut frame, ViewKind::Perspective);
            frame.image
        };
        let count = |image: &egui::ColorImage, x0: usize, x1: usize| {
            (0..h)
                .flat_map(|y| (x0..x1).map(move |x| (x, y)))
                .filter(|(x, y)| {
                    let p = image.pixels[y * w + x];
                    p == GRID_MAJOR || p == GRID_MINOR
                })
                .count()
        };
        let open = frame(false);
        let blocked = frame(true);
        assert!(count(&open, 0, w / 2) > 100, "grid on the open left half");
        assert_eq!(count(&blocked, 0, w / 2), 0, "occluded left half");
        assert_eq!(
            count(&blocked, w / 2, w),
            count(&open, w / 2, w),
            "the unoccluded half is unchanged"
        );
    }

    #[test]
    fn icons_and_bounds_appear_for_lights_and_selection() {
        let mut world = World::new();
        let light = world
            .spawn((
                Transform::from_translation(Vec3::new(0.0, 1.0, 0.0)),
                regia_render::PointLight::default(),
            ))
            .id();
        let mut selection = Selection::default();
        selection.select(light, crate::selection::SelectMode::Replace);
        world.insert_resource(selection);
        let view = ViewCamera::of(ViewKind::Perspective, &EditorCamera::default());
        let shapes = build_overlays(
            &world,
            &view,
            ViewKind::Perspective,
            Vec2::new(320.0, 240.0),
            ShowFlags {
                grid: false,
                ..ShowFlags::default()
            },
        );
        assert!(
            shapes
                .iter()
                .any(|s| matches!(s, OverlayShape::Circle { color, .. } if *color == ICON_LIGHT))
        );
        assert!(
            shapes
                .iter()
                .any(|s| matches!(s, OverlayShape::Line { color, .. } if *color == BOUNDS))
        );
        assert_eq!(icon_of(&world, light), Some(IconKind::Point));
    }

    /// A trigger volume's helper is the sensor's outline dashed: a box's 12 edges in 3 pieces each,
    /// every piece a sixth of its edge; a solid collider draws no helper (ADR-7468).
    #[test]
    fn trigger_volume_helper_is_the_sensor_outline_dashed() {
        let mut world = World::new();
        let at = Transform::default();
        let collider = |sensor| regia_physics::Collider {
            shape: regia_physics::ColliderShape::Box {
                half_extents: Vec3::splat(1.0),
            },
            sensor,
            ..regia_physics::Collider::default()
        };
        world.spawn((
            at,
            regia_scene::GlobalTransform(at.compute_affine()),
            collider(false),
        ));
        assert!(
            trigger_volume_segments(&world).is_empty(),
            "solid colliders have no helper"
        );
        world.spawn((
            at,
            regia_scene::GlobalTransform(at.compute_affine()),
            collider(true),
        ));
        let dashes = trigger_volume_segments(&world);
        assert_eq!(dashes.len(), 12 * 3);
        for (a, b) in &dashes {
            assert!(((*b - *a).length() - 2.0 / 6.0).abs() < 1e-5, "{a} {b}");
        }
        // rev-claude-57 outliner finding 1 (ADR-7495): an editor-hidden sensor draws no helper, as
        // the icon loop skips it; a solid trimesh spawned first with more edges than the segment cap does not
        // starve a sensor spawned after it.
        let hidden = world
            .spawn((
                at,
                regia_scene::GlobalTransform(at.compute_affine()),
                collider(true),
            ))
            .id();
        world.entity_mut(hidden).insert(crate::visibility::EditorHidden);
        assert_eq!(
            trigger_volume_segments(&world).len(),
            12 * 3,
            "the hidden one draws none"
        );
        let mut world = World::new();
        let n = 200u32;
        let mut vertices = Vec::new();
        let mut indices = Vec::new();
        for z in 0..=n {
            for x in 0..=n {
                vertices.push(Vec3::new(x as f32, 0.0, z as f32));
            }
        }
        for z in 0..n {
            for x in 0..n {
                let i = z * (n + 1) + x;
                indices.push([i, i + 1, i + n + 1]);
                indices.push([i + 1, i + n + 2, i + n + 1]);
            }
        }
        world.spawn((
            at,
            regia_scene::GlobalTransform(at.compute_affine()),
            regia_physics::Collider {
                shape: regia_physics::ColliderShape::TriMesh { vertices, indices },
                ..regia_physics::Collider::default()
            },
        ));
        assert!(
            regia_agent::seeing_overlays::collider_segments(&world).len()
                >= regia_agent::seeing_overlays::MAX_SEGMENTS,
            "the control: the trimesh alone fills the collider overlay's cap"
        );
        world.spawn((
            at,
            regia_scene::GlobalTransform(at.compute_affine()),
            collider(true),
        ));
        assert_eq!(
            trigger_volume_segments(&world).len(),
            12 * 3,
            "the sensor after the big trimesh still draws"
        );
        let mut world = World::new();
        let start = world.spawn(regia_framework::player::PlayerStart).id();
        assert_eq!(
            icon_of(&world, start),
            None,
            "a player start keeps its pickable proxy"
        );
    }

    /// Show ▸ Navigation (ADR-7468): a baked 8 m floor's polygons draw over the floor with the flag
    /// on, nothing with it off (the control) or in game view.
    #[test]
    fn navigation_flag_draws_the_baked_floor() {
        let mut world = World::new();
        let (a, b, c, d) = (
            Vec3::new(-4.0, 0.0, -4.0),
            Vec3::new(4.0, 0.0, -4.0),
            Vec3::new(4.0, 0.0, 4.0),
            Vec3::new(-4.0, 0.0, 4.0),
        );
        let mesh = regia_nav::NavMeshBuilder::new(regia_nav::NavMeshBuildParams::default())
            .build(&[[a, c, b], [a, d, c]])
            .expect("bake");
        assert!(!mesh.polys.is_empty(), "the floor bakes");
        world.insert_resource(mesh);
        let segments = navigation_segments(&world);
        assert!(!segments.is_empty());
        for (p, q, color) in &segments {
            assert_eq!(*color, NAVIGATION);
            for v in [p, q] {
                assert!(
                    v.x.abs() <= 4.01 && v.z.abs() <= 4.01 && (v.y - 0.05).abs() < 0.2,
                    "{v}"
                );
            }
        }
        let view = ViewCamera::of(ViewKind::Perspective, &EditorCamera::default());
        let size = Vec2::new(160.0, 120.0);
        let painted = |show: ShowFlags| {
            let shapes = build_overlays(&world, &view, ViewKind::Perspective, size, show);
            let mut image = egui::ColorImage::new([160, 120], vec![egui::Color32::BLACK; 160 * 120]);
            paint_shapes(&mut image, &shapes);
            image.pixels.iter().filter(|p| **p == NAVIGATION).count()
        };
        let off = ShowFlags {
            grid: false,
            ..ShowFlags::default()
        };
        assert!(!off.navigation, "off by default");
        assert_eq!(painted(off), 0, "the control");
        let on = ShowFlags {
            navigation: true,
            ..off
        };
        assert!(painted(on) > 40, "the floor's polygon outlines");
        let layout = ViewportLayout {
            show: on,
            game_view: true,
            ..ViewportLayout::default()
        };
        assert!(!layout.effective_show().navigation, "game view hides it");
    }

    /// `editor.set_viewport { colliders, light_radii }` (ADR-7217): the capture draws every
    /// collider's outline and every point light's reach, diffed against the flags off; game view
    /// hides them like the other helpers.
    #[test]
    fn collider_and_light_radius_overlays_draw_only_when_switched_on() {
        use regia_agent::seeing_overlays::{COLLIDER_COLOR, LIGHT_COLOR};
        let mut world = World::new();
        let at = Transform::default();
        world.spawn((
            at,
            regia_scene::GlobalTransform(at.compute_affine()),
            regia_physics::Collider {
                shape: regia_physics::ColliderShape::Box {
                    half_extents: Vec3::splat(0.5),
                },
                ..regia_physics::Collider::default()
            },
        ));
        let lamp = Transform::from_translation(Vec3::new(1.0, 0.5, 0.0));
        world.spawn((
            lamp,
            regia_scene::GlobalTransform(lamp.compute_affine()),
            regia_render::PointLight {
                radius: 0.6,
                ..regia_render::PointLight::default()
            },
        ));
        let view = ViewCamera::of(ViewKind::Perspective, &EditorCamera::default());
        let size = Vec2::new(160.0, 120.0);
        let painted = |show: ShowFlags, color: [u8; 4]| {
            let shapes = build_overlays(&world, &view, ViewKind::Perspective, size, show);
            let mut image = egui::ColorImage::new([160, 120], vec![egui::Color32::BLACK; 160 * 120]);
            paint_shapes(&mut image, &shapes);
            let [r, g, b, a] = color;
            let want = egui::Color32::from_rgba_unmultiplied(r, g, b, a);
            image.pixels.iter().filter(|p| **p == want).count()
        };
        let off = ShowFlags {
            grid: false,
            icons: false,
            ..ShowFlags::default()
        };
        assert!(!off.colliders && !off.light_radii, "off by default");
        assert_eq!(painted(off, COLLIDER_COLOR), 0);
        assert_eq!(painted(off, LIGHT_COLOR), 0);
        let on = ShowFlags {
            colliders: true,
            light_radii: true,
            ..off
        };
        assert!(painted(on, COLLIDER_COLOR) > 40, "the box outline");
        assert!(painted(on, LIGHT_COLOR) > 40, "the light's reach");
        let layout = ViewportLayout {
            show: on,
            game_view: true,
            ..ViewportLayout::default()
        };
        let hidden = layout.effective_show();
        assert!(!hidden.colliders && !hidden.light_radii, "game view hides them");
    }
}

// ---------------------------------------------------------------------------------------------
// Hover pre-highlight and the drop ghost (P-071, P-067; ADR-7769)

/// The hover pre-highlight's color: the theme's selection outline at half strength.
#[must_use]
pub fn hover_outline_color(world: &World) -> egui::Color32 {
    crate::theme::tokens(world)
        .overlay
        .selection_outline
        .gamma_multiply(0.5)
}

/// The id-buffer table entries that draw for `target`: itself and every entity whose nearest
/// selection root ([`crate::selection::selection_root_of`]) or ancestor is `target`.
fn drawn_for(world: &World, ids: &crate::picking::IdBuffer, target: Entity) -> Vec<bool> {
    ids.entities
        .iter()
        .map(|e| *e == target || crate::hierarchy_ops::ancestors(world, *e).contains(&target))
        .collect()
}

/// Stamps a thin (1 px) pre-highlight ring around `target`'s visible pixels in the frame's id
/// buffer (the hovered entity, or the selection root a click on it would select), blended at
/// half strength over the picture (or over a transparent overlay). Reads the id buffer the pane
/// already has; nothing when it has none. Works inside the target's pixel bounds only.
pub fn paint_hover_outline(frame: &mut PaneFrame, world: &World, target: Entity) {
    let _span = tracing::info_span!(target: "regia_editor", "hover_outline").entered();
    let Some(ids) = frame.ids.as_ref() else {
        return;
    };
    let table = drawn_for(world, ids, target);
    let inside = |id: u32| {
        (id as usize)
            .checked_sub(1)
            .and_then(|i| table.get(i))
            .copied()
            .unwrap_or(false)
    };
    let (w, h) = (ids.width as usize, ids.height as usize);
    if frame.image.pixels.len() != w * h {
        return;
    }
    // The target's pixel bounds, then a ring one pixel outside them.
    let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
    for (index, id) in ids.ids.iter().enumerate() {
        if inside(*id) {
            let (x, y) = (index % w, index / w);
            x0 = x0.min(x);
            x1 = x1.max(x);
            y0 = y0.min(y);
            y1 = y1.max(y);
        }
    }
    if x0 == usize::MAX {
        return;
    }
    let color = hover_outline_color(world);
    let blend = |under: egui::Color32| {
        let mix = |a: u8, b: u8| ((u16::from(a) + u16::from(b)) / 2) as u8;
        egui::Color32::from_rgba_premultiplied(
            mix(under.r(), color.r()),
            mix(under.g(), color.g()),
            mix(under.b(), color.b()),
            mix(under.a(), color.a()).max(color.a()),
        )
    };
    let (xa, ya) = (x0.saturating_sub(1), y0.saturating_sub(1));
    let (xb, yb) = ((x1 + 1).min(w - 1), (y1 + 1).min(h - 1));
    let mut ring = Vec::new();
    for y in ya..=yb {
        for x in xa..=xb {
            if inside(ids.ids[y * w + x]) {
                continue;
            }
            let near = (y.saturating_sub(1)..=(y + 1).min(h - 1))
                .any(|ny| (x.saturating_sub(1)..=(x + 1).min(w - 1)).any(|nx| inside(ids.ids[ny * w + nx])));
            if near {
                ring.push(y * w + x);
            }
        }
    }
    for index in ring {
        frame.image.pixels[index] = blend(frame.image.pixels[index]);
    }
}

/// The hover target under pane-buffer pixel `pixel`: the id buffer's entity there, mapped to
/// what a click selects ([`crate::selection::selection_root_of`]; `child_pick` is Ctrl), or
/// `None` on background, on a locked entity or when it is already selected (it has the
/// selection outline).
#[must_use]
pub fn hover_target(
    world: &World,
    ids: &crate::picking::IdBuffer,
    pixel: Vec2,
    child_pick: bool,
) -> Option<Entity> {
    let (entity, _) = ids.at(pixel.x.max(0.0) as u32, pixel.y.max(0.0) as u32)?;
    if crate::viewport_ops::is_locked(world, entity) {
        return None;
    }
    let target = crate::selection::selection_root_of(world, entity, child_pick);
    let selected = world
        .get_resource::<Selection>()
        .is_some_and(|s| s.contains(target));
    (!selected).then_some(target)
}

/// The drop ghost ([`crate::content::DropPreview`]): the dragged asset's bounds as a wire box
/// at the placement transform, in pane points (`size`), in the theme's bounds color. Empty
/// without a current preview.
#[must_use]
pub fn drop_preview_shapes(world: &World, view: &ViewCamera, size: Vec2) -> Vec<OverlayShape> {
    let Some(preview) = world.get_resource::<crate::content::DropPreview>() else {
        return Vec::new();
    };
    let (min, max) = preview.bounds;
    let affine = preview.transform.compute_affine();
    let corner = |i: usize| {
        let p = Vec3::new(
            if i & 1 == 0 { min.x } else { max.x },
            if i & 2 == 0 { min.y } else { max.y },
            if i & 4 == 0 { min.z } else { max.z },
        );
        affine.transform_point3(p)
    };
    let color = crate::theme::tokens(world).overlay.bounds;
    let mut shapes = Vec::new();
    for (a, b) in [
        (0, 1),
        (2, 3),
        (4, 5),
        (6, 7),
        (0, 2),
        (1, 3),
        (4, 6),
        (5, 7),
        (0, 4),
        (1, 5),
        (2, 6),
        (3, 7),
    ] {
        if let (Some(a), Some(b)) = (view.project(corner(a), size), view.project(corner(b), size)) {
            shapes.push(OverlayShape::Line {
                a,
                b,
                color,
                thickness: 0,
            });
        }
    }
    if let Some(at) = view.project(preview.transform.translation, size) {
        shapes.push(OverlayShape::Circle {
            center: at,
            radius: 4.0,
            color,
        });
    }
    shapes
}

#[cfg(test)]
mod hover_tests {
    use super::*;

    /// P-071: the pre-highlight is a ring around the hovered entity's pixels only (not the
    /// other entity's, not its own interior), half strength of the selection token; a hover on
    /// a child of a selection root rings the root's whole subtree; a selected or locked entity
    /// has no hover target.
    #[test]
    fn the_hover_ring_surrounds_only_the_hovered_entity_and_follows_the_selection_root() {
        let mut world = World::new();
        let a = world.spawn_empty().id();
        let b = world.spawn_empty().id();
        let (w, h) = (10u32, 6u32);
        let mut ids = vec![0u32; (w * h) as usize];
        // a: 2x2 at (2, 2); b: 2x2 at (6, 2).
        for (n, x0) in [(1u32, 2u32), (2, 6)] {
            for y in 2..4 {
                for x in x0..x0 + 2 {
                    ids[(y * w + x) as usize] = n;
                }
            }
        }
        let buffer = crate::picking::IdBuffer {
            width: w,
            height: h,
            distance: vec![1.0; ids.len()],
            ids,
            entities: vec![a, b],
        };
        assert_eq!(hover_target(&world, &buffer, Vec2::new(2.5, 2.5), false), Some(a));
        assert_eq!(
            hover_target(&world, &buffer, Vec2::new(0.5, 0.5), false),
            None,
            "background"
        );
        let base = egui::Color32::from_rgb(10, 20, 30);
        let mut frame = PaneFrame {
            image: egui::ColorImage::filled([w as usize, h as usize], base),
            view: ViewCamera::of(ViewKind::Perspective, &crate::viewport::EditorCamera::default()),
            ids: Some(buffer.clone()),
            info: crate::render::PaneInfo::default(),
        };
        paint_hover_outline(&mut frame, &world, a);
        let changed: Vec<usize> = frame
            .image
            .pixels
            .iter()
            .enumerate()
            .filter(|(_, p)| **p != base)
            .map(|(i, _)| i)
            .collect();
        // The ring around a 2x2 block is the 4x4 square minus the block: 12 pixels, all at x <= 4.
        assert_eq!(changed.len(), 12, "{changed:?}");
        assert!(changed.iter().all(|i| i % w as usize <= 4), "b is not ringed");
        // A selection root: hovering b (a child of root a) rings both a and b.
        world.entity_mut(b).insert(ChildOf(a));
        world.entity_mut(a).insert(crate::selection::EditorSelectionRoot);
        assert_eq!(hover_target(&world, &buffer, Vec2::new(6.5, 2.5), false), Some(a));
        assert_eq!(
            hover_target(&world, &buffer, Vec2::new(6.5, 2.5), true),
            Some(b),
            "Ctrl: the child"
        );
        let mut frame2 = PaneFrame {
            image: egui::ColorImage::filled([w as usize, h as usize], base),
            ..frame
        };
        paint_hover_outline(&mut frame2, &world, a);
        assert!(
            frame2.image.pixels.iter().filter(|p| **p != base).count() > 12,
            "the subtree is ringed"
        );
        // Selected: no hover target.
        let mut selection = Selection::default();
        selection.select(a, crate::selection::SelectMode::Replace);
        world.insert_resource(selection);
        assert_eq!(hover_target(&world, &buffer, Vec2::new(2.5, 2.5), false), None);
    }
}

#[cfg(test)]
mod w12_play_window_tests {
    use super::*;
    #[test]
    fn w12_separate_play_window_uses_the_same_debug_flags() {
        let mut world = World::new();
        world.insert_resource(ViewportLayout::default());
        world.spawn((
            regia_scene::GlobalTransform::IDENTITY,
            regia_nav::NavAgentState {
                waypoints: vec![Vec3::X, Vec3::Z],
                next_waypoint: 0,
                ..Default::default()
            },
        ));
        assert!(play_window_debug_frame(&world).is_empty());
        world.resource_mut::<ViewportLayout>().show.paths = true;
        assert_eq!(play_window_debug_frame(&world).segment_count(), 2);
        world.resource_mut::<ViewportLayout>().game_view = true;
        assert!(play_window_debug_frame(&world).is_empty());
    }
}

#[cfg(test)]
mod lockstep_overlay_tests {
    use regia_framework::det_debug::{DebugKind, DebugSink, DebugWhat, DetDebugDraw};

    use super::*;

    /// Show ▸ Lockstep (ADR-8562): the play world's det debug frame becomes world segments, and
    /// nothing is drawn with the flag off or the switch empty.
    #[test]
    fn lockstep_overlay_draws_the_det_debug_frame() {
        let mut world = World::new();
        let mut draw = DetDebugDraw::new(DebugWhat::ALL);
        draw.frame.line(DebugKind::Contact, [0.0; 3], [1.0, 0.0, 0.0]);
        draw.frame.circle(DebugKind::Query, [0.0; 3], 2.0);
        draw.frame.aabb(DebugKind::Grid, [0.0; 3], [4.0, 0.0, 4.0]);
        draw.frame.text(DebugKind::Occupancy, [2.0, 0.0, 2.0], "3");
        draw.tick = Some(7);
        world.insert_resource(draw);
        let segments = det_debug_segments(&world);
        // KI-ED-019 (ADR-10070): the occupancy label "3" is drawn too, as stroke segments.
        let label =
            regia_render::gizmos::stroke_text_segments("3", DET_DEBUG_TEXT_M, regia_core::math::Vec2::ZERO);
        assert!(!label.is_empty(), "the stroke font has a glyph for 3");
        assert_eq!(segments.len(), 1 + DET_DEBUG_CIRCLE_SEGMENTS + 4 + label.len());
        let occupancy = regia_framework::det_debug::DetDebugColors::default()
            .occupancy
            .to_srgb_u8();
        let text_part = &segments[1 + DET_DEBUG_CIRCLE_SEGMENTS + 4..];
        assert!(text_part.iter().all(|s| s.color == occupancy));
        assert!(
            text_part
                .iter()
                .all(|s| (s.a[0] - 2.0).abs() < 1.0 && s.a[1].abs() < 1.0 && s.a[2] == 2.0),
            "the label sits at its point, facing +Z"
        );
        let contact = regia_framework::det_debug::DetDebugColors::default()
            .contact
            .to_srgb_u8();
        assert_eq!(segments[0].color, contact);

        let off = ShowFlags::default();
        assert!(!off.lockstep, "off by default");
        assert!(selected_debug_segments(&world, off).is_empty());
        let on = ShowFlags {
            lockstep: true,
            ..off
        };
        assert_eq!(selected_debug_segments(&world, on).len(), segments.len());

        world.resource_mut::<DetDebugDraw>().what = DebugWhat::NONE;
        assert!(
            det_debug_segments(&world).is_empty(),
            "an empty switch draws nothing"
        );
    }
}
