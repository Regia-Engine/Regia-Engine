// SPDX-License-Identifier: MPL-2.0
//! MaterialX (`.mtlx`) import (ADR-3341): a MaterialX document's surface material becomes a
//! [`regia_matgraph::MaterialGraph`], which the matgraph cook then compiles and cooks like a
//! `.matgraph.ron`.
//!
//! Sources: the public MaterialX Specification (document structure, node graphs, custom nodes,
//! the standard node library) and the public Standard Surface and OpenPBR Surface
//! specifications. The mapping is original code.
//!
//! What is mapped:
//!
//! - **Surface shaders.** `standard_surface` and `open_pbr_surface` onto the graph's surface
//!   outputs (base colour, metallic, roughness, normal, emissive, alpha) and its shading block
//!   (clearcoat, anisotropy). Lobes Regia's lit model does not have (specular weight, colour and
//!   IOR, sheen/fuzz, subsurface, thin film, coat colour) are approximated or dropped, each with a
//!   `mtlx_lossy_input` warning; the table is in `book/agents/materialx-import.md`.
//! - **Standard library nodes** that exported materials typically use: `image`, `tiledimage`,
//!   `constant`, arithmetic, `mix`, `clamp`, `power`, `dotproduct`, `normalize`, `normalmap`,
//!   `texcoord`, `position`, `normal`, `extract`, `combine2/3/4`, `separate2/3/4`, `swizzle`,
//!   `convert`, `smoothstep`, `ifgreater`/`ifgreatereq`/`ifequal` (static when their inputs are
//!   constant), `noise2d`/`noise3d`/`fractal3d` (onto the matgraph `Noise` node) and a few more.
//! - **Node graphs** are flattened; a custom `nodedef` with a node graph implementation is
//!   expanded at each use. Interface inputs of node graphs and nodedef inputs become material
//!   parameters with the document's values as defaults (at most
//!   [`regia_matgraph::MAX_PARAMS`]).
//! - **Unsupported nodes** give one `mtlx_unsupported_node` warning each and a fallback
//!   constant, so the material still compiles.
//!
//! Image paths resolve relative to the document (plus any `fileprefix`) to project paths, and
//! the cook imports them through the graph texture path (`<image>#rgba8`). MaterialX texture
//! coordinates have their origin at the bottom left and Regia's at the top left, so the importer
//! flips `v` where the graph reads a texture coordinate (`flip_v` in the settings).

mod doc;
mod surface;
mod translate;

#[cfg(test)]
mod tests;

use std::path::Path;

use regia_matgraph::{MaterialGraph, ValueType};
use serde::{Deserialize, Serialize};

/// The importer id written into a `.mtlx` file's `.meta`.
pub const MATERIALX_IMPORTER: &str = "materialx";

/// Largest `.mtlx` file the importer reads, in bytes.
pub const MAX_SOURCE_BYTES: usize = 8 << 20;

/// Most graph nodes one import may emit (flattened node graphs included).
pub const MAX_EMITTED_NODES: usize = 4096;

/// Most XML nodes a document may hold.
pub(crate) const MAX_XML_NODES: u32 = 500_000;

/// Deepest chain of node references the importer follows.
pub(crate) const MAX_DEPTH: usize = 192;

/// Most custom-node expansions one import may perform.
pub(crate) const MAX_EXPANSIONS: usize = 512;

/// Warning code: a node the importer does not map (one per node; a fallback constant stands in).
pub const WARN_UNSUPPORTED_NODE: &str = "mtlx_unsupported_node";
/// Warning code: an input mapped approximately or dropped.
pub const WARN_LOSSY_INPUT: &str = "mtlx_lossy_input";
/// Warning code: more images than the graph has texture slots.
pub const WARN_TEXTURE_LIMIT: &str = "mtlx_texture_limit";
/// Warning code: more parameters than the graph has slots (the rest stay constants).
pub const WARN_PARAM_LIMIT: &str = "mtlx_param_limit";
/// Warning code: an image path that is absolute, a URI, a UDIM set, or leaves the project.
pub const WARN_TEXTURE_PATH: &str = "mtlx_texture_path";
/// Warning code: a value that does not parse for its type (the default is used).
pub const WARN_BAD_VALUE: &str = "mtlx_bad_value";
/// Warning code: a connection to a node or output that does not exist (the default is used).
pub const WARN_MISSING_NODE: &str = "mtlx_missing_node";
/// Warning code: the document has more materials than the one imported.
pub const WARN_EXTRA_MATERIAL: &str = "mtlx_extra_material";

/// The lighting model the imported graph targets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShadingModel {
    /// Regia's lit model (`specs/13` § 7), with clearcoat and anisotropy from the shader.
    #[default]
    Lit,
    /// Unlit: the base colour is shown as is (the graph's `shading.unlit`).
    Unlit,
}

/// How the imported graph treats coverage.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum AlphaPolicy {
    /// Opaque unless the shader's opacity or transmission is set, then alpha blend.
    #[default]
    Auto,
    /// Always opaque (opacity and transmission are dropped).
    Opaque,
    /// Alpha test against this cutoff.
    Mask(f32),
    /// Always alpha blend.
    Blend,
}

/// `.mtlx` importer settings (the `.meta` `settings` string, RON).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MaterialXSettings {
    /// Target shading model.
    pub shading_model: ShadingModel,
    /// The `surfacematerial` to import by name; `None` imports the first.
    pub material: Option<String>,
    /// Coverage policy.
    pub alpha: AlphaPolicy,
    /// Standard Surface `emission` is a unitless weight: emitted radiance in nits is
    /// `emission * emission_color * emission_scale`. OpenPBR's `emission_luminance` is already
    /// in nits and ignores this.
    pub emission_scale: f32,
    /// Flip `v` between MaterialX's bottom-left texture origin and Regia's top-left one.
    pub flip_v: bool,
    /// Expose the shader's unconnected inputs (base colour, metalness, roughness, emission) as
    /// material parameters, after the node graph and nodedef interface inputs.
    pub expose_surface_params: bool,
}

impl Default for MaterialXSettings {
    fn default() -> Self {
        Self {
            shading_model: ShadingModel::Lit,
            material: None,
            alpha: AlphaPolicy::Auto,
            emission_scale: 1.0,
            flip_v: true,
            expose_surface_params: true,
        }
    }
}

impl MaterialXSettings {
    /// Settings from a `.meta` `settings` string; blank is the defaults.
    ///
    /// # Errors
    ///
    /// [`MaterialXError::Settings`] for a malformed string or a non-finite number.
    pub fn from_meta_string(settings: Option<&str>) -> Result<Self, MaterialXError> {
        let parsed: Self = match settings.filter(|s| !s.trim().is_empty()) {
            Some(s) => ron::from_str(s).map_err(|e| MaterialXError::Settings(e.to_string()))?,
            None => Self::default(),
        };
        let cutoff_ok = match parsed.alpha {
            AlphaPolicy::Mask(c) => c.is_finite(),
            _ => true,
        };
        if !parsed.emission_scale.is_finite() || !cutoff_ok {
            return Err(MaterialXError::Settings(
                "emission_scale and the alpha cutoff must be finite".into(),
            ));
        }
        Ok(parsed)
    }
}

/// Why a MaterialX import failed. [`MaterialXError::code`] is the agent-protocol error code.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum MaterialXError {
    /// The file could not be read.
    #[error("MaterialX file read error: {0}")]
    Io(String),
    /// The file is larger than [`MAX_SOURCE_BYTES`].
    #[error("MaterialX file is {size} bytes, more than the limit of {limit}")]
    TooLarge {
        /// File size in bytes.
        size: usize,
        /// The limit.
        limit: usize,
    },
    /// The bytes are not UTF-8.
    #[error("MaterialX file is not UTF-8: {0}")]
    NotUtf8(String),
    /// The XML does not parse.
    #[error("MaterialX XML: {0}")]
    Xml(String),
    /// The root element is not `<materialx>`.
    #[error("the root element is <{0}>, not <materialx>")]
    NotMaterialX(String),
    /// The document's version is not a 1.x version.
    #[error("MaterialX version `{0}` is not supported (1.x documents are)")]
    Version(String),
    /// A structural problem (a nameless element, a duplicate name).
    #[error("line {line}: {message}")]
    Structure {
        /// 1-based line.
        line: u32,
        /// What is wrong.
        message: String,
    },
    /// No surface material (or the named one) was found.
    #[error("no surface material to import: {0}")]
    NoMaterial(String),
    /// The material's surface shader is not one the importer maps.
    #[error("surface shader `{name}` is a `{category}`; standard_surface and open_pbr_surface are supported")]
    UnsupportedShader {
        /// Shader node name.
        name: String,
        /// Its category.
        category: String,
    },
    /// The node references form a cycle.
    #[error("node references form a cycle at `{0}`")]
    Cycle(String),
    /// A size limit was exceeded.
    #[error("MaterialX import limit: {0}")]
    Limit(String),
    /// The `.meta` settings do not parse.
    #[error("MaterialX import settings: {0}")]
    Settings(String),
}

impl MaterialXError {
    /// The agent-protocol error code (lowercase ascii, digits and `_`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Io(_) => "mtlx_io",
            Self::TooLarge { .. } => "mtlx_too_large",
            Self::NotUtf8(_) => "mtlx_not_utf8",
            Self::Xml(_) => "mtlx_parse",
            Self::NotMaterialX(_) => "mtlx_not_materialx",
            Self::Version(_) => "mtlx_version",
            Self::Structure { .. } => "mtlx_structure",
            Self::NoMaterial(_) => "mtlx_no_material",
            Self::UnsupportedShader { .. } => "mtlx_unsupported_shader",
            Self::Cycle(_) => "mtlx_cycle",
            Self::Limit(_) => "mtlx_limit",
            Self::Settings(_) => "mtlx_settings",
        }
    }
}

/// One import warning.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MaterialXWarning {
    /// Warning code (`mtlx_unsupported_node`, `mtlx_lossy_input`, ...).
    pub code: &'static str,
    /// The node path (`nodegraph/node`, or the shader's `name.input`).
    pub node: String,
    /// The node's category (empty for shader inputs).
    pub category: String,
    /// What happened.
    pub message: String,
}

/// One MaterialX node the importer mapped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MappedNode {
    /// The node path (`nodegraph/node`; an expanded custom node adds its instance path).
    pub node: String,
    /// Its category.
    pub category: String,
    /// What it became in the material graph.
    pub mapped_to: String,
}

/// A texture the imported graph samples.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ImportedTexture {
    /// Graph texture name.
    pub name: String,
    /// Project-relative image path.
    pub path: String,
    /// `srgb` or `linear`.
    pub color_space: &'static str,
}

/// A material parameter the import exposed.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ImportedParam {
    /// Graph parameter name.
    pub name: String,
    /// Where it came from (`nodegraph.input`, `instance.input` or `shader.input`).
    pub source: String,
    /// Default value, zero-padded to four components.
    pub value: [f32; 4],
    /// Component count.
    pub width: usize,
}

/// What an import mapped, approximated and could not map.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct MaterialXReport {
    /// The document's `version`.
    pub version: String,
    /// The imported `surfacematerial` (or the shader, when the document has no material).
    pub material: String,
    /// The surface shader's category.
    pub shader: String,
    /// Every mapped node.
    pub mapped: Vec<MappedNode>,
    /// Every warning, in the order found.
    pub warnings: Vec<MaterialXWarning>,
    /// Textures the graph samples.
    pub textures: Vec<ImportedTexture>,
    /// Exposed parameters, in slot order.
    pub params: Vec<ImportedParam>,
    /// Nodes mapped to the nearest matgraph node because the exact one does not exist: work
    /// for the material graph lanes.
    pub matgraph_gaps: Vec<String>,
    /// Graph nodes emitted.
    pub emitted_nodes: usize,
}

impl MaterialXReport {
    /// The `mtlx_unsupported_node` warnings.
    pub fn unsupported(&self) -> impl Iterator<Item = &MaterialXWarning> {
        self.warnings.iter().filter(|w| w.code == WARN_UNSUPPORTED_NODE)
    }

    /// The report as JSON (the agent's `import.materialx_report` result body).
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let unsupported: Vec<&MaterialXWarning> = self.unsupported().collect();
        serde_json::json!({
            "version": self.version,
            "material": self.material,
            "shader": self.shader,
            "mapped": self.mapped,
            "unsupported": unsupported,
            "warnings": self.warnings,
            "textures": self.textures,
            "params": self.params,
            "matgraph_gaps": self.matgraph_gaps,
            "emitted_nodes": self.emitted_nodes,
        })
    }
}

/// A finished import: the graph and the report.
#[derive(Clone, Debug)]
pub struct MaterialXImport {
    /// The material graph (current `.matgraph.ron` version).
    pub graph: MaterialGraph,
    /// What was mapped.
    pub report: MaterialXReport,
}

/// Imports a MaterialX document. `doc_dir` is the project-relative directory of the document
/// (`assets/materials`): image paths resolve against it.
///
/// # Errors
///
/// [`MaterialXError`] for a document that is not MaterialX, does not parse, has no supported
/// surface material, has a reference cycle, or exceeds a limit. Unsupported nodes and lossy
/// inputs are warnings in the report, not errors.
pub fn import_materialx_str(
    text: &str,
    doc_dir: &str,
    settings: &MaterialXSettings,
) -> Result<MaterialXImport, MaterialXError> {
    let _span = tracing::info_span!("materialx_import").entered();
    if text.len() > MAX_SOURCE_BYTES {
        return Err(MaterialXError::TooLarge {
            size: text.len(),
            limit: MAX_SOURCE_BYTES,
        });
    }
    let document = doc::parse(text)?;
    translate::import(&document, doc_dir, settings)
}

/// Imports a MaterialX document from bytes ([`import_materialx_str`] after the size and UTF-8
/// checks).
///
/// # Errors
///
/// As [`import_materialx_str`], plus [`MaterialXError::TooLarge`] and
/// [`MaterialXError::NotUtf8`].
pub fn import_materialx_bytes(
    bytes: &[u8],
    doc_dir: &str,
    settings: &MaterialXSettings,
) -> Result<MaterialXImport, MaterialXError> {
    if bytes.len() > MAX_SOURCE_BYTES {
        return Err(MaterialXError::TooLarge {
            size: bytes.len(),
            limit: MAX_SOURCE_BYTES,
        });
    }
    let text = std::str::from_utf8(bytes).map_err(|e| MaterialXError::NotUtf8(e.to_string()))?;
    import_materialx_str(text, doc_dir, settings)
}

/// The project-relative directory of a project-relative file path (`assets/a/b.mtlx` →
/// `assets/a`).
#[must_use]
pub fn document_dir(path: &str) -> String {
    let path = path.replace('\\', "/");
    match path.rfind('/') {
        Some(i) => path[..i].to_string(),
        None => String::new(),
    }
}

/// Imports the project file `path` (project-relative) with its `.meta` settings when the
/// sidecar exists. Read only: a missing `.meta` is not created.
///
/// # Errors
///
/// [`MaterialXError::Io`] when the file cannot be read or leaves the project, a malformed
/// `.meta` ([`MaterialXError::Settings`]), or any error of [`import_materialx_bytes`].
pub fn import_materialx_file(project_root: &Path, path: &str) -> Result<MaterialXImport, MaterialXError> {
    let full = crate::cook::join_in_project(project_root, path).map_err(MaterialXError::Io)?;
    let bytes = std::fs::read(&full).map_err(|e| MaterialXError::Io(format!("{path}: {e}")))?;
    let meta_path = regia_asset::meta_path_for(&full);
    let settings = match std::fs::read(&meta_path) {
        Ok(meta) => {
            let meta = regia_asset::MetaSidecar::from_ron(&meta)
                .map_err(|e| MaterialXError::Settings(format!("{}: {e}", meta_path.display())))?;
            MaterialXSettings::from_meta_string(meta.settings.as_deref())?
        }
        Err(_) => MaterialXSettings::default(),
    };
    import_materialx_bytes(&bytes, &document_dir(path), &settings)
}

/// The agent's `import.materialx_report` body for the project file `path`: the report plus
/// whether the graph compiles (`compiles`, and `compile_errors` when it does not). The error
/// is `(code, message)`; the codes are [`MaterialXError::code`]'s.
///
/// # Errors
///
/// `(code, message)` of the failed import.
pub fn agent_report(project_root: &Path, path: &str) -> Result<serde_json::Value, (&'static str, String)> {
    let import = import_materialx_file(project_root, path).map_err(|e| (e.code(), e.to_string()))?;
    let mut body = import.report.to_json();
    let compiled = regia_matgraph::compile(&import.graph);
    if let Some(map) = body.as_object_mut() {
        map.insert("path".into(), serde_json::Value::String(path.to_string()));
        map.insert("compiles".into(), serde_json::Value::Bool(compiled.is_ok()));
        if let Err(errors) = compiled {
            let list: Vec<String> = errors.0.iter().map(ToString::to_string).collect();
            map.insert("compile_errors".into(), serde_json::json!(list));
        }
    }
    Ok(body)
}

/// MaterialX type name → graph value type (`integer` and `boolean` read as `Float`).
pub(crate) fn value_type(mx: &str) -> Option<ValueType> {
    Some(match mx {
        "float" | "integer" | "boolean" => ValueType::Float,
        "vector2" => ValueType::Vec2,
        "color3" | "vector3" => ValueType::Vec3,
        "color4" | "vector4" => ValueType::Vec4,
        _ => return None,
    })
}

std::thread_local! {
    static COOK_WARNINGS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Warnings of the `.mtlx` cooks on this thread since the last call (the cook report collects
/// them after each source).
pub(crate) fn take_cook_warnings() -> Vec<String> {
    COOK_WARNINGS.with(|w| std::mem::take(&mut *w.borrow_mut()))
}

/// Cooks one `.mtlx` source: import, then the matgraph cook of the result.
pub(crate) fn cook_mtlx(
    source: &Path,
    rel_path: &str,
    bytes: &[u8],
    project_root: &Path,
    cache_dir: &Path,
) -> Result<crate::matgraph_cook::MatgraphCook, crate::cook::CookError> {
    use crate::cook::CookError;
    let meta =
        crate::db::AssetDatabase::load_or_create_meta(source, MATERIALX_IMPORTER).map_err(CookError::Db)?;
    let settings = MaterialXSettings::from_meta_string(meta.settings.as_deref())
        .map_err(|e| CookError::MaterialGraph(format!("{rel_path}: {}: {e}", e.code())))?;
    let import = import_materialx_bytes(bytes, &document_dir(rel_path), &settings)
        .map_err(|e| CookError::MaterialGraph(format!("{rel_path}: {}: {e}", e.code())))?;
    COOK_WARNINGS.with(|w| {
        let mut w = w.borrow_mut();
        for warning in &import.report.warnings {
            tracing::warn!(target: "regia_asset_pipeline", "{rel_path}: {} {}: {}", warning.code, warning.node, warning.message);
            w.push(format!("{rel_path}: {} {}: {}", warning.code, warning.node, warning.message));
        }
    });
    let mut deps = blake3::Hasher::new();
    deps.update(MATERIALX_IMPORTER.as_bytes());
    deps.update(&IMPORTER_REVISION.to_le_bytes());
    deps.update(&regia_matgraph::MATGRAPH_FORMAT_VERSION.to_le_bytes());
    deps.update(&regia_matgraph::COOKED_MATGRAPH_VERSION.to_le_bytes());
    deps.update(meta.settings.as_deref().unwrap_or("").as_bytes());
    crate::matgraph_cook::cook_graph(
        import.graph,
        None,
        MATERIALX_IMPORTER,
        meta.id,
        deps,
        project_root,
        cache_dir,
    )
}

/// Bumped when the mapping changes, so cached `.mtlx` cooks recook.
const IMPORTER_REVISION: u32 = 1;
