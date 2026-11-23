// SPDX-License-Identifier: MPL-2.0
//! The canonical half of the mesh layers cook (spec 121 § 9.2): a `<map>.meshlayers.ron` document,
//! the map's heightfield and the files the document names, to a cooked `<map>.meshlayers`.
//!
//! What this lane (W-06) cooks, and what it refuses until its lane lands (`cook_not_built`, never a
//! silent skip):
//!
//! | Input | Cooked here | Later lane |
//! |---|---|---|
//! | settings, ground mode `Full`, lattice constants, the full-pitch ground counts | yes | |
//! | `Passages` sources: the pinned Stage A mesher, then the weld and trim steps (E6), then `i32` mm tiles | yes | |
//! | ground mode `Nested` (per-tile levels by error, the seam rule, hanging vertices; W-10) | yes | |
//! | the memory budget (`ground_over_budget`, a [`CookError::Checks`] finding naming the `Nested` mode that fits; W-10, ADR-8436) | yes | |
//! | mouths a layer `fills`: the lattice cut, ring pins, cut bitmaps and the collar (W-10) | yes | |
//! | a filled mouth's seal: collars that hold a ring probe, a 1 mm wedge where an f32 step leaves the collar, and a fan over a hole whose centroid falls through (ADR-8334) | yes | |
//! | a ramp top off the ground: an approach strip from the ring to the top edge, which holds its ring edges in place of the collar (ADR-8396) | yes | |
//! | the prop layer: `*_skylight` terrain patches standing in a layer span, as pilot-only floors (W-30, ADR-8323) | yes | |
//! | a skylight climb that K8 opens past its mouth's cut (its lips run down the trench, past the ring): the same strip, flat at the pins (ADR-8533) | yes | |
//! | sections, portals, zones and blocker plugs of every `Passages` layer ([`crate::sections`]), skylight shafts cooked as open cuts (K8) | yes (W-08) | the blocker flip: W-13 |
//! | `<map>.viscells.ron`: one cell per section plus outdoor cell 0, portal quads at mouths and cuts, the wall-gap leak check ([`crate::viscells`]) | yes (W-21) | |
//! | `Gltf`, `Collision` sources | refused | W-16 |
//! | `Volume` sources: the caller's meshed, ground-clipped triangles ([`cook_with_layers`], ADR-10071); without them refused | yes | |
//! | the `Passages` render mesh (presentation section, [`crate::render`]): every canonical face subdivided, displacement 0, the mouth cut quads | yes (W-07) | |
//! | displaced render detail (`look.detail`): a sum of absolute displacements above 0.05 m is `render_off_collision`; a smaller non-zero displacement is refused | `render_off_collision` / refused | W-16 |
//!
//! **Load-time conditioning is a cook step (E6).** T3 conditions DO27's tunnel mesh when it loads
//! it; here the cook does it once and the run time never conditions:
//! - **weld**: a floor vertex up to [`WELD_MAX_MM`] over the ground goes onto it (every triangle
//!   sharing that vertex moves with it, so the mesh stays closed);
//! - **trim**: a connected floor strip narrower than [`TRIM_WIDTH_M`] (width = 2 · area / boundary
//!   length) whose every triangle centroid lies within [`TRIM_NEAR_M`] of another floor (another
//!   strip's floor or the ground) carries nothing: its triangles keep their shape and get
//!   `TRI_FLAG_NO_SUPPORT`. In a filled mouth's seal, the threshold sill where a climb K8 opened
//!   past the mouth's cut leaves the rock gets the same flag by identity, since a bend can twist
//!   the floor under it out of the trim test (ADR-8533).
//!
//! The cook arithmetic (the ground plane height, the strip measure) is `f64` and cook-only; its
//! results are integer millimetres, which is what the run time reads.

use std::collections::{BTreeMap, BTreeSet};

use crate::checks::{self, DISPLACE_LIMIT_M, TerrainBakeReport};
use crate::compat::layout::SampledLayout;
use crate::compat::mesh::{TunnelFaceKind, TunnelTriangle};
use crate::compat::sampler::TerrainSampler;
use crate::cooked::{
    CANONICAL_VERSION, COOKED_FORMAT_VERSION, CookedMeshLayers, CookedTile, CookedTriangle, LatticeConstants,
    MeshLayersHeader, TRI_FLAG_NO_SUPPORT, TRI_FLAG_PILOT_ONLY, TRI_FLAG_PORTAL, TriKind, WorldGeomCooked,
    content_hash_of_bytes, encode_canonical,
};
use crate::cut::{CutError, LatticeCut, LatticeVertex, MouthFootprint};
use crate::meshlayers::{GroundMode, LayerSource, MeshLayersDocument, MeshLayersError};
use crate::passages::{PassagesDocument, PassagesError};
use crate::pitch::{PitchInputs, PitchSeamError, PitchSeamReport, RectMm};
use crate::render::{PresentationCooked, RenderLayerInputs, render_layer};
use crate::sections::{IdBase, SectionInputs, SectionsError, layer_sections, open_skylight_shafts};
use crate::units::{MAX_EXTENT_MM, m_to_mm, mm_to_m, split_tile};

/// A floor whose every vertex stands at least this far over a ring pin it covers is a passage
/// floor, not a lip, and the seal keeps it (ADR-8396), millimetres. The ground probe reaches
/// 0.9 m over the pin, so support never picks it over the collar.
pub const CLEAR_OF_RING_MM: i64 = 1000;

/// Weld: the most a floor vertex may sit over the ground and still go onto it, millimetres (E6).
pub const WELD_MAX_MM: i32 = 150;
/// Trim: a floor strip narrower than this carries nothing, metres (E6).
pub const TRIM_WIDTH_M: f64 = 2.0;
/// Trim: how close the strip must lie to another floor, metres (E6).
pub const TRIM_NEAR_M: f64 = 0.5;

/// The map's heightfield as the sim ground lattice reads it (spec 121 § 3).
#[derive(Clone, Debug, PartialEq)]
pub struct GroundInput {
    /// Samples per side (`n`).
    pub lattice_n: u32,
    /// Map width, metres.
    pub width_m: f32,
    /// Height of count 0, metres.
    pub offset_m: f32,
    /// Height of one count, metres.
    pub step_m: f32,
    /// `n * n` counts, row major (`j * n + i`, `j` along +z).
    pub counts: Vec<u16>,
}

/// What the cook reads.
pub struct CookInputs<'a> {
    /// The `<map>.meshlayers.ron` bytes.
    pub document: &'a [u8],
    /// The heightfield.
    pub ground: &'a GroundInput,
    /// The Stage A terrain the `Passages` mesher samples (DO27's sampler over the same heightfield);
    /// `None` meshes on flat ground, as the Stage A mesher does.
    pub terrain: Option<&'a TerrainSampler>,
    /// Reads a file the document names (path as written in the document).
    pub read_file: &'a dyn Fn(&str) -> Result<Vec<u8>, String>,
}

/// What one layer's cook did (for `terrain.bake_report`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LayerCookReport {
    /// The layer id.
    pub id: String,
    /// Canonical triangles written.
    pub triangles: u32,
    /// Distinct vertex positions the weld step moved onto the ground.
    pub welded_vertices: u32,
    /// Floor triangles the trim step took out of support (with, in a filled mouth's seal, the
    /// threshold sill of a skylight climb K8 opened past the mouth's cut, ADR-8533).
    pub trimmed_triangles: u32,
    /// Triangles with zero area after the millimetre quantization (kept, so indices stay the source
    /// order).
    pub degenerate_triangles: u32,
    /// Render triangles written to the presentation section (0 for a layer without a render mesh).
    pub render_triangles: u32,
    /// The render triangles of each passage, in layout order (the cut sections are in [`CookReport::sections`]).
    pub render_sections: Vec<RenderSectionReport>,
    /// Segments of skylight ramps the cook opened to the sky (K8: a skylight shaft is an open cut; lane W-08).
    pub skylight_segments_opened: u32,
    /// Sections cut from this layer.
    pub sections: u32,
    /// Blocker plug triangles appended to this layer (after its passage triangles).
    pub plug_triangles: u32,
}

/// One cut section in the cook report (`terrain.bake_report` reads it; lane W-08).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct SectionReport {
    /// The section id.
    pub id: u16,
    /// `<layer>/<passage>`.
    pub source: String,
    /// Covered (a lid over every floor) or an open cut.
    pub covered: bool,
    /// Its canonical triangles (plug triangles not counted).
    pub triangles: u32,
    /// Its portals.
    pub portals: Vec<u16>,
    /// Its blocker plug, if it is collapsible.
    pub blocker: Option<u16>,
    /// Its zone's reverb preset (`tube`, `gallery` or `open`).
    pub reverb: String,
}

/// One portal in the cook report.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct PortalReport {
    /// The portal id.
    pub id: u16,
    /// The two cells it joins (`None` = outdoors).
    pub sides: [Option<u16>; 2],
    /// The opening whose mouth it rings.
    pub mouth: Option<String>,
    /// Its nav node, world millimetres.
    pub nav_mm: [i32; 3],
    /// The blocker that closes it.
    pub blocker: Option<u16>,
}

/// One visibility cell the cook emitted (`terrain.bake_report` lists it; lane W-21).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct VisCellReport {
    /// Cell id. 0 is the outdoors.
    pub id: u16,
    /// The section source, or `outdoor`.
    pub name: String,
    /// Set on cell 0 only.
    pub outdoor: bool,
}

/// One visibility portal quad the cook emitted.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct VisPortalReport {
    /// One side's cell id.
    pub a: u16,
    /// The other side's cell id.
    pub b: u16,
    /// The cooked portal this quad was cut from.
    pub cooked: u16,
}

/// One passage's share of a layer's render mesh.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RenderSectionReport {
    /// The passage id.
    pub passage: String,
    /// Its render triangles.
    pub triangles: u32,
}

/// The cook's report.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CookReport {
    /// One entry per layer, in document order.
    pub layers: Vec<LayerCookReport>,
    /// Every cut section, in id order.
    pub sections: Vec<SectionReport>,
    /// Every portal, in id order.
    pub portals: Vec<PortalReport>,
    /// Visibility cells written to `<map>.viscells.ron` (outdoor cell 0, then one per section).
    pub vis_cells: Vec<VisCellReport>,
    /// Visibility portal quads written to `<map>.viscells.ron`.
    pub vis_portals: Vec<VisPortalReport>,
    /// The ground (spec 121 § 3): mode, levels, mouths, memory.
    pub ground: GroundReport,
}

/// One cut mouth: a named hole, its ring loops and its cut, for the render mask (spec 26 § 2a.4
/// item 2) and `terrain.bake_report`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CookedMouth {
    /// The named hole id.
    pub id: String,
    /// The layer that fills it (index in the document).
    pub layer: u16,
    /// The ring loops, lattice vertices `(i, j)` in order (closed; the first is not repeated).
    pub rings: Vec<Vec<LatticeVertex>>,
    /// Lattice triangles cut.
    pub cut_triangles: u32,
    /// Collar triangles written (canonical floor triangles on the ring).
    pub collar_triangles: u32,
}

/// What the cook did to the ground (spec 121 § 3; `regia terrain passages import --json`,
/// `terrain.bake_report`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GroundReport {
    /// The authored ground mode.
    pub mode: GroundMode,
    /// The level of every ground tile, ascending tile index.
    pub tile_levels: Vec<u8>,
    /// Ground tiles per side.
    pub tiles_per_side: u32,
    /// Hanging-vertex overrides written (seam rule 4).
    pub hanging_vertices: u32,
    /// Ring pins written.
    pub ring_pins: u32,
    /// The mouths cut, in layer then hole order.
    pub mouths: Vec<CookedMouth>,
    /// The ring loops of all mouths together (the cut's boundary; overlapping mouths share one).
    pub rings: Vec<Vec<LatticeVertex>>,
    /// The sim-geometry bytes of the canonical section (spec 121 § 11): `pitch::sim_ground_bytes`
    /// plus, with `stage_b`, the run-time query structures a load builds (the query index with its
    /// segment tables, the height pyramid, the ground axis table; ADR-10394).
    pub sim_bytes: u64,
    /// The budget those bytes are held to.
    pub budget_bytes: u64,
    /// The `ground_pitch_seam` check's measures (`Nested` only).
    pub pitch_seam: Option<PitchSeamReport>,
}

/// The cook's result.
#[derive(Clone, Debug, PartialEq)]
pub struct CookOutput {
    /// The cooked file.
    pub file: CookedMeshLayers,
    /// What each step did.
    pub report: CookReport,
    /// `<map>.viscells.ron` (version 1) emitted from the sections. Empty only when the emit did not run.
    pub viscells_ron: String,
    /// Every spec 121 § 10 check (lane W-28 reads this as `terrain.bake_report`).
    pub bake_report: TerrainBakeReport,
}

/// Why the cook failed (agent codes in parentheses).
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum CookError {
    /// The document (`meshlayers_newer_version`, `meshlayers_parse`, `meshlayers_invalid`).
    #[error(transparent)]
    Document(#[from] MeshLayersError),
    /// (`passages_*`) a layer's passages file.
    #[error("layer `{layer}`: {error}")]
    Passages {
        /// The layer id.
        layer: String,
        /// The error.
        error: PassagesError,
    },
    /// (`input_missing`) a file the document names cannot be read.
    #[error("input `{path}` cannot be read: {reason}")]
    InputMissing {
        /// The path as written.
        path: String,
        /// Why.
        reason: String,
    },
    /// (`ground_invalid`) the heightfield does not form a lattice.
    #[error("ground: {0}")]
    Ground(String),
    /// (`geom_extent_too_large`) a coordinate beyond 2^24 mm of the origin.
    #[error("a coordinate is beyond 2^24 mm of the origin")]
    ExtentTooLarge,
    /// (`passages_drift`) an imported sampled block differs from the layout sampled on this terrain.
    #[error(
        "layer `{layer}`: the stored sampled block differs from the layout sampled on this terrain; re-import or drop it"
    )]
    PassagesDrift {
        /// The layer id.
        layer: String,
    },
    /// (`mouth_not_simple`, `mouth_off_map`) the lattice cut of a mouth.
    #[error("{0}")]
    Cut(#[from] CutError),
    /// (`mouth_unknown`) a layer fills a named hole its source has no mouth for.
    #[error("layer `{layer}` fills `{hole}`, which its source has no mouth for")]
    MouthUnknown {
        /// The layer id.
        layer: String,
        /// The hole id.
        hole: String,
    },
    /// (`ground_pitch_seam`) the nested ground breaks a seam rule (spec 121 § 3, E1).
    #[error("ground_pitch_seam: {0}")]
    PitchSeam(#[from] PitchSeamError),
    /// (`cook_not_built`) a feature whose cook lands in a later lane.
    #[error("{what} is not cooked yet (lane {lane})")]
    NotBuilt {
        /// The feature.
        what: String,
        /// The lane that builds it.
        lane: &'static str,
    },
    /// (`viscell_leak`) a section wall has a gap of at least 0.5 m, or two sections still meet
    /// with every portal closed. The message names the section.
    #[error("viscell_leak: {detail}")]
    VisCellLeak {
        /// The section and the measured gap, or the section reached through a closed portal.
        detail: String,
    },
    /// (`viscell_bad_portal`) a cooked portal is not one or more convex quads.
    #[error("viscell_bad_portal: section `{section}` portal {portal} is not a convex quad")]
    VisCellPortal {
        /// The section the portal bounds.
        section: String,
        /// The cooked portal id.
        portal: u16,
    },
    /// (`viscell_bad_box`) a section zone cannot be an indoor cell box.
    #[error("viscell_bad_box: section `{section}`")]
    VisCellBox {
        /// The section source.
        section: String,
    },
    /// (`viscell_too_many`) more cells or portal quads than `<map>.viscells.ron` version 1 allows.
    #[error("viscell_too_many: {what}")]
    VisCellTooMany {
        /// `cells` or `portals`.
        what: &'static str,
    },
    /// A spec 121 § 10 check failed. [`CookError::code`] is the first unwaived failure.
    ///
    /// This includes `ground_over_budget` (spec 121 § 3 and § 11; ADR-8436): the authored ground
    /// mode does not fit the budget, and the cook refuses before the canonical section is checked or
    /// written. Its finding carries the advice: [`checks::CheckFinding::fits`] names the smallest
    /// `Nested { max_level }` that fits a `Full` map, [`checks::CheckFinding::tiles`] lists the tiles a
    /// `Nested` map holds fine. The cook never switches the mode or picks a coarser level by itself.
    #[error("{0}")]
    Checks(TerrainBakeReport),
}

impl CookError {
    /// The agent error code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Document(e) => e.code(),
            Self::Passages { error, .. } => error.code(),
            Self::InputMissing { .. } => "input_missing",
            Self::Ground(_) => "ground_invalid",
            Self::ExtentTooLarge => "geom_extent_too_large",
            Self::PassagesDrift { .. } => "passages_drift",
            Self::Cut(e) => e.code(),
            Self::MouthUnknown { .. } => "mouth_unknown",
            Self::PitchSeam(_) => "ground_pitch_seam",
            Self::NotBuilt { .. } => "cook_not_built",
            Self::VisCellLeak { .. } => "viscell_leak",
            Self::VisCellPortal { .. } => "viscell_bad_portal",
            Self::VisCellBox { .. } => "viscell_bad_box",
            Self::VisCellTooMany { .. } => "viscell_too_many",
            Self::Checks(report) => report.failure_code(),
        }
    }
}

/// The cook's leak error for one section shell. [`cook`] returns the same error when a cooked
/// section has this gap.
///
/// # Errors
///
/// [`CookError::VisCellLeak`] naming `section.source` when the wall gap is at least 0.5 m.
pub fn reject_viscell_gap(section: &crate::viscells::GapSection) -> Result<(), CookError> {
    match crate::viscells::wall_gap_of(section) {
        Some(gap) => Err(CookError::VisCellLeak { detail: gap.detail }),
        None => Ok(()),
    }
}

fn viscells_fault(fault: crate::viscells::VisCellsFault) -> CookError {
    match fault {
        crate::viscells::VisCellsFault::Leak { detail } => CookError::VisCellLeak { detail },
        crate::viscells::VisCellsFault::BadPortal { section, portal } => {
            CookError::VisCellPortal { section, portal }
        }
        crate::viscells::VisCellsFault::BadBox { section } => CookError::VisCellBox { section },
        crate::viscells::VisCellsFault::TooMany { what } => CookError::VisCellTooMany { what },
    }
}

impl GroundInput {
    /// The lattice constants this heightfield cooks to, with `ground_tile_cells`.
    ///
    /// # Errors
    ///
    /// [`CookError::Ground`] for fewer than 2 samples, a count list of the wrong length, a
    /// non-finite or non-positive width, or a non-finite offset or step;
    /// [`CookError::ExtentTooLarge`] for a map over 2^24 mm from the origin.
    pub fn lattice(&self, ground_tile_cells: u16) -> Result<LatticeConstants, CookError> {
        let n = self.lattice_n;
        if n < 2 {
            return Err(CookError::Ground(format!("lattice_n {n} is below 2")));
        }
        if self.counts.len() as u64 != u64::from(n) * u64::from(n) {
            return Err(CookError::Ground(format!(
                "{} counts for a {n} x {n} lattice",
                self.counts.len()
            )));
        }
        if !self.width_m.is_finite() || self.width_m <= 0.0 {
            return Err(CookError::Ground("width_m must be finite and above 0".into()));
        }
        if !self.offset_m.is_finite() || !self.step_m.is_finite() {
            return Err(CookError::Ground("offset_m and step_m must be finite".into()));
        }
        let width_mm = m_to_mm(self.width_m);
        if i64::from(width_mm) / 2 + 1 >= MAX_EXTENT_MM {
            return Err(CookError::ExtentTooLarge);
        }
        let plus = |v: f32| if v == 0.0 { 0.0f32 } else { v };
        Ok(LatticeConstants {
            lattice_n: n,
            width_mm,
            ground_tile_cells,
            offset_m_bits: plus(self.offset_m).to_bits(),
            step_m_bits: plus(self.step_m).to_bits(),
        })
    }

    /// The vertex height, metres, by the spec 121 § 3 formula (`offset + count * step`, `f32`).
    fn height_m(&self, i: u32, j: u32) -> f32 {
        let c = self.counts[(j as usize) * (self.lattice_n as usize) + i as usize];
        self.offset_m + (c as f32) * self.step_m
    }
}

/// The cook-time ground height at world `(x_mm, z_mm)`: the lattice triangulation with the fixed
/// `(x0, z1)-(x1, z0)` diagonal (spec 121 § 3), in `f64`, in millimetres.
pub(crate) fn ground_mm_at(ground: &GroundInput, lattice: &LatticeConstants, x_mm: f64, z_mm: f64) -> f64 {
    let last = lattice.cells();
    let find = |v: f64| -> u32 {
        let x0 = lattice.vertex_mm(0) as f64;
        let x1 = lattice.vertex_mm(last) as f64;
        let est = ((v - x0) / (x1 - x0) * f64::from(last)).floor();
        let mut i = regia_detsim::float::det_clamp_f64(est, 0.0, f64::from(last.saturating_sub(1))) as u32;
        while i > 0 && v < lattice.vertex_mm(i) as f64 {
            i -= 1;
        }
        while i + 1 < last && v >= lattice.vertex_mm(i + 1) as f64 {
            i += 1;
        }
        i
    };
    let (i, j) = (find(x_mm), find(z_mm));
    let (x0, x1) = (lattice.vertex_mm(i) as f64, lattice.vertex_mm(i + 1) as f64);
    let (z0, z1) = (lattice.vertex_mm(j) as f64, lattice.vertex_mm(j + 1) as f64);
    let u = regia_detsim::float::det_clamp_f64((x_mm - x0) / (x1 - x0), 0.0, 1.0);
    let w = regia_detsim::float::det_clamp_f64((z_mm - z0) / (z1 - z0), 0.0, 1.0);
    let h = |a: u32, b: u32| f64::from(ground.height_m(a, b)) * 1000.0;
    let (h00, h10, h01, h11) = (h(i, j), h(i + 1, j), h(i, j + 1), h(i + 1, j + 1));
    if u + w <= 1.0 {
        h00 + u * (h10 - h00) + w * (h01 - h00)
    } else {
        h11 + (1.0 - u) * (h01 - h11) + (1.0 - w) * (h10 - h11)
    }
}

/// A canonical triangle before tiling: world millimetres.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorldTriangleMm {
    /// The vertices, world millimetres.
    pub v: [[i32; 3]; 3],
    /// Its kind.
    pub kind: TriKind,
    /// `TRI_FLAG_*` bits.
    pub flags: u8,
}

/// Quantizes a Stage A collision mesh to world millimetres with the pinned `m_to_mm`, in source
/// order (Floor → `Floor`, Ceiling → `Roof`, Wall → `Wall`; `portal` and `pilot_only` become flags).
///
/// # Errors
///
/// [`CookError::ExtentTooLarge`] for a vertex beyond 2^24 mm of the origin.
pub fn quantize_mesh(triangles: &[TunnelTriangle]) -> Result<Vec<WorldTriangleMm>, CookError> {
    triangles
        .iter()
        .map(|t| {
            let mut v = [[0i32; 3]; 3];
            for (out, p) in v.iter_mut().zip([t.a, t.b, t.c]) {
                for (o, c) in out.iter_mut().zip(p) {
                    // A non-finite coordinate maps to 0 or saturates; both are refused below or here.
                    if !c.is_finite() {
                        return Err(CookError::ExtentTooLarge);
                    }
                    *o = m_to_mm(c);
                    if i64::from(*o).abs() >= MAX_EXTENT_MM {
                        return Err(CookError::ExtentTooLarge);
                    }
                }
            }
            let kind = match t.kind {
                TunnelFaceKind::Floor => TriKind::Floor,
                TunnelFaceKind::Ceiling => TriKind::Roof,
                TunnelFaceKind::Wall => TriKind::Wall,
            };
            let flags = if t.portal { TRI_FLAG_PORTAL } else { 0 }
                | if t.pilot_only { TRI_FLAG_PILOT_ONLY } else { 0 };
            Ok(WorldTriangleMm { v, kind, flags })
        })
        .collect()
}

/// The weld step (E6): every distinct floor vertex 0 < `y - ground` <= [`WELD_MAX_MM`] goes onto
/// the ground, and every triangle vertex at that position moves with it. Returns the number of
/// distinct positions moved. `None` is a mesh-only input: nothing is welded (ADR-8246 R2). T3
/// skips the weld the same way when no terrain is supplied.
pub fn weld(tris: &mut [WorldTriangleMm], ground: Option<&GroundInput>, lattice: &LatticeConstants) -> u32 {
    let Some(ground) = ground else {
        return 0;
    };
    let mut moves: BTreeMap<[i32; 3], i32> = BTreeMap::new();
    for t in tris.iter().filter(|t| t.kind == TriKind::Floor) {
        for p in t.v {
            if moves.contains_key(&p) {
                continue;
            }
            let g = ground_mm_at(ground, lattice, f64::from(p[0]), f64::from(p[2])).round_ties_even() as i32;
            let over = i64::from(p[1]) - i64::from(g);
            if over > 0 && over <= i64::from(WELD_MAX_MM) {
                moves.insert(p, g);
            }
        }
    }
    for t in tris.iter_mut() {
        for p in t.v.iter_mut() {
            if let Some(&y) = moves.get(p) {
                p[1] = y;
            }
        }
    }
    moves.len() as u32
}

fn xz(p: [i32; 3]) -> (f64, f64) {
    (f64::from(p[0]), f64::from(p[2]))
}

/// The height of the triangle's plane at `(x, z)` if the point is inside its XZ projection
/// (inclusive), millimetres.
fn plane_height_at(t: &WorldTriangleMm, x: f64, z: f64) -> Option<f64> {
    let [a, b, c] = t.v;
    let (ax, az) = xz(a);
    let (bx, bz) = xz(b);
    let (cx, cz) = xz(c);
    let det = (bx - ax) * (cz - az) - (cx - ax) * (bz - az);
    if det == 0.0 {
        return None;
    }
    let l1 = ((x - ax) * (cz - az) - (cx - ax) * (z - az)) / det;
    let l2 = ((bx - ax) * (z - az) - (x - ax) * (bz - az)) / det;
    let l0 = 1.0 - l1 - l2;
    (l0 >= 0.0 && l1 >= 0.0 && l2 >= 0.0)
        .then(|| l0 * f64::from(a[1]) + l1 * f64::from(b[1]) + l2 * f64::from(c[1]))
}

/// The trim step (E6). Floor triangles are grouped into strips (connected through shared edges with
/// equal millimetre endpoints). A strip narrower than [`TRIM_WIDTH_M`] whose every triangle
/// centroid has another floor (a floor of another strip, or the ground when one is supplied)
/// within [`TRIM_NEAR_M`] vertically gets `TRI_FLAG_NO_SUPPORT` on all its triangles. Returns the
/// number of triangles flagged. `None` does not invent a ground plane (ADR-8246 R2).
pub fn trim(tris: &mut [WorldTriangleMm], ground: Option<&GroundInput>, lattice: &LatticeConstants) -> u32 {
    let floors: Vec<usize> = (0..tris.len())
        .filter(|&i| tris[i].kind == TriKind::Floor)
        .collect();
    // Union-find over shared edges.
    let mut parent: Vec<usize> = (0..tris.len()).collect();
    fn root(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    let edge = |p: [i32; 3], q: [i32; 3]| if p <= q { (p, q) } else { (q, p) };
    let mut edges: BTreeMap<([i32; 3], [i32; 3]), Vec<usize>> = BTreeMap::new();
    for &i in &floors {
        let [a, b, c] = tris[i].v;
        for (p, q) in [(a, b), (b, c), (c, a)] {
            edges.entry(edge(p, q)).or_default().push(i);
        }
    }
    for users in edges.values() {
        for w in users.windows(2) {
            let (ra, rb) = (root(&mut parent, w[0]), root(&mut parent, w[1]));
            if ra != rb {
                parent[ra.max(rb)] = ra.min(rb);
            }
        }
    }
    let mut strips: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for &i in &floors {
        let r = root(&mut parent, i);
        strips.entry(r).or_default().push(i);
    }
    let strip_of: BTreeMap<usize, usize> = strips
        .iter()
        .flat_map(|(&r, members)| members.iter().map(move |&m| (m, r)))
        .collect();
    let mut flagged = 0u32;
    let near_mm = TRIM_NEAR_M * 1000.0;
    for (&r, members) in &strips {
        let set: BTreeSet<usize> = members.iter().copied().collect();
        let mut area = 0.0f64;
        let mut boundary: BTreeMap<([i32; 3], [i32; 3]), u32> = BTreeMap::new();
        for &i in members {
            let [a, b, c] = tris[i].v;
            let (ax, az) = xz(a);
            let (bx, bz) = xz(b);
            let (cx, cz) = xz(c);
            area += ((bx - ax) * (cz - az) - (cx - ax) * (bz - az)).abs() * 0.5;
            for (p, q) in [(a, b), (b, c), (c, a)] {
                *boundary.entry(edge(p, q)).or_default() += 1;
            }
        }
        let perimeter: f64 = boundary
            .iter()
            .filter(|(_, n)| **n == 1)
            .map(|((p, q), _)| {
                let (dx, dz) = (f64::from(q[0] - p[0]), f64::from(q[2] - p[2]));
                regia_detsim::math::sqrt_f64(dx * dx + dz * dz)
            })
            .sum();
        if perimeter <= 0.0 {
            continue;
        }
        let width_m = 2.0 * area / perimeter / 1000.0;
        if width_m >= TRIM_WIDTH_M {
            continue;
        }
        let near_other = members.iter().all(|&i| {
            let [a, b, c] = tris[i].v;
            let cx = (f64::from(a[0]) + f64::from(b[0]) + f64::from(c[0])) / 3.0;
            let cy = (f64::from(a[1]) + f64::from(b[1]) + f64::from(c[1])) / 3.0;
            let cz = (f64::from(a[2]) + f64::from(b[2]) + f64::from(c[2])) / 3.0;
            if ground.is_some_and(|ground| (ground_mm_at(ground, lattice, cx, cz) - cy).abs() <= near_mm) {
                return true;
            }
            floors.iter().any(|&o| {
                !set.contains(&o)
                    && strip_of.get(&o) != Some(&r)
                    && plane_height_at(&tris[o], cx, cz).is_some_and(|h| (h - cy).abs() <= near_mm)
            })
        });
        if near_other {
            for &i in members {
                tris[i].flags |= TRI_FLAG_NO_SUPPORT;
                flagged += 1;
            }
        }
    }
    flagged
}

/// Cut cells an f32 step off the ring lands in, which the ring-edge collar does not own.
///
/// `extra` floors (the zipper bands, written after the collar) count as authored floors. A collar
/// cell whose every ring edge is in `held` (an edge of a band or approach triangle) is not a
/// collar: it is not kept, and a probe in it gets a wedge when no floor holds it (ADR-8396).
fn ulp_wedges(
    lattice: &LatticeConstants,
    counts: &[u16],
    cut: &crate::cut::LatticeCut,
    layers: &[Vec<WorldTriangleMm>],
    extra: &[WorldTriangleMm],
    held: &BTreeSet<([i32; 3], [i32; 3])>,
) -> (Vec<crate::cut::CollarTriangle>, BTreeSet<(u32, u32, u8)>) {
    use crate::cut::{count_height_m, neighbor, tri_vertices};
    use crate::ground::vertex_m;
    let cells = lattice.cells();
    let n = lattice.lattice_n;
    let pin = |i: u32, j: u32| -> i32 {
        let vertex = j * n + i;
        match cut.ring_pins.binary_search_by_key(&vertex, |p| p.vertex) {
            Ok(k) => cut.ring_pins[k].height_mm,
            Err(_) => m_to_mm(count_height_m(lattice, counts, i, j)),
        }
    };
    // Authored floors that still carry support. A sample already on one of these, within 1 mm
    // of the pin, does not need a wedge.
    let floors: Vec<(&WorldTriangleMm, [[f32; 2]; 3])> = layers
        .iter()
        .flatten()
        .chain(extra)
        .filter(|t| t.kind == TriKind::Floor && t.flags & TRI_FLAG_NO_SUPPORT == 0)
        .map(|t| {
            let xz = t.v.map(|p| [mm_to_m(0, p[0]), mm_to_m(0, p[2])]);
            (t, xz)
        })
        .collect();
    let mut boundary: BTreeSet<(u32, u32, u8)> = BTreeSet::new();
    let mut boundary_xz: Vec<[[f32; 2]; 3]> = Vec::new();
    let mut boundary_keys: Vec<(u32, u32, u8)> = Vec::new();
    let mm_of = |(vi, vj): (u32, u32)| -> [i32; 3] {
        [
            lattice.vertex_mm(vi) as i32,
            pin(vi, vj),
            lattice.vertex_mm(vj) as i32,
        ]
    };
    for &(j, i, t) in &cut.cut {
        let on_ring: Vec<u8> = (0..3u8)
            .filter(|&e| {
                neighbor(cells, i, j, t, e).is_none_or(|(ni, nj, nt)| !cut.cut.contains(&(nj, ni, nt)))
            })
            .collect();
        let v = tri_vertices(i, j, t);
        let all_held = on_ring.iter().all(|&e| {
            let (a, b) = (mm_of(v[usize::from(e)]), mm_of(v[(usize::from(e) + 1) % 3]));
            held.contains(&if a <= b { (a, b) } else { (b, a) })
        });
        if !on_ring.is_empty() && !all_held {
            boundary.insert((j, i, t));
            boundary_keys.push((j, i, t));
            boundary_xz
                .push(tri_vertices(i, j, t).map(|(vi, vj)| [vertex_m(lattice, vi), vertex_m(lattice, vj)]));
        }
    }
    // A collar triangle stays when a ring probe it contains is not already on an authored
    // floor within 1 mm of the pin, or when its own centroid is not. The ring edge can sit on
    // an authored floor while the cell's interior still falls through to the tunnel.
    let mut keep: BTreeSet<(u32, u32, u8)> = BTreeSet::new();
    for (n, &(j, i, t)) in boundary_keys.iter().enumerate() {
        let v = tri_vertices(i, j, t).map(|(vi, vj)| {
            [
                lattice.vertex_mm(vi) as i32,
                pin(vi, vj),
                lattice.vertex_mm(vj) as i32,
            ]
        });
        let x = (mm_to_m(0, v[0][0]) + mm_to_m(0, v[1][0]) + mm_to_m(0, v[2][0])) / 3.0;
        let z = (mm_to_m(0, v[0][2]) + mm_to_m(0, v[1][2]) + mm_to_m(0, v[2][2])) / 3.0;
        let covered =
            f32_plane_m(v, x, z).is_some_and(|h| sample_on_floor(&floors, x, z, f64::from(h) * 1000.0));
        if !covered {
            keep.insert(boundary_keys[n]);
        }
    }

    // One 1 mm triangle per ring sample that has a gap, not one lattice cell per f32 step.
    // A cell is ~0.31 m and the four ulps around one sample fall in different cells; the
    // resident budget has room for tens of triangles, not hundreds.
    let mut extra: BTreeMap<(i32, i32), (i32, usize)> = BTreeMap::new();
    for ring in &cut.rings {
        if ring.len() < 2 {
            continue;
        }
        for k in 0..ring.len() {
            let (i0, j0) = ring[k];
            let (i1, j1) = ring[(k + 1) % ring.len()];
            let ax = vertex_m(lattice, i0);
            let az = vertex_m(lattice, j0);
            let bx = vertex_m(lattice, i1);
            let bz = vertex_m(lattice, j1);
            let y0 = f64::from(pin(i0, j0));
            let y1 = f64::from(pin(i1, j1));
            for step in [0.0f32, 0.25, 0.5, 0.75] {
                let x = ax + (bx - ax) * step;
                let z = az + (bz - az) * step;
                let pin_y = y0 + (y1 - y0) * f64::from(step);
                for (px, pz) in [
                    (x, z),
                    (x.next_down(), z),
                    (x.next_up(), z),
                    (x, z.next_down()),
                    (x, z.next_up()),
                ] {
                    let covered = sample_on_floor(&floors, px, pz, pin_y);
                    if covered {
                        continue;
                    }
                    for (i, tri) in boundary_xz.iter().enumerate() {
                        if xz_in_closed_tri(*tri, px, pz) {
                            keep.insert(boundary_keys[i]);
                        }
                    }
                }
                let geom = WedgeGeom {
                    boundary: &boundary,
                    boundary_xz: &boundary_xz,
                    lattice,
                    cut,
                };
                let gap = [
                    (x, z),
                    (x.next_down(), z),
                    (x.next_up(), z),
                    (x, z.next_down()),
                    (x, z.next_up()),
                ]
                .into_iter()
                .any(|(px, pz)| probe_needs_wedge(px, pz, pin_y, &floors, &geom));
                if !gap {
                    continue;
                }
                let mouth = cut_triangle_at(lattice, cut, x, z)
                    .and_then(|(j, i, t)| cut.mouths.iter().position(|m| m.cut.contains(&(j, i, t))))
                    .unwrap_or(0);
                extra
                    .entry((m_to_mm(x), m_to_mm(z)))
                    .or_insert((pin_y.round_ties_even() as i32, mouth));
            }
        }
    }
    let wedges = extra
        .into_iter()
        .map(|((cx, cz), (y, mouth))| crate::cut::CollarTriangle {
            mouth,
            // 1 mm around the sample. An f32 ulp at these coordinates is under 0.01 mm, so the
            // sample and its four neighbors sit strictly inside.
            v: [[cx, y, cz - 1], [cx - 1, y, cz + 1], [cx + 1, y, cz + 1]],
            kind: TriKind::Floor,
        })
        .collect();
    (wedges, keep)
}

/// An authored floor contains `(x, z)` and its plane there is within 1 mm of `pin_y`.
///
/// Height uses the same f32 plane as the query. The cook's f64 barycentric test misses points
/// the query still accepts, and each miss becomes a wedge the budget cannot hold.
fn sample_on_floor(floors: &[(&WorldTriangleMm, [[f32; 2]; 3])], x: f32, z: f32, pin_y: f64) -> bool {
    floors.iter().any(|(tri, xz)| {
        if !xz_in_closed_tri(*xz, x, z) {
            return false;
        }
        let Some(h) = f32_plane_m(tri.v, x, z) else {
            return false;
        };
        (f64::from(h) * 1000.0 - pin_y).abs() <= 1.0
    })
}

fn f32_plane_m(v: [[i32; 3]; 3], x: f32, z: f32) -> Option<f32> {
    let p = v.map(|q| [mm_to_m(0, q[0]), mm_to_m(0, q[1]), mm_to_m(0, q[2])]);
    let (ax, ay, az) = (p[0][0], p[0][1], p[0][2]);
    let (ux, uy, uz) = (p[1][0] - ax, p[1][1] - ay, p[1][2] - az);
    let (vx, vy, vz) = (p[2][0] - ax, p[2][1] - ay, p[2][2] - az);
    let ny = uz * vx - ux * vz;
    if ny.abs() <= 1.0e-8 {
        return None;
    }
    let nx = uy * vz - uz * vy;
    let nz = ux * vy - uy * vx;
    Some(ay - (nx * (x - ax) + nz * (z - az)) / ny)
}

/// The collar and the lattice a wedge probe reads. Bundled so the probe stays under the
/// argument limit.
struct WedgeGeom<'a> {
    boundary: &'a BTreeSet<(u32, u32, u8)>,
    boundary_xz: &'a [[[f32; 2]; 3]],
    lattice: &'a LatticeConstants,
    cut: &'a crate::cut::LatticeCut,
}

/// `true` when `(x, z)` is in the cut, off the collar, and not already on an authored floor
/// within 1 mm of `pin_y`.
fn probe_needs_wedge(
    x: f32,
    z: f32,
    pin_y: f64,
    floors: &[(&WorldTriangleMm, [[f32; 2]; 3])],
    geom: &WedgeGeom<'_>,
) -> bool {
    if geom.boundary_xz.iter().any(|tri| xz_in_closed_tri(*tri, x, z)) {
        return false;
    }
    let on_floor = sample_on_floor(floors, x, z, pin_y);
    if on_floor {
        return false;
    }
    match cut_triangle_at(geom.lattice, geom.cut, x, z) {
        Some(key) => !geom.boundary.contains(&key),
        None => false,
    }
}

/// Closed XZ containment, matching the query index: a boundary point counts, a point one
/// half-plane outside does not. `v` is `[x, z]` metres.
fn xz_in_closed_tri(v: [[f32; 2]; 3], x: f32, z: f32) -> bool {
    use regia_detsim::float::{det_max, det_min};
    let min_x = det_min(det_min(v[0][0], v[1][0]), v[2][0]);
    let max_x = det_max(det_max(v[0][0], v[1][0]), v[2][0]);
    let min_z = det_min(det_min(v[0][1], v[1][1]), v[2][1]);
    let max_z = det_max(det_max(v[0][1], v[1][1]), v[2][1]);
    if x < min_x || x > max_x || z < min_z || z > max_z {
        return false;
    }
    (0..3).all(|e| {
        let a = v[e];
        let b = v[(e + 1) % 3];
        let c = v[(e + 2) % 3];
        let (lo, hi) = if a[0] < b[0] || (a[0] == b[0] && a[1] < b[1]) {
            (a, b)
        } else {
            (b, a)
        };
        let (dx, dz) = (hi[0] - lo[0], hi[1] - lo[1]);
        let side = dx * (c[1] - lo[1]) - dz * (c[0] - lo[0]);
        if side == 0.0 {
            return false;
        }
        let edge = dx * (z - lo[1]) - dz * (x - lo[0]);
        if edge == 0.0 {
            let len2 = dx * dx + dz * dz;
            if len2 == 0.0 {
                return false;
            }
            let t = ((x - lo[0]) * dx + (z - lo[1]) * dz) / len2;
            return (0.0..=1.0).contains(&t);
        }
        if side < 0.0 { edge <= 0.0 } else { edge > 0.0 }
    })
}

/// The cut lattice triangle that owns `(x, z)`, if that triangle is cut.
fn cut_triangle_at(
    lattice: &LatticeConstants,
    cut: &crate::cut::LatticeCut,
    x: f32,
    z: f32,
) -> Option<(u32, u32, u8)> {
    use crate::ground::vertex_m;
    let i = locate_cell(lattice, x)?;
    let mut j = locate_cell(lattice, z)?;
    if j > 0 && z == vertex_m(lattice, j) {
        j -= 1;
    }
    let cells = lattice.cells();
    if i >= cells || j >= cells {
        return None;
    }
    let v01 = (vertex_m(lattice, i), vertex_m(lattice, j + 1));
    let v10 = (vertex_m(lattice, i + 1), vertex_m(lattice, j));
    let e = (v10.0 - v01.0) * (z - v01.1) - (v10.1 - v01.1) * (x - v01.0);
    let t = u8::from(e > 0.0);
    let key = (j, i, t);
    cut.cut.contains(&key).then_some(key)
}

/// Full-pitch cell index of `v`, matching [`crate::ground::GroundLattice`]'s estimate.
fn locate_cell(lattice: &LatticeConstants, v: f32) -> Option<u32> {
    use crate::ground::vertex_m;
    let cells = lattice.cells();
    if cells == 0 || v.is_nan() {
        return None;
    }
    let first = vertex_m(lattice, 0);
    let last = vertex_m(lattice, cells);
    if v < first || v > last {
        return None;
    }
    let pitch = (last - first) / cells as f32;
    let est = ((v - first) / pitch).floor();
    let mut i = if est <= 0.0 {
        0
    } else if est >= (cells - 1) as f32 {
        cells - 1
    } else {
        est as u32
    };
    while i > 0 && v < vertex_m(lattice, i) {
        i -= 1;
    }
    while i + 1 < cells && v >= vertex_m(lattice, i + 1) {
        i += 1;
    }
    Some(i)
}

/// A stitch triangle stays when its interior does not fight an authored floor.
///
/// Four interior samples are tested. An authored floor within 2 m of the stitch plane and more
/// than 2 cm off it is a second surface, and support would step between them. A floor farther
/// than 2 m, such as the tunnel under a mouth, is the hole this triangle covers.
fn stitch_fills_a_hole(tri: &WorldTriangleMm, floors: &[WorldTriangleMm]) -> bool {
    let v = tri.v.map(|p| [f64::from(p[0]), f64::from(p[2])]);
    let c = [
        (v[0][0] + v[1][0] + v[2][0]) / 3.0,
        (v[0][1] + v[1][1] + v[2][1]) / 3.0,
    ];
    let samples = [
        c,
        [(c[0] + v[0][0]) * 0.5, (c[1] + v[0][1]) * 0.5],
        [(c[0] + v[1][0]) * 0.5, (c[1] + v[1][1]) * 0.5],
        [(c[0] + v[2][0]) * 0.5, (c[1] + v[2][1]) * 0.5],
    ];
    !samples.iter().any(|&[x, z]| {
        let Some(h) = plane_height_at(tri, x, z) else {
            return false;
        };
        floors.iter().any(|floor| {
            if floor.kind != TriKind::Floor || floor.flags & TRI_FLAG_NO_SUPPORT != 0 {
                return false;
            }
            let Some(fh) = plane_height_at(floor, x, z) else {
                return false;
            };
            let d = (fh - h).abs();
            d > 20.0 && d < 2000.0
        })
    })
}

/// One canonical or plug triangle of a layer before the seal: its index in the layer then, and
/// its section (ADR-8421).
#[derive(Clone, Copy, Debug)]
struct TriMeta {
    index: u32,
    section: Option<u16>,
}

/// `Vec::retain` on a layer's triangles that keeps `meta` (which covers the layer's prefix of
/// canonical and plug triangles) in step: a dropped prefix triangle drops its entry (ADR-8421).
fn retain_with_meta(
    tris: &mut Vec<WorldTriangleMm>,
    meta: &mut Vec<TriMeta>,
    mut keep: impl FnMut(&WorldTriangleMm) -> bool,
) {
    let mut kept = Vec::with_capacity(meta.len());
    let mut i = 0;
    tris.retain(|t| {
        let k = keep(t);
        if k && let Some(m) = meta.get(i) {
            kept.push(*m);
        }
        i += 1;
        k
    });
    *meta = kept;
}

/// A pre-seal triangle range of one layer moved onto the surviving order: the survivors keep
/// their order, so the range stays contiguous (it may shrink or empty) (ADR-8421).
fn remap_range(r: &crate::cooked::TriRange, meta: &[TriMeta]) -> crate::cooked::TriRange {
    let start = meta.partition_point(|m| m.index < r.start) as u32;
    let end = meta.partition_point(|m| m.index < r.end) as u32;
    crate::cooked::TriRange {
        layer: r.layer,
        start,
        end,
    }
}

/// An authored floor that covers a ring edge and sits more than 1 mm above the pin there is not
/// the ground. Support would pick it over the collar, and keeping the triangle spends bytes the
/// seam wedges need. Drop it. The collar (and a wedge, where the collar does not reach) is the
/// floor on that edge.
///
/// The probe is the edge, not only its vertices: a floor can cover the middle of a ring edge
/// while both endpoints sit outside it. Triangles already flagged by trim stay.
fn silence_floors_above_ring(
    tris: &mut Vec<WorldTriangleMm>,
    meta: &mut Vec<TriMeta>,
    rings: &[Vec<[i32; 3]>],
) {
    let Some(lowest_pin) = rings.iter().flatten().map(|p| p[1]).min() else {
        return;
    };
    retain_with_meta(tris, meta, |tri| {
        if tri.kind != TriKind::Floor || tri.flags & TRI_FLAG_NO_SUPPORT != 0 {
            return true;
        }
        let max_y = tri.v[0][1].max(tri.v[1][1]).max(tri.v[2][1]);
        if max_y <= lowest_pin + 1 {
            return true;
        }
        !rings.iter().any(|ring| ring_above_floor(tri, ring))
    });
}

/// `true` when `ring` passes through `tri` more than 1 mm below the triangle's plane.
fn ring_above_floor(tri: &WorldTriangleMm, ring: &[[i32; 3]]) -> bool {
    if ring.len() < 2 {
        return false;
    }
    (0..ring.len()).any(|i| {
        let a = ring[i];
        let b = ring[(i + 1) % ring.len()];
        (0..=4).any(|t| {
            let p = [
                a[0] + (b[0] - a[0]) * t / 4,
                a[1] + (b[1] - a[1]) * t / 4,
                a[2] + (b[2] - a[2]) * t / 4,
            ];
            // A corner of this triangle is the pin itself, so its plane there is not above it.
            if tri.v.contains(&p) {
                return false;
            }
            // A passage floor that stands clear of the ground over the ring is not picked over
            // the collar from the ground, and it carries the passage where the ring passes under
            // it (ADR-8396). Lips and caps reach down to the ground and still go.
            if tri
                .v
                .iter()
                .all(|v| i64::from(v[1]) >= i64::from(p[1]) + CLEAR_OF_RING_MM)
            {
                return false;
            }
            plane_height_at(tri, f64::from(p[0]), f64::from(p[2])).is_some_and(|h| h > f64::from(p[1]) + 1.0)
        })
    })
}

/// The top edge of `hole`'s ramp floor as the layer's floor triangles hold it (after the weld).
/// `None` when the opening, its ramp or the floor corners are not found.
fn ramp_top(
    passages: &PassagesDocument,
    sampled: &crate::compat::layout::SampledLayout,
    hole: &str,
    tris: &[WorldTriangleMm],
) -> Option<crate::approach::RampTop> {
    let opening = passages.layout.openings.iter().find(|o| o.id == hole)?;
    let ramp = sampled.passages.iter().find(|p| p.id == opening.ramp)?;
    let n = ramp.samples.len();
    if n < 2 {
        return None;
    }
    let (b, a) = (&ramp.samples[n - 1], &ramp.samples[n - 2]);
    let half = ramp.width_m * 0.5;
    // The mesher's floor corner, `edge(b, sign, 0)`, quantized the same way.
    let corner = |sign: f32| -> [i32; 2] {
        [
            m_to_mm(b.p[0] + b.left[0] * half * sign),
            m_to_mm(b.p[2] + b.left[1] * half * sign),
        ]
    };
    let welded = |xz: [i32; 2]| -> Option<[i32; 3]> {
        tris.iter()
            .filter(|t| t.kind == TriKind::Floor)
            .flat_map(|t| t.v)
            .filter(|v| v[0] == xz[0] && v[2] == xz[1])
            .min_by_key(|v| (i64::from(v[1]) - i64::from(m_to_mm(b.p[1]))).abs())
    };
    Some(crate::approach::RampTop {
        left: welded(corner(1.0))?,
        right: welded(corner(-1.0))?,
        behind: [m_to_mm(a.p[0]), m_to_mm(a.p[1]), m_to_mm(a.p[2])],
    })
}

/// `true` when `hole`'s ramp ends in an open cut and the layout as meshed (skylight shafts opened,
/// K8) has more open-cut segments on it than `cut_from`, the layout before K8 (ADR-8533; the mouth
/// itself is cut from the meshed layout since ADR-8898): the climb K8 opened then needs the flat
/// approach strip at its top edge.
fn climb_opened_past_cut(
    passages: &PassagesDocument,
    cut_from: &SampledLayout,
    meshed: &SampledLayout,
    hole: &str,
) -> bool {
    let Some(opening) = passages.layout.openings.iter().find(|o| o.id == hole) else {
        return false;
    };
    let ramp = |layout: &SampledLayout| {
        layout
            .passages
            .iter()
            .find(|p| p.id == opening.ramp && p.kind == crate::compat::layout::PassageKind::Ramp)
            .map(|p| {
                (
                    p.covered.last() == Some(&false),
                    p.covered.iter().filter(|c| !**c).count(),
                )
            })
    };
    match (ramp(cut_from), ramp(meshed)) {
        (Some((_, open_cut)), Some((true, open_meshed))) => open_meshed > open_cut,
        _ => false,
    }
}

/// Pinned millimetre positions of one ring. `None` when a vertex has no pin or does not fit `i32`.
fn ring_mm(
    lattice: &LatticeConstants,
    cut: &crate::cut::LatticeCut,
    ring: &[crate::cut::LatticeVertex],
) -> Option<Vec<[i32; 3]>> {
    let mut out = Vec::with_capacity(ring.len());
    for &(i, j) in ring {
        let vertex = j * lattice.lattice_n + i;
        let y = match cut.ring_pins.binary_search_by_key(&vertex, |p| p.vertex) {
            Ok(k) => cut.ring_pins[k].height_mm,
            Err(_) => return None,
        };
        let x = i32::try_from(lattice.vertex_mm(i)).ok()?;
        let z = i32::try_from(lattice.vertex_mm(j)).ok()?;
        out.push([x, y, z]);
    }
    Some(out)
}

/// Splits world-millimetre triangles of one layer into cooked tiles (the tile holding each
/// triangle's XZ centroid, floor division), appending to `tiles`.
fn tile_layer(
    tiles: &mut BTreeMap<(i32, i32), Vec<CookedTriangle>>,
    layer: u16,
    tris: &[WorldTriangleMm],
    sections: &[Option<u16>],
) {
    for (index, t) in tris.iter().enumerate() {
        let cx = (i64::from(t.v[0][0]) + i64::from(t.v[1][0]) + i64::from(t.v[2][0])).div_euclid(3) as i32;
        let cz = (i64::from(t.v[0][2]) + i64::from(t.v[1][2]) + i64::from(t.v[2][2])).div_euclid(3) as i32;
        let (ox, oz) = (split_tile(cx).0, split_tile(cz).0);
        let v = t.v.map(|p| [p[0] - ox, p[1], p[2] - oz]);
        tiles.entry((oz, ox)).or_default().push(CookedTriangle {
            v,
            kind: t.kind,
            flags: t.flags,
            section: sections.get(index).copied().flatten(),
            material: 0,
            layer,
            index: index as u32,
        });
    }
}

fn degenerate(t: &WorldTriangleMm) -> bool {
    let [a, b, c] = t.v.map(|p| p.map(i64::from));
    let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let w = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
    let cross = [
        u[1] * w[2] - u[2] * w[1],
        u[2] * w[0] - u[0] * w[2],
        u[0] * w[1] - u[1] * w[0],
    ];
    cross == [0, 0, 0]
}

/// The canonical triangles of a `Passages` source on this terrain and ground: the layout sampled
/// on `terrain` (refused when a stored sampled block disagrees), meshed by the pinned Stage A
/// mesher, quantized, welded and trimmed. Returns the triangles in source order and the report.
///
/// # Errors
///
/// [`CookError::PassagesDrift`], [`CookError::ExtentTooLarge`].
pub fn passages_triangles(
    layer: &str,
    doc: &PassagesDocument,
    terrain: Option<&TerrainSampler>,
    ground: &GroundInput,
    lattice: &LatticeConstants,
) -> Result<(Vec<WorldTriangleMm>, LayerCookReport), CookError> {
    passages_canonical(layer, doc, terrain, ground, lattice).map(|(tris, _, report)| (tris, report))
}

/// [`passages_triangles`] plus the sampled layout the triangles were meshed from (the render cook's input; skylight
/// shafts already opened, [`crate::sections::open_skylight_shafts`]).
///
/// # Errors
///
/// As [`passages_triangles`].
pub fn passages_canonical(
    layer: &str,
    doc: &PassagesDocument,
    terrain: Option<&TerrainSampler>,
    ground: &GroundInput,
    lattice: &LatticeConstants,
) -> Result<(Vec<WorldTriangleMm>, SampledLayout, LayerCookReport), CookError> {
    passages_layer(layer, doc, terrain, ground, lattice).map(|l| (l.triangles, l.sampled, l.report))
}

/// One `Passages` layer's canonical cook: the triangles, the layout they were meshed from, the mesher's own
/// triangles before quantization (the section step traces them), and the report.
#[derive(Clone, Debug, PartialEq)]
pub struct PassagesLayer {
    /// The canonical triangles, source order.
    pub triangles: Vec<WorldTriangleMm>,
    /// The sampled layout as meshed (skylight shafts opened).
    pub sampled: SampledLayout,
    /// The mesher's triangles, same order, before quantization.
    pub raw: Vec<TunnelTriangle>,
    /// The report.
    pub report: LayerCookReport,
}

/// The canonical half of one `Passages` layer: the layout sampled on `terrain` (refused when a stored sampled block
/// disagrees), every skylight opening's ramp climb opened to the sky (K8), meshed by the pinned Stage A mesher,
/// quantized, welded and trimmed.
///
/// # Errors
///
/// As [`passages_triangles`].
pub fn passages_layer(
    layer: &str,
    doc: &PassagesDocument,
    terrain: Option<&TerrainSampler>,
    ground: &GroundInput,
    lattice: &LatticeConstants,
) -> Result<PassagesLayer, CookError> {
    let mut sampled = doc.layout.sample(terrain);
    if doc.sampled.as_ref().is_some_and(|stored| *stored != sampled) {
        return Err(CookError::PassagesDrift { layer: layer.into() });
    }
    let skylight_segments_opened = open_skylight_shafts(&mut sampled, &doc.layout.openings);
    let mesh = sampled.mesh(&doc.layout.openings, terrain);
    let mut tris = quantize_mesh(&mesh.triangles)?;
    let welded_vertices = weld(&mut tris, Some(ground), lattice);
    let trimmed_triangles = trim(&mut tris, Some(ground), lattice);
    let report = LayerCookReport {
        id: layer.into(),
        triangles: tris.len() as u32,
        welded_vertices,
        trimmed_triangles,
        degenerate_triangles: tris.iter().filter(|t| degenerate(t)).count() as u32,
        skylight_segments_opened,
        ..LayerCookReport::default()
    };
    Ok(PassagesLayer {
        triangles: tris,
        sampled,
        raw: mesh.triangles,
        report,
    })
}

/// The id of the cook's prop layer in [`CookReport::layers`] (ADR-8323): the dressing that is a floor.
pub const PROP_LAYER_ID: &str = "skylight_props";
/// A prop vertex this much or less over the floor under it lies on that floor, millimetres.
pub const PROP_RISE_MM: i32 = 20;
/// A prop vertex more than this over the highest layer floor under it is not on that floor's span,
/// millimetres (the shaft side climbing to the plain).
pub const PROP_MAX_RISE_MM: i32 = 4000;

/// The floor height (mm) under a prop vertex, if the vertex stands in a layer span: the highest
/// support floor no higher than [`PROP_RISE_MM`] over it and no more than [`PROP_MAX_RISE_MM`] under
/// it, with no roof between the floor and the vertex.
fn prop_floor_under(layers: &[Vec<WorldTriangleMm>], p: [i32; 3]) -> Option<f64> {
    let (x, z) = (f64::from(p[0]), f64::from(p[2]));
    let y = f64::from(p[1]);
    let mut best: Option<f64> = None;
    for t in layers.iter().flatten() {
        if t.kind != TriKind::Floor || t.flags & TRI_FLAG_NO_SUPPORT != 0 {
            continue;
        }
        if let Some(h) = plane_height_at(t, x, z)
            && h <= y + f64::from(PROP_RISE_MM)
            && y - h <= f64::from(PROP_MAX_RISE_MM)
            && best.is_none_or(|b| h > b)
        {
            best = Some(h);
        }
    }
    let floor = best?;
    let through_lid = layers
        .iter()
        .flatten()
        .any(|t| t.kind == TriKind::Roof && plane_height_at(t, x, z).is_some_and(|h| h > floor && h <= y));
    (!through_lid).then_some(floor)
}

/// The cook's prop layer (T3 addendum § 1, plan row W-30, ADR-8323): the drawn rubble under a
/// skylight becomes canonical floor triangles, so a walker stands on it through `support` and no
/// prop hook. For each `*_skylight` patch of `terrain`, in patch order, every patch cell (row-major)
/// is split into two up-facing triangles at the patch heights, quantized with `m_to_mm`. A
/// triangle is kept when its three vertices stand in a layer span ([`PROP_MAX_RISE_MM`], no roof in
/// between) and at least one is more than [`PROP_RISE_MM`] over the floor under it: the flat part
/// of the patch is the layer floor already. Kept triangles are `Floor` with `TRI_FLAG_PILOT_ONLY`
/// (support reads them; navigation does not route hulls over dressing). Cook-only `f64`.
///
/// # Errors
///
/// [`CookError::ExtentTooLarge`] for a vertex beyond 2^24 mm of the origin.
pub fn skylight_prop_triangles(
    terrain: &TerrainSampler,
    layers: &[Vec<WorldTriangleMm>],
) -> Result<Vec<WorldTriangleMm>, CookError> {
    let mut out = Vec::new();
    for patch in terrain.patches.iter().filter(|p| p.id.ends_with("_skylight")) {
        if patch.cols < 2 || patch.rows < 2 || patch.heights.len() < patch.cols * patch.rows {
            continue;
        }
        let vertex = |c: usize, r: usize| -> Result<Option<([i32; 3], bool)>, CookError> {
            let h = patch.heights[r * patch.cols + c];
            if !h.is_finite() || h <= crate::compat::terrain_patch::NONE_M + 1.0 {
                return Ok(None);
            }
            let wx = patch.origin[0] + c as f32 * patch.spacing;
            let wz = patch.origin[1] + r as f32 * patch.spacing;
            let mut p = [0i32; 3];
            for (o, v) in p.iter_mut().zip([wx, h, wz]) {
                if !v.is_finite() {
                    return Err(CookError::ExtentTooLarge);
                }
                *o = m_to_mm(v);
                if i64::from(*o).abs() >= MAX_EXTENT_MM {
                    return Err(CookError::ExtentTooLarge);
                }
            }
            Ok(prop_floor_under(layers, p).map(|f| (p, f64::from(p[1]) - f > f64::from(PROP_RISE_MM))))
        };
        for r in 0..patch.rows - 1 {
            for c in 0..patch.cols - 1 {
                let corners = [
                    vertex(c, r)?,
                    vertex(c + 1, r)?,
                    vertex(c, r + 1)?,
                    vertex(c + 1, r + 1)?,
                ];
                let [Some(v00), Some(v10), Some(v01), Some(v11)] = corners else {
                    continue;
                };
                for [a, b, c] in [[v00, v01, v10], [v10, v01, v11]] {
                    let t = WorldTriangleMm {
                        v: [a.0, b.0, c.0],
                        kind: TriKind::Floor,
                        flags: TRI_FLAG_PILOT_ONLY,
                    };
                    if (a.1 || b.1 || c.1) && !degenerate(&t) {
                        out.push(t);
                    }
                }
            }
        }
    }
    Ok(out)
}

/// The pinned mesher's threshold-sill corners at every open-cut run start of `ramp` that K8 opened:
/// segments open in `meshed` (skylight shafts opened) but covered in `cut_from`, the layout the
/// mouth was cut from (ADR-8533).
///
/// The mesher writes a sill 0.3 m over the floor where an open-cut run leaves the rock, 1 m along
/// the run. Trim takes an authored sill out of support: it lies within [`TRIM_NEAR_M`] of the
/// floor. A run K8 opens starts where a skylight ramp leaves its base passage, often on a bend, and
/// there the floor triangle under the sill can twist more than [`TRIM_NEAR_M`] away from a sill
/// centroid; that sill then carries a body 0.3 m over the floor it steps onto. The corners are the
/// mesher's own (the run start's floor edge point at the sill height, `f32` in its order, then
/// [`m_to_mm`]), so no other floor matches.
fn opened_sill_corners(cut_from: &SampledLayout, meshed: &SampledLayout, ramp: &str) -> Vec<[i32; 3]> {
    use crate::compat::layout::PassageKind;
    let find = |layout: &SampledLayout| {
        layout
            .passages
            .iter()
            .position(|p| p.id == ramp && p.kind == PassageKind::Ramp)
    };
    let (Some(ai), Some(mi)) = (find(cut_from), find(meshed)) else {
        return Vec::new();
    };
    let (authored, p) = (&cut_from.passages[ai].covered, &meshed.passages[mi]);
    let half = p.width_m * 0.5;
    let mut corners = Vec::new();
    for k in 0..p.covered.len() {
        let opened = !p.covered[k] && authored.get(k).copied().unwrap_or(false);
        let run_start = k == 0 || p.covered[k - 1];
        let Some(a) = p.samples.get(k).filter(|_| opened && run_start) else {
            continue;
        };
        // The mesher's `edge(a, sign, 0.0)` and `y0 = a.p[1] + 0.3`.
        let y0 = m_to_mm(a.p[1] + 0.3);
        for sign in [1.0f32, -1.0] {
            corners.push([
                m_to_mm(a.p[0] + a.left[0] * half * sign),
                y0,
                m_to_mm(a.p[2] + a.left[1] * half * sign),
            ]);
        }
    }
    corners
}

/// Flags every level floor triangle with a vertex in `corners` (a K8 run's threshold sill, see
/// `opened_sill_corners`) `TRI_FLAG_NO_SUPPORT`, as trim does an authored sill. Flags only: the
/// triangle order and every section range stay as they are. Returns how many it newly flagged
/// (ADR-8533).
fn quiet_sills(tris: &mut [WorldTriangleMm], corners: &BTreeSet<[i32; 3]>) -> u32 {
    let mut flagged = 0u32;
    for t in tris.iter_mut() {
        let sill = t.kind == TriKind::Floor
            && t.v.iter().all(|v| v[1] == t.v[0][1])
            && t.v.iter().any(|v| corners.contains(v));
        if sill && t.flags & TRI_FLAG_NO_SUPPORT == 0 {
            t.flags |= TRI_FLAG_NO_SUPPORT;
            flagged += 1;
        }
    }
    flagged
}

/// The header's input hash: BLAKE3 of a tag, the document bytes, the heightfield and every named
/// file (path and bytes) in layer order, each length-prefixed. The tag also identifies the cook
/// semantics: revision 2 rebuilds legacy skylight cuts (ADR-10346).
#[must_use]
pub fn input_hash(document: &[u8], ground: &GroundInput, files: &[(&str, &[u8])]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    let mut put = |b: &[u8]| {
        h.update(&(b.len() as u64).to_le_bytes());
        h.update(b);
    };
    put(b"regia-meshlayers-input/2");
    put(document);
    let mut g = Vec::with_capacity(16 + ground.counts.len() * 2);
    g.extend_from_slice(&ground.lattice_n.to_le_bytes());
    g.extend_from_slice(&ground.width_m.to_bits().to_le_bytes());
    g.extend_from_slice(&ground.offset_m.to_bits().to_le_bytes());
    g.extend_from_slice(&ground.step_m.to_bits().to_le_bytes());
    for c in &ground.counts {
        g.extend_from_slice(&c.to_le_bytes());
    }
    put(&g);
    for (path, bytes) in files {
        put(path.as_bytes());
        put(bytes);
    }
    *h.finalize().as_bytes()
}

/// Grows the rectangle `r` to hold `p`.
fn grow(r: &mut RectMm, p: [i64; 2]) {
    r[0] = [r[0][0].min(p[0]), r[0][1].min(p[1])];
    r[1] = [r[1][0].max(p[0]), r[1][1].max(p[1])];
}

/// The XZ footprints of seam rule 5: each mouth ring's bounds and each layer triangle's bounds,
/// world millimetres.
fn footprints(lattice: &LatticeConstants, cut: &LatticeCut, layers: &[Vec<WorldTriangleMm>]) -> Vec<RectMm> {
    let mut out = Vec::new();
    for m in &cut.mouths {
        for ring in &m.rings {
            let mut r = [[i64::MAX; 2], [i64::MIN; 2]];
            for &(i, j) in ring {
                grow(&mut r, [lattice.vertex_mm(i), lattice.vertex_mm(j)]);
            }
            out.push(r);
        }
    }
    for t in layers.iter().flatten() {
        let mut r = [[i64::MAX; 2], [i64::MIN; 2]];
        for p in t.v {
            grow(&mut r, [i64::from(p[0]), i64::from(p[2])]);
        }
        out.push(r);
    }
    out
}

/// The canonical triangles of one non-`Passages` layer that the caller meshed (ADR-10071, spec 26
/// § 2a.9): a `Volume` source's LOD-0 volume mesh, clipped under the ground, from the asset
/// pipeline (this crate does not mesh volumes). [`cook_with_layers`] cooks it as a layer with no
/// sections, portals or plugs; its filled holes are `mouths` and go through the same lattice cut,
/// collar and zipper as a `Passages` layer's.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProvidedLayer {
    /// The layer id it is for (a document layer with a `Volume` source).
    pub id: String,
    /// The canonical triangles, world millimetres (`m_to_mm` of each coordinate), in a
    /// deterministic order. Each becomes a floor, a roof or a wall by its normal
    /// ([`provided_kind`]).
    pub triangles: Vec<[[i32; 3]; 3]>,
    /// One footprint per hole the layer fills (matched to `fills` by id).
    pub mouths: Vec<MouthFootprint>,
    /// What the input hash covers for this layer in place of a file's bytes (the source's
    /// fingerprint: the field's identity and the meshing parameters).
    pub source_bytes: Vec<u8>,
}

/// The kind of a provided (volume) canonical triangle (ADR-10071): a floor when its normal (the
/// winding's, up out of the solid) is within 45 degrees of +y, a roof within 45 degrees of -y, a
/// wall otherwise. Integer arithmetic on the millimetre vertices, the same on every machine.
#[must_use]
pub fn provided_kind(v: &[[i32; 3]; 3]) -> TriKind {
    let d = |a: [i32; 3], b: [i32; 3]| [0, 1, 2].map(|k| i128::from(b[k]) - i128::from(a[k]));
    let (u, w) = (d(v[0], v[1]), d(v[0], v[2]));
    let n = [
        u[1] * w[2] - u[2] * w[1],
        u[2] * w[0] - u[0] * w[2],
        u[0] * w[1] - u[1] * w[0],
    ];
    let horizontal = n[0] * n[0] + n[2] * n[2];
    if n[1] > 0 && horizontal <= n[1] * n[1] {
        TriKind::Floor
    } else if n[1] < 0 && horizontal <= n[1] * n[1] {
        TriKind::Roof
    } else {
        TriKind::Wall
    }
}

/// Whether a millimetre triangle has zero area (its edge vectors are parallel).
fn provided_area_zero(v: &[[i32; 3]; 3]) -> bool {
    let d = |a: [i32; 3], b: [i32; 3]| [0, 1, 2].map(|k| i128::from(b[k]) - i128::from(a[k]));
    let (u, w) = (d(v[0], v[1]), d(v[0], v[2]));
    u[1] * w[2] == u[2] * w[1] && u[2] * w[0] == u[0] * w[2] && u[0] * w[1] == u[1] * w[0]
}

/// Cooks a document (spec 121 § 9.2): see the module docs for what is cooked and what is refused.
/// A `Volume` layer is refused (`cook_not_built`): [`cook_with_layers`] takes its meshed
/// triangles.
///
/// # Errors
///
/// [`CookError`].
pub fn cook(inputs: &CookInputs<'_>) -> Result<CookOutput, CookError> {
    cook_with_layers(inputs, &[])
}

/// [`cook`] with the canonical triangles of the document's `Volume` layers (ADR-10071): a
/// `Volume` layer with a [`ProvidedLayer`] of its id cooks from it; one without is still
/// `cook_not_built`. With no provided layers this is [`cook`], byte for byte.
///
/// # Errors
///
/// [`CookError`]; [`CookError::MouthUnknown`] when a provided layer fills a hole it has no
/// footprint for.
pub fn cook_with_layers(
    inputs: &CookInputs<'_>,
    provided: &[ProvidedLayer],
) -> Result<CookOutput, CookError> {
    let _span = tracing::info_span!("meshlayers_cook").entered();
    let text = std::str::from_utf8(inputs.document)
        .map_err(|e| CookError::Document(MeshLayersError::Parse(format!("not UTF-8: {e}"))))?;
    let doc = MeshLayersDocument::from_ron(text)?;
    let settings = doc.geom.normalized().map_err(MeshLayersError::from)?;
    let lattice = inputs.ground.lattice(doc.ground_tile_cells)?;
    if doc.audit.require_sweep {
        return Err(CookError::Checks(checks::sweep_missing_report("", doc.ground)));
    }
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut tiles: BTreeMap<(i32, i32), Vec<CookedTriangle>> = BTreeMap::new();
    let mut report = CookReport::default();
    let mut presentation = PresentationCooked::default();
    let mut sections = Vec::new();
    let mut portals = Vec::new();
    let mut zones = Vec::new();
    let mut plugs = Vec::new();
    // Per layer: its triangles (world mm) and the mouths it fills.
    let mut layer_tris: Vec<Vec<WorldTriangleMm>> = Vec::new();
    let mut mouths: Vec<(u16, MouthFootprint)> = Vec::new();
    // Per mouth: the top edge of its ramp floor, when the ramp ends off the ground (ADR-8396).
    let mut tops: Vec<Option<crate::approach::RampTop>> = Vec::new();
    // Per mouth: K8 opened more of its ramp's climb than the layout the mouth was cut from, so the
    // mesher's lips run down the trench past the ring (ADR-8533).
    let mut lips_past_ring: Vec<bool> = Vec::new();
    // Per layer: the threshold-sill corners of the climbs K8 opened past a filled mouth's cut (ADR-8533).
    let mut opened_sills: Vec<BTreeSet<[i32; 3]>> = Vec::new();
    // Per layer: the section and the pre-seal index of each canonical and plug triangle (the prefix of
    // `layer_tris` they stay; the seal appends after them), so the seal's drops keep section ranges true.
    let mut layer_meta: Vec<Vec<TriMeta>> = Vec::new();
    // Per layer: where its plug triangles start in the pre-seal order.
    let mut plug_start: Vec<u32> = Vec::new();
    let mut layouts = Vec::new();
    for (layer_index, layer) in doc.layers.iter().enumerate() {
        if let LayerSource::Volume { sdf, .. } = &layer.source
            && let Some(given) = provided.iter().find(|p| p.id == layer.id)
        {
            let tris: Vec<WorldTriangleMm> = given
                .triangles
                .iter()
                .map(|v| WorldTriangleMm {
                    v: *v,
                    kind: provided_kind(v),
                    flags: 0,
                })
                .collect();
            if tris
                .iter()
                .any(|t| t.v.iter().flatten().any(|c| i64::from(*c).abs() >= MAX_EXTENT_MM))
            {
                return Err(CookError::ExtentTooLarge);
            }
            for hole in &layer.fills {
                let Some(m) = given.mouths.iter().find(|m| &m.id == hole) else {
                    return Err(CookError::MouthUnknown {
                        layer: layer.id.clone(),
                        hole: hole.clone(),
                    });
                };
                mouths.push((layer_index as u16, m.clone()));
                tops.push(None);
                lips_past_ring.push(false);
            }
            opened_sills.push(BTreeSet::new());
            layer_meta.push(
                (0..tris.len() as u32)
                    .map(|index| TriMeta { index, section: None })
                    .collect(),
            );
            plug_start.push(tris.len() as u32);
            report.layers.push(LayerCookReport {
                id: layer.id.clone(),
                triangles: tris.len() as u32,
                degenerate_triangles: tris.iter().filter(|t| provided_area_zero(&t.v)).count() as u32,
                ..LayerCookReport::default()
            });
            layer_tris.push(tris);
            files.push((sdf.clone(), given.source_bytes.clone()));
            continue;
        }
        let path = match &layer.source {
            LayerSource::Passages { path } => path,
            LayerSource::Gltf { .. } => {
                return Err(CookError::NotBuilt {
                    what: format!("layer `{}` glTF source", layer.id),
                    lane: "W-16",
                });
            }
            LayerSource::Collision { .. } => {
                return Err(CookError::NotBuilt {
                    what: format!("layer `{}` collision source", layer.id),
                    lane: "W-16",
                });
            }
            LayerSource::Volume { .. } => {
                return Err(CookError::NotBuilt {
                    what: format!("layer `{}` volume source", layer.id),
                    lane: "W-32",
                });
            }
        };
        let bytes = (inputs.read_file)(path).map_err(|reason| CookError::InputMissing {
            path: path.clone(),
            reason,
        })?;
        let passages_text = std::str::from_utf8(&bytes).map_err(|e| CookError::Passages {
            layer: layer.id.clone(),
            error: PassagesError::Document(MeshLayersError::Parse(format!("not UTF-8: {e}"))),
        })?;
        let passages = PassagesDocument::from_ron(passages_text).map_err(|error| CookError::Passages {
            layer: layer.id.clone(),
            error,
        })?;
        if let Some(detail) = layer.look.detail {
            let sum = detail.normal_displace.abs() + detail.noise_amplitude.abs();
            if sum > DISPLACE_LIMIT_M {
                return Err(CookError::Checks(checks::displacement_report(
                    &passages.layout.map_id,
                    doc.ground,
                    &layer.id,
                    detail.normal_displace,
                    detail.noise_amplitude,
                )));
            }
            if detail.normal_displace != 0.0 || detail.noise_amplitude != 0.0 {
                return Err(CookError::NotBuilt {
                    what: format!("layer `{}` displaced render detail", layer.id),
                    lane: "W-16",
                });
            }
        }
        layouts.push(passages.layout.clone());
        let PassagesLayer {
            triangles: tris,
            sampled,
            raw,
            report: mut layer_report,
        } = passages_layer(&layer.id, &passages, inputs.terrain, inputs.ground, &lattice)?;
        let mut sills: BTreeSet<[i32; 3]> = BTreeSet::new();
        if !layer.fills.is_empty() {
            // K8 (spec 121 § 5.6, `crate::sections`): a skylight climb is an open cut and its footprint is a
            // mouth. The mouth is cut from the layout as meshed (shafts opened), so no part of an opened climb
            // stays under uncut ground: before ADR-8898 the mouth came from the layout before K8, and the
            // climb K8 opened past it (Switchbacks `west` samples 5-10, `mid_east` 6-11) answered "open sky"
            // with the ground sheet 15-27 m over it. `authored` (the layout before K8) still names the
            // threshold sills K8's runs start with (ADR-8533).
            let authored = passages.layout.sample(inputs.terrain);
            let derived = crate::cut::passage_mouths(&passages.layout, &sampled);
            for hole in &layer.fills {
                let Some(m) = derived.iter().find(|m| &m.id == hole) else {
                    return Err(CookError::MouthUnknown {
                        layer: layer.id.clone(),
                        hole: hole.clone(),
                    });
                };
                mouths.push((layer_index as u16, m.clone()));
                tops.push(ramp_top(&passages, &sampled, hole, &tris));
                // A climb K8 opened past the authored layout still gets ADR-8533's flat strip on the pins:
                // the zipper does not band the strip between the ring and the top edge of such a climb (on
                // Switchbacks `west` without it support answers the tube floor 30 m down there), whichever
                // layout the mouth is cut from.
                let past = climb_opened_past_cut(&passages, &authored, &sampled, hole);
                lips_past_ring.push(past);
                if past && let Some(o) = passages.layout.openings.iter().find(|o| &o.id == hole) {
                    sills.extend(opened_sill_corners(&authored, &sampled, &o.ramp));
                }
            }
        }
        opened_sills.push(sills);
        let sec = layer_sections(&SectionInputs {
            layer: layer_index as u16,
            layer_id: &layer.id,
            sampled: &sampled,
            layout: &passages.layout,
            raw: &raw,
            canonical: &tris,
            rule: &doc.sections,
            blockers: &layer.blockers,
            base: IdBase {
                section: sections.len(),
                portal: portals.len(),
                plug: plugs.len(),
            },
        })
        .map_err(|e| match e {
            SectionsError::Trace(msg) => CookError::Ground(format!("layer `{}`: {msg}", layer.id)),
            other => CookError::Document(MeshLayersError::Invalid(format!("layer `{}`: {other}", layer.id))),
        })?;
        let mut all = tris.clone();
        all.extend_from_slice(&sec.plug_triangles);
        if all
            .iter()
            .any(|t| t.v.iter().flatten().any(|c| i64::from(*c).abs() >= MAX_EXTENT_MM))
        {
            return Err(CookError::ExtentTooLarge);
        }
        layer_report.sections = sec.sections.len() as u32;
        layer_report.plug_triangles = sec.plug_triangles.len() as u32;
        sections.extend(sec.sections);
        portals.extend(sec.portals);
        zones.extend(sec.zones);
        plugs.extend(sec.plugs);
        let render = render_layer(
            &RenderLayerInputs {
                layer: layer_index as u16,
                id: &layer.id,
                material: &layer.material,
                uv_tile_m: layer.look.uv_tile_m,
                subdivision: layer.look.subdivision,
                sampled: &sampled,
                openings: &passages.layout.openings,
            },
            &tris,
        );
        layer_report.render_triangles = render.triangles.len() as u32;
        layer_report.render_sections = render
            .passages
            .iter()
            .map(|p| RenderSectionReport {
                passage: p.id.clone(),
                triangles: p.count,
            })
            .collect();
        presentation.layers.push(render);
        layer_meta.push(
            sec.triangle_section
                .iter()
                .enumerate()
                .map(|(index, &section)| TriMeta {
                    index: index as u32,
                    section,
                })
                .collect(),
        );
        plug_start.push(tris.len() as u32);
        layer_tris.push(all);
        report.layers.push(layer_report);
        files.push((path.clone(), bytes));
    }
    // The prop layer (ADR-8323): after the document's layers, only when a skylight patch gives one.
    if let Some(terrain) = inputs.terrain {
        let props = skylight_prop_triangles(terrain, &layer_tris)?;
        if !props.is_empty() {
            report.layers.push(LayerCookReport {
                id: PROP_LAYER_ID.into(),
                triangles: props.len() as u32,
                ..LayerCookReport::default()
            });
            // Prop triangles belong to no section and are no plug (integ-v06r-det's one section list
            // per layer; on this line the seal's `layer_meta`/`plug_start`, ADR-8421). An empty meta
            // reads as "no section" in `tile_layer` and keeps the props out of the viscells' flood,
            // like the seal's own additions.
            plug_start.push(props.len() as u32);
            layer_meta.push(Vec::new());
            layer_tris.push(props);
        }
    }
    let footprints_only: Vec<MouthFootprint> = mouths.iter().map(|(_, m)| m.clone()).collect();
    let cut = crate::cut::cut_lattice(&lattice, &inputs.ground.counts, &footprints_only)?;
    let collar = crate::cut::collar(&lattice, &inputs.ground.counts, &cut);
    let mut collar_counts = vec![0u32; mouths.len()];
    let sealed: Vec<Vec<[i32; 3]>> = cut
        .rings
        .iter()
        .filter_map(|ring| ring_mm(&lattice, &cut, ring))
        .collect();
    for (tris, meta) in layer_tris.iter_mut().zip(&mut layer_meta) {
        silence_floors_above_ring(tris, meta, &sealed);
    }
    // A climb K8 opened past its mouth's cut starts its open run with the mesher's threshold sill:
    // out of support, as trim takes an authored sill (ADR-8533).
    for ((tris, corners), layer_report) in layer_tris.iter_mut().zip(&opened_sills).zip(&mut report.layers) {
        layer_report.trimmed_triangles += quiet_sills(tris, corners);
    }
    // A hole whose centroid falls through is capped. A side lip is not written. A triangle
    // whose interior fights an authored floor is dropped. The filtered collar and the 1 mm
    // wedges still seal the ring (ADR-8334).
    let mut pending_bands: Vec<(usize, Vec<WorldTriangleMm>)> = Vec::new();
    let mut by_layer: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (mi, (layer, _)) in mouths.iter().enumerate() {
        by_layer.entry(usize::from(*layer)).or_default().push(mi);
    }
    for (layer, mouth_ix) in by_layer {
        let rings_per_mouth: Vec<Option<Vec<Vec<[i32; 3]>>>> = mouth_ix
            .iter()
            .map(|&mi| {
                cut.mouths[mi]
                    .rings
                    .iter()
                    .map(|ring| ring_mm(&lattice, &cut, ring))
                    .collect::<Option<Vec<_>>>()
            })
            .collect();
        if rings_per_mouth.iter().any(Option::is_none) {
            continue;
        }
        let rings_per_mouth: Vec<Vec<Vec<[i32; 3]>>> = rings_per_mouth.into_iter().flatten().collect();
        let bands = crate::zipper::zip_layer(&layer_tris[layer], &rings_per_mouth);
        let mut kept = Vec::new();
        for band in bands.into_iter().flatten() {
            for tri in band {
                if stitch_fills_a_hole(&tri, &layer_tris[layer]) {
                    kept.push(tri);
                }
            }
        }
        if !kept.is_empty() {
            pending_bands.push((layer, kept));
        }
    }
    // A ramp top off the ground gets one approach strip from its ring (ADR-8396). Its band holds
    // the ring at the pins, so the collar under it is filtered out below. A skylight climb that K8
    // opened past its mouth's cut gets the strip on the pins too, flat (ADR-8533): its lips run
    // down the trench past the ring, the zipper's band does not reach the top edge, and the collar
    // alone leaves the strip between the ring and the top edge open over the tunnel.
    let mut claimed: BTreeSet<([i32; 3], [i32; 3])> = BTreeSet::new();
    let mut approach_tris: Vec<(usize, [[i32; 3]; 3])> = Vec::new();
    let edge_key = |a: [i32; 3], b: [i32; 3]| if a <= b { (a, b) } else { (b, a) };
    let sealed_edges: BTreeSet<([i32; 3], [i32; 3])> = sealed
        .iter()
        .flat_map(|ring| (0..ring.len()).map(move |k| (ring[k], ring[(k + 1) % ring.len()])))
        .map(|(a, b)| edge_key(a, b))
        .collect();
    for (mi, top) in tops.iter().enumerate() {
        let Some(top) = top else {
            continue;
        };
        let on_pins = lips_past_ring[mi];
        let strip = sealed
            .iter()
            .map(|ring| crate::approach::approach_strip(ring, top, on_pins))
            .find(|(strip, _)| !strip.is_empty());
        if let Some((strip, held)) = strip {
            for (a, b) in held {
                claimed.insert(edge_key(a, b));
            }
            let layer = usize::from(mouths[mi].0);
            // A zipper cap under the rise is a second floor there, at the ground: support would
            // keep a body on it up to the top edge and then step it onto the ramp.
            for (band_layer, band) in &mut pending_bands {
                if *band_layer == layer {
                    band.retain(|tri| !strip.iter().any(|a| crate::approach::xz_overlap(a, tri)));
                }
            }
            collar_counts[mi] += strip.len() as u32;
            approach_tris.extend(strip.iter().map(|t| (mi, t.v)));
            layer_tris[layer].extend(strip);
        }
    }
    // A query one f32 step off a ring vertex can land in the next cut cell, which the
    // ring-edge collar does not cover. Seal those samples or support answers the floor below.
    // Samples already on the collar, or on an authored floor within 1 mm of the pin, are left
    // alone: each extra triangle costs resident bytes the sim budget barely has.
    // A zipper band (ADR-8334 rule 4) that has a ring edge holds it too: that edge's collar
    // would make it the edge of two canonical triangles (spec 121 § 3 item 3; ADR-8396).
    let band_floors: Vec<WorldTriangleMm> = pending_bands
        .iter()
        .flat_map(|(_, band)| band.iter().copied())
        .collect();
    for tri in &band_floors {
        for e in 0..3 {
            let k = edge_key(tri.v[e], tri.v[(e + 1) % 3]);
            if sealed_edges.contains(&k) {
                claimed.insert(k);
            }
        }
    }
    let (wedges, keep) = ulp_wedges(
        &lattice,
        &inputs.ground.counts,
        &cut,
        &layer_tris,
        &band_floors,
        &claimed,
    );
    let cells = lattice.cells();
    let mut collar_keys = Vec::new();
    for &(j, i, t) in &cut.cut {
        let ring_edge = (0..3u8).any(|e| {
            crate::cut::neighbor(cells, i, j, t, e)
                .is_none_or(|(ni, nj, nt)| !cut.cut.contains(&(nj, ni, nt)))
        });
        if ring_edge {
            collar_keys.push((j, i, t));
        }
    }
    let mut kept = Vec::new();
    // Ring edges a kept collar triangle holds although a band holds them too: a corner cell
    // whose other ring edge no band holds. The collar keeps them and the band triangle goes.
    let mut collar_owned: BTreeSet<([i32; 3], [i32; 3])> = BTreeSet::new();
    for (tri, key) in collar.into_iter().zip(collar_keys) {
        // An approach or zipper band already holds this collar's ring edges (ADR-8396): one
        // canonical triangle per ring edge.
        let ring_edges: Vec<([i32; 3], [i32; 3])> = (0..3)
            .map(|e| edge_key(tri.v[e], tri.v[(e + 1) % 3]))
            .filter(|k| sealed_edges.contains(k))
            .collect();
        if !ring_edges.is_empty() && ring_edges.iter().all(|k| claimed.contains(k)) {
            continue;
        }
        // Spec 121 § 3, the ring pin, item 3 (ADR-8533): a ring edge no band holds is an edge of its
        // collar triangle, even where an authored floor (a lip at the pin) already covers it.
        let unheld = ring_edges.iter().all(|k| !claimed.contains(k));
        if keep.contains(&key) || unheld {
            collar_owned.extend(ring_edges.iter().filter(|k| claimed.contains(k)));
            kept.push(tri);
        }
    }
    if !collar_owned.is_empty() {
        let holds =
            |v: &[[i32; 3]; 3]| (0..3).any(|e| collar_owned.contains(&edge_key(v[e], v[(e + 1) % 3])));
        for (_, band) in &mut pending_bands {
            band.retain(|t| !holds(&t.v));
        }
        let dropped: BTreeSet<[[i32; 3]; 3]> = approach_tris
            .iter()
            .filter(|(_, v)| holds(v))
            .map(|&(mi, v)| {
                collar_counts[mi] -= 1;
                v
            })
            .collect();
        if !dropped.is_empty() {
            for (tris, meta) in layer_tris.iter_mut().zip(&mut layer_meta) {
                retain_with_meta(tris, meta, |t| !dropped.contains(&t.v));
            }
        }
    }
    let mut collar = kept;
    collar.extend(wedges);
    for c in &collar {
        let layer = usize::from(mouths[c.mouth].0);
        layer_tris[layer].push(WorldTriangleMm {
            v: c.v,
            kind: c.kind,
            flags: 0,
        });
        collar_counts[c.mouth] += 1;
    }
    for (layer, band) in pending_bands {
        layer_tris[layer].extend(band);
    }
    // The seal dropped triangles from some layers: move every section and plug range onto the
    // surviving order (ADR-8421).
    for (layer_index, meta) in layer_meta.iter().enumerate() {
        let layer = layer_index as u16;
        for section in &mut sections {
            section.triangles = section
                .triangles
                .iter()
                .map(|r| {
                    if r.layer == layer {
                        remap_range(r, meta)
                    } else {
                        *r
                    }
                })
                .filter(|r| r.start < r.end)
                .collect();
        }
        for plug in &mut plugs {
            if plug.triangles.layer == layer {
                plug.triangles = remap_range(&plug.triangles, meta);
            }
        }
    }
    for (layer_index, tris) in layer_tris.iter().enumerate() {
        let meta = &layer_meta[layer_index];
        let section: Vec<Option<u16>> = meta.iter().map(|m| m.section).collect();
        tile_layer(&mut tiles, layer_index as u16, tris, &section);
        let plug_tris = meta.iter().filter(|m| m.index >= plug_start[layer_index]).count() as u32;
        let r = &mut report.layers[layer_index];
        r.plug_triangles = plug_tris;
        r.triangles = tris.len() as u32 - plug_tris;
    }
    let tiles: Vec<CookedTile> = tiles
        .into_iter()
        .map(|((oz, ox), mut triangles)| {
            triangles.sort_by_key(|t| (t.layer, t.index));
            CookedTile {
                origin_mm: [ox, oz],
                triangles,
            }
        })
        .collect();
    let pitch_inputs = PitchInputs {
        pitch_error_m: settings.pitch_error_m,
        fine_band_m: settings.fine_band_m,
        footprints: footprints(&lattice, &cut, &layer_tris),
    };
    let build = |levels: Vec<u8>| -> WorldGeomCooked {
        let hanging = crate::pitch::hanging_overrides(&lattice, &inputs.ground.counts, &levels);
        WorldGeomCooked {
            canonical_version: CANONICAL_VERSION,
            settings,
            lattice,
            ground: doc.ground,
            ground_counts: crate::pitch::count_blocks(&lattice, &inputs.ground.counts, &levels),
            tile_levels: levels,
            hanging_overrides: hanging,
            ring_pins: cut.ring_pins.clone(),
            tiles: tiles.clone(),
            cut_bitmaps: crate::cut::cut_bitmaps(&lattice, &cut.cut),
            sections: sections.clone(),
            portals: portals.clone(),
            zones: zones.clone(),
            plugs: plugs.clone(),
        }
    };
    let per_side = lattice.tiles_per_side() as usize;
    let budget = crate::pitch::sim_budget_bytes(lattice.width_mm);
    let map_id = layouts.first().map_or("", |layout| layout.map_id.as_str());
    let nested = |max_level: u8| {
        build(crate::pitch::choose_levels(
            &lattice,
            &inputs.ground.counts,
            max_level,
            &pitch_inputs,
        ))
    };
    let canonical = match doc.ground {
        GroundMode::Full => {
            let full = build(vec![0; per_side * per_side]);
            let bytes = crate::pitch::sim_ground_bytes(&full);
            if bytes > budget {
                let most = lattice.ground_tile_cells.trailing_zeros() as u8;
                let fits = (1..=most)
                    .find(|&k| crate::pitch::sim_ground_bytes(&nested(k)) <= budget)
                    .map(|max_level| GroundMode::Nested { max_level });
                if let Some(refusal) =
                    checks::ground_over_budget_report(map_id, doc.ground, bytes, budget, fits, &[])
                {
                    return Err(CookError::Checks(refusal));
                }
            }
            full
        }
        GroundMode::Nested { max_level } => {
            let c = nested(max_level);
            let bytes = crate::pitch::sim_ground_bytes(&c);
            if bytes > budget {
                let held_fine: Vec<u32> = c
                    .tile_levels
                    .iter()
                    .enumerate()
                    .filter(|(_, l)| **l < max_level)
                    .map(|(t, _)| t as u32)
                    .collect();
                if let Some(refusal) =
                    checks::ground_over_budget_report(map_id, doc.ground, bytes, budget, None, &held_fine)
                {
                    return Err(CookError::Checks(refusal));
                }
            }
            c
        }
    };
    canonical.check().map_err(|e| match e {
        crate::cooked::GeomLoadError::ExtentTooLarge => CookError::ExtentTooLarge,
        other => CookError::Ground(other.to_string()),
    })?;
    report.sections = canonical
        .sections
        .iter()
        .map(|s| SectionReport {
            id: s.id,
            source: s.source.clone(),
            covered: s.covered,
            triangles: s.triangles.iter().map(|r| r.end - r.start).sum(),
            portals: s.portals.clone(),
            blocker: s.blocker,
            reverb: canonical
                .zones
                .iter()
                .find(|z| z.section == s.id)
                .map_or_else(String::new, |z| z.reverb_preset.clone()),
        })
        .collect();
    report.portals = canonical
        .portals
        .iter()
        .map(|p| PortalReport {
            id: p.id,
            sides: p.sides,
            mouth: p.mouth.clone(),
            nav_mm: p.nav_mm,
            blocker: p.blocker,
        })
        .collect();
    let pitch_seam = match doc.ground {
        GroundMode::Full => None,
        GroundMode::Nested { .. } => Some(crate::pitch::check_ground_pitch_seam(
            &canonical,
            &inputs.ground.counts,
            &pitch_inputs,
        )?),
    };
    report.ground = GroundReport {
        mode: doc.ground,
        tile_levels: canonical.tile_levels.clone(),
        tiles_per_side: lattice.tiles_per_side(),
        hanging_vertices: canonical.hanging_overrides.len() as u32,
        ring_pins: canonical.ring_pins.len() as u32,
        mouths: mouths
            .iter()
            .zip(&cut.mouths)
            .zip(&collar_counts)
            .map(|(((layer, _), m), &collar_triangles)| CookedMouth {
                id: m.id.clone(),
                layer: *layer,
                rings: m.rings.clone(),
                cut_triangles: m.cut.len() as u32,
                collar_triangles,
            })
            .collect(),
        rings: cut.rings.clone(),
        sim_bytes: crate::pitch::sim_ground_bytes(&canonical),
        budget_bytes: budget,
        pitch_seam,
    };
    let named: Vec<(&str, &[u8])> = files.iter().map(|(p, b)| (p.as_str(), b.as_slice())).collect();
    let header = MeshLayersHeader {
        version: COOKED_FORMAT_VERSION,
        input_hash: input_hash(inputs.document, inputs.ground, &named),
        settings,
        ground: doc.ground,
    };
    // The viscells see the passages as their sections cut them: the seal's collar, wedges, zipper
    // bands and approach strips stand in for the ground at a mouth, and the ground is no cell's
    // geometry, so they are left out of the cells' flood (ADR-8421).
    let prefix: Vec<u32> = layer_meta.iter().map(|m| m.len() as u32).collect();
    let mut vis_view = canonical.clone();
    for tile in &mut vis_view.tiles {
        tile.triangles
            .retain(|t| prefix.get(usize::from(t.layer)).is_some_and(|&n| t.index < n));
    }
    let emitted = crate::viscells::emit(&vis_view).map_err(viscells_fault)?;
    report.vis_cells = emitted
        .cells
        .iter()
        .map(|c| VisCellReport {
            id: c.id,
            name: c.name.clone(),
            outdoor: c.outdoor,
        })
        .collect();
    report.vis_portals = emitted
        .portals
        .iter()
        .map(|p| VisPortalReport {
            a: p.a,
            b: p.b,
            cooked: p.cooked,
        })
        .collect();
    let canonical_bytes = encode_canonical(&canonical).map_err(|e| CookError::Ground(e.to_string()))?;
    let hash = content_hash_of_bytes(&canonical_bytes);
    // Spec 121 § 11's sim-geometry RAM holds the run-time query structures too: the query index
    // with the segment tables and column directory (ADR-8815), the height pyramid and the ground
    // axis table (ADR-8817). They are built from the section exactly as a load builds them, so
    // the report's estimate tracks them as they change (ADR-10394).
    report.ground.sim_bytes +=
        crate::geom::query_resident_bytes(&canonical_bytes).map_err(|e| CookError::Ground(e.to_string()))?;
    let content_hash = hash.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let bake_report = checks::audit(&checks::AuditInputs {
        map: map_id,
        settings,
        ground_mode: doc.ground,
        ground: inputs.ground,
        canonical: &canonical,
        layouts: &layouts,
        waive: &doc.waive,
        audit: &doc.audit,
        content_hash: &content_hash,
        pitch_seam: report.ground.pitch_seam,
    });
    let presentation = presentation
        .encode()
        .map_err(|e| CookError::Ground(e.to_string()))?;
    if !bake_report.passed {
        return Err(CookError::Checks(bake_report));
    }
    Ok(CookOutput {
        file: CookedMeshLayers {
            header,
            canonical,
            presentation,
            chunks: Vec::new(),
        },
        report,
        viscells_ron: emitted.text,
        bake_report,
    })
}
