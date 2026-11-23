// SPDX-License-Identifier: MPL-2.0
//! `.matgraph.ron` and `.matinst.ron` → [`regia_matgraph::CookedMaterialGraph`].
//!
//! The cook resolves an instance's parent and every `.matfunc.ron` the graph calls, compiles
//! the permutation (the instance's static switches, or the graph's defaults), validates the
//! generated code in every host shader, and stores the code with the parameter block and the
//! texture bindings. Each texture is cooked once more as an RGBA8 sub-asset of its image
//! (`<image path>#rgba8`, shared by every graph that samples it), because the forward pass's
//! material slots upload RGBA8 with their own mip chain. A shipped game therefore loads a
//! graph without compiling it. The cache key covers the parent, the function files and the
//! images, so editing any of them recooks the graph.

use std::path::Path;

use regia_asset::{
    AssetId, COOKED_FORMAT_VERSION, CookedHeader, ManifestEntry, cooked_file_name, encode_cooked_file,
    pack_importer_version,
};
use regia_matgraph::{
    COOKED_MATGRAPH_TYPE_PATH, CookedMaterialGraph, MATGRAPH_FORMAT_VERSION, MATINST_FORMAT_VERSION,
    MaterialGraph, MaterialInstance,
};

use crate::cook::CookError;
use crate::db::AssetDatabase;

/// Sub-asset path suffix of a graph texture's RGBA8 cook.
pub(crate) const RGBA8_SUFFIX: &str = "rgba8";

/// True for a material function file (cooked as part of the graphs that call it).
pub(crate) fn is_material_function(source: &Path) -> bool {
    source
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| crate::cook::name_ends_with(n, ".matfunc.ron"))
}

/// What [`cook_matgraph`] produced.
pub(crate) struct MatgraphCook {
    pub importer: String,
    pub settings_hash: [u8; 32],
    pub id: AssetId,
    pub payload: Vec<u8>,
    pub sub_manifest: Vec<ManifestEntry>,
}

fn read_text(project_root: &Path, path: &str) -> Result<String, CookError> {
    let full = crate::cook::join_in_project(project_root, path)
        .map_err(|e| CookError::MaterialGraph(format!("{path}: {e}")))?;
    std::fs::read_to_string(full).map_err(|e| CookError::MaterialGraph(format!("{path}: {e}")))
}

/// Cooks one graph (`instance == false`) or instance source.
pub(crate) fn cook_matgraph(
    source: &Path,
    bytes: &[u8],
    project_root: &Path,
    cache_dir: &Path,
    instance: bool,
) -> Result<MatgraphCook, CookError> {
    let importer = if instance { "matinst_ron" } else { "matgraph_ron" };
    let meta = AssetDatabase::load_or_create_meta(source, importer).map_err(CookError::Db)?;
    let text = std::str::from_utf8(bytes).map_err(|e| CookError::MaterialGraph(format!("not UTF-8: {e}")))?;
    // Everything the cooked bytes depend on besides the source itself.
    let mut deps = blake3::Hasher::new();
    deps.update(&MATGRAPH_FORMAT_VERSION.to_le_bytes());
    deps.update(&MATINST_FORMAT_VERSION.to_le_bytes());
    deps.update(&regia_matgraph::COOKED_MATGRAPH_VERSION.to_le_bytes());
    let (mut graph, inst) = if instance {
        let inst = MaterialInstance::from_ron(text).map_err(|e| CookError::MaterialGraph(e.to_string()))?;
        let parent = read_text(project_root, &inst.parent)?;
        deps.update(parent.as_bytes());
        let graph = MaterialGraph::from_ron(&parent)
            .map_err(|e| CookError::MaterialGraph(format!("{}: {e}", inst.parent)))?;
        (graph, Some(inst))
    } else {
        let graph = MaterialGraph::from_ron(text).map_err(|e| CookError::MaterialGraph(e.to_string()))?;
        (graph, None)
    };
    graph
        .resolve_function_files(&mut |path| {
            let full = crate::cook::join_in_project(project_root, path)?;
            let text = std::fs::read_to_string(full).map_err(|e| e.to_string())?;
            deps.update(text.as_bytes());
            Ok(text)
        })
        .map_err(|e| CookError::MaterialGraph(e.to_string()))?;
    // Code nodes' `#include` files (ADR-7852): a missing one fails the compile on its line.
    graph.resolve_code_includes(&mut |name| {
        let path = regia_matgraph::code::include_path(project_root, name)
            .ok_or_else(|| format!("bad include name `{name}`"))?;
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        deps.update(name.as_bytes());
        deps.update(text.as_bytes());
        Ok(text)
    });
    cook_graph(
        graph,
        inst.as_ref(),
        importer,
        meta.id,
        deps,
        project_root,
        cache_dir,
    )
}

/// Cooks a loaded graph (`instance` overrides it): compiles and validates the permutation and
/// cooks every texture it samples as an RGBA8 sub-asset. `deps` already hashes everything the
/// graph was built from besides the source bytes; the texture bytes and ids are added here. The
/// `.mtlx` importer cooks its result through this too.
pub(crate) fn cook_graph(
    graph: MaterialGraph,
    instance: Option<&MaterialInstance>,
    importer: &str,
    id: AssetId,
    mut deps: blake3::Hasher,
    project_root: &Path,
    cache_dir: &Path,
) -> Result<MatgraphCook, CookError> {
    let mut sub_manifest = Vec::new();
    let mut failure = None;
    let cooked = CookedMaterialGraph::cook(&graph, instance, &mut |path| match cook_rgba8(
        project_root,
        cache_dir,
        path,
        &mut deps,
    ) {
        Ok(entry) => {
            let id = entry.id;
            if !sub_manifest.iter().any(|e: &ManifestEntry| e.id == id) {
                sub_manifest.push(entry);
            }
            Some(id)
        }
        Err(err) => {
            failure.get_or_insert(err);
            None
        }
    })
    .map_err(|e| CookError::MaterialGraph(e.to_string()))?;
    if let Some(err) = failure {
        return Err(err);
    }
    let payload = cooked
        .to_postcard()
        .map_err(|e| CookError::MaterialGraph(e.to_string()))?;
    Ok(MatgraphCook {
        importer: importer.to_string(),
        settings_hash: *deps.finalize().as_bytes(),
        id,
        payload,
        sub_manifest,
    })
}

/// Cooks `path` (a project-relative image) as an RGBA8 sub-asset of the image.
fn cook_rgba8(
    project_root: &Path,
    cache_dir: &Path,
    path: &str,
    deps: &mut blake3::Hasher,
) -> Result<ManifestEntry, CookError> {
    let source = crate::cook::join_in_project(project_root, path)
        .map_err(|e| CookError::MaterialGraph(format!("texture {path}: {e}")))?;
    let bytes =
        std::fs::read(&source).map_err(|e| CookError::MaterialGraph(format!("texture {path}: {e}")))?;
    deps.update(blake3::hash(&bytes).as_bytes());
    let ext = source
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("png")
        .to_ascii_lowercase();
    let image_meta = AssetDatabase::load_or_create_meta(&source, &ext).map_err(CookError::Db)?;
    let id = AssetId::sub_asset(image_meta.id, RGBA8_SUFFIX);
    // The cooked graph embeds this id: a regenerated `.meta` (new id) must change the key, or
    // the graph would keep naming the dead id (audit-v035-cook L5).
    deps.update(id.to_string().as_bytes());
    let image = crate::import_png_rgba8(&bytes).map_err(CookError::Import)?;
    let payload = image.to_postcard().map_err(CookError::Postcard)?;
    let type_path = "regia_asset::CookedImage";
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: regia_core::id::TypePathHash::of(type_path),
        asset_id: id,
        importer_version: pack_importer_version(1, 0, 0),
        dependencies: vec![],
        payload_len: payload.len() as u64,
    };
    let file = encode_cooked_file(&header, &payload).map_err(CookError::Cooked)?;
    crate::cook_ledger::write_atomic(&cache_dir.join(cooked_file_name(id)), &file)?;
    Ok(ManifestEntry {
        path: format!("{path}#{RGBA8_SUFFIX}"),
        id,
        type_path: type_path.to_string(),
    })
}

/// The cooked type path (manifest and header).
pub(crate) fn type_path() -> String {
    COOKED_MATGRAPH_TYPE_PATH.to_string()
}
