// SPDX-License-Identifier: MPL-2.0
//! The `camera_rig` gizmo overlay (`docs/specs/35-camera-framework.md` § 9): each active rig's
//! pose frustum tinted by weight, the boom line from the pivot to the camera, the collision probe
//! (red when blocked), the aim point, and one marker per layer stacked above the camera.
//!
//! Off by default; `debug.set_gizmos { "group": "camera_rig", "enabled": true }` turns it on. The
//! group's system does not run while it is disabled.

use regia_app::prelude::*;
use regia_core::math::{Color, Isometry, Quat, Vec3};
use regia_render::gizmos::{CameraRigGizmos, GizmoBuffer, GizmoConfigGroup, Gizmos, gizmo_group_enabled};

use super::driver::CameraDriver;
use super::runtime::{LayerInfo, RigInfo};

/// A rig at full weight.
pub const RIG_COLOR: Color = Color::rgb(0.2, 0.85, 1.0);
/// A rig at zero weight.
pub const RIG_FADED_COLOR: Color = Color::rgb(0.25, 0.28, 0.3);
/// The boom line.
pub const BOOM_COLOR: Color = Color::rgb(1.0, 0.85, 0.2);
/// A collision probe that reached the full distance.
pub const PROBE_FREE_COLOR: Color = Color::rgb(0.3, 1.0, 0.4);
/// A collision probe that was blocked.
pub const PROBE_BLOCKED_COLOR: Color = Color::rgb(1.0, 0.2, 0.15);
/// The aim point.
pub const AIM_COLOR: Color = Color::rgb(1.0, 0.4, 0.9);
/// Layer markers.
pub const LAYER_COLOR: Color = Color::rgb(0.9, 0.9, 0.3);

/// Width over height assumed for the drawn frustum.
const ASPECT: f32 = 16.0 / 9.0;
/// Depth of the drawn frustum (metres).
const FRUSTUM_DEPTH: f32 = 1.0;

/// Draws the overlay for every driver.
#[derive(Default)]
pub struct CameraDebugPlugin;

impl Plugin for CameraDebugPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            PostUpdate,
            draw_camera_rigs
                .run_if(gizmo_group_enabled::<CameraRigGizmos>)
                .after(super::CameraSet::Director),
        );
    }
}

fn tint(weight: f32) -> Color {
    let t = weight.clamp(0.0, 1.0);
    let mix = |a: f32, b: f32| a + (b - a) * t;
    Color::rgb(
        mix(RIG_FADED_COLOR.r, RIG_COLOR.r),
        mix(RIG_FADED_COLOR.g, RIG_COLOR.g),
        mix(RIG_FADED_COLOR.b, RIG_COLOR.b),
    )
}

#[tracing::instrument(skip_all, target = "regia_framework", name = "camera_rig_gizmos")]
fn draw_camera_rigs(drivers: Query<&CameraDriver>, mut gizmos: Gizmos<CameraRigGizmos>) {
    for driver in &drivers {
        let Some(core) = driver.core() else {
            continue;
        };
        let rigs = core.rig_infos();
        for rig in &rigs {
            draw_rig(&mut gizmos, rig);
        }
        if let Some(base) = core.pose() {
            draw_layers(&mut gizmos, base.position, &core.layer_infos());
        }
    }
}

/// Draws one rig: frustum, boom line, probe, aim point.
pub fn draw_rig<G: GizmoConfigGroup>(gizmos: &mut GizmoBuffer<G>, rig: &RigInfo) {
    let pose = &rig.pose;
    let color = tint(rig.weight);
    let half_h = (pose.fov_deg.to_radians() * 0.5).tan() * FRUSTUM_DEPTH;
    let half_w = half_h * ASPECT;
    let rotation = pose.rotation();
    let corner =
        |x: f32, y: f32| pose.position + rotation * Vec3::new(x * half_w, y * half_h, -FRUSTUM_DEPTH);
    let corners = [
        corner(-1.0, -1.0),
        corner(1.0, -1.0),
        corner(1.0, 1.0),
        corner(-1.0, 1.0),
    ];
    for (i, c) in corners.iter().enumerate() {
        gizmos.line(pose.position, *c, color);
        gizmos.line(*c, corners[(i + 1) % 4], color);
    }
    if let Some(pivot) = rig.pivot {
        gizmos.line(pivot, pose.position, BOOM_COLOR);
    }
    if let Some(probe) = rig.probe {
        let color = if probe.blocked {
            PROBE_BLOCKED_COLOR
        } else {
            PROBE_FREE_COLOR
        };
        gizmos.sphere(
            Isometry::new(Quat::IDENTITY, probe.placed),
            probe.radius.max(0.02),
            color,
        );
        if probe.blocked {
            gizmos.line(probe.placed, probe.wanted, PROBE_BLOCKED_COLOR);
        }
    }
    if let Some(aim) = pose.aim {
        gizmos.sphere(Isometry::new(Quat::IDENTITY, aim), 0.1, AIM_COLOR);
    }
}

/// Draws one small marker per layer, stacked above `anchor` (the gizmo buffer has no text
/// primitive, so the layer list is a column of markers, lowest layer at the bottom).
pub fn draw_layers<G: GizmoConfigGroup>(gizmos: &mut GizmoBuffer<G>, anchor: Vec3, layers: &[LayerInfo]) {
    for (i, layer) in layers.iter().enumerate() {
        let size = 0.04 + 0.06 * layer.weight.clamp(0.0, 1.0);
        let center = anchor + Vec3::Y * (0.3 + 0.15 * i as f32);
        gizmos.cuboid(
            Isometry::new(Quat::IDENTITY, center),
            Vec3::splat(size),
            LAYER_COLOR,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::nodes::{CameraPose, Probe};
    use regia_render::gizmos::DefaultGizmoConfigGroup;

    fn rig(weight: f32, probe: Option<Probe>) -> RigInfo {
        RigInfo {
            name: "r".into(),
            weight,
            elapsed: 0.0,
            skipped: 0,
            trace: vec![],
            pose: CameraPose {
                aim: Some(Vec3::new(0.0, 0.0, -5.0)),
                ..CameraPose::default()
            },
            pivot: Some(Vec3::new(0.0, 0.0, -4.0)),
            probe,
            rails: vec![],
            cranes: vec![],
        }
    }

    fn has_color<G: GizmoConfigGroup>(b: &GizmoBuffer<G>, color: Color) -> bool {
        b.list_colors
            .iter()
            .chain(b.strip_colors.iter())
            .any(|c| *c == color)
    }

    fn segments<G: GizmoConfigGroup>(b: &GizmoBuffer<G>) -> usize {
        let strips: usize = b
            .strip_positions
            .split(|p| p.is_nan())
            .map(|r| r.len().saturating_sub(1))
            .sum();
        b.list_positions.len() / 2 + strips
    }

    #[test]
    fn a_rig_draws_a_frustum_a_boom_line_and_an_aim_point() {
        let mut b = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        draw_rig(&mut b, &rig(1.0, None));
        // Eight frustum lines, one boom line, an aim sphere (three circles).
        assert!(segments(&b) >= 9 + 3, "drew {} segments", segments(&b));
    }

    #[test]
    fn a_blocked_probe_is_drawn_red_and_a_free_one_green() {
        let probe = |blocked| Probe {
            from: Vec3::ZERO,
            wanted: Vec3::new(0.0, 0.0, 4.0),
            placed: Vec3::new(0.0, 0.0, 2.0),
            radius: 0.25,
            blocked,
        };
        let mut blocked = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        draw_rig(&mut blocked, &rig(1.0, Some(probe(true))));
        let mut free = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        draw_rig(&mut free, &rig(1.0, Some(probe(false))));
        assert!(
            segments(&blocked) > segments(&free),
            "the blocked probe adds its wanted line"
        );
        assert!(has_color(&blocked, PROBE_BLOCKED_COLOR));
        assert!(has_color(&free, PROBE_FREE_COLOR));
        assert!(!has_color(&free, PROBE_BLOCKED_COLOR));
    }

    #[test]
    fn weight_tints_the_frustum() {
        let mut full = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        draw_rig(&mut full, &rig(1.0, None));
        let mut faded = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        draw_rig(&mut faded, &rig(0.0, None));
        assert!(has_color(&full, RIG_COLOR));
        assert!(has_color(&faded, RIG_FADED_COLOR));
    }

    #[test]
    fn layers_draw_one_marker_each() {
        let layer = |id| LayerInfo {
            id: crate::camera::runtime::LayerId(id),
            rig: "x".into(),
            kind: "overlay",
            order: 0,
            phase: "settled",
            weight: 1.0,
            held: false,
        };
        let mut one = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        draw_layers(&mut one, Vec3::ZERO, &[layer(1)]);
        let mut two = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        draw_layers(&mut two, Vec3::ZERO, &[layer(1), layer(2)]);
        assert_eq!(segments(&two), segments(&one) * 2);
    }
}
