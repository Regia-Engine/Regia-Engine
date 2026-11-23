// SPDX-License-Identifier: MPL-2.0
//! Robust coincidence diagnostic (CT-05, ADR-8347).
//!
//! An explicit opt-in mode beside the legacy predicate in `geometry.rs`, which stays
//! unchanged. Differences from the legacy predicate:
//!
//! - Strips and fans are expanded with the glTF 2.0 § 3.7.2.1 index formulas
//!   ([`expand_topology`]) instead of being skipped.
//! - Candidates come from a spatial bucket of triangle centroids with a cell edge equal
//!   to the effective tolerance. A triangle is compared with every triangle in its own
//!   and the 26 neighboring cells, so a pair that straddles a cell boundary is still a
//!   candidate. If every matched corner is within the tolerance the centroids are too,
//!   so no coincident pair is missed. Each unordered pair is examined once and counted
//!   against `max_candidate_pairs`; exceeding it stops the operation (no quadratic
//!   runaway, no clean partial report).
//! - Positional agreement (`exact` or `tolerance`) is reported apart from attribute and
//!   deformation equivalence: material, the texture-coordinate sets the material reads,
//!   normals, skin joints and weights, and morph target deltas. Data the linter cannot
//!   read is labeled, never assumed equal.
//! - Non-finite, repeated-index and zero-area triangles become per-primitive evidence
//!   rows and are left out of the comparison instead of failing the whole operation.
//!
//! Nothing is welded or deleted. `safe_delete_candidate` is a label for a reviewer: it
//! is true only for exact positional agreement with equivalent attributes.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// Which coincidence analysis [`super::lint_gltf`] runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoincidenceMode {
    /// The original UV0/anisotropy predicate on triangle lists ([`super::coincident_pairs`]).
    #[default]
    Legacy,
    /// The neighbor-cell, attribute-aware diagnostic of this module.
    Robust,
}

/// Robust-mode tolerances and work budget. Lengths are source-local meters (glTF units,
/// before import scale); the field names carry the unit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RobustGeometrySettings {
    /// Absolute exclusive corner tolerance, in meters.
    pub tolerance_meters: f64,
    /// Relative exclusive corner tolerance, as a fraction of the mesh's bounding-box
    /// diagonal (unitless). The effective tolerance is the larger of the two.
    pub tolerance_relative: f64,
    /// Exclusive mean corner distance for one texture-coordinate set, in UV units.
    pub uv_tolerance: f64,
    /// Inclusive per-component tolerance for normals (unitless), skin weights (unitless)
    /// and morph deltas (meters for positions).
    pub attribute_tolerance: f64,
    /// Triangles with an area at or below this are zero-area evidence, in square meters.
    pub min_area_square_meters: f64,
    /// Maximum unordered candidate pairs examined in one lint operation.
    pub max_candidate_pairs: usize,
    /// Maximum decoded non-POSITION attribute elements (TEXCOORD, NORMAL, JOINTS, WEIGHTS,
    /// morph deltas) in one lint operation, charged before each accessor is read.
    pub max_attribute_elements: usize,
}

impl Default for RobustGeometrySettings {
    fn default() -> Self {
        Self {
            tolerance_meters: 1e-4,
            tolerance_relative: 0.0,
            uv_tolerance: 0.02,
            attribute_tolerance: 1e-5,
            min_area_square_meters: 1e-12,
            max_candidate_pairs: 1_000_000,
            max_attribute_elements: 32_000_000,
        }
    }
}

impl RobustGeometrySettings {
    /// Checks that tolerances are finite and in range and the budget is nonzero.
    ///
    /// # Errors
    /// A tolerance is non-finite or out of range, or `max_candidate_pairs` is zero.
    pub fn validate(&self) -> Result<(), String> {
        let finite = [
            self.tolerance_meters,
            self.tolerance_relative,
            self.uv_tolerance,
            self.attribute_tolerance,
            self.min_area_square_meters,
        ]
        .iter()
        .all(|v| v.is_finite());
        if !finite
            || self.tolerance_meters <= 0.0
            || !(0.0..1.0).contains(&self.tolerance_relative)
            || self.uv_tolerance <= 0.0
            || self.attribute_tolerance < 0.0
            || self.min_area_square_meters < 0.0
            || self.max_candidate_pairs == 0
            || self.max_attribute_elements == 0
        {
            return Err("robust tolerances must be finite; tolerance_meters and uv_tolerance positive; tolerance_relative in [0,1); attribute_tolerance and min_area_square_meters nonnegative; max_candidate_pairs and max_attribute_elements positive".into());
        }
        Ok(())
    }
}

/// Expands glTF triangle topology into triangles, in glTF 2.0 § 3.7.2.1 order.
///
/// Mode 4 (triangles) needs a multiple of three indices. Mode 5 (strip) yields
/// `{v[i], v[i + 1 + i % 2], v[i + 2 - i % 2]}` and mode 6 (fan) `{v[i + 1], v[i + 2], v[0]}`
/// for each `i < n - 2`, so every strip triangle keeps the first triangle's winding.
/// Fewer than three strip or fan indices yield no triangles.
///
/// # Errors
/// Any other mode, or an incomplete triangle list.
pub fn expand_topology(mode: u32, indices: &[u32]) -> Result<Vec<[u32; 3]>, String> {
    let n = indices.len();
    match mode {
        4 => {
            let (triangles, tail) = indices.as_chunks::<3>();
            if !tail.is_empty() {
                return Err("incomplete triangle".into());
            }
            Ok(triangles.to_vec())
        }
        5 => Ok((0..n.saturating_sub(2))
            .map(|i| {
                let odd = i % 2;
                [indices[i], indices[i + 1 + odd], indices[i + 2 - odd]]
            })
            .collect()),
        6 => Ok((0..n.saturating_sub(2))
            .map(|i| [indices[i + 1], indices[i + 2], indices[0]])
            .collect()),
        other => Err(format!("mode {other} is not triangle topology")),
    }
}

/// Number of triangles [`expand_topology`] yields, without allocating.
#[must_use]
pub fn topology_triangle_count(mode: u32, index_count: usize) -> Option<usize> {
    match mode {
        4 => index_count.is_multiple_of(3).then_some(index_count / 3),
        5 | 6 => Some(index_count.saturating_sub(2)),
        _ => None,
    }
}

/// Morph target displacements of one primitive. `None` means the target has no such attribute.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MorphDeltas {
    /// POSITION displacements, in meters.
    pub positions: Option<Vec<[f32; 3]>>,
    /// NORMAL displacements.
    pub normals: Option<Vec<[f32; 3]>>,
    /// TANGENT displacements.
    pub tangents: Option<Vec<[f32; 3]>>,
}

/// One skin attribute set (`JOINTS_n` with `WEIGHTS_n`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SkinSet {
    /// Joint indices per vertex.
    pub joints: Vec<[u16; 4]>,
    /// Weights per vertex.
    pub weights: Vec<[f32; 4]>,
}

/// One primitive's decoded data, owned so callers and tests can build it directly.
/// Every per-vertex list must have `positions.len()` entries.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PrimitiveData {
    /// Source primitive index within the mesh.
    pub primitive: usize,
    /// glTF primitive mode the triangles were expanded from.
    pub mode: u32,
    /// POSITION, in meters.
    pub positions: Vec<[f32; 3]>,
    /// Expanded triangles.
    pub triangles: Vec<[u32; 3]>,
    /// Material index; `None` is the glTF default material.
    pub material: Option<usize>,
    /// Texture-coordinate sets the material samples (`texCoord` of each texture it uses).
    pub material_uv_sets: BTreeSet<u32>,
    /// Labels for material data the linter cannot read, such as `khr_texture_transform_unread`.
    pub material_unread: BTreeSet<String>,
    /// Decoded `TEXCOORD_n` sets this primitive has, keyed by `n`.
    pub tex_coords: BTreeMap<u32, Vec<[f32; 2]>>,
    /// NORMAL, when present.
    pub normals: Option<Vec<[f32; 3]>>,
    /// Skin attribute sets in set order.
    pub skin: Vec<SkinSet>,
    /// Morph targets in target order.
    pub morph_targets: Vec<MorphDeltas>,
}

/// A triangle location: source primitive index and expanded triangle index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TriangleRef {
    /// Source primitive index.
    pub primitive: usize,
    /// Triangle index after strip/fan expansion.
    pub triangle: usize,
}

/// How closely the matched corners agree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Agreement {
    /// Every matched corner has bitwise equal coordinates (`-0` equals `0`).
    Exact,
    /// Every matched corner is within the effective tolerance, but not all are equal.
    Tolerance,
}

/// Whether the pair also agrees in shading and deformation data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Equivalence {
    /// Every compared attribute agrees and nothing was unavailable.
    Equivalent,
    /// At least one attribute differs (`differences`).
    Differs,
    /// Nothing compared differs, but some data was unavailable (`unavailable`).
    Unknown,
}

/// A robust-mode coincident pair within one mesh (possibly across primitives).
///
/// The canonical identity is `fingerprint` with `agreement`, `equivalence`, `differences`
/// and `unavailable`: these do not depend on primitive or triangle order. `keeper` and
/// `suspect` are locations; for two triangles with identical corners the keeper is the
/// earlier location.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RobustCoincidence {
    /// Source mesh index.
    pub mesh: usize,
    /// Reference copy: the lexicographically smaller corner set.
    pub keeper: TriangleRef,
    /// The other copy.
    pub suspect: TriangleRef,
    /// Exact or tolerance-based positional agreement.
    pub agreement: Agreement,
    /// Largest matched-corner distance, in meters.
    pub max_separation_meters: f64,
    /// Effective tolerance used for this mesh, in meters.
    pub tolerance_meters: f64,
    /// Attribute and deformation equivalence.
    pub equivalence: Equivalence,
    /// Differing data: `material`, `texcoord_<n>`, `normal`, `skin`, `morph`.
    pub differences: Vec<String>,
    /// Data that could not be compared: `texcoord_<n>_missing` or a material-unread label.
    pub unavailable: Vec<String>,
    /// Texture-coordinate sets the materials read and that were compared.
    pub uv_sets_compared: Vec<u32>,
    /// Exact agreement with equivalent attributes. A review label; nothing is deleted.
    pub safe_delete_candidate: bool,
    /// BLAKE3 hex of both corner sets, order-independent.
    pub fingerprint: String,
}

/// Shared mesh evidence: triangles left out of the robust comparison, per primitive.
/// Only primitives with at least one such triangle get a row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeshEvidence {
    /// Source mesh index.
    pub mesh: usize,
    /// Source primitive index.
    pub primitive: usize,
    /// glTF primitive mode.
    pub mode: u32,
    /// Triangles after expansion.
    pub triangles: usize,
    /// Triangles with a non-finite corner coordinate.
    pub non_finite: usize,
    /// Triangles that repeat a vertex index (including strip stitching).
    pub repeated_index: usize,
    /// Finite triangles with distinct indices whose area is at or below the floor.
    pub zero_area: usize,
    /// First non-finite triangle index.
    pub first_non_finite: Option<usize>,
    /// First repeated-index triangle index.
    pub first_repeated_index: Option<usize>,
    /// First zero-area triangle index.
    pub first_zero_area: Option<usize>,
}

/// Deterministic robust-mode work counters, summed over the operation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RobustWork {
    /// Triangles that entered the spatial bucket.
    pub eligible_triangles: u64,
    /// Occupied bucket cells.
    pub cells: u64,
    /// Unordered candidate pairs examined.
    pub candidate_pairs: u64,
}

/// Robust result for one mesh.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RobustMeshOutcome {
    /// Pairs in canonical order (corner content, then locations).
    pub coincidences: Vec<RobustCoincidence>,
    /// Evidence rows in primitive order.
    pub evidence: Vec<MeshEvidence>,
    /// Work counters for this mesh.
    pub work: RobustWork,
}

/// Why the robust diagnostic stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RobustError {
    /// Malformed input: an index or attribute count is out of range.
    Invalid(String),
    /// The candidate budget ran out after `examined` pairs.
    CandidateBudget {
        /// Pairs examined when the budget was exceeded (`limit + 1`).
        examined: usize,
        /// The budget.
        limit: usize,
    },
}

impl std::fmt::Display for RobustError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) => f.write_str(message),
            Self::CandidateBudget { examined, limit } => {
                write!(f, "max_candidate_pairs {limit} exceeded at {examined}")
            }
        }
    }
}

type Corners = [[u32; 3]; 3];

struct Eligible {
    slot: usize,
    location: TriangleRef,
    corners: [[f64; 3]; 3],
    content: Corners,
    centroid: [f64; 3],
}

fn bits(v: f32) -> u32 {
    if v == 0.0 { 0 } else { v.to_bits() }
}

fn check_counts(data: &PrimitiveData) -> Result<(), RobustError> {
    let n = data.positions.len();
    let bad = |what: &str| {
        Err(RobustError::Invalid(format!(
            "primitive {} {what} count does not match POSITION",
            data.primitive
        )))
    };
    if data.triangles.iter().flatten().any(|&i| i as usize >= n) {
        return Err(RobustError::Invalid(format!(
            "primitive {} triangle index is outside the position accessor",
            data.primitive
        )));
    }
    if data.tex_coords.values().any(|uv| uv.len() != n) {
        return bad("TEXCOORD");
    }
    if data.normals.as_ref().is_some_and(|v| v.len() != n) {
        return bad("NORMAL");
    }
    if data
        .skin
        .iter()
        .any(|set| set.joints.len() != n || set.weights.len() != n)
    {
        return bad("JOINTS/WEIGHTS");
    }
    if data.morph_targets.iter().any(|t| {
        [&t.positions, &t.normals, &t.tangents]
            .iter()
            .any(|v| v.as_ref().is_some_and(|v| v.len() != n))
    }) {
        return bad("morph target");
    }
    Ok(())
}

fn area(c: &[[f64; 3]; 3]) -> f64 {
    let u = [c[1][0] - c[0][0], c[1][1] - c[0][1], c[1][2] - c[0][2]];
    let v = [c[2][0] - c[0][0], c[2][1] - c[0][1], c[2][2] - c[0][2]];
    let x = u[1] * v[2] - u[2] * v[1];
    let y = u[2] * v[0] - u[0] * v[2];
    let z = u[0] * v[1] - u[1] * v[0];
    0.5 * (x * x + y * y + z * z).sqrt()
}

fn distance3(a: [f64; 3], b: [f64; 3]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

const PERMUTATIONS: [[usize; 3]; 6] = [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]];

/// Robust coincidence and mesh evidence for one mesh's triangle primitives.
///
/// `used` is the operation's candidate counter; it is charged and checked per pair so a
/// dense mesh stops at `max_candidate_pairs + 1` examined pairs.
///
/// # Errors
/// [`RobustError::Invalid`] for out-of-range indices or attribute counts, and
/// [`RobustError::CandidateBudget`] when the candidate budget is exceeded.
pub fn robust_mesh_coincidences(
    mesh: usize,
    primitives: &[PrimitiveData],
    settings: &RobustGeometrySettings,
    used: &mut usize,
) -> Result<RobustMeshOutcome, RobustError> {
    settings.validate().map_err(RobustError::Invalid)?;
    let mut outcome = RobustMeshOutcome::default();
    let mut eligible = Vec::new();
    let mut lo = [f64::INFINITY; 3];
    let mut hi = [f64::NEG_INFINITY; 3];
    for (slot, data) in primitives.iter().enumerate() {
        check_counts(data)?;
        let mut row = MeshEvidence {
            mesh,
            primitive: data.primitive,
            mode: data.mode,
            triangles: data.triangles.len(),
            non_finite: 0,
            repeated_index: 0,
            zero_area: 0,
            first_non_finite: None,
            first_repeated_index: None,
            first_zero_area: None,
        };
        for (t, tri) in data.triangles.iter().enumerate() {
            let raw = tri.map(|v| data.positions[v as usize]);
            if raw.iter().flatten().any(|v| !v.is_finite()) {
                row.non_finite += 1;
                row.first_non_finite.get_or_insert(t);
                continue;
            }
            if tri[0] == tri[1] || tri[1] == tri[2] || tri[0] == tri[2] {
                row.repeated_index += 1;
                row.first_repeated_index.get_or_insert(t);
                continue;
            }
            let corners = raw.map(|p| p.map(f64::from));
            if area(&corners) <= settings.min_area_square_meters {
                row.zero_area += 1;
                row.first_zero_area.get_or_insert(t);
                continue;
            }
            for c in &corners {
                for (k, &v) in c.iter().enumerate() {
                    lo[k] = lo[k].min(v);
                    hi[k] = hi[k].max(v);
                }
            }
            let mut content = raw.map(|p| p.map(bits));
            content.sort_unstable();
            let centroid = [0, 1, 2].map(|k| (corners[0][k] + corners[1][k] + corners[2][k]) / 3.0);
            eligible.push(Eligible {
                slot,
                location: TriangleRef {
                    primitive: data.primitive,
                    triangle: t,
                },
                corners,
                content,
                centroid,
            });
        }
        if row.non_finite + row.repeated_index + row.zero_area > 0 {
            outcome.evidence.push(row);
        }
    }
    outcome.work.eligible_triangles = eligible.len() as u64;
    if eligible.len() < 2 {
        return Ok(outcome);
    }
    let diagonal = distance3(lo, hi);
    let tolerance = settings
        .tolerance_meters
        .max(settings.tolerance_relative * diagonal);
    // Cell edge = tolerance. `as` saturates far coordinates; that can only add candidates,
    // each of which is verified by distance, so it never invents a match.
    let cell_of = |c: [f64; 3]| c.map(|v| (v / tolerance).floor() as i64);
    let mut cells = BTreeMap::<[i64; 3], Vec<usize>>::new();
    for (i, e) in eligible.iter().enumerate() {
        cells.entry(cell_of(e.centroid)).or_default().push(i);
    }
    outcome.work.cells = cells.len() as u64;
    let mut found = Vec::new();
    for (i, a) in eligible.iter().enumerate() {
        let home = cell_of(a.centroid);
        for dx in -1_i64..=1 {
            for dy in -1_i64..=1 {
                for dz in -1_i64..=1 {
                    let key = [
                        home[0].saturating_add(dx),
                        home[1].saturating_add(dy),
                        home[2].saturating_add(dz),
                    ];
                    // Saturation can fold two offsets into one key; visit each key once.
                    if (dx, dy, dz) != (0, 0, 0) && key == home {
                        continue;
                    }
                    let Some(members) = cells.get(&key) else {
                        continue;
                    };
                    let start = members.partition_point(|&j| j <= i);
                    for &j in &members[start..] {
                        *used += 1;
                        outcome.work.candidate_pairs += 1;
                        if *used > settings.max_candidate_pairs {
                            return Err(RobustError::CandidateBudget {
                                examined: *used,
                                limit: settings.max_candidate_pairs,
                            });
                        }
                        if let Some(row) = compare(mesh, a, &eligible[j], primitives, settings, tolerance) {
                            found.push(row);
                        }
                    }
                }
            }
        }
    }
    found.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.keeper.cmp(&b.1.keeper))
            .then_with(|| a.1.suspect.cmp(&b.1.suspect))
    });
    outcome.coincidences = found.into_iter().map(|(_, row)| row).collect();
    Ok(outcome)
}

fn compare(
    mesh: usize,
    a: &Eligible,
    b: &Eligible,
    primitives: &[PrimitiveData],
    settings: &RobustGeometrySettings,
    tolerance: f64,
) -> Option<((Corners, Corners), RobustCoincidence)> {
    // Orient so the keeper is the smaller corner content, then the earlier location.
    let (keeper, suspect) = if (a.content, a.location) <= (b.content, b.location) {
        (a, b)
    } else {
        (b, a)
    };
    let mut best: Option<(f64, [usize; 3])> = None;
    for perm in PERMUTATIONS {
        let separation = (0..3)
            .map(|k| distance3(keeper.corners[k], suspect.corners[perm[k]]))
            .fold(0.0_f64, f64::max);
        if best.is_none_or(|(d, _)| separation < d) {
            best = Some((separation, perm));
        }
    }
    let (separation, perm) = best?;
    if separation >= tolerance {
        return None;
    }
    let kp = &primitives[keeper.slot];
    let sp = &primitives[suspect.slot];
    let kt = kp.triangles[keeper.location.triangle];
    let st = sp.triangles[suspect.location.triangle];
    let pairs: [(usize, usize); 3] = [0, 1, 2].map(|k| (kt[k] as usize, st[perm[k]] as usize));
    let exact = pairs
        .iter()
        .all(|&(x, y)| kp.positions[x].map(bits) == sp.positions[y].map(bits));
    let mut differences = Vec::new();
    let mut unavailable = BTreeSet::new();
    let mut uv_sets_compared = Vec::new();
    if kp.material != sp.material {
        differences.push("material".to_owned());
    }
    unavailable.extend(kp.material_unread.iter().cloned());
    unavailable.extend(sp.material_unread.iter().cloned());
    let sets: BTreeSet<u32> = kp.material_uv_sets.union(&sp.material_uv_sets).copied().collect();
    for set in sets {
        match (kp.tex_coords.get(&set), sp.tex_coords.get(&set)) {
            (Some(ku), Some(su)) => {
                uv_sets_compared.push(set);
                let mean = pairs
                    .iter()
                    .map(|&(x, y)| {
                        let (p, q) = (ku[x], su[y]);
                        (f64::from(p[0] - q[0]).powi(2) + f64::from(p[1] - q[1]).powi(2)).sqrt()
                    })
                    .sum::<f64>()
                    / 3.0;
                if mean.is_nan() || mean >= settings.uv_tolerance {
                    differences.push(format!("texcoord_{set}"));
                }
            }
            _ => {
                unavailable.insert(format!("texcoord_{set}_missing"));
            }
        }
    }
    let tol = settings.attribute_tolerance;
    let close3 = |p: [f32; 3], q: [f32; 3]| (0..3).all(|k| f64::from((p[k] - q[k]).abs()) <= tol);
    let normals_differ = match (&kp.normals, &sp.normals) {
        (Some(kn), Some(sn)) => pairs.iter().any(|&(x, y)| !close3(kn[x], sn[y])),
        (None, None) => false,
        _ => true,
    };
    if normals_differ {
        differences.push("normal".to_owned());
    }
    let skin_differs = kp.skin.len() != sp.skin.len()
        || kp
            .skin
            .iter()
            .zip(&sp.skin)
            .any(|(ks, ss)| pairs.iter().any(|&(x, y)| !same_influences(ks, x, ss, y, tol)));
    if skin_differs {
        differences.push("skin".to_owned());
    }
    let morph_differs = kp.morph_targets.len() != sp.morph_targets.len()
        || kp.morph_targets.iter().zip(&sp.morph_targets).any(|(kt, st)| {
            [
                (&kt.positions, &st.positions),
                (&kt.normals, &st.normals),
                (&kt.tangents, &st.tangents),
            ]
            .iter()
            .any(|(k, s)| match (k, s) {
                (Some(k), Some(s)) => pairs.iter().any(|&(x, y)| !close3(k[x], s[y])),
                (None, None) => false,
                _ => true,
            })
        });
    if morph_differs {
        differences.push("morph".to_owned());
    }
    let equivalence = if !differences.is_empty() {
        Equivalence::Differs
    } else if !unavailable.is_empty() {
        Equivalence::Unknown
    } else {
        Equivalence::Equivalent
    };
    let agreement = if exact {
        Agreement::Exact
    } else {
        Agreement::Tolerance
    };
    let content_key = (keeper.content, suspect.content);
    let mut hasher = blake3::Hasher::new();
    for corner in content_key.0.iter().chain(content_key.1.iter()) {
        for v in corner {
            hasher.update(&v.to_le_bytes());
        }
    }
    Some((
        content_key,
        RobustCoincidence {
            mesh,
            keeper: keeper.location,
            suspect: suspect.location,
            agreement,
            max_separation_meters: separation,
            tolerance_meters: tolerance,
            equivalence,
            differences,
            unavailable: unavailable.into_iter().collect(),
            uv_sets_compared,
            safe_delete_candidate: agreement == Agreement::Exact && equivalence == Equivalence::Equivalent,
            fingerprint: hasher.finalize().to_hex().to_string(),
        },
    ))
}

/// Influences as sorted (joint, weight) pairs with nonzero weight; order inside the
/// vec4 does not matter.
fn same_influences(a: &SkinSet, x: usize, b: &SkinSet, y: usize, tol: f64) -> bool {
    let collect = |s: &SkinSet, v: usize| {
        let mut list: Vec<(u16, f32)> = s.joints[v]
            .iter()
            .zip(s.weights[v])
            .filter(|(_, w)| *w != 0.0)
            .map(|(&j, w)| (j, w))
            .collect();
        list.sort_by(|p, q| p.0.cmp(&q.0).then(p.1.total_cmp(&q.1)));
        list
    };
    let (p, q) = (collect(a, x), collect(b, y));
    p.len() == q.len()
        && p.iter()
            .zip(&q)
            .all(|(p, q)| p.0 == q.0 && f64::from((p.1 - q.1).abs()) <= tol)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_and_fan_follow_the_gltf_formulas() {
        assert_eq!(
            expand_topology(5, &[0, 1, 2, 3, 4]).expect("strip"),
            vec![[0, 1, 2], [1, 3, 2], [2, 3, 4]]
        );
        assert_eq!(
            expand_topology(6, &[0, 1, 2, 3]).expect("fan"),
            vec![[1, 2, 0], [2, 3, 0]]
        );
        assert!(expand_topology(5, &[0, 1]).expect("short strip").is_empty());
        assert!(expand_topology(4, &[0, 1]).is_err());
        assert!(expand_topology(1, &[0, 1]).is_err());
        assert_eq!(topology_triangle_count(5, 1), Some(0));
        assert_eq!(topology_triangle_count(4, 4), None);
    }

    #[test]
    fn settings_reject_bad_tolerances() {
        assert!(RobustGeometrySettings::default().validate().is_ok());
        for bad in [
            RobustGeometrySettings {
                tolerance_meters: 0.0,
                ..Default::default()
            },
            RobustGeometrySettings {
                tolerance_relative: 1.0,
                ..Default::default()
            },
            RobustGeometrySettings {
                uv_tolerance: f64::NAN,
                ..Default::default()
            },
            RobustGeometrySettings {
                max_candidate_pairs: 0,
                ..Default::default()
            },
        ] {
            assert!(bad.validate().is_err());
        }
    }
}
