// SPDX-License-Identifier: MPL-2.0
//! USD import (ADR-3235; `book/agents/fbx-import.md` § USD).
//!
//! A `.usda` (text), `.usdc` (binary crate) or `.usdz` (zip package) file is composed with the
//! pure-Rust `openusd` reader and converted into an in-memory glTF 2.0 binary that the glTF
//! importer cooks, as OBJ and FBX are (ADR-1602): the cooked formats are the glTF ones.
//!
//! Before anything is composed, `vfs` walks the layer graph itself: every sublayer, reference
//! and payload file is read once, within the size caps, from inside the source's project (or its
//! folder outside a project); a reference cycle and a chain deeper than
//! [`UsdLimits::max_reference_depth`] are refused with codes. The reader then composes from that
//! in-memory set only, so it never opens a file the walk did not vet.
//!
//! The mapping (`convert`) follows the public USD schema documentation (openusd.org: UsdGeom
//! Xformable, Mesh, Primvars, GeomSubset, Camera; UsdShade Material and UsdPreviewSurface;
//! UsdLux): Xform hierarchy and transform ops, meshes (fan triangulation, normals, every
//! `texCoord2f` primvar, display color, `materialBind` subsets as primitives), preview-surface
//! materials with texture inputs, cameras, and lights where glTF has them. Stage `metersPerUnit`
//! and `upAxis` are converted to meters, +Y up. References, payloads, sublayers, variant sets at
//! their selection, and instances (each instance becomes its own nodes, sharing converted
//! meshes) are flattened by composition. Skinning (UsdSkel) is not imported: skinned meshes
//! come in rigid in their bind pose, with a warning.
//!
//! The reader is the `usd_import` feature (on by default, ADR-7972). Without it the settings,
//! limits, errors and report types stay, `convert_usd_to_glb` is not compiled, and
//! [`import_usd_path`] (and a cook of a USD file) answers [`ImportError::ImporterDisabled`].

#[cfg(feature = "usd_import")]
mod convert;
#[cfg(feature = "usd_import")]
mod vfs;

use std::path::Path;

use regia_asset::AssetId;
use serde::{Deserialize, Serialize};

use crate::gltf_import::{GltfImportResult, import_gltf_converted};
use crate::lod_classes::LodThresholdInputs;
use crate::texture_compress::TextureCookQuality;
use crate::{Forward, ImportError};

/// Whether `ext` (no dot, any case) names a USD layer or package.
#[must_use]
pub fn is_usd_extension(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "usd" | "usda" | "usdc" | "usdz"
    )
}

/// The importer name recorded in the asset database and `.meta`.
pub const USD_IMPORTER: &str = "usd";

/// Caps on one USD import (untrusted input).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UsdLimits {
    /// Largest single file (layer, package or texture), in bytes (default 512 MiB).
    pub max_file_bytes: u64,
    /// Most bytes read over all files of one import (default 2 GiB).
    pub max_total_bytes: u64,
    /// Most layers (root, sublayers, references, payloads) in one import (default 1,024).
    pub max_layers: usize,
    /// Longest chain of sublayer, reference and payload arcs between files (default 16).
    pub max_reference_depth: usize,
    /// Most composed prims visited (default 1 Mi).
    pub max_prims: usize,
    /// Deepest prim hierarchy (default 256).
    pub max_hierarchy_depth: usize,
    /// Most output vertices (default 16 Mi).
    pub max_vertices: usize,
    /// Most output triangle indices (default 48 Mi).
    pub max_indices: usize,
    /// Most corners in one polygon (default 1,024).
    pub max_polygon_corners: usize,
    /// Most bracket nesting levels in a text layer, checked before parsing (default 128).
    pub max_text_nesting: usize,
}

impl Default for UsdLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 512 << 20,
            max_total_bytes: 2 << 30,
            max_layers: 1024,
            max_reference_depth: 16,
            max_prims: 1 << 20,
            max_hierarchy_depth: 256,
            max_vertices: 16 << 20,
            max_indices: 48 << 20,
            max_polygon_corners: 1024,
            max_text_nesting: 128,
        }
    }
}

/// USD importer settings (the `.meta` `settings` string, next to the glTF keys).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UsdImportSettings {
    /// Uniform scale after the unit conversion to meters (default 1).
    pub scale: f32,
    /// Overrides the stage's `metersPerUnit` (USD's own fallback, when the stage authors none,
    /// is 0.01: centimeters).
    pub meters_per_unit: Option<f64>,
    /// Overrides the stage's `upAxis`: `"Y"` or `"Z"`.
    pub up_axis: Option<String>,
    /// Convert `Camera` prims (default true).
    pub import_cameras: bool,
    /// Convert `DistantLight`, `SphereLight`, `DiskLight` and `RectLight` prims (default true).
    pub import_lights: bool,
    /// Hard caps.
    pub limits: UsdLimits,
}

impl Default for UsdImportSettings {
    fn default() -> Self {
        Self {
            scale: 1.0,
            meters_per_unit: None,
            up_axis: None,
            import_cameras: true,
            import_lights: true,
            limits: UsdLimits::default(),
        }
    }
}

impl UsdImportSettings {
    /// Settings from the `.meta` `settings` string; blank is the defaults, glTF keys are
    /// ignored, a malformed string is an error.
    pub fn try_from_meta_string(settings: Option<&str>) -> Result<Self, UsdError> {
        let parsed = match settings.filter(|s| !s.trim().is_empty()) {
            Some(s) => ron::from_str::<Self>(s).map_err(|e| UsdError::Settings(e.to_string()))?,
            None => Self::default(),
        };
        parsed.validate()?;
        Ok(parsed)
    }

    /// Refuses a non-positive scale or unit, or an up axis other than `Y` and `Z`.
    pub fn validate(&self) -> Result<(), UsdError> {
        if !(self.scale.is_finite() && self.scale > 0.0) {
            return Err(UsdError::Settings("`scale` must be a positive number".into()));
        }
        if let Some(m) = self.meters_per_unit
            && !(m.is_finite() && m > 0.0)
        {
            return Err(UsdError::Settings(
                "`meters_per_unit` must be a positive number".into(),
            ));
        }
        if let Some(axis) = &self.up_axis
            && axis != "Y"
            && axis != "Z"
        {
            return Err(UsdError::Settings(format!(
                "`up_axis` must be \"Y\" or \"Z\", not {axis:?}"
            )));
        }
        Ok(())
    }
}

/// Why a USD import failed. [`UsdError::code`] is the agent-protocol error code.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum UsdError {
    /// A file could not be read.
    #[error("USD file read error: {0}")]
    Io(String),
    /// A file is larger than [`UsdLimits::max_file_bytes`], or all files together are larger
    /// than [`UsdLimits::max_total_bytes`].
    #[error("USD input is {size} bytes, more than the limit of {limit}")]
    TooLarge {
        /// Size in bytes.
        size: u64,
        /// The limit in bytes.
        limit: u64,
    },
    /// The root file is not a USD text layer, crate file or package.
    #[error("not a USD file: {0}")]
    NotUsd(String),
    /// A layer failed to parse, or the stage failed to compose.
    #[error("USD parse error: {0}")]
    Parse(String),
    /// A sublayer, reference or payload chain leads back to a layer already on it.
    #[error("USD reference cycle: {0}")]
    ReferenceCycle(String),
    /// A chain of sublayers, references and payloads is deeper than
    /// [`UsdLimits::max_reference_depth`].
    #[error("USD reference chain deeper than {limit}: {chain}")]
    ReferenceTooDeep {
        /// The limit.
        limit: usize,
        /// The chain, root first.
        chain: String,
    },
    /// A count limit was exceeded.
    #[error("USD import limit exceeded: {0}")]
    Limit(String),
    /// The `.meta` settings string is malformed.
    #[error("USD importer settings: {0}")]
    Settings(String),
    /// The composed stage has no mesh.
    #[error("USD stage has no mesh")]
    NoMesh,
    /// The reader stopped on an internal error (caught; the input is refused).
    #[error("USD reader failed on this input: {0}")]
    Reader(String),
}

impl UsdError {
    /// The agent-protocol error code.
    pub fn code(&self) -> &'static str {
        match self {
            UsdError::Io(_) => "usd_io",
            UsdError::TooLarge { .. } => "usd_too_large",
            UsdError::NotUsd(_) => "usd_not_usd",
            UsdError::Parse(_) => "usd_parse",
            UsdError::ReferenceCycle(_) => "usd_reference_cycle",
            UsdError::ReferenceTooDeep { .. } => "usd_reference_too_deep",
            UsdError::Limit(_) => "usd_limit",
            UsdError::Settings(_) => "usd_bad_settings",
            UsdError::NoMesh => "usd_no_mesh",
            UsdError::Reader(_) => "usd_reader_failed",
        }
    }
}

/// What a USD conversion found.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct UsdImportReport {
    /// The root file's encoding: `"usda"`, `"usdc"` or `"usdz"`.
    pub format: String,
    /// Every layer read, root first, as paths relative to the import root (`pkg.usdz[inner]`
    /// for a layer inside a package).
    pub layers: Vec<String>,
    /// The stage's meters per scene unit (after any override).
    pub meters_per_unit: f64,
    /// The stage's up axis (after any override): `"Y"` or `"Z"`.
    pub up_axis: String,
    /// The scale applied to positions: `meters_per_unit` times the settings' `scale`.
    pub scale: f64,
    /// Composed prims visited.
    pub prims: usize,
    /// glTF nodes written.
    pub nodes: usize,
    /// Mesh prims converted (an instanced mesh counts once).
    pub meshes: usize,
    /// Mesh nodes that reuse another node's converted mesh (instances).
    pub instanced_meshes: usize,
    /// Output vertices.
    pub vertices: usize,
    /// Output triangles.
    pub triangles: usize,
    /// Materials converted.
    pub materials: usize,
    /// Texture files embedded (paths relative to the import root).
    pub textures: Vec<String>,
    /// Cameras converted.
    pub cameras: usize,
    /// Lights converted.
    pub lights: usize,
    /// Variant selections in effect, `prim path: set = variant`.
    pub variant_selections: Vec<String>,
    /// Problems that did not stop the import (a missing reference or texture, an unsupported
    /// prim type, skinning dropped).
    pub warnings: Vec<String>,
}

/// A USD file converted to an in-memory glTF 2.0 binary.
#[derive(Clone, Debug)]
pub struct UsdConversion {
    /// The GLB bytes the glTF importer cooks.
    pub glb: Vec<u8>,
    /// What the conversion found.
    pub report: UsdImportReport,
    /// Digest of every file the conversion read besides the root (layers and textures, by
    /// path and content): the cook's input hash.
    pub inputs: [u8; 32],
}

/// The root of a conversion: a file on disk or in-memory bytes.
#[derive(Clone, Copy, Debug)]
pub enum UsdSource<'a> {
    /// A file; sublayers, references, payloads and textures resolve beside it, inside its
    /// project (or inside its folder when it is in no project).
    File(&'a Path),
    /// In-memory bytes with a name (its extension picks nothing: the content does). Nothing
    /// outside the bytes is read; every external reference is reported missing.
    Bytes {
        /// A display name such as `inline.usda`.
        name: &'a str,
        /// The file's bytes.
        bytes: &'a [u8],
    },
}

/// Converts a USD file (or in-memory bytes) to an in-memory glTF 2.0 binary.
///
/// # Errors
///
/// A [`UsdError`]: unreadable or oversized input, a parse or composition failure, a reference
/// cycle or over-deep chain, a count limit, or no mesh.
///
/// The `usd_import` feature (ADR-7972).
#[cfg(feature = "usd_import")]
pub fn convert_usd_to_glb(
    source: UsdSource<'_>,
    settings: &UsdImportSettings,
) -> Result<UsdConversion, UsdError> {
    let _span = tracing::info_span!("usd_convert").entered();
    settings.validate()?;
    let set = match source {
        UsdSource::File(path) => vfs::LayerSet::walk_file(path, &settings.limits)?,
        UsdSource::Bytes { name, bytes } => vfs::LayerSet::walk_bytes(name, bytes, &settings.limits)?,
    };
    // The reader is a large third-party parser fed untrusted bytes: a panic in it refuses the
    // input instead of taking the editor or the cook down (where panics unwind).
    let converted =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| convert::convert(&set, settings)));
    let (glb, mut report) = match converted {
        Ok(result) => result?,
        Err(panic) => {
            let message = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panic".into());
            return Err(UsdError::Reader(message));
        }
    };
    let mut warnings = set.warnings.clone();
    warnings.append(&mut report.warnings);
    report.warnings = warnings;
    report.layers = set.layers.clone();
    report.format = set.format.to_string();
    for warning in &report.warnings {
        tracing::warn!(target: "regia_asset_pipeline", "USD import: {warning}");
    }
    Ok(UsdConversion {
        glb,
        inputs: set.inputs_digest(),
        report,
    })
}

/// A USD file imported through the glTF importer.
#[derive(Clone, Debug)]
pub struct UsdImportResult {
    /// The glTF importer's result for the converted scene.
    pub gltf: GltfImportResult,
    /// The converted GLB.
    pub glb: Vec<u8>,
    /// What the conversion found.
    pub report: UsdImportReport,
}

/// Imports a `.usda`, `.usdc`, `.usd` or `.usdz` from disk through the glTF importer, with the
/// `.meta` settings string.
///
/// # Errors
///
/// [`ImportError::Usd`] for the conversion, or the glTF importer's error;
/// [`ImportError::ImporterDisabled`] without the `usd_import` feature (ADR-7972).
pub fn import_usd_path(
    source: &Path,
    parent_id: AssetId,
    forward: Forward,
    platform: &str,
    meta_settings: Option<&str>,
    texture_quality: TextureCookQuality,
) -> Result<UsdImportResult, ImportError> {
    if let Some(disabled) = crate::disabled_importer(USD_IMPORTER) {
        return Err(disabled);
    }
    let settings = UsdImportSettings::try_from_meta_string(meta_settings)?;
    let conversion = convert_for_import(UsdSource::File(source), &settings)?;
    let lod = LodThresholdInputs::resolve(&[], "", None).0;
    let gltf = import_gltf_converted(
        &conversion.glb,
        source.parent(),
        parent_id,
        forward,
        platform,
        meta_settings,
        texture_quality,
        &lod,
    )?;
    Ok(UsdImportResult {
        gltf,
        glb: conversion.glb,
        report: conversion.report,
    })
}

/// `convert_usd_to_glb` for [`import_usd_path`], or [`ImportError::ImporterDisabled`] in a build
/// without the reader (ADR-7972).
fn convert_for_import(
    source: UsdSource<'_>,
    settings: &UsdImportSettings,
) -> Result<UsdConversion, ImportError> {
    #[cfg(feature = "usd_import")]
    {
        convert_usd_to_glb(source, settings).map_err(ImportError::from)
    }
    #[cfg(not(feature = "usd_import"))]
    {
        let _ = (source, settings);
        Err(ImportError::ImporterDisabled {
            importer: USD_IMPORTER,
            feature: "usd_import",
        })
    }
}

/// The directory the import of `source` may read from: its project (the nearest folder with a
/// project manifest), or the file's own folder outside a project.
#[cfg(feature = "usd_import")]
pub(crate) fn import_root(source: &Path) -> std::path::PathBuf {
    use regia_core::brand::{LEGACY_MANIFEST_FILE, MANIFEST_FILE};
    use std::path::PathBuf;
    let dir = match source.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let dir = dir.canonicalize().unwrap_or(dir);
    dir.ancestors()
        .find(|a| a.join(MANIFEST_FILE).is_file() || a.join(LEGACY_MANIFEST_FILE).is_file())
        .map(Path::to_path_buf)
        .unwrap_or(dir)
}

/// The fuzz fixture's text layer (`fuzz_entry::usd_fixture`): every construct the converter reads, small.
pub(crate) const FUZZ_USDA: &str = r#"#usda 1.0
(
    defaultPrim = "World"
    metersPerUnit = 1
    upAxis = "Y"
)

class Xform "_Part"
{
    def Mesh "Quad" (
        prepend apiSchemas = ["MaterialBindingAPI"]
    )
    {
        int[] faceVertexCounts = [4]
        int[] faceVertexIndices = [0, 1, 2, 3]
        point3f[] points = [(0, 0, 0), (1, 0, 0), (1, 1, 0), (0, 1, 0)]
        normal3f[] normals = [(0, 0, 1)] (
            interpolation = "constant"
        )
        texCoord2f[] primvars:st = [(0, 0), (1, 0), (1, 1), (0, 1)] (
            interpolation = "vertex"
        )
        rel material:binding = </World/Looks/Paint>
    }
}

def Xform "World" (
    variants = {
        string look = "plain"
    }
    prepend variantSets = "look"
)
{
    double3 xformOp:translate = (1, 2, 3)
    float3 xformOp:rotateXYZ = (10, 20, 30)
    float3 xformOp:scale = (1, 2, 1)
    uniform token[] xformOpOrder = ["xformOp:translate", "xformOp:rotateXYZ", "xformOp:scale"]

    def Xform "A" (
        instanceable = true
        prepend references = </_Part>
    )
    {
    }

    def Xform "B" (
        instanceable = true
        prepend references = </_Part>
    )
    {
        quatf xformOp:orient = (1, 0, 0, 0)
        uniform token[] xformOpOrder = ["xformOp:orient"]
    }

    def Camera "Cam"
    {
        float focalLength = 35
        float2 clippingRange = (0.1, 500)
    }

    def Scope "Looks"
    {
        def Material "Paint"
        {
            token outputs:surface.connect = </World/Looks/Paint/S.outputs:surface>

            def Shader "S"
            {
                uniform token info:id = "UsdPreviewSurface"
                color3f inputs:diffuseColor.connect = </World/Looks/Paint/T.outputs:rgb>
                float inputs:roughness = 0.5
                token outputs:surface
            }

            def Shader "T"
            {
                uniform token info:id = "UsdUVTexture"
                asset inputs:file = @tex/albedo.png@
                float3 outputs:rgb
            }
        }
    }

    variantSet "look" = {
        "plain" {
            def SphereLight "Bulb"
            {
                float inputs:intensity = 5
            }
        }
        "fancy" {
            def DistantLight "Sun"
            {
            }
        }
    }
}
"#;

/// The fuzz fixture's package (`fuzz_entry::usdz_fixture`): [`FUZZ_USDA`] as a binary crate layer plus the PNG its
/// material reads. Empty if the fixture cannot be built (the fuzz test then fails on its fixture).
#[cfg(feature = "usd_import")]
pub(crate) fn fuzz_usdz_fixture() -> Vec<u8> {
    use openusd::{sdf, usdc, usdz};
    let build = || -> Option<Vec<u8>> {
        let layer = sdf::Layer::from_bytes("fuzz.usda", FUZZ_USDA.as_bytes().to_vec()).ok()?;
        let mut crate_bytes = std::io::Cursor::new(Vec::new());
        usdc::CrateWriter::write(layer.data(), &mut crate_bytes).ok()?;
        let texture = image::RgbaImage::from_pixel(2, 2, image::Rgba([200, 120, 40, 255]));
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(texture)
            .write_to(&mut png, image::ImageFormat::Png)
            .ok()?;
        let mut writer = usdz::ArchiveWriter::new(std::io::Cursor::new(Vec::new()));
        writer.add_layer("scene.usdc", crate_bytes.get_ref()).ok()?;
        writer.add_layer("tex/albedo.png", png.get_ref()).ok()?;
        Some(writer.finish().ok()?.into_inner())
    };
    build().unwrap_or_default()
}

#[cfg(all(test, feature = "usd_import"))]
mod tests;
