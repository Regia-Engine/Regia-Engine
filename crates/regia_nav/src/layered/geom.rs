// SPDX-License-Identifier: MPL-2.0
//! The layered bake of a world-geometry map, and the blocker and ground-edit feed of its state
//! (plan row W-20, ADR-8486; `stage_b`).
//!
//! [`cook_layered`] rasterizes the map's layer triangles (floors, walls and roofs of every
//! passage; blocker plugs left out, they close their section at run time) and, optionally, the
//! ground as quads sampled from the native query, builds the layered polygon mesh with
//! [`NavMeshBuilder::build_layered`], tags each polygon with the section of the floor under it, and
//! bakes it to integer millimeters.
//!
//! [`sync_layers`] feeds one world-geometry generation into a [`LayerState`]: the closed sections
//! (`WorldGeom::closed_sections`, W-13's blockers) and the lattice rectangles of the ground edits
//! (W-12's `DirtyRect`s), in that order, at the state's generation.

use regia_core::math::Vec3;
use regia_world_geom::cooked::TriKind;
use regia_world_geom::edit::DirtyRect;
use regia_world_geom::{GeomState, SpanBuf, SurfaceKind, WorldGeom};

use crate::builder::{NavBuildError, NavMeshBuilder};
use crate::config::{NavAgentParams, NavMeshBuildParams};
use crate::det::layers::{
    LayerPolyInput, LayerState, LayerSync, LayeredBake, LayeredBakeSettings, RectMm, mm,
};
use crate::mesh::NavMesh;
use crate::{NAV_AREA_DEFAULT, NAV_AREA_TUNNEL};

/// Settings of [`cook_layered`].
#[derive(Clone, Debug, PartialEq)]
pub struct LayeredGeomSettings {
    /// The hull class the bake is for.
    pub agent: NavAgentParams,
    /// The voxel size, meters.
    pub cell_size: f32,
    /// The area baked, `[x0, z0, x1, z1]` meters; triangles wholly outside it are skipped.
    pub bounds: [f32; 4],
    /// Whether the ground is baked too (as quads `ground_step_m` wide over `bounds`).
    pub include_ground: bool,
    /// The ground quad side, meters.
    pub ground_step_m: f32,
    /// The most walkable layers kept per column.
    pub max_layers: u16,
    /// The nav tile side, millimeters.
    pub tile_mm: i32,
}

impl LayeredGeomSettings {
    /// A hull of `radius` over `bounds`: the hover step, standing height `radius + 1.5`, eroded by
    /// the radius alone; 0.5 m voxels, passages only, 8 layers, 32 m tiles. The slope limit is
    /// 60 degrees: a layer floor is walkable by the world geometry's own classification
    /// (`TriKind::Floor`), so the limit only has to reject walls, and a cooked ramp floor
    /// alternates flat facets with facets up to about 58 degrees (a 45 degree limit cut the
    /// Switchbacks' west and east ramps off the tube).
    pub fn hull(radius: f32, bounds: [f32; 4]) -> LayeredGeomSettings {
        LayeredGeomSettings {
            agent: NavAgentParams {
                radius,
                height: radius + 1.5,
                max_climb: crate::config::HULL_STEP,
                max_slope: std::f32::consts::FRAC_PI_3,
            },
            cell_size: 0.5,
            bounds,
            include_ground: false,
            ground_step_m: 4.0,
            max_layers: 8,
            tile_mm: crate::det::layers::DEFAULT_TILE_MM,
        }
    }
}

/// The triangles (and their areas) [`cook_layered`] rasterizes: every layer triangle except the
/// blocker plugs (tunnel area), then the ground quads (default area) if enabled.
pub fn layered_triangles(
    geom: &WorldGeom,
    state: &GeomState,
    settings: &LayeredGeomSettings,
) -> (Vec<[Vec3; 3]>, Vec<u8>) {
    let [x0, z0, x1, z1] = settings.bounds;
    let mut tris = Vec::new();
    let mut areas = Vec::new();
    for t in geom.triangles() {
        if t.kind == TriKind::Blocker {
            continue;
        }
        let xs = [t.v[0][0], t.v[1][0], t.v[2][0]];
        let zs = [t.v[0][2], t.v[1][2], t.v[2][2]];
        if xs.iter().all(|&x| x < x0) || xs.iter().all(|&x| x > x1) {
            continue;
        }
        if zs.iter().all(|&z| z < z0) || zs.iter().all(|&z| z > z1) {
            continue;
        }
        tris.push(t.v.map(|v| Vec3::new(v[0], v[1], v[2])));
        areas.push(NAV_AREA_TUNNEL);
    }
    if settings.include_ground {
        let step = regia_detsim::float::det_max(settings.ground_step_m, 0.5);
        let nx = ((x1 - x0) / step) as usize;
        let nz = ((z1 - z0) / step) as usize;
        let mut spans = SpanBuf::default();
        let mut ground = |x: f32, z: f32| -> Option<f32> {
            geom.floors_at(state, x, z, &mut spans);
            spans
                .spans()
                .iter()
                .find(|s| s.floor.kind == SurfaceKind::Ground)
                .map(|s| s.floor.y)
        };
        for j in 0..nz {
            for i in 0..nx {
                let (ax, az) = (x0 + i as f32 * step, z0 + j as f32 * step);
                let (bx, bz) = (ax + step, az + step);
                let c = [ground(ax, az), ground(bx, az), ground(bx, bz), ground(ax, bz)];
                let [Some(h0), Some(h1), Some(h2), Some(h3)] = c else {
                    continue;
                };
                let (p0, p1, p2, p3) = (
                    Vec3::new(ax, h0, az),
                    Vec3::new(bx, h1, az),
                    Vec3::new(bx, h2, bz),
                    Vec3::new(ax, h3, bz),
                );
                tris.push([p0, p2, p1]);
                tris.push([p0, p3, p2]);
                areas.push(NAV_AREA_DEFAULT);
                areas.push(NAV_AREA_DEFAULT);
            }
        }
    }
    (tris, areas)
}

/// Cooks the layered polygon mesh of `geom` and its deterministic bake. A polygon whose height
/// range (1 m slack) holds no floor of the native query at its centroid is dropped with its
/// portals: the top of a passage's roof slab rasterizes as an open surface, but nothing stands
/// there. Each kept polygon's section is the section of the nearest such floor to its mean height
/// (none outdoors).
///
/// # Errors
///
/// The builder's [`NavBuildError`] (no walkable surface in the bounds, bad settings).
pub fn cook_layered(
    geom: &WorldGeom,
    state: &GeomState,
    settings: &LayeredGeomSettings,
) -> Result<(NavMesh, LayeredBake), NavBuildError> {
    let _span = tracing::info_span!("nav_cook_layered").entered();
    let (tris, areas) = layered_triangles(geom, state, settings);
    let params = NavMeshBuildParams {
        cell_size: settings.cell_size,
        agent: settings.agent.clone(),
        ..NavMeshBuildParams::default()
    };
    let mesh = NavMeshBuilder::new(params).build_layered(&tris, &areas, &[], settings.max_layers)?;
    let (polys, links) = mesh.det_layer_inputs();
    let mut spans = SpanBuf::default();
    // The kept polygons' new indices (`u32::MAX` dropped) and their sections.
    let mut remap = vec![u32::MAX; polys.len()];
    let mut sections: Vec<Option<u16>> = Vec::with_capacity(polys.len());
    let mut kept = 0u32;
    for (i, p) in polys.iter().enumerate() {
        // A vertex count, at least 1 (integer `max`, no float extremum).
        let n = usize::max(p.vertices.len(), 1) as f32;
        let (mut cx, mut cz, mut sy) = (0.0f32, 0.0f32, 0.0f32);
        let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
        for v in &p.vertices {
            cx += v[0];
            cz += v[2];
            sy += v[1];
            lo = regia_detsim::float::det_min(lo, v[1]);
            hi = regia_detsim::float::det_max(hi, v[1]);
        }
        let (cx, cz, y) = (cx / n, cz / n, sy / n);
        geom.floors_at(state, cx, cz, &mut spans);
        let mut best: Option<(f32, Option<u16>)> = None;
        for s in spans.spans() {
            if s.floor.y < lo - 1.0 || s.floor.y > hi + 1.0 {
                continue;
            }
            let d = (s.floor.y - y).abs();
            if best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, s.floor.section.map(|id| id.0)));
            }
        }
        if let Some((_, section)) = best {
            remap[i] = kept;
            kept += 1;
            sections.push(section);
        }
    }
    let polys: Vec<LayerPolyInput> = polys
        .into_iter()
        .zip(remap.iter())
        .filter(|(_, r)| **r != u32::MAX)
        .zip(sections)
        .map(|((mut p, _), section)| {
            p.portals.retain_mut(|q| match remap.get(q.neighbor as usize) {
                Some(&r) if r != u32::MAX => {
                    q.neighbor = r;
                    true
                }
                _ => false,
            });
            p.section = section;
            p
        })
        .collect();
    let bake = LayeredBake::bake(
        &polys,
        &links,
        LayeredBakeSettings {
            tile_mm: settings.tile_mm,
            agent_radius_mm: mm(settings.agent.erosion_radius()),
            ..LayeredBakeSettings::default()
        },
    );
    Ok((mesh, bake))
}

/// The world rectangle, millimeters, of the ground cells a lattice rectangle's vertices touch.
pub fn dirty_rect_mm(geom: &WorldGeom, rect: DirtyRect) -> RectMm {
    let l = geom.cooked().lattice;
    let last = l.cells();
    let at = |i: u32| l.vertex_mm(i.min(last)) as i32;
    RectMm {
        x0: at(rect.col0.saturating_sub(1)),
        z0: at(rect.row0.saturating_sub(1)),
        x1: at(rect.col1),
        z1: at(rect.row1),
    }
}

/// Feeds one world-geometry generation into `layers`: the closed sections of `state` (blockers),
/// then `edits` (the dirty rectangles the ground edits since the last sync returned), at
/// `state.generation()`. Returns the polygons whose closed bit flipped.
pub fn sync_layers(
    bake: &LayeredBake,
    layers: &mut LayerState,
    geom: &WorldGeom,
    state: &GeomState,
    edits: &[DirtyRect],
) -> Vec<u32> {
    let mut closed = Vec::new();
    geom.closed_sections(state, &mut closed);
    let sections: Vec<u16> = closed.iter().map(|s| s.0).collect();
    let rects: Vec<RectMm> = edits
        .iter()
        .filter(|r| !r.is_empty())
        .map(|r| dirty_rect_mm(geom, *r))
        .collect();
    layers.sync(
        bake,
        &LayerSync {
            generation: state.generation().0,
            closed_sections: &sections,
            dirty_rects: &rects,
        },
    )
}
