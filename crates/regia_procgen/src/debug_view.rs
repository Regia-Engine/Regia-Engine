// SPDX-License-Identifier: MPL-2.0
//! The runtime debug overlays (`procgen.set_debug`, spec 66 § 7): each tier's cell grid colored by
//! state, build and release circles around every anchor, priority numbers on queued cells, a cell
//! with seam-risk points outlined in magenta instead of its state color, and generator bounds.
//!
//! [`tile_overlay`] is the pure half (what to draw, tested without a renderer); [`draw_overlay`]
//! draws it into the `"procgen"` gizmo group, which `procgen.set_debug` turns on with the overlay.
//! Seam-risk is per cell: the report counts the points, and the cell's outline shows them.

use regia_app::prelude::*;
use regia_core::math::{Color, Quat, Vec2, Vec3};
use regia_render::gizmos::{GizmoConfig, GizmoConfigGroup, Gizmos};

use crate::tiles::{ProcGenTileHost, ProcGenTiles, TileKey, TileState, gather_anchors, is_scheduled};
use crate::world::{ProcGenerator, generator_bounds};

/// The `"procgen"` gizmo group: off until `procgen.set_debug` (or `debug.set_gizmos`) turns it on.
#[derive(Default, Debug, Clone, Copy)]
pub struct ProcGenGizmos;

impl GizmoConfigGroup for ProcGenGizmos {
    const NAME: &'static str = "procgen";

    fn default_config() -> GizmoConfig {
        GizmoConfig {
            enabled: false,
            ..GizmoConfig::default()
        }
    }
}

/// Which overlays are on (`procgen.set_debug`).
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProcGenDebug {
    /// Cell grids, anchors, priorities and seam risk (spec 66 § 7).
    pub tiles: bool,
    /// Generator bounds.
    pub regions: bool,
}

/// The color of a cell's state.
pub fn state_color(state: TileState) -> Color {
    match state {
        TileState::Queued => Color::rgb(0.55, 0.55, 0.6),
        TileState::Capturing | TileState::Running => Color::rgb(1.0, 0.75, 0.1),
        TileState::Applying => Color::rgb(0.2, 0.75, 1.0),
        TileState::Resident => Color::rgb(0.2, 1.0, 0.35),
        TileState::Releasing => Color::rgb(0.7, 0.4, 0.1),
        TileState::Failed => Color::rgb(1.0, 0.15, 0.15),
    }
}

/// Outline color of any cell whose `seam_mismatch_risk` is greater than 0. It replaces the state
/// color. The priority number on a queued cell stays the queued gray (spec 66 § 7).
pub const SEAM_RISK_COLOR: Color = Color::rgb(1.0, 0.2, 0.9);
/// An anchor's build circle.
pub const BUILD_COLOR: Color = Color::rgb(0.3, 0.9, 1.0);
/// An anchor's release circle.
pub const RELEASE_COLOR: Color = Color::rgb(0.2, 0.4, 0.7);
/// A generator's bounds.
pub const BOUNDS_COLOR: Color = Color::rgb(0.95, 0.95, 0.4);

/// One cell of the overlay.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OverlayCell {
    /// The cell.
    pub key: TileKey,
    /// `[min_x, min_z, max_x, max_z]`.
    pub rect: [f32; 4],
    /// Its state.
    pub state: TileState,
    /// Its priority while queued.
    pub priority: Option<f64>,
    /// It has seam-risk points.
    pub seam_risk: bool,
}

/// One generator's overlay in world units.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TileOverlay {
    /// The cells of every tier.
    pub cells: Vec<OverlayCell>,
    /// Per anchor and tier: `(position, build radius, release radius)`.
    pub circles: Vec<(Vec3, f32, f32)>,
    /// Bounds rectangle `[min_x, min_z, max_x, max_z]`.
    pub bounds: [f32; 4],
    /// Height the rectangles sit at (the bounds center).
    pub y: f32,
}

/// What the overlay shows for each tiled generator, in generator order.
pub fn tile_overlay(world: &mut World) -> Vec<TileOverlay> {
    let mut gens: Vec<(Entity, ProcGenerator)> = world
        .query::<(Entity, &ProcGenerator)>()
        .iter(world)
        .filter(|(_, g)| g.tiles.is_some())
        .map(|(e, g)| (e, g.clone()))
        .collect();
    gens.sort_by_key(|g| g.0);
    let mut out = Vec::new();
    for (entity, generator) in gens {
        let bounds = generator_bounds(world, entity);
        let (lo, hi) = bounds.world_aabb();
        let mut overlay = TileOverlay {
            bounds: [lo[0], lo[2], hi[0], hi[2]],
            y: (lo[1] + hi[1]) * 0.5,
            ..TileOverlay::default()
        };
        let runtime = generator.tiles.as_ref().and_then(|t| t.runtime.clone());
        let anchors = runtime
            .as_ref()
            .filter(|_| is_scheduled(&generator))
            .map(|r| gather_anchors(world, r.anchors))
            .unwrap_or_default();
        let tiles = world.resource::<ProcGenTiles>();
        if let Some(g) = tiles.generators.get(&entity) {
            let host = world.get::<ProcGenTileHost>(entity);
            for (k, entry) in &g.entries {
                let Some(c) = g.tiers.iter().find(|t| t.index == k.tier).and_then(|t| t.cell_m) else {
                    continue;
                };
                let r = crate::tiles::TileCell::rect(k.cell, c).map(|v| v as f32);
                let seam = host
                    .and_then(|h| h.cell(*k))
                    .is_some_and(|c| c.seam_mismatch_risk > 0);
                let queued = (entry.state == TileState::Queued).then_some(entry.priority);
                overlay.cells.push(OverlayCell {
                    key: *k,
                    rect: r,
                    state: entry.state,
                    priority: queued,
                    seam_risk: seam,
                });
            }
            if let Some(r) = &runtime {
                for a in &anchors {
                    for t in &g.tiers {
                        let reach = r.reach_for(t.cell_m);
                        let scale = a.reach_scale * g.quality_scale;
                        overlay.circles.push((
                            a.pos,
                            reach.build_m * scale,
                            reach.build_m * reach.release_factor() * scale,
                        ));
                    }
                }
            }
        }
        out.push(overlay);
    }
    out
}

fn rect_loop(gizmos: &mut Gizmos<ProcGenGizmos>, r: [f32; 4], y: f32, color: Color) {
    gizmos.lineloop(
        [
            Vec3::new(r[0], y, r[1]),
            Vec3::new(r[2], y, r[1]),
            Vec3::new(r[2], y, r[3]),
            Vec3::new(r[0], y, r[3]),
        ],
        color,
    );
}

/// The overlay of this frame, gathered by [`collect_overlay`] and drawn by [`draw_overlay`].
#[derive(Resource, Clone, Debug, Default)]
pub struct ProcGenOverlay(pub Vec<TileOverlay>);

/// `PostUpdate` (exclusive): gathers what the overlays that are on will draw.
pub fn collect_overlay(world: &mut World) {
    let debug = world.get_resource::<ProcGenDebug>().copied().unwrap_or_default();
    let overlays = if debug.tiles || debug.regions {
        let _span = tracing::info_span!("procgen.debug_overlay").entered();
        tile_overlay(world)
    } else {
        Vec::new()
    };
    world.insert_resource(ProcGenOverlay(overlays));
}

/// `PostUpdate`: draws the gathered overlay.
pub fn draw_overlay(
    overlay: Res<ProcGenOverlay>,
    debug: Res<ProcGenDebug>,
    mut gizmos: Gizmos<ProcGenGizmos>,
) {
    for o in &overlay.0 {
        if debug.regions {
            rect_loop(&mut gizmos, o.bounds, o.y, BOUNDS_COLOR);
        }
        if !debug.tiles {
            continue;
        }
        for c in &o.cells {
            let r = c.rect;
            let color = if c.seam_risk {
                SEAM_RISK_COLOR
            } else {
                state_color(c.state)
            };
            rect_loop(&mut gizmos, r, o.y, color);
            if let Some(p) = c.priority {
                let center = Vec3::new((r[0] + r[2]) * 0.5, o.y, (r[1] + r[3]) * 0.5);
                let flat = Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2);
                gizmos.text(
                    (center, flat),
                    &format!("{p:.2}"),
                    (r[2] - r[0]) * 0.1,
                    Vec2::ZERO,
                    state_color(TileState::Queued),
                );
            }
        }
        for (pos, build, release) in &o.circles {
            let flat = Quat::from_rotation_x(std::f32::consts::FRAC_PI_2);
            gizmos.circle((*pos, flat), *build, BUILD_COLOR).resolution(64);
            gizmos
                .circle((*pos, flat), *release, RELEASE_COLOR)
                .resolution(64);
        }
    }
}
