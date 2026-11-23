// SPDX-License-Identifier: MPL-2.0
//! glTF material authoring policy and metadata-only repair (ADR-8061, ADR-8250).
//!
//! The default profile preserves glTF 2.0 factor multiplication, missing-factor
//! semantics and independent surface tint. The `compatibility` profile opts into
//! the older ORM, dielectric and volume-tint conventions, which change appearance.
//! Independent Rust from the Khronos glTF 2.0 material specifications. See
//! `docs/provenance.md`.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

const EMISSIVE: &str = "KHR_materials_emissive_strength";
const TRANSMISSION: &str = "KHR_materials_transmission";
const VOLUME: &str = "KHR_materials_volume";
const IOR: &str = "KHR_materials_ior";

/// First matching case-insensitive substring rule supplies optional authoring defaults.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MaterialNameRule {
    /// Substring of the material name; an empty substring is rejected.
    pub contains: String,
    /// Metalness for untextured materials (also replaces explicit 1 on a dielectric).
    pub metallic: Option<f64>,
    /// Roughness for untextured materials without an authored value.
    pub roughness: Option<f64>,
    /// Minimum emissive strength for an emissive material.
    pub emissive_strength: Option<f64>,
}

/// Which authoring conventions a lint pass may apply (ADR-8250).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterialLintProfile {
    /// Preserve glTF factor multiplication, missing-factor defaults and surface tint.
    /// Appearance-changing repairs are not selected.
    #[default]
    Standard,
    /// Opt in to ORM factor reset, dielectric missing-metal and volume-tint migration.
    /// These actions change authored appearance and require this explicit profile.
    Compatibility,
}

impl MaterialLintProfile {
    /// Stable JSON and CLI name: `standard` or `compatibility`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Compatibility => "compatibility",
        }
    }

    /// Whether this profile selects the appearance-changing ORM, dielectric and volume-tint repairs.
    #[must_use]
    pub fn changes_appearance(self) -> bool {
        matches!(self, Self::Compatibility)
    }

    /// One sentence naming the effective policy for CLI and agent reports.
    #[must_use]
    pub fn summary(self) -> &'static str {
        match self {
            Self::Standard => {
                "preserves glTF factor multiplication, missing-factor defaults and surface tint; appearance-changing repairs require an explicit profile or setting"
            }
            Self::Compatibility => {
                "selects appearance-changing ORM factor reset, dielectric missing-metal and volume-tint migration"
            }
        }
    }
}

/// Whether a repair keeps the shaded result or changes authored appearance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterialRepairEffect {
    /// Format repair, or a value the glTF specification already implies.
    PreservesAppearance,
    /// Changes authored shading. Requires an explicit profile or setting.
    ChangesAppearance,
}

impl MaterialRepairEffect {
    /// Stable JSON name: `preserves_appearance` or `changes_appearance`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::PreservesAppearance => "preserves_appearance",
            Self::ChangesAppearance => "changes_appearance",
        }
    }
}

/// Relative tolerance for an energy-preserving emissive repair (ADR-8250).
///
/// After repair, each nonnegative channel's color times strength differs from the
/// authored product by at most this fraction of `max(1, |authored product|)`.
/// The bound is the cancellation error of dividing by the peak and multiplying
/// the strength by that same peak, not a perceptual tolerance.
pub const EMISSIVE_PRODUCT_REL_TOLERANCE: f64 = 1e-9;

/// Most findings one material can record in a single pass.
///
/// Document-level rows (`extensions_used`, `extension_nesting`, image-source
/// indices) sit outside that per-material count. Callers assert the cap below
/// instead of a wall-clock budget.
pub const MAX_FINDINGS_PER_MATERIAL: usize = 24;

/// Deepest extension JSON value the coverage walk enters (ADR-8392).
pub const MAX_EXTENSION_DEPTH: usize = 32;

/// Most key and string bytes the coverage walk charges (ADR-8392).
///
/// The counter stops at this value. The unread tail is not changed.
pub const MAX_EXTENSION_BYTES: u64 = 65_536;

/// Content policy; defaults do not classify names or force daylight intensities.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MaterialLintSettings {
    /// Shared file-input and normalization ceilings for authoring tools.
    pub input_limits: crate::content_lint::ContentLimits,
    /// `standard` preserves valid glTF appearance. `compatibility` selects the
    /// appearance-changing ORM, dielectric and volume-tint conventions.
    pub profile: MaterialLintProfile,
    /// Metalness inserted for an untextured material that omits it, in [0, 1].
    /// The compatibility profile and `classify_untextured` use this value.
    /// The standard profile leaves an omitted factor absent (glTF semantic value 1)
    /// unless a name rule supplies one.
    pub default_metallic: f64,
    /// Roughness inserted for an untextured material that omits it, in [0, 1].
    /// The compatibility profile uses this value. The standard profile leaves an
    /// omitted factor absent (glTF semantic value 1) unless a name rule supplies one.
    pub default_roughness: f64,
    /// Missing strength for unit-range emission, nonnegative. HDR normalization
    /// starts from the authored strength (or glTF's implicit 1) instead.
    pub default_emissive_strength: f64,
    /// Optional thickness (meters, nonnegative) supplied when the compatibility
    /// profile moves tint into a volume that omits thickness. None preserves the
    /// glTF default. This fill changes a thin surface and runs only during that migration.
    pub default_volume_thickness: Option<f64>,
    /// Optional positive attenuation distance in meters, supplied when the
    /// compatibility profile moves a tint and the distance is absent. None
    /// preserves the glTF default.
    pub default_attenuation_distance: Option<f64>,
    /// Treat unmatched untextured names as `default_metallic` for the
    /// dielectric-at-1 guard. Off by default; matching explicit name rules
    /// still apply when this is off. This guard changes appearance and is
    /// itself the explicit selection; it does not require the compatibility profile.
    pub classify_untextured: bool,
    /// First matching rule wins. A matching rule is an explicit artistic selection
    /// on every profile.
    pub name_rules: Vec<MaterialNameRule>,
}

impl Default for MaterialLintSettings {
    fn default() -> Self {
        Self {
            input_limits: crate::content_lint::ContentLimits::default(),
            profile: MaterialLintProfile::Standard,
            default_metallic: 0.0,
            default_roughness: 1.0,
            default_emissive_strength: 1.0,
            default_volume_thickness: None,
            default_attenuation_distance: None,
            classify_untextured: false,
            name_rules: Vec::new(),
        }
    }
}

/// One material authoring violation, in material index and rule order.
///
/// `source_lint` copies `code` and `pointer` into its evidence rows (ADR-8248). Check mode
/// (`fix` false) is what evidence production calls.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MaterialFinding {
    /// Stable lowercase snake-case rule code.
    pub code: String,
    /// Material index, or `None` for a document-level finding.
    pub material: Option<usize>,
    /// JSON pointer identifying the field.
    pub pointer: String,
    /// Human-readable explanation.
    pub message: String,
    /// `preserves_appearance` or `changes_appearance`. Appearance-changing
    /// actions run only when a profile or setting explicitly selects them.
    pub effect: MaterialRepairEffect,
}

/// One extension named by the document and what the importer does with it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ExtensionEvidence {
    /// Extension name as written in the glTF document.
    pub name: String,
    /// True when `extensionsRequired` lists this name.
    pub required: bool,
    /// `cooked`, `side_table`, or `unsupported`.
    pub status: String,
}

/// One material texture slot and the color space that slot requires.
///
/// The image file name is not consulted. `color_space` is `srgb`, `linear`, or
/// `normal`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TextureRoleEvidence {
    /// Material index.
    pub material: usize,
    /// JSON pointer of the textureInfo object.
    pub pointer: String,
    /// Slot name, such as `base_color` or `metallic_roughness`.
    pub role: String,
    /// `srgb`, `linear`, or `normal`.
    pub color_space: String,
    /// UV channel the slot requires, when `texCoord` is a nonnegative integer.
    pub tex_coord: Option<u64>,
    /// `textures` index, when `index` is a nonnegative integer.
    pub texture: Option<u64>,
}

/// One extension the importer reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ImporterExtension {
    /// glTF extension name.
    pub name: &'static str,
    /// `cooked` or `side_table`.
    pub status: &'static str,
}

/// Extensions the current importer cooks or keeps in a side table.
///
/// This is the same table as [`crate::gltf_normalize::extension_support`].
/// An extension that is absent here is unsupported: optional use is preserved
/// and reported, and a required use fails compatibility.
#[must_use]
pub fn importer_capability() -> Vec<ImporterExtension> {
    crate::gltf_normalize::known_extension_support()
        .iter()
        .map(|(name, support)| ImporterExtension {
            name,
            status: support.name(),
        })
        .collect()
}

/// Findings before repair and those still present afterward.
#[derive(Clone, Debug, Serialize)]
pub struct MaterialLintReport {
    /// Effective shared input policy (file entry points).
    pub input_limits: crate::content_lint::ContentLimits,
    /// Deterministic file admission and IO counters.
    pub input_usage: crate::content_lint::ContentUsage,
    /// Original findings; check mode leaves the input untouched.
    pub findings: Vec<MaterialFinding>,
    /// Post-fix findings, or the original findings in check mode.
    pub remaining: Vec<MaterialFinding>,
    /// Whether fix mode changed metadata.
    pub changed: bool,
    /// Profile that produced this report.
    pub profile: MaterialLintProfile,
    /// False when `extensionsRequired` names an extension the importer cannot read.
    ///
    /// Unknown optional extensions do not clear this flag. They stay in the file
    /// and are listed in [`Self::extensions`] with status `unsupported`.
    pub compatible: bool,
    /// Extensions named by `extensionsUsed` or `extensionsRequired` after
    /// declaration completion, with importer support.
    pub extensions: Vec<ExtensionEvidence>,
    /// Texture slots in use and the color space each slot requires.
    pub texture_roles: Vec<TextureRoleEvidence>,
    /// Extension JSON values the bounded walk entered.
    pub extension_nodes: u64,
    /// Key and string bytes the bounded walk charged, at most [`MAX_EXTENSION_BYTES`].
    pub extension_bytes: u64,
}

impl Default for MaterialLintReport {
    fn default() -> Self {
        Self {
            input_limits: crate::content_lint::ContentLimits::default(),
            input_usage: crate::content_lint::ContentUsage::default(),
            findings: Vec::new(),
            remaining: Vec::new(),
            changed: false,
            profile: MaterialLintProfile::Standard,
            compatible: true,
            extensions: Vec::new(),
            texture_roles: Vec::new(),
            extension_nodes: 0,
            extension_bytes: 0,
        }
    }
}

/// A malformed document, invalid policy, or file operation failure.
#[derive(Debug, thiserror::Error)]
#[error("{code}: {message}")]
pub struct MaterialLintError {
    /// Stable lowercase snake-case error code.
    pub code: &'static str,
    /// Details, including a JSON pointer when relevant.
    pub message: String,
}

fn invalid(message: impl Into<String>) -> MaterialLintError {
    MaterialLintError {
        code: "gltf_material_invalid",
        message: message.into(),
    }
}

/// Inspect and optionally repair caller-owned glTF material JSON. Geometry and unknown fields survive.
/// File admission limits are enforced by `lint_material_file` and
/// `lint_material_file_bounded`; this in-memory API does not load a source file.
///
/// The standard profile does not apply appearance-changing conventions. Select
/// `MaterialLintProfile::Compatibility`, a name rule, or `classify_untextured`
/// to do so. The report's `profile` is the effective policy.
///
/// # Errors
/// Returns `gltf_material_invalid` for malformed material data and
/// `gltf_material_settings` for invalid policy. Errors never mutate `document`.
pub fn lint_materials(
    document: &mut Value,
    settings: &MaterialLintSettings,
    fix: bool,
) -> Result<MaterialLintReport, MaterialLintError> {
    lint_materials_with_checkpoint(document, settings, fix, &mut || Ok(()))
}

fn lint_materials_with_checkpoint(
    document: &mut Value,
    settings: &MaterialLintSettings,
    fix: bool,
    checkpoint: &mut dyn FnMut() -> Result<(), MaterialLintError>,
) -> Result<MaterialLintReport, MaterialLintError> {
    checkpoint()?;
    validate_settings(settings)?;
    validate_document(document)?;
    let mut repaired = document.clone();
    let first = repair(&mut repaired, settings, checkpoint)?;
    let material_count = repaired
        .get("materials")
        .and_then(Value::as_array)
        .map_or(0, |materials| materials.len());
    debug_assert!(first.findings.len() <= material_count * MAX_FINDINGS_PER_MATERIAL + 4);
    let changed = fix && repaired != *document;
    let remaining = if fix {
        repair(&mut repaired.clone(), settings, checkpoint)?.findings
    } else {
        first.findings.clone()
    };
    if changed {
        *document = repaired;
    }
    Ok(MaterialLintReport {
        findings: first.findings,
        remaining,
        changed,
        input_limits: settings.input_limits.clone(),
        input_usage: crate::content_lint::ContentUsage::default(),
        profile: settings.profile,
        compatible: first.compatible,
        extensions: first.extensions,
        texture_roles: first.texture_roles,
        extension_nodes: first.extension_nodes,
        extension_bytes: first.extension_bytes,
    })
}

fn validate_settings(s: &MaterialLintSettings) -> Result<(), MaterialLintError> {
    let unit = |v: f64| v.is_finite() && (0.0..=1.0).contains(&v);
    let strength = |v: f64| v.is_finite() && v >= 0.0;
    if !unit(s.default_metallic)
        || !unit(s.default_roughness)
        || !strength(s.default_emissive_strength)
        || s.default_volume_thickness.is_some_and(|v| !strength(v))
        || s.default_attenuation_distance
            .is_some_and(|v| !v.is_finite() || v <= 0.0)
        || s.name_rules.iter().any(|r| {
            r.contains.is_empty()
                || r.metallic.is_some_and(|v| !unit(v))
                || r.roughness.is_some_and(|v| !unit(v))
                || r.emissive_strength.is_some_and(|v| !strength(v))
        })
    {
        return Err(MaterialLintError {
            code: "gltf_material_settings",
            message:
                "factors must be in [0,1], strengths/thickness finite and nonnegative, attenuation distance positive and finite, and name substrings nonempty"
                    .into(),
        });
    }
    Ok(())
}

fn object<'a>(v: &'a Value, at: &str) -> Result<&'a Map<String, Value>, MaterialLintError> {
    v.as_object()
        .ok_or_else(|| invalid(format!("{at}: expected object")))
}

fn numeric(v: &Value, at: &str) -> Result<(), MaterialLintError> {
    if v.as_f64().is_some_and(f64::is_finite) {
        Ok(())
    } else {
        Err(invalid(format!("{at}: expected finite number")))
    }
}

fn vector(v: &Value, n: usize, at: &str) -> Result<(), MaterialLintError> {
    let a = v
        .as_array()
        .filter(|a| a.len() == n)
        .ok_or_else(|| invalid(format!("{at}: expected {n} numbers")))?;
    for c in a {
        numeric(c, at)?;
    }
    Ok(())
}

fn validate_document(g: &Value) -> Result<(), MaterialLintError> {
    let root = object(g, "/")?;
    if g.pointer("/asset/version").and_then(Value::as_str) != Some("2.0") {
        return Err(invalid("/asset/version: expected 2.0"));
    }
    for key in ["extensionsUsed", "extensionsRequired"] {
        if let Some(v) = root.get(key)
            && !v.as_array().is_some_and(|a| a.iter().all(Value::is_string))
        {
            return Err(invalid(format!("/{key}: expected string array")));
        }
    }
    let Some(materials) = root.get("materials") else {
        return Ok(());
    };
    let materials = materials
        .as_array()
        .ok_or_else(|| invalid("/materials: expected array"))?;
    for (i, m) in materials.iter().enumerate() {
        let at = format!("/materials/{i}");
        object(m, &at)?;
        if let Some(v) = m.get("name")
            && !v.is_string()
        {
            return Err(invalid(format!("{at}/name: expected string")));
        }
        if let Some(pbr) = m.get("pbrMetallicRoughness") {
            object(pbr, &format!("{at}/pbrMetallicRoughness"))?;
            for key in ["metallicFactor", "roughnessFactor"] {
                if let Some(v) = pbr.get(key) {
                    numeric(v, &format!("{at}/pbrMetallicRoughness/{key}"))?;
                }
            }
            if let Some(v) = pbr.get("baseColorFactor") {
                vector(v, 4, &format!("{at}/pbrMetallicRoughness/baseColorFactor"))?;
            }
            for key in ["metallicRoughnessTexture", "baseColorTexture"] {
                if let Some(v) = pbr.get(key) {
                    texture(v, &format!("{at}/pbrMetallicRoughness/{key}"))?;
                }
            }
        }
        if let Some(v) = m.get("emissiveFactor") {
            vector(v, 3, &format!("{at}/emissiveFactor"))?;
        }
        if let Some(v) = m.get("emissiveTexture") {
            texture(v, &format!("{at}/emissiveTexture"))?;
        }
        if let Some(ext) = m.get("extensions") {
            for (name, value) in object(ext, &format!("{at}/extensions"))? {
                object(value, &format!("{at}/extensions/{name}"))?;
                let keys: &[&str] = match name.as_str() {
                    EMISSIVE => &["emissiveStrength"],
                    TRANSMISSION => &["transmissionFactor"],
                    VOLUME => &["thicknessFactor", "attenuationDistance"],
                    IOR => &["ior"],
                    _ => &[],
                };
                for key in keys {
                    if let Some(v) = value.get(key) {
                        numeric(v, &format!("{at}/extensions/{name}/{key}"))?;
                    }
                }
                if name == VOLUME
                    && let Some(v) = value.get("attenuationColor")
                {
                    vector(v, 3, &format!("{at}/extensions/{name}/attenuationColor"))?;
                }
            }
        }
    }
    Ok(())
}

fn texture(v: &Value, at: &str) -> Result<(), MaterialLintError> {
    object(v, at)?;
    if v.get("index").and_then(Value::as_u64).is_none() {
        return Err(invalid(format!("{at}/index: expected nonnegative integer")));
    }
    if let Some(tex_coord) = v.get("texCoord") {
        numeric(tex_coord, &format!("{at}/texCoord"))?;
    }
    if let Some(extensions) = v.get("extensions") {
        let map = object(extensions, &format!("{at}/extensions"))?;
        if let Some(transform) = map.get("KHR_texture_transform") {
            let transform = object(transform, &format!("{at}/extensions/KHR_texture_transform"))?;
            if let Some(tex_coord) = transform.get("texCoord") {
                numeric(
                    tex_coord,
                    &format!("{at}/extensions/KHR_texture_transform/texCoord"),
                )?;
            }
        }
    }
    Ok(())
}

fn finding(
    out: &mut Vec<MaterialFinding>,
    i: usize,
    code: &str,
    path: &str,
    message: &str,
    effect: MaterialRepairEffect,
) {
    out.push(MaterialFinding {
        code: code.into(),
        material: Some(i),
        pointer: format!("/materials/{i}/{path}"),
        message: message.into(),
        effect,
    });
}

fn spec_effect(value: f64, spec_default: f64) -> MaterialRepairEffect {
    if value == spec_default {
        MaterialRepairEffect::PreservesAppearance
    } else {
        MaterialRepairEffect::ChangesAppearance
    }
}

fn number(v: &Value, fallback: f64) -> f64 {
    v.as_f64().unwrap_or(fallback)
}

fn clamp_factor(v: &mut Value, out: &mut Vec<MaterialFinding>, i: usize, path: &str) {
    if let Some(n) = v.as_f64() {
        let clamped = n.clamp(0.0, 1.0);
        if n != clamped {
            finding(
                out,
                i,
                "factors_in_range",
                path,
                "factor must lie in [0,1]",
                MaterialRepairEffect::PreservesAppearance,
            );
            *v = json!(clamped);
        }
    } else if let Some(a) = v.as_array_mut()
        && a.iter().any(|v| !(0.0..=1.0).contains(&number(v, 0.0)))
    {
        finding(
            out,
            i,
            "factors_in_range",
            path,
            "color components must lie in [0,1]",
            MaterialRepairEffect::PreservesAppearance,
        );
        for v in a {
            *v = json!(number(v, 0.0).clamp(0.0, 1.0));
        }
    }
}

struct LintPass {
    findings: Vec<MaterialFinding>,
    compatible: bool,
    extensions: Vec<ExtensionEvidence>,
    texture_roles: Vec<TextureRoleEvidence>,
    extension_nodes: u64,
    extension_bytes: u64,
}

fn repair(
    g: &mut Value,
    s: &MaterialLintSettings,
    checkpoint: &mut dyn FnMut() -> Result<(), MaterialLintError>,
) -> Result<LintPass, MaterialLintError> {
    let mut out = Vec::new();
    let mut extensions = std::collections::BTreeSet::new();
    if let Some(materials) = g.get_mut("materials").and_then(Value::as_array_mut) {
        for (i, m) in materials.iter_mut().enumerate() {
            checkpoint()?;
            let name = m.get("name").and_then(Value::as_str).unwrap_or("").to_lowercase();
            let rule = s
                .name_rules
                .iter()
                .find(|r| name.contains(&r.contains.to_lowercase()));
            repair_factors(m, i, s, rule, &mut out);
            repair_emissive(m, i, s, rule, &mut out)?;
            repair_transmission(m, i, s, &mut out);
            if let Some(ext) = m.get("extensions").and_then(Value::as_object) {
                extensions.extend(ext.keys().cloned());
            }
        }
    }
    let mut used: Vec<String> = g
        .get("extensionsUsed")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
        .unwrap_or_default();
    // Preserve existing declaration order, including extensions used outside materials.
    for extension in extensions {
        if !used.contains(&extension) {
            out.push(MaterialFinding {
                code: "extensions_used".into(),
                material: None,
                pointer: "/extensionsUsed".into(),
                message: format!("declare {extension}"),
                effect: MaterialRepairEffect::PreservesAppearance,
            });
            used.push(extension);
        }
    }
    if !used.is_empty() {
        g["extensionsUsed"] = json!(used);
    }
    let coverage = coverage::augment(g, &mut out);
    Ok(LintPass {
        findings: out,
        compatible: coverage.compatible,
        extensions: coverage.extensions,
        texture_roles: coverage.texture_roles,
        extension_nodes: coverage.extension_nodes,
        extension_bytes: coverage.extension_bytes,
    })
}

/// glTF 2.0 multiplies `metallicFactor` (default 1) and `roughnessFactor` (default 1)
/// by the metallic-roughness texture. The standard profile leaves that product alone.
/// The compatibility profile resets both factors to 1 and fills omitted untextured
/// factors from the policy defaults (ADR-8250).
fn repair_factors(
    m: &mut Value,
    i: usize,
    s: &MaterialLintSettings,
    rule: Option<&MaterialNameRule>,
    out: &mut Vec<MaterialFinding>,
) {
    let orm = m
        .pointer("/pbrMetallicRoughness/metallicRoughnessTexture")
        .is_some();
    let compatibility = s.profile == MaterialLintProfile::Compatibility;
    let factors = [
        (
            "metallicFactor",
            1.0_f64,
            rule.and_then(|r| r.metallic),
            s.default_metallic,
        ),
        (
            "roughnessFactor",
            1.0,
            rule.and_then(|r| r.roughness),
            s.default_roughness,
        ),
    ];
    for (key, spec_default, rule_value, policy_default) in factors {
        let path = format!("pbrMetallicRoughness/{key}");
        let present = m.pointer(&format!("/pbrMetallicRoughness/{key}")).is_some();
        if !present {
            let fill = if orm {
                compatibility.then_some(1.0)
            } else if let Some(value) = rule_value {
                Some(value)
            } else {
                compatibility.then_some(policy_default)
            };
            if let Some(value) = fill {
                let message = if value == spec_default {
                    "author an explicit factor; the value matches the glTF default"
                } else {
                    "author an explicit factor; this value differs from the glTF default and changes appearance"
                };
                finding(
                    out,
                    i,
                    "factor_missing",
                    &path,
                    message,
                    spec_effect(value, spec_default),
                );
                if !m.get("pbrMetallicRoughness").is_some_and(Value::is_object) {
                    m["pbrMetallicRoughness"] = json!({});
                }
                m["pbrMetallicRoughness"][key] = json!(value);
            }
        } else if let Some(slot) = m.pointer_mut(&format!("/pbrMetallicRoughness/{key}")) {
            clamp_factor(slot, out, i, &path);
            if orm && compatibility && number(slot, 1.0) != 1.0 {
                finding(
                    out,
                    i,
                    "orm_factors",
                    &path,
                    "compatibility profile resets both ORM factors to 1; this changes appearance",
                    MaterialRepairEffect::ChangesAppearance,
                );
                *slot = json!(1.0);
            }
        }
    }
    let classified_metallic = rule
        .and_then(|r| r.metallic)
        .or(s.classify_untextured.then_some(s.default_metallic));
    if !orm
        && m.pointer("/pbrMetallicRoughness/metallicFactor")
            .and_then(Value::as_f64)
            == Some(1.0)
        && classified_metallic == Some(0.0)
    {
        finding(
            out,
            i,
            "metallic_name_policy",
            "pbrMetallicRoughness/metallicFactor",
            "name policy classifies this surface as dielectric; this changes appearance",
            MaterialRepairEffect::ChangesAppearance,
        );
        m["pbrMetallicRoughness"]["metallicFactor"] = json!(0.0);
    }
    if let Some(bc) = m.pointer_mut("/pbrMetallicRoughness/baseColorFactor") {
        clamp_factor(bc, out, i, "pbrMetallicRoughness/baseColorFactor");
    }
}

fn repair_emissive(
    m: &mut Value,
    i: usize,
    s: &MaterialLintSettings,
    rule: Option<&MaterialNameRule>,
    out: &mut Vec<MaterialFinding>,
) -> Result<(), MaterialLintError> {
    let mut peak = 0.0_f64;
    if let Some(ef) = m.get_mut("emissiveFactor").and_then(Value::as_array_mut) {
        for v in ef.iter() {
            peak = peak.max(number(v, 0.0));
        }
        let divisor = peak.max(1.0);
        if ef.iter().any(|v| !(0.0..=1.0).contains(&number(v, 0.0))) {
            finding(
                out,
                i,
                "factors_in_range",
                "emissiveFactor",
                "normalize HDR emission into emissive strength and clamp negative components",
                MaterialRepairEffect::PreservesAppearance,
            );
            for v in ef {
                *v = json!(number(v, 0.0).max(0.0) / divisor);
            }
        }
    }
    let emissive = peak > 0.0 || m.get("emissiveTexture").is_some();
    let existing = m
        .pointer("/extensions/KHR_materials_emissive_strength/emissiveStrength")
        .and_then(Value::as_f64);
    if !emissive && existing.is_none() {
        return Ok(());
    }
    let base = existing
        .unwrap_or(if peak > 1.0 {
            1.0
        } else {
            s.default_emissive_strength
        })
        .max(0.0);
    // color × strength is unchanged when the peak scales both sides (ADR-8250).
    let energy = base * peak.max(1.0);
    if !energy.is_finite() {
        return Err(invalid(format!("/materials/{i}: emissive strength overflow")));
    }
    let minimum = if emissive {
        rule.and_then(|r| r.emissive_strength).unwrap_or(0.0)
    } else {
        0.0
    };
    let strength = energy.max(minimum);
    if existing != Some(strength) {
        let effect = if strength > energy {
            MaterialRepairEffect::ChangesAppearance
        } else {
            MaterialRepairEffect::PreservesAppearance
        };
        finding(
            out,
            i,
            "emissive_strength",
            "extensions/KHR_materials_emissive_strength/emissiveStrength",
            "supply a finite nonnegative emissive strength; HDR normalization preserves the color-strength product",
            effect,
        );
        m["extensions"][EMISSIVE]["emissiveStrength"] = json!(strength);
    }
    Ok(())
}

fn repair_transmission(
    m: &mut Value,
    i: usize,
    settings: &MaterialLintSettings,
    out: &mut Vec<MaterialFinding>,
) {
    if m.pointer("/extensions/KHR_materials_transmission").is_none() {
        return;
    }
    if let Some(v) = m.pointer_mut("/extensions/KHR_materials_transmission/transmissionFactor") {
        clamp_factor(
            v,
            out,
            i,
            "extensions/KHR_materials_transmission/transmissionFactor",
        );
    }
    if m.pointer("/extensions/KHR_materials_volume").is_some() {
        // Standard profile: base color and attenuation stay independent, and a
        // thickness of 0 (the spec default) stays thin-walled. Compatibility
        // reproduces the previous tool, which migrates tint whenever the volume
        // extension is present, including thickness 0 (ADR-8250).
        if settings.profile == MaterialLintProfile::Compatibility {
            let tint = m
                .pointer("/pbrMetallicRoughness/baseColorFactor")
                .and_then(Value::as_array)
                .cloned();
            if let Some(tint) = tint
                && tint[..3].iter().any(|v| number(v, 1.0) != 1.0)
            {
                finding(
                    out,
                    i,
                    "transmission_base_white",
                    "pbrMetallicRoughness/baseColorFactor",
                    "compatibility profile uses a white base and puts tint in attenuationColor; this changes appearance",
                    MaterialRepairEffect::ChangesAppearance,
                );
                if m["extensions"][VOLUME].get("attenuationColor").is_none() {
                    m["extensions"][VOLUME]["attenuationColor"] = json!(&tint[..3]);
                }
                m["pbrMetallicRoughness"]["baseColorFactor"] = json!([1.0, 1.0, 1.0, tint[3]]);
                for (key, value) in [
                    ("thicknessFactor", settings.default_volume_thickness),
                    ("attenuationDistance", settings.default_attenuation_distance),
                ] {
                    if m["extensions"][VOLUME].get(key).is_none()
                        && let Some(value) = value
                    {
                        m["extensions"][VOLUME][key] = json!(value);
                    }
                }
            }
        }
        if let Some(v) = m.pointer_mut("/extensions/KHR_materials_volume/attenuationColor") {
            clamp_factor(v, out, i, "extensions/KHR_materials_volume/attenuationColor");
        }
    }
    if m.pointer("/extensions/KHR_materials_ior").is_none() {
        finding(
            out,
            i,
            "transmission_ior",
            "extensions/KHR_materials_ior",
            "author the standard IOR explicitly for transmission",
            MaterialRepairEffect::PreservesAppearance,
        );
        m["extensions"][IOR] = json!({"ior":1.5});
    }
}

mod coverage;
mod file;
pub use file::{lint_material_file, lint_material_file_bounded};
