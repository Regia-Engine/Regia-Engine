// SPDX-License-Identifier: MPL-2.0
//! The band draw of the open mouths (spec 26 § 2a.4 steps 5-6, W-15 item 3, ADR-8569).
//!
//! [`MouthBands`] is the band of every open mouth as one triangle list: zipper strip, skirt and
//! throat. The terrain draw places it once per pass it draws (the camera passes and each sun
//! cascade, each with its own selection and morph eye): ring vertices go through
//! [`super::attach_ring_on`], a skirt bottom follows the ring vertex above it, and every other
//! vertex (the authored loop, the throat) stays where the cook put it. The placed vertices ride in
//! the terrain's hole words, and the terrain's own pipelines draw them, so the band adds no
//! pipeline.

use crate::cdlod::{CdlodConfig, LodSelection};
use crate::height_mips::HeightMips;
use crate::sampler::TerrainMap;

use super::{TerrainMeshLayers, attach_ring_on};

/// How one band vertex is placed in a pass.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BandPlacement {
    /// Fixed at the cook position (authored loop, throat).
    Fixed([f32; 3]),
    /// The attached ring vertex `ring`, then `dy` metres up (`0` for the ring vertex itself, minus
    /// the skirt depth for a skirt bottom).
    Ring {
        /// Index into [`MouthBands::ring_points`].
        ring: u32,
        /// Vertical offset from the attached ring vertex, metres.
        dy: f32,
    },
}

/// The band of every open mouth of one [`TerrainMeshLayers`], ready for the terrain draw.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MouthBands {
    /// Ring vertices to attach each pass (XZ is the lattice pin).
    pub ring_points: Vec<[f32; 3]>,
    /// Band vertices.
    pub vertices: Vec<BandPlacement>,
    /// Triangle corners (indices into [`Self::vertices`]), three a triangle, each triangle in both
    /// windings: the terrain pipelines cull back faces and the band is seen from both sides.
    pub corners: Vec<u32>,
    /// [`TerrainMeshLayers::generation`] the bands were built from.
    pub generation: u64,
}

impl MouthBands {
    /// The bands of `layers`' open mouths, or `None` when no open mouth has a band triangle.
    #[must_use]
    pub fn from_layers(layers: &TerrainMeshLayers) -> Option<Self> {
        let mut out = Self {
            generation: layers.generation(),
            ..Self::default()
        };
        for mouth in layers.mouths().iter().filter(|m| m.open) {
            let base = out.vertices.len() as u32;
            let ring_xz = |p: [f32; 3]| (p[0].to_bits(), p[2].to_bits());
            // The ring vertices this mouth attaches, keyed by their lattice XZ.
            let mut ring_of: std::collections::BTreeMap<(u32, u32), (u32, f32)> =
                std::collections::BTreeMap::new();
            for v in mouth.band.vertices.iter().filter(|v| v.ring) {
                ring_of.entry(ring_xz(v.position)).or_insert_with(|| {
                    out.ring_points.push(v.position);
                    ((out.ring_points.len() - 1) as u32, v.position[1])
                });
            }
            // The zipper appends one skirt bottom per ring vertex after the strip's vertices.
            let ring_total: usize = mouth.rings.iter().map(Vec::len).sum();
            let skirt_from = mouth.band.vertices.len().saturating_sub(ring_total);
            for (i, v) in mouth.band.vertices.iter().enumerate() {
                // A ring vertex, or a skirt bottom straight under one: both follow the attach.
                // An authored vertex never moves, even straight under a ring vertex.
                let skirt = i >= skirt_from;
                let placement = match ring_of.get(&ring_xz(v.position)) {
                    Some(&(ring, y)) if v.ring || (skirt && v.position[1] < y) => BandPlacement::Ring {
                        ring,
                        dy: if v.ring { 0.0 } else { v.position[1] - y },
                    },
                    _ => BandPlacement::Fixed(v.position),
                };
                out.vertices.push(placement);
            }
            for t in &mouth.band.triangles {
                out.push_both(t.map(|i| base + i));
            }
            for t in &mouth.throat {
                let first = out.vertices.len() as u32;
                out.vertices.extend(t.v.iter().map(|p| BandPlacement::Fixed(*p)));
                out.push_both([first, first + 1, first + 2]);
            }
        }
        (!out.corners.is_empty()).then_some(out)
    }

    fn push_both(&mut self, t: [u32; 3]) {
        self.corners
            .extend_from_slice(&[t[0], t[1], t[2], t[0], t[2], t[1]]);
    }

    /// Triangles drawn per pass (both windings count).
    #[must_use]
    pub fn triangle_count(&self) -> u32 {
        (self.corners.len() / 3) as u32
    }

    /// The band's vertices placed on the terrain one pass draws: `selection` and `morph_eye` are
    /// that pass's (the camera's, or a cascade's anchored selection and morph eye).
    #[must_use]
    pub fn place(
        &self,
        map: &TerrainMap,
        config: &CdlodConfig,
        root_level: u32,
        mips: &HeightMips,
        selection: &LodSelection,
        morph_eye: [f32; 2],
    ) -> Vec<[f32; 3]> {
        let ring = attach_ring_on(
            map,
            config,
            root_level,
            mips,
            selection,
            morph_eye,
            &self.ring_points,
        );
        self.place_with(&ring)
    }

    /// The band's vertices for already attached ring vertices (`ring[i]` for
    /// [`Self::ring_points`]`[i]`).
    #[must_use]
    pub fn place_with(&self, ring: &[[f32; 3]]) -> Vec<[f32; 3]> {
        self.vertices
            .iter()
            .map(|v| match *v {
                BandPlacement::Fixed(p) => p,
                BandPlacement::Ring { ring: r, dy } => {
                    let p = ring.get(r as usize).copied().unwrap_or([0.0; 3]);
                    [p[0], p[1] + dy, p[2]]
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh_layer::{BandMesh, BandVertex, CopiedTri, LayerMouth};

    fn vertex(p: [f32; 3], ring: bool) -> BandVertex {
        BandVertex {
            position: p,
            lod0: p,
            ring,
        }
    }

    /// A square ring at y 1 (four lattice vertices), an authored vertex straight under ring
    /// vertex 0 at y -2, the strip to it, and the skirt (one bottom per ring vertex, appended last).
    fn mouth(open: bool) -> LayerMouth {
        let ring = vec![[0.0, 1.0, 0.0], [1.0, 1.0, 0.0], [1.0, 1.0, 1.0], [0.0, 1.0, 1.0]];
        let mut vertices: Vec<BandVertex> = vec![vertex([0.0, -2.0, 0.0], false)];
        vertices.extend(ring.iter().map(|p| vertex(*p, true)));
        vertices.extend(ring.iter().map(|p| vertex([p[0], p[1] - 0.25, p[2]], false)));
        LayerMouth {
            hole: "m".into(),
            rings: vec![ring],
            band: BandMesh {
                vertices,
                triangles: vec![[0, 1, 2], [1, 2, 6]],
            },
            collar: Vec::new(),
            throat: vec![CopiedTri {
                v: [[0.2, -0.5, 0.2], [0.8, -0.5, 0.2], [0.5, -0.5, 0.8]],
                source: None,
            }],
            body: Vec::new(),
            open,
        }
    }

    #[test]
    fn ring_and_skirt_follow_the_attach_and_the_rest_stays() {
        let mut layers = TerrainMeshLayers::new();
        layers.push_mouth(mouth(true));
        let bands = MouthBands::from_layers(&layers).expect("an open mouth");
        assert_eq!(bands.ring_points.len(), 4);
        // The authored vertex under ring vertex 0 is not a skirt bottom: it never moves.
        assert_eq!(bands.vertices[0], BandPlacement::Fixed([0.0, -2.0, 0.0]));
        assert_eq!(bands.vertices[1], BandPlacement::Ring { ring: 0, dy: 0.0 });
        assert_eq!(bands.vertices[5], BandPlacement::Ring { ring: 0, dy: -0.25 });
        // Two band triangles and one throat triangle, each in both windings.
        assert_eq!(bands.triangle_count(), 6);
        assert_eq!(&bands.corners[..6], &[0, 1, 2, 0, 2, 1]);
        let ring: Vec<[f32; 3]> = bands
            .ring_points
            .iter()
            .map(|p| [p[0], p[1] + 3.0, p[2]])
            .collect();
        let placed = bands.place_with(&ring);
        assert_eq!(placed.len(), bands.vertices.len());
        assert_eq!(placed[0], [0.0, -2.0, 0.0]);
        assert_eq!(placed[1], [0.0, 4.0, 0.0]);
        assert_eq!(placed[6], [1.0, 3.75, 0.0]);
        assert_eq!(
            placed[9],
            [0.2, -0.5, 0.2],
            "the throat stays at its cook position"
        );
        // At the cook ring the band is the cook band.
        let cook = bands.place_with(&bands.ring_points);
        let mouth = mouth(true);
        for (a, b) in cook.iter().zip(&mouth.band.vertices) {
            assert_eq!(*a, b.position);
        }
    }

    #[test]
    fn a_closed_mouth_has_no_band() {
        let mut layers = TerrainMeshLayers::new();
        layers.push_mouth(mouth(false));
        assert!(MouthBands::from_layers(&layers).is_none());
        layers.set_open("m", true);
        assert!(MouthBands::from_layers(&layers).is_some());
    }
}
