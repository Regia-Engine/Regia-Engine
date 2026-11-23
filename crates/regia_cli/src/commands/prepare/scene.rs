// SPDX-License-Identifier: MPL-2.0
//! The maps of `regia prepare --scene` (ADR-8453): a level source turned into the frame its
//! pipelines come from.
//!
//! The level is spawned headless (the scene plugin and the render types, no device), and each
//! distinct `MeshSource` material gets a stand-in quad in front of the camera. A pipeline depends on the vertex
//! layout (every static mesh is a `LabVertex` mesh), the material's kind (alpha mode, double
//! sided, transmission, unlit, its graph) and the frame's settings, never on the geometry or the
//! pixels of a texture, so the stand-in quad and 1×1 stand-in textures build the same pipelines
//! as the map's own meshes and images. `regia_render::extract_frame` then gives the frame, with
//! the level's lights, cameras, fog and environment.
//!
//! What this loader does not see (the game's own loader does, through
//! `regia_render::prepare::run_on`): content the game spawns from code, skinned or morphed
//! meshes (a glTF's skin is not read here), terrain and effects that a level does not carry as
//! components.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia::prelude::*;
use regia_core::id::Guid;
use regia_render::hard_surface::{AuthoredSurfaceMaps, ResolvedBaseMaps};
use regia_render::{
    CpuMaterial, CpuMesh, ExtractedFrame, LabMaterial, LabTexture, LabVertex, MeshRenderer, MeshSource,
};
use regia_scene::level::{Level, LevelId, LevelLibrary, LoadMode, open_level};
use regia_scene::{GlobalTransform, Transform};

/// The suffixes a level source carries.
const LEVEL_SUFFIXES: [&str; 2] = [".level.ron", ".level.d"];

/// The map's name in the unit list: the level's path under `project` (or its file name), `/`
/// separated, without the level suffix.
pub(super) fn map_name(project: &Path, path: &Path) -> String {
    let shown = path.strip_prefix(project).unwrap_or(path);
    let mut name = shown.to_string_lossy().replace('\\', "/");
    if path.is_absolute() && shown == path {
        name = path
            .file_name()
            .map_or_else(|| name.clone(), |file| file.to_string_lossy().to_string());
    }
    for suffix in LEVEL_SUFFIXES {
        if let Some(stem) = name.strip_suffix(suffix) {
            return stem.trim_start_matches("./").to_string();
        }
    }
    name.trim_start_matches("./").to_string()
}

/// A hash of the level's files (every file of a `.level.d`, in path order) and of the material
/// files they name (blake3, hex). A changed level or material gives another fingerprint.
pub(super) fn fingerprint(project: &Path, path: &Path) -> Result<String, String> {
    let mut files = level_files(path)?;
    if files.is_empty() {
        return Err(format!("{}: no level source here", path.display()));
    }
    files.sort();
    let mut hasher = blake3::Hasher::new();
    let mut materials = std::collections::BTreeSet::new();
    for file in &files {
        let bytes = std::fs::read(file).map_err(|err| format!("{}: {err}", file.display()))?;
        hash_entry(
            &mut hasher,
            &file.strip_prefix(path).unwrap_or(file).to_string_lossy(),
            &bytes,
        );
        materials.extend(material_refs(&String::from_utf8_lossy(&bytes)));
    }
    for material in materials {
        let file = resolve_asset(project, &material);
        // A missing material is the loader's error to report; the hash notes its absence.
        let bytes = std::fs::read(&file).unwrap_or_default();
        hash_entry(&mut hasher, &material, &bytes);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn hash_entry(hasher: &mut blake3::Hasher, name: &str, bytes: &[u8]) {
    hasher.update(&(name.len() as u64).to_le_bytes());
    hasher.update(name.as_bytes());
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

/// The level's own files: the file itself, or every file under a `.level.d` directory.
fn level_files(path: &Path) -> Result<Vec<PathBuf>, String> {
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    if !path.is_dir() {
        return Err(format!("{}: no such level", path.display()));
    }
    let mut out = Vec::new();
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|err| format!("{}: {err}", dir.display()))?;
            let child = entry.path();
            if child.is_dir() {
                stack.push(child);
            } else {
                out.push(child);
            }
        }
    }
    Ok(out)
}

/// Material paths a level's text names (`"...material.ron"`, `.matgraph.ron`, `.matinst.ron`).
fn material_refs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for quoted in text.split('"').skip(1).step_by(2) {
        let lower = quoted.to_ascii_lowercase();
        if [".material.ron", ".matgraph.ron", ".matinst.ron"]
            .iter()
            .any(|suffix| lower.ends_with(suffix))
        {
            out.push(quoted.to_string());
        }
    }
    out
}

/// An asset path from a level: absolute stays, else under the project (then `project/assets`).
fn resolve_asset(project: &Path, path: &str) -> PathBuf {
    let normalized = path.replace('\\', "/");
    let raw = Path::new(&normalized);
    if raw.is_absolute() {
        return raw.to_path_buf();
    }
    let direct = project.join(raw);
    if direct.exists() {
        return direct;
    }
    let under_assets = project.join("assets").join(raw);
    if under_assets.exists() {
        under_assets
    } else {
        direct
    }
}

/// Spawns the level headless and returns its extracted frame (one window view; a level without
/// a camera gets one looking at the origin).
pub(super) fn frame_of(project: &Path, path: &Path) -> Result<ExtractedFrame, String> {
    let _span = tracing::info_span!("prepare_scene_load", path = %path.display()).entered();
    let scene = regia_scene::level_dir::read_level_source(path)
        .map_err(|err| format!("{}: {err}", path.display()))?;
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    regia_render::register_render_types(&mut app);
    app.init_resource::<ExtractedFrame>();
    app.add_systems(Render, regia_render::extract_frame);
    let id = LevelId(Guid::from_u128(0x7072_6570_6172_655f_6d61_7073));
    app.world_mut()
        .resource_mut::<LevelLibrary>()
        .insert(Level::from_scene_file(id, scene));
    open_level(app.world_mut(), id, LoadMode::Replace).map_err(|err| format!("{}: {err}", path.display()))?;
    ensure_camera(app.world_mut());
    app.update();
    resolve_meshes(app.world_mut(), project)?;
    app.update();
    let frame = app.world().resource::<ExtractedFrame>().clone();
    if frame.window_views().is_empty() {
        return Err(format!("{}: the level draws no window view", path.display()));
    }
    Ok(frame)
}

/// One stand-in quad per distinct `MeshSource` material, unparented, 5 m in front of the first
/// camera, so every material is in view of the warm frame whatever the level's layout (a mesh the
/// frame culls would not build its material's pipelines). The level's own entities keep no
/// renderer.
fn resolve_meshes(world: &mut World, project: &Path) -> Result<(), String> {
    let quad = Arc::new(stand_in_quad());
    let mut query = world.query_filtered::<&MeshSource, Without<MeshRenderer>>();
    let names: std::collections::BTreeSet<String> =
        query.iter(world).map(|source| source.material.clone()).collect();
    let anchor = anchor_in_view(world);
    for (index, name) in names.into_iter().enumerate() {
        let material = Arc::new(if name.is_empty() {
            LabMaterial::default()
        } else {
            load_material(project, &name)?
        });
        // Side by side, 0.1 m apart: all in view, none hiding another's draw.
        let offset = regia_core::math::Vec3::new(index as f32 * 0.1, 0.0, 0.0);
        let transform = Transform::from_translation(anchor + offset);
        world.spawn((
            transform,
            GlobalTransform(transform.compute_affine()),
            MeshRenderer::new(Arc::clone(&quad), material),
        ));
    }
    Ok(())
}

/// 5 m in front of the first camera (the origin without one).
fn anchor_in_view(world: &mut World) -> regia_core::math::Vec3 {
    let mut cameras = world.query::<(&regia_render::Camera, &GlobalTransform)>();
    cameras.iter(world).min_by_key(|(camera, _)| camera.order).map_or(
        regia_core::math::Vec3::ZERO,
        |(_, global)| {
            let forward = regia_core::math::Vec3::from(global.0.matrix3 * regia_core::math::Vec3A::NEG_Z);
            regia_core::math::Vec3::from(global.0.translation) + forward.normalize_or_zero() * 5.0
        },
    )
}

/// A level without a camera still warms its materials: one camera at (0, 3, 6) looking down -Z.
fn ensure_camera(world: &mut World) {
    let mut cameras = world.query::<&regia_render::Camera>();
    if cameras.iter(world).next().is_some() {
        return;
    }
    let transform = Transform::from_translation(regia_core::math::Vec3::new(0.0, 3.0, 6.0));
    world.spawn((
        transform,
        GlobalTransform(transform.compute_affine()),
        regia_render::Camera::default(),
    ));
}

/// A 2 m quad on XZ facing +Y, in the vertex layout of every static mesh.
fn stand_in_quad() -> CpuMesh {
    let corners = [
        [-1.0, 0.0, -1.0],
        [1.0, 0.0, -1.0],
        [1.0, 0.0, 1.0],
        [-1.0, 0.0, 1.0],
    ];
    let uvs = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    CpuMesh {
        vertices: corners
            .into_iter()
            .zip(uvs)
            .map(|(position, uv)| LabVertex::from_pos_normal_uv(position, [0.0, 1.0, 0.0], uv))
            .collect(),
        indices: vec![0, 2, 1, 0, 3, 2],
    }
}

/// A 1×1 stand-in image: pipelines do not depend on texture contents.
fn stand_in_texture() -> LabTexture {
    LabTexture {
        width: 1,
        height: 1,
        rgba: vec![128, 128, 255, 255],
    }
}

/// A material file as the game draws it, with stand-in textures.
fn load_material(project: &Path, material: &str) -> Result<CpuMaterial, String> {
    let path = resolve_asset(project, material);
    let name = path.display().to_string();
    let lower = name.to_ascii_lowercase();
    let bytes = std::fs::read(&path).map_err(|err| format!("material {name}: {err}"))?;
    let stand_in = |present: bool| present.then(stand_in_texture);
    if lower.ends_with(".material.ron") {
        let asset = regia_asset::MaterialAssetRon::from_ron(&bytes)
            .map_err(|err| format!("material {name}: {err}"))?;
        let base = ResolvedBaseMaps {
            albedo: stand_in(asset.base.base_color_map.is_some()),
            normal: stand_in(asset.base.normal_map.is_some()),
            occlusion: stand_in(asset.base.orm_map.is_some()),
            emissive: stand_in(asset.base.emissive_map.is_some()),
        };
        let film = asset.thin_film.as_ref();
        let maps = AuthoredSurfaceMaps {
            thin_film_coverage: stand_in(film.is_some_and(|f| f.factor_map.is_some())),
            thin_film_thickness: stand_in(film.is_some_and(|f| f.thickness_map.is_some())),
            ..AuthoredSurfaceMaps::default()
        };
        return Ok(LabMaterial::from_authored_resolved(&name, &asset, base, maps));
    }
    let text = String::from_utf8(bytes).map_err(|err| format!("material {name}: {err}"))?;
    let (graph, instance) = if lower.ends_with(".matgraph.ron") {
        let graph = regia_matgraph::MaterialGraph::from_ron(&text)
            .map_err(|err| format!("material {name}: {err}"))?;
        (graph, None)
    } else if lower.ends_with(".matinst.ron") {
        let instance = regia_matgraph::MaterialInstance::from_ron(&text)
            .map_err(|err| format!("material {name}: {err}"))?;
        let sibling = path.parent().map(|dir| dir.join(&instance.parent));
        let parent = match sibling {
            Some(sibling) if sibling.exists() => sibling,
            _ => resolve_asset(project, &instance.parent),
        };
        let parent_text = std::fs::read_to_string(&parent)
            .map_err(|err| format!("material {}: {err}", parent.display()))?;
        let graph = regia_matgraph::MaterialGraph::from_ron(&parent_text)
            .map_err(|err| format!("material {}: {err}", parent.display()))?;
        (graph, Some(instance))
    } else {
        return Err(format!(
            "material {name}: not a material (.material.ron, .matgraph.ron, .matinst.ron)"
        ));
    };
    let no_switches = BTreeMap::new();
    let switches = instance.as_ref().map_or(&no_switches, |i| &i.switches);
    let compiled =
        regia_matgraph::compile_with(&graph, switches).map_err(|err| format!("material {name}: {err}"))?;
    let images: BTreeMap<String, LabTexture> = graph
        .textures
        .iter()
        .map(|decl| (decl.name.clone(), stand_in_texture()))
        .collect();
    match &instance {
        Some(instance) => regia_matgraph::instance_material(&graph, &compiled, instance, &images, 0.0),
        None => regia_matgraph::lab_material(
            &graph,
            &compiled,
            regia_matgraph::default_params(&graph, 0.0),
            &images,
        ),
    }
    .map_err(|err| format!("material {name}: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_names_drop_the_project_and_the_level_suffix() {
        let project = Path::new("/game");
        assert_eq!(
            map_name(project, Path::new("/game/levels/forest.level.ron")),
            "levels/forest"
        );
        assert_eq!(
            map_name(project, Path::new("/game/levels/canyon.level.d")),
            "levels/canyon"
        );
        assert_eq!(map_name(project, Path::new("/elsewhere/bore.level.ron")), "bore");
    }

    #[test]
    fn material_refs_are_the_quoted_material_paths() {
        let text = r#"(mesh: "assets/rock.gltf", material: "assets/rock.material.ron"), (material: "g.MATGRAPH.ron")"#;
        assert_eq!(
            material_refs(text),
            vec!["assets/rock.material.ron", "g.MATGRAPH.ron"]
        );
    }
}
