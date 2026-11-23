// SPDX-License-Identifier: MPL-2.0
//! Full glTF 2.0 import (`docs/specs/12-assets.md` § 7, `docs/formats/gltf-conventions.md`).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use bevy_mikktspace::{Geometry, TangentSpace, generate_tangents};
use regia_asset::{
    AlphaMode, AssetId, CookedMesh, CookedStandardMaterial, CpuMesh, CpuMorphTarget, CpuSkin, CullMode,
    LodGroup, MaterialAssetRon, MaterialVariantSet, PrefabInstanceSet, PrefabInstancedNode, PrefabSocketSet,
    SubAssetEntry, TextureCookUsage, TextureSlot, TransmissionParams,
};
use regia_core::math::Mat4;
use tracing::instrument;

use crate::gltf_anim::SkinJointLayout;
use crate::gltf_extensions::{GltfExtensionRegistry, GltfExtensionTable};
use crate::gltf_normalize::{
    GltfLoadData, Normalized, is_fallback_buffer, normalize, prepare_root, raw_json,
};
use crate::gltf_settings::GltfImporterSettings;
use crate::lod_classes::LodThresholdInputs;
use crate::mesh_cook::{cook_mesh, pack_cooked_mesh};
use crate::texture_compress::{TextureCookError, TextureCookQuality};
use crate::{Forward, ImportError};

/// One cooked sub-asset emitted from a glTF source.
#[derive(Clone, Debug)]
pub struct GltfSubCook {
    /// Stable sub path (`mesh/Body/primitive_0`, `texture/0`, ...).
    pub path: String,
    /// Deterministic id.
    pub id: AssetId,
    /// Reflect type path for the cooked payload.
    pub type_path: String,
    /// Postcard payload bytes (caller wraps in cooked header).
    pub payload: Vec<u8>,
    /// Dependency asset ids embedded in the payload.
    pub dependencies: Vec<AssetId>,
}

/// Result of importing one glTF file.
#[derive(Clone, Debug)]
pub struct GltfImportResult {
    /// Primary packed mesh shown when loading the source path.
    ///
    /// The first `_LOD0` mesh, or the first mesh whose name has no `_LODn` tag.
    /// When every triangle mesh is `_LODn` with `n > 0`, this is the lowest index
    /// present (the first such mesh in the scene walk). ADR-7959.
    pub primary_mesh: CookedMesh,
    /// Sub-assets (extra meshes, materials, textures).
    pub sub_assets: Vec<GltfSubCook>,
    /// Entries to store in `.meta`.
    pub meta_entries: Vec<SubAssetEntry>,
    /// Authored `_LOD0`…`_LODn` chains grouped by stem.
    pub lod_groups: Vec<LodGroup>,
    /// `socket_*` empties for the cooked prefab.
    pub sockets: PrefabSocketSet,
    /// Rem / A52 (and similar) material variant sets selectable at runtime.
    pub material_variants: Vec<MaterialVariantSet>,
    /// glTF extensions: the typed material extensions the cook did not store in the cooked
    /// material (sheen, specular, texture transforms) and the raw JSON of unknown ones
    /// ([`crate::gltf_extensions`]). Not part of any cooked format.
    pub extensions: GltfExtensionTable,
    /// The `collision_*` proxies fitted to shapes (also written as the `collision` sub-asset), or
    /// `None` when the file has none (ADR-2341, [`crate::collision_proxies`]).
    pub collision: Option<regia_asset::CookedCollisionSet>,
    /// The material sub-asset id of each glTF material, by document index: how the voxel far
    /// cook finds a material's extensions in [`Self::extensions`] (ADR-3227).
    pub material_ids: Vec<AssetId>,
    /// `EXT_mesh_gpu_instancing`: each instanced node's parts and matrices (also written as the
    /// `prefab/instances` sub-asset), or `None` when the file has none (ADR-3016).
    pub instances: Option<PrefabInstanceSet>,
}

/// Import a `.gltf` or `.glb` from disk (supports external buffers and images).
#[instrument(skip_all, fields(path = %source.display()))]
pub fn import_gltf_path(
    source: &Path,
    parent_id: AssetId,
    forward: Forward,
    platform: &str,
    meta_settings: Option<&str>,
    texture_quality: TextureCookQuality,
) -> Result<GltfImportResult, ImportError> {
    import_gltf_path_with_handlers(
        source,
        parent_id,
        forward,
        platform,
        meta_settings,
        texture_quality,
        &mut GltfExtensionRegistry::new(),
    )
}

/// [`import_gltf_path`] that also calls the registry's extension handlers
/// ([`GltfExtensionRegistry`]) for every extension block of the file.
#[instrument(skip_all, fields(path = %source.display()))]
pub fn import_gltf_path_with_handlers(
    source: &Path,
    parent_id: AssetId,
    forward: Forward,
    platform: &str,
    meta_settings: Option<&str>,
    texture_quality: TextureCookQuality,
    handlers: &mut GltfExtensionRegistry,
) -> Result<GltfImportResult, ImportError> {
    import_gltf_path_inner(
        source,
        parent_id,
        forward,
        platform,
        meta_settings,
        texture_quality,
        handlers,
        None,
    )
}

/// [`import_gltf_path`] with the authored LOD chains' screen sizes resolved against a project
/// class table first ([`crate::lod_classes::LodThresholdInputs::resolve`], ADR-260). The other
/// entry points read only the `_LODn` nodes' own extras.
#[instrument(skip_all, fields(path = %source.display()))]
pub fn import_gltf_path_with_lod(
    source: &Path,
    parent_id: AssetId,
    forward: Forward,
    platform: &str,
    meta_settings: Option<&str>,
    texture_quality: TextureCookQuality,
    lod: &LodThresholdInputs,
) -> Result<GltfImportResult, ImportError> {
    import_gltf_path_inner(
        source,
        parent_id,
        forward,
        platform,
        meta_settings,
        texture_quality,
        &mut GltfExtensionRegistry::new(),
        Some(lod),
    )
}

#[allow(clippy::too_many_arguments)]
fn import_gltf_path_inner(
    source: &Path,
    parent_id: AssetId,
    forward: Forward,
    platform: &str,
    meta_settings: Option<&str>,
    texture_quality: TextureCookQuality,
    handlers: &mut GltfExtensionRegistry,
    lod: Option<&LodThresholdInputs>,
) -> Result<GltfImportResult, ImportError> {
    let GltfOpened {
        document,
        buffers,
        images,
        load_data,
    } = parse_gltf_path(source)?;
    let buffer_bytes: Vec<Vec<u8>> = buffers.into_iter().map(|b| b.0).collect();
    let importer_settings = GltfImporterSettings::from_meta_string(meta_settings);
    // An explicit `.meta` `forward` overrides the argument. Omitted, the argument stands, so a
    // caller that passes `Forward::Keep` without that key still keeps the file's axes. `scale`
    // always comes from the settings (default 1). Converted importers do not take this path:
    // an FBX `.meta` `scale` is that importer's unit scale, already applied to the GLB.
    let forward = crate::gltf_settings::explicit_forward(meta_settings).unwrap_or(forward);
    let scale = importer_settings.import_scale();
    // Morph target names live in `mesh.extras.targetNames`, and extension blocks the typed
    // document does not keep; an unreadable file only loses the names and the side table.
    let source_bytes = std::fs::read(source).unwrap_or_default();
    let target_names = morph_target_names(&source_bytes);
    let extensions = GltfExtensionTable::from_bytes(&source_bytes, handlers);
    let lod = match lod {
        Some(lod) => lod.clone(),
        None => extras_only_lod_inputs(&source_bytes),
    };
    import_document(
        &document,
        &buffer_bytes,
        &images,
        parent_id,
        forward,
        scale,
        platform,
        source.parent(),
        importer_settings.mesh_lods(),
        importer_settings.drop_double_sided_on_closed,
        importer_settings.vertex_ao,
        importer_settings.eight_skin_influences(),
        texture_quality,
        &target_names,
        extensions,
        &lod,
        &load_data,
        &importer_settings.part_options(),
    )
}

/// The LOD threshold inputs of a file imported without a project: its `_LODn` nodes' extras.
fn extras_only_lod_inputs(bytes: &[u8]) -> LodThresholdInputs {
    let (inputs, warnings) = LodThresholdInputs::resolve(bytes, "", None);
    for warning in warnings {
        tracing::warn!(target: "regia_asset_pipeline", "{warning}");
    }
    inputs
}

/// Import from in-memory GLB (used by unit tests).
pub fn import_gltf_glb_bytes(
    bytes: &[u8],
    parent_id: AssetId,
    forward: Forward,
    platform: &str,
    texture_quality: TextureCookQuality,
) -> Result<GltfImportResult, ImportError> {
    import_gltf_glb_bytes_with_handlers(
        bytes,
        parent_id,
        forward,
        platform,
        texture_quality,
        &mut GltfExtensionRegistry::new(),
    )
}

/// [`import_gltf_glb_bytes`] that also calls the registry's extension handlers.
pub fn import_gltf_glb_bytes_with_handlers(
    bytes: &[u8],
    parent_id: AssetId,
    forward: Forward,
    platform: &str,
    texture_quality: TextureCookQuality,
    handlers: &mut GltfExtensionRegistry,
) -> Result<GltfImportResult, ImportError> {
    glb_bytes_with_parts(
        bytes,
        parent_id,
        forward,
        platform,
        texture_quality,
        handlers,
        &crate::gltf_settings::PartOptions::default(),
    )
}

/// [`import_gltf_glb_bytes`] with a `.meta` settings string's per-mesh and per-material options
/// (`meshes`, `materials`; ADR-8376) applied. The other keys are not read here (scale 1, the
/// defaults); [`import_gltf_path`] reads them all. The import dialog extracts a material from
/// this import of a file it has in memory.
///
/// # Errors
///
/// As [`import_gltf_glb_bytes`].
pub fn import_gltf_glb_bytes_with_settings(
    bytes: &[u8],
    parent_id: AssetId,
    forward: Forward,
    platform: &str,
    texture_quality: TextureCookQuality,
    meta_settings: Option<&str>,
) -> Result<GltfImportResult, ImportError> {
    let settings = GltfImporterSettings::from_meta_string_converted(meta_settings);
    glb_bytes_with_parts(
        bytes,
        parent_id,
        forward,
        platform,
        texture_quality,
        &mut GltfExtensionRegistry::new(),
        &settings.part_options(),
    )
}

fn glb_bytes_with_parts(
    bytes: &[u8],
    parent_id: AssetId,
    forward: Forward,
    platform: &str,
    texture_quality: TextureCookQuality,
    handlers: &mut GltfExtensionRegistry,
    part_options: &crate::gltf_settings::PartOptions,
) -> Result<GltfImportResult, ImportError> {
    let (gltf, raw) = parse_gltf_bytes(bytes)?;
    let buffers = embedded_buffers(&gltf, &raw, "GLB")?;
    let Normalized {
        document,
        buffers,
        data: load_data,
    } = normalize(gltf.document, &raw, buffers)?;
    let images = decode_images(&document, &buffers)?;
    import_document(
        &document,
        &buffers,
        &images,
        parent_id,
        forward,
        1.0,
        platform,
        None,
        None,
        false,
        false,
        false,
        texture_quality,
        &morph_target_names(bytes),
        GltfExtensionTable::from_bytes(bytes, handlers),
        &extras_only_lod_inputs(bytes),
        &load_data,
        part_options,
    )
}

/// The buffers of a file parsed from memory: the binary chunk for a buffer with no URI, nothing yet
/// for a fallback buffer (it fills in when its views decode).
fn embedded_buffers(
    gltf: &gltf::Gltf,
    raw: &serde_json::Value,
    what: &str,
) -> Result<Vec<Vec<u8>>, ImportError> {
    let blob = gltf.blob.clone().unwrap_or_default();
    let mut buffers: Vec<Vec<u8>> = Vec::new();
    for buffer in gltf.document.buffers() {
        if is_fallback_buffer(raw, buffer.index()) {
            buffers.push(Vec::new());
            continue;
        }
        match buffer.source() {
            gltf::buffer::Source::Bin => buffers.push(blob.clone()),
            gltf::buffer::Source::Uri(uri) => {
                return Err(ImportError::Gltf(format!(
                    "{what} buffer uri not supported: {uri}"
                )));
            }
        }
    }
    Ok(buffers)
}

/// Import a glTF 2.0 binary another importer converted in memory (the FBX importer, ADR-1602).
/// Embedded images are decoded from the binary chunk; a relative image URI is read from
/// `base_dir` (where that image's own `.meta` texture overrides also apply). The `.meta`
/// settings string and the LOD inputs mean what they mean for a `.glb` on disk.
#[allow(clippy::too_many_arguments)] // `import_document`'s inputs for a buffer with a directory.
pub(crate) fn import_gltf_converted(
    bytes: &[u8],
    base_dir: Option<&Path>,
    parent_id: AssetId,
    forward: Forward,
    platform: &str,
    meta_settings: Option<&str>,
    texture_quality: TextureCookQuality,
    lod: &LodThresholdInputs,
) -> Result<GltfImportResult, ImportError> {
    let (gltf, raw) = parse_gltf_bytes(bytes)?;
    reject_hostile_uris(&gltf.document, base_dir)?;
    let buffers = embedded_buffers(&gltf, &raw, "converted GLB")?;
    let Normalized {
        document,
        buffers,
        data: load_data,
    } = normalize(gltf.document, &raw, buffers)?;
    let mut images = Vec::new();
    for image in document.images() {
        let rgba = match image.source() {
            gltf::image::Source::View { view, .. } => {
                let buffer = buffers
                    .get(view.buffer().index())
                    .ok_or_else(|| ImportError::Gltf("image buffer index out of range".into()))?;
                let slice = view
                    .offset()
                    .checked_add(view.length())
                    .and_then(|end| buffer.get(view.offset()..end))
                    .ok_or_else(|| ImportError::Gltf("image view out of range".into()))?;
                image::load_from_memory(slice).map_err(|e| ImportError::Image(e.to_string()))?
            }
            gltf::image::Source::Uri { uri, .. } => {
                let dir = base_dir.ok_or_else(|| {
                    ImportError::Image(format!("external image {uri} requires source directory"))
                })?;
                let path = dir.join(decode_gltf_uri(uri).unwrap_or_else(|| uri.to_string()));
                image::open(&path).map_err(|e| ImportError::Image(format!("{}: {e}", path.display())))?
            }
        }
        .to_rgba8();
        let (width, height) = rgba.dimensions();
        images.push(gltf::image::Data {
            pixels: rgba.into_raw(),
            format: gltf::image::Format::R8G8B8A8,
            width,
            height,
        });
    }
    let importer_settings = GltfImporterSettings::from_meta_string_converted(meta_settings);
    import_document(
        &document,
        &buffers,
        &images,
        parent_id,
        forward,
        1.0,
        platform,
        base_dir,
        importer_settings.mesh_lods(),
        importer_settings.drop_double_sided_on_closed,
        importer_settings.vertex_ao,
        importer_settings.eight_skin_influences(),
        texture_quality,
        &morph_target_names(bytes),
        GltfExtensionTable::from_bytes(bytes, &mut GltfExtensionRegistry::new()),
        lod,
        &load_data,
        &importer_settings.part_options(),
    )
}

/// `meshes[i].extras.targetNames` (the glTF 2.0 convention for morph target names) per mesh,
/// read from the JSON of a `.gltf` or the JSON chunk of a `.glb`. Missing or malformed names
/// give empty lists (targets are then named `target_<n>`).
pub fn morph_target_names(bytes: &[u8]) -> Vec<Vec<String>> {
    let json = if bytes.starts_with(b"glTF") {
        let chunk_len = bytes
            .get(12..16)
            .and_then(|b| b.try_into().ok())
            .map(u32::from_le_bytes)
            .unwrap_or(0) as usize;
        if bytes.get(16..20) != Some(b"JSON".as_slice()) {
            return Vec::new();
        }
        match bytes.get(20..20usize.saturating_add(chunk_len)) {
            Some(json) => json,
            None => return Vec::new(),
        }
    } else {
        bytes
    };
    let Ok(root) = serde_json::from_slice::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(meshes) = root.get("meshes").and_then(|m| m.as_array()) else {
        return Vec::new();
    };
    meshes
        .iter()
        .map(|mesh| {
            mesh.pointer("/extras/targetNames")
                .and_then(|names| names.as_array())
                .map(|names| {
                    names
                        .iter()
                        .map(|n| n.as_str().unwrap_or_default().to_string())
                        .collect()
                })
                .unwrap_or_default()
        })
        .collect()
}

/// Per-document data the mesh importer needs for skins and morph targets.
struct MeshContext<'a> {
    /// Joint layout per glTF skin (an unusable skin cooks its meshes rigid, with a warning).
    skins: &'a [Result<SkinJointLayout, String>],
    /// Per glTF skin, the matrix its skinned primitives bake in
    /// ([`crate::gltf_anim::skin_bake_matrix`]; the skeleton's inverse bind matrices undo it).
    skin_bakes: &'a [[f32; 16]],
    /// Morph target names per glTF mesh.
    target_names: &'a [Vec<String>],
    /// `vertex_ao` importer setting (ADR-1512): COLOR_0 alpha is an occlusion factor.
    vertex_ao: bool,
    /// `skin_influences: 8` importer setting (ADR-1622): a skin with a second influence set
    /// opts into drawing all eight.
    eight_influences: bool,
    /// `EXT_mesh_gpu_instancing`: per node index, the local matrices of its instances.
    instances: &'a BTreeMap<usize, Vec<[f32; 16]>>,
    /// Mesh keys the `.meta` `meshes` option leaves out (ADR-8376).
    skip_meshes: &'a std::collections::BTreeSet<String>,
}

/// Reject a GLB whose declared header length is below the 12-byte header, which the `gltf` crate
/// (1.4) reads as `length - 12` and would underflow-panic on. A plain pre-check, not a
/// `catch_unwind`, so it holds under `panic = "abort"` (shipping, where a panic cannot be
/// unwound). JSON glTF has no GLB length field and is left to the parser.
pub(crate) fn reject_hostile_glb_header(bytes: &[u8]) -> Result<(), String> {
    if !bytes.starts_with(b"glTF") {
        return Ok(());
    }
    if bytes.len() < 12 {
        return Err("GLB header is truncated (fewer than 12 bytes)".to_string());
    }
    let length = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    if length < 12 {
        return Err(format!(
            "GLB header declares length {length}, less than the 12-byte header"
        ));
    }
    Ok(())
}

/// `gltf::Gltf::from_slice` without the indices gltf 1.4 leaves unchecked: the primitive
/// validation reads `accessors[POSITION]` before anything checks the index, and an animation
/// channel's target node is never validated (`Channel::target().node()` unwraps it). Either
/// panics on a hostile file (aborts the process under `panic = "abort"`, shipping). The document
/// is parsed unvalidated, the extension policy runs, those indices are checked, then the full
/// validation runs.
///
/// Also returns the file's raw JSON, for the extension blocks the typed document does not keep. The
/// error keeps its agent code.
pub(crate) fn checked_parse(bytes: &[u8]) -> Result<(gltf::Gltf, serde_json::Value), ImportError> {
    let gltf =
        gltf::Gltf::from_slice_without_validation(bytes).map_err(|e| ImportError::Gltf(e.to_string()))?;
    let raw = raw_json(bytes);
    let gltf = validated(gltf, &raw, true)?;
    Ok((gltf, raw))
}

/// [`checked_parse`] without the forest check, for a caller whose own node walk already visits
/// each node once (the cloth cook, audit F5): its answer for a looping file stays its own.
pub(crate) fn checked_parse_any_nodes(bytes: &[u8]) -> Result<(gltf::Gltf, serde_json::Value), ImportError> {
    let gltf =
        gltf::Gltf::from_slice_without_validation(bytes).map_err(|e| ImportError::Gltf(e.to_string()))?;
    let raw = raw_json(bytes);
    let gltf = validated(gltf, &raw, false)?;
    Ok((gltf, raw))
}

/// Bytes of zero-filled elements the view-less accessors of one glTF may declare, per byte of its
/// buffers (ADR-5869). A sparse morph target without a view is as large as its mesh's positions,
/// which the buffers hold; sixteen such targets per stored byte is far past real files.
const VIEWLESS_ACCESSOR_RATIO: u64 = 16;

/// The view-less budget of a glTF with tiny (or no) buffers: 64 MiB.
const VIEWLESS_ACCESSOR_MIN_BYTES: u64 = 64 * 1024 * 1024;

/// glTF 2.0 § 3.5.3: the nodes form disjoint strict trees, so a node has at most one parent and no
/// node is its own ancestor. The importer's walk guards its own recursion, but every other walker
/// of a node graph (scene export, plant export, the editor's hierarchy and mesh loaders) trusted
/// it: a cycle looped for ever and a node shared by two parents doubled the work per level
/// (ADR-5869). Checked here, every caller of the checked parse gets a forest. Linear time.
fn reject_non_tree_nodes(nodes: &[gltf::json::Node]) -> Result<(), String> {
    const NONE: usize = usize::MAX;
    let mut parent = vec![NONE; nodes.len()];
    for (n, node) in nodes.iter().enumerate() {
        for child in node.children.iter().flatten() {
            let c = child.value();
            if c >= nodes.len() {
                continue; // the validation reports the index
            }
            if c == n {
                return Err(format!(
                    "glTF nodes must form trees (glTF 2.0 § 3.5.3): node {n} is its own child"
                ));
            }
            if parent[c] != NONE {
                return Err(format!(
                    "glTF nodes must form trees (glTF 2.0 § 3.5.3): node {c} has more than one parent ({} and {n})",
                    parent[c]
                ));
            }
            parent[c] = n;
        }
    }
    // One parent each: a cycle is a walk up that comes back to a node of the same walk.
    // 0 = not seen, 1 = on the current walk, 2 = known to reach a root.
    let mut state = vec![0_u8; nodes.len()];
    let mut path = Vec::new();
    for start in 0..nodes.len() {
        let mut at = start;
        while at != NONE && state[at] == 0 {
            state[at] = 1;
            path.push(at);
            at = parent[at];
        }
        if at != NONE && state[at] == 1 {
            return Err(format!(
                "glTF nodes must form trees (glTF 2.0 § 3.5.3): node {at} is its own ancestor (a cycle)"
            ));
        }
        for n in path.drain(..) {
            state[n] = 2;
        }
    }
    Ok(())
}

/// Checks what gltf 1.4 unwraps without validating (the `POSITION` accessor and channel target
/// node indices, the accessor types of animation samplers and skins, image sources), then runs
/// the `gltf` crate's validation.
fn validated(gltf: gltf::Gltf, raw: &serde_json::Value, forest: bool) -> Result<gltf::Gltf, ImportError> {
    let gltf::Gltf { document, blob } = gltf;
    let mut json = document.into_json();
    let quantized = crate::gltf_normalize::declares_mesh_quantization(&json);
    prepare_root(&mut json, raw)?;
    let document = validated_structure(json, forest, quantized).map_err(ImportError::Gltf)?;
    Ok(gltf::Gltf { document, blob })
}

/// The checks of [`validated`] on the prepared root. `quantized`: the file declares
/// `KHR_mesh_quantization` (read before the policy cleared its required list).
fn validated_structure(
    json: gltf::json::Root,
    forest: bool,
    quantized: bool,
) -> Result<gltf::Document, String> {
    let accessors = json.accessors.len();
    for (m, mesh) in json.meshes.iter().enumerate() {
        for (p, primitive) in mesh.primitives.iter().enumerate() {
            let position = gltf::json::validation::Checked::Valid(gltf::Semantic::Positions);
            if let Some(index) = primitive.attributes.get(&position)
                && index.value() >= accessors
            {
                return Err(format!(
                    "mesh {m} primitive {p}: POSITION names accessor {} of {accessors}",
                    index.value()
                ));
            }
        }
    }
    // The readers assume the accessor types glTF requires for these slots (a VEC3 read as a
    // rotation's VEC4 trips gltf's size assertion); a wrong type is an error here instead.
    use gltf::json::accessor::Type;
    use gltf::json::animation::Property;
    use gltf::json::validation::Checked;
    let accessor_type = |index: usize| match json.accessors.get(index).map(|a| &a.type_) {
        Some(Checked::Valid(kind)) => Some(*kind),
        _ => None,
    };
    let wrong = |index: usize, want: Type| accessor_type(index).is_some_and(|kind| kind != want);
    let nodes = json.nodes.len();
    if forest {
        reject_non_tree_nodes(&json.nodes)?;
    }
    for (a, animation) in json.animations.iter().enumerate() {
        for (c, channel) in animation.channels.iter().enumerate() {
            if channel.target.node.value() >= nodes {
                return Err(format!(
                    "animation {a} channel {c}: target node {} of {nodes}",
                    channel.target.node.value()
                ));
            }
            let Some(sampler) = animation.samplers.get(channel.sampler.value()) else {
                continue; // the validation below reports the index
            };
            let output = match channel.target.path {
                Checked::Valid(Property::Translation | Property::Scale) => Some(Type::Vec3),
                Checked::Valid(Property::Rotation) => Some(Type::Vec4),
                Checked::Valid(Property::MorphTargetWeights) => Some(Type::Scalar),
                Checked::Invalid => None,
            };
            // The rotation and morph-weight readers handle 8/16-bit integer and float outputs
            // only; any other component type reaches an `unreachable!` in gltf 1.4.
            if matches!(
                channel.target.path,
                Checked::Valid(Property::Rotation | Property::MorphTargetWeights)
            ) && json
                .accessors
                .get(sampler.output.value())
                .is_some_and(|a| match &a.component_type {
                    Checked::Valid(c) => c.0 == gltf::json::accessor::ComponentType::U32,
                    Checked::Invalid => false,
                })
            {
                return Err(format!(
                    "animation {a} channel {c}: output accessor has an unsupported component type"
                ));
            }
            if wrong(sampler.input.value(), Type::Scalar)
                || output.is_some_and(|want| wrong(sampler.output.value(), want))
            {
                return Err(format!(
                    "animation {a} channel {c}: sampler accessors have the wrong type for the target"
                ));
            }
        }
    }
    // Byte ranges: the accessor readers add offsets, strides and counts unchecked (an overflow
    // panics in debug, a zero count underflows). glTF requires every view inside its buffer and
    // every accessor (count >= 1) inside its view.
    for (v, view) in json.buffer_views.iter().enumerate() {
        let end = view
            .byte_offset
            .map_or(0, |o| o.0)
            .checked_add(view.byte_length.0);
        let buffer = json.buffers.get(view.buffer.value()).map(|b| b.byte_length.0);
        if end.is_none_or(|end| buffer.is_some_and(|len| end > len)) {
            return Err(format!("buffer view {v} reaches past its buffer"));
        }
    }
    // Whether `n` elements of `elem` bytes from `offset` fit in view `view` (an unknown view is
    // left to the validation).
    let fits = |view: usize, offset: u64, n: u64, elem: u64| -> bool {
        let Some(view) = json.buffer_views.get(view) else {
            return true;
        };
        let stride = view.byte_stride.map_or(elem, |s| s.0 as u64).max(elem);
        n > 0
            && stride
                .checked_mul(n - 1)
                .and_then(|bytes| bytes.checked_add(elem))
                .and_then(|bytes| bytes.checked_add(offset))
                .is_some_and(|end| end <= view.byte_length.0)
    };
    let mut viewless_bytes = 0_u64;
    for (i, accessor) in json.accessors.iter().enumerate() {
        let (Checked::Valid(kind), Checked::Valid(component)) = (&accessor.type_, &accessor.component_type)
        else {
            continue; // the validation reports it
        };
        let elem = (component.0.size() * kind.multiplicity()) as u64;
        let offset = accessor.byte_offset.map_or(0, |o| o.0);
        // A stride below the element size makes the readers' element slice too short.
        let stride_ok = accessor
            .buffer_view
            .and_then(|view| json.buffer_views.get(view.value()))
            .and_then(|view| view.byte_stride)
            .is_none_or(|stride| stride.0 as u64 >= elem);
        if !stride_ok {
            return Err(format!("accessor {i}: byteStride is below the element size"));
        }
        let base_ok = accessor
            .buffer_view
            .is_none_or(|view| fits(view.value(), offset, accessor.count.0, elem));
        // An accessor without a view is `count` zeros (plus its sparse values), so `count` is
        // backed by no bytes at all: a 200-byte file could ask the readers for billions of
        // elements. Hold the zero-filled elements of all such accessors to a budget tied to the
        // file's real data (ADR-5869).
        if accessor.buffer_view.is_none() {
            viewless_bytes = viewless_bytes.saturating_add(accessor.count.0.saturating_mul(elem));
        }
        let sparse_ok = accessor.sparse.as_ref().is_none_or(|sparse| {
            let index_size = match &sparse.indices.component_type {
                Checked::Valid(indices) => indices.0.size() as u64,
                Checked::Invalid => return true,
            };
            fits(
                sparse.indices.buffer_view.value(),
                sparse.indices.byte_offset.0,
                sparse.count.0,
                index_size,
            ) && fits(
                sparse.values.buffer_view.value(),
                sparse.values.byte_offset.0,
                sparse.count.0,
                elem,
            )
        });
        if !base_ok || !sparse_ok {
            return Err(format!(
                "accessor {i} reaches past its buffer view (or has no elements)"
            ));
        }
    }
    let data_bytes = json
        .buffers
        .iter()
        .fold(0_u64, |sum, b| sum.saturating_add(b.byte_length.0));
    let viewless_budget = data_bytes
        .saturating_mul(VIEWLESS_ACCESSOR_RATIO)
        .max(VIEWLESS_ACCESSOR_MIN_BYTES);
    if viewless_bytes > viewless_budget {
        return Err(format!(
            "accessors without a buffer view declare {viewless_bytes} bytes of zeros, more than {viewless_budget} \
             ({VIEWLESS_ACCESSOR_RATIO}x the file's {data_bytes} buffer bytes)"
        ));
    }
    // Vertex streams: each reader assumes its semantic's glTF type (TEXCOORD VEC2, POSITION
    // VEC3, ...); another type trips gltf's element-size assertion.
    for (m, mesh) in json.meshes.iter().enumerate() {
        for (p, primitive) in mesh.primitives.iter().enumerate() {
            let mut streams: Vec<(usize, &[Type])> = Vec::new();
            for (semantic, index) in &primitive.attributes {
                let Checked::Valid(semantic) = semantic else {
                    continue;
                };
                use gltf::Semantic as S;
                let allowed: &[Type] = if matches!(semantic, S::Positions | S::Normals) {
                    &[Type::Vec3]
                } else if matches!(semantic, S::Tangents | S::Joints(_) | S::Weights(_)) {
                    &[Type::Vec4]
                } else if matches!(semantic, S::TexCoords(_)) {
                    &[Type::Vec2]
                } else if matches!(semantic, S::Colors(_)) {
                    &[Type::Vec3, Type::Vec4]
                } else {
                    &[]
                };
                streams.push((index.value(), allowed));
            }
            if let Some(indices) = primitive.indices {
                streams.push((indices.value(), &[Type::Scalar]));
            }
            for target in primitive.targets.iter().flatten() {
                for index in [target.positions, target.normals, target.tangents]
                    .into_iter()
                    .flatten()
                {
                    streams.push((index.value(), &[Type::Vec3]));
                }
            }
            for (index, allowed) in streams {
                if !allowed.is_empty() && accessor_type(index).is_some_and(|kind| !allowed.contains(&kind)) {
                    return Err(format!(
                        "mesh {m} primitive {p}: accessor {index} has the wrong type for its use"
                    ));
                }
            }
        }
    }
    // The texcoord, color, joint, weight and index readers match on the component type and reach
    // an `unreachable!` in gltf 1.4 for any other (a u32 TEXCOORD_0 from the guided fuzzer,
    // ADR-2723). glTF 2.0 § 3.7.2.1 allows only these. `KHR_mesh_quantization` adds BYTE and SHORT
    // to TEXCOORD_n (its table has no other use these checks cover), and only in a file that declares
    // it: `gltf_normalize::normalize` rewrites them as floats before any reader runs, on every path
    // that reads accessor data after this parse (ADR-5958).
    use gltf::json::accessor::ComponentType as C;
    let component = |index: usize| match json.accessors.get(index).map(|a| &a.component_type) {
        Some(Checked::Valid(c)) => Some(c.0),
        _ => None,
    };
    for (m, mesh) in json.meshes.iter().enumerate() {
        for (p, primitive) in mesh.primitives.iter().enumerate() {
            let mut streams: Vec<(usize, &[C])> = Vec::new();
            for (semantic, index) in &primitive.attributes {
                let Checked::Valid(semantic) = semantic else {
                    continue;
                };
                use gltf::Semantic as S;
                if quantized && matches!(semantic, S::TexCoords(_)) {
                    streams.push((index.value(), &[C::I8, C::U8, C::I16, C::U16, C::F32]));
                } else if matches!(semantic, S::TexCoords(_) | S::Colors(_) | S::Weights(_)) {
                    streams.push((index.value(), &[C::U8, C::U16, C::F32]));
                } else if matches!(semantic, S::Joints(_)) {
                    streams.push((index.value(), &[C::U8, C::U16]));
                }
            }
            if let Some(indices) = primitive.indices {
                streams.push((indices.value(), &[C::U8, C::U16, C::U32]));
            }
            for (index, allowed) in streams {
                if component(index).is_some_and(|c| !allowed.contains(&c)) {
                    return Err(format!(
                        "mesh {m} primitive {p}: accessor {index} has a component type its use does not allow"
                    ));
                }
            }
        }
    }
    // `Image::source` unwraps the MIME type of a buffer-view image and the URI of any other.
    for (i, image) in json.images.iter().enumerate() {
        let complete = match image.buffer_view {
            Some(_) => image.mime_type.is_some(),
            None => image.uri.is_some(),
        };
        if !complete {
            return Err(format!(
                "image {i}: a bufferView image needs a mimeType, any other a uri"
            ));
        }
    }
    for (k, skin) in json.skins.iter().enumerate() {
        if skin
            .inverse_bind_matrices
            .is_some_and(|matrices| wrong(matrices.value(), Type::Mat4))
        {
            return Err(format!("skin {k}: inverseBindMatrices is not a MAT4 accessor"));
        }
    }
    gltf::Document::from_json(json).map_err(|e| e.to_string())
}

/// `gltf::Gltf::from_slice`, guarded against a hostile GLB header (see
/// [`reject_hostile_glb_header`]) and contained so any *other* malformed GLB cannot panic the
/// importer in a debug/test build (M18 robustness).
fn parse_gltf_bytes(bytes: &[u8]) -> Result<(gltf::Gltf, serde_json::Value), ImportError> {
    reject_hostile_glb_header(bytes).map_err(ImportError::Gltf)?;
    match std::panic::catch_unwind(|| checked_parse(bytes)) {
        Ok(result) => result,
        Err(_) => Err(ImportError::Gltf(
            "glTF parser panicked on malformed input".into(),
        )),
    }
}

/// A glTF file opened with every data-layout extension applied ([`crate::gltf_normalize`]).
pub struct GltfOpened {
    /// The document, as the accessor readers expect it.
    pub document: gltf::Document,
    /// One entry per buffer of [`Self::document`], decoded.
    pub buffers: Vec<gltf::buffer::Data>,
    /// One entry per image of [`Self::document`], decoded (WebP included).
    pub images: Vec<gltf::image::Data>,
    /// The instance transforms and what was decoded.
    pub load_data: GltfLoadData,
}

impl std::fmt::Debug for GltfOpened {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GltfOpened")
            .field("buffers", &self.buffers.len())
            .field("images", &self.images.len())
            .field("load_data", &self.load_data)
            .finish_non_exhaustive()
    }
}

/// [`parse_gltf_bytes`] for the disk path (`gltf::import` loads external buffers and images too).
fn parse_gltf_path(source: &Path) -> Result<GltfOpened, ImportError> {
    reject_hostile_glb_header_from_path(source)?;
    match std::panic::catch_unwind(|| parse_gltf_path_inner(source)) {
        Ok(result) => result,
        Err(_) => Err(ImportError::Gltf(
            "glTF parser panicked on malformed input".into(),
        )),
    }
}

/// Opens a `.gltf` or `.glb` the way the importer does: the file's checks, its external buffers and
/// images, and every data-layout extension applied. The caller then reads plain floats and sees
/// instance transforms in [`GltfOpened::load_data`].
///
/// # Errors
///
/// The file cannot be read, does not parse, fails the checks, requires an extension the importer
/// cannot read (`gltf_ext_unsupported`), or holds broken extension data.
pub fn open_gltf(source: &Path) -> Result<GltfOpened, ImportError> {
    parse_gltf_path(source)
}

/// What a glTF's extensions amount to, read without decoding images or cooking anything
/// (`asset.gltf_extensions`).
///
/// # Errors
///
/// As [`open_gltf`], for everything but the images.
pub fn gltf_extension_report(
    source: &Path,
) -> Result<crate::gltf_normalize::GltfExtensionReport, ImportError> {
    reject_hostile_glb_header_from_path(source)?;
    let (_, _, data) = std::panic::catch_unwind(|| open_buffers(source))
        .map_err(|_| ImportError::Gltf("glTF parser panicked on malformed input".into()))??;
    let bytes = std::fs::read(source).map_err(|e| ImportError::Gltf(e.to_string()))?;
    let table = GltfExtensionTable::from_bytes(&bytes, &mut GltfExtensionRegistry::new());
    Ok(crate::gltf_normalize::GltfExtensionReport::new(&table, &data))
}

/// Parses a glTF document (extension policy and structure checks included) without reading any buffer.
///
/// # Errors
///
/// As [`open_gltf`], for the checks that need no buffer.
pub fn parse_gltf_document(bytes: &[u8]) -> Result<gltf::Gltf, ImportError> {
    parse_gltf_bytes(bytes).map(|(gltf, _)| gltf)
}

/// What `gltf::import` does (open, load buffers, load images), with the document's external URIs
/// checked in between: the `gltf` crate decodes a relative URI with an `unwrap` that aborts the
/// process under `panic = "abort"` (shipping), which a `catch_unwind` cannot contain.
fn parse_gltf_path_inner(source: &Path) -> Result<GltfOpened, ImportError> {
    let (document, buffers, load_data) = open_buffers(source)?;
    let base = source.parent().unwrap_or_else(|| Path::new("./"));
    let buffers: Vec<gltf::buffer::Data> = buffers.into_iter().map(gltf::buffer::Data).collect();
    // Image headers are budgeted before any decode (ADR-5869).
    check_image_budget(&document, |i| buffers.get(i).map(|d| d.0.as_slice()), Some(base))?;
    let images = load_images(&document, base, &buffers)?;
    Ok(GltfOpened {
        document,
        buffers,
        images,
        load_data,
    })
}

/// The document and decoded buffers of a file on disk.
fn open_buffers(source: &Path) -> Result<(gltf::Document, Vec<Vec<u8>>, GltfLoadData), ImportError> {
    open_buffers_with(source, true)
}

/// [`open_buffers`], with the node forest check only when `forest` is set (see
/// [`checked_parse_any_nodes`]).
fn open_buffers_with(
    source: &Path,
    forest: bool,
) -> Result<(gltf::Document, Vec<Vec<u8>>, GltfLoadData), ImportError> {
    let bytes = std::fs::read(source).map_err(|e| ImportError::Gltf(e.to_string()))?;
    let (gltf, raw) = if forest {
        checked_parse(&bytes)?
    } else {
        checked_parse_any_nodes(&bytes)?
    };
    let base = source.parent().unwrap_or_else(|| Path::new("./"));
    let buffers = disk_buffers(&gltf, &raw, base)?;
    let Normalized {
        document,
        buffers,
        data,
    } = normalize(gltf.document, &raw, buffers)?;
    Ok((document, buffers, data))
}

/// What `gltf::import_buffers` does, leaving a fallback buffer out: the file does not hold it.
pub(crate) fn disk_buffers(
    gltf: &gltf::Gltf,
    raw: &serde_json::Value,
    base: &Path,
) -> Result<Vec<Vec<u8>>, ImportError> {
    reject_hostile_uris(&gltf.document, Some(base))?;
    let mut blob = gltf.blob.clone();
    let mut buffers = Vec::new();
    for buffer in gltf.document.buffers() {
        if is_fallback_buffer(raw, buffer.index()) {
            buffers.push(Vec::new());
            continue;
        }
        let data = gltf::buffer::Data::from_source_and_blob(buffer.source(), Some(base), &mut blob)
            .map_err(|e| ImportError::Gltf(e.to_string()))?;
        if data.len() < buffer.length() {
            return Err(ImportError::Gltf(format!(
                "buffer {} has {} bytes, the file declares {}",
                buffer.index(),
                data.len(),
                buffer.length()
            )));
        }
        buffers.push(data.0);
    }
    Ok(buffers)
}

/// `gltf::import_images` with WebP (`EXT_texture_webp`): the `gltf` crate's decoder takes PNG and JPEG
/// only, and keeps their layouts, so every other image still goes through it.
fn load_images(
    document: &gltf::Document,
    base: &Path,
    buffers: &[gltf::buffer::Data],
) -> Result<Vec<gltf::image::Data>, ImportError> {
    let mut images = Vec::new();
    for image in document.images() {
        let webp = match image.source() {
            gltf::image::Source::View { view, mime_type } => {
                let bytes = buffers
                    .get(view.buffer().index())
                    .and_then(|buffer| buffer.get(view.offset()..view.offset().checked_add(view.length())?));
                bytes
                    .filter(|bytes| mime_type == "image/webp" || is_webp(bytes))
                    .map(<[u8]>::to_vec)
            }
            gltf::image::Source::Uri { uri, mime_type } if uri.starts_with("data:") => {
                (mime_type == Some("image/webp") || uri.starts_with("data:image/webp"))
                    .then(|| data_uri_bytes(uri))
                    .transpose()?
            }
            gltf::image::Source::Uri { uri, mime_type } => {
                let named = mime_type == Some("image/webp")
                    || Path::new(uri)
                        .extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case("webp"));
                if named {
                    let path = base.join(decode_gltf_uri(uri).unwrap_or_else(|| uri.to_string()));
                    Some(
                        std::fs::read(&path)
                            .map_err(|e| ImportError::Image(format!("{}: {e}", path.display())))?,
                    )
                } else {
                    None
                }
            }
        };
        let data = match webp {
            Some(bytes) => {
                let rgba = image::load_from_memory_with_format(&bytes, image::ImageFormat::WebP)
                    .map_err(|e| ImportError::Image(format!("WebP image {}: {e}", image.index())))?
                    .to_rgba8();
                let (width, height) = rgba.dimensions();
                gltf::image::Data {
                    pixels: rgba.into_raw(),
                    format: gltf::image::Format::R8G8B8A8,
                    width,
                    height,
                }
            }
            None => gltf::image::Data::from_source(image.source(), Some(base), buffers)
                .map_err(|e| ImportError::Gltf(e.to_string()))?,
        };
        images.push(data);
    }
    Ok(images)
}

/// The RIFF container with the `WEBP` form type.
fn is_webp(bytes: &[u8]) -> bool {
    bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP"
}

/// Opens a glTF or GLB file and loads its buffers the way the importer does (hostile-header,
/// index, accessor-range and URI checks first, every data-layout extension applied), without
/// decoding images. For tools that read meshes and animation from an untrusted file; a hostile
/// file is an `Err`, not a panic.
///
/// # Errors
///
/// The file cannot be read, does not parse, or fails the checks.
pub fn open_gltf_checked(source: &Path) -> Result<(gltf::Document, Vec<gltf::buffer::Data>), String> {
    open_gltf_checked_with(source, true)
}

/// Opens buffers for authoring diagnostics under explicit input budgets.
/// Images are never decoded. Existing structural, extension and URI checks apply.
///
/// # Errors
/// Returns a typed limit, cancellation, changed-file or malformed-input error.
pub fn open_gltf_bounded(
    source: &Path,
    budget: &mut crate::content_lint::ContentBudget<'_>,
) -> Result<(gltf::Document, Vec<gltf::buffer::Data>), crate::content_lint::ContentError> {
    crate::content_lint::open(source, budget)
}

/// [`open_gltf_checked`] without the node forest check, for the cloth cook, whose own walk visits
/// each node once (audit F5).
pub(crate) fn open_gltf_checked_any_nodes(
    source: &Path,
) -> Result<(gltf::Document, Vec<gltf::buffer::Data>), String> {
    open_gltf_checked_with(source, false)
}

fn open_gltf_checked_with(
    source: &Path,
    forest: bool,
) -> Result<(gltf::Document, Vec<gltf::buffer::Data>), String> {
    reject_hostile_glb_header_from_path(source).map_err(|e| e.to_string())?;
    let (document, buffers, _) = open_buffers_with(source, forest).map_err(|e| match e {
        ImportError::Gltf(message) => message,
        other => other.to_string(),
    })?;
    Ok((document, buffers.into_iter().map(gltf::buffer::Data).collect()))
}

/// `gltf::import` with the importer's checks (ADR-5869): hostile GLB header, the indices gltf 1.4
/// leaves unchecked, accessor ranges, a node forest, view-less accessor and image budgets, and URIs
/// that stay beside the file. For tools (the editor's hierarchy, mesh-source and report loaders)
/// that walk a model the user drops in; a hostile file is an `Err`, not a panic, a hang or an
/// unbounded allocation.
///
/// # Errors
///
/// The file cannot be read, does not parse, or fails a check.
pub fn import_gltf_checked(
    source: &Path,
) -> Result<(gltf::Document, Vec<gltf::buffer::Data>, Vec<gltf::image::Data>), String> {
    let opened = open_gltf(source).map_err(|e| e.to_string())?;
    Ok((opened.document, opened.buffers, opened.images))
}

/// `gltf::Gltf::from_slice` with the checks of [`import_gltf_checked`] that need no file system
/// (no buffers or images are loaded).
///
/// # Errors
///
/// The bytes do not parse or fail a check.
pub fn parse_gltf_checked(bytes: &[u8]) -> Result<gltf::Gltf, String> {
    parse_gltf_document(bytes).map_err(|e| e.to_string())
}

/// Percent-decode a relative glTF URI the way the `gltf` crate does (a malformed `%XX` stays
/// literal), returning `None` when the decoded bytes are not UTF-8 (where the crate would panic).
pub fn decode_gltf_uri(uri: &str) -> Option<String> {
    let bytes = uri.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| (b as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok()
}

/// Reject an external buffer or image URI the `gltf` crate would panic on (a relative URI that
/// percent-decodes to invalid UTF-8), that is not a portable URI (a backslash separator opens on
/// Windows and misses on the Linux cook host), or that leaves the project: the crate opens
/// `file:///x`, `/x` and `../../x` as given, and what it reads is cooked into the build. `base` is
/// the glTF's directory.
pub(crate) fn reject_hostile_uris(document: &gltf::Document, base: Option<&Path>) -> Result<(), ImportError> {
    reject_uris_with(document, |decoded| {
        crate::cook::join_portable(base.unwrap_or_else(|| Path::new("")), decoded)
    })
}

/// [`reject_hostile_uris`] with an explicit bound: every relative URI, resolved against `base`,
/// must stay under `bound` (`cook::join_bounded`), project or not. The cloth cook passes the
/// cloth source's bound (finding 19, ADR-4922).
pub(crate) fn reject_hostile_uris_within(
    document: &gltf::Document,
    base: Option<&Path>,
    bound: &Path,
) -> Result<(), ImportError> {
    reject_uris_with(document, |decoded| {
        crate::cook::join_bounded(base.unwrap_or_else(|| Path::new("")), decoded, Some(bound))
    })
}

pub(crate) fn reject_uris_with(
    document: &gltf::Document,
    join: impl Fn(&str) -> Result<std::path::PathBuf, String>,
) -> Result<(), ImportError> {
    let buffer_uris = document.buffers().filter_map(|b| match b.source() {
        gltf::buffer::Source::Uri(uri) => Some(uri),
        gltf::buffer::Source::Bin => None,
    });
    let image_uris = document.images().filter_map(|i| match i.source() {
        gltf::image::Source::Uri { uri, .. } => Some(uri),
        gltf::image::Source::View { .. } => None,
    });
    for uri in buffer_uris.chain(image_uris) {
        if uri.starts_with("data:") {
            continue;
        }
        if uri.contains('\\') {
            return Err(ImportError::Gltf(format!(
                "glTF URI {uri:?} contains a backslash; use `/` and percent-encoding"
            )));
        }
        if uri.contains(':') {
            return Err(ImportError::Gltf(format!(
                "glTF URI {uri:?} is not a relative path; data: URIs and relative files only"
            )));
        }
        let Some(decoded) = decode_gltf_uri(uri) else {
            return Err(ImportError::Gltf(format!(
                "glTF URI {uri:?} does not percent-decode to valid UTF-8"
            )));
        };
        join(&decoded).map_err(|why| ImportError::Gltf(format!("glTF URI {uri:?}: {why}")))?;
    }
    Ok(())
}

/// [`reject_hostile_glb_header`] for a file: peeks the first 12 bytes so a `.glb` on disk with a
/// hostile length is rejected before [`gltf::import`] reads it (which would underflow-panic).
pub(crate) fn reject_hostile_glb_header_from_path(source: &Path) -> Result<(), ImportError> {
    use std::io::Read;
    let mut head = [0u8; 12];
    let mut file = std::fs::File::open(source).map_err(|e| ImportError::Gltf(e.to_string()))?;
    let read = file
        .read(&mut head)
        .map_err(|e| ImportError::Gltf(e.to_string()))?;
    reject_hostile_glb_header(&head[..read]).map_err(ImportError::Gltf)
}

/// A referenced image's own `.meta` overrides (usage, max size, quality, compression opt-out); an
/// embedded image has none. The material slot it fills is the default usage.
fn gltf_image_settings(
    image: &gltf::Image<'_>,
    base_dir: Option<&Path>,
) -> crate::texture_settings::TextureImporterSettings {
    match (image.source(), base_dir) {
        (gltf::image::Source::Uri { uri, .. }, Some(dir)) if !uri.starts_with("data:") => {
            let path = dir.join(decode_gltf_uri(uri).unwrap_or_else(|| uri.to_string()));
            // Inside a cook, the project's import rules reach a referenced image as they reach
            // a standalone one (ADR-3321); the cook key hashes the rules it matches.
            match crate::import_rules::referenced_settings(&path) {
                Some(settings) => {
                    crate::texture_settings::TextureImporterSettings::from_meta_string(Some(&settings))
                }
                None => crate::texture_settings::TextureImporterSettings::for_source(&path),
            }
        }
        _ => crate::texture_settings::TextureImporterSettings::default(),
    }
}

/// One image of a glTF as [`import_document`] takes it, before encoding
/// ([`visit_gltf_images`]).
pub(crate) struct GltfImageInput {
    /// The image's settings (its `.meta`, for an external file).
    pub(crate) settings: crate::texture_settings::TextureImporterSettings,
    /// The usage the cook gives it: its `.meta`, else the material slot it fills.
    pub(crate) usage: TextureCookUsage,
    /// Width, height and RGBA8 texels after the settings' `max_size`.
    pub(crate) image: (u32, u32, Vec<u8>),
}

/// Calls `visit` with the index and the cook's input of every image of the glTF at `source`,
/// in document order, without cooking or writing anything (`regia cook --size-report`). An image
/// that does not decode reaches `visit` as an error; the others still do.
///
/// # Errors
/// The file does not parse.
pub(crate) fn visit_gltf_images(
    source: &Path,
    mut visit: impl FnMut(usize, Result<GltfImageInput, ImportError>),
) -> Result<(), ImportError> {
    let GltfOpened {
        document,
        images: disk_images,
        ..
    } = parse_gltf_path(source)?;
    let source_bytes = std::fs::read(source).map_err(|e| ImportError::Gltf(e.to_string()))?;
    let extensions = GltfExtensionTable::from_bytes(&source_bytes, &mut GltfExtensionRegistry::new());
    let texture_usage = texture_usages_for_document(&document, &extensions);
    let base_dir = source.parent();
    for (index, image) in document.images().enumerate() {
        let settings = gltf_image_settings(&image, base_dir);
        let usage = settings.usage_or(
            texture_usage
                .get(&index)
                .copied()
                .unwrap_or(TextureCookUsage::ColorSrgb),
        );
        let input = load_image_rgba8(image, &disk_images, base_dir).map(|(w, h, rgba)| GltfImageInput {
            image: settings.fit(w, h, rgba),
            usage,
            settings,
        });
        visit(index, input);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn import_document(
    document: &gltf::Document,
    buffers: &[Vec<u8>],
    disk_images: &[gltf::image::Data],
    parent_id: AssetId,
    forward: Forward,
    scale: f32,
    platform: &str,
    base_dir: Option<&Path>,
    mesh_lods: Option<crate::mesh_cook::MeshLodSettings>,
    drop_double_sided_on_closed: bool,
    vertex_ao: bool,
    eight_influences: bool,
    texture_quality: TextureCookQuality,
    target_names: &[Vec<String>],
    extensions: GltfExtensionTable,
    lod: &LodThresholdInputs,
    load_data: &GltfLoadData,
    part_options: &crate::gltf_settings::PartOptions,
) -> Result<GltfImportResult, ImportError> {
    let mut sub_assets = Vec::new();
    let mut meta_entries = Vec::new();
    let mut texture_ids: HashMap<usize, AssetId> = HashMap::new();
    let texture_usage = texture_usages_for_document(document, &extensions);

    for (image_index, image) in document.images().enumerate() {
        let settings = gltf_image_settings(&image, base_dir);
        let path = format!("texture/{image_index}");
        let usage = settings.usage_or(
            texture_usage
                .get(&image_index)
                .copied()
                .unwrap_or(TextureCookUsage::ColorSrgb),
        );
        let shared_id =
            crate::gltf_texture_share::identity(&image, base_dir, &settings, usage, texture_quality);
        let id = shared_id.unwrap_or_else(|| crate::gltf_texture_share::private_id(parent_id, &path));
        let rgba = load_image_rgba8(image, disk_images, base_dir)?;
        let rgba = settings.fit(rgba.0, rgba.1, rgba.2);
        let reused = shared_id.and_then(|id| {
            crate::gltf_texture_share::cached(id, rgba.0, rgba.1, &rgba.2).or_else(|| {
                // Repeated image declarations in one document have not reached the file writer
                // yet. Their source/settings identity is already the same.
                sub_assets
                    .iter()
                    .find(|sub: &&GltfSubCook| sub.id == id)
                    .map(|sub| (sub.type_path.clone(), sub.payload.clone()))
            })
        });
        if let Some((type_path, payload)) = reused {
            sub_assets.push(GltfSubCook {
                path: path.clone(),
                id,
                type_path: type_path.clone(),
                payload,
                dependencies: vec![],
            });
            meta_entries.push(SubAssetEntry { path, id, type_path });
            texture_ids.insert(image_index, id);
            continue;
        }
        // The `.meta` channel conventions of spec 22 T2.14 (ADR-3320): a DirectX-convention
        // normal's green flips, a packed map in another order swizzles to ORM — at cook, never
        // on disk.
        let mut pixels = rgba.2;
        crate::texture_settings::apply_channel_transforms(
            &mut pixels,
            usage,
            settings.normal_green_or_default(),
            settings.orm_channel_order_or_default(),
        );
        let cooked = if settings.compress() {
            // The image's own `.meta` `alpha_cutoff` keeps its mips' coverage (ADR-3090).
            crate::texture_compress::cook_texture_rgba8_coverage(
                rgba.0,
                rgba.1,
                &pixels,
                usage,
                platform,
                settings.quality_or(texture_quality),
                settings.coverage_cutoff(usage),
            )
        } else {
            Err(TextureCookError::TooSmall {
                width: rgba.0,
                height: rgba.1,
            })
        };
        let cooked = match cooked {
            Ok(t) => settings
                .apply_stream(t)
                .map_err(|e| ImportError::Image(e.to_string()))?,
            // Too small to block-compress, or `compress: false`: an RGBA8 image.
            Err(TextureCookError::TooSmall { .. }) => {
                // Fall back to legacy RGBA8 cooked image for tiny/ASTC-stub paths.
                let img = regia_asset::CookedImage {
                    width: rgba.0,
                    height: rgba.1,
                    rgba8: pixels,
                };
                img.validate().map_err(|e| ImportError::Image(e.to_string()))?;
                let payload = img.to_postcard().map_err(|e| ImportError::Image(e.to_string()))?;
                sub_assets.push(GltfSubCook {
                    path: path.clone(),
                    id,
                    type_path: "regia_asset::CookedImage".to_string(),
                    payload,
                    dependencies: vec![],
                });
                meta_entries.push(SubAssetEntry {
                    path,
                    id,
                    type_path: "regia_asset::CookedImage".to_string(),
                });
                texture_ids.insert(image_index, id);
                continue;
            }
            Err(e) => return Err(ImportError::Image(e.to_string())),
        };
        let payload = cooked
            .to_postcard()
            .map_err(|e| ImportError::Image(e.to_string()))?;
        sub_assets.push(GltfSubCook {
            path: path.clone(),
            id,
            type_path: "regia_asset::CookedTexture".to_string(),
            payload,
            dependencies: vec![],
        });
        meta_entries.push(SubAssetEntry {
            path,
            id,
            type_path: "regia_asset::CookedTexture".to_string(),
        });
        texture_ids.insert(image_index, id);
    }

    let mut primary_mesh: Option<CookedMesh> = None;
    // Lowest `_LODn` (n > 0) seen before any LOD0 or untagged mesh. Used only when
    // the file has no classic primary (ADR-7959).
    let mut lod_fallback: Option<(u32, CookedMesh)> = None;
    let mut lod_mesh_entries: Vec<(String, AssetId)> = Vec::new();
    let mut sockets = Vec::new();
    let mut parts: Vec<(AssetId, Option<usize>, String)> = Vec::new();
    // Skinned primitives write joint indices in the cooked skeleton's order (cooked mesh v2).
    let node_parents = crate::gltf_anim::node_parents(document, document.nodes().len());
    let skins: Vec<Result<SkinJointLayout, String>> = document
        .skins()
        .map(|skin| crate::gltf_anim::skin_joint_layout(document, &skin, &node_parents))
        .collect();
    // A transform cycle is rejected by the node walk below; bake skins with the yaw alone then.
    let node_worlds = crate::gltf_anim::node_worlds(document).unwrap_or_default();
    let skin_bakes: Vec<[f32; 16]> = document
        .skins()
        .map(|skin| {
            crate::gltf_anim::skin_bake_matrix(document, skin.index(), &node_worlds, forward, scale)
                .to_cols_array()
        })
        .collect();
    let mesh_ctx = MeshContext {
        skins: &skins,
        skin_bakes: &skin_bakes,
        target_names,
        vertex_ao,
        eight_influences,
        instances: &load_data.instances,
        skip_meshes: &part_options.skip_meshes,
    };
    let scene = document.default_scene().or_else(|| document.scenes().next());
    let root_nodes: Vec<gltf::Node<'_>> = scene.into_iter().flat_map(|s| s.nodes()).collect();
    let mut walk = NodeWalk::default();
    for node in root_nodes {
        visit_node(
            node,
            root_matrix(forward, scale),
            buffers,
            &mesh_ctx,
            parent_id,
            &mut sub_assets,
            &mut meta_entries,
            &mut primary_mesh,
            &mut lod_fallback,
            mesh_lods.as_ref(),
            &mut lod_mesh_entries,
            &mut sockets,
            &mut parts,
            &mut walk,
        )?;
    }

    // Opt-in cook hint: `doubleSided` dropped on materials only closed, opaque meshes use.
    let closed =
        drop_double_sided_on_closed.then(|| crate::closed_mesh::closed_mesh_report(document, buffers));
    let mut material_names: Vec<(String, AssetId)> = Vec::new();
    // The draw binding of each material (ADR-8376): its own sub-asset, or the project material
    // the `.meta` `materials` option names for its key.
    let mut material_binding: Vec<AssetId> = Vec::new();
    for (mat_index, material) in document.materials().enumerate() {
        let mat_name = material
            .name()
            .map(sanitize_name)
            .unwrap_or_else(|| mat_index.to_string());
        // Duplicate material names: the later ones carry their index (and a counter past it when
        // an authored name already spells that path), so no two materials share an id.
        let path = unused_sub_path(&sub_assets, format!("material/{mat_name}"), |n| match n {
            0 => format!("material/{mat_name}~{mat_index}"),
            n => format!("material/{mat_name}~{mat_index}~{n}"),
        });
        let id = crate::subasset_remap::sub_id(parent_id, &path);
        let mut authored = gltf_material_to_ron(&material, &texture_ids)?;
        extensions.apply_to_material(mat_index, &mut authored, |image| {
            texture_slot(image, &texture_ids)
        });
        if closed
            .as_ref()
            .and_then(|report| report.materials.get(mat_index))
            .is_some_and(|m| *m == crate::closed_mesh::MaterialSidedness::DoubleSidedClosed)
        {
            tracing::debug!(material = %mat_name, "doubleSided dropped: only closed opaque meshes use it");
            authored.render_state.cull_mode = MaterialAssetRon::default_standard().render_state.cull_mode;
        }
        if let Some(orm_id) = cook_packed_orm(
            &material,
            disk_images,
            base_dir,
            parent_id,
            mat_index,
            platform,
            texture_quality,
            &mut sub_assets,
            &mut meta_entries,
        )? {
            authored.base.orm_map = Some(TextureSlot::Asset(orm_id));
        }
        let cooked = CookedStandardMaterial::new(authored);
        let payload = cooked
            .to_postcard()
            .map_err(|e| ImportError::Gltf(e.to_string()))?;
        let deps = texture_deps_from_material(&cooked.authored, &texture_ids);
        let key = path.strip_prefix("material/").unwrap_or(&path);
        material_binding.push(part_options.materials.get(key).copied().unwrap_or(id));
        sub_assets.push(GltfSubCook {
            path: path.clone(),
            id,
            type_path: "regia_asset::CookedStandardMaterial".to_string(),
            payload,
            dependencies: deps,
        });
        meta_entries.push(SubAssetEntry {
            path,
            id,
            type_path: "regia_asset::CookedStandardMaterial".to_string(),
        });
        material_names.push((mat_name, id));
    }

    // Which material each primitive draws with: mesh sub-assets do not name it, and a runtime
    // that only has the pak cannot read the glTF.
    {
        let model = regia_asset::CookedModel {
            version: regia_asset::COOKED_MODEL_VERSION,
            parts: parts
                .into_iter()
                .map(|(mesh, material, node)| regia_asset::CookedModelPart {
                    mesh,
                    material: material.and_then(|m| material_binding.get(m)).copied(),
                    node,
                })
                .collect(),
        };
        let path = "model".to_string();
        let id = crate::subasset_remap::sub_id(parent_id, &path);
        let payload = model
            .to_postcard()
            .map_err(|e| ImportError::Gltf(e.to_string()))?;
        let mut dependencies: Vec<AssetId> = model
            .parts
            .iter()
            .flat_map(|p| std::iter::once(p.mesh).chain(p.material))
            .collect();
        dependencies.sort();
        dependencies.dedup();
        sub_assets.push(GltfSubCook {
            path: path.clone(),
            id,
            type_path: "regia_asset::CookedModel".to_string(),
            payload,
            dependencies,
        });
        meta_entries.push(SubAssetEntry {
            path,
            id,
            type_path: "regia_asset::CookedModel".to_string(),
        });
    }

    let (lod_groups, lod_warnings) = crate::gltf_conventions::build_lod_groups_with(&lod_mesh_entries, lod);
    for warning in lod_warnings {
        tracing::warn!(target: "regia_asset_pipeline", "{warning}");
    }
    let mut material_variants = crate::gltf_conventions::build_material_variant_sets(&material_names);
    material_variants.extend(khr_material_variant_sets(
        &extensions,
        &walk.slots,
        &material_names,
    ));
    let sockets = crate::gltf_conventions::prefab_socket_set(sockets);

    // Persist convention payloads as cooked sub-assets.
    for (i, group) in lod_groups.iter().enumerate() {
        let path = format!("lod_group/{}", sanitize_name(&group.name));
        let id = crate::subasset_remap::sub_id(parent_id, &path);
        let payload = postcard::to_allocvec(group).map_err(|e| ImportError::Gltf(e.to_string()))?;
        sub_assets.push(GltfSubCook {
            path: path.clone(),
            id,
            type_path: "regia_asset::LodGroup".to_string(),
            payload,
            dependencies: group.levels.iter().map(|l| l.mesh).collect(),
        });
        meta_entries.push(SubAssetEntry {
            path,
            id,
            type_path: "regia_asset::LodGroup".to_string(),
        });
        let _ = i;
    }
    {
        let path = "prefab/sockets".to_string();
        let id = crate::subasset_remap::sub_id(parent_id, &path);
        let payload = postcard::to_allocvec(&sockets).map_err(|e| ImportError::Gltf(e.to_string()))?;
        sub_assets.push(GltfSubCook {
            path: path.clone(),
            id,
            type_path: "regia_asset::PrefabSocketSet".to_string(),
            payload,
            dependencies: vec![],
        });
        meta_entries.push(SubAssetEntry {
            path,
            id,
            type_path: "regia_asset::PrefabSocketSet".to_string(),
        });
    }
    for set in &material_variants {
        let path = unused_sub_path(
            &sub_assets,
            format!("material_variants/{}", sanitize_name(&set.slot)),
            |n| format!("material_variants/{}~{n}", sanitize_name(&set.slot)),
        );
        let id = crate::subasset_remap::sub_id(parent_id, &path);
        let payload = postcard::to_allocvec(set).map_err(|e| ImportError::Gltf(e.to_string()))?;
        sub_assets.push(GltfSubCook {
            path: path.clone(),
            id,
            type_path: "regia_asset::MaterialVariantSet".to_string(),
            payload,
            dependencies: set.variants.iter().map(|v| v.material).collect(),
        });
        meta_entries.push(SubAssetEntry {
            path,
            id,
            type_path: "regia_asset::MaterialVariantSet".to_string(),
        });
    }

    // Authored collision proxies (ADR-2341): only a file with proxies gains the sub-asset, so
    // every other file cooks as before.
    let (collision, collision_warnings) = crate::collision_proxies::cook_proxies(&walk.proxies);
    for warning in collision_warnings {
        tracing::warn!(target: "regia_asset_pipeline", "{warning}");
    }
    if let Some(set) = &collision {
        let path = regia_asset::COLLISION_SET_SUB_PATH.to_string();
        let id = crate::subasset_remap::sub_id(parent_id, &path);
        let payload = set.to_postcard().map_err(|e| ImportError::Gltf(e.to_string()))?;
        sub_assets.push(GltfSubCook {
            path: path.clone(),
            id,
            type_path: regia_asset::COLLISION_SET_TYPE_PATH.to_string(),
            payload,
            dependencies: vec![],
        });
        meta_entries.push(SubAssetEntry {
            path,
            id,
            type_path: regia_asset::COLLISION_SET_TYPE_PATH.to_string(),
        });
    }

    // `EXT_mesh_gpu_instancing` (ADR-3016): only a file with instances gains the sub-asset.
    let instances = prefab_instances(&walk.instanced);
    if let Some(set) = &instances {
        let path = regia_asset::PREFAB_INSTANCES_SUB_PATH.to_string();
        let id = crate::subasset_remap::sub_id(parent_id, &path);
        let payload = set.to_postcard().map_err(|e| ImportError::Gltf(e.to_string()))?;
        let mut dependencies: Vec<AssetId> = set.nodes.iter().flat_map(|n| n.parts.iter().copied()).collect();
        dependencies.sort();
        dependencies.dedup();
        sub_assets.push(GltfSubCook {
            path: path.clone(),
            id,
            type_path: regia_asset::PREFAB_INSTANCES_TYPE_PATH.to_string(),
            payload,
            dependencies,
        });
        meta_entries.push(SubAssetEntry {
            path,
            id,
            type_path: regia_asset::PREFAB_INSTANCES_TYPE_PATH.to_string(),
        });
    }

    // LOD0 or an untagged mesh stays the primary. A file of only `_LOD1`/`_LOD2`
    // (one LOD per file, or a clip GLB's bind mesh) uses the lowest index present.
    let primary_mesh = primary_mesh
        .or_else(|| lod_fallback.map(|(_, mesh)| mesh))
        .ok_or(ImportError::NoMesh)?;
    Ok(GltfImportResult {
        primary_mesh,
        sub_assets,
        meta_entries,
        lod_groups,
        sockets,
        material_variants,
        extensions,
        collision,
        material_ids: material_names.iter().map(|(_, id)| *id).collect(),
        instances,
    })
}

/// `KHR_materials_variants` as one set per mapped primitive. The slot is the primitive's mesh sub-path
/// without `mesh/`, so a consumer finds the part. Every variant name of the file is listed in every
/// set, with the primitive's default material where it has no mapping (the extension's rule for an
/// unmapped variant), so selecting one name changes the whole model. A primitive with no default
/// material leaves such a name out, and keeps what it draws.
fn khr_material_variant_sets(
    extensions: &GltfExtensionTable,
    slots: &[PrimitiveSlot],
    materials: &[(String, AssetId)],
) -> Vec<MaterialVariantSet> {
    let mut sets = Vec::new();
    for slot in slots {
        let Some(mappings) = extensions.primitive_variants(slot.mesh, slot.primitive) else {
            continue;
        };
        let mut variants = Vec::new();
        for (index, name) in extensions.variant_names.iter().enumerate() {
            let mapped = mappings.iter().find(|m| m.variants.contains(&index));
            let material = mapped.map(|m| m.material).or(slot.material);
            match material.and_then(|m| materials.get(m)) {
                Some((_, id)) => variants.push(regia_asset::MaterialVariant {
                    name: name.clone(),
                    material: *id,
                }),
                None if mapped.is_some() => tracing::warn!(
                    target: "regia_asset_pipeline",
                    slot = %slot.slot,
                    variant = %name,
                    "KHR_materials_variants: a mapping names a material the file does not have"
                ),
                None => {}
            }
        }
        if !variants.is_empty() {
            sets.push(MaterialVariantSet {
                slot: slot.slot.clone(),
                variants,
            });
        }
    }
    sets
}

/// The cooked instance set of the walk's instanced nodes. A cooked part already has its node's world
/// `W` in its vertices, so instance `I` is stored as `W * I * W⁻¹`, which is the identity for a node
/// drawn where it stands. A node whose `W` has no inverse (a scale of zero) cannot be expressed that
/// way; it keeps its single baked copy, with a warning.
fn prefab_instances(instanced: &[InstancedNode]) -> Option<PrefabInstanceSet> {
    let mut nodes = Vec::with_capacity(instanced.len());
    for node in instanced {
        let world = Mat4::from_cols_array(&node.world);
        if world.determinant().abs() < 1e-12 || !world.is_finite() {
            tracing::warn!(
                target: "regia_asset_pipeline",
                node = %node.name,
                "EXT_mesh_gpu_instancing: the node's transform has no inverse; it draws once"
            );
            continue;
        }
        let inverse = world.inverse();
        let transforms: Vec<[f32; 16]> = node
            .locals
            .iter()
            .map(|local| (world * Mat4::from_cols_array(local) * inverse).to_cols_array())
            .collect();
        if transforms.iter().flatten().any(|v| !v.is_finite()) {
            tracing::warn!(
                target: "regia_asset_pipeline",
                node = %node.name,
                "EXT_mesh_gpu_instancing: an instance transform overflows; the node draws once"
            );
            continue;
        }
        nodes.push(PrefabInstancedNode {
            node: node.name.clone(),
            parts: node.parts.clone(),
            transforms,
        });
    }
    (!nodes.is_empty()).then(|| PrefabInstanceSet::new(nodes))
}

/// Most RGBA8 bytes the decoded images of one glTF may hold together (ADR-5869). Each image is
/// widened to RGBA8 and every one is kept until the import ends, and many image entries may name
/// one small, highly compressible file: 32 references to a 500 KB grey PNG of 22000² pixels were
/// 64 GiB. Real assets (fifty 4K maps are 3.2 GiB) stay below it.
pub const MAX_GLTF_IMAGE_BYTES: u64 = 4 << 30;

/// Longest image edge a glTF may carry, as for DDS sources.
pub const MAX_GLTF_IMAGE_EDGE: u32 = 1 << 15;

/// Reads only the header of every image the document names (a buffer view, a `data:` URI, or a
/// file beside `base`) and refuses an edge over [`MAX_GLTF_IMAGE_EDGE`] or a sum of RGBA8 bytes
/// over [`MAX_GLTF_IMAGE_BYTES`] before anything is decoded. An image whose header does not read
/// is left to the decode, which reports it.
pub(crate) fn check_image_budget<'a>(
    document: &gltf::Document,
    buffer: impl Fn(usize) -> Option<&'a [u8]>,
    base: Option<&Path>,
) -> Result<(), ImportError> {
    fn dims_of(reader: image::ImageReader<impl std::io::BufRead + std::io::Seek>) -> Option<(u32, u32)> {
        reader.with_guessed_format().ok()?.into_dimensions().ok()
    }
    let mut total = 0_u64;
    for image in document.images() {
        let dims = match image.source() {
            gltf::image::Source::View { view, .. } => buffer(view.buffer().index())
                .and_then(|b| b.get(view.offset()..view.offset().checked_add(view.length())?))
                .and_then(|slice| dims_of(image::ImageReader::new(std::io::Cursor::new(slice)))),
            gltf::image::Source::Uri { uri, .. } if uri.starts_with("data:") => data_uri_bytes(uri)
                .ok()
                .and_then(|bytes| dims_of(image::ImageReader::new(std::io::Cursor::new(bytes)))),
            gltf::image::Source::Uri { uri, .. } => base
                .zip(decode_gltf_uri(uri))
                .and_then(|(base, rel)| crate::cook::join_portable(base, &rel).ok())
                .and_then(|path| std::fs::File::open(path).ok())
                .and_then(|file| dims_of(image::ImageReader::new(std::io::BufReader::new(file)))),
        };
        let Some((w, h)) = dims else {
            continue;
        };
        if w > MAX_GLTF_IMAGE_EDGE || h > MAX_GLTF_IMAGE_EDGE {
            return Err(ImportError::Image(format!(
                "image {} is {w}×{h}, over the {MAX_GLTF_IMAGE_EDGE}-texel edge limit",
                image.index()
            )));
        }
        total = total.saturating_add(u64::from(w) * u64::from(h) * 4);
        if total > MAX_GLTF_IMAGE_BYTES {
            return Err(ImportError::Image(format!(
                "the images decode to more than {MAX_GLTF_IMAGE_BYTES} bytes of RGBA8 (at image {})",
                image.index()
            )));
        }
    }
    Ok(())
}

fn decode_images(
    document: &gltf::Document,
    buffers: &[Vec<u8>],
) -> Result<Vec<gltf::image::Data>, ImportError> {
    check_image_budget(document, |i| buffers.get(i).map(Vec::as_slice), None)?;
    let mut out = Vec::new();
    for image in document.images() {
        let data = match image.source() {
            gltf::image::Source::View { view, mime_type: _ } => {
                let buffer = buffers
                    .get(view.buffer().index())
                    .ok_or_else(|| ImportError::Gltf("image buffer index out of range".into()))?;
                let start = view.offset();
                let slice = start
                    .checked_add(view.length())
                    .and_then(|end| buffer.get(start..end))
                    .ok_or_else(|| ImportError::Gltf("image view out of range".into()))?;
                image::load_from_memory(slice).map_err(|e| ImportError::Image(e.to_string()))?
            }
            gltf::image::Source::Uri { uri, mime_type: _ } => {
                let bytes = data_uri_bytes(uri)?;
                image::load_from_memory(&bytes).map_err(|e| ImportError::Image(e.to_string()))?
            }
        };
        let rgba = data.to_rgba8();
        let (w, h) = rgba.dimensions();
        out.push(gltf::image::Data {
            pixels: rgba.into_raw(),
            format: gltf::image::Format::R8G8B8A8,
            width: w,
            height: h,
        });
    }
    Ok(out)
}

/// The bytes of a base64 `data:` URI (`data:[<mime>];base64,<payload>`, RFC 2397). An in-memory
/// import has no directory to resolve any other URI against.
///
/// # Errors
/// The URI is external, lacks a comma or `;base64`, or has a malformed base64 payload.
pub fn data_uri_bytes(uri: &str) -> Result<Vec<u8>, ImportError> {
    use base64::Engine;
    let Some(rest) = uri.strip_prefix("data:") else {
        return Err(ImportError::Image(format!(
            "external image {uri} requires source directory"
        )));
    };
    let (header, payload) = rest
        .split_once(',')
        .ok_or_else(|| ImportError::Image("data URI image without a ',' separator".into()))?;
    if !header.ends_with(";base64") {
        return Err(ImportError::Image("data URI image is not base64-encoded".into()));
    }
    base64::engine::general_purpose::STANDARD
        .decode(payload.trim())
        .map_err(|e| ImportError::Image(format!("data URI image: {e}")))
}

fn texture_deps_from_material(
    mat: &MaterialAssetRon,
    _texture_ids: &HashMap<usize, AssetId>,
) -> Vec<AssetId> {
    let mut deps = Vec::new();
    let mut push_slot = |slot: &Option<TextureSlot>| {
        if let Some(TextureSlot::Asset(id)) = slot {
            deps.push(*id);
        }
    };
    push_slot(&mat.base.base_color_map);
    push_slot(&mat.base.normal_map);
    push_slot(&mat.base.orm_map);
    push_slot(&mat.base.emissive_map);
    push_slot(&mat.clearcoat.as_ref().and_then(|c| c.normal_map.clone()));
    if let Some(detail) = mat.hard_surface.as_ref().and_then(|h| h.detail.as_ref()) {
        push_slot(&detail.albedo);
        push_slot(&detail.normal);
        push_slot(&detail.roughness);
        push_slot(&detail.mask);
    }
    push_slot(&mat.thin_film.as_ref().and_then(|f| f.factor_map.clone()));
    push_slot(&mat.thin_film.as_ref().and_then(|f| f.thickness_map.clone()));
    deps.sort();
    deps.dedup();
    deps
}

fn gltf_material_to_ron(
    material: &gltf::Material,
    texture_ids: &HashMap<usize, AssetId>,
) -> Result<MaterialAssetRon, ImportError> {
    let pbr = material.pbr_metallic_roughness();
    let base_color = pbr.base_color_factor();
    let mut authored = MaterialAssetRon::default_standard();
    authored.name = material.name().map(|s| s.to_string());
    authored.base.base_color = base_color;
    authored.base.metallic = pbr.metallic_factor();
    authored.base.roughness = pbr.roughness_factor();
    if let Some(tex) = pbr.base_color_texture() {
        authored.base.base_color_map = texture_slot(tex.texture().source().index(), texture_ids);
    }
    if let Some(tex) = material.normal_texture() {
        authored.base.normal_map = texture_slot(tex.texture().source().index(), texture_ids);
    }
    if let Some(tex) = pbr.metallic_roughness_texture() {
        authored.base.orm_map = texture_slot(tex.texture().source().index(), texture_ids);
    }
    // The cook packs occlusion (R) with the metallic-roughness channels (G, B); a material with
    // only an occlusion map gets G and B at 255 (`cook_packed_orm`), so the scale is a no-op.
    authored.base.orm_packed = true;
    // Emission is the factor times `KHR_materials_emissive_strength`, whether or not a texture
    // modulates it: a lamp authored as a bare emissive factor (DO27's `M_Light`, `M_Glow`) used
    // to import dark.
    authored.base.emissive = material.emissive_factor();
    authored.base.emissive_strength = material
        .emissive_strength()
        .filter(|s| s.is_finite())
        .unwrap_or(1.0)
        .max(0.0);
    if let Some(tex) = material.emissive_texture() {
        authored.base.emissive_map = texture_slot(tex.texture().source().index(), texture_ids);
    }
    authored.base.ior = material
        .ior()
        .filter(|v| v.is_finite() && *v >= 1.0)
        .unwrap_or(1.5);
    authored.render_state.alpha_mode = match material.alpha_mode() {
        gltf::material::AlphaMode::Opaque => AlphaMode::Opaque,
        gltf::material::AlphaMode::Mask => AlphaMode::Mask,
        gltf::material::AlphaMode::Blend => AlphaMode::Blend,
    };
    authored.render_state.alpha_cutoff = material.alpha_cutoff().unwrap_or(0.5);
    if material.double_sided() {
        authored.render_state.cull_mode = Some(CullMode::None);
    }
    if let Some(t) = material.transmission() {
        if t.transmission_texture().is_some() {
            // The cooked material has one transmission factor: no map (docs/formats/gltf-extensions.md).
            tracing::warn!(
                material = material.name().unwrap_or_default(),
                "glTF transmissionTexture is not read: the material transmits by transmissionFactor alone"
            );
        }
        let mut params = TransmissionParams {
            factor: t.transmission_factor().clamp(0.0, 1.0),
            ..TransmissionParams::default()
        };
        if let Some(v) = material.volume() {
            params.thickness = v.thickness_factor().max(0.0);
            params.attenuation_color = v.attenuation_color();
            let d = v.attenuation_distance();
            // glTF's default is +inf (no absorption); the format stores that as 0.
            params.attenuation_distance = if d.is_finite() && d > 0.0 { d } else { 0.0 };
        }
        authored.transmission = Some(params);
    }
    Ok(authored)
}

/// The materials of a glTF file exactly as the cook writes them (before its packed-ORM
/// sub-asset replaces `orm_map`), in glTF material order. Texture slots name the source
/// image (`image/<index>`). For tests and tools that compare the cooked material with the
/// source document without cooking textures.
///
/// # Errors
///
/// The file is not a readable glTF.
pub fn gltf_materials(source: &Path) -> Result<Vec<MaterialAssetRon>, ImportError> {
    let GltfOpened { document, .. } = parse_gltf_path(source)?;
    let extensions = GltfExtensionTable::from_bytes(
        &std::fs::read(source).unwrap_or_default(),
        &mut GltfExtensionRegistry::new(),
    );
    let ids: HashMap<usize, AssetId> = HashMap::new();
    let mut out = Vec::new();
    for (index, material) in document.materials().enumerate() {
        let mut authored = gltf_material_to_ron(&material, &ids)?;
        extensions.apply_to_material(index, &mut authored, |i| {
            Some(TextureSlot::Path(format!("image/{i}")))
        });
        let pbr = material.pbr_metallic_roughness();
        let slot = |index: Option<usize>| index.map(|i| TextureSlot::Path(format!("image/{i}")));
        authored.base.base_color_map = slot(pbr.base_color_texture().map(|t| t.texture().source().index()));
        authored.base.normal_map = slot(material.normal_texture().map(|t| t.texture().source().index()));
        authored.base.orm_map = slot(
            pbr.metallic_roughness_texture()
                .map(|t| t.texture().source().index())
                .or_else(|| material.occlusion_texture().map(|t| t.texture().source().index())),
        );
        authored.base.emissive_map = slot(material.emissive_texture().map(|t| t.texture().source().index()));
        out.push(authored);
    }
    Ok(out)
}

fn texture_slot(image_index: usize, map: &HashMap<usize, AssetId>) -> Option<TextureSlot> {
    map.get(&image_index).map(|id| TextureSlot::Asset(*id))
}

/// Packs separate occlusion and metallic–roughness images into one ORM sub-asset.
///
/// Channel layout is R occlusion, G roughness, B metallic, A 255
/// (`docs/formats/gltf-conventions.md`). The material's `orm_map` is pointed at
/// this asset by the caller. Returns `None` when the material has neither map.
#[allow(clippy::too_many_arguments)]
fn cook_packed_orm(
    material: &gltf::Material,
    disk_images: &[gltf::image::Data],
    base_dir: Option<&Path>,
    parent_id: AssetId,
    mat_index: usize,
    platform: &str,
    texture_quality: TextureCookQuality,
    sub_assets: &mut Vec<GltfSubCook>,
    meta_entries: &mut Vec<SubAssetEntry>,
) -> Result<Option<AssetId>, ImportError> {
    let pbr = material.pbr_metallic_roughness();
    let mr_image = pbr.metallic_roughness_texture().map(|tex| tex.texture().source());
    let ao_image = material.occlusion_texture().map(|tex| tex.texture().source());
    if mr_image.is_none() && ao_image.is_none() {
        return Ok(None);
    }
    // The packed map is built from the source pixels, so the image's `.meta` channel order
    // (ADR-3320) must apply here too: the per-image `texture/<n>` swizzle never reaches the
    // material once its `orm_map` points at this packed asset.
    let load = |image: &gltf::Image<'_>| -> Result<Option<(u32, u32, Vec<u8>)>, ImportError> {
        let Some((w, h, mut pixels)) = disk_images
            .get(image.index())
            .map(image_data_to_rgba8)
            .transpose()?
        else {
            return Ok(None);
        };
        let settings = gltf_image_settings(image, base_dir);
        crate::texture_settings::apply_channel_transforms(
            &mut pixels,
            TextureCookUsage::ColorLinear,
            settings.normal_green_or_default(),
            settings.orm_channel_order_or_default(),
        );
        Ok(Some((w, h, pixels)))
    };
    let mr = match &mr_image {
        Some(image) => load(image)?,
        None => None,
    };
    let ao = match &ao_image {
        Some(image) => load(image)?,
        None => None,
    };
    let (width, height) = if let Some((w, h, _)) = mr.as_ref() {
        (*w, *h)
    } else if let Some((w, h, _)) = ao.as_ref() {
        (*w, *h)
    } else {
        return Ok(None);
    };
    let mr_bytes = mr
        .as_ref()
        .filter(|(w, h, _)| *w == width && *h == height)
        .map(|(_, _, pixels)| pixels.as_slice());
    let ao_bytes = ao
        .as_ref()
        .filter(|(w, h, _)| *w == width && *h == height)
        .map(|(_, _, pixels)| pixels.as_slice());
    let packed = crate::gltf_conventions::pack_orm_rgba8(width, height, ao_bytes, mr_bytes);
    // Normal-variance roughness (Toksvig): the roughness mips under a normal map carry the
    // spread of the normals each texel averages. The cooked normal map is two-channel, so the
    // runtime cannot measure it from the sample's length (`crate::normal_variance`).
    let normal = match (mr_bytes.is_some(), material.normal_texture()) {
        (true, Some(info)) => disk_images
            .get(info.texture().source().index())
            .map(image_data_to_rgba8)
            .transpose()?
            .map(|image| (info.scale(), image)),
        _ => None,
    };
    let roughness_factor = pbr.roughness_factor();
    let path = format!("texture/orm/{mat_index}");
    let id = crate::subasset_remap::sub_id(parent_id, &path);
    let (type_path, payload) = match crate::texture_compress::cook_texture_rgba8_with(
        width,
        height,
        &packed,
        TextureCookUsage::ColorLinear,
        platform,
        texture_quality,
        |chain| {
            if let Some((scale, (nw, nh, pixels))) = &normal {
                crate::normal_variance::apply_normal_variance_roughness(
                    chain,
                    (*nw, *nh, pixels),
                    *scale,
                    roughness_factor,
                );
            }
        },
    ) {
        Ok(texture) => {
            let payload = texture
                .to_postcard()
                .map_err(|e| ImportError::Image(e.to_string()))?;
            ("regia_asset::CookedTexture".to_string(), payload)
        }
        Err(TextureCookError::TooSmall { .. }) => {
            let img = regia_asset::CookedImage {
                width,
                height,
                rgba8: packed,
            };
            img.validate().map_err(|e| ImportError::Image(e.to_string()))?;
            let payload = img.to_postcard().map_err(|e| ImportError::Image(e.to_string()))?;
            ("regia_asset::CookedImage".to_string(), payload)
        }
        Err(e) => return Err(ImportError::Image(e.to_string())),
    };
    sub_assets.push(GltfSubCook {
        path: path.clone(),
        id,
        type_path: type_path.clone(),
        payload,
        dependencies: vec![],
    });
    meta_entries.push(SubAssetEntry { path, id, type_path });
    Ok(Some(id))
}

fn load_image_rgba8(
    image: gltf::Image,
    disk_images: &[gltf::image::Data],
    base_dir: Option<&Path>,
) -> Result<(u32, u32, Vec<u8>), ImportError> {
    if let Some(data) = disk_images.get(image.index()) {
        return image_data_to_rgba8(data);
    }
    if let gltf::image::Source::Uri { uri, .. } = image.source() {
        if uri.starts_with("data:") {
            return Err(ImportError::Image(
                "data URI images are not supported in M4".into(),
            ));
        }
        let path = base_dir
            .map(|d| d.join(decode_gltf_uri(uri).unwrap_or_else(|| uri.to_string())))
            .ok_or_else(|| ImportError::Image(format!("external image {uri} requires source directory")))?;
        let bytes = std::fs::read(&path).map_err(|e| ImportError::Image(e.to_string()))?;
        let dyn_img = image::load_from_memory(&bytes).map_err(|e| ImportError::Image(e.to_string()))?;
        let rgba = dyn_img.to_rgba8();
        let (w, h) = rgba.dimensions();
        return Ok((w, h, rgba.into_raw()));
    }
    Err(ImportError::Image("glTF image has no pixel data".to_string()))
}

/// An image `gltf::import` decoded from a `.gltf`'s external file, as RGBA8.
///
/// `gltf::import` keeps the decoded layout: a grey PNG is `R8`, grey with alpha `R8G8`, a 16-bit
/// PNG `R16…`, an HDR/EXR file 32-bit float. Each widens the way the `image` crate's `to_rgba8`
/// does for the same file inside a `.glb` (grey to `(l, l, l)`, 16-bit to the nearest 8-bit
/// value, float clamped to 0..1), so a `.gltf` and a `.glb` of one model cook the same texture.
/// Before this every layout but `R8G8B8`/`R8G8B8A8` failed the whole import (a grey roughness
/// or mask PNG is common). A pixel buffer of the wrong length is an error, not a panic.
fn image_data_to_rgba8(data: &gltf::image::Data) -> Result<(u32, u32, Vec<u8>), ImportError> {
    use gltf::image::Format;
    let (w, h) = (data.width, data.height);
    let texels = (w as usize)
        .checked_mul(h as usize)
        .ok_or_else(|| ImportError::Image(format!("glTF image {w}x{h} is too large")))?;
    let (channels, bytes_per_channel) = match data.format {
        Format::R8 => (1, 1),
        Format::R8G8 => (2, 1),
        Format::R8G8B8 => (3, 1),
        Format::R8G8B8A8 => (4, 1),
        Format::R16 => (1, 2),
        Format::R16G16 => (2, 2),
        Format::R16G16B16 => (3, 2),
        Format::R16G16B16A16 => (4, 2),
        // The 32-bit float layouts (HDR/EXR): RGB or RGBA by their size.
        other => match data.pixels.len().checked_div(texels) {
            Some(12) => (3, 4),
            Some(16) => (4, 4),
            _ => {
                return Err(ImportError::Image(format!(
                    "unsupported glTF image format {other:?}"
                )));
            }
        },
    };
    let texel_bytes = channels * bytes_per_channel;
    if texels.checked_mul(texel_bytes) != Some(data.pixels.len()) {
        return Err(ImportError::Image(format!(
            "glTF image {w}x{h} {:?} holds {} bytes, expected {texel_bytes} per texel",
            data.format,
            data.pixels.len()
        )));
    }
    let channel = |bytes: &[u8]| -> u8 {
        match bytes {
            [v] => *v,
            [a, b] => ((u32::from(u16::from_ne_bytes([*a, *b])) + 128) / 257) as u8,
            [a, b, c, d] => (f32::from_ne_bytes([*a, *b, *c, *d]).clamp(0.0, 1.0) * 255.0).round() as u8,
            _ => 0,
        }
    };
    let mut rgba = Vec::with_capacity(texels.saturating_mul(4));
    for texel in data.pixels.chunks_exact(texel_bytes) {
        let mut values = texel.chunks_exact(bytes_per_channel).map(channel);
        let out = match channels {
            1 => {
                let l = values.next().unwrap_or(0);
                [l, l, l, 255]
            }
            2 => {
                let l = values.next().unwrap_or(0);
                [l, l, l, values.next().unwrap_or(255)]
            }
            _ => [
                values.next().unwrap_or(0),
                values.next().unwrap_or(0),
                values.next().unwrap_or(0),
                values.next().unwrap_or(255),
            ],
        };
        rgba.extend_from_slice(&out);
    }
    Ok((w, h, rgba))
}

fn read_primitive(
    primitive: &gltf::Primitive,
    buffers: &[Vec<u8>],
    world: &[f32; 16],
    skin: Option<&SkinJointLayout>,
    target_names: &[String],
) -> Result<CpuMesh, ImportError> {
    let reader = primitive.reader(|buffer| {
        let idx = buffer.index();
        buffers.get(idx).map(|b| b.as_slice())
    });
    let positions_raw: Vec<[f32; 3]> = reader
        .read_positions()
        .ok_or(ImportError::MissingPositions)?
        .map(|p| [p[0], p[1], p[2]])
        .collect();
    let normals_raw: Vec<[f32; 3]> = if let Some(iter) = reader.read_normals() {
        iter.map(|n| [n[0], n[1], n[2]]).collect()
    } else {
        // A non-indexed primitive is a triangle list over the vertices in order.
        let idx: Vec<u32> = match reader.read_indices() {
            Some(ri) => ri.into_u32().collect(),
            None => (0..positions_raw.len() as u32).collect(),
        };
        generate_normals(&positions_raw, Some(idx.into_iter()))
    };
    let uvs_raw: Vec<[f32; 2]> = if let Some(uv) = reader.read_tex_coords(0) {
        uv.into_f32().map(|t| [t[0], t[1]]).collect()
    } else {
        vec![[0.0, 0.0]; positions_raw.len()]
    };
    // Every attribute of a primitive has one entry per vertex (glTF 2.0 § 3.7.2.1): a short
    // `NORMAL` or `TEXCOORD_0` is an error naming it, not a mesh cut to the shorter list.
    for (attribute, count) in [("NORMAL", normals_raw.len()), ("TEXCOORD_0", uvs_raw.len())] {
        if count != positions_raw.len() {
            return Err(ImportError::Gltf(format!(
                "{attribute} has {count} entries for {} vertices",
                positions_raw.len()
            )));
        }
    }
    let mut indices: Vec<u32> = if let Some(idx) = reader.read_indices() {
        idx.into_u32().collect()
    } else {
        (0..positions_raw.len() as u32).collect()
    };
    // Coincident duplicate triangles (the same corners and winding twice, often with other
    // UVs) z-fight into a moiré; the first copy stays (importer 1.6.0).
    let dropped = regia_render::drop_coincident_triangles(&positions_raw, &mut indices);
    if dropped > 0 {
        tracing::debug!(dropped, "glTF primitive: coincident duplicate triangles removed");
    }

    let normal_xf = normal_matrix(world);
    let mut positions = Vec::with_capacity(positions_raw.len());
    let mut normals = Vec::with_capacity(normals_raw.len());
    for (p, n) in positions_raw.iter().zip(normals_raw.iter()) {
        let tp = transform_point(world, p);
        let tn = transform_normal(&normal_xf, n);
        positions.push(tp);
        normals.push(tn);
    }

    let vertex_count = positions.len();
    let skin = match skin {
        Some(layout) => read_skin(&reader, layout, vertex_count)?,
        None => None,
    };
    let colors: Vec<[f32; 4]> = match reader.read_colors(0) {
        Some(colors) => colors.into_rgba_f32().collect(),
        None => Vec::new(),
    };
    if !colors.is_empty() && colors.len() != vertex_count {
        return Err(ImportError::Gltf(format!(
            "COLOR_0 has {} entries for {vertex_count} vertices",
            colors.len()
        )));
    }
    let frame = MorphFrame {
        world,
        normal_matrix: &normal_xf,
        base_normals: &normals_raw,
    };
    let morph_targets = read_morph_targets(&reader, &frame, vertex_count, target_names)?;

    let mut mesh = CpuMesh {
        positions,
        normals,
        uvs: uvs_raw,
        tangents: Vec::new(),
        indices,
        skin,
        colors,
        morph_targets,
    };
    mesh.tangents = generate_mikkt_tangents(&mesh)?;
    mesh.validate()?;
    Ok(mesh)
}

/// Reads `JOINTS_0`/`WEIGHTS_0` (and `JOINTS_1`/`WEIGHTS_1` when present) into a [`CpuSkin`].
///
/// Joint slots become skeleton joint ids through `layout`; influence slots keep their order.
/// Weights of both sets are normalised together ([`regia_asset::quantize_skin_weights`]);
/// integer (`unorm8`/`unorm16`) weights that already sum to one are kept bit for bit
/// ([`regia_asset::normalize_unorm16_weights`]). A primitive without `JOINTS_0` is rigid.
/// Sets past the second are dropped with a warning (the cooked format holds eight influences).
fn read_skin<'a, 's, F>(
    reader: &gltf::mesh::Reader<'a, 's, F>,
    layout: &SkinJointLayout,
    vertex_count: usize,
) -> Result<Option<CpuSkin>, ImportError>
where
    F: Clone + Fn(gltf::Buffer<'a>) -> Option<&'s [u8]>,
{
    let mut sets: Vec<(Vec<[u16; 4]>, SkinWeights)> = Vec::new();
    for set in 0..2u32 {
        let (Some(joints), Some(weights)) = (reader.read_joints(set), reader.read_weights(set)) else {
            break;
        };
        let joints: Vec<[u16; 4]> = joints.into_u16().collect();
        let weights = match weights {
            gltf::mesh::util::ReadWeights::U8(w) => {
                SkinWeights::Unorm(w.map(|v| v.map(|x| u16::from(x) * 257)).collect())
            }
            gltf::mesh::util::ReadWeights::U16(w) => SkinWeights::Unorm(w.collect()),
            gltf::mesh::util::ReadWeights::F32(w) => SkinWeights::Float(w.collect()),
        };
        if joints.len() != vertex_count || weights.len() != vertex_count {
            return Err(ImportError::Gltf(format!(
                "JOINTS_{set}/WEIGHTS_{set} have {}/{} entries for {vertex_count} vertices",
                joints.len(),
                weights.len()
            )));
        }
        sets.push((joints, weights));
    }
    if sets.is_empty() {
        return Ok(None);
    }
    if reader.read_joints(2).is_some() {
        tracing::warn!(
            target: "regia_asset_pipeline",
            "glTF primitive has more than 8 joint influences per vertex; sets past JOINTS_1 are dropped"
        );
    }
    let all_unorm = sets.iter().all(|(_, w)| matches!(w, SkinWeights::Unorm(_)));
    let remap = |slot: u16, weight_is_zero: bool| -> Result<u16, ImportError> {
        match layout.slot_to_joint.get(usize::from(slot)) {
            Some(&joint) => Ok(joint as u16),
            None if weight_is_zero => Ok(0),
            None => Err(ImportError::Gltf(format!(
                "joint index {slot} is out of range for a skin of {} joints",
                layout.slot_to_joint.len()
            ))),
        }
    };
    let mut skin = CpuSkin {
        joint_names: layout.names.clone(),
        joints: Vec::with_capacity(vertex_count),
        weights: Vec::with_capacity(vertex_count),
        ..CpuSkin::default()
    };
    let eight = sets.len() == 2;
    for v in 0..vertex_count {
        let mut weights = [0u16; 8];
        if all_unorm {
            let mut raw = [0u16; 8];
            for (s, (_, w)) in sets.iter().enumerate() {
                raw[s * 4..s * 4 + 4].copy_from_slice(&w.unorm(v));
            }
            if eight {
                weights = regia_asset::normalize_unorm16_weights(raw);
            } else {
                let four: [u16; 4] = [raw[0], raw[1], raw[2], raw[3]];
                weights[..4].copy_from_slice(&regia_asset::normalize_unorm16_weights(four));
            }
        } else {
            let mut raw = [0f32; 8];
            for (s, (_, w)) in sets.iter().enumerate() {
                raw[s * 4..s * 4 + 4].copy_from_slice(&w.float(v));
            }
            if eight {
                weights = regia_asset::quantize_skin_weights(raw);
            } else {
                let four: [f32; 4] = [raw[0], raw[1], raw[2], raw[3]];
                weights[..4].copy_from_slice(&regia_asset::quantize_skin_weights(four));
            }
        }
        for (s, (joints, _)) in sets.iter().enumerate() {
            let mut out = [0u16; 4];
            for slot in 0..4 {
                out[slot] = remap(joints[v][slot], weights[s * 4 + slot] == 0)?;
            }
            let w: [u16; 4] = [
                weights[s * 4],
                weights[s * 4 + 1],
                weights[s * 4 + 2],
                weights[s * 4 + 3],
            ];
            if s == 0 {
                skin.joints.push(out);
                skin.weights.push(w);
            } else {
                skin.joints1.push(out);
                skin.weights1.push(w);
            }
        }
    }
    Ok(Some(skin))
}

/// One `WEIGHTS_n` set as read from the file.
enum SkinWeights {
    /// `unorm8` (widened ×257) or `unorm16`.
    Unorm(Vec<[u16; 4]>),
    /// `float`.
    Float(Vec<[f32; 4]>),
}

impl SkinWeights {
    fn len(&self) -> usize {
        match self {
            Self::Unorm(w) => w.len(),
            Self::Float(w) => w.len(),
        }
    }

    fn unorm(&self, vertex: usize) -> [u16; 4] {
        match self {
            Self::Unorm(w) => w[vertex],
            Self::Float(w) => w[vertex].map(|x| (x.clamp(0.0, 1.0) * 65535.0).round() as u16),
        }
    }

    fn float(&self, vertex: usize) -> [f32; 4] {
        match self {
            Self::Unorm(w) => w[vertex].map(|x| f32::from(x) / 65535.0),
            Self::Float(w) => w[vertex],
        }
    }
}

/// How a primitive's morph deltas are turned into the cooked frame (the same transforms as its
/// base attributes).
struct MorphFrame<'a> {
    /// The matrix baked into the base positions.
    world: &'a [f32; 16],
    /// [`normal_matrix`] of `world`, used for the base normals.
    normal_matrix: &'a [f32; 9],
    /// The primitive's source normals (before the bake), one per vertex.
    base_normals: &'a [[f32; 3]],
}

/// Reads the primitive's morph targets as sparse targets, and keeps a vertex when any of its
/// deltas is non-zero once rounded to `f16` (glTF 2.0 § 3.7.2.2). Deltas follow their base
/// attribute (ADR-226):
/// - position deltas are turned by the bake's linear part, as the base positions are;
/// - normal deltas by the base normals' [`normal_matrix`], divided by the length the base normal
///   had before it was renormalised, so `normal + delta` keeps the direction the bake gives it;
/// - tangent deltas (surface directions) by the linear part, keeping their length (exact for
///   rotation and uniform scale; the cooked base tangents are regenerated unit vectors).
fn read_morph_targets<'a, 's, F>(
    reader: &gltf::mesh::Reader<'a, 's, F>,
    frame: &MorphFrame<'_>,
    vertex_count: usize,
    names: &[String],
) -> Result<Vec<CpuMorphTarget>, ImportError>
where
    F: Clone + Fn(gltf::Buffer<'a>) -> Option<&'s [u8]>,
{
    let world = frame.world;
    let linear = |d: [f32; 3]| -> [f32; 3] {
        [
            world[0] * d[0] + world[4] * d[1] + world[8] * d[2],
            world[1] * d[0] + world[5] * d[1] + world[9] * d[2],
            world[2] * d[0] + world[6] * d[1] + world[10] * d[2],
        ]
    };
    let length = |v: [f32; 3]| (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    let normal_delta = |v: usize, d: [f32; 3]| -> [f32; 3] {
        let turned = mul_mat3(frame.normal_matrix, d);
        let base = frame
            .base_normals
            .get(v)
            .map(|n| length(mul_mat3(frame.normal_matrix, *n)))
            .filter(|l| *l > 1e-12)
            .unwrap_or(1.0);
        turned.map(|c| c / base)
    };
    let tangent_delta = |d: [f32; 3]| -> [f32; 3] {
        let turned = linear(d);
        let (from, to) = (length(d), length(turned));
        if to > 1e-20 {
            turned.map(|c| c * (from / to))
        } else {
            [0.0; 3]
        }
    };
    let mut targets = Vec::new();
    for (index, (positions, normals, tangents)) in reader.read_morph_targets().enumerate() {
        let dense = |deltas: Option<Vec<[f32; 3]>>,
                     what: &str,
                     turn: &dyn Fn(usize, [f32; 3]) -> [f32; 3]|
         -> Result<Vec<[f32; 3]>, ImportError> {
            match deltas {
                None => Ok(Vec::new()),
                Some(d) if d.len() == vertex_count => {
                    Ok(d.into_iter().enumerate().map(|(v, d)| turn(v, d)).collect())
                }
                Some(d) => Err(ImportError::Gltf(format!(
                    "morph target {index} has {} {what} deltas for {vertex_count} vertices",
                    d.len()
                ))),
            }
        };
        let positions = dense(positions.map(|p| p.collect()), "position", &|_, d| linear(d))?;
        let normals = dense(normals.map(|n| n.collect()), "normal", &normal_delta)?;
        let tangents = dense(tangents.map(|t| t.collect()), "tangent", &|_, d| tangent_delta(d))?;
        let moves = |d: &[[f32; 3]], v: usize| {
            d.get(v)
                .is_some_and(|d| d.iter().any(|c| regia_asset::f32_to_f16_round(*c) & 0x7fff != 0))
        };
        let mut target = CpuMorphTarget {
            name: names
                .get(index)
                .filter(|n| !n.is_empty())
                .cloned()
                .unwrap_or_else(|| format!("target_{index}")),
            ..CpuMorphTarget::default()
        };
        for v in 0..vertex_count {
            if !(moves(&positions, v) || moves(&normals, v) || moves(&tangents, v)) {
                continue;
            }
            target.vertex_indices.push(v as u32);
            target
                .position_deltas
                .push(positions.get(v).copied().unwrap_or([0.0; 3]));
            if !normals.is_empty() {
                target.normal_deltas.push(normals[v]);
            }
            if !tangents.is_empty() {
                target.tangent_deltas.push(tangents[v]);
            }
        }
        targets.push(target);
    }
    Ok(targets)
}

struct MikktMesh<'a> {
    mesh: &'a CpuMesh,
    tangents: Vec<[f32; 4]>,
}

impl Geometry for MikktMesh<'_> {
    fn num_faces(&self) -> usize {
        self.mesh.indices.len() / 3
    }

    fn num_vertices_of_face(&self, _face: usize) -> usize {
        3
    }

    fn position(&self, face: usize, vert: usize) -> [f32; 3] {
        let i = self.mesh.indices[face * 3 + vert] as usize;
        self.mesh.positions[i]
    }

    fn normal(&self, face: usize, vert: usize) -> [f32; 3] {
        let i = self.mesh.indices[face * 3 + vert] as usize;
        self.mesh.normals[i]
    }

    fn tex_coord(&self, face: usize, vert: usize) -> [f32; 2] {
        let i = self.mesh.indices[face * 3 + vert] as usize;
        // V flipped: glTF normal textures put +Y up the image while glTF's V runs down it
        // (glTF 2.0 § 3.9.3); MikkTSpace's bitangent follows +V (see regia_render::tangents).
        let [u, v] = self.mesh.uvs[i];
        [u, 1.0 - v]
    }

    fn set_tangent(&mut self, tangent_space: Option<TangentSpace>, face: usize, vert: usize) {
        let i = self.mesh.indices[face * 3 + vert] as usize;
        if let Some(ts) = tangent_space {
            let w = if ts.is_orientation_preserving() { 1.0 } else { -1.0 };
            self.tangents[i] = [ts.tangent()[0], ts.tangent()[1], ts.tangent()[2], w];
        }
    }
}

pub(crate) fn generate_mikkt_tangents(mesh: &CpuMesh) -> Result<Vec<[f32; 4]>, ImportError> {
    // A hostile file can carry an index past the vertex count or attributes of different lengths;
    // the tangent generator indexes all of them, so refuse before it runs.
    let vertices = mesh.positions.len();
    if mesh.normals.len() != vertices || mesh.uvs.len() != vertices {
        return Err(ImportError::MeshValidation(
            regia_asset::MeshValidationError::MismatchedAttributeLengths {
                pos: vertices,
                nrm: mesh.normals.len(),
                uv: mesh.uvs.len(),
                tan: 0,
            },
        ));
    }
    if let Some((offset, &index)) = mesh
        .indices
        .iter()
        .enumerate()
        .find(|&(_, &i)| i as usize >= vertices)
    {
        return Err(ImportError::MeshValidation(
            regia_asset::MeshValidationError::IndexOutOfRange {
                index,
                offset,
                vertex_count: vertices,
            },
        ));
    }
    let mut mikkt = MikktMesh {
        mesh,
        tangents: vec![[1.0, 0.0, 0.0, 1.0]; mesh.positions.len()],
    };
    generate_tangents(&mut mikkt).map_err(|_| {
        ImportError::MeshValidation(regia_asset::MeshValidationError::InvalidIndexCount {
            count: mesh.indices.len(),
        })
    })?;
    Ok(mikkt.tangents)
}

/// Which usage wins when one image fills several material slots: a normal map keeps its BC5
/// encoding, a colour map keeps all three channels (a base colour image that is also the
/// occlusion map would otherwise cook as a one-channel mask), and the single-channel and linear
/// slots (which `cook_packed_orm` repacks on their own) yield; an occlusion-and-roughness image
/// stays a mask, as before.
fn usage_rank(usage: TextureCookUsage) -> u8 {
    match usage {
        TextureCookUsage::NormalMap => 4,
        TextureCookUsage::ColorSrgb => 3,
        TextureCookUsage::Mask => 2,
        TextureCookUsage::ColorLinear => 1,
        _ => 0,
    }
}

fn texture_usages_for_document(
    document: &gltf::Document,
    extensions: &GltfExtensionTable,
) -> HashMap<usize, TextureCookUsage> {
    let mut map: HashMap<usize, TextureCookUsage> = HashMap::new();
    // One image can fill several slots; the cook keeps the usage that loses the least data.
    let claim = |map: &mut HashMap<usize, TextureCookUsage>, image: usize, usage: TextureCookUsage| {
        let keep = map
            .get(&image)
            .is_some_and(|old| usage_rank(*old) >= usage_rank(usage));
        if !keep {
            map.insert(image, usage);
        }
    };
    for material in document.materials() {
        let pbr = material.pbr_metallic_roughness();
        if let Some(tex) = pbr.base_color_texture() {
            claim(
                &mut map,
                tex.texture().source().index(),
                TextureCookUsage::ColorSrgb,
            );
        }
        if let Some(tex) = material.normal_texture() {
            claim(
                &mut map,
                tex.texture().source().index(),
                TextureCookUsage::NormalMap,
            );
        }
        if let Some(tex) = pbr.metallic_roughness_texture() {
            // Source MR stays linear. `cook_packed_orm` writes the packed ORM the material references.
            claim(
                &mut map,
                tex.texture().source().index(),
                TextureCookUsage::ColorLinear,
            );
        }
        if let Some(tex) = material.occlusion_texture() {
            claim(&mut map, tex.texture().source().index(), TextureCookUsage::Mask);
        }
        if let Some(tex) = material.emissive_texture() {
            claim(
                &mut map,
                tex.texture().source().index(),
                TextureCookUsage::ColorSrgb,
            );
        }
    }
    // Extension textures never override a core slot's usage, with one exception: an extension
    // that reads every channel of the cooked image (iridescence thickness in G, say) lifts a core
    // `Mask` (one channel, BC4) to `ColorLinear`, or the extension would read zeros. The core
    // slot reads R either way (audit wave 5, render M2).
    for (image, usage) in extensions.texture_usages() {
        match map.get(&image) {
            None => {
                map.insert(image, usage);
            }
            Some(TextureCookUsage::Mask) if usage == TextureCookUsage::ColorLinear => {
                map.insert(image, usage);
            }
            Some(_) => {}
        }
    }
    map
}

/// Deepest node nesting the importer walks. Real hierarchies (armatures, nested prefabs) are a
/// few dozen levels; a cycle or a hostile chain stops here with an error instead of exhausting
/// the stack, which aborts the process (the cook, the editor) rather than failing one asset.
pub(crate) const MAX_NODE_DEPTH: usize = 256;

/// Most node visits one import makes. A node listed under several parents is walked once per
/// parent (instancing); a file that nests such sharing so the walk grows exponentially stops here.
pub(crate) const MAX_NODE_VISITS: usize = 1 << 20;

/// Where [`visit_node`] is in the hierarchy walk.
#[derive(Default)]
struct NodeWalk {
    /// Nesting depth of the node being visited (scene roots are 0).
    depth: usize,
    /// Nodes visited so far.
    visits: usize,
    /// `collision_*` nodes with a mesh, in visit order (ADR-2341).
    proxies: Vec<crate::collision_proxies::ProxySource>,
    /// Every cooked primitive, for `KHR_materials_variants`.
    slots: Vec<PrimitiveSlot>,
    /// Nodes with `EXT_mesh_gpu_instancing`, in visit order.
    instanced: Vec<InstancedNode>,
    /// Instances over every entry of `instanced`.
    instance_total: usize,
    /// Mesh sub-asset paths of the nodes the `meshes` option skipped (ADR-8376): reserved, so a
    /// later node of the same name keeps the path it has when nothing is skipped.
    skipped_paths: Vec<String>,
}

/// One cooked primitive and where it came from.
struct PrimitiveSlot {
    /// glTF mesh index.
    mesh: usize,
    /// Primitive index in that mesh.
    primitive: usize,
    /// The mesh sub-path without `mesh/`.
    slot: String,
    /// The primitive's own material.
    material: Option<usize>,
}

/// A node drawn many times.
struct InstancedNode {
    name: String,
    /// The matrix baked into `parts`.
    world: [f32; 16],
    parts: Vec<AssetId>,
    /// The authored instance matrices.
    locals: Vec<[f32; 16]>,
}

/// A `collision_*` node's mesh as the proxy cook reads it: every primitive's positions in the
/// node's space, and the triangles of its triangle-list primitives.
fn read_proxy(
    name: &str,
    mesh: &gltf::Mesh<'_>,
    buffers: &[Vec<u8>],
    world: [f32; 16],
) -> crate::collision_proxies::ProxySource {
    let mut proxy = crate::collision_proxies::ProxySource {
        node: name.to_string(),
        world,
        ..Default::default()
    };
    for primitive in mesh.primitives() {
        let reader = primitive.reader(|buffer| buffers.get(buffer.index()).map(|b| b.as_slice()));
        let Some(positions) = reader.read_positions() else {
            continue;
        };
        let base = proxy.positions.len() as u32;
        proxy.positions.extend(positions);
        let count = proxy.positions.len() as u32 - base;
        if matches!(primitive.mode(), gltf::mesh::Mode::Triangles) {
            let indices: Vec<u32> = match reader.read_indices() {
                Some(read) => read.into_u32().collect(),
                None => (0..count).collect(),
            };
            proxy.triangles.extend(
                indices
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .map(|t| t.map(|i| if i < count { base + i } else { u32::MAX })),
            );
        }
    }
    proxy
}

/// Records `packed` as the file's primary mesh.
///
/// An `_LOD0` mesh, or a mesh with no `_LODn` tag, wins, and the first of those
/// in the walk is kept. When the file has neither, the lowest `_LODn` index
/// present is kept instead (the first mesh of that index). A later classic mesh
/// still wins over a fallback recorded earlier in the walk.
fn consider_primary(
    name: &str,
    packed: CookedMesh,
    primary_mesh: &mut Option<CookedMesh>,
    lod_fallback: &mut Option<(u32, CookedMesh)>,
) {
    let parsed = crate::gltf_conventions::parse_lod_name(name);
    let classic = matches!(parsed, Some((_, 0)) | None);
    if primary_mesh.is_none() && classic {
        *primary_mesh = Some(packed);
        // A classic mesh wins even when a higher LOD was visited first.
        *lod_fallback = None;
        return;
    }
    if primary_mesh.is_none()
        && let Some((_, level)) = parsed
        && lod_fallback.as_ref().is_none_or(|(have, _)| level < *have)
    {
        *lod_fallback = Some((level, packed));
    }
}

#[allow(clippy::too_many_arguments)]
fn visit_node(
    node: gltf::Node<'_>,
    parent_xf: [f32; 16],
    buffers: &[Vec<u8>],
    mesh_ctx: &MeshContext<'_>,
    parent_id: AssetId,
    sub_assets: &mut Vec<GltfSubCook>,
    meta_entries: &mut Vec<SubAssetEntry>,
    primary_mesh: &mut Option<CookedMesh>,
    lod_fallback: &mut Option<(u32, CookedMesh)>,
    mesh_lods: Option<&crate::mesh_cook::MeshLodSettings>,
    lod_mesh_entries: &mut Vec<(String, AssetId)>,
    sockets: &mut Vec<regia_asset::PrefabSocket>,
    parts: &mut Vec<(AssetId, Option<usize>, String)>,
    walk: &mut NodeWalk,
) -> Result<(), ImportError> {
    walk.visits += 1;
    if walk.depth > MAX_NODE_DEPTH {
        return Err(ImportError::Gltf(format!(
            "node {} is nested deeper than {MAX_NODE_DEPTH} levels (or is its own ancestor)",
            node.index()
        )));
    }
    if walk.visits > MAX_NODE_VISITS {
        return Err(ImportError::Gltf(format!(
            "the node hierarchy needs more than {MAX_NODE_VISITS} node visits (nodes shared by many parents)"
        )));
    }
    let name = node.name().unwrap_or("Unnamed");
    let local = node_local_matrix(&node);
    // `mul_mat4(a, b)` is `b * a` for these column-major arrays, so this is `parent * local`.
    let world = mul_mat4(&local, &parent_xf);

    if let Some(socket) = crate::gltf_conventions::socket_from_node(name, world) {
        sockets.push(socket);
    }

    if name.starts_with("collision_") {
        // A collision proxy is never drawn (DO-6); its vertices become a collider shape.
        match node.mesh() {
            Some(mesh) => walk.proxies.push(read_proxy(name, &mesh, buffers, world)),
            None => {
                tracing::warn!(target: "regia_asset_pipeline", node = name, "collision node without a mesh: no collider")
            }
        }
    }
    let skip_mesh = name.starts_with("socket_") || name.starts_with("collision_");
    if !skip_mesh && let Some(mesh) = node.mesh() {
        let skin = match node.skin().map(|s| (s.index(), mesh_ctx.skins.get(s.index()))) {
            Some((index, Some(Ok(layout)))) => mesh_ctx.skin_bakes.get(index).map(|bake| (layout, bake)),
            Some((index, problem)) => {
                let reason = match problem {
                    Some(Err(reason)) => reason.as_str(),
                    _ => "missing",
                };
                tracing::warn!(
                    target: "regia_asset_pipeline",
                    node = name,
                    skin = index,
                    "skin is unusable ({reason}); the mesh cooks rigid"
                );
                None
            }
            None => None,
        };
        let target_names = mesh_ctx
            .target_names
            .get(mesh.index())
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut cooked_parts = Vec::new();
        // The `meshes` option (ADR-8376): a node whose first primitive's key is skipped cooks no
        // primitive; its paths stay reserved so no other node's id moves.
        let clean_name = sanitize_name(name);
        let skipped = mesh_sub_path(sub_assets, &walk.skipped_paths, &clean_name, node.index(), 0);
        let primitives: Vec<(usize, gltf::mesh::Primitive<'_>)> =
            if crate::gltf_settings::mesh_key_of(&skipped)
                .is_some_and(|key| mesh_ctx.skip_meshes.contains(key))
            {
                for prim_index in 0..mesh.primitives().count() {
                    let path = mesh_sub_path(
                        sub_assets,
                        &walk.skipped_paths,
                        &clean_name,
                        node.index(),
                        prim_index,
                    );
                    walk.skipped_paths.push(path);
                }
                Vec::new()
            } else {
                mesh.primitives().enumerate().collect()
            };
        for (prim_index, primitive) in primitives {
            if !matches!(primitive.mode(), gltf::mesh::Mode::Triangles) {
                return Err(ImportError::UnsupportedMode(primitive.mode() as u32));
            }
            // Skinned primitives bake the skin's matrix, not this node's world: glTF ignores a
            // skinned mesh node's transform, and the skeleton's inverse bind matrices undo the bake.
            let (bake, layout) = match skin {
                Some((layout, bake)) => (bake, Some(layout)),
                None => (&world, None),
            };
            let mut cpu = read_primitive(&primitive, buffers, bake, layout, target_names)?;
            if mesh_ctx.vertex_ao {
                crate::gltf_settings::clamp_vertex_ao(&mut cpu.colors);
            }
            if mesh_ctx.eight_influences {
                crate::gltf_settings::opt_into_eight_influences(&mut cpu, name);
            }
            let cooked = cook_mesh(cpu);
            // Authored LOD levels keep their own mesh; only LOD0 (or non-LOD meshes)
            // may receive generated LODs from cook settings.
            let pack_lods = match crate::gltf_conventions::parse_lod_name(name) {
                Some((_, level)) if level > 0 => None,
                _ => mesh_lods,
            };
            let packed = pack_cooked_mesh(&cooked, pack_lods)?;
            // Two nodes with one name (or two unnamed ones) must not share a sub-asset id: the
            // first keeps the plain path, later ones carry their node index.
            // (A counter follows the node index when that path is taken too: an authored name
            // spelling it, or the same node reached again under another parent.)
            let sub_path = mesh_sub_path(
                sub_assets,
                &walk.skipped_paths,
                &clean_name,
                node.index(),
                prim_index,
            );
            let id = crate::subasset_remap::sub_id(parent_id, &sub_path);
            let sub_path_slot = sub_path.strip_prefix("mesh/").unwrap_or(&sub_path).to_string();
            let payload = packed
                .to_postcard()
                .map_err(|e| ImportError::Gltf(e.to_string()))?;
            sub_assets.push(GltfSubCook {
                path: sub_path.clone(),
                id,
                type_path: "regia_asset::CookedMesh".to_string(),
                payload,
                dependencies: vec![],
            });
            meta_entries.push(SubAssetEntry {
                path: sub_path,
                id,
                type_path: "regia_asset::CookedMesh".to_string(),
            });
            parts.push((id, primitive.material().index(), name.to_string()));
            cooked_parts.push(id);
            walk.slots.push(PrimitiveSlot {
                mesh: mesh.index(),
                primitive: prim_index,
                slot: sub_path_slot,
                material: primitive.material().index(),
            });
            if crate::gltf_conventions::parse_lod_name(name).is_some() {
                lod_mesh_entries.push((name.to_string(), id));
            }
            // LOD0, or a mesh with no `_LODn` tag, is the primary (first one in the
            // walk). A file whose meshes are all `_LODn` with n > 0 still imports:
            // the lowest index wins, and the first mesh of that index wins a tie.
            // Generated LODs stay off for n > 0, as before (ADR-7959).
            consider_primary(name, packed, primary_mesh, lod_fallback);
        }
        if let Some(locals) = mesh_ctx.instances.get(&node.index())
            && !cooked_parts.is_empty()
        {
            // A node shared by many parents is visited once per parent, each with its own copy.
            walk.instance_total = walk.instance_total.saturating_add(locals.len());
            if walk.instance_total > crate::gltf_normalize::MAX_FILE_INSTANCES {
                return Err(ImportError::GltfExtension {
                    code: crate::gltf_normalize::GLTF_INSTANCING_INVALID,
                    extension: "EXT_mesh_gpu_instancing".to_string(),
                    message: format!(
                        "node {}: the hierarchy reaches more than {} instances",
                        node.index(),
                        crate::gltf_normalize::MAX_FILE_INSTANCES
                    ),
                });
            }
            walk.instanced.push(InstancedNode {
                name: name.to_string(),
                world,
                parts: cooked_parts,
                locals: locals.clone(),
            });
        }
    }
    for child in node.children() {
        walk.depth += 1;
        let visited = visit_node(
            child,
            world,
            buffers,
            mesh_ctx,
            parent_id,
            sub_assets,
            meta_entries,
            primary_mesh,
            lod_fallback,
            mesh_lods,
            lod_mesh_entries,
            sockets,
            parts,
            walk,
        );
        walk.depth -= 1;
        visited?;
    }
    Ok(())
}

/// The matrix above every scene root: the axis yaw for [`Forward::Convert`], identity otherwise,
/// times `scale` on the diagonal. The yaw is applied once here, not per node, or nested nodes
/// would cancel it. `scale == 1` returns the historical matrices (exact ±1 entries).
fn root_matrix(forward: Forward, scale: f32) -> [f32; 16] {
    let s = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    if s == 1.0 {
        return if forward == Forward::Convert {
            y_rotation_180()
        } else {
            [
                1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
            ]
        };
    }
    let (x, z) = if forward == Forward::Convert {
        (-s, -s)
    } else {
        (s, s)
    };
    [
        x, 0.0, 0.0, 0.0, 0.0, s, 0.0, 0.0, 0.0, 0.0, z, 0.0, 0.0, 0.0, 0.0, 1.0,
    ]
}

/// `first` when no sub-asset uses it yet, else the first of `retry(0)`, `retry(1)`, ... that is
/// unused. The retry spellings are tried in order until one is free, so a sub path is never
/// shared by two sub-assets (two would get one id and one cooked file).
fn unused_sub_path(sub_assets: &[GltfSubCook], first: String, retry: impl Fn(usize) -> String) -> String {
    let taken = |path: &str| sub_assets.iter().any(|s| s.path == path);
    if !taken(&first) {
        return first;
    }
    (0..).map(retry).find(|path| !taken(path)).unwrap_or(first)
}

/// The mesh sub-asset path of primitive `prim_index` of node `node_index` (named `clean`): the
/// plain `mesh/<clean>/primitive_<i>` unless a cooked or skipped (ADR-8376) primitive has it, then
/// the node index, then a counter.
fn mesh_sub_path(
    sub_assets: &[GltfSubCook],
    skipped: &[String],
    clean: &str,
    node_index: usize,
    prim_index: usize,
) -> String {
    let taken = |path: &str| skipped.iter().any(|s| s == path) || sub_assets.iter().any(|s| s.path == path);
    let first = format!("mesh/{clean}/primitive_{prim_index}");
    if !taken(&first) {
        return first;
    }
    (0..)
        .map(|n| match n {
            0 => format!("mesh/{clean}~{node_index}/primitive_{prim_index}"),
            n => format!("mesh/{clean}~{node_index}~{n}/primitive_{prim_index}"),
        })
        .find(|path| !taken(path))
        .unwrap_or(first)
}

/// The importer's mesh and material keys of `document` without cooking (ADR-8376,
/// [`crate::gltf_settings::gltf_part_keys`]): [`visit_node`]'s walk and naming and the material
/// loop's naming, with nothing skipped.
pub(crate) fn part_keys(document: &gltf::Document) -> crate::gltf_settings::GltfPartKeys {
    fn walk(
        node: gltf::Node<'_>,
        taken: &mut Vec<String>,
        visits: &mut usize,
        out: &mut Vec<(String, usize, usize)>,
    ) {
        *visits += 1;
        if *visits > MAX_NODE_VISITS {
            return;
        }
        let name = node.name().unwrap_or("Unnamed");
        let skip_mesh = name.starts_with("socket_") || name.starts_with("collision_");
        if !skip_mesh && let Some(mesh) = node.mesh() {
            let clean = sanitize_name(name);
            let mut key = None;
            for prim_index in 0..mesh.primitives().count() {
                let path = mesh_sub_path(&[], taken, &clean, node.index(), prim_index);
                if key.is_none() {
                    key = crate::gltf_settings::mesh_key_of(&path).map(str::to_string);
                }
                taken.push(path);
            }
            if let Some(key) = key {
                out.push((key, node.index(), mesh.index()));
            }
        }
        for child in node.children() {
            walk(child, taken, visits, out);
        }
    }
    let mut out = crate::gltf_settings::GltfPartKeys::default();
    let scene = document.default_scene().or_else(|| document.scenes().next());
    let (mut taken, mut visits) = (Vec::new(), 0);
    for node in scene.into_iter().flat_map(|s| s.nodes()) {
        walk(node, &mut taken, &mut visits, &mut out.meshes);
    }
    let mut paths: Vec<String> = Vec::new();
    for (mat_index, material) in document.materials().enumerate() {
        let mat_name = material
            .name()
            .map(sanitize_name)
            .unwrap_or_else(|| mat_index.to_string());
        let path = material_sub_path(&paths, &mat_name, mat_index);
        out.materials
            .push(path.strip_prefix("material/").unwrap_or(&path).to_string());
        paths.push(path);
    }
    out
}

/// The material loop's path rule over the material paths taken so far.
fn material_sub_path(taken: &[String], mat_name: &str, mat_index: usize) -> String {
    let is_taken = |path: &str| taken.iter().any(|t| t == path);
    let first = format!("material/{mat_name}");
    if !is_taken(&first) {
        return first;
    }
    (0..)
        .map(|n| match n {
            0 => format!("material/{mat_name}~{mat_index}"),
            n => format!("material/{mat_name}~{mat_index}~{n}"),
        })
        .find(|path| !is_taken(path))
        .unwrap_or(first)
}

fn sanitize_name(name: &str) -> String {
    name.replace(['/', '\\', '#'], "_")
}

fn node_local_matrix(node: &gltf::Node) -> [f32; 16] {
    let m = node.transform().matrix();
    let mut out = [0.0f32; 16];
    for i in 0..4 {
        for j in 0..4 {
            out[i * 4 + j] = m[i][j];
        }
    }
    out
}

fn y_rotation_180() -> [f32; 16] {
    [
        -1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, -1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
    ]
}

fn mul_mat4(a: &[f32; 16], b: &[f32; 16]) -> [f32; 16] {
    let mut out = [0.0f32; 16];
    for i in 0..4 {
        for j in 0..4 {
            out[i * 4 + j] = a[i * 4] * b[j]
                + a[i * 4 + 1] * b[4 + j]
                + a[i * 4 + 2] * b[8 + j]
                + a[i * 4 + 3] * b[12 + j];
        }
    }
    out
}

fn transform_point(m: &[f32; 16], p: &[f32; 3]) -> [f32; 3] {
    let x = m[0] * p[0] + m[4] * p[1] + m[8] * p[2] + m[12];
    let y = m[1] * p[0] + m[5] * p[1] + m[9] * p[2] + m[13];
    let z = m[2] * p[0] + m[6] * p[1] + m[10] * p[2] + m[14];
    [x, y, z]
}

/// The 3×3 (column-major) that turns normals for the bake `m`: the inverse transpose of its
/// linear part, up to a positive scale (the cofactor matrix, sign-corrected for mirroring
/// bakes). A rotation-and-uniform-scale bake returns its linear part unchanged, which is the
/// same direction and keeps those meshes' normals bit for bit.
fn normal_matrix(m: &[f32; 16]) -> [f32; 9] {
    let c = [[m[0], m[1], m[2]], [m[4], m[5], m[6]], [m[8], m[9], m[10]]];
    let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let cross = |a: [f32; 3], b: [f32; 3]| {
        [
            a[1] * b[2] - a[2] * b[1],
            a[2] * b[0] - a[0] * b[2],
            a[0] * b[1] - a[1] * b[0],
        ]
    };
    let lengths = c.map(|col| dot(col, col));
    let largest = lengths.iter().copied().fold(0.0f32, f32::max);
    let tolerance = 1e-5 * largest;
    let similarity = lengths.iter().all(|l| (l - largest).abs() <= tolerance)
        && dot(c[0], c[1]).abs() <= tolerance
        && dot(c[0], c[2]).abs() <= tolerance
        && dot(c[1], c[2]).abs() <= tolerance;
    if similarity {
        return [
            c[0][0], c[0][1], c[0][2], c[1][0], c[1][1], c[1][2], c[2][0], c[2][1], c[2][2],
        ];
    }
    // Columns of the cofactor matrix: cof(M) = det(M)·M⁻ᵀ.
    let k = [cross(c[1], c[2]), cross(c[2], c[0]), cross(c[0], c[1])];
    let sign = if dot(c[0], k[0]) < 0.0 { -1.0 } else { 1.0 };
    [
        sign * k[0][0],
        sign * k[0][1],
        sign * k[0][2],
        sign * k[1][0],
        sign * k[1][1],
        sign * k[1][2],
        sign * k[2][0],
        sign * k[2][1],
        sign * k[2][2],
    ]
}

/// Column-major 3×3 times a vector.
fn mul_mat3(m: &[f32; 9], v: [f32; 3]) -> [f32; 3] {
    [
        m[0] * v[0] + m[3] * v[1] + m[6] * v[2],
        m[1] * v[0] + m[4] * v[1] + m[7] * v[2],
        m[2] * v[0] + m[5] * v[1] + m[8] * v[2],
    ]
}

/// Turns a normal by a [`normal_matrix`] and renormalises it.
fn transform_normal(m: &[f32; 9], n: &[f32; 3]) -> [f32; 3] {
    let [x, y, z] = mul_mat3(m, *n);
    let len = (x * x + y * y + z * z).sqrt();
    if len > 1e-8 {
        [x / len, y / len, z / len]
    } else {
        [0.0, 1.0, 0.0]
    }
}

fn generate_normals(positions: &[[f32; 3]], index_iter: Option<impl Iterator<Item = u32>>) -> Vec<[f32; 3]> {
    let vc = positions.len();
    let mut accum = vec![[0.0f32; 3]; vc];
    let mut add_face = |i0: usize, i1: usize, i2: usize| {
        if i0 >= vc || i1 >= vc || i2 >= vc {
            return;
        }
        let (p0, p1, p2) = (positions[i0], positions[i1], positions[i2]);
        let e1 = [p1[0] - p0[0], p1[1] - p0[1], p1[2] - p0[2]];
        let e2 = [p2[0] - p0[0], p2[1] - p0[1], p2[2] - p0[2]];
        let n = [
            e1[1] * e2[2] - e1[2] * e2[1],
            e1[2] * e2[0] - e1[0] * e2[2],
            e1[0] * e2[1] - e1[1] * e2[0],
        ];
        for i in [i0, i1, i2] {
            accum[i][0] += n[0];
            accum[i][1] += n[1];
            accum[i][2] += n[2];
        }
    };
    if let Some(iter) = index_iter {
        let indices: Vec<u32> = iter.collect();
        for tri in indices.chunks(3) {
            if tri.len() == 3 {
                add_face(tri[0] as usize, tri[1] as usize, tri[2] as usize);
            }
        }
    }
    accum
        .iter()
        .map(|&[x, y, z]| {
            let len = (x * x + y * y + z * z).sqrt();
            if len > 1e-10 {
                [x / len, y / len, z / len]
            } else {
                [0.0, 0.0, 0.0]
            }
        })
        .collect()
}

#[cfg(test)]
mod orm_and_variant_tests {
    use super::{Forward, TextureCookQuality, import_gltf_glb_bytes};
    use image::ImageEncoder;
    use regia_asset::{
        AssetId, COOKED_MATERIAL_VERSION, CookedImage, CookedStandardMaterial, MATERIAL_FORMAT_VERSION,
        MaterialShaderRef, PrefabInstance, TextureSlot,
    };
    use regia_ecs::prelude::*;

    fn rgba_png(pixel: [u8; 4]) -> Vec<u8> {
        let mut out = Vec::new();
        image::codecs::png::PngEncoder::new(&mut out)
            .write_image(&pixel, 1, 1, image::ExtendedColorType::Rgba8)
            .expect("png");
        out
    }

    fn pad4(buf: &mut Vec<u8>) {
        while !buf.len().is_multiple_of(4) {
            buf.push(0);
        }
    }

    /// GLB: one triangle, occlusion R=10, metallic-roughness G=20 B=30, hull_Rem / hull_A52.
    fn variant_orm_glb() -> Vec<u8> {
        let ao = rgba_png([10, 0, 0, 255]);
        let mr = rgba_png([0, 20, 30, 255]);
        let mut bin = Vec::new();
        for _ in 0..3 {
            bin.extend_from_slice(&0.0f32.to_le_bytes());
            bin.extend_from_slice(&0.0f32.to_le_bytes());
            bin.extend_from_slice(&0.0f32.to_le_bytes());
        }
        for &i in &[0u32, 1, 2] {
            bin.extend_from_slice(&i.to_le_bytes());
        }
        pad4(&mut bin);
        let ao_off = bin.len();
        bin.extend_from_slice(&ao);
        pad4(&mut bin);
        let mr_off = bin.len();
        bin.extend_from_slice(&mr);
        pad4(&mut bin);

        let json = format!(
            r#"{{"asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[0]}}],"nodes":[{{"mesh":0,"name":"Body_LOD0"}}],"meshes":[{{"primitives":[{{"attributes":{{"POSITION":0}},"indices":1,"mode":4,"material":0}}]}}],"materials":[{{"name":"hull_Rem","pbrMetallicRoughness":{{"baseColorFactor":[0.8,0.1,0.1,1.0],"metallicRoughnessTexture":{{"index":1}}}},"occlusionTexture":{{"index":0}}}},{{"name":"hull_A52","pbrMetallicRoughness":{{"baseColorFactor":[0.1,0.15,0.85,1.0]}}}}],"textures":[{{"source":0}},{{"source":1}}],"images":[{{"bufferView":2,"mimeType":"image/png"}},{{"bufferView":3,"mimeType":"image/png"}}],"accessors":[{{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0.0,0.0,0.0],"max":[0.0,0.0,0.0]}},{{"bufferView":1,"componentType":5125,"count":3,"type":"SCALAR"}}],"bufferViews":[{{"buffer":0,"byteOffset":0,"byteLength":36,"byteStride":12,"target":34962}},{{"buffer":0,"byteOffset":36,"byteLength":12,"target":34963}},{{"buffer":0,"byteOffset":{ao_off},"byteLength":{ao_len}}},{{"buffer":0,"byteOffset":{mr_off},"byteLength":{mr_len}}}],"buffers":[{{"byteLength":{bin_len}}}]}}"#,
            ao_len = ao.len(),
            mr_len = mr.len(),
            bin_len = bin.len(),
        );
        let mut json_bytes = json.into_bytes();
        while !json_bytes.len().is_multiple_of(4) {
            json_bytes.push(b' ');
        }
        let total = 12 + 8 + json_bytes.len() + 8 + bin.len();
        let mut glb = Vec::new();
        glb.extend_from_slice(&0x4654_6C67u32.to_le_bytes());
        glb.extend_from_slice(&2u32.to_le_bytes());
        glb.extend_from_slice(&(total as u32).to_le_bytes());
        glb.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"JSON");
        glb.extend_from_slice(&json_bytes);
        glb.extend_from_slice(&(bin.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"BIN\0");
        glb.extend_from_slice(&bin);
        glb
    }

    /// GLB: a 1 m panel twice (DO27's cargo box), the second copy on its own vertices with
    /// its UVs transposed, plus the same panel's back face (opposite winding).
    fn duplicated_panel_glb() -> Vec<u8> {
        let corners = [
            [0.6f32, 0.0, 0.0],
            [0.6, 1.0, 0.0],
            [0.6, 1.0, 1.0],
            [0.6, 0.0, 1.0],
        ];
        let uv_a = [[0.0f32, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
        let uv_b = [[0.0f32, 0.0], [0.0, 1.0], [1.0, 1.0], [1.0, 0.0]];
        let mut bin = Vec::new();
        for p in corners.iter().chain(corners.iter()) {
            for c in p {
                bin.extend_from_slice(&c.to_le_bytes());
            }
        }
        for uv in uv_a.iter().chain(uv_b.iter()) {
            for c in uv {
                bin.extend_from_slice(&c.to_le_bytes());
            }
        }
        let idx: [u32; 18] = [0, 1, 2, 0, 2, 3, 4, 5, 6, 4, 6, 7, 0, 2, 1, 0, 3, 2];
        for i in idx {
            bin.extend_from_slice(&i.to_le_bytes());
        }
        let json = r#"{"asset":{"version":"2.0"},"scene":0,"scenes":[{"nodes":[0]}],"nodes":[{"mesh":0,"name":"Box_LOD0"}],"meshes":[{"primitives":[{"attributes":{"POSITION":0,"TEXCOORD_0":1},"indices":2,"mode":4}]}],"accessors":[{"bufferView":0,"componentType":5126,"count":8,"type":"VEC3","min":[0.6,0.0,0.0],"max":[0.6,1.0,1.0]},{"bufferView":1,"componentType":5126,"count":8,"type":"VEC2"},{"bufferView":2,"componentType":5125,"count":18,"type":"SCALAR"}],"bufferViews":[{"buffer":0,"byteOffset":0,"byteLength":96},{"buffer":0,"byteOffset":96,"byteLength":64},{"buffer":0,"byteOffset":160,"byteLength":72}],"buffers":[{"byteLength":232}]}"#;
        let mut json_bytes = json.as_bytes().to_vec();
        while !json_bytes.len().is_multiple_of(4) {
            json_bytes.push(b' ');
        }
        let total = 12 + 8 + json_bytes.len() + 8 + bin.len();
        let mut glb = Vec::new();
        glb.extend_from_slice(&0x4654_6C67u32.to_le_bytes());
        glb.extend_from_slice(&2u32.to_le_bytes());
        glb.extend_from_slice(&(total as u32).to_le_bytes());
        glb.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"JSON");
        glb.extend_from_slice(&json_bytes);
        glb.extend_from_slice(&(bin.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"BIN\0");
        glb.extend_from_slice(&bin);
        glb
    }

    #[test]
    fn cook_drops_the_coincident_copy_of_a_panel_and_keeps_its_back_face() {
        let imported = import_gltf_glb_bytes(
            &duplicated_panel_glb(),
            AssetId::new_random(),
            Forward::Keep,
            "linux",
            TextureCookQuality::Low,
        )
        .expect("import");
        let mesh = &imported.primary_mesh;
        let width = match mesh.index_width {
            regia_asset::CookedIndexWidth::U16 => 2,
            regia_asset::CookedIndexWidth::U32 => 4,
        };
        // LOD 0: front panel (2 triangles) + back face (2); the transposed-UV copy (2) is gone.
        let lod0 = mesh
            .lods
            .first()
            .map_or(mesh.indices.len(), |l| l.index_byte_length as usize);
        assert_eq!(lod0 / width, 12, "{} LOD 0 indices", lod0 / width);
    }

    fn image_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        image::codecs::png::PngEncoder::new(&mut out)
            .write_image(rgba, width, height, image::ExtendedColorType::Rgba8)
            .expect("png");
        out
    }

    /// GLB: one triangle with a 16² metallic-roughness texture (roughness G = 40) and, when
    /// `with_normal`, a 16² checker of normals tilted ±0.5 rad as its normal map.
    fn orm_normal_glb(with_normal: bool) -> Vec<u8> {
        let mr = image_png(16, 16, &[0u8, 40, 0, 255].repeat(256));
        let mut normal_px = Vec::new();
        for y in 0..16u32 {
            for x in 0..16u32 {
                let s = if (x + y) % 2 == 0 { 0.5f32 } else { -0.5 };
                normal_px.extend([s.sin(), 0.0, s.cos()].map(|c| ((c * 0.5 + 0.5) * 255.0 + 0.5) as u8));
                normal_px.push(255);
            }
        }
        let normal = image_png(16, 16, &normal_px);
        let mut bin = Vec::new();
        for _ in 0..3 {
            bin.extend_from_slice(&[0u8; 12]);
        }
        for &i in &[0u32, 1, 2] {
            bin.extend_from_slice(&i.to_le_bytes());
        }
        pad4(&mut bin);
        let mr_off = bin.len();
        bin.extend_from_slice(&mr);
        pad4(&mut bin);
        let n_off = bin.len();
        bin.extend_from_slice(&normal);
        pad4(&mut bin);
        let normal_ref = if with_normal {
            r#","normalTexture":{"index":1}"#
        } else {
            ""
        };
        let json = format!(
            r#"{{"asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[0]}}],"nodes":[{{"mesh":0,"name":"Plate"}}],"meshes":[{{"primitives":[{{"attributes":{{"POSITION":0}},"indices":1,"mode":4,"material":0}}]}}],"materials":[{{"name":"plate","pbrMetallicRoughness":{{"metallicRoughnessTexture":{{"index":0}}}}{normal_ref}}}],"textures":[{{"source":0}},{{"source":1}}],"images":[{{"bufferView":2,"mimeType":"image/png"}},{{"bufferView":3,"mimeType":"image/png"}}],"accessors":[{{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0.0,0.0,0.0],"max":[0.0,0.0,0.0]}},{{"bufferView":1,"componentType":5125,"count":3,"type":"SCALAR"}}],"bufferViews":[{{"buffer":0,"byteOffset":0,"byteLength":36,"byteStride":12,"target":34962}},{{"buffer":0,"byteOffset":36,"byteLength":12,"target":34963}},{{"buffer":0,"byteOffset":{mr_off},"byteLength":{mr_len}}},{{"buffer":0,"byteOffset":{n_off},"byteLength":{n_len}}}],"buffers":[{{"byteLength":{bin_len}}}]}}"#,
            mr_len = mr.len(),
            n_len = normal.len(),
            bin_len = bin.len(),
        );
        let mut json_bytes = json.into_bytes();
        while !json_bytes.len().is_multiple_of(4) {
            json_bytes.push(b' ');
        }
        let total = 12 + 8 + json_bytes.len() + 8 + bin.len();
        let mut glb = Vec::new();
        glb.extend_from_slice(&0x4654_6C67u32.to_le_bytes());
        glb.extend_from_slice(&2u32.to_le_bytes());
        glb.extend_from_slice(&(total as u32).to_le_bytes());
        glb.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"JSON");
        glb.extend_from_slice(&json_bytes);
        glb.extend_from_slice(&(bin.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"BIN\0");
        glb.extend_from_slice(&bin);
        glb
    }

    /// The cooked packed ORM's KTX2 levels (mip 0 first).
    fn cooked_orm_levels(with_normal: bool) -> Vec<Vec<u8>> {
        let imported = import_gltf_glb_bytes(
            &orm_normal_glb(with_normal),
            AssetId::new_random(),
            Forward::Keep,
            "linux",
            TextureCookQuality::High,
        )
        .expect("import");
        let orm = imported
            .sub_assets
            .iter()
            .find(|s| s.path == "texture/orm/0")
            .expect("packed orm sub-asset");
        assert_eq!(orm.type_path, "regia_asset::CookedTexture");
        let texture = regia_asset::CookedTexture::from_postcard(&orm.payload).expect("orm texture");
        let reader = ktx2::Reader::new(&texture.ktx2[..]).expect("ktx2");
        reader.levels().map(|level| level.data.to_vec()).collect()
    }

    /// Toksvig at cook: the packed ORM under a bumpy normal map keeps its level 0 and gets
    /// different (rougher) mips; without the normal map the chain is the plain box filter.
    #[test]
    fn cook_bakes_normal_variance_into_the_orm_mips() {
        let plain = cooked_orm_levels(false);
        let baked = cooked_orm_levels(true);
        assert_eq!(plain.len(), baked.len());
        assert!(plain.len() >= 3, "16² has a mip chain");
        assert_eq!(plain[0], baked[0], "level 0 (one normal per texel) is unchanged");
        assert_ne!(plain[1], baked[1], "level 1 carries the checker's variance");
    }

    fn cooked_material(imported: &super::GltfImportResult, path: &str) -> CookedStandardMaterial {
        let sub = imported
            .sub_assets
            .iter()
            .find(|s| s.path == path)
            .unwrap_or_else(|| panic!("missing {path}"));
        assert_eq!(sub.type_path, "regia_asset::CookedStandardMaterial");
        let cooked = CookedStandardMaterial::from_postcard(&sub.payload)
            .unwrap_or_else(|e| panic!("decode {path}: {e}"));
        assert_eq!(cooked.version, COOKED_MATERIAL_VERSION);
        assert_eq!(cooked.authored.format_version, MATERIAL_FORMAT_VERSION);
        let hard_surface = cooked
            .authored
            .hard_surface
            .as_ref()
            .expect("glTF cook carries the v2 hard-surface defaults");
        assert!(hard_surface.multi_scatter && hard_surface.specular_occlusion);
        assert!(cooked.authored.name.is_some());
        assert!(matches!(cooked.authored.shader, MaterialShaderRef::Standard));
        cooked
    }

    #[test]
    fn cook_packs_orm_channels_and_runtime_variant_changes_base_color() {
        let parent = AssetId::new_random();
        let imported = import_gltf_glb_bytes(
            &variant_orm_glb(),
            parent,
            Forward::Keep,
            "linux",
            TextureCookQuality::Low,
        )
        .expect("import");

        let orm = imported
            .sub_assets
            .iter()
            .find(|s| s.path == "texture/orm/0")
            .expect("packed orm sub-asset");
        assert_eq!(orm.type_path, "regia_asset::CookedImage");
        let image = CookedImage::from_postcard(&orm.payload).expect("orm image");
        assert_eq!(image.rgba8, vec![10, 20, 30, 255], "R ao, G rough, B metal");

        let rem = cooked_material(&imported, "material/hull_Rem");
        assert!(
            matches!(rem.authored.base.orm_map, Some(TextureSlot::Asset(id)) if id == orm.id),
            "orm slot must be the packed texture, got {:?}",
            rem.authored.base.orm_map
        );
        let rem_color = rem.authored.base.base_color;
        assert_eq!(
            imported
                .sub_assets
                .iter()
                .find(|s| s.path == "material/hull_Rem")
                .expect("rem payload")
                .dependencies
                .as_slice(),
            &[orm.id],
            "cooked material must depend on the packed ORM, not the raw MR texture"
        );
        let raw_mr = imported
            .sub_assets
            .iter()
            .find(|s| s.path == "texture/1")
            .expect("source mr still cooked");
        assert_ne!(raw_mr.id, orm.id, "packed ORM is a new sub-asset");

        let set = imported
            .material_variants
            .iter()
            .find(|s| s.slot == "hull")
            .expect("hull variants")
            .clone();
        let inst = PrefabInstance::new(set, "Rem").expect("rem");
        let rem_id = imported
            .sub_assets
            .iter()
            .find(|s| s.path == "material/hull_Rem")
            .expect("rem id")
            .id;
        assert_eq!(inst.material, rem_id);

        let mut world = World::new();
        let entity = world.spawn(inst).id();
        let mut schedule = Schedule::default();
        schedule.add_systems(|mut query: Query<&mut PrefabInstance>| {
            for mut prefab in &mut query {
                assert!(prefab.select("A52"));
                assert!(!prefab.select("missing"), "unknown variant must not stick");
            }
        });
        schedule.run(&mut world);

        let prefab = world.get::<PrefabInstance>(entity).expect("spawned");
        assert_eq!(prefab.active_variant, "A52");
        let a52_id = imported
            .sub_assets
            .iter()
            .find(|s| s.path == "material/hull_A52")
            .expect("a52 id")
            .id;
        assert_eq!(prefab.material, a52_id);
        let a52 = cooked_material(&imported, "material/hull_A52");
        assert!(a52.authored.base.orm_map.is_none(), "A52 has no packed ORM");
        let a52_color = a52.authored.base.base_color;
        assert!(
            (rem_color[0] - 0.8).abs() < 1e-5 && (rem_color[2] - 0.1).abs() < 1e-5,
            "Rem base color {rem_color:?}"
        );
        assert!(
            (a52_color[2] - 0.85).abs() < 1e-5 && (a52_color[0] - 0.1).abs() < 1e-5,
            "A52 base color {a52_color:?}"
        );
        assert_ne!(rem_color, a52_color);
    }
}

#[cfg(test)]
mod emissive_tests {
    use std::collections::HashMap;

    use super::gltf_material_to_ron;

    #[test]
    fn emissive_factor_and_strength_import_without_a_texture() {
        let json = br#"{"asset":{"version":"2.0"},"extensionsUsed":["KHR_materials_emissive_strength"],
            "materials":[{"name":"M_Light","emissiveFactor":[0.2,1.0,0.32],
            "extensions":{"KHR_materials_emissive_strength":{"emissiveStrength":12.0}}},
            {"name":"M_Plain","emissiveFactor":[0.5,0.25,0.0]}]}"#;
        let gltf = gltf::Gltf::from_slice(json).expect("gltf");
        let mats: Vec<_> = gltf.document.materials().collect();
        let light = gltf_material_to_ron(&mats[0], &HashMap::new()).expect("material");
        assert_eq!(light.base.emissive, [0.2, 1.0, 0.32]);
        assert!((light.base.emissive_strength - 12.0).abs() < 1e-6);
        assert!(light.base.emissive_map.is_none());
        let plain = gltf_material_to_ron(&mats[1], &HashMap::new()).expect("material");
        assert_eq!(plain.base.emissive, [0.5, 0.25, 0.0]);
        assert!((plain.base.emissive_strength - 1.0).abs() < 1e-6);
    }
}

#[cfg(test)]
mod extension_tests {
    use std::collections::HashMap;

    use regia_asset::{AlphaMode, CullMode};

    use super::gltf_material_to_ron;

    #[test]
    fn glass_alpha_and_double_sided_import_per_the_gltf_extensions() {
        let json = br#"{"asset":{"version":"2.0"},
            "extensionsUsed":["KHR_materials_transmission","KHR_materials_volume","KHR_materials_ior"],
            "materials":[{"name":"M_Glass","doubleSided":true,
              "pbrMetallicRoughness":{"baseColorFactor":[1,1,1,1],"metallicFactor":0.0,"roughnessFactor":0.2},
              "extensions":{"KHR_materials_transmission":{"transmissionFactor":0.92},
                "KHR_materials_volume":{"thicknessFactor":0.012,"attenuationDistance":0.08,"attenuationColor":[0.75,0.88,0.84]},
                "KHR_materials_ior":{"ior":1.52}}},
              {"name":"M_Decal","alphaMode":"MASK","alphaCutoff":0.3},
              {"name":"M_Clear","extensions":{"KHR_materials_transmission":{"transmissionFactor":1.0},
                "KHR_materials_volume":{"thicknessFactor":0.01}}}]}"#;
        let gltf = gltf::Gltf::from_slice(json).expect("gltf");
        let mats: Vec<_> = gltf.document.materials().collect();
        let glass = gltf_material_to_ron(&mats[0], &HashMap::new()).expect("glass");
        let t = glass.transmission.as_ref().expect("transmission");
        assert!((t.factor - 0.92).abs() < 1e-6);
        assert!((t.thickness - 0.012).abs() < 1e-6);
        assert!((t.attenuation_distance - 0.08).abs() < 1e-6);
        assert_eq!(t.attenuation_color, [0.75, 0.88, 0.84]);
        assert!((glass.base.ior - 1.52).abs() < 1e-6);
        assert_eq!(glass.render_state.cull_mode, Some(CullMode::None));
        assert_eq!(glass.render_state.alpha_mode, AlphaMode::Opaque);
        let decal = gltf_material_to_ron(&mats[1], &HashMap::new()).expect("decal");
        assert_eq!(decal.render_state.alpha_mode, AlphaMode::Mask);
        assert!((decal.render_state.alpha_cutoff - 0.3).abs() < 1e-6);
        assert!(decal.transmission.is_none());
        assert_eq!(decal.render_state.cull_mode, None);
        assert!((decal.base.ior - 1.5).abs() < 1e-6);
        // glTF's default attenuation distance is infinite: stored as 0 (no absorption).
        let clear = gltf_material_to_ron(&mats[2], &HashMap::new()).expect("clear");
        assert_eq!(clear.transmission.as_ref().expect("t").attenuation_distance, 0.0);
    }
}

#[cfg(test)]
mod normal_transform_tests {
    use super::{mul_mat3, normal_matrix, transform_normal};

    fn diag(x: f32, y: f32, z: f32) -> [f32; 16] {
        [
            x, 0.0, 0.0, 0.0, 0.0, y, 0.0, 0.0, 0.0, 0.0, z, 0.0, 0.0, 0.0, 0.0, 1.0,
        ]
    }

    fn close(a: [f32; 3], b: [f32; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-5)
    }

    #[test]
    fn non_uniform_scale_turns_normals_by_the_inverse_transpose() {
        // A 45° slope stretched 2× along X gets shallower: its normal leans toward +Y.
        let s = std::f32::consts::FRAC_1_SQRT_2;
        let n = transform_normal(&normal_matrix(&diag(2.0, 1.0, 1.0)), &[s, s, 0.0]);
        let len = (0.25f32 + 1.0).sqrt();
        assert!(close(n, [0.5 / len, 1.0 / len, 0.0]), "{n:?}");
    }

    #[test]
    fn mirroring_bakes_keep_normals_outward() {
        for m in [diag(-1.0, 1.0, 1.0), diag(-2.0, 1.0, 1.0)] {
            let n = transform_normal(&normal_matrix(&m), &[1.0, 0.0, 0.0]);
            assert!(close(n, [-1.0, 0.0, 0.0]), "{n:?}");
        }
    }

    #[test]
    fn similarity_bakes_use_the_linear_part_unchanged() {
        // Rotation (90° about Z) and uniform 0.01 scale: the bake's own 3×3, so normals of
        // existing content are bit for bit what they were.
        let m = [
            0.0, 0.01, 0.0, 0.0, -0.01, 0.0, 0.0, 0.0, 0.0, 0.0, 0.01, 0.0, 1.0, 2.0, 3.0, 1.0,
        ];
        let n = normal_matrix(&m);
        assert_eq!(n, [0.0, 0.01, 0.0, -0.01, 0.0, 0.0, 0.0, 0.0, 0.01]);
        assert!(close(mul_mat3(&n, [1.0, 0.0, 0.0]), [0.0, 0.01, 0.0]));
    }
}

#[cfg(test)]
mod uri_tests {
    use super::{decode_gltf_uri, parse_gltf_path};

    fn write_gltf(tag: &str, uri: &str) -> std::path::PathBuf {
        let dir = regia_test_utils::unique_temp_dir(tag);
        let path = dir.join("m.gltf");
        let json = format!(r#"{{"asset":{{"version":"2.0"}},"buffers":[{{"uri":"{uri}","byteLength":4}}]}}"#);
        std::fs::write(&path, json).expect("write gltf");
        path
    }

    #[test]
    fn decode_handles_percent_escapes_and_rejects_invalid_utf8() {
        assert_eq!(decode_gltf_uri("tex%20a.png").as_deref(), Some("tex a.png"));
        assert_eq!(decode_gltf_uri("caf%C3%A9.png").as_deref(), Some("café.png"));
        assert_eq!(decode_gltf_uri("a%ZZb%2").as_deref(), Some("a%ZZb%2"));
        assert_eq!(decode_gltf_uri("tex%FF.png"), None);
    }

    #[test]
    fn invalid_utf8_uri_is_an_error_not_a_panic() {
        let path = write_gltf("gltf-uri-utf8", "tex%FF.bin");
        let err = parse_gltf_path(&path).expect_err("must reject");
        assert!(err.to_string().contains("percent-decode"), "{err}");
    }

    #[test]
    fn backslash_uri_is_rejected_on_every_host() {
        let path = write_gltf("gltf-uri-bslash", r"sub\\a.bin");
        let err = parse_gltf_path(&path).expect_err("must reject");
        assert!(err.to_string().contains("backslash"), "{err}");
    }
}

#[cfg(test)]
mod shared_image_usage_tests {
    use super::*;

    fn usages(json: &str) -> HashMap<usize, TextureCookUsage> {
        let gltf = gltf::Gltf::from_slice(json.as_bytes()).expect("gltf");
        let extensions = GltfExtensionTable::from_bytes(json.as_bytes(), &mut GltfExtensionRegistry::new());
        texture_usages_for_document(&gltf.document, &extensions)
    }

    /// Audit wave 5 (render M2): one image that is both the occlusion map and the iridescence
    /// thickness map cooks with every channel (the thickness is in G), not as a one-channel
    /// mask; an occlusion map no extension shares stays a mask.
    #[test]
    fn an_occlusion_image_shared_with_the_film_thickness_keeps_its_channels() {
        let shared = r#"{
            "asset": { "version": "2.0" },
            "extensionsUsed": ["KHR_materials_iridescence"],
            "images": [{ "uri": "orm.png" }, { "uri": "ao.png" }],
            "textures": [{ "source": 0 }, { "source": 1 }],
            "materials": [
                {
                    "occlusionTexture": { "index": 0 },
                    "extensions": { "KHR_materials_iridescence": {
                        "iridescenceFactor": 1.0,
                        "iridescenceThicknessTexture": { "index": 0 }
                    } }
                },
                { "occlusionTexture": { "index": 1 } }
            ]
        }"#;
        let map = usages(shared);
        assert_eq!(map.get(&0), Some(&TextureCookUsage::ColorLinear));
        assert_eq!(map.get(&1), Some(&TextureCookUsage::Mask));
    }
}

#[cfg(test)]
mod open_checked_tests {
    use super::open_gltf_checked;

    const B64_12: &str = "data:application/octet-stream;base64,AAAAAAAAAAAAAAAA";

    fn open(tag: &str, json: &str) -> Result<(), String> {
        let dir = regia_test_utils::unique_temp_dir(tag);
        let path = dir.join("m.gltf");
        std::fs::write(&path, json).expect("write gltf");
        open_gltf_checked(&path).map(|_| ())
    }

    #[test]
    fn rotation_output_of_u32_is_an_error_not_unreachable() {
        // Sampler output accessor is VEC4 of UNSIGNED_INT (5125); gltf 1.4 hits `unreachable!`.
        let json = format!(
            r#"{{"asset":{{"version":"2.0"}},"nodes":[{{}}],"scenes":[{{"nodes":[0]}}],
            "buffers":[{{"uri":"{B64_12}","byteLength":12}}],
            "bufferViews":[{{"buffer":0,"byteLength":12}}],
            "accessors":[{{"bufferView":0,"componentType":5126,"count":1,"type":"SCALAR"}},
                         {{"bufferView":0,"componentType":5125,"count":0,"type":"VEC4"}}],
            "animations":[{{"samplers":[{{"input":0,"output":1}}],
              "channels":[{{"sampler":0,"target":{{"node":0,"path":"rotation"}}}}]}}]}}"#
        );
        let err = open("gltf-rot-u32", &json).expect_err("must reject");
        assert!(
            err.contains("component type") || err.contains("reaches past"),
            "{err}"
        );
    }

    #[test]
    fn stride_below_element_size_is_an_error() {
        let json = format!(
            r#"{{"asset":{{"version":"2.0"}},
            "buffers":[{{"uri":"{B64_12}","byteLength":12}}],
            "bufferViews":[{{"buffer":0,"byteLength":12,"byteStride":4}}],
            "accessors":[{{"bufferView":0,"componentType":5126,"count":1,"type":"VEC3"}}]}}"#
        );
        let err = open("gltf-stride", &json).expect_err("must reject");
        assert!(err.contains("byteStride"), "{err}");
    }

    #[test]
    fn hostile_images_indices_and_headers_are_errors() {
        let image = r#"{"asset":{"version":"2.0"},"images":[{}]}"#;
        assert!(open("gltf-img", image).is_err());
        let position =
            r#"{"asset":{"version":"2.0"},"meshes":[{"primitives":[{"attributes":{"POSITION":0}}]}]}"#;
        assert!(open("gltf-pos", position).is_err());
        let node = r#"{"asset":{"version":"2.0"},"buffers":[{"uri":"data:application/octet-stream;base64,AAAAAAAAAAAAAAAA","byteLength":12}],
            "bufferViews":[{"buffer":0,"byteLength":12}],
            "accessors":[{"bufferView":0,"componentType":5126,"count":1,"type":"SCALAR"},{"bufferView":0,"componentType":5126,"count":1,"type":"VEC3"}],
            "animations":[{"samplers":[{"input":0,"output":1}],"channels":[{"sampler":0,"target":{"node":0,"path":"translation"}}]}]}"#;
        assert!(open("gltf-node", node).is_err());
        let uri = r#"{"asset":{"version":"2.0"},"buffers":[{"uri":"%80","byteLength":4}]}"#;
        assert!(open("gltf-uri", uri).is_err());
        let dir = regia_test_utils::unique_temp_dir("gltf-glb0");
        let path = dir.join("m.glb");
        let mut glb = b"glTF".to_vec();
        glb.extend(2u32.to_le_bytes());
        glb.extend(0u32.to_le_bytes());
        std::fs::write(&path, glb).expect("write glb");
        assert!(open_gltf_checked(&path).is_err());
    }
}

#[cfg(test)]
mod skin_joint_range_tests {
    use super::{Forward, ImportError, TextureCookQuality, import_gltf_glb_bytes};
    use regia_asset::AssetId;

    /// Audit asset_pipeline1 #5 claimed a skin of 65537 joints cooks a weighted slot whose
    /// skeleton id is 65536 as joint 0 (`joint as u16` in `read_skin`). It does not cook: the
    /// skin's joint names are the whole layout, and `CpuSkin::validate` refuses more than 65536
    /// joints, so every skeleton id that reaches a cooked mesh fits in `u16`. This pins that.
    #[test]
    fn a_skin_past_65536_joints_is_refused_not_cooked_with_a_wrapped_joint() {
        const JOINTS: usize = 65_537;
        // Slots 1..=65536 are roots; slot 0 is the child of slot 65536, so its skeleton id is 65536.
        let mut nodes: Vec<String> = (0..JOINTS - 1).map(|i| format!(r#"{{"name":"j{i}"}}"#)).collect();
        nodes.push(format!(r#"{{"name":"j{}","children":[0]}}"#, JOINTS - 1));
        nodes.push(r#"{"name":"body","mesh":0,"skin":0}"#.to_owned());
        let joints: Vec<String> = (0..JOINTS).map(|i| i.to_string()).collect();
        let mut bin: Vec<u8> = Vec::new();
        for v in [0.0f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0] {
            bin.extend_from_slice(&v.to_le_bytes()); // 0..36 positions
        }
        for _ in 0..3 {
            bin.extend_from_slice(&[0u8; 8]); // 36..60 JOINTS_0 = slot 0 (u16 x 4)
        }
        for _ in 0..3 {
            for w in [1.0f32, 0.0, 0.0, 0.0] {
                bin.extend_from_slice(&w.to_le_bytes()); // 60..108 WEIGHTS_0
            }
        }
        for i in [0u32, 1, 2] {
            bin.extend_from_slice(&i.to_le_bytes()); // 108..120
        }
        let json = format!(
            r#"{{"asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[{body}]}}],"nodes":[{nodes}],
            "skins":[{{"joints":[{joints}]}}],
            "meshes":[{{"primitives":[{{"attributes":{{"POSITION":0,"JOINTS_0":1,"WEIGHTS_0":2}},"indices":3}}]}}],
            "accessors":[
                {{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0.0,0.0,0.0],"max":[1.0,1.0,0.0]}},
                {{"bufferView":1,"componentType":5123,"count":3,"type":"VEC4"}},
                {{"bufferView":2,"componentType":5126,"count":3,"type":"VEC4"}},
                {{"bufferView":3,"componentType":5125,"count":3,"type":"SCALAR"}}],
            "bufferViews":[
                {{"buffer":0,"byteOffset":0,"byteLength":36}},
                {{"buffer":0,"byteOffset":36,"byteLength":24}},
                {{"buffer":0,"byteOffset":60,"byteLength":48}},
                {{"buffer":0,"byteOffset":108,"byteLength":12}}],
            "buffers":[{{"byteLength":120}}]}}"#,
            body = JOINTS,
            nodes = nodes.join(","),
            joints = joints.join(","),
        );
        let mut json = json.into_bytes();
        while !json.len().is_multiple_of(4) {
            json.push(b' ');
        }
        let mut glb = Vec::new();
        glb.extend_from_slice(b"glTF");
        glb.extend_from_slice(&2u32.to_le_bytes());
        glb.extend_from_slice(&((12 + 8 + json.len() + 8 + bin.len()) as u32).to_le_bytes());
        glb.extend_from_slice(&(json.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"JSON");
        glb.extend_from_slice(&json);
        glb.extend_from_slice(&(bin.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"BIN\0");
        glb.extend_from_slice(&bin);
        match import_gltf_glb_bytes(
            &glb,
            AssetId::new_random(),
            Forward::Keep,
            "linux",
            TextureCookQuality::High,
        ) {
            Err(ImportError::MeshValidation(error)) => {
                assert!(error.to_string().contains("65537 joints"), "{error}");
            }
            Err(other) => panic!("expected the skin's joint-count error, got {other}"),
            Ok(result) => panic!(
                "a {JOINTS}-joint skin cooked ({} sub-assets)",
                result.sub_assets.len()
            ),
        }
    }
}

#[cfg(test)]
mod meta_axis_tests {
    use super::{Forward, TextureCookQuality, import_gltf_path};
    use regia_asset::AssetId;

    /// One triangle whose nose is `(0, 0, 1)`. No indices: the three positions are the triangle.
    fn nose_glb() -> Vec<u8> {
        let json = r#"{"asset":{"version":"2.0"},"scene":0,"scenes":[{"nodes":[0]}],"nodes":[{"mesh":0}],"meshes":[{"primitives":[{"attributes":{"POSITION":0,"TEXCOORD_0":1}}]}],"accessors":[{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0.0,0.0,1.0],"max":[1.0,1.0,1.0]},{"bufferView":1,"componentType":5126,"count":3,"type":"VEC2","min":[0.0,0.0],"max":[1.0,1.0]}],"bufferViews":[{"buffer":0,"byteOffset":0,"byteLength":36},{"buffer":0,"byteOffset":36,"byteLength":24}],"buffers":[{"byteLength":60}]}"#;
        let mut bin = Vec::new();
        for p in [0.0f32, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 1.0, 1.0] {
            bin.extend_from_slice(&p.to_le_bytes());
        }
        for uv in [0.0f32, 0.0, 1.0, 0.0, 0.5, 1.0] {
            bin.extend_from_slice(&uv.to_le_bytes());
        }
        let json_bytes = json.as_bytes();
        let json_pad = (4 - json_bytes.len() % 4) % 4;
        let json_len = json_bytes.len() + json_pad;
        let total = 12 + 8 + json_len + 8 + bin.len();
        let mut out = Vec::new();
        out.extend_from_slice(b"glTF");
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&(total as u32).to_le_bytes());
        out.extend_from_slice(&(json_len as u32).to_le_bytes());
        out.extend_from_slice(b"JSON");
        out.extend_from_slice(json_bytes);
        out.extend(std::iter::repeat_n(b' ', json_pad));
        out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
        out.extend_from_slice(b"BIN\0");
        out.extend_from_slice(&bin);
        out
    }

    fn nose_z(meta: Option<&str>, forward: Forward) -> f32 {
        let dir = regia_test_utils::unique_temp_dir("gltf-meta-axis");
        let path = dir.join("nose.glb");
        std::fs::write(&path, nose_glb()).expect("write glb");
        let imported = import_gltf_path(
            &path,
            AssetId::new_random(),
            forward,
            "linux",
            meta,
            TextureCookQuality::High,
        )
        .expect("import");
        let cpu = imported.primary_mesh.to_cpu_mesh().expect("unpack");
        let nose = cpu
            .positions
            .iter()
            .find(|p| p[0].abs() < 1e-5 && p[1].abs() < 1e-5)
            .copied()
            .expect("the nose vertex is still on the Y axis");
        let _ = std::fs::remove_dir_all(&dir);
        nose[2]
    }

    /// `.meta` `forward: Keep` and `scale: 0.01` bake into the mesh. Omitted keys keep the
    /// caller's axis and scale 1 (`docs/formats/gltf-conventions.md`, specs/12 § 7).
    #[test]
    fn meta_forward_and_scale_bake_into_the_mesh() {
        let kept = nose_z(Some("(forward: Keep, scale: 0.01)"), Forward::Convert);
        assert!((kept - 0.01).abs() < 1e-6, "Keep + 0.01 scale, nose z {kept}");
        let converted = nose_z(Some("(meshlets: false)"), Forward::Convert);
        assert!(
            (converted + 1.0).abs() < 1e-5,
            "omitted forward stays Convert, nose z {converted}"
        );
        let argument = nose_z(None, Forward::Keep);
        assert!(
            (argument - 1.0).abs() < 1e-5,
            "no meta key keeps the Keep argument, nose z {argument}"
        );
        let overridden = nose_z(Some("(forward: Convert)"), Forward::Keep);
        assert!(
            (overridden + 1.0).abs() < 1e-5,
            "an explicit Convert overrides a Keep argument, nose z {overridden}"
        );
    }
}
