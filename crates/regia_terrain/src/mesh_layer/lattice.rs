// SPDX-License-Identifier: MPL-2.0
//! The lattice cut of the open mouths as the GPU draws it (spec 26 § 2a.4 item 2, ADR-8470).
//!
//! The cut is W-10's cooked mouth-cut bitmap ([`regia_world_geom::ground::GroundLattice::is_cut`]),
//! windowed around each open mouth's rings: [`crate::holes::LatticeHoleCut`]. `terrain.wgsl`'s
//! `in_hole` reads the windows at the pre-morph grid position; [`TerrainMeshLayers::cuts`] is the
//! CPU mirror.

use regia_app::prelude::*;
use regia_world_geom::ground::GroundLattice;

use super::{MouthBands, TerrainMeshLayers};
use crate::Terrain;
use crate::holes::{LatticeHoleCut, TerrainHoles};
use crate::sampler::TerrainMap;

/// Requests the lattice cut of a changed [`TerrainMeshLayers`] on the terrain's draw
/// ([`TerrainHoles::request_lattice`]), and the open mouths' bands ([`TerrainHoles::request_bands`],
/// ADR-8569): every open mouth draws from the cooked cut, with its band, on the next prepare.
/// A removed component, or one with no open mouth, requests `None` (coverage windows, no band).
/// Runs only when a layer component changed or went away.
pub fn sync_mesh_layer_lattice(
    changed: Query<(&Terrain, &TerrainMeshLayers), Changed<TerrainMeshLayers>>,
    mut removed: RemovedComponents<TerrainMeshLayers>,
    holes: Option<Res<TerrainHoles>>,
) {
    let Some(holes) = holes else {
        return;
    };
    let gone = removed.read().count() > 0;
    let mut request = None;
    for (terrain, layers) in &changed {
        let _span = tracing::info_span!("terrain.mesh_layer_lattice").entered();
        // The band draws with the cut (ADR-8569): an open mouth shows both on the same prepare.
        request = Some((
            layers.lattice_hole_cut(&terrain.map),
            MouthBands::from_layers(layers).map(std::sync::Arc::new),
        ));
    }
    match request {
        Some((cut, bands)) => {
            holes.request_lattice(cut);
            holes.request_bands(bands);
        }
        None if gone => {
            holes.request_lattice(None);
            holes.request_bands(None);
        }
        None => {}
    }
}

impl TerrainMeshLayers {
    /// The lattice windows of the open mouths on `map`'s sample grid, or `None` without cooked
    /// geometry, without an open mouth, or when the cooked lattice is not `map`'s grid (its
    /// sample count differs).
    ///
    /// Each open mouth gets the cells of its rings' box (plus the near-ring band); a triangle is
    /// cut where the cooked bitmap cuts it.
    #[must_use]
    pub fn lattice_hole_cut(&self, map: &TerrainMap) -> Option<LatticeHoleCut> {
        let geom = self.geom()?;
        let cooked = geom.cooked();
        let n = cooked.lattice.lattice_n;
        if n < 2 || map.width() != n || map.height() != n {
            return None;
        }
        let ground = GroundLattice::new(cooked);
        let origin = map.sample_to_world(0, 0);
        let far = map.sample_to_world(n - 1, n - 1);
        let cells = n - 1;
        let step = (far[0] - origin[0]) / cells as f32;
        let mut cut = LatticeHoleCut::new(origin, step, [cells, cells]);
        let mut any = false;
        for mouth in self.mouths().iter().filter(|m| m.open) {
            let mut lo = [u32::MAX; 2];
            let mut hi = [0u32; 2];
            for p in mouth.rings.iter().flatten() {
                let [u, v] = map.world_to_grid(p[0], p[2]);
                let (u, v) = (u.clamp(0.0, cells as f32), v.clamp(0.0, cells as f32));
                lo = [lo[0].min(u.floor() as u32), lo[1].min(v.floor() as u32)];
                hi = [hi[0].max(u.ceil() as u32 + 1), hi[1].max(v.ceil() as u32 + 1)];
            }
            if lo[0] >= hi[0] || lo[1] >= hi[1] {
                continue;
            }
            let hi = [hi[0].min(cells), hi[1].min(cells)];
            cut.set_hole(&mouth.hole, lo, hi, |i, j, t| ground.is_cut(i, j, t));
            any = true;
        }
        any.then_some(cut)
    }

    /// `true` where world `(x, z)`, read as a pre-morph grid position, lies on a lattice triangle
    /// an open mouth cuts: the CPU mirror of `terrain.wgsl`'s lattice-window test on `map`
    /// ([`LatticeHoleCut::cuts`] of [`Self::lattice_hole_cut`]). Builds the windows: for one-off
    /// queries; keep the cut for many.
    #[must_use]
    pub fn cuts(&self, map: &TerrainMap, x: f32, z: f32) -> bool {
        self.lattice_hole_cut(map).is_some_and(|c| c.cuts(x, z))
    }
}
