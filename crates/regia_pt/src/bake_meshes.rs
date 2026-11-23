// SPDX-License-Identifier: MPL-2.0
//! Static meshes in a GI bake (`.bake.ron` v1 `meshes`, ADR-279): loading their sources, placing
//! them in a [`PtScene`] as occluders and bounce surfaces, the DO27 bundle contracts
//! (`static_meshes.json` version 1, `interiors.json` `do27-bake-interiors/1`), the bundle's
//! `MANIFEST.sha256` check, and a terrain heightfield rasterized from a terrain grid mesh.
//!
//! `regia_pt` reads cooked meshes (`.fckd`, [`CookedMeshLoader`]) itself; glTF sources need the
//! asset pipeline, which a tools crate below the CLI may not depend on, so `regia bake-gi` hands
//! [`crate::bake_desc::GiBakeDescription::build_with`] its own [`BakeMeshLoader`].

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use regia_core::glam::{Mat4, Vec3};
use serde::Serialize;

use crate::bake_desc::{BakeDescError, InteriorDesc, MaterialOverride, MeshDesc};
use crate::scene::{PtHeightfield, PtMaterial, PtMesh, PtScene};

/// Format string of the DO27 interiors file.
pub const INTERIORS_FORMAT: &str = "do27-bake-interiors/1";

/// Layers a bake never places: spawned at match time or moved by the simulation.
pub const UNBAKED_LAYERS: [&str; 2] = ["structure", "dynamic"];

/// One primitive of a loaded mesh source: geometry in the source's frame and its material.
#[derive(Clone, Debug)]
pub struct BakeMeshPart {
    /// Geometry (positions, normals, indices; UVs and tangents are not needed by the bake).
    pub mesh: PtMesh,
    /// Linear base color factor and alpha.
    pub base_color: [f32; 4],
    /// Metallic factor.
    pub metallic: f32,
    /// Perceptual roughness.
    pub roughness: f32,
    /// Emitted radiance, cd/m² (emissive color × strength): such parts become lights.
    pub emissive: [f32; 3],
    /// The material has a base color texture. The bake does not sample textures, so the
    /// description's `default_albedo` stands in for the texture average (times the factor).
    pub textured: bool,
}

impl BakeMeshPart {
    /// A part of `mesh` with a plain material of linear color `rgb`.
    #[must_use]
    pub fn plain(mesh: PtMesh, rgb: [f32; 3]) -> Self {
        Self {
            mesh,
            base_color: [rgb[0], rgb[1], rgb[2], 1.0],
            metallic: 0.0,
            roughness: 1.0,
            emissive: [0.0; 3],
            textured: false,
        }
    }
}

/// A loaded mesh source (a cooked mesh, or every primitive of a glTF with its node transforms).
#[derive(Clone, Debug, Default)]
pub struct BakeMeshSource {
    /// The primitives.
    pub parts: Vec<BakeMeshPart>,
}

/// Loads mesh sources for a bake.
pub trait BakeMeshLoader {
    /// Loads the source at `path` (already resolved against the description's `root`).
    ///
    /// # Errors
    ///
    /// A missing, unreadable or unsupported file, as a message (the caller adds the key path).
    fn load(&mut self, path: &Path) -> Result<BakeMeshSource, String>;

    /// Files loading `path` also reads (a `.gltf`'s buffers and images), for the manifest check.
    fn companions(&self, path: &Path) -> Vec<PathBuf> {
        gltf_companions(path)
    }
}

/// The loader `regia_pt` has on its own: cooked meshes (`.fckd`, `regia_asset::load_mesh_bytes`),
/// one part with a plain 0.5 grey material (give `material: Some((albedo: ...))` to change it).
#[derive(Clone, Copy, Debug, Default)]
pub struct CookedMeshLoader;

impl BakeMeshLoader for CookedMeshLoader {
    fn load(&mut self, path: &Path) -> Result<BakeMeshSource, String> {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !ext.eq_ignore_ascii_case("fckd") {
            return Err(format!(
                "{}: only cooked meshes (.fckd) load here; glTF sources load through `regia bake-gi`",
                path.display()
            ));
        }
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let loaded = regia_asset::load_mesh_bytes(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(BakeMeshSource {
            parts: vec![BakeMeshPart::plain(pt_mesh_from_cpu(&loaded.mesh), [0.5; 3])],
        })
    }
}

/// The geometry of a cooked CPU mesh for the tracer (positions, normals, UVs, indices).
#[must_use]
pub fn pt_mesh_from_cpu(mesh: &regia_asset::CpuMesh) -> PtMesh {
    PtMesh {
        positions: mesh.positions.clone(),
        normals: if mesh.normals.len() == mesh.positions.len() {
            mesh.normals.clone()
        } else {
            vec![[0.0; 3]; mesh.positions.len()]
        },
        uvs: if mesh.uvs.len() == mesh.positions.len() {
            mesh.uvs.clone()
        } else {
            Vec::new()
        },
        colors: Vec::new(),
        tangents: Vec::new(),
        indices: mesh.indices.clone(),
    }
}

/// The external files a `.gltf` names (`buffers[].uri`, `images[].uri`, not `data:` URIs),
/// resolved against its directory (`%20` decoded). Empty for a `.glb` or an unreadable file.
#[must_use]
pub fn gltf_companions(path: &Path) -> Vec<PathBuf> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if !ext.eq_ignore_ascii_case("gltf") {
        return Vec::new();
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut out = BTreeSet::new();
    for key in ["buffers", "images"] {
        for item in json.get(key).and_then(|v| v.as_array()).into_iter().flatten() {
            if let Some(uri) = item.get("uri").and_then(|u| u.as_str())
                && !uri.starts_with("data:")
            {
                out.insert(dir.join(uri.replace("%20", " ")));
            }
        }
    }
    out.into_iter().collect()
}

/// A bundle's `MANIFEST.sha256` (`<hex>  <path relative to the bundle root>` per line): every
/// file the bake reads under the root must be listed with a matching hash.
#[derive(Clone, Debug)]
pub struct Manifest {
    root: PathBuf,
    entries: HashMap<String, String>,
    verified: BTreeSet<PathBuf>,
}

impl Manifest {
    /// Reads the manifest at `path` for the bundle at `root`.
    ///
    /// # Errors
    ///
    /// `bake_source_missing` when the file cannot be read, `bake_desc_invalid` for a malformed line.
    pub fn load(path: &Path, root: &Path, key: &str) -> Result<Self, BakeDescError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| BakeDescError::missing(key, format!("{}: {e}", path.display())))?;
        Self::parse(&text, root, key)
    }

    /// Parses manifest text for the bundle at `root`.
    ///
    /// # Errors
    ///
    /// `bake_desc_invalid` naming the first malformed line.
    pub fn parse(text: &str, root: &Path, key: &str) -> Result<Self, BakeDescError> {
        let mut entries = HashMap::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim_end();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (hash, rest) = line.split_once(char::is_whitespace).ok_or_else(|| {
                BakeDescError::invalid(key, format!("line {}: expected `<sha256>  <path>`", n + 1))
            })?;
            let file = rest.trim_start().trim_start_matches('*');
            if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) || file.is_empty() {
                return Err(BakeDescError::invalid(
                    key,
                    format!("line {}: expected `<sha256>  <path>`", n + 1),
                ));
            }
            entries.insert(file.to_string(), hash.to_ascii_lowercase());
        }
        Ok(Self {
            root: root.to_path_buf(),
            entries,
            verified: BTreeSet::new(),
        })
    }

    /// Checks `file` against its entry (once per file). Files outside the bundle root are not
    /// part of it and are skipped (`Ok(false)`).
    ///
    /// # Errors
    ///
    /// `bake_manifest_missing_entry` (not listed), `bake_manifest_mismatch` (hash differs),
    /// `bake_source_missing` (unreadable).
    pub fn verify(&mut self, file: &Path, key: &str) -> Result<bool, BakeDescError> {
        let Ok(rel) = file.strip_prefix(&self.root) else {
            return Ok(false);
        };
        if self.verified.contains(file) {
            return Ok(true);
        }
        let name = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        let Some(want) = self.entries.get(&name) else {
            return Err(BakeDescError {
                code: "bake_manifest_missing_entry",
                path: Some(key.to_string()),
                message: format!("{name} is not listed in the manifest"),
            });
        };
        let got = crate::sha256::file_hex(file)
            .map_err(|e| BakeDescError::missing(key, format!("{}: {e}", file.display())))?;
        if &got != want {
            return Err(BakeDescError {
                code: "bake_manifest_mismatch",
                path: Some(key.to_string()),
                message: format!("{name}: sha256 {got}, the manifest lists {want}"),
            });
        }
        self.verified.insert(file.to_path_buf());
        Ok(true)
    }

    /// Files checked so far.
    #[must_use]
    pub fn verified(&self) -> usize {
        self.verified.len()
    }
}

/// What reading a DO27 `static_meshes.json` gave.
#[derive(Clone, Debug, Default)]
pub struct StaticMeshImport {
    /// Entries the bake places, in file order.
    pub meshes: Vec<MeshDesc>,
    /// Entries in the file.
    pub entries: usize,
    /// Entries not placed, with the reason (`not_static`, `unbaked_layer`, `generated_placement`).
    pub skipped: Vec<(String, &'static str)>,
}

fn json_f32s<const N: usize>(v: &serde_json::Value) -> Option<[f32; N]> {
    let a = v.as_array()?;
    if a.len() != N {
        return None;
    }
    let mut out = [0.0f32; N];
    for (o, x) in out.iter_mut().zip(a) {
        *o = x.as_f64()? as f32;
        if !o.is_finite() {
            return None;
        }
    }
    Some(out)
}

/// Parses a DO27 `static_meshes.json` (header version 1, metres, +Y up, right-handed; meshes with
/// `id`, column-major `transform`, `static`, `layer`, `source` glTF relative to the bundle root,
/// optional `instances` and `approximate`; unknown keys ignored). Entries with `static: false`,
/// a `structure`/`dynamic` layer, or `instances: null` with a generated `placement` are skipped;
/// an `instances` array places the source once per transform (after the entry's own). `mesh`
/// (the shell's cooked `.gltc`) is never read.
///
/// # Errors
///
/// `bake_desc_invalid` naming the key path (`<key>: header.units`, `<key>: meshes[3].layer`, …).
pub fn parse_static_meshes_json(text: &str, key: &str) -> Result<StaticMeshImport, BakeDescError> {
    let bad = |path: String, msg: &str| BakeDescError::invalid(&format!("{key}: {path}"), msg.to_string());
    let json: serde_json::Value =
        serde_json::from_str(text).map_err(|e| BakeDescError::parse(key, format!("not JSON: {e}")))?;
    let header = json
        .get("header")
        .ok_or_else(|| bad("header".into(), "missing"))?;
    if header.get("version").and_then(serde_json::Value::as_u64) != Some(1) {
        return Err(bad("header.version".into(), "must be 1"));
    }
    if !matches!(
        header.get("units").and_then(|v| v.as_str()),
        Some("metres" | "meters")
    ) {
        return Err(bad("header.units".into(), "must be \"metres\""));
    }
    if header.get("up").and_then(|v| v.as_str()) != Some("+Y") {
        return Err(bad("header.up".into(), "must be \"+Y\""));
    }
    if header.get("handedness").and_then(|v| v.as_str()) != Some("right") {
        return Err(bad("header.handedness".into(), "must be \"right\""));
    }
    let entries = json
        .get("meshes")
        .and_then(|v| v.as_array())
        .ok_or_else(|| bad("meshes".into(), "missing or not a list"))?;
    let mut out = StaticMeshImport {
        entries: entries.len(),
        ..StaticMeshImport::default()
    };
    for (i, e) in entries.iter().enumerate() {
        let at = |field: &str| format!("meshes[{i}].{field}");
        let id = e
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| bad(at("id"), "missing or not a string"))?
            .to_string();
        let transform = e
            .get("transform")
            .and_then(json_f32s::<16>)
            .ok_or_else(|| bad(at("transform"), "missing or not 16 finite numbers"))?;
        let is_static = e
            .get("static")
            .and_then(serde_json::Value::as_bool)
            .ok_or_else(|| bad(at("static"), "missing or not a bool"))?;
        let layer = e
            .get("layer")
            .and_then(|v| v.as_str())
            .ok_or_else(|| bad(at("layer"), "missing or not a string"))?;
        if !matches!(
            layer,
            "terrain" | "setpiece" | "scatter" | "structure" | "dynamic"
        ) {
            return Err(bad(
                at("layer"),
                "must be terrain, setpiece, scatter, structure or dynamic",
            ));
        }
        let instances = match e.get("instances") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::Array(list)) => {
                let mut placed = Vec::with_capacity(list.len());
                for (k, m) in list.iter().enumerate() {
                    placed.push(json_f32s::<16>(m).ok_or_else(|| {
                        bad(format!("meshes[{i}].instances[{k}]"), "not 16 finite numbers")
                    })?);
                }
                Some(placed)
            }
            Some(_) => return Err(bad(at("instances"), "must be a list or null")),
        };
        if !is_static {
            out.skipped.push((id, "not_static"));
            continue;
        }
        if UNBAKED_LAYERS.contains(&layer) {
            out.skipped.push((id, "unbaked_layer"));
            continue;
        }
        if instances.is_none() && e.get("placement").is_some_and(|p| !p.is_null()) {
            out.skipped.push((id, "generated_placement"));
            continue;
        }
        let source = e
            .get("source")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| bad(at("source"), "missing (the glTF the bake loads)"))?;
        out.meshes.push(MeshDesc {
            id: Some(id),
            source: PathBuf::from(source),
            transform,
            instances,
            layer: Some(layer.to_string()),
            is_static: true,
            material: None,
            approximate: e
                .get("approximate")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        });
    }
    Ok(out)
}

/// Parses a DO27 `interiors.json` (`format: "do27-bake-interiors/1"`, an object with an
/// `interiors` list of `id`, `aabb_min`, `aabb_max`, optional `sky_open` and `openings`; unknown
/// keys ignored). Any non-null, non-false `openings` value marks the interior as having openings.
///
/// # Errors
///
/// `bake_desc_invalid` naming the key path.
pub fn parse_interiors_json(text: &str, key: &str) -> Result<Vec<InteriorDesc>, BakeDescError> {
    let bad = |path: String, msg: &str| BakeDescError::invalid(&format!("{key}: {path}"), msg.to_string());
    let json: serde_json::Value =
        serde_json::from_str(text).map_err(|e| BakeDescError::parse(key, format!("not JSON: {e}")))?;
    if json.get("format").and_then(|v| v.as_str()) != Some(INTERIORS_FORMAT) {
        return Err(bad("format".into(), "must be \"do27-bake-interiors/1\""));
    }
    let list = json
        .get("interiors")
        .and_then(|v| v.as_array())
        .ok_or_else(|| bad("interiors".into(), "missing or not a list"))?;
    let mut out = Vec::with_capacity(list.len());
    for (i, e) in list.iter().enumerate() {
        let at = |field: &str| format!("interiors[{i}].{field}");
        let id = e
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| bad(at("id"), "missing or not a string"))?;
        let min = e
            .get("aabb_min")
            .and_then(json_f32s::<3>)
            .ok_or_else(|| bad(at("aabb_min"), "missing or not 3 finite numbers"))?;
        let max = e
            .get("aabb_max")
            .and_then(json_f32s::<3>)
            .ok_or_else(|| bad(at("aabb_max"), "missing or not 3 finite numbers"))?;
        if (0..3).any(|a| min[a] >= max[a]) {
            return Err(bad(at("aabb_max"), "must exceed aabb_min on every axis"));
        }
        let openings = e
            .get("openings")
            .is_some_and(|v| !v.is_null() && v.as_bool() != Some(false));
        out.push(InteriorDesc {
            id: id.to_string(),
            min,
            max,
            sky_open: e
                .get("sky_open")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            openings,
            spacing: None,
        });
    }
    Ok(out)
}

/// One placed copy of a mesh source.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PlacedMesh {
    /// The entry's `id` (or `meshes[<index>]`).
    pub id: String,
    /// The entry's layer tag.
    pub layer: Option<String>,
    /// World bounds, minimum corner.
    pub min: [f32; 3],
    /// World bounds, maximum corner.
    pub max: [f32; 3],
    /// The scene instance of the source's first part; the placement's parts are consecutive.
    pub first_instance: usize,
    /// Instances of this placement: one per part of the source.
    pub parts: usize,
}

/// Counts of what [`add_meshes`] placed.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct MeshStats {
    /// Mesh entries given (inline + imported).
    pub entries: usize,
    /// Distinct source files loaded.
    pub sources: usize,
    /// Placed copies (instances).
    pub placements: usize,
    /// Triangles over every placement.
    pub triangles: u64,
    /// Placements with a mirroring transform (negative determinant; winding flipped).
    pub mirrored: usize,
    /// Entries skipped (`static: false`, an unbaked layer, a generated placement, no placements).
    pub skipped: usize,
    /// Textured parts lit with `default_albedo` (no albedo override).
    pub default_albedo_parts: usize,
}

struct LoadedPart {
    mesh: usize,
    flipped: Option<usize>,
    part: BakeMeshPart,
    triangles: u64,
    min: [f32; 3],
    max: [f32; 3],
}

struct LoadedSource {
    parts: Vec<LoadedPart>,
}

fn object_bounds(mesh: &PtMesh) -> ([f32; 3], [f32; 3]) {
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for p in &mesh.positions {
        for a in 0..3 {
            min[a] = min[a].min(p[a]);
            max[a] = max[a].max(p[a]);
        }
    }
    (min, max)
}

/// World bounds of the object box `min..max` under `m`.
#[must_use]
pub fn transform_bounds(m: &Mat4, min: [f32; 3], max: [f32; 3]) -> ([f32; 3], [f32; 3]) {
    let mut wmin = [f32::INFINITY; 3];
    let mut wmax = [f32::NEG_INFINITY; 3];
    for corner in 0..8 {
        let c = Vec3::new(
            if corner & 1 == 0 { min[0] } else { max[0] },
            if corner & 2 == 0 { min[1] } else { max[1] },
            if corner & 4 == 0 { min[2] } else { max[2] },
        );
        let w = m.transform_point3(c).to_array();
        for a in 0..3 {
            wmin[a] = wmin[a].min(w[a]);
            wmax[a] = wmax[a].max(w[a]);
        }
    }
    (wmin, wmax)
}

/// `mesh` with every triangle's winding reversed (normals kept): the copy a mirroring
/// (negative-determinant) transform places, so its faces still wind counter-clockwise seen from
/// the side their normals point to.
#[must_use]
pub fn flip_winding(mesh: &PtMesh) -> PtMesh {
    let mut out = mesh.clone();
    for tri in out.indices.as_chunks_mut::<3>().0 {
        tri.swap(1, 2);
    }
    out
}

/// Reverses the winding of `mesh` when its triangles face against its vertex normals (summed
/// over all triangles; zero normals vote nothing). A glTF node with a negative determinant
/// bakes mirrored positions and normals into the part but keeps the index order, so its
/// geometric (cross-product) normals point inward while the shading normals point outward.
/// Returns whether the mesh was flipped.
pub fn align_winding_to_normals(mesh: &mut PtMesh) -> bool {
    let mut vote = 0.0f64;
    for tri in mesh.indices.as_chunks::<3>().0 {
        let (Some(a), Some(b), Some(c)) = (
            mesh.positions.get(tri[0] as usize),
            mesh.positions.get(tri[1] as usize),
            mesh.positions.get(tri[2] as usize),
        ) else {
            continue;
        };
        let face =
            (Vec3::from_array(*b) - Vec3::from_array(*a)).cross(Vec3::from_array(*c) - Vec3::from_array(*a));
        let mut n = Vec3::ZERO;
        for &i in tri {
            if let Some(v) = mesh.normals.get(i as usize) {
                n += Vec3::from_array(*v);
            }
        }
        vote += f64::from(face.dot(n));
    }
    if vote < 0.0 {
        for tri in mesh.indices.as_chunks_mut::<3>().0 {
            tri.swap(1, 2);
        }
        true
    } else {
        false
    }
}

/// The material a part bakes with: `over` wins per field; a textured part without an albedo
/// override takes `default_albedo` times its factor (the bake samples no textures).
#[must_use]
pub fn bake_material(
    part: &BakeMeshPart,
    over: Option<&MaterialOverride>,
    default_albedo: [f32; 3],
) -> PtMaterial {
    let f = part.base_color;
    let albedo = over.and_then(|o| o.albedo).unwrap_or(if part.textured {
        [
            default_albedo[0] * f[0],
            default_albedo[1] * f[1],
            default_albedo[2] * f[2],
        ]
    } else {
        [f[0], f[1], f[2]]
    });
    PtMaterial {
        base_color: [
            albedo[0].clamp(0.0, 1.0),
            albedo[1].clamp(0.0, 1.0),
            albedo[2].clamp(0.0, 1.0),
            1.0,
        ],
        metallic: over
            .and_then(|o| o.metallic)
            .unwrap_or(part.metallic)
            .clamp(0.0, 1.0),
        roughness: over
            .and_then(|o| o.roughness)
            .unwrap_or(part.roughness)
            .clamp(0.0, 1.0),
        emissive: over.and_then(|o| o.emissive).unwrap_or(part.emissive),
        ..PtMaterial::default()
    }
}

fn mat4(cols: &[f32; 16]) -> Mat4 {
    Mat4::from_cols_array(cols)
}

/// Loads every static entry of `meshes` through `loader` (each source once; paths relative to
/// `root`; checked against `manifest` when given) and places it in `scene`: one instance per
/// placement (`transform × instance`), a winding-flipped copy for mirroring transforms, a material
/// per part from [`bake_material`]. Emissive parts become lights through the scene as usual.
/// `key` prefixes error paths (`meshes`, or `static_meshes.json: meshes`).
///
/// # Errors
///
/// `bake_mesh_load` for a source that does not load, `bake_desc_invalid` for a degenerate
/// transform, and the manifest errors of [`Manifest::verify`].
pub fn add_meshes(
    scene: &mut PtScene,
    meshes: &[MeshDesc],
    root: &Path,
    loader: &mut dyn BakeMeshLoader,
    mut manifest: Option<&mut Manifest>,
    default_albedo: [f32; 3],
    key: &str,
) -> Result<(Vec<PlacedMesh>, MeshStats), BakeDescError> {
    let _span = tracing::info_span!("pt_bake_add_meshes", entries = meshes.len()).entered();
    let mut stats = MeshStats {
        entries: meshes.len(),
        ..MeshStats::default()
    };
    let mut placed = Vec::new();
    let mut loaded: HashMap<PathBuf, LoadedSource> = HashMap::new();
    for (i, desc) in meshes.iter().enumerate() {
        let layer_skipped = desc.layer.as_deref().is_some_and(|l| UNBAKED_LAYERS.contains(&l));
        if !desc.is_static || layer_skipped || desc.instances.as_ref().is_some_and(Vec::is_empty) {
            stats.skipped += 1;
            continue;
        }
        let at = |field: &str| format!("{key}[{i}].{field}");
        let path = root.join(&desc.source);
        if let std::collections::hash_map::Entry::Vacant(slot) = loaded.entry(path.clone()) {
            if let Some(m) = manifest.as_deref_mut() {
                if !m.verify(&path, &at("source"))? {
                    // A manifest lists the bundle's files; a source outside the root would be
                    // traced without being hashed.
                    return Err(BakeDescError {
                        code: "bake_manifest_missing_entry",
                        path: Some(at("source")),
                        message: format!("{} lies outside the bundle root", path.display()),
                    });
                }
                for c in loader.companions(&path) {
                    m.verify(&c, &at("source"))?;
                }
            }
            let source = loader.load(&path).map_err(|e| BakeDescError {
                code: "bake_mesh_load",
                path: Some(at("source")),
                message: e,
            })?;
            let parts = source
                .parts
                .into_iter()
                .filter(|p| p.mesh.indices.len() >= 3 && !p.mesh.positions.is_empty())
                .map(|mut p| {
                    align_winding_to_normals(&mut p.mesh);
                    // The scene owns the geometry; the part keeps its material and bounds.
                    let (min, max) = object_bounds(&p.mesh);
                    let triangles = p.mesh.triangle_count() as u64;
                    let index = scene.add_mesh(std::mem::take(&mut p.mesh));
                    LoadedPart {
                        mesh: index,
                        flipped: None,
                        part: p,
                        triangles,
                        min,
                        max,
                    }
                })
                .collect();
            slot.insert(LoadedSource { parts });
            stats.sources += 1;
        }
        let Some(source) = loaded.get_mut(&path) else {
            continue;
        };
        let materials: Vec<usize> = source
            .parts
            .iter()
            .map(|LoadedPart { part, .. }| {
                if part.textured && desc.material.as_ref().and_then(|o| o.albedo).is_none() {
                    stats.default_albedo_parts += 1;
                }
                scene.add_material(bake_material(part, desc.material.as_ref(), default_albedo))
            })
            .collect();
        let base = mat4(&desc.transform);
        let single = [Mat4::IDENTITY.to_cols_array()];
        let instances: &[[f32; 16]] = desc.instances.as_deref().unwrap_or(&single);
        let id = desc.id.clone().unwrap_or_else(|| format!("{key}[{i}]"));
        for (k, inst) in instances.iter().enumerate() {
            let m = base * mat4(inst);
            let det = m.determinant();
            if !det.is_finite() || det.abs() < 1e-12 || !m.is_finite() {
                let field = if desc.instances.is_some() {
                    format!("instances[{k}]")
                } else {
                    "transform".to_string()
                };
                return Err(BakeDescError::invalid(
                    &at(&field),
                    "a degenerate or non-finite transform".to_string(),
                ));
            }
            let mirrored = det < 0.0;
            stats.mirrored += usize::from(mirrored);
            let mut wmin = [f32::INFINITY; 3];
            let mut wmax = [f32::NEG_INFINITY; 3];
            let first_instance = scene.instances.len();
            for (lp, &material) in source.parts.iter_mut().zip(&materials) {
                let index = if mirrored {
                    let original = lp.mesh;
                    *lp.flipped.get_or_insert_with(|| {
                        let flipped = flip_winding(&scene.meshes[original]);
                        scene.add_mesh(flipped)
                    })
                } else {
                    lp.mesh
                };
                scene.add_instance(index, material, m);
                stats.triangles += lp.triangles;
                let (a, b) = transform_bounds(&m, lp.min, lp.max);
                for ax in 0..3 {
                    wmin[ax] = wmin[ax].min(a[ax]);
                    wmax[ax] = wmax[ax].max(b[ax]);
                }
            }
            if wmin[0] <= wmax[0] {
                stats.placements += 1;
                placed.push(PlacedMesh {
                    id: id.clone(),
                    layer: desc.layer.clone(),
                    min: wmin,
                    max: wmax,
                    first_instance,
                    parts: scene.instances.len() - first_instance,
                });
            }
        }
    }
    Ok((placed, stats))
}

/// Most samples per side of a heightfield rasterized from a mesh (the tracer's cell limit + 1).
const MAX_RASTER_SAMPLES: u32 = (1 << 14) + 1;

/// A terrain heightfield from a terrain grid mesh: every part's triangles under `transform`,
/// rasterized top-down on a grid of `spacing` metres over the mesh's XZ bounds (the highest
/// surface over each sample). Samples no triangle covers are NaN, i.e. cut (a hole such as an
/// open borehole mouth): the tracer drops those cells and the terrain layer stands the columns
/// there on the rim's ground. Built at the mesh's own vertex spacing, the samples are its vertices.
///
/// # Errors
///
/// A non-positive spacing, an empty mesh, or more than 16 385 samples per side.
pub fn heightfield_from_mesh(
    source: &BakeMeshSource,
    transform: &Mat4,
    spacing: f32,
    material: usize,
) -> Result<PtHeightfield, String> {
    let _span = tracing::info_span!("pt_bake_heightfield_from_mesh").entered();
    if !(spacing > 0.0 && spacing.is_finite()) {
        return Err("spacing must be positive".into());
    }
    let world: Vec<Vec<[f32; 3]>> = source
        .parts
        .iter()
        .map(|p| {
            p.mesh
                .positions
                .iter()
                .map(|q| transform.transform_point3(Vec3::from_array(*q)).to_array())
                .collect()
        })
        .collect();
    let (mut min, mut max) = ([f32::INFINITY; 2], [f32::NEG_INFINITY; 2]);
    for p in world.iter().flatten() {
        min = [min[0].min(p[0]), min[1].min(p[2])];
        max = [max[0].max(p[0]), max[1].max(p[2])];
    }
    if !(min[0] < max[0] && min[1] < max[1]) {
        return Err("the terrain mesh has no extent in X and Z".into());
    }
    let samples = |len: f32| ((len / spacing + 1e-3).floor() as u64).saturating_add(1);
    let (w, d) = (samples(max[0] - min[0]), samples(max[1] - min[1]));
    if w > u64::from(MAX_RASTER_SAMPLES) || d > u64::from(MAX_RASTER_SAMPLES) {
        return Err(format!(
            "{w} x {d} samples at {spacing} m; at most {MAX_RASTER_SAMPLES} per side"
        ));
    }
    let (w, d) = (w as u32, d as u32);
    let mut heights = vec![f32::NAN; w as usize * d as usize];
    for (part, pos) in source.parts.iter().zip(&world) {
        for tri in part.mesh.indices.as_chunks::<3>().0 {
            let (Some(a), Some(b), Some(c)) = (
                pos.get(tri[0] as usize),
                pos.get(tri[1] as usize),
                pos.get(tri[2] as usize),
            ) else {
                continue;
            };
            rasterize_triangle(*a, *b, *c, min, spacing, [w, d], &mut heights);
        }
    }
    Ok(PtHeightfield {
        width: w,
        depth: d,
        origin: min,
        spacing: [spacing; 2],
        heights,
        material,
    })
}

fn rasterize_triangle(
    a: [f32; 3],
    b: [f32; 3],
    c: [f32; 3],
    origin: [f32; 2],
    spacing: f32,
    counts: [u32; 2],
    heights: &mut [f32],
) {
    // Edge functions in XZ; samples on an edge (within a small tolerance) count as inside.
    let area = (b[0] - a[0]) * (c[2] - a[2]) - (c[0] - a[0]) * (b[2] - a[2]);
    if area.abs() < 1e-12 {
        return;
    }
    let tol = 1e-4 * area.abs();
    let lo = |v: f32, o: f32, n: u32| (((v - o) / spacing - 1e-3).ceil().max(0.0) as u32).min(n);
    let hi = |v: f32, o: f32, n: u32| ((v - o) / spacing + 1e-3).floor().min(n as f32 - 1.0);
    let (x0, x1) = (a[0].min(b[0]).min(c[0]), a[0].max(b[0]).max(c[0]));
    let (z0, z1) = (a[2].min(b[2]).min(c[2]), a[2].max(b[2]).max(c[2]));
    let (i0, i1) = (lo(x0, origin[0], counts[0]), hi(x1, origin[0], counts[0]));
    let (j0, j1) = (lo(z0, origin[1], counts[1]), hi(z1, origin[1], counts[1]));
    if i1 < 0.0 || j1 < 0.0 {
        return;
    }
    for j in j0..=(j1 as u32) {
        let z = origin[1] + j as f32 * spacing;
        for i in i0..=(i1 as u32) {
            let x = origin[0] + i as f32 * spacing;
            let wa = (b[0] - x) * (c[2] - z) - (c[0] - x) * (b[2] - z);
            let wb = (c[0] - x) * (a[2] - z) - (a[0] - x) * (c[2] - z);
            let wc = area - wa - wb;
            let inside = if area > 0.0 {
                wa >= -tol && wb >= -tol && wc >= -tol
            } else {
                wa <= tol && wb <= tol && wc <= tol
            };
            if !inside {
                continue;
            }
            let h = (wa * a[1] + wb * b[1] + wc * c[1]) / area;
            let slot = &mut heights[(j * counts[0] + i) as usize];
            if slot.is_nan() || h > *slot {
                *slot = h;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::PtCamera;

    fn quad() -> PtMesh {
        // A unit quad in the XY plane facing +Z (counter-clockwise seen from +Z).
        PtMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 1.0, 0.0], [0.0, 1.0, 0.0]],
            normals: vec![[0.0, 0.0, 1.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3],
            ..PtMesh::default()
        }
    }

    #[test]
    fn a_part_wound_against_its_normals_is_flipped_once() {
        // The quad mirrored in X by a node: positions and normals follow, the indices do not.
        let mut mesh = quad();
        for p in &mut mesh.positions {
            p[0] = -p[0];
        }
        assert!(face_normal(&Mat4::IDENTITY, &mesh, 0).z < 0.0);
        assert!(align_winding_to_normals(&mut mesh));
        for t in 0..2 {
            assert!(face_normal(&Mat4::IDENTITY, &mesh, t).z > 0.0);
        }
        // Already consistent (and a second call): untouched.
        assert!(!align_winding_to_normals(&mut mesh));
        let mut plain = quad();
        assert!(!align_winding_to_normals(&mut plain));
        assert_eq!(plain.indices, quad().indices);
        // No normals at all: no vote, no change.
        let mut bare = quad();
        bare.normals = vec![[0.0; 3]; 4];
        assert!(!align_winding_to_normals(&mut bare));
    }

    fn face_normal(m: &Mat4, mesh: &PtMesh, t: usize) -> Vec3 {
        let p =
            |k: usize| m.transform_point3(Vec3::from_array(mesh.positions[mesh.indices[3 * t + k] as usize]));
        (p(1) - p(0)).cross(p(2) - p(0)).normalize()
    }

    fn shading_normal(m: &Mat4, mesh: &PtMesh, v: usize) -> Vec3 {
        // Normals transform by the inverse transpose, as the tracer does.
        (m.inverse()
            .transpose()
            .transform_vector3(Vec3::from_array(mesh.normals[v])))
        .normalize()
    }

    struct Fixture(BakeMeshSource);

    impl BakeMeshLoader for Fixture {
        fn load(&mut self, _path: &Path) -> Result<BakeMeshSource, String> {
            Ok(self.0.clone())
        }
    }

    fn desc(transform: [f32; 16]) -> MeshDesc {
        MeshDesc {
            id: Some("quad".into()),
            source: PathBuf::from("quad.gltf"),
            transform,
            instances: None,
            layer: Some("setpiece".into()),
            is_static: true,
            material: None,
            approximate: false,
        }
    }

    /// A node-mirrored part (positions mirrored, indices kept) is front-facing to the tracer's
    /// geometric normal whether it is placed plainly or under a second mirror.
    #[test]
    fn a_node_mirrored_part_is_consistent_under_every_placement() {
        let mut inside_out = quad();
        for p in &mut inside_out.positions {
            p[0] = -p[0];
        }
        let mirror =
            Mat4::from_cols_array(&[-1.0, 0., 0., 0., 0., 1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1.]);
        let mut scene = PtScene::new(PtCamera::pinhole([0.0; 3], [0.0, 0.0, -1.0], 1.0, 4, 4));
        let mut loader = Fixture(BakeMeshSource {
            parts: vec![BakeMeshPart::plain(inside_out, [0.5; 3])],
        });
        add_meshes(
            &mut scene,
            &[desc(Mat4::IDENTITY.to_cols_array()), desc(mirror.to_cols_array())],
            Path::new("/bundle"),
            &mut loader,
            None,
            [0.5; 3],
            "meshes",
        )
        .expect("place");
        assert_eq!(scene.instances.len(), 2);
        for inst in &scene.instances {
            let mesh = &scene.meshes[inst.mesh];
            for t in 0..2 {
                for v in 0..4 {
                    assert!(
                        face_normal(&inst.transform, mesh, t).dot(shading_normal(&inst.transform, mesh, v))
                            > 0.99
                    );
                }
            }
        }
    }

    /// With a manifest, a source outside the bundle root is refused instead of traced unhashed.
    #[test]
    fn a_source_outside_the_root_is_refused_with_a_manifest() {
        let dir = regia_test_utils::unique_temp_dir("regia_pt_manifest_outside");
        std::fs::create_dir_all(&dir).expect("dir");
        let mut manifest = Manifest::parse("", &dir, "manifest").expect("parse");
        let mut scene = PtScene::new(PtCamera::pinhole([0.0; 3], [0.0, 0.0, -1.0], 1.0, 4, 4));
        let mut loader = Fixture(BakeMeshSource {
            parts: vec![BakeMeshPart::plain(quad(), [0.5; 3])],
        });
        let mut d = desc(Mat4::IDENTITY.to_cols_array());
        d.source = dir.parent().unwrap_or(&dir).join("elsewhere.gltf");
        let err = add_meshes(
            &mut scene,
            &[d],
            &dir,
            &mut loader,
            Some(&mut manifest),
            [0.5; 3],
            "meshes",
        )
        .expect_err("outside the root");
        assert_eq!(err.code, "bake_manifest_missing_entry");
        assert!(scene.instances.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// DO27 deviation (2): a mirroring transform (det < 0, `mirror_x`) places a winding-flipped
    /// copy, so the world face normal agrees with the transformed vertex normals again.
    #[test]
    fn a_mirrored_placement_flips_the_winding_and_keeps_normals_consistent() {
        let mirror =
            Mat4::from_cols_array(&[-1.0, 0., 0., 0., 0., 1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1.]);
        let q = quad();
        // Placed as is, the mirror turns the winding against the normals.
        assert!(face_normal(&mirror, &q, 0).dot(shading_normal(&mirror, &q, 0)) < 0.0);
        let mut scene = PtScene::new(PtCamera::pinhole([0.0; 3], [0.0, 0.0, -1.0], 1.0, 4, 4));
        let mut loader = Fixture(BakeMeshSource {
            parts: vec![BakeMeshPart::plain(q.clone(), [0.5; 3])],
        });
        let (placed, stats) = add_meshes(
            &mut scene,
            &[desc(mirror.to_cols_array()), desc(Mat4::IDENTITY.to_cols_array())],
            Path::new("/bundle"),
            &mut loader,
            None,
            [0.5; 3],
            "meshes",
        )
        .expect("place");
        assert_eq!(stats.sources, 1);
        assert_eq!(stats.placements, 2);
        assert_eq!(stats.mirrored, 1);
        assert_eq!(placed[0].min, [-1.0, 0.0, 0.0]);
        assert_eq!(placed[0].max, [0.0, 1.0, 0.0]);
        // Original + its flipped copy, two instances.
        assert_eq!(scene.meshes.len(), 2);
        assert_eq!(scene.instances.len(), 2);
        let mirrored = &scene.instances[0];
        let mesh = &scene.meshes[mirrored.mesh];
        for t in 0..2 {
            for v in 0..4 {
                assert!(
                    face_normal(&mirrored.transform, mesh, t).dot(shading_normal(
                        &mirrored.transform,
                        mesh,
                        v
                    )) > 0.99
                );
            }
        }
        // The world normal is still +Z: the mirror about X leaves a plane facing +Z facing +Z.
        assert!((face_normal(&mirrored.transform, mesh, 0) - Vec3::Z).length() < 1e-5);
        let plain = &scene.instances[1];
        assert_eq!(plain.mesh, 0);
        assert!((face_normal(&plain.transform, &scene.meshes[0], 0) - Vec3::Z).length() < 1e-5);
    }

    #[test]
    fn instances_place_one_copy_each_and_skips_are_counted() {
        let mut scene = PtScene::new(PtCamera::pinhole([0.0; 3], [0.0, 0.0, -1.0], 1.0, 4, 4));
        let mut loader = Fixture(BakeMeshSource {
            parts: vec![BakeMeshPart::plain(quad(), [0.5; 3])],
        });
        let at = |x: f32| Mat4::from_translation(Vec3::new(x, 0.0, 0.0)).to_cols_array();
        let mut rocks = desc(Mat4::IDENTITY.to_cols_array());
        rocks.instances = Some(vec![at(10.0), at(20.0), at(30.0)]);
        let mut dynamic = desc(Mat4::IDENTITY.to_cols_array());
        dynamic.layer = Some("dynamic".into());
        let mut not_static = desc(Mat4::IDENTITY.to_cols_array());
        not_static.is_static = false;
        let (placed, stats) = add_meshes(
            &mut scene,
            &[rocks, dynamic, not_static],
            Path::new("/b"),
            &mut loader,
            None,
            [0.5; 3],
            "meshes",
        )
        .expect("place");
        assert_eq!(stats.placements, 3);
        assert_eq!(stats.skipped, 2);
        assert_eq!(scene.meshes.len(), 1, "one source, shared by every instance");
        assert_eq!(placed[2].min[0], 30.0);
        let mut bad = desc([0.0; 16]);
        bad.id = None;
        let err = add_meshes(
            &mut scene,
            &[bad],
            Path::new("/b"),
            &mut loader,
            None,
            [0.5; 3],
            "meshes",
        )
        .expect_err("degenerate");
        assert_eq!(err.path.as_deref(), Some("meshes[0].transform"));
    }

    #[test]
    fn textured_parts_take_the_default_albedo_unless_overridden() {
        let mut part = BakeMeshPart::plain(quad(), [1.0, 1.0, 0.5]);
        part.textured = true;
        assert_eq!(
            bake_material(&part, None, [0.4; 3]).base_color,
            [0.4, 0.4, 0.2, 1.0]
        );
        let over = MaterialOverride {
            albedo: Some([0.2, 0.3, 0.4]),
            emissive: Some([5.0, 0.0, 0.0]),
            roughness: None,
            metallic: None,
        };
        let m = bake_material(&part, Some(&over), [0.4; 3]);
        assert_eq!(m.base_color, [0.2, 0.3, 0.4, 1.0]);
        assert!(m.is_emissive());
        part.textured = false;
        assert_eq!(
            bake_material(&part, None, [0.4; 3]).base_color,
            [1.0, 1.0, 0.5, 1.0]
        );
    }

    const STATIC_MESHES: &str = r#"{
        "header": {"version": 1, "units": "metres", "up": "+Y", "handedness": "right"},
        "revision": 2,
        "meshes": [
            {"id": "terrain", "mesh": "cooked/terrain.tpak", "transform": [1,0,0,0, 0,1,0,0, 0,0,1,0, 0,0,0,1],
             "source": "terrain/terrain.gltf", "static": true, "layer": "terrain", "note": "x"},
            {"id": "hab", "mesh": "cooked/gltf/a.gltc", "transform": [-1,0,0,0, 0,1,0,0, 0,0,1,0, 0,0,0,1],
             "static": true, "layer": "setpiece", "mirror_x": true, "source": "setpieces/hab.gltf", "cooked_lods": {}},
            {"id": "scatter_rocks", "mesh": "a.gltc", "mesh_variants": [], "sources": ["r0.gltf"], "source": "r0.gltf",
             "transform": [1,0,0,0, 0,1,0,0, 0,0,1,0, 0,0,0,1], "static": true, "size_m": 1.0, "layer": "scatter",
             "instances": null, "placement": {"generator": "x", "seed": 1}},
            {"id": "scatter_stones", "transform": [1,0,0,0, 0,1,0,0, 0,0,1,0, 0,0,0,1], "static": false, "layer": "scatter",
             "source": "s.gltf", "instances": null, "placement": {"seed": 1}},
            {"id": "hints", "mesh": "b.gltc", "transform": [1,0,0,0, 0,1,0,0, 0,0,1,0, 0,0,0,1], "static": true,
             "source": "boulder.gltf", "size_m": 1.5, "layer": "scatter", "approximate": true,
             "instances": [[1,0,0,0, 0,1,0,0, 0,0,1,0, 639.9,-36.8,45.8,1], [1,0,0,0, 0,1,0,0, 0,0,1,0, 1,2,3,1]]}
        ]
    }"#;

    /// DO27 deviations (1), (3), (5): `source` is loaded (never `mesh`), generated scatter
    /// classes and non-static entries are skipped, the rock hints are placed per instance, unknown
    /// keys are ignored.
    #[test]
    fn static_meshes_json_maps_to_mesh_entries() {
        let import = parse_static_meshes_json(STATIC_MESHES, "static_meshes.json").expect("parse");
        assert_eq!(import.entries, 5);
        let ids: Vec<_> = import
            .meshes
            .iter()
            .map(|m| m.id.clone().unwrap_or_default())
            .collect();
        assert_eq!(ids, ["terrain", "hab", "hints"]);
        assert_eq!(import.meshes[1].source, PathBuf::from("setpieces/hab.gltf"));
        assert_eq!(import.meshes[1].transform[0], -1.0);
        let hints = &import.meshes[2];
        assert!(hints.approximate);
        let inst = hints.instances.as_ref().expect("instances");
        assert_eq!(inst.len(), 2);
        for (got, want) in inst[0][12..15].iter().zip([639.9f32, -36.8, 45.8]) {
            assert!((got - want).abs() < 1e-3, "{got} {want}");
        }
        assert_eq!(
            import.skipped,
            [
                ("scatter_rocks".to_string(), "generated_placement"),
                ("scatter_stones".to_string(), "not_static")
            ]
        );
    }

    /// DO27 deviation (5): the header and the required fields are validated, errors name the key.
    #[test]
    fn static_meshes_json_errors_name_the_field() {
        let cases = [
            (STATIC_MESHES.replace("\"metres\"", "\"feet\""), "header.units"),
            (
                STATIC_MESHES.replace("\"right\"", "\"left\""),
                "header.handedness",
            ),
            (
                STATIC_MESHES.replace("\"layer\": \"setpiece\"", "\"layer\": \"props\""),
                "meshes[1].layer",
            ),
            (STATIC_MESHES.replace("\"id\": \"hab\",", ""), "meshes[1].id"),
            (
                STATIC_MESHES.replace(
                    "\"static\": true, \"layer\": \"setpiece\"",
                    "\"layer\": \"setpiece\"",
                ),
                "meshes[1].static",
            ),
            (
                STATIC_MESHES.replace("[-1,0,0,0, 0,1,0,0, 0,0,1,0, 0,0,0,1]", "[1, 2]"),
                "meshes[1].transform",
            ),
            (
                STATIC_MESHES.replace(", \"source\": \"setpieces/hab.gltf\"", ""),
                "meshes[1].source",
            ),
        ];
        for (text, want) in cases {
            let err = parse_static_meshes_json(&text, "static_meshes.json").expect_err(want);
            assert_eq!(err.code, "bake_desc_invalid");
            assert_eq!(
                err.path.as_deref(),
                Some(format!("static_meshes.json: {want}").as_str()),
                "{err}"
            );
        }
    }

    #[test]
    fn interiors_json_parses_tolerantly_and_checks_the_format() {
        let text = r#"{"format": "do27-bake-interiors/1", "frame": "render", "interiors": [
            {"id": "hangar", "kind": "hangar", "aabb_min": [48, -4.2, -54], "aabb_max": [78, 10.4, -3],
             "source": "x", "openings": "east wall has a 10 m vehicle bay"},
            {"id": "hab", "aabb_min": [-13.6, 2.7, 142], "aabb_max": [14, 16.8, 157.5], "note": "?"},
            {"id": "borehole_shaft", "aabb_min": [-22.5, -92.4, -22.5], "aabb_max": [22.5, 7.4, 22.5], "sky_open": true}
        ]}"#;
        let list = parse_interiors_json(text, "interiors.json").expect("parse");
        assert_eq!(list.len(), 3);
        assert!(list[0].openings && !list[0].sky_open);
        assert!(!list[1].openings && !list[1].sky_open);
        assert!(list[2].sky_open);
        let err = parse_interiors_json(&text.replace("/1\"", "/2\""), "interiors.json").expect_err("format");
        assert_eq!(err.path.as_deref(), Some("interiors.json: format"));
        let err = parse_interiors_json(
            &text.replace("[78, 10.4, -3]", "[40, 10.4, -3]"),
            "interiors.json",
        )
        .expect_err("box");
        assert_eq!(err.path.as_deref(), Some("interiors.json: interiors[0].aabb_max"));
        // A bare list (not the contract's object) is refused.
        assert!(parse_interiors_json("[]", "interiors.json").is_err());
    }

    #[test]
    fn manifest_checks_listed_files() {
        let dir = regia_test_utils::unique_temp_dir("regia_pt_manifest");
        std::fs::create_dir_all(dir.join("terrain")).expect("dir");
        std::fs::write(dir.join("terrain/a.bin"), b"abc").expect("write");
        std::fs::write(dir.join("b.bin"), b"abd").expect("write");
        std::fs::write(dir.join("c.bin"), b"x").expect("write");
        let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let text = format!("{abc}  terrain/a.bin\n{abc}  b.bin\n");
        let mut m = Manifest::parse(&text, &dir, "manifest").expect("parse");
        assert!(m.verify(&dir.join("terrain/a.bin"), "k").expect("listed"));
        assert_eq!(
            m.verify(&dir.join("b.bin"), "k").expect_err("mismatch").code,
            "bake_manifest_mismatch"
        );
        assert_eq!(
            m.verify(&dir.join("c.bin"), "k").expect_err("missing").code,
            "bake_manifest_missing_entry"
        );
        assert!(
            !m.verify(Path::new("/elsewhere/file"), "k")
                .expect("outside the root")
        );
        assert_eq!(m.verified(), 1);
        assert!(Manifest::parse("nothex  a\n", &dir, "manifest").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gltf_companions_lists_external_buffers_and_images() {
        let dir = regia_test_utils::unique_temp_dir("regia_pt_gltf_companions");
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("t.gltf");
        std::fs::write(
            &path,
            r#"{"buffers": [{"uri": "t.bin", "byteLength": 4}, {"uri": "data:application/octet-stream;base64,AAAA"}],
                "images": [{"uri": "tex/albedo%20a.png"}, {"bufferView": 0}]}"#,
        )
        .expect("write");
        assert_eq!(
            gltf_companions(&path),
            [dir.join("t.bin"), dir.join("tex/albedo a.png")]
        );
        assert!(gltf_companions(&dir.join("t.glb")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A 3 × 3 vertex grid (1 m) rasterizes back to its own heights at its own spacing; a missing
    /// cell's corner-free samples stay NaN (a hole).
    #[test]
    fn a_grid_mesh_rasterizes_to_its_heights() {
        let h = |i: u32, j: u32| (i * 3 + j) as f32 * 0.25;
        let mut positions = Vec::new();
        for j in 0..3u32 {
            for i in 0..3u32 {
                positions.push([i as f32 - 1.0, h(i, j), j as f32 - 1.0]);
            }
        }
        let mut indices = Vec::new();
        for j in 0..2u32 {
            for i in 0..2u32 {
                let v = j * 3 + i;
                indices.extend_from_slice(&[v, v + 3, v + 1, v + 1, v + 3, v + 4]);
            }
        }
        let mesh = PtMesh {
            normals: vec![[0.0, 1.0, 0.0]; positions.len()],
            positions,
            indices,
            ..PtMesh::default()
        };
        let source = BakeMeshSource {
            parts: vec![BakeMeshPart::plain(mesh.clone(), [0.3; 3])],
        };
        let hf = heightfield_from_mesh(&source, &Mat4::IDENTITY, 1.0, 0).expect("raster");
        assert_eq!((hf.width, hf.depth), (3, 3));
        assert_eq!(hf.origin, [-1.0, -1.0]);
        for j in 0..3 {
            for i in 0..3 {
                assert!((hf.height(i, j) - h(i, j)).abs() < 1e-5, "({i}, {j})");
            }
        }
        // At half the spacing the samples between vertices interpolate the triangles.
        let fine = heightfield_from_mesh(&source, &Mat4::IDENTITY, 0.5, 0).expect("raster");
        assert_eq!((fine.width, fine.depth), (5, 5));
        assert!(fine.heights.iter().all(|v| v.is_finite()));
        // Drop the last cell's two triangles: its far corner sample is not covered.
        let mut holed = mesh;
        holed.indices.truncate(18);
        let source = BakeMeshSource {
            parts: vec![BakeMeshPart::plain(holed, [0.3; 3])],
        };
        let hf = heightfield_from_mesh(&source, &Mat4::IDENTITY, 1.0, 0).expect("raster");
        assert!(hf.height(2, 2).is_nan());
        assert!(hf.height(1, 1).is_finite());
        assert!(heightfield_from_mesh(&source, &Mat4::IDENTITY, 0.0, 0).is_err());
    }
}
