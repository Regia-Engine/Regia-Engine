// SPDX-License-Identifier: MPL-2.0
//! Read-only glTF geometry and rig diagnostics (ADR-8068).
//!
//! The coincident predicate is rewritten from DO27 `tools/coincident_tris.py`,
//! lines 1–123, d6ed37677 (regia-absorb-1), under the owner's 2026-11-16 exception.
//! Generic inspection requirements: `art/generators/regia_validate_glb.py`, lines
//! 96–155, same pin. No asset-kind thresholds or game identifiers are used.
//! See `docs/provenance.md` and `book/agents/gltf-lint.md`.
//!
//! Diagnostics use source-local meters, before import scale or axis conversion.
//! They never rewrite geometry. Repeated instances of a mesh are not duplicates.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

mod geometry;
mod rig;
mod robust;
pub use geometry::{CoincidentPair, GeometrySettings, coincident_pairs};
pub use robust::{
    Agreement, CoincidenceMode, Equivalence, MeshEvidence, MorphDeltas, PrimitiveData, RobustCoincidence,
    RobustError, RobustGeometrySettings, RobustMeshOutcome, RobustWork, SkinSet, TriangleRef,
    expand_topology, robust_mesh_coincidences, topology_triangle_count,
};

/// Thresholds and work budgets for one lint operation; no persistent content format.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GltfLintSettings {
    /// Shared encoded, decoded and accessor admission policy.
    pub input_limits: crate::content_lint::ContentLimits,
    /// Coincident-triangle thresholds.
    pub geometry: GeometrySettings,
    /// Coincidence analysis. `legacy` (default) keeps the original predicate; `robust`
    /// runs the neighbor-cell, attribute-aware diagnostic (ADR-8347).
    pub mode: CoincidenceMode,
    /// Robust-mode tolerances and budgets; used only when `mode` is `robust`.
    pub robust: RobustGeometrySettings,
    /// Maximum POSITION elements, summed over primitive uses of accessors.
    pub max_vertices: usize,
    /// Maximum triangles, summed over mesh primitives.
    pub max_triangles: usize,
    /// Maximum animation channels.
    pub max_channels: usize,
    /// Maximum input and output accessor elements summed over channel uses.
    pub max_animation_elements: usize,
    /// Maximum total diagnostic rows; exceeding it is an error, never a clean report.
    pub max_findings: usize,
}

impl Default for GltfLintSettings {
    fn default() -> Self {
        Self {
            input_limits: crate::content_lint::ContentLimits::default(),
            geometry: GeometrySettings::default(),
            mode: CoincidenceMode::Legacy,
            robust: RobustGeometrySettings::default(),
            max_vertices: 4_000_000,
            max_triangles: 2_000_000,
            max_channels: 100_000,
            max_animation_elements: 4_000_000,
            max_findings: 10_000,
        }
    }
}

impl GltfLintSettings {
    /// Validates thresholds and nonzero work budgets.
    ///
    /// # Errors
    /// `gltf_lint_settings` when a threshold or budget is unusable.
    pub fn validate(&self) -> Result<(), GltfLintError> {
        self.geometry.validate().map_err(GltfLintError::Settings)?;
        self.robust.validate().map_err(GltfLintError::Settings)?;
        if [
            self.max_vertices,
            self.max_triangles,
            self.max_channels,
            self.max_animation_elements,
            self.max_findings,
        ]
        .contains(&0)
        {
            return Err(GltfLintError::Settings("work budgets must be positive".into()));
        }
        Ok(())
    }
}

/// A lint operation that could not finish. No partial report is presented as complete.
#[derive(Debug, thiserror::Error)]
pub enum GltfLintError {
    /// Shared loader limit, cancellation, file change or malformed input.
    #[error(transparent)]
    Input(#[from] crate::content_lint::ContentError),
    /// Invalid tolerances or work budgets.
    #[error("{0}")]
    Settings(String),
    /// The checked glTF loader refused the file.
    #[error("{0}")]
    Unreadable(String),
    /// A work or diagnostic budget was exceeded.
    #[error("lint budget exceeded: {0}")]
    Limit(String),
    /// Mesh data cannot be interpreted safely.
    #[error("{0}")]
    Geometry(String),
}

impl GltfLintError {
    /// Stable lowercase error reason for host tools.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Input(error) => error.code(),
            Self::Settings(_) => "gltf_lint_settings",
            Self::Unreadable(_) => "gltf_unreadable",
            Self::Limit(_) => "gltf_lint_limit",
            Self::Geometry(_) => "gltf_lint_geometry",
        }
    }
}

/// Mesh definitions with identical source-local triangle geometry, irrespective of shading.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DuplicateMesh {
    /// Earlier mesh index with this geometry.
    pub original: usize,
    /// Later mesh index with this geometry.
    pub duplicate: usize,
}

/// A coincident pair within one mesh primitive.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CoincidentTriangles {
    /// Source mesh index.
    pub mesh: usize,
    /// Source primitive index.
    pub primitive: usize,
    /// Triangle indices within the primitive, before any cook-time cleanup.
    pub pair: CoincidentPair,
}

/// An animation channel that needs author review.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RigChannelWarning {
    /// Stable lowercase diagnostic code; see the agent guide.
    pub code: String,
    /// Source animation index.
    pub animation: usize,
    /// Source channel index within that animation.
    pub channel: usize,
    /// Target node index.
    pub node: usize,
}

/// A non-triangle-list primitive excluded from geometry comparison.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedPrimitive {
    /// Source mesh index.
    pub mesh: usize,
    /// Source primitive index.
    pub primitive: usize,
    /// glTF primitive mode (0–3, 5 or 6).
    pub mode: u32,
}

/// Deterministically ordered diagnostics. Empty findings mean clean only for the inspected scope.
///
/// Project policy and JSON/SARIF evidence adapt this report in `source_lint` (ADR-8248).
/// Call-local settings stay on [`lint_gltf`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GltfLintReport {
    /// Effective shared input policy for this operation.
    #[serde(default)]
    pub input_limits: crate::content_lint::ContentLimits,
    /// Deterministic loader admission and IO counters.
    #[serde(default)]
    pub input_usage: crate::content_lint::ContentUsage,
    /// Number of mesh definitions examined, including unused definitions.
    pub meshes: usize,
    /// Number of triangle-list primitives examined.
    pub primitives: usize,
    /// Number of triangles examined, including duplicate copies.
    pub triangles: usize,
    /// Redundant geometry definitions, ordered by later mesh index.
    pub duplicate_meshes: Vec<DuplicateMesh>,
    /// Suspect faces, ordered by mesh, primitive and suspect triangle index.
    pub coincident_triangles: Vec<CoincidentTriangles>,
    /// Channel warnings, ordered by animation, channel and code.
    pub rig_channels: Vec<RigChannelWarning>,
    /// Excluded topology, in document order; never silently considered clean.
    pub skipped_primitives: Vec<SkippedPrimitive>,
    /// Coincidence analysis that ran.
    #[serde(default)]
    pub mode: CoincidenceMode,
    /// Robust-mode pairs, ordered by mesh, then canonical corner order. Empty in legacy mode.
    #[serde(default)]
    pub robust_coincidences: Vec<RobustCoincidence>,
    /// Robust-mode non-finite, repeated-index and zero-area evidence, by mesh and primitive.
    #[serde(default)]
    pub mesh_evidence: Vec<MeshEvidence>,
    /// Robust-mode deterministic work counters.
    #[serde(default)]
    pub robust_work: RobustWork,
}

impl GltfLintReport {
    fn check_budget(&self, settings: &GltfLintSettings) -> Result<(), GltfLintError> {
        let count = self
            .duplicate_meshes
            .len()
            .saturating_add(self.coincident_triangles.len())
            .saturating_add(self.rig_channels.len())
            .saturating_add(self.skipped_primitives.len())
            .saturating_add(self.robust_coincidences.len())
            .saturating_add(self.mesh_evidence.len());
        if count > settings.max_findings {
            return Err(GltfLintError::Limit("max_findings".into()));
        }
        Ok(())
    }
}

fn charge(total: &mut usize, amount: usize, limit: usize, name: &str) -> Result<(), GltfLintError> {
    *total = total
        .checked_add(amount)
        .filter(|&v| v <= limit)
        .ok_or_else(|| GltfLintError::Limit(name.into()))?;
    Ok(())
}

type TriangleKey = [[u32; 3]; 3];

/// One triangle-list primitive kept so a later threshold change can rerun geometry checks
/// without opening the glTF again (ADR-8390).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CapturedPrimitive {
    /// Source mesh index.
    pub mesh: usize,
    /// Source primitive index.
    pub primitive: usize,
    /// POSITION components as IEEE-754 bit patterns. `0.0` and `-0.0` are stored as `0`.
    pub positions_bits: Vec<[u32; 3]>,
    /// TEXCOORD_0 components as IEEE-754 bit patterns, when the primitive has them.
    pub uv_bits: Option<Vec<[u32; 2]>>,
    /// Triangle index triples.
    pub triangles: Vec<[u32; 3]>,
}

/// One mesh definition captured with its triangle-list primitives.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CapturedMesh {
    /// Source mesh index.
    pub index: usize,
    /// True when every primitive was a triangle list.
    pub complete: bool,
    /// Triangle-list primitives in document order.
    pub primitives: Vec<CapturedPrimitive>,
}

/// Geometry retained after one source parse.
///
/// Positions are bit patterns, not decimal JSON numbers, so a round trip does not change
/// duplicate detection. Rig rows come from that parse; a threshold change does not walk the
/// animation channels again.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CapturedGeometry {
    /// Meshes in document order.
    pub meshes: Vec<CapturedMesh>,
    /// Non-triangle primitives.
    pub skipped_primitives: Vec<SkippedPrimitive>,
    /// Rig warnings from the parse.
    pub rig_channels: Vec<RigChannelWarning>,
    /// Effective input limits of the parse.
    pub input_limits: crate::content_lint::ContentLimits,
    /// Admission counters of the parse. A cache replay does not add these again.
    pub input_usage: crate::content_lint::ContentUsage,
}

fn position_bits(value: [f32; 3]) -> [u32; 3] {
    value.map(|component| if component == 0.0 { 0 } else { component.to_bits() })
}

fn uv_bits(value: [f32; 2]) -> [u32; 2] {
    value.map(|component| if component == 0.0 { 0 } else { component.to_bits() })
}

fn position_from_bits(bits: [u32; 3]) -> [f32; 3] {
    bits.map(f32::from_bits)
}

fn uv_from_bits(bits: [u32; 2]) -> [f32; 2] {
    bits.map(f32::from_bits)
}

/// Lints a `.gltf` or `.glb` through the importer's checked, normalized buffer loader.
/// Images are not decoded and neither source nor cooked data is written.
///
/// # Errors
/// Invalid settings, malformed mesh/file data, unsupported required extensions, or exhausted budgets.
#[tracing::instrument(skip_all, fields(path = %path.display()))]
pub fn lint_gltf(path: &Path, settings: &GltfLintSettings) -> Result<GltfLintReport, GltfLintError> {
    let mut budget = crate::content_lint::ContentBudget::new(settings.input_limits.clone(), &|| false);
    lint_gltf_bounded(path, settings, &mut budget)
}

/// Lints with an explicit operation budget and cancellation probe.
/// The budget's limits override `settings.input_limits` for this invocation.
///
/// # Errors
/// As `lint_gltf`, including `content_lint_limit` and `content_lint_cancelled`.
pub fn lint_gltf_bounded(
    path: &Path,
    settings: &GltfLintSettings,
    budget: &mut crate::content_lint::ContentBudget<'_>,
) -> Result<GltfLintReport, GltfLintError> {
    Ok(lint_gltf_captured(path, settings, budget)?.0)
}

/// Lints and keeps the decoded primitives for a dependency cache (ADR-8390).
///
/// # Errors
/// As [`lint_gltf_bounded`].
pub fn lint_gltf_captured(
    path: &Path,
    settings: &GltfLintSettings,
    budget: &mut crate::content_lint::ContentBudget<'_>,
) -> Result<(GltfLintReport, CapturedGeometry), GltfLintError> {
    settings.validate()?;
    let (document, buffers) = crate::open_gltf_bounded(path, budget)?;
    let mut report = GltfLintReport {
        input_limits: budget.limits().clone(),
        mode: settings.mode,
        ..Default::default()
    };
    let mut vertices = 0;
    let robust = settings.mode == CoincidenceMode::Robust;
    let mut candidates = 0_usize;
    let mut attribute_elements = 0_usize;
    let unread = unread_material_labels(&document);
    let mut captured_meshes = Vec::new();
    // Exact keys, not hash-only equality: attribute/accessor packing and index order do not matter.
    let mut mesh_keys = BTreeMap::<Vec<TriangleKey>, usize>::new();
    for mesh in document.meshes() {
        budget.checkpoint()?;
        report.meshes += 1;
        let mut mesh_key = Vec::new();
        let mut complete = true;
        let mut robust_primitives = Vec::new();
        let mesh_uv_sets = mesh
            .primitives()
            .flat_map(|p| material_uv_sets(&p.material()))
            .collect::<std::collections::BTreeSet<_>>();
        let mut captured_prims = Vec::new();
        for primitive in mesh.primitives() {
            budget.checkpoint()?;
            let mode = primitive.mode().as_gl_enum();
            if robust && matches!(mode, 4..=6) {
                let context = || format!("mesh {} primitive {}", mesh.index(), primitive.index());
                let data = read_robust_primitive(
                    &primitive,
                    &buffers,
                    &mesh_uv_sets,
                    &unread,
                    settings,
                    (&mut vertices, &mut report.triangles, &mut attribute_elements),
                    &context,
                )?;
                for &triangle in &data.triangles {
                    let mut key = triangle
                        .map(|v| data.positions[v as usize].map(|v| if v == 0.0 { 0 } else { v.to_bits() }));
                    key.sort();
                    mesh_key.push(key);
                }
                robust_primitives.push(data);
                report.primitives += 1;
                continue;
            }
            if primitive.mode() != gltf::mesh::Mode::Triangles {
                complete = false;
                report.skipped_primitives.push(SkippedPrimitive {
                    mesh: mesh.index(),
                    primitive: primitive.index(),
                    mode: primitive.mode().as_gl_enum(),
                });
                report.check_budget(settings)?;
                continue;
            }
            let context = || format!("mesh {} primitive {}", mesh.index(), primitive.index());
            let position_accessor = primitive
                .get(&gltf::Semantic::Positions)
                .ok_or_else(|| GltfLintError::Geometry(format!("{} has no POSITION", context())))?;
            charge(
                &mut vertices,
                position_accessor.count(),
                settings.max_vertices,
                "max_vertices",
            )?;
            let index_count = primitive
                .indices()
                .map_or(position_accessor.count(), |a| a.count());
            if !index_count.is_multiple_of(3) {
                return Err(GltfLintError::Geometry(format!(
                    "{} has an incomplete triangle",
                    context()
                )));
            }
            charge(
                &mut report.triangles,
                index_count / 3,
                settings.max_triangles,
                "max_triangles",
            )?;
            // Check before reading: accessors can alias the same large buffer many times.
            if let Some(uv) = primitive.get(&gltf::Semantic::TexCoords(0))
                && uv.count() != position_accessor.count()
            {
                return Err(GltfLintError::Geometry(format!(
                    "{} has mismatched UV count",
                    context()
                )));
            }
            let reader = primitive.reader(|buffer| buffers.get(buffer.index()).map(|b| b.0.as_slice()));
            let positions: Vec<_> = reader
                .read_positions()
                .ok_or_else(|| GltfLintError::Geometry(format!("{} has unreadable positions", context())))?
                .collect();
            let uvs = reader
                .read_tex_coords(0)
                .map(|v| v.into_f32().collect::<Vec<_>>());
            let indices = match reader.read_indices() {
                Some(v) => v.into_u32().collect::<Vec<_>>(),
                None if primitive.indices().is_none() => {
                    let count = u32::try_from(positions.len())
                        .map_err(|_| GltfLintError::Limit("vertex index range".into()))?;
                    (0..count).collect()
                }
                None => {
                    return Err(GltfLintError::Geometry(format!(
                        "{} has unreadable indices",
                        context()
                    )));
                }
            };
            let (triangles, tail) = indices.as_chunks::<3>();
            if !tail.is_empty() {
                return Err(GltfLintError::Geometry(format!(
                    "{} has an incomplete triangle",
                    context()
                )));
            }
            let pairs = coincident_pairs(&positions, uvs.as_deref(), triangles, &settings.geometry)
                .map_err(|e| GltfLintError::Geometry(format!("{}: {e}", context())))?;
            for pair in pairs {
                report.coincident_triangles.push(CoincidentTriangles {
                    mesh: mesh.index(),
                    primitive: primitive.index(),
                    pair,
                });
                report.check_budget(settings)?;
            }
            for &triangle in triangles {
                let mut key = triangle.map(|v| position_bits(positions[v as usize]));
                key.sort();
                mesh_key.push(key);
            }
            captured_prims.push(CapturedPrimitive {
                mesh: mesh.index(),
                primitive: primitive.index(),
                positions_bits: positions.iter().copied().map(position_bits).collect(),
                uv_bits: uvs
                    .as_ref()
                    .map(|rows| rows.iter().copied().map(uv_bits).collect()),
                triangles: triangles.to_vec(),
            });
            report.primitives += 1;
        }
        if !robust_primitives.is_empty() {
            let outcome = robust_mesh_coincidences(
                mesh.index(),
                &robust_primitives,
                &settings.robust,
                &mut candidates,
            )
            .map_err(|e| match e {
                RobustError::CandidateBudget { .. } => GltfLintError::Limit("max_candidate_pairs".into()),
                RobustError::Invalid(message) => {
                    GltfLintError::Geometry(format!("mesh {}: {message}", mesh.index()))
                }
            })?;
            report.robust_work.eligible_triangles += outcome.work.eligible_triangles;
            report.robust_work.cells += outcome.work.cells;
            report.robust_work.candidate_pairs += outcome.work.candidate_pairs;
            report.mesh_evidence.extend(outcome.evidence);
            report.robust_coincidences.extend(outcome.coincidences);
            report.check_budget(settings)?;
        }
        captured_meshes.push(CapturedMesh {
            index: mesh.index(),
            complete,
            primitives: captured_prims,
        });
        if complete && !mesh_key.is_empty() {
            mesh_key.sort();
            if let Some(&original) = mesh_keys.get(&mesh_key) {
                report.duplicate_meshes.push(DuplicateMesh {
                    original,
                    duplicate: mesh.index(),
                });
                report.check_budget(settings)?;
            } else {
                mesh_keys.insert(mesh_key, mesh.index());
            }
        }
    }
    rig::lint(&document, &buffers, settings, &mut report, budget)?;
    budget.checkpoint()?;
    report.input_usage = budget.usage().clone();
    let captured = CapturedGeometry {
        meshes: captured_meshes,
        skipped_primitives: report.skipped_primitives.clone(),
        rig_channels: report.rig_channels.clone(),
        input_limits: report.input_limits.clone(),
        input_usage: report.input_usage.clone(),
    };
    Ok((report, captured))
}

/// Reruns duplicate and coincident checks on [`CapturedGeometry`].
///
/// Does not open a file and does not walk rig channels again. The result matches
/// [`lint_gltf_bounded`] for the same primitives and settings.
///
/// Robust coincidence mode (ADR-8347) reads normals, skin weights, morph targets and every
/// sampled UV set, which a capture does not keep, so it is refused here: lint the source with
/// [`lint_gltf_bounded`] instead (ADR-8574).
///
/// # Errors
/// Invalid settings, robust mode, a captured index that does not fit, or an exhausted budget.
pub fn evaluate_captured(
    captured: &CapturedGeometry,
    settings: &GltfLintSettings,
) -> Result<GltfLintReport, GltfLintError> {
    settings.validate()?;
    if settings.mode == CoincidenceMode::Robust {
        return Err(GltfLintError::Settings(
            "robust coincidence mode needs the source; captured geometry replays legacy mode only".into(),
        ));
    }
    let mut report = GltfLintReport {
        input_limits: captured.input_limits.clone(),
        input_usage: captured.input_usage.clone(),
        meshes: captured.meshes.len(),
        skipped_primitives: captured.skipped_primitives.clone(),
        rig_channels: captured.rig_channels.clone(),
        ..Default::default()
    };
    let mut vertices = 0;
    let mut mesh_keys = BTreeMap::<Vec<TriangleKey>, usize>::new();
    for mesh in &captured.meshes {
        let mut mesh_key = Vec::new();
        for primitive in &mesh.primitives {
            charge(
                &mut vertices,
                primitive.positions_bits.len(),
                settings.max_vertices,
                "max_vertices",
            )?;
            charge(
                &mut report.triangles,
                primitive.triangles.len(),
                settings.max_triangles,
                "max_triangles",
            )?;
            let positions: Vec<[f32; 3]> = primitive
                .positions_bits
                .iter()
                .copied()
                .map(position_from_bits)
                .collect();
            let uvs = primitive
                .uv_bits
                .as_ref()
                .map(|rows| rows.iter().copied().map(uv_from_bits).collect::<Vec<_>>());
            let context = || format!("mesh {} primitive {}", primitive.mesh, primitive.primitive);
            let pairs = coincident_pairs(
                &positions,
                uvs.as_deref(),
                &primitive.triangles,
                &settings.geometry,
            )
            .map_err(|error| GltfLintError::Geometry(format!("{}: {error}", context())))?;
            for pair in pairs {
                report.coincident_triangles.push(CoincidentTriangles {
                    mesh: primitive.mesh,
                    primitive: primitive.primitive,
                    pair,
                });
                report.check_budget(settings)?;
            }
            for &triangle in &primitive.triangles {
                if triangle.iter().any(|&index| index as usize >= positions.len()) {
                    return Err(GltfLintError::Geometry(format!(
                        "{} has an index outside the captured positions",
                        context()
                    )));
                }
                let mut key = triangle.map(|index| position_bits(positions[index as usize]));
                key.sort();
                mesh_key.push(key);
            }
            report.primitives += 1;
        }
        if mesh.complete && !mesh_key.is_empty() {
            mesh_key.sort();
            if let Some(&original) = mesh_keys.get(&mesh_key) {
                report.duplicate_meshes.push(DuplicateMesh {
                    original,
                    duplicate: mesh.index,
                });
                report.check_budget(settings)?;
            } else {
                mesh_keys.insert(mesh_key, mesh.index);
            }
        }
    }
    report.check_budget(settings)?;
    Ok(report)
}

/// Document-wide labels for material data this build cannot read (ADR-8347).
fn unread_material_labels(document: &gltf::Document) -> std::collections::BTreeSet<String> {
    const READ: [&str; 4] = [
        "KHR_materials_emissive_strength",
        "KHR_materials_ior",
        "KHR_materials_transmission",
        "KHR_materials_volume",
    ];
    let mut labels = std::collections::BTreeSet::new();
    for name in document.extensions_used() {
        if name == "KHR_texture_transform" {
            labels.insert("khr_texture_transform_unread".to_owned());
        } else if name.starts_with("KHR_materials_") && !READ.contains(&name) {
            labels.insert("material_extension_unread".to_owned());
        }
    }
    labels
}

/// `texCoord` of every texture the material samples. The default material samples none.
fn material_uv_sets(material: &gltf::Material<'_>) -> Vec<u32> {
    let pbr = material.pbr_metallic_roughness();
    let mut sets = Vec::new();
    sets.extend(pbr.base_color_texture().map(|t| t.tex_coord()));
    sets.extend(pbr.metallic_roughness_texture().map(|t| t.tex_coord()));
    sets.extend(material.normal_texture().map(|t| t.tex_coord()));
    sets.extend(material.occlusion_texture().map(|t| t.tex_coord()));
    sets.extend(material.emissive_texture().map(|t| t.tex_coord()));
    sets.extend(
        material
            .transmission()
            .and_then(|t| t.transmission_texture())
            .map(|t| t.tex_coord()),
    );
    sets.extend(
        material
            .volume()
            .and_then(|v| v.thickness_texture())
            .map(|t| t.tex_coord()),
    );
    sets
}

type RobustCounters<'c> = (&'c mut usize, &'c mut usize, &'c mut usize);

/// Reads one triangle, strip or fan primitive for the robust diagnostic. Every budget is
/// charged and every count checked before the accessor is decoded.
fn read_robust_primitive(
    primitive: &gltf::Primitive<'_>,
    buffers: &[gltf::buffer::Data],
    mesh_uv_sets: &std::collections::BTreeSet<u32>,
    unread: &std::collections::BTreeSet<String>,
    settings: &GltfLintSettings,
    (vertices, triangles, attribute_elements): RobustCounters<'_>,
    context: &dyn Fn() -> String,
) -> Result<PrimitiveData, GltfLintError> {
    let geometry = |what: &str| GltfLintError::Geometry(format!("{} {what}", context()));
    let mode = primitive.mode().as_gl_enum();
    let position_accessor = primitive
        .get(&gltf::Semantic::Positions)
        .ok_or_else(|| geometry("has no POSITION"))?;
    let count = position_accessor.count();
    charge(vertices, count, settings.max_vertices, "max_vertices")?;
    let index_count = primitive.indices().map_or(count, |a| a.count());
    let triangle_count =
        topology_triangle_count(mode, index_count).ok_or_else(|| geometry("has an incomplete triangle"))?;
    charge(triangles, triangle_count, settings.max_triangles, "max_triangles")?;
    let mut attribute = |accessor: &gltf::Accessor<'_>, name: &str| -> Result<(), GltfLintError> {
        if accessor.count() != count {
            return Err(geometry(&format!("has mismatched {name} count")));
        }
        charge(
            attribute_elements,
            count,
            settings.robust.max_attribute_elements,
            "max_attribute_elements",
        )
    };
    let material = primitive.material();
    let material_uv_sets: std::collections::BTreeSet<u32> = material_uv_sets(&material).into_iter().collect();
    let has_textures = !material_uv_sets.is_empty();
    let mut sets = Vec::new();
    for &set in mesh_uv_sets {
        if let Some(accessor) = primitive.get(&gltf::Semantic::TexCoords(set)) {
            attribute(&accessor, &format!("TEXCOORD_{set}"))?;
            sets.push(set);
        }
    }
    if let Some(accessor) = primitive.get(&gltf::Semantic::Normals) {
        attribute(&accessor, "NORMAL")?;
    }
    let mut skin_sets = 0;
    while let Some(joints) = primitive.get(&gltf::Semantic::Joints(skin_sets)) {
        let weights = primitive
            .get(&gltf::Semantic::Weights(skin_sets))
            .ok_or_else(|| geometry(&format!("has JOINTS_{skin_sets} without WEIGHTS_{skin_sets}")))?;
        attribute(&joints, "JOINTS")?;
        attribute(&weights, "WEIGHTS")?;
        skin_sets += 1;
    }
    for target in primitive.morph_targets() {
        for accessor in [target.positions(), target.normals(), target.tangents()]
            .into_iter()
            .flatten()
        {
            attribute(&accessor, "morph target")?;
        }
    }
    let reader = primitive.reader(|buffer| buffers.get(buffer.index()).map(|b| b.0.as_slice()));
    let unreadable = |what: &str| geometry(&format!("has unreadable {what}"));
    let positions: Vec<[f32; 3]> = reader
        .read_positions()
        .ok_or_else(|| unreadable("positions"))?
        .collect();
    let indices = match reader.read_indices() {
        Some(v) => v.into_u32().collect::<Vec<_>>(),
        None if primitive.indices().is_none() => {
            let n = u32::try_from(positions.len())
                .map_err(|_| GltfLintError::Limit("vertex index range".into()))?;
            (0..n).collect()
        }
        None => return Err(unreadable("indices")),
    };
    let expanded = expand_topology(mode, &indices).map_err(|e| geometry(&e))?;
    if expanded.iter().flatten().any(|&i| i as usize >= positions.len()) {
        return Err(geometry("has a triangle index outside the position accessor"));
    }
    let mut tex_coords = BTreeMap::new();
    for set in sets {
        let uv = reader
            .read_tex_coords(set)
            .ok_or_else(|| unreadable("TEXCOORD"))?
            .into_f32()
            .collect();
        tex_coords.insert(set, uv);
    }
    let normals = match primitive.get(&gltf::Semantic::Normals) {
        Some(_) => Some(
            reader
                .read_normals()
                .ok_or_else(|| unreadable("NORMAL"))?
                .collect(),
        ),
        None => None,
    };
    let mut skin = Vec::new();
    for set in 0..skin_sets {
        skin.push(SkinSet {
            joints: reader
                .read_joints(set)
                .ok_or_else(|| unreadable("JOINTS"))?
                .into_u16()
                .collect(),
            weights: reader
                .read_weights(set)
                .ok_or_else(|| unreadable("WEIGHTS"))?
                .into_f32()
                .collect(),
        });
    }
    let morph_targets = reader
        .read_morph_targets()
        .map(|(p, n, t)| MorphDeltas {
            positions: p.map(Iterator::collect),
            normals: n.map(Iterator::collect),
            tangents: t.map(Iterator::collect),
        })
        .collect();
    Ok(PrimitiveData {
        primitive: primitive.index(),
        mode,
        positions,
        triangles: expanded,
        material: material.index(),
        material_uv_sets,
        material_unread: unread
            .iter()
            .filter(|label| {
                material.index().is_some()
                    && (label.as_str() != "khr_texture_transform_unread" || has_textures)
            })
            .cloned()
            .collect(),
        tex_coords,
        normals,
        skin,
        morph_targets,
    })
}
