// SPDX-License-Identifier: MPL-2.0
//! Cooks `.cloth.ron` and `.soft.ron` sources (spec 55 § 4) into cooked `.cloth.bin` /
//! `.soft.bin` files: resolves the glTF nodes a source names into sim, render and surface
//! meshes, runs the generator for `Tetra` soft bodies without listed tets (`crate::tet_gen`),
//! and hands the rest to `regia_physics::cloth::asset` (constraints, maps, binding, payload).
//!
//! Sim meshes are welded by exact position (glTF splits vertices at UV seams; a cloth must not
//! split there). A node's world transform is applied to its vertices unless the node is skinned
//! (glTF 2.0 § 3.7.3: skinned vertices are in the skin's bind space). The output depends only
//! on the source files and the given asset id, so cooking is byte-reproducible.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_asset::AssetId;
use regia_asset::{COOKED_FORMAT_VERSION, CookedHeader, encode_cooked_file, pack_importer_version};
use regia_core::id::TypePathHash;
use regia_core::math::{Mat4, Vec3};
use regia_physics::cloth::ClothError;
use regia_physics::cloth::asset::{
    COOKED_CLOTH_TYPE_PATH, COOKED_SOFT_TYPE_PATH, ClothAsset, ClothSource, MeshRef, RenderMeshInput,
    SimMeshInput, SoftAsset, SoftModelSource, SoftSource,
};

use crate::tet_gen::{TetGenReport, generate_tets};

/// Importer version written into the cooked header of cloth and soft files.
const CLOTH_IMPORTER_VERSION: (u32, u32, u32) = (1, 0, 0);

/// Why a cloth or soft source failed to cook.
#[derive(Debug, thiserror::Error)]
pub enum ClothCookError {
    /// Reading a file failed.
    #[error("{path}: {source}")]
    Io {
        /// File.
        path: PathBuf,
        /// Cause.
        source: std::io::Error,
    },
    /// A glTF file could not be read, or lacks the named node or primitive.
    #[error("{path}: {reason}")]
    Gltf {
        /// File.
        path: PathBuf,
        /// What is wrong.
        reason: String,
    },
    /// The source or meshes break a cloth rule (the error's `code()` is the protocol reason).
    #[error(transparent)]
    Cloth(#[from] ClothError),
    /// The cooked header could not be written.
    #[error("cooked header: {0}")]
    Header(String),
}

impl ClothCookError {
    /// Stable lowercase code: the cloth error's code, or `cloth_cook_io` / `cloth_cook_gltf` /
    /// `cloth_cook_header`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Io { .. } => "cloth_cook_io",
            Self::Gltf { .. } => "cloth_cook_gltf",
            Self::Cloth(e) => e.code(),
            Self::Header(_) => "cloth_cook_header",
        }
    }
}

fn read(path: &Path) -> Result<Vec<u8>, ClothCookError> {
    std::fs::read(path).map_err(|source| ClothCookError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn gltf_err(path: &Path, reason: impl Into<String>) -> ClothCookError {
    ClothCookError::Gltf {
        path: path.to_path_buf(),
        reason: reason.into(),
    }
}

/// One glTF node's mesh: positions, normals-free triangles, first UV set, first joint and
/// weight sets.
struct NodeMesh {
    primitives: Vec<SimMeshInput>,
}

/// Loads node `node` of the glTF at `path` (world transform applied unless skinned). Its buffer and
/// image URIs must stay under `bound` (the cloth source's [`SourceBound`]).
fn load_node(path: &Path, node: &str, bound: &Path) -> Result<NodeMesh, ClothCookError> {
    let bytes = read(path)?;
    // The checked parse: `from_slice` panics on out-of-range accessor and channel indices.
    // Without the forest check: the cloth walk visits each node once, so its answer for a looping
    // file stays its own (audit F5).
    let (gltf, _) =
        crate::gltf_import::checked_parse_any_nodes(&bytes).map_err(|e| gltf_err(path, e.to_string()))?;
    crate::gltf_import::reject_hostile_uris_within(&gltf.document, path.parent(), bound)
        .map_err(|e| gltf_err(path, e.to_string()))?;
    // The normalized document and buffers: quantized and meshopt-compressed views decoded
    // (ADR-3016), as the glTF importer reads them.
    let (document, buffers) =
        crate::gltf_import::open_gltf_checked_any_nodes(path).map_err(|e| gltf_err(path, e))?;
    // World matrices of every node reachable from a scene (column-major glTF matrices).
    let mut worlds: BTreeMap<usize, Mat4> = BTreeMap::new();
    let mut stack: Vec<(gltf::Node<'_>, Mat4)> = Vec::new();
    for scene in document.scenes() {
        stack.extend(scene.nodes().map(|n| (n, Mat4::IDENTITY)));
    }
    while let Some((n, parent)) = stack.pop() {
        // A glTF node has one parent, but the crate does not reject a cycle: visit each node once.
        if worlds.contains_key(&n.index()) {
            continue;
        }
        let world = parent * Mat4::from_cols_array_2d(&n.transform().matrix());
        worlds.insert(n.index(), world);
        stack.extend(n.children().map(|c| (c, world)));
    }
    let found = document
        .nodes()
        .find(|n| n.name() == Some(node))
        .ok_or_else(|| gltf_err(path, format!("no node named {node:?}")))?;
    let mesh = found
        .mesh()
        .ok_or_else(|| gltf_err(path, format!("node {node:?} has no mesh")))?;
    let world = if found.skin().is_some() {
        Mat4::IDENTITY
    } else {
        worlds
            .get(&found.index())
            .copied()
            .unwrap_or_else(|| Mat4::from_cols_array_2d(&found.transform().matrix()))
    };
    let mut primitives = Vec::new();
    for prim in mesh.primitives() {
        if prim.mode() != gltf::mesh::Mode::Triangles {
            return Err(gltf_err(path, format!("node {node:?}: only triangle primitives")));
        }
        let reader = prim.reader(|b| buffers.get(b.index()).map(|d| &d.0[..]));
        let positions: Vec<Vec3> = reader
            .read_positions()
            .ok_or_else(|| gltf_err(path, format!("node {node:?}: a primitive has no positions")))?
            .map(|p| world.transform_point3(Vec3::from_array(p)))
            .collect();
        let n = positions.len() as u32;
        let indices: Vec<u32> = match reader.read_indices() {
            Some(i) => i.into_u32().collect(),
            None => (0..n).collect(),
        };
        if !indices.len().is_multiple_of(3) {
            return Err(gltf_err(
                path,
                format!("node {node:?}: index count is not a multiple of 3"),
            ));
        }
        let triangles = indices.as_chunks::<3>().0.to_vec();
        let uvs: Vec<[f32; 2]> = reader
            .read_tex_coords(0)
            .map(|t| t.into_f32().collect())
            .unwrap_or_default();
        // A UV list that does not match the positions is dropped (like joints and weights).
        let uvs = if uvs.len() == positions.len() {
            uvs
        } else {
            Vec::new()
        };
        let joints: Vec<[u16; 4]> = reader
            .read_joints(0)
            .map(|j| j.into_u16().collect())
            .unwrap_or_default();
        let weights: Vec<[f32; 4]> = reader
            .read_weights(0)
            .map(|w| w.into_f32().collect())
            .unwrap_or_default();
        let (joints, weights) = if joints.len() == positions.len() && weights.len() == positions.len() {
            (joints, weights)
        } else {
            (Vec::new(), Vec::new())
        };
        primitives.push(SimMeshInput {
            positions,
            triangles,
            joints,
            weights,
            uvs,
        });
    }
    Ok(NodeMesh { primitives })
}

/// Merges the primitives and welds vertices with bit-identical positions (the first
/// occurrence keeps its UV and influences); triangles that collapse are dropped.
fn welded(mesh: NodeMesh) -> SimMeshInput {
    let mut out = SimMeshInput::default();
    let mut index: BTreeMap<[u32; 3], u32> = BTreeMap::new();
    let skinned = mesh.primitives.iter().all(|p| !p.joints.is_empty());
    let textured = mesh.primitives.iter().any(|p| !p.uvs.is_empty());
    for prim in &mesh.primitives {
        let remap: Vec<u32> = prim
            .positions
            .iter()
            .enumerate()
            .map(|(i, p)| {
                *index.entry(p.to_array().map(f32::to_bits)).or_insert_with(|| {
                    out.positions.push(*p);
                    if skinned {
                        out.joints.push(prim.joints[i]);
                        out.weights.push(prim.weights[i]);
                    }
                    if textured {
                        out.uvs.push(prim.uvs.get(i).copied().unwrap_or([0.0, 0.0]));
                    }
                    (out.positions.len() - 1) as u32
                })
            })
            .collect();
        for t in &prim.triangles {
            let Some(t) = t
                .iter()
                .map(|&i| remap.get(i as usize).copied())
                .collect::<Option<Vec<u32>>>()
            else {
                continue;
            };
            if t[0] != t[1] && t[1] != t[2] && t[0] != t[2] {
                out.triangles.push([t[0], t[1], t[2]]);
            }
        }
    }
    out
}

/// Where a cloth or soft source sits and what its references must stay under: the project around
/// it, or, for a source cooked outside any project, its own folder (finding 19, ADR-4922). The
/// glTF files it names and every buffer and image those name are confined to the bound, through
/// symlinks too.
struct SourceBound {
    /// The source's folder (references are relative to it).
    base: PathBuf,
    /// What every resolved reference must stay under.
    bound: PathBuf,
}

impl SourceBound {
    fn of(source_path: &Path) -> Result<Self, ClothCookError> {
        let base = source_path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let bound = crate::cook::reference_bound(&base).ok_or_else(|| ClothCookError::Io {
            path: source_path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "the source's folder does not resolve",
            ),
        })?;
        Ok(Self { base, bound })
    }

    /// The glTF a mesh reference names: `\\` is a separator on every host and the path stays
    /// inside the bound, like every other importer's references (audit F9).
    fn mesh_path(&self, source: &str) -> Result<PathBuf, ClothCookError> {
        crate::cook::join_bounded(&self.base, source, Some(&self.bound))
            .map_err(|e| gltf_err(&self.base.join(source), e))
    }

    fn sim_mesh(&self, mesh: &MeshRef) -> Result<SimMeshInput, ClothCookError> {
        Ok(welded(load_node(
            &self.mesh_path(&mesh.source)?,
            &mesh.node,
            &self.bound,
        )?))
    }
}

/// Cooks a `.cloth.ron` into a [`ClothAsset`] (meshes resolved relative to the source).
///
/// # Errors
/// [`ClothCookError`]: IO, glTF, or a cloth rule (`cloth_asset_invalid`,
/// `cloth_map_size_mismatch`, `cloth_unsupported_version`, ...).
pub fn cook_cloth(source_path: &Path) -> Result<ClothAsset, ClothCookError> {
    let text = String::from_utf8(read(source_path)?).map_err(|_| ClothError::AssetInvalid {
        reason: "source is not UTF-8".into(),
    })?;
    let source = ClothSource::from_ron_str(&text)?;
    let refs = SourceBound::of(source_path)?;
    let lod0 = refs.sim_mesh(&source.sim_mesh)?;
    let render = match &source.render_mesh {
        Some(r) => {
            let path = refs.mesh_path(&r.source)?;
            let node = load_node(&path, &r.node, &refs.bound)?;
            let prim = node
                .primitives
                .into_iter()
                .nth(r.section as usize)
                .ok_or_else(|| {
                    gltf_err(&path, format!("node {:?} has no primitive {}", r.node, r.section))
                })?;
            Some(RenderMeshInput {
                positions: prim.positions,
                uvs: prim.uvs,
                triangles: prim.triangles,
            })
        }
        None => None,
    };
    let lods = source
        .lods
        .iter()
        .map(|l| l.sim_mesh.as_ref().map(|m| refs.sim_mesh(m)).transpose())
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ClothAsset::cook(&source, &lod0, render.as_ref(), &lods)?)
}

fn wrap(type_path: &str, asset_id: AssetId, payload: Vec<u8>) -> Result<Vec<u8>, ClothCookError> {
    let (major, minor, patch) = CLOTH_IMPORTER_VERSION;
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: TypePathHash::of(type_path),
        asset_id,
        importer_version: pack_importer_version(major, minor, patch),
        dependencies: Vec::new(),
        payload_len: payload.len() as u64,
    };
    encode_cooked_file(&header, &payload).map_err(|e| ClothCookError::Header(e.to_string()))
}

/// Cooks a `.cloth.ron` into the bytes of a `.cloth.bin` (`FCKD` header with `asset_id`).
///
/// # Errors
/// As [`cook_cloth`].
pub fn cook_cloth_bytes(source_path: &Path, asset_id: AssetId) -> Result<Vec<u8>, ClothCookError> {
    let asset = cook_cloth(source_path)?;
    wrap(COOKED_CLOTH_TYPE_PATH, asset_id, asset.to_bytes()?)
}

/// Cooks a `.soft.ron` into a [`SoftAsset`]; a `Tetra` model without tets runs the tet
/// generator at its `cell` size and returns the generator's report.
///
/// # Errors
/// As [`cook_cloth`], plus `soft_not_closed`, `soft_inverted_rest`, `soft_bad_cluster` and
/// `cloth_bad_param` (a `Tetra` model with neither tets nor a cell).
pub fn cook_soft(source_path: &Path) -> Result<(SoftAsset, Option<TetGenReport>), ClothCookError> {
    let text = String::from_utf8(read(source_path)?).map_err(|_| ClothError::AssetInvalid {
        reason: "source is not UTF-8".into(),
    })?;
    let source = SoftSource::from_ron_str(&text)?;
    let surface = SourceBound::of(source_path)?.sim_mesh(&source.surface)?;
    let (tets, report) = match &source.model {
        SoftModelSource::Tetra { tets, cell, .. } if tets.is_empty() => {
            let cell = cell.ok_or(ClothError::BadParam { what: "cell" })?;
            let (mesh, report) = generate_tets(&surface.positions, &surface.triangles, cell)?;
            (Some(mesh), Some(report))
        }
        _ => (None, None),
    };
    Ok((SoftAsset::cook(&source, &surface, tets.as_ref())?, report))
}

/// Cooks a `.soft.ron` into the bytes of a `.soft.bin`.
///
/// # Errors
/// As [`cook_soft`].
pub fn cook_soft_bytes(source_path: &Path, asset_id: AssetId) -> Result<Vec<u8>, ClothCookError> {
    let (asset, _) = cook_soft(source_path)?;
    wrap(COOKED_SOFT_TYPE_PATH, asset_id, asset.to_bytes()?)
}

/// Cooks a `.cloth.ron` for `cook_project`: the cooked payload, keyed on its own hash so an
/// edit to a referenced glTF recooks (`cook_project` keys every other source on its own bytes).
pub(crate) fn cook_cloth_payload(source_path: &Path) -> Result<Vec<u8>, ClothCookError> {
    Ok(cook_cloth(source_path)?.to_bytes()?)
}

/// As [`cook_cloth_payload`], for a `.soft.ron`.
pub(crate) fn cook_soft_payload(source_path: &Path) -> Result<Vec<u8>, ClothCookError> {
    Ok(cook_soft(source_path)?.0.to_bytes()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gltf_write::{GlbMesh, GlbNode, GlbPrimitive, GlbScene};

    /// audit F5: a node that is its own ancestor ended the load in an endless walk.
    #[test]
    fn a_node_cycle_is_walked_once() {
        let dir = regia_test_utils::unique_temp_dir("cloth-gltf-cycle");
        let path = dir.join("loop.gltf");
        std::fs::write(
            &path,
            r#"{"asset":{"version":"2.0"},"scene":0,"scenes":[{"nodes":[0]}],
               "nodes":[{"name":"a","children":[1,1]},{"name":"b","children":[0,1]}]}"#,
        )
        .expect("gltf");
        let err = load_node(&path, "a", &dir).err().expect("a has no mesh");
        assert!(err.to_string().contains("has no mesh"), "{err}");
    }

    /// audit F9: `\\` separates folders on every host and the reference stays in the project.
    #[test]
    fn mesh_paths_take_backslashes_and_stay_inside() {
        let dir = regia_test_utils::unique_temp_dir("cloth-mesh-path");
        let refs = SourceBound::of(&dir.join("a.cloth.ron")).expect("bound");
        assert_eq!(
            refs.mesh_path(r"meshes\a.glb").ok(),
            Some(dir.join("meshes/a.glb"))
        );
        assert!(refs.mesh_path("/etc/a.glb").is_err());
        assert!(refs.mesh_path(r"C:\a.glb").is_err());
        assert!(refs.mesh_path("../a.glb").is_err());
        assert!(refs.mesh_path("meshes/nul.glb").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A GLB with node `Sim`: a 4 x 4 sheet split along its middle column (UV seam: duplicated
    /// positions), translated by (0, 2, 0); and node `Box`: a closed 0.4 m cube.
    fn fixture_glb() -> Vec<u8> {
        fixture_glb_at(2.0)
    }

    fn fixture_glb_at(y: f32) -> Vec<u8> {
        let mut sheet = GlbPrimitive::default();
        let mut uvs = Vec::new();
        // Two halves, each 2 columns x 4 rows sharing x = 0.1 (the seam).
        for half in 0..2 {
            let base = sheet.positions.len() as u32;
            for r in 0..4 {
                for c in 0..2 {
                    let x = (half + c) as f32 * 0.1;
                    sheet.positions.push([x, -(r as f32) * 0.1, 0.0]);
                    uvs.push([half as f32 * 0.5 + c as f32 * 0.25, r as f32 / 3.0]);
                }
            }
            for r in 0..3 {
                let i = base + r * 2;
                sheet
                    .indices
                    .extend_from_slice(&[i, i + 2, i + 1, i + 1, i + 2, i + 3]);
            }
        }
        sheet.uvs = vec![uvs];
        let (bp, bt) = crate::tet_gen::tests_support::box_surface(Vec3::ZERO, Vec3::splat(0.4));
        let cube = GlbPrimitive {
            positions: bp.iter().map(|p| p.to_array()).collect(),
            indices: bt.iter().flatten().copied().collect(),
            ..GlbPrimitive::default()
        };
        GlbScene {
            generator: "cloth test".into(),
            nodes: vec![
                GlbNode {
                    name: "Sim".into(),
                    translation: [0.0, y, 0.0],
                    mesh: Some(0),
                    ..GlbNode::default()
                },
                GlbNode {
                    name: "Box".into(),
                    mesh: Some(1),
                    ..GlbNode::default()
                },
            ],
            roots: vec![0, 1],
            meshes: vec![
                GlbMesh {
                    name: "Sheet".into(),
                    primitives: vec![sheet],
                    ..GlbMesh::default()
                },
                GlbMesh {
                    name: "Cube".into(),
                    primitives: vec![cube],
                    ..GlbMesh::default()
                },
            ],
            ..GlbScene::default()
        }
        .to_glb()
        .expect("writes")
    }

    /// A one-node GLB whose primitives are triangles with the given UV lists.
    fn uv_glb(uvs: &[Option<Vec<[f32; 2]>>]) -> Vec<u8> {
        let primitives = uvs
            .iter()
            .enumerate()
            .map(|(k, uv)| {
                let x = k as f32;
                GlbPrimitive {
                    positions: vec![[x, 0.0, 0.0], [x, 1.0, 0.0], [x, 0.0, 1.0]],
                    indices: vec![0, 1, 2],
                    uvs: uv.iter().cloned().collect(),
                    ..GlbPrimitive::default()
                }
            })
            .collect();
        GlbScene {
            generator: "cloth uv test".into(),
            nodes: vec![GlbNode {
                name: "Sim".into(),
                mesh: Some(0),
                ..GlbNode::default()
            }],
            roots: vec![0],
            meshes: vec![GlbMesh {
                name: "M".into(),
                primitives,
                ..GlbMesh::default()
            }],
            ..GlbScene::default()
        }
        .to_glb()
        .expect("writes")
    }

    fn weld_uv_glb(uvs: &[Option<Vec<[f32; 2]>>]) -> SimMeshInput {
        let dir = regia_test_utils::unique_temp_dir("cloth_cook_uv");
        let path = dir.join("uv.glb");
        std::fs::write(&path, uv_glb(uvs)).expect("write glb");
        let mesh = load_node(&path, "Sim", &dir).expect("loads");
        welded(mesh)
    }

    #[test]
    fn short_uv_list_is_dropped_instead_of_indexing_out_of_range() {
        // 3 positions but one UV: used to panic at the second vertex.
        let out = weld_uv_glb(&[Some(vec![[0.5, 0.5]])]);
        assert_eq!(out.positions.len(), 3);
        assert!(out.uvs.is_empty());
    }

    #[test]
    fn primitive_without_uvs_does_not_drop_the_uvs_of_the_others() {
        let out = weld_uv_glb(&[Some(vec![[0.1, 0.2], [0.3, 0.4], [0.5, 0.6]]), None]);
        assert_eq!(out.positions.len(), 6);
        assert_eq!(out.uvs.len(), 6);
        assert_eq!(out.uvs[1], [0.3, 0.4]);
        assert_eq!(out.uvs[4], [0.0, 0.0]);
    }

    fn fixture_dir() -> PathBuf {
        let dir = regia_test_utils::unique_temp_dir("cloth_cook");
        std::fs::write(dir.join("fixture.glb"), fixture_glb()).expect("write glb");
        dir
    }

    #[test]
    fn cloth_source_cooks_from_gltf_welds_seams_and_is_reproducible() {
        let dir = fixture_dir();
        let path = dir.join("flag.cloth.ron");
        std::fs::write(
            &path,
            r#"(format_version: 1, name: "flag", sim_mesh: (source: "fixture.glb", node: "Sim"),
                maps: { "reach": Sparse(default: 1.0, entries: [(0, 0.0)]) })"#,
        )
        .expect("write source");
        let asset = cook_cloth(&path).expect("cooks");
        let lod = asset.lod0();
        // 16 positions, 4 on the seam duplicated -> 12 welded vertices, 12 triangles.
        assert_eq!(lod.rest_positions.len(), 12);
        assert_eq!(lod.triangles.len(), 12);
        assert!(
            (lod.rest_positions[0].y - 2.0).abs() < 1e-6,
            "node transform applied"
        );
        let id = AssetId::sub_asset(AssetId::NIL, "cloth/flag");
        let a = cook_cloth_bytes(&path, id).expect("bytes");
        let b = cook_cloth_bytes(&path, id).expect("bytes");
        assert_eq!(a, b, "byte-reproducible");
        assert_eq!(ClothAsset::from_cooked_file(&a).expect("loads"), asset);
        let wrong = cook_soft_bytes(&path, id);
        assert!(wrong.is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cook_project_cooks_sources_and_recooks_when_a_referenced_gltf_changes() {
        let root = regia_test_utils::unique_temp_dir("cloth_cook_project");
        let assets = root.join("assets");
        std::fs::create_dir_all(&assets).expect("assets");
        std::fs::write(assets.join("fixture.glb"), fixture_glb()).expect("glb");
        std::fs::write(
            assets.join("flag.cloth.ron"),
            r#"(format_version: 1, name: "flag", sim_mesh: (source: "fixture.glb", node: "Sim"))"#,
        )
        .expect("cloth");
        std::fs::write(
            assets.join("block.soft.ron"),
            r#"(format_version: 1, name: "block", model: Tetra(cell: Some(0.1), volume_compliance: 0.0),
                surface: (source: "fixture.glb", node: "Box"))"#,
        )
        .expect("soft");
        let cook = || {
            let report = crate::cook_project(crate::CookOptions {
                project_root: root.clone(),
                platform: "linux".to_string(),
                incremental: true,
            })
            .expect("cook");
            assert!(report.errors.is_empty(), "{:?}", report.errors);
        };
        let open = || {
            regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
                project_root: root.clone(),
                platform: "linux".to_string(),
                content_dir: None,
            })
            .expect("open")
        };
        cook();
        let mut server = open();
        let id = server
            .id_for_path("assets/flag.cloth.ron")
            .expect("in the manifest");
        let (first, _) = server.read_cooked(id).expect("cooked");
        let asset = ClothAsset::from_cooked_file(&first).expect("loads");
        assert!((asset.lod0().rest_positions[0].y - 2.0).abs() < 1e-6);
        let soft = server
            .id_for_path("assets/block.soft.ron")
            .expect("in the manifest");
        let (bytes, _) = server.read_cooked(soft).expect("cooked");
        assert_eq!(SoftAsset::from_cooked_file(&bytes).expect("loads").name, "block");

        // Nothing changed: the same bytes. A referenced glTF changed: a new cook of the source,
        // although the `.cloth.ron` itself did not change.
        cook();
        let (again, _) = open().read_cooked(id).expect("cooked");
        assert_eq!(first, again);
        std::fs::write(assets.join("fixture.glb"), fixture_glb_at(3.0)).expect("glb");
        cook();
        let (edited, _) = open().read_cooked(id).expect("cooked");
        let asset = ClothAsset::from_cooked_file(&edited).expect("loads");
        assert!(
            (asset.lod0().rest_positions[0].y - 3.0).abs() < 1e-6,
            "recooked from the new glTF"
        );

        // A bad source is a per-file cook error with its code.
        std::fs::write(assets.join("bad.cloth.ron"), "(format_version: 9, name: \"x\")").expect("bad");
        let report = crate::cook_project(crate::CookOptions {
            project_root: root.clone(),
            platform: "linux".to_string(),
            incremental: true,
        })
        .expect("cook runs");
        let (path, message) = report.errors.first().expect("the bad source");
        assert!(path.ends_with("bad.cloth.ron"), "{path}");
        assert!(message.contains("cloth_"), "{message}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn missing_nodes_and_files_report_codes() {
        let dir = fixture_dir();
        let path = dir.join("bad.cloth.ron");
        std::fs::write(
            &path,
            r#"(format_version: 1, name: "bad", sim_mesh: (source: "fixture.glb", node: "Nope"))"#,
        )
        .expect("write");
        assert_eq!(cook_cloth(&path).expect_err("node").code(), "cloth_cook_gltf");
        std::fs::write(
            &path,
            r#"(format_version: 1, name: "bad", sim_mesh: (source: "missing.glb", node: "Sim"))"#,
        )
        .expect("write");
        assert_eq!(cook_cloth(&path).expect_err("file").code(), "cloth_cook_io");
        assert_eq!(
            cook_cloth(&dir.join("absent.cloth.ron")).expect_err("src").code(),
            "cloth_cook_io"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn tetra_soft_body_generates_tets_and_loads() {
        let dir = fixture_dir();
        let path = dir.join("block.soft.ron");
        std::fs::write(
            &path,
            r#"(format_version: 1, name: "block", model: Tetra(cell: Some(0.1), volume_compliance: 0.0),
                surface: (source: "fixture.glb", node: "Box"))"#,
        )
        .expect("write");
        let (asset, report) = cook_soft(&path).expect("cooks");
        let report = report.expect("generated");
        assert_eq!(report.tets, 320);
        assert_eq!(asset.surface_binding.len(), 8);
        let id = AssetId::sub_asset(AssetId::NIL, "soft/block");
        let bytes = cook_soft_bytes(&path, id).expect("bytes");
        let loaded = SoftAsset::from_cooked_file(&bytes).expect("loads");
        assert_eq!(loaded, asset);
        let solver = loaded
            .build_solver(1.0 / 60.0, regia_core::math::Affine3A::IDENTITY)
            .expect("solver");
        assert!((solver.rest_volume() - 0.064).abs() < 1e-4);
        let _ = std::fs::remove_dir_all(dir);
    }
}
