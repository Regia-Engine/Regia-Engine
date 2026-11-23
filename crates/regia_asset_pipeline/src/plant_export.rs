// SPDX-License-Identifier: MPL-2.0
//! Writes the plant exports `plant.export` nodes record (`docs/specs/68-procedural-plants.md`
//! § 6.3): LODs by meshoptimizer simplification (bark) and leaf thinning, an impostor billboard part
//! on the last LOD (the CPU bake of `regia_terrain::scatter::impostor`, ADR-2731), optional
//! collision capsules, and a glTF 2.0 binary in the
//! tree conventions (`_LOD<n>` nodes, `lod_screen_size` / `shadow_lod` extras, the `_WIND` /
//! `_WIND_PIVOT` attributes of `docs/formats/plant-wind.md`). Writes are atomic (tmp + rename).
//! Original code.

use std::path::{Path, PathBuf};

use meshopt::{SimplifyOptions, simplify_decoder};
use regia_procgen::data::MeshData;
use regia_procgen::plant::{
    ExportCollision, ExportJob, PlantBuildError, PlantMesh, PlantMeshSlot, SlotKind, WrittenPlant,
};
use regia_render::{LabMaterial, LabVertex};
use regia_terrain::scatter::impostor::{
    ImpostorCard, ImpostorPart, ImpostorSettings, ImpostorSource, bake_cards, capture_basis,
    capture_direction,
};
use serde_json::{Map, Value, json};

use crate::gltf_conventions::default_lod_screen_size;
use crate::gltf_write::{GlbImage, GlbMaterial, GlbMesh, GlbNode, GlbPrimitive, GlbScene, GlbTextureRef};

/// The wind layout version written as mesh extras `regia_wind_layout`.
pub const WIND_LAYOUT: u32 = 1;

/// What a write produced.
#[derive(Clone, Debug, PartialEq)]
pub struct ExportReport {
    /// The file written.
    pub path: PathBuf,
    /// Triangles per LOD (LOD 0 first; the billboard counts on the last).
    pub triangles_per_lod: Vec<usize>,
    /// Object-space bounds `(min, max)` of LOD 0.
    pub bounds: ([f32; 3], [f32; 3]),
    /// File size in bytes.
    pub bytes: usize,
}

/// A failed plant export.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    /// The mesh has no triangles (`plant_not_meshed`).
    #[error("plant_not_meshed: the plant mesh has no triangles")]
    NotMeshed,
    /// No LOD ratios.
    #[error("export_invalid: {0}")]
    Invalid(String),
    /// Writing the file failed.
    #[error("io: {path}: {source}")]
    Io {
        /// The path.
        path: PathBuf,
        /// The error.
        source: std::io::Error,
    },
}

impl ExportError {
    /// The stable lowercase code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotMeshed => "plant_not_meshed",
            Self::Invalid(_) => "export_invalid",
            Self::Io { .. } => "io",
        }
    }
}

/// Writes `job` to `job.out` (relative paths resolve against the working directory).
///
/// # Errors
///
/// See [`ExportError`].
pub fn write_job(job: &ExportJob) -> Result<ExportReport, ExportError> {
    write_job_to(job, Path::new(&job.out))
}

/// Writes `job` to `path`.
///
/// # Errors
///
/// See [`ExportError`].
pub fn write_job_to(job: &ExportJob, path: &Path) -> Result<ExportReport, ExportError> {
    let _span = tracing::info_span!("plant_export", path = %path.display()).entered();
    let (bytes, triangles_per_lod) = glb_bytes(job)?;
    let io = |source| ExportError::Io {
        path: path.to_path_buf(),
        source,
    };
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    crate::cook_ledger::write_atomic(path, &bytes).map_err(io)?;
    Ok(ExportReport {
        path: path.to_path_buf(),
        triangles_per_lod,
        bounds: job.mesh.bounds(),
        bytes: bytes.len(),
    })
}

/// [`write_job_to`] in the shape a plant build host hands to `build_plant_variants` and to the
/// agent's `PlantExportHook`.
///
/// # Errors
///
/// [`ExportError`]'s code and message.
pub fn host_write(job: &ExportJob, path: &Path) -> Result<WrittenPlant, PlantBuildError> {
    let report = write_job_to(job, path).map_err(|e| PlantBuildError {
        code: e.code().to_string(),
        message: e.to_string(),
    })?;
    Ok(WrittenPlant {
        triangles_per_lod: report.triangles_per_lod,
        bounds: report.bounds,
    })
}

/// Reads a glTF file's triangles in scene space into one [`MeshData`] named `asset` (an envelope for
/// `plant.envelope_mesh`): every triangle primitive of the default scene, node transforms applied.
/// Nodes named `<name>_LOD<n>` with `n` ≥ 1 (and their children) are skipped, so an exported plant
/// with LODs contributes its LOD 0 only: its LODs overlap, and welded together they would not be a
/// closed surface.
///
/// # Errors
///
/// A message when the file does not read or parse or fails the importer's checks
/// (`gltf_import::open_gltf_checked`: hostile GLB headers, bad accessors, a node graph that is not
/// a forest, buffer URIs that leave the file's folder or project), when the node hierarchy nests
/// deeper than the importer allows or is walked more often than it allows, when an index points
/// past its primitive's vertices, when a primitive's index count is not a multiple of 3, or when
/// the mesh has more than `u32::MAX` vertices.
pub fn host_load_mesh(path: &Path, asset: &str) -> Result<MeshData, String> {
    use regia_core::glam::{Mat4, Vec3};

    /// `<name>_LOD<n>` with `n` ≥ 1.
    fn lower_lod(name: Option<&str>) -> bool {
        name.and_then(|n| n.rsplit_once("_LOD"))
            .filter(|(_, n)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            .is_some_and(|(_, n)| n.bytes().any(|b| b != b'0'))
    }

    /// Nesting depth of `node` (scene roots are 0) and the nodes visited so far: a cycle or a
    /// hostile chain stops with an error instead of overflowing the stack, which aborts the
    /// process (the importer's limits, `gltf_import::MAX_NODE_DEPTH` and `MAX_NODE_VISITS`).
    struct Walk {
        depth: usize,
        visits: usize,
    }

    fn walk(
        node: &gltf::Node<'_>,
        parent: Mat4,
        buffers: &[gltf::buffer::Data],
        out: &mut MeshData,
        state: &mut Walk,
    ) -> Result<(), String> {
        if state.depth > crate::gltf_import::MAX_NODE_DEPTH {
            return Err(format!(
                "node {} is nested deeper than {} levels (or is its own ancestor)",
                node.index(),
                crate::gltf_import::MAX_NODE_DEPTH
            ));
        }
        state.visits += 1;
        if state.visits > crate::gltf_import::MAX_NODE_VISITS {
            return Err(format!(
                "the node hierarchy is walked more than {} times (shared children nested too often)",
                crate::gltf_import::MAX_NODE_VISITS
            ));
        }
        if lower_lod(node.name()) {
            return Ok(());
        }
        let world = parent * Mat4::from_cols_array_2d(&node.transform().matrix());
        if let Some(mesh) = node.mesh() {
            for primitive in mesh
                .primitives()
                .filter(|p| p.mode() == gltf::mesh::Mode::Triangles)
            {
                let reader = primitive.reader(|b| buffers.get(b.index()).map(|d| &d.0[..]));
                let Some(positions) = reader.read_positions() else {
                    continue;
                };
                let before = out.positions.len();
                out.positions
                    .extend(positions.map(|p| world.transform_point3(Vec3::from_array(p)).to_array()));
                let too_many = || format!("mesh {}: more than u32::MAX vertices", mesh.index());
                let base = u32::try_from(before).map_err(|_| too_many())?;
                let count = u32::try_from(out.positions.len() - before).map_err(|_| too_many())?;
                base.checked_add(count).ok_or_else(too_many)?;
                let first_index = out.indices.len();
                match reader.read_indices() {
                    Some(indices) => {
                        for i in indices.into_u32() {
                            if i >= count {
                                return Err(format!(
                                    "mesh {} primitive {}: index {i} past its {count} vertices",
                                    mesh.index(),
                                    primitive.index()
                                ));
                            }
                            out.indices.push(base + i);
                        }
                    }
                    None => out.indices.extend(base..base + count),
                }
                // A partial triangle would shift every later triangle of the envelope.
                let added = out.indices.len() - first_index;
                if !added.is_multiple_of(3) {
                    return Err(format!(
                        "mesh {} primitive {}: {added} indices is not a whole number of triangles",
                        mesh.index(),
                        primitive.index()
                    ));
                }
            }
        }
        state.depth += 1;
        for child in node.children() {
            walk(&child, world, buffers, out, state)?;
        }
        state.depth -= 1;
        Ok(())
    }

    // The importer's checked open (ADR-5869; buffers only, no images): hostile header, indices,
    // accessor ranges, a node forest (the walk above recurses per child; its depth and visit
    // limits stay as a second guard) and URIs that stay beside the file. A plain `gltf::import`
    // panicked on a hostile GLB header, read any absolute or `..` buffer URI and recursed on a
    // node cycle until the stack overflowed.
    let (doc, buffers) =
        crate::gltf_import::open_gltf_checked(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut out = MeshData {
        asset: asset.to_string(),
        positions: Vec::new(),
        normals: Vec::new(),
        indices: Vec::new(),
    };
    if let Some(scene) = doc.default_scene().or_else(|| doc.scenes().next()) {
        let mut state = Walk { depth: 0, visits: 0 };
        for node in scene.nodes() {
            walk(&node, Mat4::IDENTITY, &buffers, &mut out, &mut state)?;
        }
    }
    Ok(out)
}

/// The GLB bytes of `job` and the triangles per LOD. Byte-stable: sorted JSON keys, fixed float
/// formatting and buffer order (spec 68 § 9).
///
/// # Errors
///
/// [`ExportError::NotMeshed`] or [`ExportError::Invalid`].
pub fn glb_bytes(job: &ExportJob) -> Result<(Vec<u8>, Vec<usize>), ExportError> {
    let mesh = &job.mesh;
    if mesh.triangles() == 0 {
        return Err(ExportError::NotMeshed);
    }
    let s = &job.settings;
    if s.lods.is_empty() {
        return Err(ExportError::Invalid("lods is empty".into()));
    }
    // `lod_screen_size` lands in the node extras JSON; serde_json 1.0.151 writes a non-finite
    // number as `null` there, an invalid glTF (asset_pipeline3 correctness audit, finding 3).
    if s.lod_screen_size.iter().any(|v| !v.is_finite()) {
        return Err(ExportError::Invalid(
            "lod_screen_size has a non-finite value".into(),
        ));
    }
    let name = if s.name.is_empty() {
        "plant"
    } else {
        s.name.as_str()
    };
    let mut scene = GlbScene {
        generator: "Regia Engine plant export".into(),
        ..GlbScene::default()
    };
    // One material per slot material name, in slot order.
    let mut material_names: Vec<String> = Vec::new();
    for slot in &mesh.slots {
        if !material_names.contains(&slot.material) {
            material_names.push(slot.material.clone());
        }
    }
    let billboard_material = format!("{name}_billboard");
    if s.billboard {
        material_names.push(billboard_material.clone());
    }
    let impostor = if s.billboard {
        Some(bake_impostor(mesh)?)
    } else {
        None
    };
    if let Some(imp) = &impostor {
        scene.images.push(GlbImage::Embedded {
            bytes: imp.albedo_png.clone(),
            mime: "image/png",
        });
        scene.images.push(GlbImage::Embedded {
            bytes: imp.normal_depth_png.clone(),
            mime: "image/png",
        });
    }
    scene.materials = material_names
        .iter()
        .map(|m| GlbMaterial {
            name: m.clone(),
            double_sided: mesh
                .slots
                .iter()
                .any(|sl| &sl.material == m && sl.kind == SlotKind::Leaf)
                || *m == billboard_material,
            // The baked atlas: coverage in alpha, the two quads read its side captures.
            alpha_blend: impostor.is_some() && *m == billboard_material,
            base_color_texture: (impostor.is_some() && *m == billboard_material).then_some(GlbTextureRef {
                image: 0,
                tex_coord: 0,
            }),
            ..GlbMaterial::default()
        })
        .collect();
    let material_of = |m: &str| material_names.iter().position(|x| x == m);

    let root = GlbNode {
        name: name.to_string(),
        extras: Some(json!({ "shadow_lod": s.shadow_lod })),
        ..GlbNode::default()
    };
    scene.nodes.push(root);
    scene.roots.push(0);
    let lods = s.lods.len();
    let mut triangles_per_lod = Vec::with_capacity(lods);
    for (level, &ratio) in s.lods.iter().enumerate() {
        let mut primitives = Vec::new();
        let mut tris = 0usize;
        for slot in &mesh.slots {
            let indices = match slot.kind {
                SlotKind::Bark => simplify_bark(slot, ratio),
                SlotKind::Leaf => thin_leaves(slot, ratio),
            };
            if indices.is_empty() {
                continue;
            }
            let leaf_scale = if slot.kind == SlotKind::Leaf && ratio < 1.0 {
                (1.0 / ratio.max(0.01)).sqrt().min(2.0)
            } else {
                1.0
            };
            let prim = compact(slot, &indices, leaf_scale, material_of(&slot.material));
            tris += prim.indices.len() / 3;
            primitives.push(prim);
        }
        let last_impostor = impostor.as_ref().filter(|_| level + 1 == lods);
        if let Some(imp) = last_impostor {
            let prim = billboard(imp, material_of(&billboard_material));
            tris += prim.indices.len() / 3;
            primitives.push(prim);
        }
        triangles_per_lod.push(tris);
        let mut extras = Map::new();
        extras.insert("regia_wind_layout".into(), json!(WIND_LAYOUT));
        scene.meshes.push(GlbMesh {
            name: format!("{name}_LOD{level}"),
            primitives,
            extras,
            ..GlbMesh::default()
        });
        let screen = s
            .lod_screen_size
            .get(level)
            .copied()
            .unwrap_or_else(|| default_lod_screen_size(level as u32));
        let mut node_extras = json!({ "lod_screen_size": screen });
        if let Some(imp) = last_impostor {
            node_extras["regia_impostor"] = imp.extras();
        }
        scene.nodes.push(GlbNode {
            name: format!("{name}_LOD{level}"),
            mesh: Some(scene.meshes.len() - 1),
            extras: Some(node_extras),
            ..GlbNode::default()
        });
        let idx = scene.nodes.len() - 1;
        scene.nodes[0].children.push(idx);
    }
    if s.collision == ExportCollision::Capsules {
        for (k, (a, b, r)) in capsules(mesh).into_iter().enumerate() {
            let prim = capsule_mesh(a, b, r);
            scene.meshes.push(GlbMesh {
                name: format!("collision_{name}_{k}"),
                primitives: vec![prim],
                ..GlbMesh::default()
            });
            scene.nodes.push(GlbNode {
                name: format!("collision_{name}_{k}"),
                mesh: Some(scene.meshes.len() - 1),
                extras: Some(json!({ "regia_capsule": [a[0], a[1], a[2], b[0], b[1], b[2], r] })),
                ..GlbNode::default()
            });
            let idx = scene.nodes.len() - 1;
            scene.nodes[0].children.push(idx);
        }
    }
    let bytes = scene.to_glb().map_err(|e| ExportError::Invalid(e.to_string()))?;
    Ok((bytes, triangles_per_lod))
}

/// Bark LOD: meshoptimizer simplification to `ratio` of the triangles (borders locked so ring seams
/// stay closed).
fn simplify_bark(slot: &PlantMeshSlot, ratio: f32) -> Vec<u32> {
    if ratio >= 1.0 {
        return slot.indices.clone();
    }
    let target = ((slot.indices.len() as f32 * ratio) as usize / 3 * 3).max(3);
    simplify_decoder(
        &slot.indices,
        &slot.positions,
        target,
        0.05,
        SimplifyOptions::LockBorder,
        None,
    )
}

/// Leaf LOD: thinning by whole leaves (cards are two triangles; groups of 6 indices), keeping an
/// even spread (`ceil((j + 1) r) > ceil(j r)`: leaf 0 is always kept for
/// `ratio > 0`, so a single card or a tiny ratio never thins to nothing).
fn thin_leaves(slot: &PlantMeshSlot, ratio: f32) -> Vec<u32> {
    if ratio >= 1.0 {
        return slot.indices.clone();
    }
    let r = f64::from(ratio);
    let mut out = Vec::new();
    for (j, leaf) in slot.indices.chunks(6).enumerate() {
        let j = j as f64;
        if ((j + 1.0) * r).ceil() > (j * r).ceil() {
            out.extend_from_slice(leaf);
        }
    }
    out
}

/// A primitive with only the vertices `indices` use (in first-use order); leaves scaled by
/// `leaf_scale` about each leaf's centroid (thinned LODs keep their coverage).
fn compact(slot: &PlantMeshSlot, indices: &[u32], leaf_scale: f32, material: Option<usize>) -> GlbPrimitive {
    let mut remap = vec![u32::MAX; slot.positions.len()];
    let mut prim = GlbPrimitive {
        material,
        uvs: vec![Vec::new()],
        ..GlbPrimitive::default()
    };
    let mut wind = Vec::new();
    let mut pivot = Vec::new();
    for group in indices.chunks(6) {
        let centroid = if leaf_scale != 1.0 {
            let mut c = [0.0f32; 3];
            for &i in group {
                let p = slot.positions[i as usize];
                c = std::array::from_fn(|k| c[k] + p[k] / group.len() as f32);
            }
            Some(c)
        } else {
            None
        };
        for &i in group {
            let iu = i as usize;
            if remap[iu] == u32::MAX {
                remap[iu] = prim.positions.len() as u32;
                let p = slot.positions[iu];
                let p = match centroid {
                    Some(c) => std::array::from_fn(|k| c[k] + (p[k] - c[k]) * leaf_scale),
                    None => p,
                };
                prim.positions.push(p);
                prim.normals
                    .push(slot.normals.get(iu).copied().unwrap_or([0.0, 1.0, 0.0]));
                prim.uvs[0].push(slot.uvs.get(iu).copied().unwrap_or([0.0, 0.0]));
                wind.push(slot.wind.get(iu).copied().unwrap_or([0.0; 4]));
                pivot.push(slot.wind_pivot.get(iu).copied().unwrap_or([0.0; 3]));
            }
            prim.indices.push(remap[iu]);
        }
    }
    prim.custom_vec4.push(("_WIND".into(), wind));
    prim.custom_vec3.push(("_WIND_PIVOT".into(), pivot));
    prim
}

/// Linear vertex colors of the impostor bake: slots name material placeholders and no bark or leaf
/// sheet is baked (spec 68 § 12), so the two kinds differ by a neutral tint. The material's base
/// color multiplies this at draw time, as it does on the mesh LODs.
const BARK_TINT: [f32; 4] = [0.30, 0.22, 0.15, 1.0];
const LEAF_TINT: [f32; 4] = [0.22, 0.38, 0.12, 1.0];

/// A plant's baked impostor: the atlas of `grid²` captures as PNG, and the sphere they frame.
struct PlantImpostor {
    settings: ImpostorSettings,
    centre_y: f32,
    radius: f32,
    /// Height of the plant's base: the sway pivot of the quads.
    base_y: f32,
    albedo_png: Vec<u8>,
    normal_depth_png: Vec<u8>,
}

impl PlantImpostor {
    /// The extras of the last LOD node: what an importer needs to draw the full octahedral card.
    fn extras(&self) -> Value {
        json!({
            "grid": self.settings.grid,
            "cell": self.settings.cell,
            "centre_y": self.centre_y,
            "radius": self.radius,
            "albedo_image": 0,
            "normal_depth_image": 1,
        })
    }
}

/// Bakes the impostor of LOD 0 through `regia_terrain::scatter::impostor` (a deterministic CPU
/// rasterizer). The bake frames a sphere on the pivot axis, so the radius covers the farthest vertex
/// from `(0, centre_y, 0)`, not the bounds' own center.
fn bake_impostor(mesh: &PlantMesh) -> Result<PlantImpostor, ExportError> {
    let (lo, hi) = mesh.bounds();
    let centre_y = (lo[1] + hi[1]) * 0.5;
    if !centre_y.is_finite() {
        return Err(ExportError::Invalid(
            "a plant vertex is non-finite; the impostor was not baked".into(),
        ));
    }
    let mut vertices: Vec<LabVertex> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    let mut radius2 = 0.0f32;
    for slot in &mesh.slots {
        let base = u32::try_from(vertices.len()).map_err(|_| {
            ExportError::Invalid("a plant has more vertices than a glTF index can name".into())
        })?;
        let tint = match slot.kind {
            SlotKind::Bark => BARK_TINT,
            SlotKind::Leaf => LEAF_TINT,
        };
        for (k, &position) in slot.positions.iter().enumerate() {
            let d = [position[0], position[1] - centre_y, position[2]];
            radius2 = radius2.max(d[0] * d[0] + d[1] * d[1] + d[2] * d[2]);
            vertices.push(LabVertex {
                position,
                normal: slot.normals.get(k).copied().unwrap_or([0.0, 1.0, 0.0]),
                uv: slot.uvs.get(k).copied().unwrap_or([0.0, 0.0]),
                tangent: [1.0, 0.0, 0.0, 1.0],
                color: tint,
                joints: [0; 4],
                weights: [1.0, 0.0, 0.0, 0.0],
            });
        }
        for index in &slot.indices {
            let Some(next) = index.checked_add(base) else {
                return Err(ExportError::Invalid("a plant index does not fit in u32".into()));
            };
            indices.push(next);
        }
    }
    let radius = radius2.sqrt().max(0.01);
    if !radius.is_finite() {
        return Err(ExportError::Invalid(
            "a plant vertex is non-finite; the impostor was not baked".into(),
        ));
    }
    // Twice the far-scatter default per capture: a plant is tall and thin, and at 32 pixels a 16 m
    // conifer is four pixels wide.
    let settings = ImpostorSettings {
        cell: 64,
        ..ImpostorSettings::default()
    }
    .sanitized();
    let material = LabMaterial::default();
    let source = ImpostorSource {
        parts: vec![ImpostorPart {
            vertices: &vertices,
            indices: &indices,
            material: &material,
            colour: [1.0; 3],
        }],
        centre_y,
        radius,
    };
    let card = bake_cards(&[source], &settings).into_iter().next();
    let Some(ImpostorCard {
        albedo, normal_depth, ..
    }) = card
    else {
        return Err(ExportError::Invalid("the impostor bake gave no card".into()));
    };
    let side = settings.grid * settings.cell;
    let png = |levels: &[Vec<u8>]| -> Result<Vec<u8>, ExportError> {
        let top = levels
            .first()
            .ok_or_else(|| ExportError::Invalid("the impostor bake gave no image".into()))?;
        png_rgba8(top, side).map_err(|e| ExportError::Invalid(format!("impostor image: {e}")))
    };
    Ok(PlantImpostor {
        settings,
        centre_y,
        radius,
        base_y: lo[1],
        albedo_png: png(&albedo)?,
        normal_depth_png: png(&normal_depth)?,
    })
}

/// An RGBA8 image as PNG bytes (fixed encoder settings: same pixels, same bytes).
fn png_rgba8(rgba: &[u8], side: u32) -> Result<Vec<u8>, image::ImageError> {
    use image::ImageEncoder;
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out).write_image(
        rgba,
        side,
        side,
        image::ExtendedColorType::Rgba8,
    )?;
    Ok(out)
}

/// The capture whose view direction is closest to `want`: `(i, j, direction)` (first in grid order
/// on a tie).
fn nearest_capture(grid: u32, want: [f32; 3]) -> (u32, u32, [f32; 3]) {
    let mut best = (0, 0, capture_direction(0, 0, grid));
    let mut best_dot = f32::NEG_INFINITY;
    for j in 0..grid {
        for i in 0..grid {
            let d = capture_direction(i, j, grid);
            let dot = d[0] * want[0] + d[1] * want[1] + d[2] * want[2];
            if dot > best_dot {
                best_dot = dot;
                best = (i, j, d);
            }
        }
    }
    best
}

/// Two crossed quads textured with the atlas's captures nearest +Z and +X (the far billboard part).
/// Each quad lies in its capture's image plane through the framed sphere's center, spans the sphere,
/// and maps the capture's cell in the capture's own axes, so a texel lands where the bake saw it.
fn billboard(imp: &PlantImpostor, material: Option<usize>) -> GlbPrimitive {
    let grid = imp.settings.grid;
    let mut prim = GlbPrimitive {
        material,
        uvs: vec![Vec::new()],
        ..GlbPrimitive::default()
    };
    let mut wind = Vec::new();
    let mut pivot = Vec::new();
    for want in [[0.0f32, 0.0, 1.0], [1.0, 0.0, 0.0]] {
        let (i, j, forward) = nearest_capture(grid, want);
        let (right, up, normal) = capture_basis(forward);
        let base = prim.positions.len() as u32;
        for (x, y) in [(-1.0f32, -1.0f32), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            prim.positions.push(std::array::from_fn(|k| {
                let centre = if k == 1 { imp.centre_y } else { 0.0 };
                centre + (right[k] * x + up[k] * y) * imp.radius
            }));
            prim.normals.push(normal);
            prim.uvs[0].push([
                (i as f32 + x * 0.5 + 0.5) / grid as f32,
                (j as f32 + 0.5 - y * 0.5) / grid as f32,
            ]);
            wind.push([0.0, 0.0, 1.0, 0.0]);
            pivot.push([0.0, imp.base_y, 0.0]);
        }
        prim.indices
            .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    prim.custom_vec4.push(("_WIND".into(), wind));
    prim.custom_vec3.push(("_WIND_PIVOT".into(), pivot));
    prim
}

/// One capsule `(a, b, radius)` per trunk and order-1 axis whose base radius is over 5 cm.
fn capsules(mesh: &PlantMesh) -> Vec<([f32; 3], [f32; 3], f32)> {
    let mut out = Vec::new();
    for p in &mesh.skeleton.plants {
        for axis in p.axes() {
            let first = &p.nodes[axis.nodes[0]];
            if first.order > 1 || first.radius <= 0.05 {
                continue;
            }
            let start = axis.attach.map_or(first.position, |a| p.nodes[a].position);
            let end = axis
                .nodes
                .iter()
                .take_while(|&&i| p.nodes[i].radius > 0.05)
                .last()
                .map_or(first.position, |&i| p.nodes[i].position);
            out.push((
                start.map(|x| x as f32),
                end.map(|x| x as f32),
                first.radius as f32,
            ));
        }
    }
    out
}

/// A closed six-sided prism from `a` to `b` (the collision proxy of a capsule).
fn capsule_mesh(a: [f32; 3], b: [f32; 3], r: f32) -> GlbPrimitive {
    let d = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let l = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt().max(1e-6);
    let t = [d[0] / l, d[1] / l, d[2] / l];
    let refv = if t[1].abs() < 0.99 {
        [0.0, 1.0, 0.0]
    } else {
        [1.0, 0.0, 0.0]
    };
    let cross = |x: [f32; 3], y: [f32; 3]| {
        [
            x[1] * y[2] - x[2] * y[1],
            x[2] * y[0] - x[0] * y[2],
            x[0] * y[1] - x[1] * y[0],
        ]
    };
    let u = cross(refv, t);
    let ul = (u[0] * u[0] + u[1] * u[1] + u[2] * u[2]).sqrt().max(1e-6);
    let u = [u[0] / ul, u[1] / ul, u[2] / ul];
    let v = cross(t, u);
    // Hexagon offsets: cos/sin of k·60° (exact constants, no libm).
    const H: [(f32, f32); 6] = [
        (1.0, 0.0),
        (0.5, 0.866_025_4),
        (-0.5, 0.866_025_4),
        (-1.0, 0.0),
        (-0.5, -0.866_025_4),
        (0.5, -0.866_025_4),
    ];
    let mut prim = GlbPrimitive::default();
    for end in [a, b] {
        for (c, s) in H {
            prim.positions
                .push(std::array::from_fn(|k| end[k] + (u[k] * c + v[k] * s) * r));
        }
    }
    for k in 0..6u32 {
        let n = (k + 1) % 6;
        prim.indices.extend_from_slice(&[k, n, 6 + k, n, 6 + n, 6 + k]);
    }
    for k in 1..5u32 {
        prim.indices
            .extend_from_slice(&[0, k + 1, k, 6, 6 + k, 6 + k + 1]);
    }
    prim
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use regia_procgen::plant::{ExportSettings, PlantMesh, PlantMeshSlot, PlantSkeleton, SlotKind};

    use super::*;

    #[test]
    fn failed_rename_leaves_no_temp_file() {
        let dir = regia_test_utils::unique_temp_dir("plant_export_tmp");
        let target = dir.join("plant.glb");
        std::fs::create_dir(&target).expect("dir at the destination");
        assert!(write_job_to(&sample(3, 2), &target).is_err());
        let names: Vec<_> = std::fs::read_dir(&dir)
            .expect("read dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("plant.glb")], "{names:?}");
    }

    #[test]
    fn thin_leaves_keeps_at_least_one_card_for_any_positive_ratio() {
        let mut slot = PlantMeshSlot::new("leaf", SlotKind::Leaf);
        // Three cards of 6 indices.
        slot.indices = (0..18).collect();
        for ratio in [0.9f32, 0.5, 0.25, 0.01] {
            let kept = thin_leaves(&slot, ratio);
            assert_eq!(kept.len() % 6, 0);
            assert!(!kept.is_empty(), "ratio {ratio} dropped every card");
            assert_eq!(kept.len() / 6, (3.0 * f64::from(ratio)).ceil() as usize);
        }
        // A single card survives every positive ratio.
        slot.indices = (0..6).collect();
        assert_eq!(thin_leaves(&slot, 0.25), (0..6).collect::<Vec<u32>>());
        assert!(thin_leaves(&slot, 0.0).is_empty());
        assert_eq!(thin_leaves(&slot, 1.0).len(), 6);
    }

    /// One hundred cards at ratio `0.05`. That f32 is slightly above 1/20, so
    /// `ceil((j+1)·r) > ceil(j·r)` keeps six cards (0, 19, 39, 59, 79, 99):
    /// `ceil(n·r)` plus the extra step the float causes (specs/68 § 6.3).
    #[test]
    fn thin_leaves_keeps_the_cards_the_ceil_rule_selects_at_ratio_0_05() {
        let mut slot = PlantMeshSlot::new("leaf", SlotKind::Leaf);
        slot.indices = (0..600).collect();
        let kept = thin_leaves(&slot, 0.05);
        assert_eq!(kept.len() % 6, 0);
        assert_eq!(kept.len() / 6, 6);
        let cards: Vec<u32> = kept.chunks(6).map(|card| card[0] / 6).collect();
        assert_eq!(cards, vec![0, 19, 39, 59, 79, 99]);
    }

    /// A bark tube of `rings` rings × 8 sides plus `leaves` leaf cards.
    fn sample(rings: u32, leaves: u32) -> ExportJob {
        let mut bark = PlantMeshSlot::new("bark", SlotKind::Bark);
        let sides = 8u32;
        for k in 0..rings {
            for s in 0..=sides {
                let a = std::f32::consts::TAU * (s % sides) as f32 / sides as f32;
                let (sn, cs) = a.sin_cos();
                bark.positions.push([0.1 * cs, k as f32 * 0.2, 0.1 * sn]);
                bark.normals.push([cs, 0.0, sn]);
                bark.uvs.push([s as f32 / sides as f32, k as f32 * 0.2]);
                bark.wind.push([0.0, 0.5, 1.0, 0.0]);
                bark.wind_pivot.push([0.0; 3]);
            }
        }
        let ring = sides + 1;
        for k in 0..rings - 1 {
            for s in 0..sides {
                let a = k * ring + s;
                bark.indices
                    .extend_from_slice(&[a, a + 1, a + ring, a + 1, a + ring + 1, a + ring]);
            }
        }
        let mut leaf = PlantMeshSlot::new("leaf", SlotKind::Leaf);
        for j in 0..leaves {
            let base = leaf.positions.len() as u32;
            let y = 1.0 + j as f32 * 0.01;
            for p in [
                [0.0, y, 0.0],
                [0.05, y, 0.0],
                [0.05, y + 0.1, 0.0],
                [0.0, y + 0.1, 0.0],
            ] {
                leaf.positions.push(p);
                leaf.normals.push([0.0, 0.0, 1.0]);
                leaf.uvs.push([0.0, 0.0]);
                leaf.wind.push([3.0, 0.2, 0.1, 1.0]);
                leaf.wind_pivot.push([0.0, 0.5, 0.0]);
            }
            leaf.indices
                .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
        }
        ExportJob {
            node: 1,
            mesh: Arc::new(PlantMesh {
                slots: vec![bark, leaf],
                skeleton: Arc::new(PlantSkeleton::default()),
                ..PlantMesh::default()
            }),
            settings: ExportSettings::default(),
            out: String::new(),
        }
    }

    #[test]
    fn lods_decrease_and_the_glb_is_byte_stable() {
        let job = sample(40, 200);
        let (a, tris) = glb_bytes(&job).expect("writes");
        let (b, _) = glb_bytes(&job).expect("writes");
        assert_eq!(a, b, "byte-stable");
        assert_eq!(tris.len(), 4);
        assert!(
            tris.windows(2).all(|w| w[1] < w[0]),
            "triangles per LOD decrease: {tris:?}"
        );
        let (doc, _, _) = gltf::import_slice(&a).expect("valid glTF");
        let names: Vec<String> = doc.nodes().filter_map(|n| n.name().map(str::to_string)).collect();
        for k in 0..4 {
            assert!(names.contains(&format!("plant_LOD{k}")), "{names:?}");
        }
    }

    #[test]
    fn empty_meshes_are_not_exported() {
        let mut job = sample(2, 0);
        job.mesh = Arc::new(PlantMesh::default());
        assert!(matches!(glb_bytes(&job), Err(ExportError::NotMeshed)));
    }

    /// An infinite vertex makes the impostor radius non-finite, which serde_json 1.0.151 would
    /// write as `null` — an invalid glTF. The export returns `export_invalid` instead.
    #[test]
    fn audfix_an_infinite_vertex_is_an_export_error_not_a_panic() {
        let mut job = sample(2, 1);
        let mesh = Arc::make_mut(&mut job.mesh);
        mesh.slots[1].positions[0][1] = f32::INFINITY;
        let err = glb_bytes(&job).expect_err("infinite vertex");
        assert_eq!(err.code(), "export_invalid");
    }
}
