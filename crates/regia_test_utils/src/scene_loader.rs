// SPDX-License-Identifier: MPL-2.0
//! Load glTF 2.0 scenes for golden tests (`specs/13` § 13).
//!
//! Finds models from the Khronos glTF-Sample-Assets repository in a gitignored cache (or
//! `REGIA_KHRONOS_ASSETS`) and converts them into [`regia_render::LabScene`] objects
//! for headless rendering. Nothing is downloaded here: `cargo xtask fetch-test-assets` fills the cache.
//!
//! Skinned meshes are converted to world-space joint palettes (`LabMesh::joints`
//! with an identity model matrix), so the lab frame's GPU skinning path is
//! exercised exactly like the runtime's.

use glam::{Mat4, Quat, Vec3};
use regia_render::{
    ALPHA_BLEND, ALPHA_MASK, ALPHA_OPAQUE, DirectionalLightUniform, JointMatrix, LabMaterial, LabMesh,
    LabScene, LabTexture, LabVertex, MAX_JOINTS_PER_SKIN, generate_vertex_normals, generate_vertex_tangents,
    skin_position,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Cache path
// ---------------------------------------------------------------------------

/// Directory where glTF models are cached (gitignored).
fn cache_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("gltf-cache")
}

/// Ensure a glTF model is in the cache.
///
/// * `model` — model name (e.g. `"Sponza"`, `"DamagedHelmet"`, `"Fox"`).
///
/// Looks for the model, in order: the local `gltf-cache` directory, then the
/// `REGIA_KHRONOS_ASSETS` cache (both the `Models/<name>/glTF[-Binary]`
/// layout used by a full glTF-Sample-Assets checkout and the flat layout the
/// `xtask fetch-test-assets` cache uses). Never touches the network: when the
/// model is nowhere to be found, returns `Err` so tests skip cleanly.
pub fn ensure_model(model: &str) -> Result<PathBuf, String> {
    let dir = cache_dir();
    let has_glb = matches!(
        model,
        "DamagedHelmet" | "Fox" | "FlightHelmet" | "MetalRoughSpheres"
    );
    let filename = if has_glb {
        format!("{model}.glb")
    } else {
        format!("{model}.gltf")
    };

    // Local crate cache (gitignored): flat (`Fox.glb`) or one-directory
    // (`Sponza/Sponza.gltf`, the Khronos sample download layout).
    if dir.join(&filename).exists() {
        return Ok(dir.join(&filename));
    }
    if dir.join(model).join(&filename).exists() {
        return Ok(dir.join(model).join(&filename));
    }

    let khr = khronos_assets_dir();
    if let Some(khr) = &khr {
        // Flat layout (xtask fetch-test-assets).
        if khr.join(&filename).exists() {
            return Ok(khr.join(&filename));
        }
        // glTF-Sample-Assets checkout layout.
        let sub = if has_glb { "glTF-Binary" } else { "glTF" };
        let nested = khr.join("Models").join(model).join(sub).join(&filename);
        if nested.exists() {
            return Ok(nested);
        }
    }
    Err(format!(
        "{model} ({filename}) not found in {dir:?} or REGIA_KHRONOS_ASSETS; \
         run `cargo xtask fetch-test-assets` or set REGIA_KHRONOS_ASSETS"
    ))
}

/// Returns the `REGIA_KHRONOS_ASSETS` directory if set and it exists.
fn khronos_assets_dir() -> Option<PathBuf> {
    regia_core::brand::env_var("REGIA_KHRONOS_ASSETS")
        .ok()
        .map(PathBuf::from)
        .filter(|p| p.exists())
}

// ---------------------------------------------------------------------------
// Camera views
// ---------------------------------------------------------------------------

/// How [`load_gltf_model`] frames the loaded scene.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LabView {
    /// Auto-frame: camera outside the bounding sphere looking at the centre.
    #[default]
    Auto,
    /// Inside the Sponza atrium, standing at one end of the nave and looking
    /// along it (columns, arches and floor fill the frame).
    SponzaAtrium,
    /// A portrait shot of a small skinned model (e.g. the Fox), framed so the
    /// model fills a good part of the frame against the background.
    ModelPortrait,
}

// ---------------------------------------------------------------------------
// Scene loading
// ---------------------------------------------------------------------------

/// Errors from glTF scene loading.
#[derive(Debug)]
pub enum LoadError {
    /// glTF parsing failed.
    Gltf(String),
    /// The file has no meshes.
    NoGeo,
    /// A primitive uses non-triangle topology.
    NonTriangles,
    /// The file has no default scene and no scenes at all.
    NoScene,
    /// The requested view does not fit the model's bounds.
    BadView(String),
    /// The requested animation clip is not in the file.
    NoClip {
        /// The clip name that was asked for.
        wanted: String,
        /// The names of the animations the file does have.
        available: Vec<String>,
    },
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Gltf(s) => write!(f, "glTF parse: {s}"),
            Self::NoGeo => write!(f, "no geometry in glTF file"),
            Self::NonTriangles => write!(f, "non-triangle primitive"),
            Self::NoScene => write!(f, "no scene in glTF file"),
            Self::BadView(s) => write!(f, "view does not fit the model: {s}"),
            Self::NoClip { wanted, available } => {
                write!(f, "clip {wanted:?} not found (available: {available:?})")
            }
        }
    }
}

/// Parsed skin data.
struct ParsedSkin {
    inverse_bind_matrices: Vec<Mat4>,
    joint_node_indices: Vec<usize>,
}

/// Load a glTF model with automatic framing (see [`LabView::Auto`]).
pub fn load_gltf_model(path: &Path) -> Result<LabScene, LoadError> {
    load_gltf_model_with_view(path, LabView::Auto, None)
}

/// Load a glTF model, framing it with `view` and optionally sampling `clip`
/// (an animation name) at `time` seconds so skinned meshes land in a stored
/// non-bind pose (`specs/13` § 13).
pub fn load_gltf_model_with_view(
    path: &Path,
    view: LabView,
    clip: Option<(&str, f32)>,
) -> Result<LabScene, LoadError> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let (document, buffers, images) = if ext.eq_ignore_ascii_case("glb") {
        let bytes = std::fs::read(path).map_err(|e| LoadError::Gltf(e.to_string()))?;
        gltf::import_slice(&bytes).map_err(|e| LoadError::Gltf(e.to_string()))?
    } else {
        gltf::import(path).map_err(|e| LoadError::Gltf(e.to_string()))?
    };
    let mut scene = load_gltf_scene_from_parts(&document, &buffers, &images, view, clip)?;
    // The Khronos `Sponza` glTF (generator "glTF-Transform v1.2.3") is inverted
    // as a whole: its triangles AND vertex normals are flipped relative to the
    // convention of every other sample asset (Fox and DamagedHelmet render
    // correctly as loaded; a normals-vs-winding vote finds Sponza
    // self-consistent, so the inversion only shows in the render — the atrium
    // view comes out empty). Flip both winding and normals for this asset so
    // the interior is front-facing and sun-lit.
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
    if stem.eq_ignore_ascii_case("Sponza") {
        for mesh in &mut scene.meshes {
            // A malformed index list may end in a partial triangle: leave it.
            let (tris, _tail) = mesh.indices.as_chunks_mut::<3>();
            for tri in tris {
                tri.swap(1, 2);
            }
        }
    }
    Ok(scene)
}

/// Load from pre-parsed glTF document parts with automatic framing.
fn load_gltf_scene_from_parts(
    document: &gltf::Document,
    buffers: &[gltf::buffer::Data],
    images: &[gltf::image::Data],
    view: LabView,
    clip: Option<(&str, f32)>,
) -> Result<LabScene, LoadError> {
    let scene = document
        .default_scene()
        .or_else(|| document.scenes().next())
        .ok_or(LoadError::NoScene)?;

    // --- Node local transforms (possibly overridden by a sampled clip) ---
    let mut local: Vec<Mat4> = document.nodes().map(|n| node_transform(&n)).collect();
    if let Some((clip_name, time)) = clip {
        sample_clip(document, buffers, clip_name, time, &mut local)?;
    }

    // --- World transforms: recursive traversal from the scene roots ---
    // (A previous version propagated transforms in document-node order, which
    // silently squared every parent's local matrix and shrank hierarchical
    // models like the Fox to invisibility.)
    let mut world: Vec<Mat4> = vec![Mat4::IDENTITY; local.len()];
    for root in scene.nodes() {
        propagate(&root, Mat4::IDENTITY, &local, &mut world);
    }

    // --- Skins ---
    let mut skins: Vec<ParsedSkin> = Vec::new();
    for skin in document.skins() {
        let ibm: Vec<Mat4> = skin
            .reader(|buf| buffers.get(buf.index()).map(|data| &data.0[..]))
            .read_inverse_bind_matrices()
            .map(|iter| iter.map(|m| Mat4::from_cols_array_2d(&m)).collect())
            .unwrap_or_else(|| vec![Mat4::IDENTITY; skin.joints().len()]);
        let joints: Vec<usize> = skin.joints().map(|n| n.index()).collect();
        skins.push(ParsedSkin {
            inverse_bind_matrices: ibm,
            joint_node_indices: joints,
        });
    }

    // --- Materials ---
    let mut materials: Vec<LabMaterial> = Vec::new();
    let mut texture_cache: HashMap<usize, LabTexture> = HashMap::new();

    for mat in document.materials() {
        let pbr = mat.pbr_metallic_roughness();
        let base_color = pbr.base_color_factor();
        let emissive = mat.emissive_factor();
        let alpha_mode = match mat.alpha_mode() {
            gltf::material::AlphaMode::Opaque => ALPHA_OPAQUE,
            gltf::material::AlphaMode::Mask => ALPHA_MASK,
            gltf::material::AlphaMode::Blend => ALPHA_BLEND,
        };
        let alpha_cutoff = mat.alpha_cutoff().unwrap_or(0.5);

        let albedo = pbr
            .base_color_texture()
            .and_then(|info| load_texture(&info.texture(), images, &mut texture_cache));
        let normal_map = mat
            .normal_texture()
            .and_then(|info| load_texture(&info.texture(), images, &mut texture_cache));
        let occlusion_map = mat
            .occlusion_texture()
            .and_then(|info| load_texture(&info.texture(), images, &mut texture_cache));
        let emissive_map = mat
            .emissive_texture()
            .and_then(|info| load_texture(&info.texture(), images, &mut texture_cache));

        materials.push(LabMaterial {
            base_color: [base_color[0], base_color[1], base_color[2], base_color[3]],
            metallic: pbr.metallic_factor(),
            roughness: pbr.roughness_factor(),
            normal_scale: mat.normal_texture().map_or(1.0, |t| t.scale()),
            occlusion_strength: mat.occlusion_texture().map_or(1.0, |t| t.strength()),
            emissive: [emissive[0], emissive[1], emissive[2]],
            emissive_intensity: 1.0,
            alpha_mode,
            alpha_cutoff,
            unlit: false,
            albedo,
            normal_map,
            occlusion_map,
            emissive_map,
            ..LabMaterial::default()
        });
    }

    if materials.is_empty() {
        materials.push(LabMaterial::default());
    }

    // --- Meshes ---
    let mut lab_meshes: Vec<LabMesh> = Vec::new();
    let mut packed_joints: Vec<JointMatrix> = vec![JointMatrix::identity()];
    let mut skin_base: HashMap<usize, u32> = HashMap::new();

    for root in scene.nodes() {
        collect_meshes(
            &root,
            &world,
            buffers,
            &skins,
            &mut packed_joints,
            &mut skin_base,
            &mut lab_meshes,
        );
    }

    if lab_meshes.is_empty() {
        return Err(LoadError::NoGeo);
    }

    let mut scene = build_lab_scene(lab_meshes, materials, view)?;
    // Interior vs exterior exposure: the atrium needs about two stops more than the
    // outdoor "sunny 16" value because its surfaces are shaded by the roof and
    // only the oculus shafts and the sky through the ends light the hall. Since
    // the CSM eye/z-convention fixes (4e41d44) the sun really is occluded by the
    // roof, so the nave is sky-ambient lit (direct sun reaches ~7% of the frame,
    // measured); EV 11.5 (~3.5 stops over sunny 16) exposes the shaded interior
    // without blowing out the oculus sun shafts (0.9% near-white, 0.7% near-black).
    scene.exposure_ev = match view {
        LabView::SponzaAtrium => 11.5,
        _ => 14.5,
    };
    Ok(scene)
}

/// Recursively propagate world transforms: `world[child] = world[parent] * local[child]`.
fn propagate(node: &gltf::Node<'_>, parent: Mat4, local: &[Mat4], world: &mut [Mat4]) {
    let own = parent * local[node.index()];
    world[node.index()] = own;
    for child in node.children() {
        propagate(&child, own, local, world);
    }
}

/// Sample one animation clip by name at `time`, overriding node local transforms.
///
/// Each channel (translation/rotation/scale) updates only its own component of the
/// node's TRS — setting the whole matrix per channel would let the last channel
/// clobber the others and collapse the pose. Supports LINEAR and STEP
/// interpolation (what the Khronos sample models use); CUBICSPLINE channels keep
/// the bind pose.
fn sample_clip(
    document: &gltf::Document,
    buffers: &[gltf::buffer::Data],
    clip_name: &str,
    time: f32,
    local: &mut [Mat4],
) -> Result<(), LoadError> {
    // A file without animations keeps the bind pose; a file with animations but not this name
    // is a typo in the caller, so it fails instead of posing the wrong take.
    if document.animations().next().is_none() {
        return Ok(());
    }
    let Some(animation) = document.animations().find(|a| a.name() == Some(clip_name)) else {
        return Err(LoadError::NoClip {
            wanted: clip_name.to_owned(),
            available: document
                .animations()
                .filter_map(|a| a.name().map(str::to_owned))
                .collect(),
        });
    };
    // Per-node TRS overrides, composed after all channels are read.
    let mut translations: HashMap<usize, Vec3> = HashMap::new();
    let mut rotations: HashMap<usize, Quat> = HashMap::new();
    let mut scales: HashMap<usize, Vec3> = HashMap::new();
    let mut bind_order: Vec<usize> = Vec::new();
    for channel in animation.channels() {
        let node = channel.target().node();
        let reader = channel.reader(|buf| buffers.get(buf.index()).map(|data| &data.0[..]));
        let Some(times) = reader.read_inputs() else {
            continue;
        };
        let times: Vec<f32> = times.collect();
        let (i0, i1, t) = keyframe_span(&times, time);
        // The output accessor may be shorter than the input accessor.
        let out_len = match reader.read_outputs() {
            Some(gltf::animation::util::ReadOutputs::Translations(v)) => v.len(),
            Some(gltf::animation::util::ReadOutputs::Scales(v)) => v.len(),
            Some(gltf::animation::util::ReadOutputs::Rotations(v)) => v.into_f32().count(),
            _ => 0,
        };
        if i0.max(i1) >= out_len {
            continue;
        }
        let to_quat = |q: [f32; 4]| Quat::from_array(q);
        match reader.read_outputs() {
            Some(gltf::animation::util::ReadOutputs::Translations(values)) => {
                let values: Vec<[f32; 3]> = values.collect();
                let v0 = Vec3::from_array(values[i0]);
                let v1 = Vec3::from_array(values[i1]);
                translations.insert(node.index(), v0.lerp(v1, t));
                bind_order.push(node.index());
            }
            Some(gltf::animation::util::ReadOutputs::Scales(values)) => {
                let values: Vec<[f32; 3]> = values.collect();
                let v0 = Vec3::from_array(values[i0]);
                let v1 = Vec3::from_array(values[i1]);
                scales.insert(node.index(), v0.lerp(v1, t));
                bind_order.push(node.index());
            }
            Some(gltf::animation::util::ReadOutputs::Rotations(values)) => {
                let values: Vec<[f32; 4]> = values.into_f32().collect();
                let r0 = to_quat(values[i0]);
                let r1 = to_quat(values[i1]);
                // Nearest-compatible slerp (SHORTPATH for LINEAR, NONE for STEP).
                let rotation = if t <= 0.0 {
                    r0
                } else if t >= 1.0 {
                    r1
                } else {
                    r0.slerp(r1, t)
                };
                rotations.insert(node.index(), rotation);
                bind_order.push(node.index());
            }
            _ => {}
        }
    }
    for node_index in bind_order {
        let (t, r, s) = (
            translations.remove(&node_index),
            rotations.remove(&node_index),
            scales.remove(&node_index),
        );
        if t.is_none() && r.is_none() && s.is_none() {
            continue;
        }
        // Decompose the current local matrix, override the animated components,
        // recompose. glTF nodes are TRS so this round-trips exactly.
        // (glam returns scale first, translation last.)
        let (cur_s, cur_r, cur_t) = decompose_trs(local[node_index]);
        let translation = t.unwrap_or(cur_t);
        let rotation = r.unwrap_or(cur_r);
        let scale = s.unwrap_or(cur_s);
        local[node_index] =
            Mat4::from_translation(translation) * Mat4::from_quat(rotation) * Mat4::from_scale(scale);
    }
    Ok(())
}

/// Decomposes a TRS-composed matrix back into (translation, rotation, scale).
fn decompose_trs(matrix: Mat4) -> (Vec3, Quat, Vec3) {
    let (t, r, s) = matrix.to_scale_rotation_translation();
    (t, r, s)
}

/// Find the keyframe pair surrounding `time` and the blend factor between them.
///
/// At an exact keyframe hit, the span starts at that keyframe (`t = 0`).
fn keyframe_span(times: &[f32], time: f32) -> (usize, usize, f32) {
    if times.is_empty() {
        return (0, 0, 0.0);
    }
    let last = times.len() - 1;
    if last == 0 || !time.is_finite() || !times[0].is_finite() || !times[last].is_finite() {
        return (0, 0, 0.0);
    }
    if time <= times[0] {
        return (0, 0, 0.0);
    }
    if time >= times[last] {
        return (last, last, 0.0);
    }
    // Largest index whose time is <= the sample time.
    let i = times
        .iter()
        .take_while(|&&t| t <= time)
        .count()
        .saturating_sub(1)
        .min(last.saturating_sub(1));
    let span = (times[i + 1] - times[i]).max(1e-6);
    (i, i + 1, (time - times[i]) / span)
}

/// Build a [`LabScene`] from loaded meshes and materials.
fn build_lab_scene(
    lab_meshes: Vec<LabMesh>,
    materials: Vec<LabMaterial>,
    view: LabView,
) -> Result<LabScene, LoadError> {
    // --- Compute the world-space bounding box of everything drawn ---
    let mut bounds_min = Vec3::splat(f32::MAX);
    let mut bounds_max = Vec3::splat(f32::MIN);
    let mut skinned_min = Vec3::splat(f32::MAX);
    let mut skinned_max = Vec3::splat(f32::MIN);
    for mesh in &lab_meshes {
        let model = Mat4::from_cols_array_2d(&mesh.model);
        for v in &mesh.vertices {
            let wp = model.transform_point3(Vec3::from_array(v.position));
            bounds_min = bounds_min.min(wp);
            bounds_max = bounds_max.max(wp);
            if mesh.joints.is_empty() {
                continue;
            }
            let sp = skinned_world_position(v, &mesh.joints);
            skinned_min = skinned_min.min(sp);
            skinned_max = skinned_max.max(sp);
        }
    }

    let (center, extent) = if skinned_min.x < f32::MAX {
        (
            (skinned_min + skinned_max) * 0.5,
            (skinned_max - skinned_min).length(),
        )
    } else {
        (
            (bounds_min + bounds_max) * 0.5,
            (bounds_max - bounds_min).length(),
        )
    };
    if extent <= 0.0 || !extent.is_finite() {
        return Err(LoadError::BadView("model has no finite extent".into()));
    }

    let (eye, target, fov_y) = match view {
        LabView::Auto => {
            let distance = extent / (50.0_f32.to_radians().tan()) * 1.05;
            let eye = center + Vec3::new(0.3 * distance, 0.35 * distance, distance);
            (eye, center, 50.0_f32.to_radians())
        }
        LabView::SponzaAtrium => {
            // Inside the nave: the building's long horizontal axis is the nave;
            // stand near one end of the open hall at eye height above the floor,
            // level with the horizon, looking down the middle toward the far
            // end — columns and arches on both sides, floor and far wall in
            // frame (the round-4 camera sat 5.8 m up and 12 m out near the
            // end wall and photographed the wall itself).
            let size = bounds_max - bounds_min;
            let axis = if size.x >= size.z { Vec3::X } else { Vec3::Z };
            let length = size.x.max(size.z);
            let floor = bounds_min.y;
            let eye_y = floor + (bounds_max.y - floor) * 0.14;
            let eye = Vec3::new(center.x, eye_y, center.z) - axis * length * 0.36;
            let target = Vec3::new(center.x, eye_y, center.z) + axis * length * 0.32;
            (eye, target, 60.0_f32.to_radians())
        }
        LabView::ModelPortrait => {
            // Portrait: the model fills a good part of the frame from slightly
            // above and in front, against the background.
            let radius = extent * 0.5;
            let distance = radius / (35.0_f32.to_radians().tan()) * 1.15;
            let eye = center + Vec3::new(0.25 * distance, 0.15 * distance, distance);
            (eye, center, 35.0_f32.to_radians())
        }
    };
    let distance = (eye - target).length();

    // Front light for the standard golden cameras (both look down -Z): the sun
    // travels toward the scene from the camera side, so camera-facing surfaces
    // are lit, not back-lit into silhouette. The atrium's oculus runs along
    // the nave, so the Sponza view wants a mostly overhead sun whose shafts
    // fall onto the floor.
    let directional = match view {
        LabView::SponzaAtrium => {
            DirectionalLightUniform::new([-0.3, -0.9, -0.3], [1.0, 0.98, 0.95], 100_000.0)
        }
        _ => DirectionalLightUniform::new([-0.45, -0.55, -0.7], [1.0, 0.98, 0.95], 100_000.0),
    };

    Ok(LabScene {
        width: 512,
        height: 512,
        eye: eye.into(),
        target: target.into(),
        fov_y,
        near: (distance * 0.001f32).clamp(0.05, 1.0),
        far: (distance * 8.0).max(50.0),
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.5,
        ambient_lux: 12_000.0,
        double_sided: true,
        post: None,
        directional,
        local_lights: Vec::new(),
        materials,
        meshes: lab_meshes,
    })
}

/// World-space position of a skinned vertex (CPU mirror of the GPU blend).
fn skinned_world_position(vertex: &LabVertex, joints: &[JointMatrix]) -> Vec3 {
    let fetch = |index: u32| -> JointMatrix {
        joints
            .get(index as usize)
            .copied()
            .unwrap_or(JointMatrix::identity())
    };
    let j = [
        fetch(vertex.joints[0]),
        fetch(vertex.joints[1]),
        fetch(vertex.joints[2]),
        fetch(vertex.joints[3]),
    ];
    Vec3::from_array(skin_position(vertex.position, &j, vertex.weights))
}

/// Compute a node's local transform from its TRS properties.
fn node_transform(node: &gltf::Node<'_>) -> Mat4 {
    let (t, r, s) = node.transform().decomposed();
    let translation = Mat4::from_translation(Vec3::from_array(t));
    let rotation = Mat4::from_quat(Quat::from_array(r));
    let scale = Mat4::from_scale(Vec3::from_array(s));
    translation * rotation * scale
}

/// Load a glTF texture into an RGBA8 [`LabTexture`], bytes as authored.
///
/// glTF colour textures (base colour, emissive) are sRGB-encoded and data
/// textures (normal, occlusion, metallic-roughness) are linear (`glTF 2.0`
/// § 3.9.2). The bytes are passed through unchanged: the renderer uploads the
/// colour slots as `Rgba8UnormSrgb` so the sampler decodes them (full precision
/// in the darks), and the data slots as `Rgba8Unorm`.
fn load_texture(
    texture: &gltf::Texture<'_>,
    images: &[gltf::image::Data],
    cache: &mut HashMap<usize, LabTexture>,
) -> Option<LabTexture> {
    let index = texture.source().index();
    if let Some(cached) = cache.get(&index) {
        return Some(cached.clone());
    }
    let image_data = images.get(index)?;
    let rgba = match image_data.format {
        gltf::image::Format::R8G8B8A8 => image_data.pixels.clone(),
        gltf::image::Format::R8G8B8 => {
            let mut out = Vec::with_capacity(image_data.pixels.len() / 3 * 4);
            for chunk in image_data.pixels.chunks(3) {
                out.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 255]);
            }
            out
        }
        _ => image::load_from_memory(&image_data.pixels)
            .ok()?
            .into_rgba8()
            .into_raw(),
    };
    let width = image_data.width;
    let height = image_data.height;
    if rgba.len() != width as usize * height as usize * 4 {
        return None;
    }
    let tex = LabTexture { width, height, rgba };
    cache.insert(index, tex.clone());
    Some(tex)
}

/// Recursively collect meshes from the scene graph.
fn collect_meshes(
    node: &gltf::Node<'_>,
    world: &[Mat4],
    buffers: &[gltf::buffer::Data],
    skins: &[ParsedSkin],
    packed_joints: &mut Vec<JointMatrix>,
    skin_base: &mut HashMap<usize, u32>,
    lab_meshes: &mut Vec<LabMesh>,
) {
    let world_matrix = world[node.index()];

    let name = node.name().unwrap_or("");
    if name.starts_with("socket_") || name.starts_with("collision_") {
        return;
    }

    if let Some(mesh) = node.mesh() {
        // Skinned meshes get a world-space joint palette; their model matrix is
        // identity so the palette is applied exactly once (lab shader contract).
        let (joint_base, joint_worlds): (Option<(u32, Vec<JointMatrix>)>, Vec<JointMatrix>) =
            match node.skin() {
                Some(skin) => {
                    let idx = skin.index();
                    let parsed = &skins[idx];
                    let palette: Vec<JointMatrix> = parsed
                        .joint_node_indices
                        .iter()
                        .enumerate()
                        .map(|(j, node_index)| {
                            let joint_world = world[*node_index];
                            let ibm = parsed
                                .inverse_bind_matrices
                                .get(j)
                                .copied()
                                .unwrap_or(Mat4::IDENTITY);
                            JointMatrix::from_mat4(joint_world * ibm)
                        })
                        .collect();
                    let base = *skin_base.entry(idx).or_insert_with(|| {
                        let start = packed_joints.len() as u32;
                        let count = palette.len().min(MAX_JOINTS_PER_SKIN);
                        packed_joints.extend_from_slice(&palette[..count]);
                        start
                    });
                    (Some((base, palette.clone())), palette)
                }
                None => (None, Vec::new()),
            };

        for prim in mesh.primitives() {
            if prim.mode() != gltf::mesh::Mode::Triangles {
                continue;
            }
            let reader = prim.reader(|buf| buffers.get(buf.index()).map(|d| &d.0[..]));

            let positions: Vec<[f32; 3]> = reader
                .read_positions()
                .map(|iter| iter.collect())
                .unwrap_or_default();
            if positions.is_empty() {
                continue;
            }

            let normals: Vec<[f32; 3]> = match reader.read_normals() {
                Some(iter) => iter.collect(),
                // glTF allows meshes without NORMAL (e.g. the Khronos Fox); a zero
                // normal would zero every N·L term and leave the mesh unlit, so
                // derive smooth normals from the triangle winding.
                None => {
                    let mut generated = vec![[0.0f32; 3]; positions.len()];
                    let indices_for_normals: Vec<u32> = reader
                        .read_indices()
                        .map(|iter| iter.into_u32().collect())
                        .unwrap_or_else(|| (0..positions.len() as u32).collect());
                    generate_vertex_normals(&positions, &indices_for_normals, &mut generated);
                    generated
                }
            };

            let uvs: Vec<[f32; 2]> = reader
                .read_tex_coords(0)
                .map(|iter| iter.into_f32().collect())
                .unwrap_or_else(|| vec![[0.0f32; 2]; positions.len()]);

            let indices: Vec<u32> = reader
                .read_indices()
                .map(|iter| iter.into_u32().collect())
                .unwrap_or_else(|| (0..positions.len() as u32).collect());

            let has_tangents = reader.read_tangents().is_some();

            let joints_raw: Vec<[u16; 4]> = reader
                .read_joints(0)
                .map(|iter| iter.into_u16().collect())
                .unwrap_or_else(|| vec![[0u16; 4]; positions.len()]);

            let weights_raw: Vec<[f32; 4]> = reader
                .read_weights(0)
                .map(|iter| iter.into_f32().collect())
                .unwrap_or_else(|| vec![[0.0f32; 4]; positions.len()]);

            let vc = positions.len();
            // Attribute accessors shorter than POSITION would index out of range.
            let tangents_len = reader.read_tangents().map(|t| t.len());
            let mismatch = normals.len() != vc
                || uvs.len() != vc
                || joints_raw.len() != vc
                || weights_raw.len() != vc
                || tangents_len.is_some_and(|n| n != vc);
            if mismatch {
                eprintln!("scene_loader: skipping primitive with mismatched attribute counts");
                continue;
            }

            let mut tangents: Vec<[f32; 4]> = if has_tangents {
                reader
                    .read_tangents()
                    .map(|iter| iter.collect())
                    .unwrap_or_else(|| vec![[1.0, 0.0, 0.0, 1.0]; vc])
            } else {
                vec![[1.0, 0.0, 0.0, 1.0]; vc]
            };

            if !has_tangents && uvs.len() == vc {
                generate_vertex_tangents(&positions, &normals, &uvs, &indices, &mut tangents);
            }

            let material_index = prim.material().index().unwrap_or(0);

            let mut vertices: Vec<LabVertex> = Vec::with_capacity(vc);
            for i in 0..vc {
                // Joint indices are LOCAL to this mesh's palette (`LabMesh::joints`);
                // the renderer re-bases them when it appends the palette to the
                // shared GPU buffer.
                let joints_u32: [u32; 4] = if let Some((base, _)) = joint_base {
                    let _ = base;
                    let count = joint_worlds.len() as u32;
                    let map = |j: u16| if (j as u32) < count { u32::from(j) } else { 0 };
                    [
                        map(joints_raw[i][0]),
                        map(joints_raw[i][1]),
                        map(joints_raw[i][2]),
                        map(joints_raw[i][3]),
                    ]
                } else {
                    [0u32; 4]
                };
                vertices.push(LabVertex {
                    position: positions[i],
                    normal: normals[i],
                    uv: uvs[i],
                    tangent: tangents[i],
                    color: [1.0; 4],
                    joints: joints_u32,
                    weights: weights_raw[i],
                });
            }

            // Skinned meshes carry the world transform in the palette; the model
            // matrix stays identity. Rigid meshes use the node's world matrix.
            let model = if joint_base.is_some() {
                Mat4::IDENTITY
            } else {
                world_matrix
            };
            let joints = joint_worlds.clone();
            lab_meshes.push(LabMesh {
                vertices,
                indices,
                material: material_index,
                model: model.to_cols_array_2d(),
                joints,
            });
        }
    }

    for child in node.children() {
        collect_meshes(
            &child,
            world,
            buffers,
            skins,
            packed_joints,
            skin_base,
            lab_meshes,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyframe_span_clamps_outside_the_clip() {
        let times = [0.0, 0.5, 1.0, 1.5];
        assert_eq!(keyframe_span(&times, -1.0), (0, 0, 0.0));
        assert_eq!(keyframe_span(&times, 2.0), (3, 3, 0.0));
        assert_eq!(keyframe_span(&times, 0.75), (1, 2, 0.5));
        assert_eq!(keyframe_span(&times, 0.5), (1, 2, 0.0));
        assert_eq!(keyframe_span(&[], 1.0), (0, 0, 0.0));
    }

    #[test]
    fn a_missing_clip_name_is_an_error_not_the_first_animation() {
        let json = br#"{"asset":{"version":"2.0"},"nodes":[{}],
            "animations":[{"name":"Idle","channels":[],"samplers":[]}]}"#;
        let gltf = gltf::Gltf::from_slice_without_validation(json).expect("parse");
        let mut local = vec![Mat4::IDENTITY];
        match sample_clip(&gltf.document, &[], "Walk", 0.0, &mut local) {
            Err(LoadError::NoClip { wanted, available }) => {
                assert_eq!(wanted, "Walk");
                assert_eq!(available, vec!["Idle".to_owned()]);
            }
            other => panic!("expected NoClip, got {other:?}"),
        }
        assert!(sample_clip(&gltf.document, &[], "Idle", 0.0, &mut local).is_ok());
        let still =
            gltf::Gltf::from_slice_without_validation(br#"{"asset":{"version":"2.0"}}"#).expect("parse");
        assert!(sample_clip(&still.document, &[], "Walk", 0.0, &mut local).is_ok());
    }

    #[test]
    fn ensure_model_missing_is_an_error_not_a_panic() {
        // No cache has this model on a clean checkout.
        if ensure_model("DefinitelyNotAModel").is_ok() {
            // If someone added such a file to a cache, the test would lie; accept it.
            let path = ensure_model("DefinitelyNotAModel").expect("checked above");
            assert!(path.exists());
        }
    }

    // ---- ADR-free regression tests for malformed glTF (Grok panic audit) ----

    /// Packs `json` and `bin` into a GLB container.
    fn glb(json: &str, bin: &[u8]) -> Vec<u8> {
        let mut j = json.as_bytes().to_vec();
        while !j.len().is_multiple_of(4) {
            j.push(b' ');
        }
        let mut b = bin.to_vec();
        while !b.len().is_multiple_of(4) {
            b.push(0);
        }
        let total = 12 + 8 + j.len() + 8 + b.len();
        let mut out = Vec::new();
        out.extend_from_slice(b"glTF");
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&(total as u32).to_le_bytes());
        out.extend_from_slice(&(j.len() as u32).to_le_bytes());
        out.extend_from_slice(b"JSON");
        out.extend_from_slice(&j);
        out.extend_from_slice(&(b.len() as u32).to_le_bytes());
        out.extend_from_slice(b"BIN\0");
        out.extend_from_slice(&b);
        out
    }

    fn floats(v: &[f32]) -> Vec<u8> {
        v.iter().flat_map(|f| f.to_le_bytes()).collect()
    }

    fn load_bytes(name: &str, bytes: &[u8]) -> Result<LabScene, LoadError> {
        let dir = crate::unique_temp_dir("scene_loader_malformed");
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("write test glb");
        let r = load_gltf_model_with_view(&path, LabView::Auto, None);
        let _ = std::fs::remove_dir_all(&dir);
        r
    }

    #[test]
    fn keyframe_span_never_indexes_out_of_range() {
        assert_eq!(keyframe_span(&[0.0], f32::NAN), (0, 0, 0.0));
        assert_eq!(keyframe_span(&[0.0], 0.5), (0, 0, 0.0));
        assert_eq!(keyframe_span(&[0.0, 1.0], f32::NAN), (0, 0, 0.0));
        assert_eq!(keyframe_span(&[f32::NAN, 1.0], 0.5), (0, 0, 0.0));
    }

    #[test]
    fn short_normal_accessor_is_skipped_not_a_panic() {
        // 3 positions, 1 normal.
        let mut bin = floats(&[0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]);
        bin.extend(floats(&[0.0, 0.0, 1.0]));
        let json = r#"{"asset":{"version":"2.0"},"scene":0,"scenes":[{"nodes":[0]}],
"nodes":[{"mesh":0}],"meshes":[{"primitives":[{"attributes":{"POSITION":0,"NORMAL":1}}]}],
"buffers":[{"byteLength":48}],
"bufferViews":[{"buffer":0,"byteOffset":0,"byteLength":36},{"buffer":0,"byteOffset":36,"byteLength":12}],
"accessors":[{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,0]},
{"bufferView":1,"componentType":5126,"count":1,"type":"VEC3"}]}"#;
        // Must return (Ok with the primitive skipped, or an error) without panicking.
        let _ = load_bytes("short_normal.glb", &glb(json, &bin));
    }

    #[test]
    fn sponza_partial_triangle_does_not_panic() {
        // 3 positions, indices count 4 (one full triangle + a tail of 1).
        let mut bin = floats(&[0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]);
        for i in [0u16, 1, 2, 0] {
            bin.extend_from_slice(&i.to_le_bytes());
        }
        let json = r#"{"asset":{"version":"2.0"},"scene":0,"scenes":[{"nodes":[0]}],
"nodes":[{"mesh":0}],"meshes":[{"primitives":[{"attributes":{"POSITION":0},"indices":1}]}],
"buffers":[{"byteLength":44}],
"bufferViews":[{"buffer":0,"byteOffset":0,"byteLength":36},{"buffer":0,"byteOffset":36,"byteLength":8}],
"accessors":[{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,0]},
{"bufferView":1,"componentType":5123,"count":4,"type":"SCALAR"}]}"#;
        let _ = load_bytes("Sponza.glb", &glb(json, &bin));
    }
}
