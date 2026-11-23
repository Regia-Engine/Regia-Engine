// SPDX-License-Identifier: MPL-2.0
//! Project-resolved source-lint policy and evidence (ADR-8248).
//!
//! [`crate::validators::Validator`] stays disk-free. This module is the separate
//! evidence-production API: it resolves `config/source_lint.ron` (engine defaults, then
//! project defaults, then the named target profile, then per-asset overlays) and emits one
//! deterministic report for in-memory sources. Geometry and material linters keep their
//! call-local settings; this layer only chooses those settings and records what ran.
//!
//! Policy merge follows the import-rules field rule (ADR-3321): a present overlay field
//! replaces that field, and a later matching asset glob wins. Overlay globs match
//! ASCII-case-insensitively. Suppression paths are exact.
//!
//! SARIF 2.1.0 is the OASIS interchange shape. BLAKE3 (the public spec) is the evidence hash.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::gltf_lint::{
    CoincidentTriangles, DuplicateMesh, GltfLintError, GltfLintReport, GltfLintSettings, RigChannelWarning,
    SkippedPrimitive, lint_gltf,
};
use crate::gltf_material_lint::{
    MaterialFinding, MaterialLintProfile, MaterialLintReport, MaterialLintSettings, MaterialNameRule,
    MaterialRepairEffect, lint_material_file,
};

/// `config/source_lint.ron` version. The first published shape.
pub const SOURCE_LINT_POLICY_VERSION: u32 = 1;

/// JSON evidence report version. Independent of the policy file version.
pub const SOURCE_LINT_REPORT_VERSION: u32 = 1;

/// Project-relative policy path.
pub const SOURCE_LINT_FILE: &str = "config/source_lint.ron";

/// Tool name stored in the evidence key and in SARIF.
pub const SOURCE_LINT_TOOL: &str = "regia-source-lint";

const METRIC_UNIT: &str = "count";
const PRODUCER_GEOMETRY: &str = "gltf_geometry";
const PRODUCER_MATERIALS: &str = "gltf_materials";
const PRODUCER_TEXTURE: &str = "texture";

static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(1);

/// A policy, target, IO, or report failure. `code` is the stable agent reason.
#[derive(Debug, thiserror::Error)]
pub enum SourceLintError {
    /// The policy text or a resolved setting is unusable.
    #[error("{0}")]
    Policy(String),
    /// The requested target is empty or not in the policy.
    #[error("{0}")]
    Target(String),
    /// A scratch file or the policy file could not be read or written.
    #[error("{0}")]
    Io(String),
    /// JSON or SARIF could not be encoded.
    #[error("{0}")]
    Report(String),
}

impl SourceLintError {
    /// Stable lowercase reason: `content_lint_policy`, `content_lint_target`,
    /// `content_lint_io`, or `content_lint_report`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Policy(_) => "content_lint_policy",
            Self::Target(_) => "content_lint_target",
            Self::Io(_) => "content_lint_io",
            Self::Report(_) => "content_lint_report",
        }
    }

    fn policy(message: impl Into<String>) -> Self {
        Self::Policy(message.into())
    }

    fn target(message: impl Into<String>) -> Self {
        Self::Target(message.into())
    }

    fn io(message: impl Into<String>) -> Self {
        Self::Io(message.into())
    }
}

/// One geometry field overlay. `Some` replaces that field; `None` leaves the lower layer.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GeometryOverlay {
    /// Position quantization grid, in meters.
    pub quantization: Option<f64>,
    /// Exclusive maximum matched-corner separation, in meters.
    pub separation: Option<f64>,
    /// Exclusive separation below which UVs do not matter.
    pub exact_separation: Option<f64>,
    /// Exclusive mean UV-corner distance.
    pub uv_distance: Option<f64>,
    /// Exclusive anisotropy ratio.
    pub anisotropy_ratio: Option<f64>,
    /// Exclusive minimum suspect anisotropy.
    pub anisotropy_floor: Option<f64>,
    /// Maximum POSITION elements.
    pub max_vertices: Option<usize>,
    /// Maximum triangles.
    pub max_triangles: Option<usize>,
    /// Maximum animation channels.
    pub max_channels: Option<usize>,
    /// Maximum animation accessor elements.
    pub max_animation_elements: Option<usize>,
    /// Maximum geometry diagnostic rows.
    pub max_findings: Option<usize>,
    /// Coincidence analysis: `legacy` or `robust` (ADR-8347).
    pub coincidence_mode: Option<crate::gltf_lint::CoincidenceMode>,
    /// Robust absolute corner tolerance, in meters.
    pub tolerance_meters: Option<f64>,
    /// Robust relative corner tolerance, as a fraction of the mesh bounding-box diagonal.
    pub tolerance_relative: Option<f64>,
    /// Robust mean UV-corner tolerance, in UV units.
    pub uv_tolerance: Option<f64>,
    /// Robust normal, weight and morph-delta tolerance.
    pub attribute_tolerance: Option<f64>,
    /// Robust zero-area floor, in square meters.
    pub min_area_square_meters: Option<f64>,
    /// Robust candidate-pair budget.
    pub max_candidate_pairs: Option<usize>,
    /// Robust decoded attribute-element budget.
    pub max_attribute_elements: Option<usize>,
}

/// One material field overlay.
///
/// `default_volume_thickness` and `default_attenuation_distance` are `Option<Option<f64>>`:
/// omitted inherits, `Some(None)` clears the lower layer, `Some(Some(v))` sets a value.
/// `name_rules` replaces the whole list when `Some`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MaterialOverlay {
    /// Metalness inserted when an untextured material omits it.
    pub default_metallic: Option<f64>,
    /// Roughness inserted when an untextured material omits it.
    pub default_roughness: Option<f64>,
    /// Missing emissive strength.
    pub default_emissive_strength: Option<f64>,
    /// Thickness supplied when a moved tint has none. Inner `None` clears.
    pub default_volume_thickness: Option<Option<f64>>,
    /// Attenuation distance supplied with a moved tint. Inner `None` clears.
    pub default_attenuation_distance: Option<Option<f64>>,
    /// Apply the dielectric-at-1 guard to unmatched untextured names.
    pub classify_untextured: Option<bool>,
    /// First matching name rule wins. `Some` replaces the whole list.
    pub name_rules: Option<Vec<MaterialNameRule>>,
}

/// Geometry and material overlays for one layer.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuleOverlay {
    /// Geometry thresholds and budgets. `None` leaves the lower layer unchanged.
    pub geometry: Option<GeometryOverlay>,
    /// Material policy. `None` leaves the lower layer unchanged.
    pub materials: Option<MaterialOverlay>,
}

/// A per-asset overlay. `glob` uses the import-rules glob, ASCII-case-insensitively.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetOverlay {
    /// Project-relative glob. Backslashes are rejected.
    pub glob: String,
    /// Geometry fields this glob replaces.
    #[serde(default)]
    pub geometry: Option<GeometryOverlay>,
    /// Material fields this glob replaces.
    #[serde(default)]
    pub materials: Option<MaterialOverlay>,
}

/// A narrow suppression. It applies only when rule, path, and content hash all match.
/// `location`, when set, also has to match the finding location.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Suppression {
    /// Stable rule id from [`rules`].
    pub rule_id: String,
    /// Exact project-relative path. Not a glob and not case-folded.
    pub path: String,
    /// Lowercase BLAKE3 hex of the source bytes (64 digits).
    pub content_hash: String,
    /// Why this pair is waived. Non-empty.
    pub reason: String,
    /// Optional JSON pointer or geometry location. `None` covers every location of the rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

/// `config/source_lint.ron` v1.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceLintPolicy {
    /// [`SOURCE_LINT_POLICY_VERSION`].
    pub format_version: u32,
    /// Project defaults, above the engine defaults.
    #[serde(default)]
    pub defaults: RuleOverlay,
    /// Named target profiles. The key `default` applies when the caller asks for `default`.
    #[serde(default)]
    pub profiles: BTreeMap<String, RuleOverlay>,
    /// Per-asset overlays in file order. Later matching globs win field by field.
    #[serde(default)]
    pub assets: Vec<AssetOverlay>,
    /// Narrow suppressions in file order. The first match supplies the reason.
    #[serde(default)]
    pub suppressions: Vec<Suppression>,
}

/// Settings after the four-layer merge, ready for the existing linters.
#[derive(Clone, Debug, Serialize)]
pub struct ResolvedLintSettings {
    /// Geometry lint settings.
    pub geometry: GltfLintSettings,
    /// Material lint settings.
    pub materials: MaterialLintSettings,
}

impl SourceLintPolicy {
    /// Engine defaults and no project file. Resolving `default` uses the linter defaults.
    #[must_use]
    pub fn engine_defaults() -> Self {
        Self {
            format_version: SOURCE_LINT_POLICY_VERSION,
            defaults: RuleOverlay::default(),
            profiles: BTreeMap::new(),
            assets: Vec::new(),
            suppressions: Vec::new(),
        }
    }

    /// Parse and structurally validate a policy. Numeric settings are checked in [`Self::resolve`].
    ///
    /// # Errors
    /// `content_lint_policy` when the text, version, glob, or suppression identity is unusable.
    pub fn from_ron(text: &str) -> Result<Self, SourceLintError> {
        let policy: Self = ron::from_str(text).map_err(|e| SourceLintError::policy(e.to_string()))?;
        policy.validate_structure()?;
        Ok(policy)
    }

    /// Load `config/source_lint.ron`. A missing file is the engine default, not an error.
    ///
    /// # Errors
    /// `content_lint_policy` when the file exists but is invalid, `content_lint_io` when it cannot be read.
    pub fn load_project(root: &Path) -> Result<Self, SourceLintError> {
        let path = root.join(SOURCE_LINT_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::from_ron(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::engine_defaults()),
            Err(e) => Err(SourceLintError::io(format!(
                "cannot read {SOURCE_LINT_FILE}: {e}"
            ))),
        }
    }

    /// Merge engine defaults, project defaults, the named profile, then matching asset globs.
    ///
    /// `target` `default` is always valid. A profile named `default` applies only when it exists.
    /// Any other target must name a profile. `path` `None` skips asset overlays.
    ///
    /// # Errors
    /// `content_lint_target` for an empty or unknown target. `content_lint_policy` when the
    /// merged settings fail the linter's own checks.
    pub fn resolve(&self, target: &str, path: Option<&str>) -> Result<ResolvedLintSettings, SourceLintError> {
        if target.is_empty() {
            return Err(SourceLintError::target("target is empty"));
        }
        let profile = if target == "default" {
            self.profiles.get("default")
        } else {
            Some(
                self.profiles
                    .get(target)
                    .ok_or_else(|| SourceLintError::target(format!("unknown target `{target}`")))?,
            )
        };
        let mut geometry = GltfLintSettings::default();
        // Evidence was specified against the appearance-changing conventions (ADR-8061)
        // before ADR-8250 made those an explicit profile. The direct material-lint
        // default stays `standard`. This producer starts at `compatibility` so an
        // omitted factor still records `factor_missing`. Overlays may replace it.
        let mut materials = MaterialLintSettings {
            profile: MaterialLintProfile::Compatibility,
            ..Default::default()
        };
        apply_overlay(&mut geometry, &mut materials, &self.defaults);
        if let Some(layer) = profile {
            apply_overlay(&mut geometry, &mut materials, layer);
        }
        if let Some(path) = path {
            for asset in &self.assets {
                if glob_matches(&asset.glob, path) {
                    apply_parts(
                        &mut geometry,
                        &mut materials,
                        asset.geometry.as_ref(),
                        asset.materials.as_ref(),
                    );
                }
            }
        }
        geometry
            .validate()
            .map_err(|e| SourceLintError::policy(e.to_string()))?;
        validate_materials(&materials)?;
        Ok(ResolvedLintSettings { geometry, materials })
    }

    fn validate_structure(&self) -> Result<(), SourceLintError> {
        if self.format_version != SOURCE_LINT_POLICY_VERSION {
            return Err(SourceLintError::policy(format!(
                "format_version {} is not supported (want {SOURCE_LINT_POLICY_VERSION})",
                self.format_version
            )));
        }
        for key in self.profiles.keys() {
            if key.is_empty() {
                return Err(SourceLintError::policy("a profile name is empty"));
            }
        }
        for asset in &self.assets {
            check_glob(&asset.glob)?;
        }
        let mut seen = BTreeSet::new();
        for suppression in &self.suppressions {
            check_suppression(suppression)?;
            let location = suppression.location.clone().unwrap_or_default();
            let key = (
                suppression.rule_id.clone(),
                suppression.path.clone(),
                suppression.content_hash.clone(),
                location,
            );
            if !seen.insert(key) {
                return Err(SourceLintError::policy(format!(
                    "duplicate suppression for {} {}",
                    suppression.rule_id, suppression.path
                )));
            }
        }
        Ok(())
    }
}

fn check_glob(glob: &str) -> Result<(), SourceLintError> {
    if glob.is_empty() || glob.contains('\\') || glob.contains('\0') {
        return Err(SourceLintError::policy(
            "an asset glob must be a non-empty project-relative pattern without backslashes",
        ));
    }
    Ok(())
}

fn check_suppression(suppression: &Suppression) -> Result<(), SourceLintError> {
    if rules().iter().all(|rule| rule.id != suppression.rule_id) {
        return Err(SourceLintError::policy(format!(
            "unknown suppression rule `{}`",
            suppression.rule_id
        )));
    }
    if suppression.path.is_empty() || suppression.path.contains('\\') || suppression.path.contains('\0') {
        return Err(SourceLintError::policy(
            "a suppression path must be a non-empty exact project-relative path without backslashes",
        ));
    }
    if !is_blake3_hex(&suppression.content_hash) {
        return Err(SourceLintError::policy(
            "content_hash must be 64 lowercase hex digits",
        ));
    }
    if suppression.reason.trim().is_empty() {
        return Err(SourceLintError::policy("a suppression reason must be non-empty"));
    }
    if let Some(location) = &suppression.location
        && (location.is_empty() || !location.starts_with('/'))
    {
        return Err(SourceLintError::policy(
            "a suppression location must be an absolute pointer starting with /",
        ));
    }
    Ok(())
}

fn is_blake3_hex(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn glob_matches(pattern: &str, path: &str) -> bool {
    regia_asset::import_rules::glob_match(&pattern.to_ascii_lowercase(), &path.to_ascii_lowercase())
}

fn apply_overlay(geometry: &mut GltfLintSettings, materials: &mut MaterialLintSettings, layer: &RuleOverlay) {
    apply_parts(
        geometry,
        materials,
        layer.geometry.as_ref(),
        layer.materials.as_ref(),
    );
}

fn apply_parts(
    geometry: &mut GltfLintSettings,
    materials: &mut MaterialLintSettings,
    geometry_over: Option<&GeometryOverlay>,
    material_over: Option<&MaterialOverlay>,
) {
    if let Some(over) = geometry_over {
        set_f64(&mut geometry.geometry.quantization, over.quantization);
        set_f64(&mut geometry.geometry.separation, over.separation);
        set_f64(&mut geometry.geometry.exact_separation, over.exact_separation);
        set_f64(&mut geometry.geometry.uv_distance, over.uv_distance);
        set_f64(&mut geometry.geometry.anisotropy_ratio, over.anisotropy_ratio);
        set_f64(&mut geometry.geometry.anisotropy_floor, over.anisotropy_floor);
        set_copy(&mut geometry.max_vertices, over.max_vertices);
        set_copy(&mut geometry.max_triangles, over.max_triangles);
        set_copy(&mut geometry.max_channels, over.max_channels);
        set_copy(&mut geometry.max_animation_elements, over.max_animation_elements);
        set_copy(&mut geometry.max_findings, over.max_findings);
        set_copy(&mut geometry.mode, over.coincidence_mode);
        set_f64(&mut geometry.robust.tolerance_meters, over.tolerance_meters);
        set_f64(&mut geometry.robust.tolerance_relative, over.tolerance_relative);
        set_f64(&mut geometry.robust.uv_tolerance, over.uv_tolerance);
        set_f64(&mut geometry.robust.attribute_tolerance, over.attribute_tolerance);
        set_f64(
            &mut geometry.robust.min_area_square_meters,
            over.min_area_square_meters,
        );
        set_copy(&mut geometry.robust.max_candidate_pairs, over.max_candidate_pairs);
        set_copy(
            &mut geometry.robust.max_attribute_elements,
            over.max_attribute_elements,
        );
    }
    if let Some(over) = material_over {
        set_f64(&mut materials.default_metallic, over.default_metallic);
        set_f64(&mut materials.default_roughness, over.default_roughness);
        set_f64(
            &mut materials.default_emissive_strength,
            over.default_emissive_strength,
        );
        if let Some(value) = &over.default_volume_thickness {
            materials.default_volume_thickness = *value;
        }
        if let Some(value) = &over.default_attenuation_distance {
            materials.default_attenuation_distance = *value;
        }
        set_copy(&mut materials.classify_untextured, over.classify_untextured);
        if let Some(rules) = &over.name_rules {
            materials.name_rules.clone_from(rules);
        }
    }
}

fn set_f64(slot: &mut f64, value: Option<f64>) {
    if let Some(value) = value {
        *slot = value;
    }
}

fn set_copy<T: Copy>(slot: &mut T, value: Option<T>) {
    if let Some(value) = value {
        *slot = value;
    }
}

fn validate_materials(settings: &MaterialLintSettings) -> Result<(), SourceLintError> {
    let unit = |v: f64| v.is_finite() && (0.0..=1.0).contains(&v);
    let strength = |v: f64| v.is_finite() && v >= 0.0;
    let bad = !unit(settings.default_metallic)
        || !unit(settings.default_roughness)
        || !strength(settings.default_emissive_strength)
        || settings.default_volume_thickness.is_some_and(|v| !strength(v))
        || settings
            .default_attenuation_distance
            .is_some_and(|v| !v.is_finite() || v <= 0.0)
        || settings.name_rules.iter().any(|rule| {
            rule.contains.is_empty()
                || rule.metallic.is_some_and(|v| !unit(v))
                || rule.roughness.is_some_and(|v| !unit(v))
                || rule.emissive_strength.is_some_and(|v| !strength(v))
        });
    if bad {
        return Err(SourceLintError::policy(
            "factors must be in [0,1], strengths and thickness finite and nonnegative, attenuation distance positive, and name substrings nonempty",
        ));
    }
    Ok(())
}

/// One stable rule the evidence report can name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuleInfo {
    /// Stable id shared by JSON and SARIF.
    pub id: &'static str,
    /// `gltf_geometry`, `gltf_materials`, or `texture`.
    pub producer: &'static str,
    /// One-line description.
    pub summary: &'static str,
    /// Unit token. Findings use `finding`. Measured metrics use `count`.
    pub unit: &'static str,
}

/// The rule catalog, sorted by id.
#[must_use]
pub fn rules() -> &'static [RuleInfo] {
    RULES
}

const RULES: &[RuleInfo] = &[
    RuleInfo {
        id: "attenuation_distance",
        producer: PRODUCER_MATERIALS,
        summary: "Volume attenuationDistance is not greater than 0",
        unit: "finding",
    },
    RuleInfo {
        id: "emissive_strength",
        producer: PRODUCER_MATERIALS,
        summary: "Emissive strength is below the name-rule minimum",
        unit: "finding",
    },
    RuleInfo {
        id: "extension_dependency",
        producer: PRODUCER_MATERIALS,
        summary: "KHR_materials_volume is present without KHR_materials_transmission",
        unit: "finding",
    },
    RuleInfo {
        id: "extension_nesting",
        producer: PRODUCER_MATERIALS,
        summary: "Extension JSON exceeded the depth or byte walk",
        unit: "finding",
    },
    RuleInfo {
        id: "extensions_used",
        producer: PRODUCER_MATERIALS,
        summary: "A material extension is missing from extensionsUsed",
        unit: "finding",
    },
    RuleInfo {
        id: "factor_missing",
        producer: PRODUCER_MATERIALS,
        summary: "A metallic or roughness factor is omitted",
        unit: "finding",
    },
    RuleInfo {
        id: "factors_in_range",
        producer: PRODUCER_MATERIALS,
        summary: "A factor is outside [0,1]",
        unit: "finding",
    },
    RuleInfo {
        id: "gltf_coincident_advisory",
        producer: PRODUCER_GEOMETRY,
        summary: "Robust mode: positions agree but attribute or deformation data differs or is unavailable",
        unit: "finding",
    },
    RuleInfo {
        id: "gltf_coincident_equivalent",
        producer: PRODUCER_GEOMETRY,
        summary: "Robust mode: positions and attribute data agree",
        unit: "finding",
    },
    RuleInfo {
        id: "gltf_coincident_triangles",
        producer: PRODUCER_GEOMETRY,
        summary: "Two triangles occupy the same quantized corners",
        unit: "finding",
    },
    RuleInfo {
        id: "gltf_duplicate_mesh",
        producer: PRODUCER_GEOMETRY,
        summary: "Two mesh definitions share triangle geometry",
        unit: "finding",
    },
    RuleInfo {
        id: "gltf_mesh_degenerate",
        producer: PRODUCER_GEOMETRY,
        summary: "Robust mode: triangles repeat a vertex index",
        unit: "finding",
    },
    RuleInfo {
        id: "gltf_mesh_nonfinite",
        producer: PRODUCER_GEOMETRY,
        summary: "Robust mode: triangles have a non-finite corner",
        unit: "finding",
    },
    RuleInfo {
        id: "gltf_mesh_zero_area",
        producer: PRODUCER_GEOMETRY,
        summary: "Robust mode: triangles have zero area",
        unit: "finding",
    },
    RuleInfo {
        id: "gltf_rig_duplicate_channel",
        producer: PRODUCER_GEOMETRY,
        summary: "Two channels animate the same node property",
        unit: "finding",
    },
    RuleInfo {
        id: "gltf_rig_input_times",
        producer: PRODUCER_GEOMETRY,
        summary: "Channel input times are empty, non-finite, or not increasing",
        unit: "finding",
    },
    RuleInfo {
        id: "gltf_rig_morph_target_missing",
        producer: PRODUCER_GEOMETRY,
        summary: "A weights channel has no consistent morph target count",
        unit: "finding",
    },
    RuleInfo {
        id: "gltf_rig_non_joint_channel",
        producer: PRODUCER_GEOMETRY,
        summary: "A transform channel targets a node that is not a skin joint",
        unit: "finding",
    },
    RuleInfo {
        id: "gltf_rig_nonfinite_output",
        producer: PRODUCER_GEOMETRY,
        summary: "A channel output sample is non-finite",
        unit: "finding",
    },
    RuleInfo {
        id: "gltf_rig_output_count",
        producer: PRODUCER_GEOMETRY,
        summary: "A channel output count does not match its inputs",
        unit: "finding",
    },
    RuleInfo {
        id: "gltf_skipped_primitive",
        producer: PRODUCER_GEOMETRY,
        summary: "A primitive is not a triangle list and was not compared",
        unit: "finding",
    },
    RuleInfo {
        id: "ior_domain",
        producer: PRODUCER_MATERIALS,
        summary: "KHR_materials_ior ior is below 1",
        unit: "finding",
    },
    RuleInfo {
        id: "metallic_name_policy",
        producer: PRODUCER_MATERIALS,
        summary: "An untextured name rule classifies the surface as dielectric",
        unit: "finding",
    },
    RuleInfo {
        id: "orm_factors",
        producer: PRODUCER_MATERIALS,
        summary: "An ORM texture is paired with a factor other than 1",
        unit: "finding",
    },
    RuleInfo {
        id: "texture_budget_unknown",
        producer: PRODUCER_TEXTURE,
        summary: "A texture size could not be measured",
        unit: "finding",
    },
    RuleInfo {
        id: "texture_color_space",
        producer: PRODUCER_MATERIALS,
        summary: "One texture index is required in two color spaces",
        unit: "finding",
    },
    RuleInfo {
        id: "texture_impossible_dimensions",
        producer: PRODUCER_TEXTURE,
        summary: "A texture header dimension or decoded size cannot be admitted",
        unit: "finding",
    },
    RuleInfo {
        id: "texture_index",
        producer: PRODUCER_MATERIALS,
        summary: "A texture or image index is outside its array",
        unit: "finding",
    },
    RuleInfo {
        id: "texture_missing_mip",
        producer: PRODUCER_TEXTURE,
        summary: "A pass-through block chain is shorter than its surviving top level",
        unit: "finding",
    },
    RuleInfo {
        id: "texture_usage_mismatch",
        producer: PRODUCER_TEXTURE,
        summary: "Texture color space and roles disagree",
        unit: "finding",
    },
    RuleInfo {
        id: "texture_wrong_compression",
        producer: PRODUCER_TEXTURE,
        summary: "The block family does not match the platform cook policy",
        unit: "finding",
    },
    RuleInfo {
        id: "transmission_base_white",
        producer: PRODUCER_MATERIALS,
        summary: "Transmission base color is not white",
        unit: "finding",
    },
    RuleInfo {
        id: "transmission_ior",
        producer: PRODUCER_MATERIALS,
        summary: "Transmission is missing the standard IOR",
        unit: "finding",
    },
    RuleInfo {
        id: "uv_channel",
        producer: PRODUCER_MATERIALS,
        summary: "A material texture uses a UV channel the mesh does not have",
        unit: "finding",
    },
    RuleInfo {
        id: "volume_thickness",
        producer: PRODUCER_MATERIALS,
        summary: "Volume thicknessFactor is negative",
        unit: "finding",
    },
];

/// Lowercase BLAKE3 hex of `bytes`.
#[must_use]
pub fn content_hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Whether a producer finished, refused the source, or stopped early.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupportStatus {
    /// The producer finished and its metrics are known.
    Supported,
    /// The source is not a glTF document or could not be read. This is a finished classification.
    Unsupported,
    /// A budget, truncation, or cancellation stopped the producer.
    Incomplete,
}

/// How serious a finding is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingSeverity {
    /// A rule violation.
    Warning,
    /// Explicitly skipped topology.
    Info,
}

/// A measured count. Missing data is [`MetricReading::unknown`], never zero.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricReading {
    /// `false` when the producer did not measure this metric.
    pub known: bool,
    /// The count when `known` is true. Omitted from JSON when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<u64>,
    /// `count`.
    pub unit: String,
    /// `gltf_geometry`, `gltf_materials`, or `texture`.
    pub producer: String,
}

impl MetricReading {
    /// A measurement, including a real zero.
    #[must_use]
    pub fn known(value: u64, producer: &str) -> Self {
        Self {
            known: true,
            value: Some(value),
            unit: METRIC_UNIT.to_owned(),
            producer: producer.to_owned(),
        }
    }

    /// The producer did not measure this metric.
    #[must_use]
    pub fn unknown(producer: &str) -> Self {
        Self {
            known: false,
            value: None,
            unit: METRIC_UNIT.to_owned(),
            producer: producer.to_owned(),
        }
    }

    /// `Some` only for a known value. A known flag without a value is unknown.
    #[must_use]
    pub fn as_count(&self) -> Option<u64> {
        if self.known { self.value } else { None }
    }
}

/// One source row in the evidence report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceEvidence {
    /// Project-relative path.
    pub path: String,
    /// BLAKE3 of the source bytes.
    pub content_hash: String,
    /// Finished, refused, or stopped.
    pub support: SupportStatus,
    /// Why support is not `supported`. Omitted when the source was fully measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// `triangles`, `meshes`, and `materials`, each known or unknown.
    pub metrics: BTreeMap<String, MetricReading>,
}

impl SourceEvidence {
    /// The metric `name`, or unknown when it is absent or has no value.
    #[must_use]
    pub fn metric(&self, name: &str) -> MetricReading {
        let producer = metric_producer(name);
        match self.metrics.get(name) {
            Some(reading) if reading.as_count().is_some() => reading.clone(),
            _ => MetricReading::unknown(producer),
        }
    }
}

fn metric_producer(name: &str) -> &'static str {
    match name {
        "materials" => PRODUCER_MATERIALS,
        "texture_width" | "texture_height" | "decoded_bytes" => PRODUCER_TEXTURE,
        _ => PRODUCER_GEOMETRY,
    }
}

/// One finding. JSON and SARIF both use `rule_id` and `location`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LintFinding {
    /// Project-relative source path.
    pub path: String,
    /// Stable rule id.
    pub rule_id: String,
    /// JSON pointer or geometry location.
    pub location: String,
    /// Human-readable explanation.
    pub message: String,
    /// `warning` or `info`.
    pub severity: FindingSeverity,
    /// True when a matching suppression waived this finding. The row stays in the report.
    pub suppressed: bool,
    /// The suppression reason when `suppressed` is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suppression_reason: Option<String>,
    /// `preserves_appearance` or `changes_appearance` when the material linter recorded one.
    /// Absent on geometry findings and on evidence written before material profiles.
    /// Omitted from JSON when absent so older reports still parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<String>,
}

/// Bounded work recorded on the report. Tests assert these counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkCounters {
    /// Sources in the request, after path checks.
    pub sources_seen: usize,
    /// Sources that were linted or classified. Omitted rows are not processed.
    pub sources_processed: usize,
    /// Bytes of processed sources.
    pub bytes_hashed: u64,
    /// Findings before the finding cap, including suppressed rows.
    pub findings: usize,
    /// Findings kept after the finding cap.
    pub findings_kept: usize,
    /// `processed glTF and texture sources *` the catalog length. Other sources add zero.
    pub rules_checked: usize,
}

/// One evidence report. `to_json` is the canonical bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceLintReport {
    /// [`SOURCE_LINT_REPORT_VERSION`].
    pub format_version: u32,
    /// False when truncation, cancellation, or a linter budget stopped the run.
    pub complete: bool,
    /// Target name that was resolved.
    pub target: String,
    /// BLAKE3 of the target, the source hash, the settings hash, and the tool hash.
    pub evidence_key: String,
    /// BLAKE3 of the processed `path\\ncontent_hash` lines.
    pub source_hash: String,
    /// BLAKE3 of the resolved settings and applicable suppressions.
    pub settings_hash: String,
    /// BLAKE3 of the tool name, report version, and rule catalog.
    pub tool_hash: String,
    /// Every requested source, sorted by path. Omitted rows are incomplete, not clean.
    pub sources: Vec<SourceEvidence>,
    /// Findings after the cap, sorted by path, rule, location, message, and suppressed.
    pub findings: Vec<LintFinding>,
    /// Work counters for this run.
    pub work: WorkCounters,
}

impl SourceLintReport {
    /// Compact deterministic JSON.
    ///
    /// # Errors
    /// `content_lint_report` when encoding fails.
    pub fn to_json(&self) -> Result<Vec<u8>, SourceLintError> {
        serde_json::to_vec(self).map_err(|e| SourceLintError::Report(e.to_string()))
    }

    /// SARIF 2.1.0 for the same findings. `ruleId` and the logical location match the JSON row.
    ///
    /// # Errors
    /// `content_lint_report` when encoding fails.
    pub fn to_sarif(&self) -> Result<Vec<u8>, SourceLintError> {
        let rules: Vec<serde_json::Value> = RULES
            .iter()
            .map(|rule| {
                serde_json::json!({
                    "id": rule.id,
                    "shortDescription": { "text": rule.summary },
                })
            })
            .collect();
        let results: Vec<serde_json::Value> = self
            .findings
            .iter()
            .map(|finding| {
                let mut result = serde_json::json!({
                    "ruleId": finding.rule_id,
                    "level": if finding.severity == FindingSeverity::Info { "note" } else { "warning" },
                    "message": { "text": finding.message },
                    "locations": [{
                        "physicalLocation": {
                            "artifactLocation": { "uri": finding.path }
                        },
                        "logicalLocations": [{
                            "fullyQualifiedName": finding.location
                        }]
                    }],
                });
                if finding.suppressed {
                    result["suppressions"] = serde_json::json!([{
                        "kind": "external",
                        "justification": finding.suppression_reason.as_deref().unwrap_or(""),
                    }]);
                }
                if let Some(effect) = &finding.effect {
                    result["properties"] = serde_json::json!({ "effect": effect });
                }
                result
            })
            .collect();
        let doc = serde_json::json!({
            "version": "2.1.0",
            "$schema": "https://raw.githubusercontent.com/oasis-tcs/sarif-spec/master/Schemata/sarif-schema-2.1.0.json",
            "runs": [{
                "tool": {
                    "driver": {
                        "name": SOURCE_LINT_TOOL,
                        "version": SOURCE_LINT_REPORT_VERSION.to_string(),
                        "rules": rules,
                    }
                },
                "results": results,
            }],
        });
        serde_json::to_vec(&doc).map_err(|e| SourceLintError::Report(e.to_string()))
    }

    /// Metric `name` on `path`, or unknown when the source or the value is missing.
    #[must_use]
    pub fn metric(&self, path: &str, name: &str) -> MetricReading {
        self.sources
            .iter()
            .find(|source| source.path == path)
            .map_or_else(
                || MetricReading::unknown(metric_producer(name)),
                |source| source.metric(name),
            )
    }
}

/// Caps for one evidence run. Zero findings with an empty result is still complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProduceLimits {
    /// How many sources, in path order, may be linted.
    pub max_assets: usize,
    /// How many findings may be kept. Suppressed rows count.
    pub max_findings: usize,
    /// How many source bytes may be hashed.
    pub max_bytes: u64,
}

impl Default for ProduceLimits {
    fn default() -> Self {
        Self {
            max_assets: 10_000,
            max_findings: 10_000,
            max_bytes: 64 * 1024 * 1024,
        }
    }
}

/// One in-memory source. `path` is project-relative.
#[derive(Clone, Copy, Debug)]
pub struct SourceDocument<'a> {
    /// Project-relative path. No backslashes.
    pub path: &'a str,
    /// File bytes.
    pub bytes: &'a [u8],
}

/// Build one evidence report.
///
/// Sources are sorted by path before caps are applied, so a shuffled request with the same
/// bytes yields the same JSON. `cancel` is polled once before the run and once before each
/// source. A true poll stops the run: `complete` is false and unprocessed sources are not
/// reported as clean.
///
/// # Errors
/// `content_lint_policy` or `content_lint_target` from resolution, `content_lint_io` when a
/// scratch file cannot be written, or a path error (`content_lint_policy`) for duplicates and
/// backslashes.
pub fn produce(
    sources: &[SourceDocument<'_>],
    policy: &SourceLintPolicy,
    target: &str,
    limits: &ProduceLimits,
    cancel: &dyn Fn() -> bool,
) -> Result<SourceLintReport, SourceLintError> {
    produce_in(None, sources, policy, target, limits, cancel)
}

/// Build one evidence report, reading texture `.meta` and `Regia.toml` from `project` when set.
///
/// `None` keeps the in-memory path: texture settings and the texture policy stay at their
/// defaults, and glTF image URIs are not opened. The editor worker passes the project root
/// so a texture scan sees the same settings as CLI and cook.
///
/// # Errors
/// The same cases as [`produce`], plus `content_lint_policy` when a texture's project
/// policy cannot be read and `content_lint_io` when an existing referenced image cannot be read.
pub fn produce_in(
    project: Option<&Path>,
    sources: &[SourceDocument<'_>],
    policy: &SourceLintPolicy,
    target: &str,
    limits: &ProduceLimits,
    cancel: &dyn Fn() -> bool,
) -> Result<SourceLintReport, SourceLintError> {
    let _span = tracing::info_span!("source_lint_produce", target = target).entered();
    check_source_paths(sources)?;
    // Resolve once up front so an unknown target fails before any scratch file.
    let _ = policy.resolve(target, None)?;
    let mut order: Vec<usize> = (0..sources.len()).collect();
    order.sort_by_key(|&index| sources[index].path.to_owned());

    let mut scratch = None;
    let mut rows = Vec::with_capacity(sources.len());
    let mut processed: Vec<Processed> = Vec::new();
    let mut findings = Vec::new();
    let mut complete = true;
    let mut bytes_hashed = 0_u64;
    let mut rules_checked = 0_usize;
    let mut taken = 0_usize;
    let mut stop = if cancel() {
        complete = false;
        Some("cancelled")
    } else {
        None
    };

    for index in order {
        let source = &sources[index];
        if stop.is_none() && cancel() {
            complete = false;
            stop = Some("cancelled");
        }
        let len = u64::try_from(source.bytes.len()).unwrap_or(u64::MAX);
        if stop.is_none()
            && (taken >= limits.max_assets || bytes_hashed.saturating_add(len) > limits.max_bytes)
        {
            complete = false;
            stop = Some("truncated");
        }
        if let Some(reason) = stop {
            rows.push(omitted_row(source, reason));
            continue;
        }
        let resolved = policy.resolve(target, Some(source.path))?;
        let hash = content_hash(source.bytes);
        let (row, source_findings, gltf) =
            classify_source(project, target, source, &resolved, &hash, &mut scratch)?;
        if row.support == SupportStatus::Incomplete {
            complete = false;
        }
        if gltf {
            rules_checked = rules_checked.saturating_add(RULES.len());
        }
        findings.extend(source_findings);
        processed.push(Processed {
            path: source.path.to_owned(),
            hash: hash.clone(),
            settings: resolved,
        });
        rows.push(row);
        taken += 1;
        bytes_hashed = bytes_hashed.saturating_add(len);
    }

    apply_suppressions(&mut findings, policy, &processed);
    sort_findings(&mut findings);
    let findings_total = findings.len();
    if limits.max_findings == 0 {
        if !findings.is_empty() {
            complete = false;
            findings.clear();
        }
    } else if findings.len() > limits.max_findings {
        complete = false;
        findings.truncate(limits.max_findings);
    }
    rows.sort_by_key(|row| row.path.clone());

    let source_hash = hash_sources(&processed);
    let settings_hash = hash_settings(target, policy, &processed)?;
    let tool_hash = hash_tool();
    let evidence_key = hash_evidence(target, &source_hash, &settings_hash, &tool_hash)?;
    let findings_kept = findings.len();
    Ok(SourceLintReport {
        format_version: SOURCE_LINT_REPORT_VERSION,
        complete,
        target: target.to_owned(),
        evidence_key,
        source_hash,
        settings_hash,
        tool_hash,
        sources: rows,
        findings,
        work: WorkCounters {
            sources_seen: sources.len(),
            sources_processed: taken,
            bytes_hashed,
            findings: findings_total,
            findings_kept,
            rules_checked,
        },
    })
}

struct Processed {
    path: String,
    hash: String,
    settings: ResolvedLintSettings,
}

fn check_source_paths(sources: &[SourceDocument<'_>]) -> Result<(), SourceLintError> {
    let mut seen = BTreeSet::new();
    for source in sources {
        if source.path.is_empty() || source.path.contains('\\') || source.path.contains('\0') {
            return Err(SourceLintError::policy(
                "a source path must be a non-empty project-relative path without backslashes",
            ));
        }
        if !seen.insert(source.path) {
            return Err(SourceLintError::policy(format!(
                "duplicate source path `{}`",
                source.path
            )));
        }
    }
    Ok(())
}

fn omitted_row(source: &SourceDocument<'_>, reason: &str) -> SourceEvidence {
    SourceEvidence {
        path: source.path.to_owned(),
        content_hash: content_hash(source.bytes),
        support: SupportStatus::Incomplete,
        reason: Some(reason.to_owned()),
        metrics: unknown_metrics(),
    }
}

fn unknown_metrics() -> BTreeMap<String, MetricReading> {
    let mut metrics = BTreeMap::new();
    metrics.insert("meshes".to_owned(), MetricReading::unknown(PRODUCER_GEOMETRY));
    metrics.insert("triangles".to_owned(), MetricReading::unknown(PRODUCER_GEOMETRY));
    metrics.insert("materials".to_owned(), MetricReading::unknown(PRODUCER_MATERIALS));
    metrics
}

fn classify_source(
    project: Option<&Path>,
    target: &str,
    source: &SourceDocument<'_>,
    settings: &ResolvedLintSettings,
    hash: &str,
    scratch: &mut Option<ScratchDir>,
) -> Result<(SourceEvidence, Vec<LintFinding>, bool), SourceLintError> {
    if crate::texture_lint::is_texture_path(source.path) {
        return classify_texture(project, target, source, hash);
    }
    if !is_gltf_path(source.path) {
        return Ok((
            SourceEvidence {
                path: source.path.to_owned(),
                content_hash: hash.to_owned(),
                support: SupportStatus::Unsupported,
                reason: Some("not_gltf".to_owned()),
                metrics: unknown_metrics(),
            },
            Vec::new(),
            false,
        ));
    }
    let dir = scratch_dir(scratch)?;
    let name = if extension_is(source.path, "gltf") {
        "source.gltf"
    } else {
        "source.glb"
    };
    let path = write_atomic(dir, name, source.bytes)?;
    let geometry = lint_gltf(&path, &settings.geometry);
    let material = match lint_material_file(&path, &settings.materials, false) {
        Err(error) if error.code == "gltf_material_io" => {
            return Err(SourceLintError::io(error.message));
        }
        other => other,
    };
    let counted = material
        .as_ref()
        .ok()
        .and_then(|_| json_material_count(source.bytes, source.path));
    let (row, mut findings, counts_rules) = finish_gltf(source.path, hash, geometry, material, counted);
    if let Some(root) = project {
        let shared = crate::texture_lint::shared_usage_findings(root, source.path, source.bytes, target)
            .map_err(|error| {
                if error.starts_with("cannot read") {
                    SourceLintError::io(error)
                } else {
                    SourceLintError::policy(error)
                }
            })?;
        findings.extend(shared.into_iter().map(texture_finding_row));
    }
    Ok((row, findings, counts_rules))
}

fn classify_texture(
    project: Option<&Path>,
    target: &str,
    source: &SourceDocument<'_>,
    hash: &str,
) -> Result<(SourceEvidence, Vec<LintFinding>, bool), SourceLintError> {
    let platform = crate::texture_lint::platform_for_target(target);
    let (policy, settings) = if let Some(root) = project {
        let policy = crate::texture_policy::TexturePolicy::for_project(root, platform)
            .map_err(SourceLintError::policy)?;
        let settings = crate::texture_settings::TextureImporterSettings::for_source(&root.join(source.path));
        (policy, settings)
    } else {
        (
            crate::texture_policy::TexturePolicy::default(),
            crate::texture_settings::TextureImporterSettings::default(),
        )
    };
    let budget = crate::content_lint::ContentLimits::default().max_decoded_bytes;
    let outcome = crate::texture_lint::assess(&crate::texture_lint::TextureLintRequest {
        path: source.path,
        bytes: source.bytes,
        file_len: None,
        platform,
        policy,
        settings,
        usages: &[],
        max_decoded_bytes: budget,
    });
    let mut metrics = BTreeMap::new();
    metrics.insert("texture_width".to_owned(), metric_edge(outcome.width));
    metrics.insert("texture_height".to_owned(), metric_edge(outcome.height));
    metrics.insert(
        "decoded_bytes".to_owned(),
        match outcome.decoded_bytes {
            Some(value) => MetricReading::known(value, PRODUCER_TEXTURE),
            None => MetricReading::unknown(PRODUCER_TEXTURE),
        },
    );
    let findings = outcome.findings.into_iter().map(texture_finding_row).collect();
    Ok((
        SourceEvidence {
            path: source.path.to_owned(),
            content_hash: hash.to_owned(),
            support: if outcome.incomplete {
                SupportStatus::Incomplete
            } else {
                SupportStatus::Supported
            },
            reason: outcome
                .incomplete
                .then(|| crate::texture_lint::RULE_BUDGET_UNKNOWN.to_owned()),
            metrics,
        },
        findings,
        true,
    ))
}

fn metric_edge(value: Option<u32>) -> MetricReading {
    match value {
        Some(value) => MetricReading::known(u64::from(value), PRODUCER_TEXTURE),
        None => MetricReading::unknown(PRODUCER_TEXTURE),
    }
}

fn texture_finding_row(finding: crate::texture_lint::TextureLintFinding) -> LintFinding {
    LintFinding {
        path: finding.path,
        rule_id: finding.rule_id,
        location: finding.location,
        message: finding.message,
        severity: FindingSeverity::Warning,
        suppressed: false,
        suppression_reason: None,
        effect: Some("preserves_appearance".to_owned()),
    }
}

fn finish_gltf(
    path: &str,
    hash: &str,
    geometry: Result<GltfLintReport, GltfLintError>,
    material: Result<MaterialLintReport, crate::gltf_material_lint::MaterialLintError>,
    material_count: Option<u64>,
) -> (SourceEvidence, Vec<LintFinding>, bool) {
    let mut metrics = unknown_metrics();
    let mut findings = Vec::new();
    let mut incomplete = false;
    let mut failed = false;
    let mut reason = None;
    match geometry {
        Ok(report) => {
            metrics.insert(
                "meshes".to_owned(),
                MetricReading::known(u64_from(report.meshes), PRODUCER_GEOMETRY),
            );
            metrics.insert(
                "triangles".to_owned(),
                MetricReading::known(u64_from(report.triangles), PRODUCER_GEOMETRY),
            );
            findings.extend(findings_from_gltf_report(path, &report));
        }
        Err(error) => {
            failed = true;
            reason = Some(error.code().to_owned());
            if matches!(error, GltfLintError::Limit(_)) {
                incomplete = true;
            }
        }
    }
    match material {
        Ok(report) => {
            if let Some(count) = material_count {
                metrics.insert(
                    "materials".to_owned(),
                    MetricReading::known(count, PRODUCER_MATERIALS),
                );
            } else {
                failed = true;
                if reason.is_none() {
                    reason = Some("gltf_material_invalid".to_owned());
                }
            }
            findings.extend(findings_from_material_report(path, &report));
            if !report.compatible {
                failed = true;
                // The checked glTF loader refuses an unknown required extension
                // as `gltf_unreadable` before this producer records support.
                // Compatibility is the stable reason unless a work limit already
                // made the row incomplete. Bytes are not rewritten here.
                if !incomplete {
                    reason = Some("gltf_ext_unsupported".to_owned());
                }
            }
        }
        Err(error) => {
            failed = true;
            if reason.is_none() {
                reason = Some(error.code.to_owned());
            }
        }
    }
    let support = if incomplete {
        SupportStatus::Incomplete
    } else if failed {
        SupportStatus::Unsupported
    } else {
        SupportStatus::Supported
    };
    (
        SourceEvidence {
            path: path.to_owned(),
            content_hash: hash.to_owned(),
            support,
            reason: if support == SupportStatus::Supported {
                None
            } else {
                reason
            },
            metrics,
        },
        findings,
        true,
    )
}

fn json_material_count(bytes: &[u8], path: &str) -> Option<u64> {
    let json = json_slice(bytes, path)?;
    let value: serde_json::Value = serde_json::from_slice(json).ok()?;
    let count = value
        .get("materials")
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len);
    Some(u64_from(count))
}

fn json_slice<'a>(bytes: &'a [u8], path: &str) -> Option<&'a [u8]> {
    if extension_is(path, "gltf") {
        return Some(bytes);
    }
    if bytes.len() < 20 || &bytes[..4] != b"glTF" || &bytes[16..20] != b"JSON" {
        return None;
    }
    let len = u32::from_le_bytes(bytes[12..16].try_into().ok()?) as usize;
    bytes.get(20..20 + len)
}

fn u64_from(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn is_gltf_path(path: &str) -> bool {
    extension_is(path, "gltf") || extension_is(path, "glb")
}

fn extension_is(path: &str, want: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case(want))
}

fn apply_suppressions(findings: &mut [LintFinding], policy: &SourceLintPolicy, processed: &[Processed]) {
    for finding in findings {
        let Some(source) = processed.iter().find(|source| source.path == finding.path) else {
            continue;
        };
        for suppression in &policy.suppressions {
            if suppression.rule_id != finding.rule_id || suppression.path != finding.path {
                continue;
            }
            if suppression.content_hash != source.hash {
                continue;
            }
            if suppression
                .location
                .as_ref()
                .is_some_and(|location| location != &finding.location)
            {
                continue;
            }
            finding.suppressed = true;
            finding.suppression_reason = Some(suppression.reason.clone());
            break;
        }
    }
}

fn sort_findings(findings: &mut [LintFinding]) {
    findings.sort_by_key(|finding| {
        (
            finding.path.clone(),
            finding.rule_id.clone(),
            finding.location.clone(),
            finding.message.clone(),
            finding.suppressed,
        )
    });
}

#[derive(Serialize)]
struct SettingsCanon<'a> {
    target: &'a str,
    assets: Vec<AssetCanon<'a>>,
}

#[derive(Serialize)]
struct AssetCanon<'a> {
    path: &'a str,
    geometry: &'a GltfLintSettings,
    materials: &'a MaterialLintSettings,
    suppressions: Vec<SuppressionCanon<'a>>,
}

#[derive(Serialize)]
struct SuppressionCanon<'a> {
    rule_id: &'a str,
    path: &'a str,
    content_hash: &'a str,
    reason: &'a str,
    location: &'a str,
}

fn hash_sources(processed: &[Processed]) -> String {
    let mut text = String::new();
    let mut rows: Vec<&Processed> = processed.iter().collect();
    rows.sort_by_key(|row| row.path.clone());
    for row in rows {
        text.push_str(&row.path);
        text.push('\n');
        text.push_str(&row.hash);
        text.push('\n');
    }
    content_hash(text.as_bytes())
}

fn hash_settings(
    target: &str,
    policy: &SourceLintPolicy,
    processed: &[Processed],
) -> Result<String, SourceLintError> {
    let mut assets: Vec<&Processed> = processed.iter().collect();
    assets.sort_by_key(|row| row.path.clone());
    let canon = SettingsCanon {
        target,
        assets: assets
            .into_iter()
            .map(|row| AssetCanon {
                path: &row.path,
                geometry: &row.settings.geometry,
                materials: &row.settings.materials,
                suppressions: applicable_suppressions(policy, row),
            })
            .collect(),
    };
    let bytes = serde_json::to_vec(&canon).map_err(|e| SourceLintError::Report(e.to_string()))?;
    Ok(content_hash(&bytes))
}

fn applicable_suppressions<'a>(
    policy: &'a SourceLintPolicy,
    row: &'a Processed,
) -> Vec<SuppressionCanon<'a>> {
    let mut matched: Vec<&Suppression> = policy
        .suppressions
        .iter()
        .filter(|suppression| suppression.path == row.path && suppression.content_hash == row.hash)
        .collect();
    matched.sort_by_key(|suppression| {
        (
            suppression.rule_id.clone(),
            suppression.path.clone(),
            suppression.content_hash.clone(),
            suppression.location.clone().unwrap_or_default(),
        )
    });
    matched
        .into_iter()
        .map(|suppression| SuppressionCanon {
            rule_id: &suppression.rule_id,
            path: &suppression.path,
            content_hash: &suppression.content_hash,
            reason: &suppression.reason,
            location: suppression.location.as_deref().unwrap_or(""),
        })
        .collect()
}

fn hash_tool() -> String {
    let mut text = String::new();
    text.push_str(SOURCE_LINT_TOOL);
    text.push('\n');
    text.push_str(&SOURCE_LINT_REPORT_VERSION.to_string());
    text.push('\n');
    for rule in RULES {
        text.push_str(rule.id);
        text.push('\t');
        text.push_str(rule.producer);
        text.push('\t');
        text.push_str(rule.summary);
        text.push('\n');
    }
    content_hash(text.as_bytes())
}

#[derive(Serialize)]
struct EvidenceCanon<'a> {
    source_hash: &'a str,
    settings_hash: &'a str,
    target: &'a str,
    tool_hash: &'a str,
}

fn hash_evidence(
    target: &str,
    source_hash: &str,
    settings_hash: &str,
    tool_hash: &str,
) -> Result<String, SourceLintError> {
    let bytes = serde_json::to_vec(&EvidenceCanon {
        source_hash,
        settings_hash,
        target,
        tool_hash,
    })
    .map_err(|e| SourceLintError::Report(e.to_string()))?;
    Ok(content_hash(&bytes))
}

struct ScratchDir(PathBuf);

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scratch_dir(slot: &mut Option<ScratchDir>) -> Result<&Path, SourceLintError> {
    if slot.is_none() {
        let seq = SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-source-lint-{}-{seq}", std::process::id()));
        std::fs::create_dir(&dir)
            .map_err(|e| SourceLintError::io(format!("cannot create scratch dir: {e}")))?;
        *slot = Some(ScratchDir(dir));
    }
    slot.as_ref()
        .map(|dir| dir.0.as_path())
        .ok_or_else(|| SourceLintError::io("scratch dir was not created"))
}

fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf, SourceLintError> {
    let final_path = dir.join(name);
    let tmp_path = dir.join(format!("{name}.tmp"));
    let write_result = (|| {
        let mut file = std::fs::File::create(&tmp_path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp_path, &final_path)?;
        Ok::<(), std::io::Error>(())
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(SourceLintError::io(format!("cannot write scratch file: {error}")));
    }
    Ok(final_path)
}

/// Findings for a geometry report. Counts are not stored; locations are.
#[must_use]
pub fn findings_from_gltf_report(path: &str, report: &GltfLintReport) -> Vec<LintFinding> {
    let mut findings = Vec::new();
    for row in &report.duplicate_meshes {
        findings.push(finding(
            path,
            "gltf_duplicate_mesh",
            format!("/meshes/{}/duplicate_of/{}", row.duplicate, row.original),
            format!("mesh {} repeats mesh {}", row.duplicate, row.original),
            FindingSeverity::Warning,
        ));
    }
    for row in &report.coincident_triangles {
        findings.push(finding(
            path,
            "gltf_coincident_triangles",
            format!(
                "/meshes/{}/primitives/{}/triangles/{}",
                row.mesh, row.primitive, row.pair.suspect
            ),
            format!("keeper {}", row.pair.keeper),
            FindingSeverity::Warning,
        ));
    }
    for row in &report.robust_coincidences {
        let equivalent = row.equivalence == crate::gltf_lint::Equivalence::Equivalent;
        let mut detail = row.differences.clone();
        detail.extend(row.unavailable.iter().cloned());
        findings.push(finding(
            path,
            if equivalent {
                "gltf_coincident_equivalent"
            } else {
                "gltf_coincident_advisory"
            },
            format!(
                "/meshes/{}/primitives/{}/triangles/{}/keeper/{}/{}",
                row.mesh, row.suspect.primitive, row.suspect.triangle, row.keeper.primitive, row.keeper.triangle
            ),
            format!(
                "keeper primitive {} triangle {}; agreement {}; separation {:e} m of {:e} m; {}{}; safe_delete_candidate {}; fingerprint {}",
                row.keeper.primitive,
                row.keeper.triangle,
                match row.agreement {
                    crate::gltf_lint::Agreement::Exact => "exact",
                    crate::gltf_lint::Agreement::Tolerance => "tolerance",
                },
                row.max_separation_meters,
                row.tolerance_meters,
                match row.equivalence {
                    crate::gltf_lint::Equivalence::Equivalent => "equivalent",
                    crate::gltf_lint::Equivalence::Differs => "differs",
                    crate::gltf_lint::Equivalence::Unknown => "unknown",
                },
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", detail.join(", "))
                },
                row.safe_delete_candidate,
                row.fingerprint
            ),
            if equivalent {
                FindingSeverity::Warning
            } else {
                FindingSeverity::Info
            },
        ));
    }
    for row in &report.mesh_evidence {
        for (rule, kind, count, first) in [
            (
                "gltf_mesh_nonfinite",
                "nonfinite",
                row.non_finite,
                row.first_non_finite,
            ),
            (
                "gltf_mesh_degenerate",
                "degenerate",
                row.repeated_index,
                row.first_repeated_index,
            ),
            (
                "gltf_mesh_zero_area",
                "zero_area",
                row.zero_area,
                row.first_zero_area,
            ),
        ] {
            if count == 0 {
                continue;
            }
            findings.push(finding(
                path,
                rule,
                format!("/meshes/{}/primitives/{}/{kind}", row.mesh, row.primitive),
                format!(
                    "{count} of {} triangles (mode {}), first {}; excluded from coincidence",
                    row.triangles,
                    row.mode,
                    first.unwrap_or(0)
                ),
                FindingSeverity::Info,
            ));
        }
    }
    for row in &report.rig_channels {
        findings.push(finding(
            path,
            &row.code,
            format!(
                "/animations/{}/channels/{}/nodes/{}",
                row.animation, row.channel, row.node
            ),
            "channel needs review".to_owned(),
            FindingSeverity::Warning,
        ));
    }
    for row in &report.skipped_primitives {
        findings.push(finding(
            path,
            "gltf_skipped_primitive",
            format!(
                "/meshes/{}/primitives/{}/mode/{}",
                row.mesh, row.primitive, row.mode
            ),
            format!("primitive mode {} is not a triangle list", row.mode),
            FindingSeverity::Info,
        ));
    }
    findings
}

fn finding(
    path: &str,
    rule_id: &str,
    location: String,
    message: String,
    severity: FindingSeverity,
) -> LintFinding {
    LintFinding {
        path: path.to_owned(),
        rule_id: rule_id.to_owned(),
        location,
        message,
        severity,
        suppressed: false,
        suppression_reason: None,
        effect: None,
    }
}

/// A geometry report projected back from findings. Examined counts are not recovered.
#[must_use]
pub fn gltf_report_from_findings(findings: &[LintFinding]) -> GltfLintReport {
    let mut report = GltfLintReport::default();
    for finding in findings {
        let parts: Vec<&str> = finding.location.split('/').collect();
        match finding.rule_id.as_str() {
            "gltf_duplicate_mesh" => {
                if let (Some(duplicate), Some(original)) = (part_usize(&parts, 2), part_usize(&parts, 4)) {
                    report
                        .duplicate_meshes
                        .push(DuplicateMesh { original, duplicate });
                }
            }
            "gltf_coincident_triangles" => {
                if let (Some(mesh), Some(primitive), Some(suspect)) = (
                    part_usize(&parts, 2),
                    part_usize(&parts, 4),
                    part_usize(&parts, 6),
                ) {
                    report.coincident_triangles.push(CoincidentTriangles {
                        mesh,
                        primitive,
                        pair: crate::gltf_lint::CoincidentPair {
                            keeper: keeper_from_message(&finding.message),
                            suspect,
                        },
                    });
                }
            }
            "gltf_skipped_primitive" => {
                if let (Some(mesh), Some(primitive), Some(mode)) =
                    (part_usize(&parts, 2), part_usize(&parts, 4), part_u32(&parts, 6))
                {
                    report.skipped_primitives.push(SkippedPrimitive {
                        mesh,
                        primitive,
                        mode,
                    });
                }
            }
            rule if rule.starts_with("gltf_rig_") => {
                if let (Some(animation), Some(channel), Some(node)) = (
                    part_usize(&parts, 2),
                    part_usize(&parts, 4),
                    part_usize(&parts, 6),
                ) {
                    report.rig_channels.push(RigChannelWarning {
                        code: rule.to_owned(),
                        animation,
                        channel,
                        node,
                    });
                }
            }
            _ => {}
        }
    }
    report
}

fn part_usize(parts: &[&str], index: usize) -> Option<usize> {
    parts.get(index)?.parse().ok()
}

fn part_u32(parts: &[&str], index: usize) -> Option<u32> {
    parts.get(index)?.parse().ok()
}

fn keeper_from_message(message: &str) -> usize {
    message
        .strip_prefix("keeper ")
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|token| token.parse().ok())
        .unwrap_or(0)
}

/// Findings for a material report. The check-mode `findings` list is the evidence.
#[must_use]
pub fn findings_from_material_report(path: &str, report: &MaterialLintReport) -> Vec<LintFinding> {
    report
        .findings
        .iter()
        .map(|finding| LintFinding {
            path: path.to_owned(),
            rule_id: finding.code.clone(),
            location: finding.pointer.clone(),
            message: finding.message.clone(),
            severity: FindingSeverity::Warning,
            suppressed: false,
            suppression_reason: None,
            effect: Some(finding.effect.name().to_owned()),
        })
        .collect()
}

/// A check-mode material report projected back from findings. `changed` is false.
///
/// Input limits, usage, and profile stay at the engine defaults: this projection
/// does not rerun file admission and does not recover which profile produced the
/// rows. A finding with no recorded `effect`, or with an effect name this crate
/// does not know, is omitted. The projection does not invent a repair class.
/// `compatible` stays true and `extensions`, `texture_roles`, and the extension
/// walk counters stay empty: a finding list does not record importer capability.
#[must_use]
pub fn material_report_from_findings(findings: &[LintFinding]) -> MaterialLintReport {
    let rows: Vec<MaterialFinding> = findings
        .iter()
        .filter(|finding| !finding.rule_id.starts_with("gltf_"))
        .filter(|finding| {
            rules()
                .iter()
                .any(|rule| rule.id == finding.rule_id && rule.producer == PRODUCER_MATERIALS)
        })
        .filter_map(|finding| {
            let effect = match finding.effect.as_deref() {
                Some("preserves_appearance") => MaterialRepairEffect::PreservesAppearance,
                Some("changes_appearance") => MaterialRepairEffect::ChangesAppearance,
                _ => return None,
            };
            Some(MaterialFinding {
                code: finding.rule_id.clone(),
                material: material_index(&finding.location),
                pointer: finding.location.clone(),
                message: finding.message.clone(),
                effect,
            })
        })
        .collect();
    MaterialLintReport {
        remaining: rows.clone(),
        findings: rows,
        changed: false,
        ..MaterialLintReport::default()
    }
}

fn material_index(pointer: &str) -> Option<usize> {
    let mut parts = pointer.split('/');
    if parts.next() != Some("") || parts.next() != Some("materials") {
        return None;
    }
    parts.next()?.parse().ok()
}

/// Serde field list of the policy file and the evidence report, for `cargo xtask check-formats`.
#[must_use]
pub fn source_lint_schema_fingerprint() -> String {
    use regia_core::schema::{
        join_field_lists, serde_variant_names, serde_variant_shapes, try_serde_field_names,
    };
    fn fields<T: Deserialize<'static>>() -> Vec<String> {
        try_serde_field_names::<T>().unwrap_or_else(|error| vec![error])
    }
    fn variants<T: Deserialize<'static>>() -> Vec<String> {
        let mut list = serde_variant_names::<T>();
        list.extend(serde_variant_shapes::<T>());
        list
    }
    join_field_lists(&[
        fields::<SourceLintPolicy>(),
        fields::<RuleOverlay>(),
        fields::<GeometryOverlay>(),
        fields::<MaterialOverlay>(),
        fields::<AssetOverlay>(),
        fields::<Suppression>(),
        fields::<SourceLintReport>(),
        fields::<SourceEvidence>(),
        fields::<MetricReading>(),
        fields::<LintFinding>(),
        fields::<WorkCounters>(),
        variants::<SupportStatus>(),
        variants::<FindingSeverity>(),
    ])
}

/// Serde field list of the evidence report alone.
#[must_use]
pub fn source_lint_report_schema_fingerprint() -> String {
    use regia_core::schema::{
        join_field_lists, serde_variant_names, serde_variant_shapes, try_serde_field_names,
    };
    fn fields<T: Deserialize<'static>>() -> Vec<String> {
        try_serde_field_names::<T>().unwrap_or_else(|error| vec![error])
    }
    fn variants<T: Deserialize<'static>>() -> Vec<String> {
        let mut list = serde_variant_names::<T>();
        list.extend(serde_variant_shapes::<T>());
        list
    }
    join_field_lists(&[
        fields::<SourceLintReport>(),
        fields::<SourceEvidence>(),
        fields::<MetricReading>(),
        fields::<LintFinding>(),
        fields::<WorkCounters>(),
        variants::<SupportStatus>(),
        variants::<FindingSeverity>(),
    ])
}

/// No older policy version exists.
#[must_use]
pub fn source_lint_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The four precedence layer names, in apply order.
pub const PRECEDENCE: [&str; 4] = [
    "engine_defaults",
    "project_defaults",
    "target_profile",
    "asset_overlay",
];

#[cfg(test)]
mod tests {
    use super::*;

    const FINGERPRINT: &str = "\
format_version,defaults,profiles,assets,suppressions;\
geometry,materials;\
quantization,separation,exact_separation,uv_distance,anisotropy_ratio,anisotropy_floor,max_vertices,max_triangles,max_channels,max_animation_elements,max_findings,coincidence_mode,tolerance_meters,tolerance_relative,uv_tolerance,attribute_tolerance,min_area_square_meters,max_candidate_pairs,max_attribute_elements;\
default_metallic,default_roughness,default_emissive_strength,default_volume_thickness,default_attenuation_distance,classify_untextured,name_rules;\
glob,geometry,materials;\
rule_id,path,content_hash,reason,location;\
format_version,complete,target,evidence_key,source_hash,settings_hash,tool_hash,sources,findings,work;\
path,content_hash,support,reason,metrics;\
known,value,unit,producer;\
path,rule_id,location,message,severity,suppressed,suppression_reason,effect;\
sources_seen,sources_processed,bytes_hashed,findings,findings_kept,rules_checked;\
supported,unsupported,incomplete,0:unit,1:unit,2:unit;\
warning,info,0:unit,1:unit";

    #[test]
    fn schema_fingerprint_is_stable() {
        let fingerprint = source_lint_schema_fingerprint();
        assert_eq!(fingerprint, FINGERPRINT, "{fingerprint}");
        let hash = blake3::hash(fingerprint.as_bytes()).to_hex().to_string();
        eprintln!("source-lint schema hash {hash}");
        assert!(!source_lint_report_schema_fingerprint().is_empty());
        assert!(rules().windows(2).all(|pair| pair[0].id < pair[1].id));
    }
}
