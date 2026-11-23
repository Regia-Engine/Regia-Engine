// SPDX-License-Identifier: MPL-2.0
//! The bridge from a layered [`NavMesh`] to the deterministic layered bake (plan row W-20, ADR-8486).
//!
//! [`NavMesh::det_layer_inputs`] lists the mesh's polygons, portals and enabled off-mesh links in
//! the plain form [`crate::det::layers::LayeredBake::bake`] reads, and [`NavMesh::det_layered_bake`]
//! bakes them. The det module itself imports nothing from the rest of the crate, so this lives here.
//!
//! With the `stage_b` feature, `geom` cooks the layered mesh from a world-geometry map
//! (`regia_world_geom::WorldGeom`) and feeds its blockers and ground edits into
//! [`crate::det::layers::LayerState`].

use crate::det::layers::{
    LayerLinkInput, LayerPolyInput, LayerPortalInput, LayeredBake, LayeredBakeSettings, mm,
};
use crate::mesh::NavMesh;

#[cfg(feature = "stage_b")]
pub mod geom;

impl NavMesh {
    /// The mesh's polygons (with their portals) and its enabled off-mesh links, meters, for
    /// [`LayeredBake::bake`]. Sections are `None` (a world-geometry cook sets them).
    pub fn det_layer_inputs(&self) -> (Vec<LayerPolyInput>, Vec<LayerLinkInput>) {
        let polys = self
            .polys
            .iter()
            .map(|p| LayerPolyInput {
                vertices: p.vertices.iter().map(|v| [v.x, v.y, v.z]).collect(),
                area: p.area,
                portals: p
                    .portals
                    .iter()
                    .map(|q| LayerPortalInput {
                        neighbor: q.neighbor,
                        a: [q.a.x, q.a.y, q.a.z],
                        b: [q.b.x, q.b.y, q.b.z],
                    })
                    .collect(),
                section: None,
            })
            .collect();
        let links = self
            .off_mesh_links
            .iter()
            .filter(|l| l.enabled)
            .map(|l| LayerLinkInput {
                start: [l.from.x, l.from.y, l.from.z],
                end: [l.to.x, l.to.y, l.to.z],
                bidirectional: l.bidirectional,
                flags: u32::from(l.flags),
            })
            .collect();
        (polys, links)
    }

    /// The deterministic layered bake of this mesh, with nav tiles `tile_mm` wide and the build
    /// agent's erosion radius recorded.
    pub fn det_layered_bake(&self, tile_mm: i32) -> LayeredBake {
        let (polys, links) = self.det_layer_inputs();
        LayeredBake::bake(
            &polys,
            &links,
            LayeredBakeSettings {
                tile_mm,
                agent_radius_mm: mm(self.config.agent.erosion_radius()),
                ..LayeredBakeSettings::default()
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::det::layers::{LAYER_AREA_FLANK, LAYER_AREA_TUNNEL};

    #[test]
    fn the_det_area_ids_are_the_builder_area_ids() {
        assert_eq!(LAYER_AREA_TUNNEL, crate::NAV_AREA_TUNNEL);
        assert_eq!(LAYER_AREA_FLANK, crate::NAV_AREA_FLANK);
    }
}
