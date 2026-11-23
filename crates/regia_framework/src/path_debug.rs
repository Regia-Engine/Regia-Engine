// SPDX-License-Identifier: MPL-2.0
//! Remaining navigation paths, shared by the runtime gizmo system and editor overlays.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_nav::NavAgentState;
use regia_render::gizmos::{GizmoConfig, GizmoConfigGroup, Gizmos};
use regia_scene::GlobalTransform;

/// Maximum path segments drawn per frame, across all agents.
pub const MAX_PATH_SEGMENTS: usize = 20_000;

/// Independently switchable navigation path overlay (`debug.set_gizmos`, group `paths`).
#[derive(Default, Debug, Clone, Copy)]
pub struct PathGizmos;

impl GizmoConfigGroup for PathGizmos {
    const NAME: &'static str = "paths";
    fn default_config() -> GizmoConfig {
        GizmoConfig {
            enabled: false,
            ..Default::default()
        }
    }
}

fn append_path(out: &mut Vec<(Vec3, Vec3)>, origin: Vec3, state: &NavAgentState, work_left: &mut usize) {
    let Some(remaining) = state.waypoints.get(state.next_waypoint..) else {
        return;
    };
    let mut from = origin;
    for &to in remaining {
        if *work_left == 0 {
            break;
        }
        *work_left -= 1;
        if from.is_finite() && to.is_finite() {
            out.push((from, to));
        }
        // Do not bridge over invalid input: the next valid pair may draw independently.
        from = to;
    }
}

/// World-space segments from each agent's current position through its unvisited waypoints.
/// Completed or invalid paths draw nothing; invalid edges are omitted without bridging them.
#[must_use]
pub fn path_segments(world: &World) -> Vec<(Vec3, Vec3)> {
    let _span = tracing::trace_span!("path_debug_segments").entered();
    let mut out = Vec::new();
    let mut work_left = MAX_PATH_SEGMENTS;
    if let Some(mut agents) = world.try_query::<(&GlobalTransform, &NavAgentState)>() {
        for (pose, state) in agents.iter(world) {
            append_path(&mut out, pose.0.translation.into(), state, &mut work_left);
            work_left = work_left.saturating_sub(1);
            if work_left == 0 {
                break;
            }
        }
    }
    out
}

/// Runtime path drawing; scheduled only while the `paths` gizmo group is enabled.
pub(crate) fn draw_paths(
    agents: Query<(&GlobalTransform, &NavAgentState)>,
    colors: Res<crate::debug_gizmos::DebugColors>,
    mut gizmos: Gizmos<PathGizmos>,
) {
    let _span = tracing::trace_span!("path_debug_draw").entered();
    let mut segments = Vec::new();
    let mut work_left = MAX_PATH_SEGMENTS;
    for (pose, state) in &agents {
        append_path(&mut segments, pose.0.translation.into(), state, &mut work_left);
        work_left = work_left.saturating_sub(1);
        if work_left == 0 {
            break;
        }
    }
    for (from, to) in segments {
        gizmos.line(from, to, colors.path);
    }
}
