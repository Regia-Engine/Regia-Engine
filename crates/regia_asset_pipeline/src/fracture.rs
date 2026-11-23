// SPDX-License-Identifier: MPL-2.0
//! Pre-fractured destruction tool (destruction level 2): splits a closed triangle mesh into
//! Voronoi chunks, caps the cuts, and derives the chunk adjacency graph and the breakable-joint
//! strengths from the contact areas. Specification: `docs/formats/fracture.md`, ADR-1567.
//!
//! Method. `N` sites are placed inside the solid (explicit; seeded best-candidate sampling:
//! Mitchell, "Spectrally optimal sampling for distribution ray tracing", SIGGRAPH 1991; or the
//! impact-biased and clustered rejection samplers of ADR-9870). The
//! Voronoi cell of a site is the intersection of the half-spaces bounded by the bisector planes
//! to every other site (Aurenhammer, "Voronoi diagrams: a survey of a fundamental geometric data
//! structure", ACM Computing Surveys 23(3), 1991); a chunk is the solid clipped by that cell,
//! plane by plane, with every cut closed by a cap (`fracture_clip.rs`). Planes are applied
//! nearest site first and the loop stops at the first plane farther from the site than the
//! current chunk reaches, so a chunk never needs the far sites. The solid may be concave: the
//! chunk of a concave solid can be several lumps, which stay one chunk. Everything is
//! deterministic: the same mesh, settings and seed give the same bytes.
//!
//! Joints. Two chunks touch where their caps meet; the contact area is the area of that cap, and
//! the break force is `strength_per_area * area` (clamped), the simplest model of a bond whose
//! strength grows with the bonded surface. The two sides of one contact are measured separately
//! and averaged; they agree to rounding.
//!
//! Render attributes (`fracture_render.rs`, ADR-1967). Every chunk also carries a render mesh with
//! normals, UVs and tangents: a surface piece reads them from its source triangle, a cut face gets
//! the normal of its plane and a planar projection (the two sides of one cut show the same
//! pattern), and the vertices split where those differ.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::fracture_clip::{
    CapError, P, Plane, SURFACE, Tri, add, cap, clip, cross, dot, length, scale, sub,
};
pub use crate::fracture_render::{ChunkRenderMesh, FractureSource, RenderSettings};
use crate::fracture_render::{RenderBuilder, SourceCorners, derive_render_mesh};

/// Version of the `.fracture.ron` shape ([`FractureSet`]). v2 added [`FractureChunk::render`]
/// (ADR-1967); v1 files load through [`migrate_fracture`].
pub const FRACTURE_FORMAT_VERSION: u32 = 2;

/// Most chunks one fracture may make.
pub const MAX_CHUNKS: usize = 1024;

/// Most candidates the random sampler tries per site ([`FractureSettings::candidates_per_site`]):
/// beyond it the spread barely improves while the sampling time grows without bound.
pub const MAX_CANDIDATES_PER_SITE: u32 = 1024;

/// Smallest [`FractureSites::Clusters`] `spread`: a clump narrower than this cannot keep its
/// sites apart by the coincidence distance (ADR-7550).
pub const MIN_CLUSTER_SPREAD: f32 = 1e-3;

/// Draws one candidate site may take before sampling gives up.
const DRAWS_PER_CANDIDATE: u32 = 4096;

/// Where the Voronoi sites come from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum FractureSites {
    /// `count` sites inside the solid, spread by best-candidate sampling from `seed`.
    Random {
        /// Number of chunks to ask for (1 to [`MAX_CHUNKS`]).
        count: usize,
        /// Seed of the sampling; the same seed gives the same sites.
        seed: u64,
    },
    /// Explicit sites in the mesh's space; each must be inside the solid and no two may coincide.
    Explicit(Vec<[f32; 3]>),
    /// Impact-biased sites (ADR-9870, ADR-7550): `count` rejection-sampled sites whose density
    /// per unit volume falls off as `1/r^bias` with the distance `r` from `at` (flat within
    /// `r_min`, 1 % of the bounding-box diagonal, of the hit), so the chunks are fine debris at the
    /// hit and grow with distance. `bias` is 0 (even over the solid), 1 or 2 (the strongest); the
    /// three levels keep the sampling free of transcendental functions, so the output is
    /// byte-identical on every platform. `at` is usually on the surface; it need not be inside the
    /// solid, and a site no draw can place inside the solid (a hit far outside it at bias 1 or 2)
    /// is [`FractureError::SiteSampling`].
    Impact {
        /// Number of chunks to ask for (1 to [`MAX_CHUNKS`]).
        count: usize,
        /// Seed of the sampling; the same seed gives the same sites.
        seed: u64,
        /// Where the hit lands (the mesh's space).
        at: [f32; 3],
        /// Falloff level: 0, 1 or 2.
        bias: f32,
    },
    /// Clustered sites (ADR-9870): `clusters` centres are sampled inside the solid and the
    /// `count` sites are spread `spread` (a fraction of the solid's bounding-box half-diagonal,
    /// [`MIN_CLUSTER_SPREAD`] to 1) around them in the order `site k` belongs to cluster `k % clusters`,
    /// so the cut is a few clumps of small chunks between large ones.
    Clusters {
        /// Number of chunks to ask for (1 to [`MAX_CHUNKS`]).
        count: usize,
        /// Seed of the sampling; the same seed gives the same sites.
        seed: u64,
        /// Number of clumps (1 to `count`).
        clusters: usize,
        /// Radius of one clump, as a fraction of the bounding-box half-diagonal
        /// ([`MIN_CLUSTER_SPREAD`] to 1).
        spread: f32,
    },
}

/// How a contact area turns into a joint break force.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JointSettings {
    /// Newtons of break force per square metre of contact.
    pub strength_per_area: f32,
    /// Lower bound of a joint's break force, in newtons. Zero means "no lower bound".
    pub min_break_force: f32,
    /// Upper bound of a joint's break force, in newtons; `None` leaves it unbounded.
    pub max_break_force: Option<f32>,
    /// Contacts smaller than this (square metres) are not joints (chunks that only touch along
    /// an edge or at a point).
    pub min_contact_area: f32,
}

impl Default for JointSettings {
    fn default() -> Self {
        Self {
            strength_per_area: 500_000.0,
            min_break_force: 0.0,
            max_break_force: None,
            min_contact_area: 1e-6,
        }
    }
}

/// Everything that steers one fracture.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FractureSettings {
    /// Where the sites come from.
    pub sites: FractureSites,
    /// Candidates tried per site by the random sampler (more spreads the sites more evenly); 1 to
    /// [`MAX_CANDIDATES_PER_SITE`].
    pub candidates_per_site: u32,
    /// Chunks smaller than this fraction of the solid's volume are dropped (slivers a site
    /// pressed against the surface leaves).
    pub min_chunk_volume_fraction: f32,
    /// Break forces of the joints.
    pub joints: JointSettings,
    /// How the chunks' render attributes are made.
    #[serde(default)]
    pub render: RenderSettings,
}

impl FractureSettings {
    /// `count` random sites from `seed`, with the defaults of everything else.
    #[must_use]
    pub fn random(count: usize, seed: u64) -> Self {
        Self {
            sites: FractureSites::Random { count, seed },
            candidates_per_site: 12,
            min_chunk_volume_fraction: 1e-5,
            joints: JointSettings::default(),
            render: RenderSettings::default(),
        }
    }
}

/// One chunk: a closed triangle mesh in the source mesh's space.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FractureChunk {
    /// The Voronoi site this chunk grew from.
    pub site: [f32; 3],
    /// Vertex positions (welded: a vertex is shared by every triangle that meets there).
    pub positions: Vec<[f32; 3]>,
    /// Triangle indices, counter-clockwise from outside. The first `interior_start` triangles are
    /// pieces of the source surface; the rest are the capped interior faces.
    pub indices: Vec<u32>,
    /// Index of the first interior (cap) triangle (not the index of its first index).
    pub interior_start: u32,
    /// Volume in cubic metres.
    pub volume: f32,
    /// Centre of mass at uniform density.
    pub centroid: [f32; 3],
    /// Bounds minimum.
    pub bounds_min: [f32; 3],
    /// Bounds maximum.
    pub bounds_max: [f32; 3],
    /// Total area of the interior faces, square metres.
    pub interior_area: f32,
    /// What to draw: the same triangles in the same order as `indices`, with vertices split where
    /// normals or UVs differ. Empty only in a v1 text before [`migrate_fracture`].
    #[serde(default)]
    pub render: ChunkRenderMesh,
}

impl FractureChunk {
    /// Triangles in this chunk.
    #[must_use]
    pub fn triangle_count(&self) -> usize {
        self.indices.len() / 3
    }

    /// Triangles that are pieces of the source surface.
    #[must_use]
    pub fn surface_triangle_count(&self) -> usize {
        self.interior_start as usize
    }
}

/// A breakable bond between two touching chunks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FractureJoint {
    /// Lower chunk index.
    pub a: u32,
    /// Higher chunk index.
    pub b: u32,
    /// Area of the shared face, square metres.
    pub contact_area: f32,
    /// Area-weighted centre of the shared face, in the source mesh's space: where to anchor a
    /// joint.
    pub anchor: [f32; 3],
    /// Unit normal of the shared face, pointing from chunk `a` toward chunk `b`.
    pub normal: [f32; 3],
    /// Newtons of constraint force that break the bond (see [`JointSettings`]).
    pub break_force: f32,
}

/// The result of one fracture: the chunks and their joints (the adjacency graph).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FractureSet {
    /// Must be [`FRACTURE_FORMAT_VERSION`].
    pub format_version: u32,
    /// Volume of the source solid, cubic metres.
    pub source_volume: f32,
    /// Chunks in site order (dropped slivers leave no gap).
    pub chunks: Vec<FractureChunk>,
    /// One joint per touching pair, sorted by `(a, b)`.
    pub joints: Vec<FractureJoint>,
}

impl FractureSet {
    /// Chunks that touch `chunk` (ascending), empty for an unknown index.
    #[must_use]
    pub fn neighbors(&self, chunk: u32) -> Vec<u32> {
        let mut out: Vec<u32> = self
            .joints
            .iter()
            .filter_map(|j| {
                if j.a == chunk {
                    Some(j.b)
                } else if j.b == chunk {
                    Some(j.a)
                } else {
                    None
                }
            })
            .collect();
        out.sort_unstable();
        out
    }

    /// The adjacency lists of all chunks.
    #[must_use]
    pub fn adjacency(&self) -> Vec<Vec<u32>> {
        let mut lists = vec![Vec::new(); self.chunks.len()];
        for j in &self.joints {
            if let Some(l) = lists.get_mut(j.a as usize) {
                l.push(j.b);
            }
            if let Some(l) = lists.get_mut(j.b as usize) {
                l.push(j.a);
            }
        }
        for l in &mut lists {
            l.sort_unstable();
        }
        lists
    }

    /// Sum of the chunk volumes.
    #[must_use]
    pub fn total_chunk_volume(&self) -> f64 {
        self.chunks.iter().map(|c| f64::from(c.volume)).sum()
    }

    /// Writes the set as RON.
    ///
    /// # Errors
    /// [`FractureError::Format`] when serialization fails.
    pub fn to_ron(&self) -> Result<String, FractureError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| FractureError::Format(e.to_string()))
    }

    /// Reads a set from RON, bringing an older version up to [`FRACTURE_FORMAT_VERSION`], and
    /// checks that every chunk number is finite (a non-finite volume would become an infinite
    /// rigid-body mass in [`Self::chunk_descs`]) and that every render mesh is well formed.
    ///
    /// # Errors
    /// [`FractureError::Format`] for malformed text, a non-finite chunk value or a broken render
    /// mesh, [`FractureError::UnsupportedVersion`] for a version with no migrator.
    pub fn from_ron(text: &str) -> Result<Self, FractureError> {
        let (version, parsed) = Self::parse_with_version(text)?;
        Self::finish_read(version, parsed)
    }

    /// The typed parse of `text` and the format version it names (ADR-7703). The typed parse runs
    /// first and carries the version; only a text it refuses is peeked, so a read answers exactly as
    /// a peek-first read did (the peek's error for text with no readable version, else the typed
    /// parse's error). `peek_ron_version` parses the whole text into a `ron::Value`: on a 40-chunk
    /// set (3.6 MB) that took 193 s in a debug build where the typed parse took 74 ms.
    fn parse_with_version(text: &str) -> Result<(u32, Result<Self, String>), FractureError> {
        match ron::from_str::<Self>(text) {
            Ok(set) => Ok((set.format_version, Ok(set))),
            Err(e) => {
                let version = regia_core::version::peek_ron_version(text).map_err(FractureError::Format)?;
                Ok((version, Err(e.to_string())))
            }
        }
    }

    /// The rest of [`Self::from_ron`] after [`Self::parse_with_version`]: the upgrade and the checks.
    fn finish_read(version: u32, parsed: Result<Self, String>) -> Result<Self, FractureError> {
        let mut set = parsed.map_err(FractureError::Format)?;
        if version == 1 {
            set.upgrade_v1()?;
        } else if version != FRACTURE_FORMAT_VERSION {
            return Err(FractureError::UnsupportedVersion {
                found: version,
                expected: FRACTURE_FORMAT_VERSION,
            });
        }
        set.check_finite()?;
        set.check_render()?;
        Ok(set)
    }

    /// v1 to v2: every chunk gets a render mesh made from its welded mesh alone (the source mesh
    /// is gone: crease-smoothed normals and box-projected UVs on the surface pieces).
    fn upgrade_v1(&mut self) -> Result<(), FractureError> {
        let settings = RenderSettings::default();
        for (k, chunk) in self.chunks.iter_mut().enumerate() {
            chunk.render = derive_render_mesh(k, chunk, &settings)?;
        }
        self.format_version = FRACTURE_FORMAT_VERSION;
        Ok(())
    }

    /// Every render mesh has matching array lengths, indices in range, finite attributes and
    /// the triangle count of its chunk.
    fn check_render(&self) -> Result<(), FractureError> {
        for (k, c) in self.chunks.iter().enumerate() {
            c.render
                .check(c.indices.len(), c.interior_start)
                .map_err(|why| FractureError::Format(format!("chunk {k} render mesh: {why}")))?;
        }
        Ok(())
    }

    /// Every volume, area and point of the chunks is finite.
    fn check_finite(&self) -> Result<(), FractureError> {
        let bad = |what: String| Err(FractureError::Format(format!("{what} is not finite")));
        if !self.source_volume.is_finite() {
            return bad("source_volume".into());
        }
        for (k, c) in self.chunks.iter().enumerate() {
            let points = [c.site, c.centroid, c.bounds_min, c.bounds_max];
            if !c.volume.is_finite() {
                return bad(format!("chunk {k} volume"));
            }
            if !c.interior_area.is_finite() {
                return bad(format!("chunk {k} interior_area"));
            }
            if !points.iter().flatten().all(|v| v.is_finite()) {
                return bad(format!("chunk {k} site, centroid or bounds"));
            }
            if let Some(v) = c.positions.iter().position(|p| !p.iter().all(|v| v.is_finite())) {
                return bad(format!("chunk {k} position {v}"));
            }
        }
        Ok(())
    }

    /// Physics chunk descriptions for `regia_physics::DebrisSpawner`: each chunk as a convex hull
    /// of its vertices about its centroid, with mass `volume * density` (kg for `density` in
    /// kg/m^3). The hull of a concave chunk is a conservative approximation.
    #[must_use]
    pub fn chunk_descs(&self, density: f32) -> Vec<regia_physics::ChunkDesc> {
        use regia_core::math::{Quat, Vec3};
        self.chunks
            .iter()
            .map(|c| {
                let o = Vec3::from_array(c.centroid);
                regia_physics::ChunkDesc {
                    local_translation: o,
                    local_rotation: Quat::IDENTITY,
                    shape: regia_physics::ColliderShape::ConvexHull {
                        points: c.positions.iter().map(|p| Vec3::from_array(*p) - o).collect(),
                    },
                    mass: c.volume * density,
                }
            })
            .collect()
    }
}

/// Why a fracture failed. [`FractureError::code`] is the agent-protocol error code.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum FractureError {
    /// The mesh has no triangles.
    #[error("the mesh has no triangles")]
    EmptyMesh,
    /// The index count is not a multiple of three.
    #[error("the index count {0} is not a multiple of 3")]
    BadIndexCount(usize),
    /// An index points past the vertex list.
    #[error("index {index} is out of range for {vertices} vertices")]
    IndexOutOfRange {
        /// The offending index.
        index: u32,
        /// Vertex count.
        vertices: usize,
    },
    /// A position is NaN or infinite.
    #[error("vertex {0} is not finite")]
    NonFinite(usize),
    /// A triangle uses one vertex twice.
    #[error("triangle {0} repeats a vertex")]
    DegenerateTriangle(usize),
    /// An edge is used by more than two triangles or twice in one direction.
    #[error("the mesh is not a manifold (edge shared inconsistently)")]
    NotManifold,
    /// The mesh has holes (an edge with no partner).
    #[error("the mesh is not closed")]
    NotClosed,
    /// The triangles wind clockwise from outside (negative volume).
    #[error("the mesh is inside out (negative volume)")]
    InsideOut,
    /// The mesh encloses no volume.
    #[error("the mesh encloses no volume")]
    ZeroVolume,
    /// The chunk count or a setting is out of range.
    #[error("invalid settings: {0}")]
    InvalidSettings(String),
    /// The sampler found no point inside the solid.
    #[error("could not place site {0} inside the solid")]
    SiteSampling(usize),
    /// An explicit site is not inside the solid.
    #[error("site {0} is outside the solid")]
    SiteOutside(usize),
    /// Two explicit sites coincide.
    #[error("sites {0} and {1} coincide")]
    DuplicateSites(usize, usize),
    /// Capping a cut failed (the clipped surface did not close).
    #[error("could not cap the cut of chunk {0}")]
    CapFailed(usize),
    /// Serialization or parsing failed.
    #[error("fracture file: {0}")]
    Format(String),
    /// A source attribute array does not match the mesh, or holds a non-finite value.
    #[error("source {0}")]
    BadAttributes(String),
    /// The tangents of a chunk could not be generated.
    #[error("tangents of chunk {0}: {1}")]
    Tangents(usize, String),
    /// A `.fracture.ron` with another `format_version`.
    #[error("fracture format_version {found} is not supported (expected {expected})")]
    UnsupportedVersion {
        /// Version found.
        found: u32,
        /// Version supported.
        expected: u32,
    },
}

impl FractureError {
    /// The agent-protocol error code: lowercase ASCII, digits and `_`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::EmptyMesh => "fracture_empty_mesh",
            Self::BadIndexCount(_) => "fracture_bad_index_count",
            Self::IndexOutOfRange { .. } => "fracture_index_out_of_range",
            Self::NonFinite(_) => "fracture_non_finite",
            Self::DegenerateTriangle(_) => "fracture_degenerate_triangle",
            Self::NotManifold => "fracture_not_manifold",
            Self::NotClosed => "fracture_not_closed",
            Self::InsideOut => "fracture_inside_out",
            Self::ZeroVolume => "fracture_zero_volume",
            Self::InvalidSettings(_) => "fracture_invalid_settings",
            Self::SiteSampling(_) => "fracture_site_sampling",
            Self::SiteOutside(_) => "fracture_site_outside",
            Self::DuplicateSites(..) => "fracture_duplicate_sites",
            Self::CapFailed(_) => "fracture_cap_failed",
            Self::Format(_) => "fracture_format",
            Self::BadAttributes(_) => "fracture_bad_attributes",
            Self::Tangents(..) => "fracture_tangents",
            Self::UnsupportedVersion { .. } => "fracture_unsupported_version",
        }
    }
}

/// SplitMix64 (Steele, Lea and Flood, OOPSLA 2014): a small, seedable generator that is the same
/// on every platform.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / 9_007_199_254_740_992.0)
    }
}

/// The checked, welded source solid.
pub(crate) struct Solid {
    pub(crate) tris: Vec<Tri>,
    /// Welded vertex ids per triangle (the source's index order).
    pub(crate) welded: Vec<[u32; 3]>,
    volume: f64,
    min: P,
    max: P,
}

fn build_solid(positions: &[[f32; 3]], indices: &[u32]) -> Result<Solid, FractureError> {
    if indices.is_empty() {
        return Err(FractureError::EmptyMesh);
    }
    if !indices.len().is_multiple_of(3) {
        return Err(FractureError::BadIndexCount(indices.len()));
    }
    for (i, p) in positions.iter().enumerate() {
        if !p.iter().all(|c| c.is_finite()) {
            return Err(FractureError::NonFinite(i));
        }
    }
    for &i in indices {
        if i as usize >= positions.len() {
            return Err(FractureError::IndexOutOfRange {
                index: i,
                vertices: positions.len(),
            });
        }
    }
    // Weld bit-exactly so a UV or normal seam does not open the surface.
    let mut welded: HashMap<[u32; 3], u32> = HashMap::new();
    let mut remap: Vec<u32> = Vec::with_capacity(positions.len());
    for p in positions {
        let k = p.map(|c| if c == 0.0 { 0 } else { c.to_bits() });
        let next = welded.len() as u32;
        remap.push(*welded.entry(k).or_insert(next));
    }
    let mut edges: HashMap<(u32, u32), u32> = HashMap::new();
    let mut welded_tris: Vec<[u32; 3]> = Vec::with_capacity(indices.len() / 3);
    for (t, tri) in indices.chunks(3).enumerate() {
        let w = [
            remap[tri[0] as usize],
            remap[tri[1] as usize],
            remap[tri[2] as usize],
        ];
        if w[0] == w[1] || w[1] == w[2] || w[0] == w[2] {
            return Err(FractureError::DegenerateTriangle(t));
        }
        welded_tris.push(w);
        for e in 0..3 {
            *edges.entry((w[e], w[(e + 1) % 3])).or_insert(0) += 1;
        }
    }
    // Sorted, not hash order: a mesh with both a doubled edge and a hole names the same error
    // every run.
    let mut sorted: Vec<((u32, u32), u32)> = edges.iter().map(|(&e, &n)| (e, n)).collect();
    sorted.sort_unstable();
    for ((a, b), n) in sorted {
        if n > 1 {
            return Err(FractureError::NotManifold);
        }
        if !edges.contains_key(&(b, a)) {
            return Err(FractureError::NotClosed);
        }
    }
    let to_p = |i: u32| {
        let p = positions[i as usize];
        [f64::from(p[0]), f64::from(p[1]), f64::from(p[2])]
    };
    let tris: Vec<Tri> = indices
        .chunks(3)
        .enumerate()
        .map(|(i, t)| Tri {
            v: [to_p(t[0]), to_p(t[1]), to_p(t[2])],
            tag: SURFACE,
            src: i as u32,
        })
        .collect();
    let volume = tris.iter().map(Tri::volume6).sum::<f64>() / 6.0;
    if volume < 0.0 {
        return Err(FractureError::InsideOut);
    }
    // "No volume" is judged against the size of the terms the volume was summed from: their
    // rounding error scales with that sum, so a flat sliver pair (terms that cancel) is refused
    // while a long thin rod (terms that add up) is kept. A threshold from the cube of the
    // bounding-box diagonal refused valid long thin solids (audit ext-b Medium).
    let magnitude = tris.iter().map(|t| t.volume6().abs()).sum::<f64>() / 6.0;
    if volume <= 1e-9 * magnitude {
        return Err(FractureError::ZeroVolume);
    }
    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    for t in &tris {
        for v in &t.v {
            for a in 0..3 {
                min[a] = min[a].min(v[a]);
                max[a] = max[a].max(v[a]);
            }
        }
    }
    Ok(Solid {
        tris,
        welded: welded_tris,
        volume,
        min,
        max,
    })
}

/// Whether `p` is inside the closed solid: ray parity along three fixed directions, majority vote
/// (a ray through an edge or vertex can miscount; the vote outvotes it).
fn inside(tris: &[Tri], p: P) -> bool {
    const DIRS: [P; 3] = [
        [0.577_350_269, 0.317_136_221, 0.753_218_904],
        [-0.273_604_195, 0.912_345_67, -0.304_882_113],
        [0.134_902_311, -0.427_813_902, 0.893_271_445],
    ];
    let votes = DIRS
        .iter()
        .filter(|d| {
            let n = length(**d);
            let d = scale(**d, 1.0 / n);
            tris.iter().filter(|t| ray_hits(t, p, d)).count() % 2 == 1
        })
        .count();
    votes >= 2
}

/// Moller-Trumbore ray/triangle test (Moller and Trumbore, "Fast, minimum storage ray-triangle
/// intersection", JGT 2(1), 1997), forward hits only.
fn ray_hits(t: &Tri, o: P, d: P) -> bool {
    let e1 = sub(t.v[1], t.v[0]);
    let e2 = sub(t.v[2], t.v[0]);
    let h = cross(d, e2);
    let a = dot(e1, h);
    if a.abs() < 1e-18 {
        return false;
    }
    let f = 1.0 / a;
    let s = sub(o, t.v[0]);
    let u = f * dot(s, h);
    if !(0.0..=1.0).contains(&u) {
        return false;
    }
    let q = cross(s, e1);
    let v = f * dot(d, q);
    if v < 0.0 || u + v > 1.0 {
        return false;
    }
    f * dot(e2, q) > 1e-12
}

/// A direction uniform on the unit sphere: points of the unit cube are rejected until one falls
/// inside the unit ball, then normalised (rejection sampling, von Neumann 1951; the cube and the
/// ball agree in direction distribution). `None` when 32 draws all missed the ball or fell on its
/// centre (probability below 1e-10): the caller redraws.
fn unit_direction(rng: &mut Rng) -> Option<P> {
    for _ in 0..32 {
        let d = [
            2.0 * rng.unit() - 1.0,
            2.0 * rng.unit() - 1.0,
            2.0 * rng.unit() - 1.0,
        ];
        let l = length(d);
        if l > 0.0 && l <= 1.0 {
            return Some(scale(d, 1.0 / l));
        }
    }
    None
}

/// Whether `p` is within `min_sep` of an existing site (the same separation [`FractureSites::Explicit`]
/// refuses as coincident).
fn too_close(pts: &[P], p: P, min_sep: f64) -> bool {
    pts.iter().any(|q| length(sub(*q, p)) <= min_sep)
}

/// The count check every seeded sampler shares.
fn check_count(count: usize) -> Result<(), FractureError> {
    if count == 0 || count > MAX_CHUNKS {
        return Err(FractureError::InvalidSettings(format!(
            "chunk count {count} (1 to {MAX_CHUNKS})"
        )));
    }
    Ok(())
}

/// Draws one point uniform over the solid's bounding box and keeps it when inside the solid.
fn draw_in_solid(rng: &mut Rng, solid: &Solid) -> Option<P> {
    let p = [
        solid.min[0] + rng.unit() * (solid.max[0] - solid.min[0]),
        solid.min[1] + rng.unit() * (solid.max[1] - solid.min[1]),
        solid.min[2] + rng.unit() * (solid.max[2] - solid.min[2]),
    ];
    inside(&solid.tris, p).then_some(p)
}

/// The impact law's flat core, as a fraction of the bounding-box diagonal (ADR-7550).
const IMPACT_FLOOR: f64 = 0.01;

/// Whether `p` is inside the solid's bounding box (a cheap test before `inside`).
fn in_box(solid: &Solid, p: P) -> bool {
    (0..3).all(|a| p[a] >= solid.min[a] && p[a] <= solid.max[a])
}

/// One proposal of the impact sampler (ADR-7550): a point of the solid's bounding box whose
/// density per unit volume is proportional to `w(r) = min(1, (r_min / r)^bias)`, `r` its distance
/// from `at`, or `None` when the draw is rejected. Bias 0 and 1 draw uniformly in the box and keep
/// a point with probability `w(r)` (one multiply and compare for bias 1), so a thin solid hit at
/// a corner is filled as easily as a cube; bias 2, whose weight would reject almost every
/// uniform draw, draws the radius linearly in the ball of radius `reach` around `at` (density
/// `1/r^2` per unit volume: exactly the law beyond `r_min`) and evens the core out by keeping a
/// point inside `r_min` with probability `(r / r_min)^2`. Only +, -, *, / and sqrt: exact IEEE
/// operations, so the output is byte-identical on every platform.
fn impact_draw(rng: &mut Rng, solid: &Solid, at: P, bias: f32, r_min: f64, reach: f64) -> Option<P> {
    if bias < 2.0 {
        let p = [
            solid.min[0] + rng.unit() * (solid.max[0] - solid.min[0]),
            solid.min[1] + rng.unit() * (solid.max[1] - solid.min[1]),
            solid.min[2] + rng.unit() * (solid.max[2] - solid.min[2]),
        ];
        if bias == 1.0 {
            // Keep with probability r_min / r (1 inside the core).
            let r = length(sub(p, at));
            if rng.unit() * r > r_min {
                return None;
            }
        }
        return Some(p);
    }
    let t = rng.unit();
    let r = t * reach;
    if r < r_min && rng.unit() * (r_min * r_min) > r * r {
        return None;
    }
    let dir = unit_direction(rng)?;
    let p = add(at, scale(dir, r));
    in_box(solid, p).then_some(p)
}

fn place_sites(solid: &Solid, sites: &FractureSites, candidates: u32) -> Result<Vec<P>, FractureError> {
    let diag = length(sub(solid.max, solid.min));
    match sites {
        FractureSites::Explicit(list) => {
            if list.is_empty() || list.len() > MAX_CHUNKS {
                return Err(FractureError::InvalidSettings(format!(
                    "{} explicit sites (1 to {MAX_CHUNKS})",
                    list.len()
                )));
            }
            let pts: Vec<P> = list
                .iter()
                .map(|p| [f64::from(p[0]), f64::from(p[1]), f64::from(p[2])])
                .collect();
            for (i, p) in pts.iter().enumerate() {
                if !p.iter().all(|c| c.is_finite()) {
                    return Err(FractureError::NonFinite(i));
                }
                if !inside(&solid.tris, *p) {
                    return Err(FractureError::SiteOutside(i));
                }
                for (j, q) in pts.iter().enumerate().take(i) {
                    if length(sub(*p, *q)) <= 1e-6 * diag {
                        return Err(FractureError::DuplicateSites(j, i));
                    }
                }
            }
            Ok(pts)
        }
        FractureSites::Random { count, seed } => {
            check_count(*count)?;
            let mut rng = Rng(*seed);
            let mut draw = |index: usize| -> Result<P, FractureError> {
                for _ in 0..DRAWS_PER_CANDIDATE {
                    let p = [
                        solid.min[0] + rng.unit() * (solid.max[0] - solid.min[0]),
                        solid.min[1] + rng.unit() * (solid.max[1] - solid.min[1]),
                        solid.min[2] + rng.unit() * (solid.max[2] - solid.min[2]),
                    ];
                    if inside(&solid.tris, p) {
                        return Ok(p);
                    }
                }
                Err(FractureError::SiteSampling(index))
            };
            let mut pts: Vec<P> = Vec::with_capacity(*count);
            pts.push(draw(0)?);
            while pts.len() < *count {
                let mut best: Option<(P, f64)> = None;
                for _ in 0..candidates {
                    let c = draw(pts.len())?;
                    let nearest = pts
                        .iter()
                        .map(|q| {
                            let d = sub(c, *q);
                            dot(d, d)
                        })
                        .fold(f64::INFINITY, f64::min);
                    if best.is_none_or(|(_, b)| nearest > b) {
                        best = Some((c, nearest));
                    }
                }
                let Some((c, _)) = best else {
                    return Err(FractureError::SiteSampling(pts.len()));
                };
                pts.push(c);
            }
            Ok(pts)
        }
        FractureSites::Impact {
            count,
            seed,
            at,
            bias,
        } => {
            check_count(*count)?;
            let bad = |what: &str| FractureError::InvalidSettings(what.to_string());
            if !at.iter().all(|c| c.is_finite()) {
                return Err(bad("`at` must be finite"));
            }
            if ![0.0, 1.0, 2.0].contains(bias) {
                return Err(bad("bias must be 0, 1 or 2"));
            }
            let at = at.map(f64::from);
            // The law's floor: inside `r_min` of the hit the density is flat, so the weight
            // never divides by a vanishing distance (ADR-7550).
            let r_min = IMPACT_FLOOR * diag;
            // The ball of this radius around `at` covers the whole solid, so no part of it is
            // unreachable; its corners are the farthest points of the bounding box.
            let reach = (0..8)
                .map(|c| {
                    let corner = [
                        if c & 1 == 0 { solid.min[0] } else { solid.max[0] },
                        if c & 2 == 0 { solid.min[1] } else { solid.max[1] },
                        if c & 4 == 0 { solid.min[2] } else { solid.max[2] },
                    ];
                    length(sub(corner, at))
                })
                .fold(0.0, f64::max);
            let mut rng = Rng(*seed);
            let mut pts: Vec<P> = Vec::with_capacity(*count);
            while pts.len() < *count {
                let index = pts.len();
                let mut placed = None;
                for _ in 0..DRAWS_PER_CANDIDATE {
                    let Some(p) = impact_draw(&mut rng, solid, at, *bias, r_min, reach) else {
                        continue;
                    };
                    if !inside(&solid.tris, p) || too_close(&pts, p, 1e-6 * diag) {
                        continue;
                    }
                    placed = Some(p);
                    break;
                }
                let Some(p) = placed else {
                    return Err(FractureError::SiteSampling(index));
                };
                pts.push(p);
            }
            Ok(pts)
        }
        FractureSites::Clusters {
            count,
            seed,
            clusters,
            spread,
        } => {
            check_count(*count)?;
            let bad = |what: &str| FractureError::InvalidSettings(what.to_string());
            if *clusters == 0 || *clusters > *count {
                return Err(bad("clusters must be 1 to the chunk count"));
            }
            if !(spread.is_finite() && *spread >= MIN_CLUSTER_SPREAD && *spread <= 1.0) {
                return Err(bad("spread must be in [0.001, 1]"));
            }
            let radius = f64::from(*spread) * (0.5 * diag);
            let mut rng = Rng(*seed);
            let mut centres: Vec<P> = Vec::with_capacity(*clusters);
            for _ in 0..*clusters {
                let mut centre = None;
                for _ in 0..DRAWS_PER_CANDIDATE {
                    if let Some(p) = draw_in_solid(&mut rng, solid) {
                        centre = Some(p);
                        break;
                    }
                }
                let Some(c) = centre else {
                    return Err(FractureError::SiteSampling(centres.len()));
                };
                centres.push(c);
            }
            let mut pts: Vec<P> = Vec::with_capacity(*count);
            while pts.len() < *count {
                let index = pts.len();
                let c = centres[index % centres.len()];
                let mut placed = None;
                for _ in 0..DRAWS_PER_CANDIDATE {
                    // An offset uniform in the cluster's ball: cube rejection, radius included.
                    let o = [
                        (2.0 * rng.unit() - 1.0) * radius,
                        (2.0 * rng.unit() - 1.0) * radius,
                        (2.0 * rng.unit() - 1.0) * radius,
                    ];
                    if length(o) > radius {
                        continue;
                    }
                    let p = add(c, o);
                    if !inside(&solid.tris, p) || too_close(&pts, p, 1e-6 * diag) {
                        continue;
                    }
                    placed = Some(p);
                    break;
                }
                let Some(p) = placed else {
                    return Err(FractureError::SiteSampling(index));
                };
                pts.push(p);
            }
            Ok(pts)
        }
    }
}

/// One cell before it is welded into a [`FractureChunk`].
struct Cell {
    tris: Vec<Tri>,
    volume: f64,
    /// Per neighbor site: (cap area, area-weighted cap centroid sum).
    contacts: BTreeMap<u32, (f64, P)>,
}

fn build_cell(solid: &Solid, sites: &[P], i: usize, eps: f64) -> Result<Cell, FractureError> {
    let si = sites[i];
    let mut order: Vec<(f64, usize)> = sites
        .iter()
        .enumerate()
        .filter(|(j, _)| *j != i)
        .map(|(j, s)| (length(sub(*s, si)), j))
        .collect();
    order.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    let mut tris = solid.tris.clone();
    for (dist, j) in order {
        let reach = tris
            .iter()
            .flat_map(|t| t.v.iter())
            .map(|v| length(sub(*v, si)))
            .fold(0.0f64, f64::max);
        if 0.5 * dist > reach + eps {
            break;
        }
        let n = scale(sub(sites[j], si), 1.0 / dist);
        let mid = scale(add(si, sites[j]), 0.5);
        let plane = Plane { n, c: dot(n, mid) };
        tris = clip(&tris, &plane, eps);
        if tris.is_empty() {
            break;
        }
        cap(&mut tris, &plane, j as u32).map_err(|e| match e {
            CapError::OpenBoundary | CapError::OrphanHole => FractureError::CapFailed(i),
        })?;
    }
    let volume = tris.iter().map(Tri::volume6).sum::<f64>() / 6.0;
    let mut contacts: BTreeMap<u32, (f64, P)> = BTreeMap::new();
    for t in &tris {
        if t.tag == SURFACE {
            continue;
        }
        let a = t.area();
        let c = scale(add(add(t.v[0], t.v[1]), t.v[2]), 1.0 / 3.0);
        let e = contacts.entry(t.tag).or_insert((0.0, [0.0; 3]));
        e.0 += a;
        e.1 = add(e.1, scale(c, a));
    }
    Ok(Cell {
        tris,
        volume,
        contacts,
    })
}

fn weld_chunk(
    index: usize,
    sites: &[P],
    cell: &Cell,
    corners: &SourceCorners,
    settings: &RenderSettings,
) -> Result<FractureChunk, FractureError> {
    let site = sites[index];
    let mut render = RenderBuilder::default();
    let mut tris: Vec<Tri> = cell.tris.clone();
    // Surface first, then the caps grouped by the neighbor that cut them.
    tris.sort_by_key(|t| (t.tag != SURFACE, t.tag));
    // Weld in f32, the stored precision: two f64 points a rounding apart become one vertex, and
    // the sliver triangles that collapse with them are dropped (their neighbors then meet).
    let mut ids: HashMap<[u32; 3], u32> = HashMap::new();
    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut indices: Vec<u32> = Vec::with_capacity(tris.len() * 3);
    let mut interior_start = 0usize;
    let mut kept = 0usize;
    for t in &tris {
        let mut tri = [0u32; 3];
        for (slot, v) in tri.iter_mut().zip(&t.v) {
            let p = [v[0] as f32, v[1] as f32, v[2] as f32];
            let k = p.map(|c| if c == 0.0 { 0 } else { c.to_bits() });
            *slot = *ids.entry(k).or_insert_with(|| {
                positions.push(p);
                (positions.len() - 1) as u32
            });
        }
        if tri[0] == tri[1] || tri[1] == tri[2] || tri[0] == tri[2] {
            continue;
        }
        indices.extend_from_slice(&tri);
        if t.tag == SURFACE {
            interior_start = kept + 1;
            render.push_surface(t, corners);
        } else {
            let n = sub(sites[t.tag as usize], site);
            render.push_cap(t, scale(n, 1.0 / length(n)), settings.interior_uv_scale);
        }
        kept += 1;
    }
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for p in &positions {
        for a in 0..3 {
            min[a] = min[a].min(p[a]);
            max[a] = max[a].max(p[a]);
        }
    }
    let mut weighted = [0.0f64; 3];
    let mut vol6 = 0.0f64;
    for t in &tris {
        let w = t.volume6();
        vol6 += w;
        let c = scale(add(add(t.v[0], t.v[1]), t.v[2]), 0.25);
        weighted = add(weighted, scale(c, w));
    }
    let centroid = if vol6.abs() > 0.0 {
        scale(weighted, 1.0 / vol6)
    } else {
        site
    };
    let interior_area: f64 = tris.iter().filter(|t| t.tag != SURFACE).map(Tri::area).sum();
    let render = render
        .finish(interior_start as u32)
        .map_err(|e| FractureError::Tangents(index, e.to_string()))?;
    Ok(FractureChunk {
        site: site.map(|c| c as f32),
        positions,
        indices,
        interior_start: interior_start as u32,
        volume: cell.volume as f32,
        centroid: centroid.map(|c| c as f32),
        bounds_min: min,
        bounds_max: max,
        interior_area: interior_area as f32,
        render,
    })
}

fn check_settings(settings: &FractureSettings) -> Result<(), FractureError> {
    let bad = |what: &str| Err(FractureError::InvalidSettings(what.to_string()));
    if settings.candidates_per_site == 0 || settings.candidates_per_site > MAX_CANDIDATES_PER_SITE {
        return Err(FractureError::InvalidSettings(format!(
            "candidates_per_site must be 1 to {MAX_CANDIDATES_PER_SITE}"
        )));
    }
    let j = &settings.joints;
    let f = settings.min_chunk_volume_fraction;
    if !(0.0..1.0).contains(&f) {
        return bad("min_chunk_volume_fraction must be in [0, 1)");
    }
    if !(j.strength_per_area.is_finite() && j.strength_per_area >= 0.0) {
        return bad("strength_per_area must be finite and not negative");
    }
    if !(j.min_break_force.is_finite() && j.min_break_force >= 0.0) {
        return bad("min_break_force must be finite and not negative");
    }
    if !(j.min_contact_area.is_finite() && j.min_contact_area >= 0.0) {
        return bad("min_contact_area must be finite and not negative");
    }
    if let Some(max) = j.max_break_force
        && !(max.is_finite() && max >= j.min_break_force)
    {
        return bad("max_break_force must be finite and not below min_break_force");
    }
    settings.render.check().map_err(FractureError::InvalidSettings)
}

/// Fractures a closed triangle mesh into Voronoi chunks.
///
/// `positions` and `indices` describe a closed, outward-facing (counter-clockwise), 2-manifold
/// triangle mesh (positions are welded bit-exactly first). The mesh may be concave. The result
/// is the same for the same inputs. The surface gets generated normals and UVs; give
/// [`fracture_source`] the mesh's own when it has them.
///
/// # Errors
/// A [`FractureError`] for an open, non-manifold, inside-out or empty mesh, bad settings
/// (including a `min_chunk_volume_fraction` that drops every chunk), sites that cannot be placed,
/// or a cut that cannot be capped.
pub fn fracture_mesh(
    positions: &[[f32; 3]],
    indices: &[u32],
    settings: &FractureSettings,
) -> Result<FractureSet, FractureError> {
    fracture_source(
        &FractureSource {
            positions,
            indices,
            normals: None,
            uvs: None,
        },
        settings,
    )
}

/// [`fracture_mesh`] of a mesh with its own vertex attributes: the surface pieces of the chunks
/// carry the source's normals and UVs, interpolated across its triangles.
///
/// # Errors
/// See [`fracture_mesh`], plus [`FractureError::BadAttributes`] for an attribute array that does
/// not match `positions` or holds a non-finite value or a zero normal.
pub fn fracture_source(
    source: &FractureSource<'_>,
    settings: &FractureSettings,
) -> Result<FractureSet, FractureError> {
    let (positions, indices) = (source.positions, source.indices);
    let _span = tracing::info_span!("fracture_mesh", triangles = indices.len() / 3).entered();
    check_settings(settings)?;
    let solid = build_solid(positions, indices)?;
    let corners = SourceCorners::new(source, &solid, &settings.render)?;
    let diag = length(sub(solid.max, solid.min));
    let eps = 1e-9 * diag;
    let sites = place_sites(&solid, &settings.sites, settings.candidates_per_site)?;
    let min_volume = f64::from(settings.min_chunk_volume_fraction) * solid.volume;

    let mut kept: Vec<(usize, FractureChunk, Cell)> = Vec::new();
    for i in 0..sites.len() {
        let cell = build_cell(&solid, &sites, i, eps)?;
        if cell.tris.is_empty() || cell.volume <= min_volume || cell.volume <= 0.0 {
            continue;
        }
        let chunk = weld_chunk(i, &sites, &cell, &corners, &settings.render)?;
        kept.push((i, chunk, cell));
    }
    if kept.is_empty() {
        return Err(FractureError::InvalidSettings(format!(
            "every chunk is below min_chunk_volume_fraction {}",
            settings.min_chunk_volume_fraction
        )));
    }
    let chunk_of: HashMap<u32, u32> = kept
        .iter()
        .enumerate()
        .map(|(c, (site, ..))| (*site as u32, c as u32))
        .collect();

    let jset = &settings.joints;
    // Both sides of a contact are measured; a pair counts if either side saw it.
    type Side = Option<(f64, P)>;
    let mut pairs: BTreeMap<(u32, u32), [Side; 2]> = BTreeMap::new();
    for (ca, (_, _, cell)) in kept.iter().enumerate() {
        for (nb_site, contact) in &cell.contacts {
            let Some(&cb) = chunk_of.get(nb_site) else {
                continue;
            };
            let ca = ca as u32;
            let (pair, slot) = if ca < cb { ((ca, cb), 0) } else { ((cb, ca), 1) };
            pairs.entry(pair).or_insert([None, None])[slot] = Some(*contact);
        }
    }
    let mut joints: Vec<FractureJoint> = Vec::new();
    for ((ca, cb), sides) in pairs {
        let present: Vec<(f64, P)> = sides.iter().flatten().copied().collect();
        let area = present.iter().map(|s| s.0).sum::<f64>() / present.len() as f64;
        if area < f64::from(jset.min_contact_area).max(1e-300) {
            continue;
        }
        // The area-weighted centroid of every side that measured the face (both, when both saw
        // it), so the anchor agrees with the averaged area.
        let (area_sum, csum) = present
            .iter()
            .fold((0.0, [0.0; 3]), |(a, c), (area, sum)| (a + area, add(c, *sum)));
        let anchor = scale(csum, 1.0 / area_sum);
        let (sa, sb) = (kept[ca as usize].0, kept[cb as usize].0);
        let d = sub(sites[sb], sites[sa]);
        let normal = scale(d, 1.0 / length(d));
        let mut force = f64::from(jset.strength_per_area) * area;
        force = force.max(f64::from(jset.min_break_force));
        if let Some(max) = jset.max_break_force {
            force = force.min(f64::from(max));
        }
        joints.push(FractureJoint {
            a: ca,
            b: cb,
            contact_area: area as f32,
            anchor: anchor.map(|c| c as f32),
            normal: normal.map(|c| c as f32),
            break_force: force as f32,
        });
    }
    joints.sort_by_key(|j| (j.a, j.b));
    tracing::debug!(chunks = kept.len(), joints = joints.len(), "fracture done");
    Ok(FractureSet {
        format_version: FRACTURE_FORMAT_VERSION,
        source_volume: solid.volume as f32,
        chunks: kept.into_iter().map(|(_, c, _)| c).collect(),
        joints,
    })
}

/// Brings a `.fracture.ron` text to [`FRACTURE_FORMAT_VERSION`] (`regia upgrade-project`): a v1 set
/// gains its render meshes. The current version comes back unchanged.
///
/// # Errors
/// As [`FractureSet::from_ron`].
pub fn migrate_fracture(text: &str) -> Result<String, FractureError> {
    let (version, parsed) = FractureSet::parse_with_version(text)?;
    if version == FRACTURE_FORMAT_VERSION {
        return Ok(text.to_string());
    }
    FractureSet::finish_read(version, parsed)?.to_ron()
}

/// Versions [`migrate_fracture`] accepts below the current one.
#[must_use]
pub fn fracture_migration_sources() -> Vec<u32> {
    vec![1]
}

/// Serde field-list fingerprint of `.fracture.ron` (for `cargo xtask check-formats`).
#[must_use]
pub fn fracture_schema_fingerprint() -> String {
    use regia_core::schema::serde_field_names;
    regia_core::schema::join_field_lists(&[
        serde_field_names::<FractureSet>(),
        serde_field_names::<FractureChunk>(),
        serde_field_names::<ChunkRenderMesh>(),
        serde_field_names::<FractureJoint>(),
    ])
}

/// A closed axis-aligned box mesh `[-h, h]` (12 triangles), for tests, tools and the agent
/// preview. `half` is the half extent on each axis and must be positive.
///
/// # Errors
/// [`FractureError::InvalidSettings`] when an extent is not finite and positive.
pub fn box_mesh(half: [f32; 3]) -> Result<(Vec<[f32; 3]>, Vec<u32>), FractureError> {
    if !half.iter().all(|h| h.is_finite() && *h > 0.0) {
        return Err(FractureError::InvalidSettings(
            "box half extents must be finite and positive".to_string(),
        ));
    }
    let [x, y, z] = half;
    let positions = vec![
        [-x, -y, -z],
        [x, -y, -z],
        [x, y, -z],
        [-x, y, -z],
        [-x, -y, z],
        [x, -y, z],
        [x, y, z],
        [-x, y, z],
    ];
    let indices = vec![
        0, 2, 1, 0, 3, 2, // -z
        4, 5, 6, 4, 6, 7, // +z
        0, 1, 5, 0, 5, 4, // -y
        3, 7, 6, 3, 6, 2, // +y
        0, 4, 7, 0, 7, 3, // -x
        1, 2, 6, 1, 6, 5, // +x
    ];
    Ok((positions, indices))
}

#[cfg(test)]
mod site_sampling_tests {
    use super::{FractureSites, MIN_CLUSTER_SPREAD, P, box_mesh, build_solid, length, place_sites, sub};

    fn impact(count: usize, seed: u64, at: [f32; 3], bias: f32) -> FractureSites {
        FractureSites::Impact {
            count,
            seed,
            at,
            bias,
        }
    }

    /// The review's probes (sprint-rev-claude-60 § 4 finding 1): a 2 m x 2 m x 1 cm pane hit at a
    /// corner, at the middle of an edge and at the centre of its face. Every bias places every
    /// site for ten seeds; the old ball-and-thinning proposal failed the corner at bias 0 and 1
    /// for all ten seeds.
    #[test]
    fn impact_sites_fill_a_thin_pane_hit_at_its_corner_edge_and_face() {
        let (p, i) = box_mesh([1.0, 1.0, 0.005]).expect("pane");
        let solid = build_solid(&p, &i).expect("solid");
        let mut failures = Vec::new();
        for (name, at) in [
            ("corner", [1.0, 1.0, 0.005]),
            ("edge middle", [1.0, 0.0, 0.005]),
            ("face centre", [0.0, 0.0, 0.005]),
        ] {
            for bias in [0.0, 1.0, 2.0] {
                for seed in 0..10 {
                    if let Err(e) = place_sites(&solid, &impact(40, seed, at, bias), 8) {
                        failures.push(format!("{name} bias {bias} seed {seed}: {}", e.code()));
                    }
                }
            }
        }
        assert!(failures.is_empty(), "{} failures: {failures:#?}", failures.len());
    }

    /// The density law the docs state: per unit volume, flat inside `r_min` (1 % of the
    /// bounding-box diagonal) and `(r_min / r)^bias` beyond it. Sites from a hit at the centre of
    /// a 2 m cube, counted inside balls of growing radius (all inside the cube), against the
    /// law's mass fractions. A fixed seed list: the counts are the same on every run.
    #[test]
    fn impact_site_density_follows_the_documented_law() {
        let (p, i) = box_mesh([1.0, 1.0, 1.0]).expect("cube");
        let solid = build_solid(&p, &i).expect("solid");
        let r_min = 0.01 * length(sub(solid.max, solid.min));
        // Mass of the law inside the ball of radius `r` (dropping the common 4*pi).
        let mass = |r: f64, bias: i32| {
            let core = r.min(r_min).powi(3) / 3.0;
            if r <= r_min {
                return core;
            }
            let shell = match bias {
                0 => (r.powi(3) - r_min.powi(3)) / 3.0,
                1 => r_min * (r * r - r_min * r_min) / 2.0,
                _ => r_min * r_min * (r - r_min),
            };
            core + shell
        };
        for bias in [0, 1, 2] {
            let mut sites: Vec<P> = Vec::new();
            for seed in 1..=8 {
                sites.extend(
                    place_sites(&solid, &impact(1024, seed, [0.0; 3], bias as f32), 8).expect("sites"),
                );
            }
            let within = |r: f64| sites.iter().filter(|s| length(**s) < r).count() as f64;
            let n1 = within(1.0);
            for r in [0.25, 0.5, 0.75] {
                let got = within(r) / n1;
                let want = mass(r, bias) / mass(1.0, bias);
                // Five binomial standard deviations: a wrong exponent misses by far more
                // (0.125 / 0.25 / 0.5 at r = 0.5 for bias 0 / 1 / 2).
                let tol = 5.0 * (want * (1.0 - want) / n1).sqrt();
                assert!(
                    (got - want).abs() <= tol,
                    "bias {bias}: fraction within {r} of the sites within 1 is {got}, the law \
                     gives {want} (tolerance {tol}, {n1} sites)"
                );
            }
        }
    }

    /// Clustered spreads below `MIN_CLUSTER_SPREAD` cannot separate their sites (the review's
    /// probe: one clump, spread 1e-7, always failed with `fracture_site_sampling`): they are
    /// refused as settings, and the floor itself samples.
    #[test]
    fn clustered_spreads_below_the_floor_are_refused_and_the_floor_samples() {
        let (p, i) = box_mesh([1.0, 1.0, 1.0]).expect("cube");
        let solid = build_solid(&p, &i).expect("solid");
        let clusters = |spread: f32| FractureSites::Clusters {
            count: 64,
            seed: 3,
            clusters: 1,
            spread,
        };
        for spread in [1e-7, 0.5 * MIN_CLUSTER_SPREAD] {
            let e = place_sites(&solid, &clusters(spread), 8).expect_err("refused");
            assert_eq!(e.code(), "fracture_invalid_settings", "spread {spread}: {e}");
        }
        let sites = place_sites(&solid, &clusters(MIN_CLUSTER_SPREAD), 8).expect("the floor samples");
        assert_eq!(sites.len(), 64);
    }
}
