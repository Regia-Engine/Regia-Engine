// SPDX-License-Identifier: MPL-2.0
//! Presentation seam between a cooked mesh layer and the CDLOD ground (spec 26 § 2a, ADR-8291).
//!
//! The canonical triangles stay in [`regia_world_geom::WorldGeom`]. This module derives the band
//! (zipper, skirt, throat) and the § 2a.6 ground collar for one dirty rect. It does not store a
//! second geometry, and it does not answer `surface_at` (that query is W-11's, on `WorldGeom`).

mod band_draw;
mod joins;
mod lattice;
mod seam;
pub mod stream;
pub mod volume;
mod zipper;

use regia_app::prelude::*;
use regia_world_geom::{TriRef, WorldGeom};

pub use band_draw::{BandPlacement, MouthBands};
pub use joins::{BandHit, LayerColliderPiece, LayerReferencePiece, REFERENCE_PIECE_TAG};
pub use lattice::sync_mesh_layer_lattice;
pub use seam::{RingAttach, attach_ring, attach_ring_detail, attach_ring_on, seam_report_with_mips};
pub use stream::{
    ChunkDraw, ChunkKey, CookedChunkLod, CookedLayer, CookedLayerChunk, DEFAULT_LAYER_FADE_FRAMES,
    LayerChunkGrid, LayerPresentationError, LayerStreamStats, MAX_LAYER_LODS, PRESENTATION_VERSION,
    REFERENCE_CHUNK_TAG, ReferenceChunks, TerrainLayerChunk, TerrainLayerStream, TerrainMeshLayersCooked,
    chunk_lod_asset_id, chunk_lod_asset_path, layer_mesh_resident_bytes, load_layer_chunk_mesh,
    reference_chunks_lod0, upgrade_cooked_mesh_layers,
};
pub use zipper::{ZipperInput, ZipperOutput, zipper};

use crate::Terrain;
use crate::deform::DirtyRect;
use crate::height_mips::HeightMips;
use crate::holes::HoleShape;
use crate::sampler::TerrainMap;

/// Why a seam cook refused a mouth.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum SeamCookError {
    /// No authored boundary loop lies within `max_stitch_gap_m` of the ring.
    #[error("seam loop for {hole} is {gap_m} m from the ring")]
    LoopMissing {
        /// The hole id.
        hole: String,
        /// The nearest loop's farthest vertex, metres in XZ.
        gap_m: f32,
    },
    /// The stitched strip folds back on itself.
    #[error("seam strip for {hole} folds at {at:?}")]
    StripFolds {
        /// The hole id.
        hole: String,
        /// A point on the folded triangle.
        at: [f32; 3],
    },
    /// An authored loop vertex outside the cut rises above the LOD-0 ground.
    #[error("seam vertex for {hole} is above the ground at {at:?}")]
    AboveGround {
        /// The hole id.
        hole: String,
        /// The authored vertex.
        at: [f32; 3],
    },
}

impl SeamCookError {
    /// Agent and cook code: `seam_loop_missing`, `seam_strip_folds` or `seam_above_ground`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::LoopMissing { .. } => "seam_loop_missing",
            Self::StripFolds { .. } => "seam_strip_folds",
            Self::AboveGround { .. } => "seam_above_ground",
        }
    }
}

/// Distances for one zipper (spec 26 § 2a.4).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ZipperParams {
    /// A boundary loop farther than this, at its farthest vertex, is `seam_loop_missing`.
    pub max_stitch_gap_m: f32,
    /// Vertical skirt dropped from the ring, metres.
    pub skirt_m: f32,
    /// Source triangles less than this far under the ground, inside the footprint, join the band.
    pub throat_m: f32,
    /// An authored vertex outside the cut may sit this far above the LOD-0 ground.
    pub above_ground_slack_m: f32,
}

impl Default for ZipperParams {
    fn default() -> Self {
        Self {
            max_stitch_gap_m: 1.0,
            skirt_m: 0.25,
            throat_m: 2.0,
            above_ground_slack_m: 0.01,
        }
    }
}

/// One vertex of the presentation band.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BandVertex {
    /// Drawn position. Ring vertices keep the lattice XZ.
    pub position: [f32; 3],
    /// LOD-0 position. A height edit rewrites Y inside the dirty rect and leaves XZ.
    pub lod0: [f32; 3],
    /// `true` for a lattice ring vertex (not a skirt bottom).
    pub ring: bool,
}

/// The zipper strip plus the skirt. Indices address [`BandMesh::vertices`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BandMesh {
    /// Band vertices.
    pub vertices: Vec<BandVertex>,
    /// Triangles.
    pub triangles: Vec<[u32; 3]>,
}

impl BandMesh {
    /// Triangle corners in vertex order.
    #[must_use]
    pub fn triangle_positions(&self) -> Vec<[[f32; 3]; 3]> {
        self.triangles
            .iter()
            .filter_map(|ix| {
                Some([
                    self.vertices.get(ix[0] as usize)?.position,
                    self.vertices.get(ix[1] as usize)?.position,
                    self.vertices.get(ix[2] as usize)?.position,
                ])
            })
            .collect()
    }
}

/// A source triangle copied into the throat or left in the body.
///
/// The copy is the presentation. The canonical triangle stays in `WorldGeom`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CopiedTri {
    /// Vertex positions, metres.
    pub v: [[f32; 3]; 3],
    /// Canonical id when the triangle came from a cooked layer.
    pub source: Option<TriRef>,
}

/// One mouth after the zipper and the collar.
#[derive(Clone, Debug)]
pub struct LayerMouth {
    /// Named hole this mouth fills.
    pub hole: String,
    /// Lattice rings (XZ is the pin; Y is the cook height). The zipper reads these.
    pub rings: Vec<Vec<[f32; 3]>>,
    /// Strip and skirt.
    pub band: BandMesh,
    /// § 2a.6 collar triangles (LOD-0), outside the lattice cut.
    pub collar: Vec<[[f32; 3]; 3]>,
    /// Throat triangles moved into the band.
    pub throat: Vec<CopiedTri>,
    /// Source triangles that stay in the body.
    pub body: Vec<CopiedTri>,
    /// A closed mouth draws no band and no throat.
    pub open: bool,
}

impl LayerMouth {
    /// Assembles a mouth from a zipper result and a collar.
    #[must_use]
    pub fn from_zipper(
        hole: impl Into<String>,
        ring: Vec<[f32; 3]>,
        output: ZipperOutput,
        collar: Vec<[[f32; 3]; 3]>,
        open: bool,
    ) -> Self {
        Self {
            hole: hole.into(),
            rings: vec![ring],
            band: output.band,
            collar,
            throat: output.throat,
            body: output.body,
            open,
        }
    }

    /// Band and throat triangles when the mouth is open, otherwise nothing.
    #[must_use]
    pub fn band_draw(&self) -> Vec<[[f32; 3]; 3]> {
        if !self.open {
            return Vec::new();
        }
        let mut draw = self.band.triangle_positions();
        draw.extend(self.throat.iter().map(|t| t.v));
        draw
    }
}

/// Which ground surface owns an XZ point just outside a lattice cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceOwner {
    /// The point's LOD-0 triangle is inside the lattice cut.
    LatticeCut,
    /// A removed heightfield cell's triangle that the lattice cut did not take.
    Collar,
    /// A heightfield cell `cut_heights` would keep.
    Heightfield,
    /// Outside the sample grid.
    OutsideMap,
}

/// The LOD-0 triangle that contains `(x, z)`, and whether the cell's samples are cut.
struct Lod0Tri {
    corners: [[f32; 3]; 3],
    removed_cell: bool,
    centroid_in_hole: bool,
}

/// Classifies `(x, z)` for the collar coverage test.
///
/// A cell is removed when any of its four samples lies in `shape` (`HolesMask::cut_heights`
/// with a zero margin). A triangle of that cell whose centroid is outside `shape` is the
/// collar. A triangle whose centroid is inside `shape` is the lattice cut.
#[must_use]
pub fn surface_owner(map: &TerrainMap, shape: &HoleShape, x: f32, z: f32) -> SurfaceOwner {
    match lod0_tri(map, shape, x, z) {
        None => SurfaceOwner::OutsideMap,
        Some(tri) if tri.centroid_in_hole => SurfaceOwner::LatticeCut,
        Some(tri) if tri.removed_cell => SurfaceOwner::Collar,
        Some(_) => SurfaceOwner::Heightfield,
    }
}

/// Collar triangles: LOD-0 triangles of cells the heightfield cut removes that lie outside
/// the lattice cut. This replaces the temporary collar W-10 stored on the canonical mesh.
#[must_use]
pub fn collar_triangles(map: &TerrainMap, shape: &HoleShape) -> Vec<[[f32; 3]; 3]> {
    collect_tris(map, shape, |tri| tri.removed_cell && !tri.centroid_in_hole)
}

/// LOD-0 triangles of cells the heightfield cut keeps, excluding any whose centroid is inside
/// the lattice cut.
#[must_use]
pub fn heightfield_triangles(map: &TerrainMap, shape: &HoleShape) -> Vec<[[f32; 3]; 3]> {
    collect_tris(map, shape, |tri| !tri.removed_cell && !tri.centroid_in_hole)
}

fn collect_tris(map: &TerrainMap, shape: &HoleShape, keep: impl Fn(&Lod0Tri) -> bool) -> Vec<[[f32; 3]; 3]> {
    let mut out = Vec::new();
    let (w, h) = (map.width(), map.height());
    if w < 2 || h < 2 {
        return out;
    }
    for row in 0..h - 1 {
        for col in 0..w - 1 {
            for which in [false, true] {
                if let Some(tri) = cell_tri(map, shape, col, row, which)
                    && keep(&tri)
                {
                    out.push(tri.corners);
                }
            }
        }
    }
    out
}

fn lod0_tri(map: &TerrainMap, shape: &HoleShape, x: f32, z: f32) -> Option<Lod0Tri> {
    let [u, v] = map.world_to_grid(x, z);
    let w = map.width();
    let h = map.height();
    if w < 2 || h < 2 || u < 0.0 || v < 0.0 || u > (w - 1) as f32 || v > (h - 1) as f32 {
        return None;
    }
    let col = (u.floor() as u32).min(w - 2);
    let row = (v.floor() as u32).min(h - 2);
    let fu = (u - col as f32).clamp(0.0, 1.0);
    let fv = (v - row as f32).clamp(0.0, 1.0);
    cell_tri(map, shape, col, row, fu + fv > 1.0)
}

fn cell_tri(map: &TerrainMap, shape: &HoleShape, col: u32, row: u32, upper: bool) -> Option<Lod0Tri> {
    let corner = |c: u32, r: u32| -> [f32; 3] {
        let [x, z] = map.sample_to_world(c, r);
        [x, map.height_at_sample(i64::from(c), i64::from(r)), z]
    };
    let v00 = corner(col, row);
    let v10 = corner(col + 1, row);
    let v01 = corner(col, row + 1);
    let v11 = corner(col + 1, row + 1);
    let removed_cell = [v00, v10, v01, v11].iter().any(|p| shape.contains(p[0], p[2]));
    let corners = if upper { [v11, v01, v10] } else { [v00, v01, v10] };
    let c = [
        (corners[0][0] + corners[1][0] + corners[2][0]) / 3.0,
        (corners[0][2] + corners[1][2] + corners[2][2]) / 3.0,
    ];
    Some(Lod0Tri {
        corners,
        removed_cell,
        centroid_in_hole: shape.contains(c[0], c[1]),
    })
}

/// Aggregate seam of one selection (spec 26 § 2a.11).
#[derive(Clone, Debug, PartialEq)]
pub struct SeamMouthReport {
    /// Largest drawn-terrain-above-ring sample, metres.
    pub max_gap_m: f32,
    /// Largest ring-above-drawn-terrain sample, metres.
    pub max_lip_m: f32,
    /// World position of the worse of the two.
    pub worst_at: [f32; 3],
    /// Ring vertices walked.
    pub ring_vertices: u32,
    /// Node levels the ring vertices landed on.
    pub pass_levels: Vec<u32>,
}

/// Presentation of the cooked mesh layers on one terrain.
///
/// Holds the canonical [`WorldGeom`] when a cooked file was loaded, and the derived mouths.
/// A map with no `<map>.meshlayers` has no component, so the heightfield draw is unchanged.
#[derive(Component, Clone, Debug, Default)]
pub struct TerrainMeshLayers {
    geom: Option<std::sync::Arc<WorldGeom>>,
    mouths: Vec<LayerMouth>,
    generation: u64,
    /// The cooked presentation section (layer chunks), when the file had one.
    presentation: Option<std::sync::Arc<TerrainMeshLayersCooked>>,
    /// Where the chunk LOD meshes are read when there is no asset server.
    chunk_dir: Option<std::path::PathBuf>,
    /// Bumps when the presentation is replaced (the stream starts over).
    presentation_generation: u64,
}

impl TerrainMeshLayers {
    /// An empty presentation, generation 0.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Remembers the canonical geometry. The triangles are not copied.
    #[must_use]
    pub fn with_geom(geom: std::sync::Arc<WorldGeom>) -> Self {
        Self {
            geom: Some(geom),
            generation: 1,
            ..Self::default()
        }
    }

    /// Sets the layer chunks (the cooked presentation section, spec 26 § 2a.5) and the directory the
    /// chunk LOD meshes are read from when the app has no asset server.
    #[must_use]
    pub fn with_presentation(
        mut self,
        presentation: TerrainMeshLayersCooked,
        chunk_dir: Option<std::path::PathBuf>,
    ) -> Self {
        self.presentation = Some(std::sync::Arc::new(presentation));
        self.chunk_dir = chunk_dir;
        self.presentation_generation = self.presentation_generation.wrapping_add(1);
        self
    }

    /// The layer chunks, when the cooked file had a presentation section.
    #[must_use]
    pub fn presentation(&self) -> Option<&TerrainMeshLayersCooked> {
        self.presentation.as_deref()
    }

    /// A shared handle on [`Self::presentation`].
    #[must_use]
    pub fn presentation_arc(&self) -> Option<std::sync::Arc<TerrainMeshLayersCooked>> {
        self.presentation.clone()
    }

    /// Bumps each time [`Self::with_presentation`] replaces the chunks.
    #[must_use]
    pub fn presentation_generation(&self) -> u64 {
        self.presentation_generation
    }

    /// Where chunk LOD meshes are read without an asset server.
    #[must_use]
    pub fn chunk_dir(&self) -> Option<&std::path::Path> {
        self.chunk_dir.as_deref()
    }

    /// The canonical geometry, when a cooked file was loaded.
    #[must_use]
    pub fn geom(&self) -> Option<&WorldGeom> {
        self.geom.as_deref()
    }

    /// Derived mouths.
    #[must_use]
    pub fn mouths(&self) -> &[LayerMouth] {
        &self.mouths
    }

    /// Bumps when a mouth opens, closes, or a dirty rect is rebuilt.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Adds a mouth and bumps the generation.
    pub fn push_mouth(&mut self, mouth: LayerMouth) {
        self.mouths.push(mouth);
        self.generation = self.generation.saturating_add(1);
    }

    /// Hole ids a layer fills (open or closed).
    #[must_use]
    pub fn filled_holes(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.mouths.iter().map(|m| m.hole.clone()).collect();
        ids.sort();
        ids.dedup();
        ids
    }

    /// Opens or closes `hole`. Returns whether a mouth had that id.
    pub fn set_open(&mut self, hole: &str, open: bool) -> bool {
        let mut found = false;
        for mouth in &mut self.mouths {
            if mouth.hole == hole {
                mouth.open = open;
                found = true;
            }
        }
        if found {
            self.generation = self.generation.saturating_add(1);
        }
        found
    }

    /// Drops the mouth `hole`.
    pub fn remove_mouth(&mut self, hole: &str) -> bool {
        let before = self.mouths.len();
        self.mouths.retain(|m| m.hole != hole);
        let removed = self.mouths.len() != before;
        if removed {
            self.generation = self.generation.saturating_add(1);
        }
        removed
    }

    /// The mouth `hole`.
    #[must_use]
    pub fn mouth(&self, hole: &str) -> Option<&LayerMouth> {
        self.mouths.iter().find(|m| m.hole == hole)
    }

    /// Rewrites LOD-0 Y of ring vertices and collar corners inside `dirty`.
    ///
    /// XZ stays bit for bit (an edit never moves a mouth-ring lattice vertex). The drawn ring
    /// for a later seam report comes from [`attach_ring`], which reads the heightfield.
    pub fn note_heights_changed(&mut self, map: &TerrainMap, dirty: &DirtyRect) {
        let inside = |x: f32, z: f32| -> bool {
            let [c, r] = map.world_to_grid(x, z);
            let col = c.round() as i64;
            let row = r.round() as i64;
            col >= i64::from(dirty.col_start)
                && col < i64::from(dirty.col_end)
                && row >= i64::from(dirty.row_start)
                && row < i64::from(dirty.row_end)
        };
        for mouth in &mut self.mouths {
            for v in &mut mouth.band.vertices {
                if v.ring && inside(v.lod0[0], v.lod0[2]) {
                    let y = map.sample_bilinear(v.lod0[0], v.lod0[2]);
                    v.lod0[1] = y;
                    v.position[1] = y;
                }
            }
            for tri in &mut mouth.collar {
                for p in tri.iter_mut() {
                    if inside(p[0], p[2]) {
                        let [c, r] = map.world_to_grid(p[0], p[2]);
                        p[1] = map.height_at_sample(c.round() as i64, r.round() as i64);
                    }
                }
            }
        }
        self.generation = self.generation.saturating_add(1);
    }
}

/// Seam of `selection` over the open mouths. Builds the height mips from `terrain`.
#[must_use]
pub fn seam_report(
    terrain: &Terrain,
    layers: &TerrainMeshLayers,
    selection: &crate::cdlod::LodSelection,
    eye: [f32; 2],
) -> SeamMouthReport {
    let mips = HeightMips::build(&terrain.map);
    seam_report_with_mips(terrain, &mips, layers, selection, eye)
}
