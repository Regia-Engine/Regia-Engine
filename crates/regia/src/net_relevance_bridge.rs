// SPDX-License-Identifier: MPL-2.0
//! The `net_relevance` gizmo group (spec 72 § 10, ADR-2665 (5)).
//!
//! `regia_net` has no edge to the renderer, so the drawing lives here, next to the lag
//! compensation bridge. For one connection (the agent's `net.relevance_gizmos {peer}`, else the
//! connected peer with the lowest id) the group draws:
//!
//! - each replicated entity as a small box colored by its state: relevant (green), lingering
//!   (amber) or filtered out (red); an asleep entity gets a second, larger white outline;
//! - each of the connection's viewpoints as a cross with a circle of the project default
//!   profile's radius on the ground plane;
//! - the interest grid's cells around each viewpoint (cell size from the pipeline).
//!
//! Off by default; `net.relevance_gizmos {visible: true}` (or `debug.set_gizmos`) turns it on.
//! It draws on a server or a listen host with a renderer; a dedicated server has nothing to draw
//! on, and the agent method still answers the same rows there.

use regia_app::prelude::*;
use regia_net::replication::relevance_view::NetRelevanceView;
#[cfg(feature = "client")]
use regia_net::replication::relevance_view::{RelevanceMarkState, RelevanceSnapshot};

#[cfg(feature = "client")]
use regia_core::math::{Color, Isometry, Quat, UVec2, Vec2, Vec3};
#[cfg(feature = "client")]
use regia_render::gizmos::{
    AppGizmoBuilder, GizmoBuffer, GizmoConfig, GizmoConfigGroup, GizmoConfigStore, Gizmos,
    gizmo_group_enabled,
};

/// Relevant to the connection.
#[cfg(feature = "client")]
pub const RELEVANT: Color = Color::rgb(0.25, 0.85, 0.3);
/// Still held for its linger time.
#[cfg(feature = "client")]
pub const LINGERING: Color = Color::rgb(0.95, 0.7, 0.15);
/// Not relevant to the connection.
#[cfg(feature = "client")]
pub const FILTERED_OUT: Color = Color::rgb(0.9, 0.2, 0.2);
/// The outline of an asleep entity.
#[cfg(feature = "client")]
pub const ASLEEP: Color = Color::rgb(0.95, 0.95, 0.95);
/// Viewpoints and their radius.
#[cfg(feature = "client")]
pub const VIEWPOINT: Color = Color::rgb(0.3, 0.6, 0.95);
/// The interest grid.
#[cfg(feature = "client")]
pub const GRID: Color = Color::rgb(0.35, 0.35, 0.45);

/// Half size of an entity's box, metres.
#[cfg(feature = "client")]
const MARK_HALF: f32 = 0.25;
/// Grid cells drawn on each side of a viewpoint (at most), so a huge radius stays cheap.
#[cfg(feature = "client")]
const MAX_GRID_CELLS: u32 = 32;

/// Gizmo group `net_relevance` (spec 72 § 10). Off by default.
#[cfg(feature = "client")]
#[derive(Default, Debug, Clone, Copy)]
pub struct NetRelevanceGizmos;

#[cfg(feature = "client")]
impl GizmoConfigGroup for NetRelevanceGizmos {
    const NAME: &'static str = "net_relevance";

    fn default_config() -> GizmoConfig {
        GizmoConfig {
            enabled: false,
            ..GizmoConfig::default()
        }
    }
}

/// See the module docs. `DefaultPlugins` adds it when the `net` feature is on.
#[derive(Default, Debug, Clone, Copy)]
pub struct NetRelevanceBridgePlugin;

impl Plugin for NetRelevanceBridgePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<NetRelevanceView>();
    }

    #[cfg(feature = "client")]
    fn finish(&self, app: &mut App) {
        // A dedicated server has no renderer and nothing to draw on.
        if !app.world().contains_resource::<GizmoConfigStore>() {
            return;
        }
        app.init_gizmo_group::<NetRelevanceGizmos>();
        app.add_systems(
            PostUpdate,
            draw_relevance_gizmos
                .after(PostUpdateSet::TransformPropagate)
                .run_if(gizmo_group_enabled::<NetRelevanceGizmos>),
        );
    }
}

/// The color of a state.
#[cfg(feature = "client")]
pub fn state_color(state: RelevanceMarkState) -> Color {
    match state {
        RelevanceMarkState::Relevant => RELEVANT,
        RelevanceMarkState::Lingering => LINGERING,
        RelevanceMarkState::FilteredOut => FILTERED_OUT,
    }
}

/// Draws one connection's view into `buffer`; `position` gives an entity's world position
/// (entities without one are skipped).
#[cfg(feature = "client")]
pub fn draw_relevance(
    buffer: &mut GizmoBuffer<NetRelevanceGizmos>,
    snapshot: &RelevanceSnapshot,
    position: impl Fn(Entity) -> Option<Vec3>,
) {
    let ground = Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2);
    for (at, _forward) in &snapshot.viewpoints {
        buffer.cross(Isometry::from_translation(*at), 0.5, VIEWPOINT);
        if let Some(radius) = snapshot.radius_m {
            buffer.circle(Isometry::new(ground, *at), radius, VIEWPOINT);
        }
        if snapshot.cell_m > 0.0 {
            // The cells the radius covers, snapped to the grid, at most `MAX_GRID_CELLS` a side.
            let reach = snapshot.radius_m.unwrap_or(snapshot.cell_m * 4.0);
            let cells = ((2.0 * reach / snapshot.cell_m).ceil() as u32).clamp(1, MAX_GRID_CELLS);
            let c = snapshot.cell_m;
            let half = cells as f32 * c * 0.5;
            // The grid's lines fall on multiples of the cell size, as the pipeline's cells do.
            let min_x = ((at.x - half) / c).floor() * c;
            let min_z = ((at.z - half) / c).floor() * c;
            let center = Vec3::new(min_x + half, at.y, min_z + half);
            buffer
                .grid(
                    Isometry::new(ground, center),
                    UVec2::splat(cells),
                    Vec2::splat(c),
                    GRID,
                )
                .outer_edges();
        }
    }
    for mark in &snapshot.marks {
        let Some(at) = position(mark.entity) else {
            continue;
        };
        let iso = Isometry::from_translation(at);
        buffer.cuboid(iso, Vec3::splat(MARK_HALF), state_color(mark.state));
        if mark.asleep {
            buffer.cuboid(iso, Vec3::splat(MARK_HALF * 1.4), ASLEEP);
        }
    }
}

/// `PostUpdate`: the `net_relevance` group on a server or listen host. Runs only while the group
/// is on.
#[cfg(feature = "client")]
fn draw_relevance_gizmos(
    rep: Option<Res<regia_net::replication::ServerReplication>>,
    (interest, profiles): (
        Option<Res<regia_net::relevance::NetInterest>>,
        Option<Res<regia_net::profile::NetProfiles>>,
    ),
    (node, mut view): (
        Option<Res<regia_net::runtime::NetNode>>,
        Option<ResMut<NetRelevanceView>>,
    ),
    asleep: Query<(), With<regia_net::replication::NetAsleep>>,
    transforms: Query<&regia_scene::transform::GlobalTransform>,
    mut gizmos: Gizmos<NetRelevanceGizmos>,
) {
    let _span = tracing::trace_span!("net_relevance_gizmos").entered();
    let Some(rep) = rep.as_deref() else {
        return;
    };
    let Some(peer) = regia_net::replication::relevance_view::view_peer_of(node.as_deref(), view.as_deref())
    else {
        return;
    };
    let Some(view) = view.as_deref_mut() else {
        return;
    };
    regia_net::replication::relevance_view::with_cached_relevance_snapshot(
        view,
        rep,
        interest.as_deref(),
        profiles.as_deref(),
        peer,
        |entity| asleep.contains(entity),
        |snapshot| {
            draw_relevance(&mut gizmos, snapshot, |entity| {
                transforms.get(entity).ok().map(|t| t.translation())
            });
        },
    );
}
