// SPDX-License-Identifier: MPL-2.0
//! FBX import (ADR-1602, `book/agents/fbx-import.md`).
//!
//! An `.fbx` file (binary or ASCII, FBX 6.1 to 7.7) is parsed by the `ufbx` C library through
//! the desktop pipeline only, converted into an in-memory glTF 2.0 binary, and cooked by the
//! glTF importer: the cooked meshes, materials, textures, skeletons and clips are the same
//! formats a `.glb` produces, so nothing downstream knows the source was FBX.
//!
//! The parser sees untrusted bytes, so before it runs this module caps the file size and walks
//! the file's record structure in Rust ([`prescan_fbx`]: bounds and nesting depth), and the
//! parse itself runs under memory, allocation and node-depth limits ([`FbxLimits`]). After the
//! parse the vertex, index, node, texture and key counts are capped again. No `ufbx` type leaves
//! the crate-private reader.
//!
//! Conventions: the file's axes and unit are converted to right-handed, +Y up, one unit per
//! meter (the glTF frame); the glTF importer then applies Regia Engine's forward convention
//! (`docs/formats/gltf-conventions.md`). FBX V coordinates are flipped to glTF's top-left origin.
//!
//! The parser is the `fbx_import` feature (on by default, ADR-7972). Without it the settings, the
//! limits, the prescan and the report types stay, `convert_fbx_to_glb` is not compiled, and
//! [`import_fbx_path`] / [`import_fbx_bytes`] (and a cook of an `.fbx`) answer
//! [`ImportError::ImporterDisabled`].

use std::path::Path;

use regia_asset::AssetId;
use serde::{Deserialize, Serialize};

use crate::gltf_import::{GltfImportResult, import_gltf_converted};
use crate::lod_classes::LodThresholdInputs;
use crate::texture_compress::TextureCookQuality;
use crate::{Forward, ImportError};

/// The binary FBX magic: `"Kaydara FBX Binary  "`, NUL, 0x1A, NUL.
const BINARY_MAGIC: &[u8; 23] = b"Kaydara FBX Binary  \x00\x1a\x00";

/// A one-triangle ASCII FBX 7.4 file (meters, +Y up): the smallest input the importer
/// converts. For smoke tests of the CLI and the agent method (`book/agents/fbx-import.md`).
pub const TRIANGLE_FBX: &str = "; FBX 7.4.0 project file
FBXHeaderExtension:  {
\tFBXHeaderVersion: 1003
\tFBXVersion: 7400
}
GlobalSettings:  {
\tVersion: 1000
\tProperties70:  {
\t\tP: \"UpAxis\", \"int\", \"Integer\", \"\",1
\t\tP: \"UpAxisSign\", \"int\", \"Integer\", \"\",1
\t\tP: \"FrontAxis\", \"int\", \"Integer\", \"\",2
\t\tP: \"FrontAxisSign\", \"int\", \"Integer\", \"\",1
\t\tP: \"CoordAxis\", \"int\", \"Integer\", \"\",0
\t\tP: \"CoordAxisSign\", \"int\", \"Integer\", \"\",1
\t\tP: \"UnitScaleFactor\", \"double\", \"Number\", \"\",100
\t}
}
Objects:  {
\tGeometry: 100, \"Geometry::Triangle\", \"Mesh\" {
\t\tVertices: *9 {
\t\t\ta: 0,0,0,1,0,0,0,1,0
\t\t}
\t\tPolygonVertexIndex: *3 {
\t\t\ta: 0,1,-3
\t\t}
\t}
\tModel: 200, \"Model::Triangle\", \"Mesh\" {
\t\tVersion: 232
\t}
}
Connections:  {
\tC: \"OO\",200,0
\tC: \"OO\",100,200
}
";

/// Hard caps on what one FBX import may read, parse and produce.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FbxLimits {
    /// Largest source file, in bytes (default 512 MiB).
    pub max_file_bytes: u64,
    /// Deepest record nesting in the file (default 64; real files nest under 10).
    pub max_record_depth: usize,
    /// Most records (nodes of the file's tree, not scene nodes) in a binary file (default 16 Mi).
    pub max_records: usize,
    /// Deepest scene-node hierarchy (default 256).
    pub max_node_depth: u32,
    /// Most scene nodes (default 65,536).
    pub max_nodes: usize,
    /// Most output vertices over all meshes (default 16 Mi).
    pub max_vertices: usize,
    /// Most output triangle indices over all meshes (default 48 Mi).
    pub max_indices: usize,
    /// Most distinct textures (default 1,024).
    pub max_textures: usize,
    /// Most animation keys over all clips (default 32 Mi).
    pub max_animation_keys: usize,
    /// Memory the parser may allocate, per allocator, in bytes (default 4 GiB).
    pub max_memory_bytes: usize,
    /// Allocations the parser may make, per allocator (default 64 Mi).
    pub max_allocations: usize,
}

impl Default for FbxLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 512 << 20,
            max_record_depth: 64,
            max_records: 16 << 20,
            max_node_depth: 256,
            max_nodes: 65_536,
            max_vertices: 16 << 20,
            max_indices: 48 << 20,
            max_textures: 1024,
            max_animation_keys: 32 << 20,
            max_memory_bytes: 4 << 30,
            max_allocations: 64 << 20,
        }
    }
}

/// FBX importer settings: the `.meta` `settings` string (RON), next to the glTF importer's keys
/// (`meshlets`, `generate_lods` …), which apply to the converted scene as they do to a `.glb`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FbxImportSettings {
    /// Uniform scale applied after the unit conversion to meters (default 1). Use it for a file
    /// whose declared unit is wrong (`scale: 0.01` for centimeter data saved as meters).
    pub scale: f32,
    /// Import animation stacks as clips (default true).
    pub import_animation: bool,
    /// Samples per second for non-linear animation curves (1..=240, default 30). Linear keys are
    /// kept as they are.
    pub sample_rate: f32,
    /// The FBX UV set that becomes `TEXCOORD_0`, the set the cooked mesh keeps (default: the
    /// first). The others are carried in the converted glTF.
    pub uv_set: Option<String>,
    /// Hard caps (untrusted input).
    pub limits: FbxLimits,
    /// Keep every animation clip of the file lossless at cook (`.meta` v2, ADR-7819), as the glTF
    /// importer's key of the same name: the cook reads it from the converted scene's glTF settings
    /// and keys the cache on it, so it is not part of `Self::digest`. Off by default.
    pub lossless_clips: bool,
}

impl Default for FbxImportSettings {
    fn default() -> Self {
        Self {
            scale: 1.0,
            import_animation: true,
            sample_rate: 30.0,
            uv_set: None,
            limits: FbxLimits::default(),
            lossless_clips: false,
        }
    }
}

impl FbxImportSettings {
    /// Settings from the `.meta` `settings` string. Blank or missing settings are the defaults;
    /// glTF importer keys are ignored here; a malformed string is an error.
    pub fn try_from_meta_string(settings: Option<&str>) -> Result<Self, FbxError> {
        match settings.filter(|s| !s.trim().is_empty()) {
            Some(s) => ron::from_str::<Self>(s).map_err(|e| FbxError::Settings(e.to_string())),
            None => Ok(Self::default()),
        }
    }

    /// The settings' contribution to the cook's cache key.
    #[cfg(feature = "fbx_import")]
    fn digest(&self) -> String {
        format!(
            "scale={};anim={};rate={};uv={:?};limits={:?}",
            self.scale, self.import_animation, self.sample_rate, self.uv_set, self.limits
        )
    }
}

/// Why an FBX import failed. [`FbxError::code`] is the agent-protocol error code.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum FbxError {
    /// The file could not be read.
    #[error("FBX file read error: {0}")]
    Io(String),
    /// The file is larger than [`FbxLimits::max_file_bytes`].
    #[error("FBX file is {size} bytes, more than the limit of {limit}")]
    TooLarge {
        /// File size in bytes.
        size: u64,
        /// The limit in bytes.
        limit: u64,
    },
    /// The file is neither a binary nor an ASCII FBX.
    #[error("not an FBX file: {0}")]
    NotFbx(String),
    /// The file's records nest deeper than a limit.
    #[error("FBX file nests deeper than the limit of {0}")]
    TooDeep(usize),
    /// The file's record structure is broken (an offset outside the file, a truncated header).
    #[error("malformed FBX file: {0}")]
    Malformed(String),
    /// The parser rejected the file.
    #[error("FBX parse error: {0}")]
    Parse(String),
    /// The FBX version is not supported by the parser.
    #[error("unsupported FBX version: {0}")]
    UnsupportedVersion(String),
    /// A count or memory limit was exceeded.
    #[error("FBX import limit exceeded: {0}")]
    Limit(String),
    /// The `.meta` settings string is malformed.
    #[error("FBX importer settings: {0}")]
    Settings(String),
    /// The scene has no mesh to cook.
    #[error("FBX scene has no mesh")]
    NoMesh,
}

impl FbxError {
    /// The agent-protocol error code (`book/agents/fbx-import.md`).
    pub fn code(&self) -> &'static str {
        match self {
            FbxError::Io(_) => "fbx_io",
            FbxError::TooLarge { .. } => "fbx_too_large",
            FbxError::NotFbx(_) => "fbx_not_fbx",
            FbxError::TooDeep(_) => "fbx_too_deep",
            FbxError::Malformed(_) => "fbx_malformed",
            FbxError::Parse(_) => "fbx_parse",
            FbxError::UnsupportedVersion(_) => "fbx_unsupported_version",
            FbxError::Limit(_) => "fbx_limit",
            FbxError::Settings(_) => "fbx_bad_settings",
            FbxError::NoMesh => "fbx_no_mesh",
        }
    }

    /// Maps a `ufbx` error to the importer's error kinds.
    #[cfg(feature = "fbx_import")]
    pub(crate) fn from_ufbx(kind: ufbx::ErrorType, description: &str) -> Self {
        use ufbx::ErrorType as E;
        let message = format!("{description} ({kind:?})");
        match kind {
            E::MemoryLimit | E::AllocationLimit | E::OutOfMemory => FbxError::Limit(message),
            E::NodeDepthLimit => FbxError::Limit(message),
            E::UnsupportedVersion => FbxError::UnsupportedVersion(message),
            E::UnrecognizedFileFormat | E::EmptyFile => FbxError::NotFbx(message),
            _ => FbxError::Parse(message),
        }
    }
}

/// Which FBX encoding a file uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FbxEncoding {
    /// The binary encoding, with its version number.
    Binary {
        /// The header's version (7400 = FBX 7.4).
        version: u32,
    },
    /// The ASCII encoding.
    Ascii,
}

/// Where a material's texture came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FbxTextureStatus {
    /// Embedded in the FBX (its `Video` content).
    Embedded,
    /// A file inside the FBX's directory.
    External,
    /// Not found inside the FBX's directory: the slot is dropped.
    Missing,
    /// Found but not decodable: the slot is dropped.
    Undecodable,
}

/// One texture a material references.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FbxTextureInfo {
    /// The material's name.
    pub material: String,
    /// The slot: `base_color`, `normal` or `emissive`.
    pub role: String,
    /// The file as found (relative to the FBX), or as the FBX names it when missing.
    pub file: String,
    /// Where it came from.
    pub status: FbxTextureStatus,
}

/// One converted animation clip.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FbxClipInfo {
    /// The animation stack's name.
    pub name: String,
    /// Length in seconds (from 0).
    pub duration_seconds: f32,
    /// Sampled channels (translation, rotation, scale per node; weights per morphing node).
    pub channels: usize,
}

/// What an FBX conversion found and did.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct FbxImportReport {
    /// ASCII (not binary) file.
    pub ascii: bool,
    /// FBX version (7400 = 7.4).
    pub version: u32,
    /// The exporting application, as the file names it.
    pub creator: String,
    /// The file's unit, in meters (0.01 = centimeters).
    pub source_unit_meters: f64,
    /// The file's up axis (`+Y`, `+Z` …).
    pub source_up_axis: String,
    /// The import scale setting.
    pub scale: f32,
    /// Scene nodes written.
    pub nodes: usize,
    /// Meshes written.
    pub meshes: usize,
    /// Output vertices over all meshes.
    pub vertices: usize,
    /// Output triangles over all meshes.
    pub triangles: usize,
    /// Skins (skinned mesh instances).
    pub skins: usize,
    /// Joints over all skins.
    pub joints: usize,
    /// Morph targets over all meshes.
    pub morph_targets: usize,
    /// Materials.
    pub materials: usize,
    /// UV set names, `TEXCOORD_0` first.
    pub uv_sets: Vec<String>,
    /// Material textures and where each came from.
    pub textures: Vec<FbxTextureInfo>,
    /// Animation clips.
    pub clips: Vec<FbxClipInfo>,
    /// Problems that did not stop the import.
    pub warnings: Vec<String>,
}

/// An FBX file converted to an in-memory glTF 2.0 binary.
#[derive(Clone, Debug)]
pub struct FbxConversion {
    /// The GLB bytes the glTF importer cooks.
    pub glb: Vec<u8>,
    /// What the conversion found.
    pub report: FbxImportReport,
}

/// An FBX file imported through the glTF importer.
#[derive(Clone, Debug)]
pub struct FbxImportResult {
    /// The glTF importer's result for the converted scene (cooked meshes, materials, textures).
    pub gltf: GltfImportResult,
    /// The converted GLB (also the input of the animation cook).
    pub glb: Vec<u8>,
    /// What the conversion found.
    pub report: FbxImportReport,
}

/// Reads an FBX file, refusing one larger than `limits.max_file_bytes` before reading it.
pub fn read_fbx_file(path: &Path, limits: &FbxLimits) -> Result<Vec<u8>, FbxError> {
    use std::io::Read;
    let file = crate::mesh_convert::open_regular(path)
        .map_err(|e| FbxError::Io(format!("{}: {e}", path.display())))?;
    let size = file
        .metadata()
        .map_err(|e| FbxError::Io(format!("{}: {e}", path.display())))?
        .len();
    if size > limits.max_file_bytes {
        return Err(FbxError::TooLarge {
            size,
            limit: limits.max_file_bytes,
        });
    }
    let mut bytes = Vec::with_capacity(size as usize);
    // A file that grows while it is read stops at the limit (+1 to notice).
    file.take(limits.max_file_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| FbxError::Io(format!("{}: {e}", path.display())))?;
    if bytes.len() as u64 > limits.max_file_bytes {
        return Err(FbxError::TooLarge {
            size: bytes.len() as u64,
            limit: limits.max_file_bytes,
        });
    }
    Ok(bytes)
}

/// Checks the file's size and record structure in Rust before the C parser sees it: a binary
/// file's record offsets must stay inside their parent and the file, and records may nest at
/// most `limits.max_record_depth` deep; an ASCII file's `{ }` blocks (outside strings and `;`
/// comments) obey the same depth. Returns the encoding.
pub fn prescan_fbx(bytes: &[u8], limits: &FbxLimits) -> Result<FbxEncoding, FbxError> {
    if bytes.len() as u64 > limits.max_file_bytes {
        return Err(FbxError::TooLarge {
            size: bytes.len() as u64,
            limit: limits.max_file_bytes,
        });
    }
    if bytes.is_empty() {
        return Err(FbxError::NotFbx("empty file".into()));
    }
    if bytes.starts_with(BINARY_MAGIC) {
        return prescan_binary(bytes, limits);
    }
    prescan_ascii(bytes, limits)?;
    Ok(FbxEncoding::Ascii)
}

/// Little-endian `u32` at `at`, or `None` past the end.
fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

/// Little-endian `u64` at `at`, or `None` past the end.
fn read_u64(bytes: &[u8], at: usize) -> Option<u64> {
    let slice = bytes.get(at..at.checked_add(8)?)?;
    let mut array = [0u8; 8];
    array.copy_from_slice(slice);
    Some(u64::from_le_bytes(array))
}

/// The binary record walk (record layout from the public FBX binary format descriptions: an
/// end offset, a property count, a property-list length, a name; 64-bit fields from 7.5).
fn prescan_binary(bytes: &[u8], limits: &FbxLimits) -> Result<FbxEncoding, FbxError> {
    let version = read_u32(bytes, 23).ok_or_else(|| FbxError::Malformed("truncated binary header".into()))?;
    let wide = version >= 7500;
    let header_len: usize = if wide { 25 } else { 13 };
    let mut pos: usize = 27;
    // End offsets of the open records (their children are being walked).
    let mut open: Vec<usize> = Vec::new();
    let mut records = 0usize;
    loop {
        let limit = open.last().copied().unwrap_or(bytes.len());
        let header_end = pos.checked_add(header_len);
        if header_end.is_none_or(|end| end > limit) {
            match open.pop() {
                // The parent's children end without a terminator: continue after the parent.
                Some(end) => {
                    pos = end;
                    continue;
                }
                // The top-level list ran into the footer.
                None => break,
            }
        }
        let (end, prop_len, name_len) = if wide {
            (
                read_u64(bytes, pos),
                read_u64(bytes, pos + 16),
                bytes.get(pos + 24).copied(),
            )
        } else {
            (
                read_u32(bytes, pos).map(u64::from),
                read_u32(bytes, pos + 8).map(u64::from),
                bytes.get(pos + 12).copied(),
            )
        };
        let (Some(end), Some(prop_len), Some(name_len)) = (end, prop_len, name_len) else {
            return Err(FbxError::Malformed(format!(
                "truncated record header at byte {pos}"
            )));
        };
        if end == 0 {
            // A null record: the end of a child list (or of the top level).
            match open.pop() {
                Some(parent_end) => {
                    pos = parent_end;
                    continue;
                }
                None => break,
            }
        }
        let end = usize::try_from(end)
            .map_err(|_| FbxError::Malformed(format!("record end {end} out of range")))?;
        if end <= pos || end > limit {
            return Err(FbxError::Malformed(format!(
                "record at byte {pos} ends at {end}, outside its parent (which ends at {limit})"
            )));
        }
        let content = usize::try_from(prop_len)
            .ok()
            .and_then(|p| {
                pos.checked_add(header_len)?
                    .checked_add(usize::from(name_len))?
                    .checked_add(p)
            })
            .filter(|content| *content <= end)
            .ok_or_else(|| {
                FbxError::Malformed(format!("record at byte {pos}: properties overrun the record"))
            })?;
        records += 1;
        if records > limits.max_records {
            return Err(FbxError::Limit(format!(
                "more than {} file records",
                limits.max_records
            )));
        }
        // This record's level: top-level records are level 1.
        if open.len() >= limits.max_record_depth {
            return Err(FbxError::TooDeep(limits.max_record_depth));
        }
        if content < end {
            open.push(end);
            pos = content;
        } else {
            pos = end;
        }
    }
    Ok(FbxEncoding::Binary { version })
}

/// The ASCII block-depth walk. ASCII FBX is `Name: values { … }` blocks; `;` starts a comment
/// to the end of the line and `"` delimits strings.
fn prescan_ascii(bytes: &[u8], limits: &FbxLimits) -> Result<(), FbxError> {
    let head = &bytes[..bytes.len().min(4096)];
    if head.contains(&0) {
        return Err(FbxError::NotFbx(
            "binary data without the binary FBX header".into(),
        ));
    }
    let mut depth = 0usize;
    let mut in_string = false;
    let mut in_comment = false;
    let mut saw_block = false;
    for &b in bytes {
        if in_comment {
            if b == b'\n' {
                in_comment = false;
            }
            continue;
        }
        if in_string {
            if b == b'"' || b == b'\n' {
                in_string = false;
            }
            continue;
        }
        match b {
            b';' => in_comment = true,
            b'"' => in_string = true,
            b'{' => {
                depth += 1;
                saw_block = true;
                if depth > limits.max_record_depth {
                    return Err(FbxError::TooDeep(limits.max_record_depth));
                }
            }
            b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    if !saw_block {
        return Err(FbxError::NotFbx("no FBX blocks found".into()));
    }
    Ok(())
}

/// Converts FBX bytes to an in-memory glTF 2.0 binary. `base_dir` is the FBX's directory:
/// external textures are looked up inside it only (none when `None`); embedded ones are kept.
/// The `fbx_import` feature (ADR-7972).
#[cfg(feature = "fbx_import")]
pub fn convert_fbx_to_glb(
    bytes: &[u8],
    base_dir: Option<&Path>,
    settings: &FbxImportSettings,
) -> Result<FbxConversion, FbxError> {
    let _span = tracing::info_span!("fbx_convert", bytes = bytes.len()).entered();
    prescan_fbx(bytes, &settings.limits)?;
    let (scene, report) = crate::fbx_scene::read_scene(bytes, base_dir, settings)?;
    if scene.meshes.iter().all(|m| m.primitives.is_empty()) {
        return Err(FbxError::NoMesh);
    }
    // The FBX builder replaces non-finite components, so this cannot fire for well-formed
    // input; a failure here means the sanitizer missed a path, and the message says so.
    let glb = scene.to_glb().map_err(|e| FbxError::Parse(e.to_string()))?;
    for warning in &report.warnings {
        tracing::warn!(target: "regia_asset_pipeline", "FBX import: {warning}");
    }
    Ok(FbxConversion { glb, report })
}

/// Imports an `.fbx` from disk: [`read_fbx_file`], `convert_fbx_to_glb` with the file's
/// directory, then the glTF importer on the result with the same `.meta` settings string.
/// [`ImportError::ImporterDisabled`] without the `fbx_import` feature (ADR-7972).
pub fn import_fbx_path(
    source: &Path,
    parent_id: AssetId,
    forward: Forward,
    platform: &str,
    meta_settings: Option<&str>,
    texture_quality: TextureCookQuality,
) -> Result<FbxImportResult, ImportError> {
    if let Some(disabled) = crate::disabled_importer("fbx") {
        return Err(disabled);
    }
    let settings = FbxImportSettings::try_from_meta_string(meta_settings)?;
    let bytes = read_fbx_file(source, &settings.limits)?;
    let lod = LodThresholdInputs::resolve(&[], "", None).0;
    import_fbx_converted(
        &bytes,
        source.parent(),
        parent_id,
        forward,
        platform,
        meta_settings,
        texture_quality,
        &settings,
        &lod,
    )
}

/// Imports FBX bytes with no source directory (embedded textures only).
/// [`ImportError::ImporterDisabled`] without the `fbx_import` feature (ADR-7972).
pub fn import_fbx_bytes(
    bytes: &[u8],
    parent_id: AssetId,
    forward: Forward,
    platform: &str,
    texture_quality: TextureCookQuality,
    settings: &FbxImportSettings,
) -> Result<FbxImportResult, ImportError> {
    if let Some(disabled) = crate::disabled_importer("fbx") {
        return Err(disabled);
    }
    let lod = LodThresholdInputs::resolve(&[], "", None).0;
    import_fbx_converted(
        bytes,
        None,
        parent_id,
        forward,
        platform,
        None,
        texture_quality,
        settings,
        &lod,
    )
}

#[allow(clippy::too_many_arguments)] // The glTF importer's inputs plus the FBX settings.
pub(crate) fn import_fbx_converted(
    bytes: &[u8],
    base_dir: Option<&Path>,
    parent_id: AssetId,
    forward: Forward,
    platform: &str,
    meta_settings: Option<&str>,
    texture_quality: TextureCookQuality,
    settings: &FbxImportSettings,
    lod: &LodThresholdInputs,
) -> Result<FbxImportResult, ImportError> {
    let conversion = convert_for_import(bytes, base_dir, settings)?;
    let gltf = import_gltf_converted(
        &conversion.glb,
        base_dir,
        parent_id,
        forward,
        platform,
        meta_settings,
        texture_quality,
        lod,
    )?;
    Ok(FbxImportResult {
        gltf,
        glb: conversion.glb,
        report: conversion.report,
    })
}

/// `convert_fbx_to_glb` for the importers above, or [`ImportError::ImporterDisabled`] in a build
/// without the parser (ADR-7972).
fn convert_for_import(
    bytes: &[u8],
    base_dir: Option<&Path>,
    settings: &FbxImportSettings,
) -> Result<FbxConversion, ImportError> {
    #[cfg(feature = "fbx_import")]
    {
        convert_fbx_to_glb(bytes, base_dir, settings).map_err(ImportError::from)
    }
    #[cfg(not(feature = "fbx_import"))]
    {
        let _ = (bytes, base_dir, settings);
        Err(ImportError::ImporterDisabled {
            importer: "fbx",
            feature: "fbx_import",
        })
    }
}

/// The cook's input digest for an FBX besides its own bytes: the settings, the `.meta` string
/// (glTF keys), and every external texture the conversion read (by path, content and `.meta`).
#[cfg(feature = "fbx_import")]
pub(crate) fn fbx_inputs_hash(
    settings: &FbxImportSettings,
    meta_settings: Option<&str>,
    base_dir: Option<&Path>,
    report: &FbxImportReport,
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"fbx;");
    hasher.update(settings.digest().as_bytes());
    hasher.update(b";meta=");
    hasher.update(meta_settings.unwrap_or_default().as_bytes());
    for texture in &report.textures {
        hasher.update(b";texture=");
        hasher.update(texture.file.as_bytes());
        if texture.status == FbxTextureStatus::External
            && let Some(dir) = base_dir
        {
            // A path the file names outside the project (the author's machine, or a hostile file)
            // is hashed as missing, so the input hash answers nothing about what exists there.
            let path = crate::cook::join_portable(dir, &texture.file).ok();
            match path.as_ref().map(std::fs::read) {
                Some(Ok(bytes)) => hasher.update(blake3::hash(&bytes).as_bytes()),
                _ => hasher.update(b"<missing>"),
            };
            if let Some(Ok(meta)) = path.map(|p| std::fs::read(regia_asset::meta_path_for(&p))) {
                hasher.update(b";meta=");
                hasher.update(blake3::hash(&meta).as_bytes());
            }
        }
    }
    *hasher.finalize().as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binary_header(version: u32) -> Vec<u8> {
        let mut out = BINARY_MAGIC.to_vec();
        out.extend_from_slice(&version.to_le_bytes());
        out
    }

    /// A 7.4 record: `name`, no properties, `children` as nested bytes (null-terminated).
    fn record(at: usize, name: &str, children: &[u8]) -> Vec<u8> {
        let mut nested = children.to_vec();
        if !children.is_empty() {
            nested.extend_from_slice(&[0u8; 13]);
        }
        let end = at + 13 + name.len() + nested.len();
        let mut out = Vec::new();
        out.extend_from_slice(&(end as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.push(name.len() as u8);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&nested);
        out
    }

    /// A chain of `depth` nested records starting at byte 27.
    fn nested_chain(depth: usize) -> Vec<u8> {
        // Build from the innermost out: each level's offset depends on its position.
        let starts: Vec<usize> = (0..depth).map(|level| 27 + level * 14).collect();
        let mut inner: Vec<u8> = Vec::new();
        for level in (0..depth).rev() {
            inner = record(starts[level], "N", &inner);
        }
        let mut file = binary_header(7400);
        file.extend_from_slice(&inner);
        file.extend_from_slice(&[0u8; 13]);
        file
    }

    #[test]
    fn the_binary_prescan_accepts_nesting_within_the_limit() {
        let limits = FbxLimits::default();
        assert_eq!(
            prescan_fbx(&nested_chain(5), &limits),
            Ok(FbxEncoding::Binary { version: 7400 })
        );
    }

    #[test]
    fn the_binary_prescan_rejects_deep_nesting_before_the_parser() {
        let limits = FbxLimits {
            max_record_depth: 8,
            ..FbxLimits::default()
        };
        assert_eq!(prescan_fbx(&nested_chain(9), &limits), Err(FbxError::TooDeep(8)));
        assert!(prescan_fbx(&nested_chain(8), &limits).is_ok());
    }

    #[test]
    fn the_binary_prescan_rejects_offsets_outside_the_file() {
        let mut file = nested_chain(3);
        // The first record's end offset points past the end of the file.
        file[27..31].copy_from_slice(&u32::MAX.to_le_bytes());
        let err = prescan_fbx(&file, &FbxLimits::default()).expect_err("must fail");
        assert_eq!(err.code(), "fbx_malformed");
        // A backwards end offset.
        let mut file = nested_chain(3);
        file[27..31].copy_from_slice(&5u32.to_le_bytes());
        assert_eq!(
            prescan_fbx(&file, &FbxLimits::default()).map_err(|e| e.code()),
            Err("fbx_malformed")
        );
    }

    #[test]
    fn the_prescan_survives_every_truncation_and_byte_flip() {
        let limits = FbxLimits::default();
        let file = nested_chain(6);
        for len in 0..file.len() {
            let _ = prescan_fbx(&file[..len], &limits);
        }
        for i in 0..file.len() {
            for flip in [0x01u8, 0x80, 0xFF] {
                let mut bad = file.clone();
                bad[i] ^= flip;
                let _ = prescan_fbx(&bad, &limits);
            }
        }
    }

    #[test]
    fn the_ascii_prescan_counts_depth_outside_strings_and_comments() {
        let limits = FbxLimits {
            max_record_depth: 3,
            ..FbxLimits::default()
        };
        let ok = b"; {{{{{{ comment\nA: { B: \"{{{{{{\" { C: { } } }\n";
        assert_eq!(prescan_fbx(ok, &limits), Ok(FbxEncoding::Ascii));
        let deep = b"A: { B: { C: { D: { } } } }";
        assert_eq!(prescan_fbx(deep, &limits), Err(FbxError::TooDeep(3)));
        assert_eq!(
            prescan_fbx(b"", &limits).map_err(|e| e.code()),
            Err("fbx_not_fbx")
        );
        assert_eq!(
            prescan_fbx(b"just text", &limits).map_err(|e| e.code()),
            Err("fbx_not_fbx")
        );
        assert_eq!(
            prescan_fbx(b"\0\x01\x02{", &limits).map_err(|e| e.code()),
            Err("fbx_not_fbx")
        );
    }

    #[test]
    fn the_size_cap_applies_to_bytes_and_files() {
        let limits = FbxLimits {
            max_file_bytes: 16,
            ..FbxLimits::default()
        };
        let err = prescan_fbx(&[b'{'; 17], &limits).expect_err("too large");
        assert_eq!(err.code(), "fbx_too_large");
        let dir = regia_test_utils::unique_temp_dir("fbx-size-cap");
        let path = dir.join("big.fbx");
        std::fs::write(&path, [b' '; 32]).expect("write");
        assert_eq!(
            read_fbx_file(&path, &limits).map_err(|e| e.code()),
            Err("fbx_too_large")
        );
        assert_eq!(
            read_fbx_file(&dir.join("absent.fbx"), &limits).map_err(|e| e.code()),
            Err("fbx_io")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_parse_from_meta_and_ignore_gltf_keys() {
        let s = FbxImportSettings::try_from_meta_string(Some(
            "(scale: 0.01, sample_rate: 60, uv_set: Some(\"lightmap\"), meshlets: true)",
        ))
        .expect("parse");
        assert_eq!(s.scale, 0.01);
        assert_eq!(s.sample_rate, 60.0);
        assert_eq!(s.uv_set.as_deref(), Some("lightmap"));
        assert!(s.import_animation);
        assert_eq!(
            FbxImportSettings::try_from_meta_string(None),
            Ok(FbxImportSettings::default())
        );
        assert_eq!(
            FbxImportSettings::try_from_meta_string(Some("(scale: \"x\")")).map_err(|e| e.code()),
            Err("fbx_bad_settings")
        );
        let limited =
            FbxImportSettings::try_from_meta_string(Some("(limits: (max_vertices: 10))")).expect("limits");
        assert_eq!(limited.limits.max_vertices, 10);
        assert_eq!(limited.limits.max_nodes, FbxLimits::default().max_nodes);
    }

    #[cfg(feature = "fbx_import")]
    #[test]
    fn the_sample_triangle_converts() {
        let conversion = convert_fbx_to_glb(TRIANGLE_FBX.as_bytes(), None, &FbxImportSettings::default())
            .expect("the sample converts");
        assert_eq!(conversion.report.meshes, 1);
        assert_eq!(conversion.report.triangles, 1);
        assert_eq!(conversion.report.vertices, 3);
        assert!(conversion.report.ascii);
        assert_eq!(conversion.report.source_up_axis, "+Y");
        assert!(gltf::Gltf::from_slice(&conversion.glb).is_ok());
    }

    /// ADR-7972: without the `fbx_import` feature an `.fbx` import answers `importer_disabled`
    /// (never a panic, never an empty result); with it the same call converts the sample.
    #[test]
    fn fbx_import_answers_importer_disabled_only_without_the_feature() {
        let result = import_fbx_bytes(
            TRIANGLE_FBX.as_bytes(),
            AssetId::new_random(),
            Forward::Convert,
            "linux",
            TextureCookQuality::default(),
            &FbxImportSettings::default(),
        );
        if cfg!(feature = "fbx_import") {
            assert!(result.is_ok(), "{:?}", result.err());
        } else {
            let err = result.expect_err("the import must fail without the parser");
            assert_eq!(err.code(), Some("importer_disabled"), "{err}");
            assert!(
                matches!(
                    err,
                    ImportError::ImporterDisabled {
                        importer: "fbx",
                        feature: "fbx_import"
                    }
                ),
                "{err}"
            );
            assert!(err.to_string().contains("fbx_import"), "{err}");
        }
    }

    #[test]
    fn error_codes_are_protocol_shaped() {
        let errors = [
            FbxError::Io(String::new()),
            FbxError::TooLarge { size: 0, limit: 0 },
            FbxError::NotFbx(String::new()),
            FbxError::TooDeep(0),
            FbxError::Malformed(String::new()),
            FbxError::Parse(String::new()),
            FbxError::UnsupportedVersion(String::new()),
            FbxError::Limit(String::new()),
            FbxError::Settings(String::new()),
            FbxError::NoMesh,
        ];
        for error in errors {
            let code = error.code();
            assert!(code.starts_with("fbx_"), "{code}");
            assert!(
                code.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{code}"
            );
        }
    }
}
