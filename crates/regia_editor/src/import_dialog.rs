// SPDX-License-Identifier: MPL-2.0
//! The import dialog (w12 UI parity addendum § 8.10, P-150/P-151, ADR-7904).
//!
//! Content ▸ Import… with several files, or with a glTF/GLB, FBX or OBJ, and the Content
//! Browser's Re-import with options… on an imported model open this dialog instead of importing
//! at once. It lists the files (each can be skipped), and for the file it shows:
//!
//! - a **scene preview**: the asset viewer's orbit render at dialog size (glTF/GLB directly, FBX
//!   and OBJ through their in-memory GLB conversion), turned by dragging or the arrow keys;
//! - a **scene tree** of meshes (primitives, triangles), materials (base color, texture count)
//!   and animations. Each animation becomes a clip with options: import or skip, its clip name,
//!   and frame ranges that split one take into several clips (frames at the importer's 30 fps
//!   clip rate; stored in seconds). The options are the `.meta` settings key `clips`
//!   ([`regia_asset_pipeline::ClipImportSettings`]), which the cook passes to the
//!   clip import (ADR-7904);
//! - the **importer settings form** (the Import settings panel's grid) and the project's
//!   **presets** for the importer ([`crate::import_presets`]).
//!
//! **Import** writes every file's `.meta` with its settings next to its destination before the
//! pipeline import copies the files, so the import's single cook already uses them (a re-import
//! writes the existing `.meta` through `editor.set_import_settings`' writer and cooks once).
//! **Apply to all** copies the shown file's settings (without its per-file clips) to every other
//! file of the same importer, then imports.
//!
//! Per-mesh and per-material options (glTF/GLB, ADR-8376): each mesh-bearing node has an
//! Import toggle, and each material a Use choice: the embedded material, **Extract** it to a
//! `.material.ron` (written into the project at Import, then the model draws with that asset),
//! or **Replace** it with a material asset of the project (picked from a list). Both are named by
//! the importer's sub-asset keys ([`regia_asset_pipeline::gltf_part_keys`]) and stored in the
//! `.meta` settings keys `meshes` and `materials`, so a re-import shows and keeps them. FBX and
//! OBJ list their meshes and materials read-only (their settings do not take these keys yet).
//! The generated read-only prefab (P-166) is deferred.
//!
//! Agent surface: `editor.import_dialog_open`, `editor.import_dialog_state` and
//! `editor.import_with` (`book/agents/import-settings.md`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_asset::import_rules::SettingsObject;
use regia_asset_pipeline::{
    ClipImportSettings, ClipRangeSettings, MaterialImportMode, MaterialImportSettings, MeshImportSettings,
};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::agent_ext::EditorProject;
use crate::asset_ops_agent::reasoned;
use crate::import_settings_panel::{ImportSettingsPanel, ImporterKind, validate_settings};
use crate::viewport_agent::Handler;

/// The clip rate the importer bakes clips at (`gltf_anim`: `clip.sample_rate = 30.0`): the
/// dialog's frame numbers are frames at this rate.
pub const CLIP_FPS: f32 = 30.0;

/// The settings key of the per-animation clip options.
pub const CLIPS_KEY: &str = "clips";

/// The settings key of the per-mesh options (ADR-8376).
pub const MESHES_KEY: &str = "meshes";

/// The settings key of the per-material options (ADR-8376).
pub const MATERIALS_KEY: &str = "materials";

/// The settings keys the scene tree owns (not the form).
const TREE_KEYS: [&str; 3] = [CLIPS_KEY, MESHES_KEY, MATERIALS_KEY];

/// A mesh of the scene tree: one mesh-bearing node, as the importer cooks it.
#[derive(Debug, Clone, PartialEq)]
pub struct MeshItem {
    /// The node's name as the importer keys it (its mesh sub-asset key, ADR-8376).
    pub name: String,
    /// Primitives (one per material part).
    pub primitives: usize,
    /// Triangles over all primitives.
    pub triangles: usize,
    /// The mesh sub-asset key (`mesh/<key>/primitive_<i>`) the `meshes` option names.
    pub key: String,
    /// Import the node's mesh (false: skip it).
    pub import: bool,
}

/// A material of the scene tree.
#[derive(Debug, Clone, PartialEq)]
pub struct MaterialItem {
    /// The material's name (`material_<index>` when unnamed).
    pub name: String,
    /// Base color factor (linear RGBA).
    pub base_color: [f32; 4],
    /// Textures the material references.
    pub textures: usize,
    /// The material sub-asset key (`material/<key>`) the `materials` option names.
    pub key: String,
    /// What the model's parts draw with: embedded, extracted or a replacement (ADR-8376).
    pub mode: MaterialImportMode,
}

/// One clip range in the dialog (seconds from the animation's start).
#[derive(Debug, Clone, PartialEq)]
pub struct RangeItem {
    /// The clip's name.
    pub name: String,
    /// Start, seconds.
    pub start: f32,
    /// End, seconds.
    pub end: f32,
}

/// An animation of the scene tree and its clip options.
#[derive(Debug, Clone, PartialEq)]
pub struct ClipItem {
    /// The animation's name in the file (`anim_<index>` when unnamed, as the importer names it).
    pub animation: String,
    /// The take's length in seconds (the largest sampler input time).
    pub duration: f32,
    /// Import a clip (false: skip the animation).
    pub import: bool,
    /// The clip's name (the animation's by default).
    pub name: String,
    /// Split ranges; empty imports one clip.
    pub ranges: Vec<RangeItem>,
}

impl ClipItem {
    /// Whether the options differ from the importer's default (one clip, the animation's name).
    #[must_use]
    pub fn is_default(&self) -> bool {
        self.import && self.name == self.animation && self.ranges.is_empty()
    }

    /// The `.meta` entry of these options.
    #[must_use]
    pub fn to_settings(&self) -> ClipImportSettings {
        ClipImportSettings {
            animation: self.animation.clone(),
            skip: !self.import,
            name: (self.name != self.animation && !self.name.trim().is_empty()).then(|| self.name.clone()),
            ranges: self
                .ranges
                .iter()
                .map(|r| ClipRangeSettings {
                    name: r.name.clone(),
                    start: r.start,
                    end: r.end,
                })
                .collect(),
        }
    }
}

/// What a model file holds, for the scene tree.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SceneItems {
    /// Meshes, in file order.
    pub meshes: Vec<MeshItem>,
    /// Materials, in file order.
    pub materials: Vec<MaterialItem>,
    /// Animations, in file order.
    pub clips: Vec<ClipItem>,
}

/// One file of the dialog.
#[derive(Debug, Default)]
pub struct DialogFile {
    /// The file to import (outside the project), or the project source a re-import rewrites.
    pub source: PathBuf,
    /// The project-relative source path of a re-import (`None` for a new import).
    pub reimport: Option<String>,
    /// The file's importer (`None`: no settings form; it is imported as it is).
    pub kind: Option<ImporterKind>,
    /// The settings form (a detached Import settings panel over this file).
    pub form: ImportSettingsPanel,
    /// The scene tree (models only).
    pub scene: SceneItems,
    /// Left out of the import.
    pub skipped: bool,
    /// Why the scene could not be read (the file still imports).
    pub problem: Option<String>,
    /// The converted GLB of an FBX/OBJ, written for the preview (under the project state dir).
    pub preview_glb: Option<PathBuf>,
}

impl DialogFile {
    /// The file name.
    #[must_use]
    pub fn name(&self) -> String {
        self.source
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// The settings this file is imported with: the form plus the clip options (`None`: no
    /// override, the importer's defaults).
    #[must_use]
    pub fn settings_text(&self) -> Option<String> {
        let mut object = self.form.form.clone();
        object
            .fields
            .retain(|(key, _)| !TREE_KEYS.contains(&key.as_str()));
        let clips: Vec<ClipImportSettings> = self
            .scene
            .clips
            .iter()
            .filter(|c| !c.is_default())
            .map(ClipItem::to_settings)
            .collect();
        if !clips.is_empty() {
            object.fields.push((CLIPS_KEY.into(), clips_ron(&clips)));
        }
        let meshes: Vec<MeshImportSettings> = self
            .scene
            .meshes
            .iter()
            .filter(|m| !m.import)
            .map(|m| MeshImportSettings {
                mesh: m.key.clone(),
                skip: true,
            })
            .collect();
        if !meshes.is_empty() {
            object.fields.push((MESHES_KEY.into(), meshes_ron(&meshes)));
        }
        let materials: Vec<MaterialImportSettings> = self
            .scene
            .materials
            .iter()
            .filter(|m| m.mode != MaterialImportMode::Embedded)
            .map(|m| MaterialImportSettings {
                material: m.key.clone(),
                mode: m.mode.clone(),
            })
            .collect();
        if !materials.is_empty() {
            object
                .fields
                .push((MATERIALS_KEY.into(), materials_ron(&materials)));
        }
        (!object.fields.is_empty()).then(|| object.to_ron())
    }

    /// Whether the file takes per-mesh and per-material options (glTF/GLB; ADR-8376).
    #[must_use]
    pub fn takes_part_options(&self) -> bool {
        self.kind == Some(ImporterKind::Gltf)
    }

    /// Whether the file is a model (glTF/GLB, FBX, OBJ): it has a scene tree and a preview.
    #[must_use]
    pub fn is_model(&self) -> bool {
        matches!(
            self.kind,
            Some(ImporterKind::Gltf | ImporterKind::Fbx | ImporterKind::Obj)
        )
    }
}

/// The compact RON text of clip options (only the fields that differ from the defaults).
fn clips_ron(clips: &[ClipImportSettings]) -> String {
    let quote = |s: &str| ron::ser::to_string(&s).unwrap_or_else(|_| format!("{s:?}"));
    let entries: Vec<String> = clips
        .iter()
        .map(|clip| {
            let mut fields = vec![format!("animation: {}", quote(&clip.animation))];
            if clip.skip {
                fields.push("skip: true".into());
            }
            if let Some(name) = &clip.name {
                fields.push(format!("name: Some({})", quote(name)));
            }
            if !clip.ranges.is_empty() {
                let ranges: Vec<String> = clip
                    .ranges
                    .iter()
                    .map(|r| {
                        format!(
                            "(name: {}, start: {:?}, end: {:?})",
                            quote(&r.name),
                            r.start,
                            r.end
                        )
                    })
                    .collect();
                fields.push(format!("ranges: [{}]", ranges.join(", ")));
            }
            format!("({})", fields.join(", "))
        })
        .collect();
    format!("[{}]", entries.join(", "))
}

/// A RON string literal.
fn ron_quote(s: &str) -> String {
    ron::ser::to_string(&s).unwrap_or_else(|_| format!("{s:?}"))
}

/// The compact RON text of mesh options (`[(mesh: "body", skip: true)]`).
fn meshes_ron(meshes: &[MeshImportSettings]) -> String {
    let entries: Vec<String> = meshes
        .iter()
        .map(|m| format!("(mesh: {}, skip: {})", ron_quote(&m.mesh), m.skip))
        .collect();
    format!("[{}]", entries.join(", "))
}

/// The compact RON text of material options (`[(material: "paint", mode: Replaced(asset: "<id>",
/// path: "assets/red.material.ron"))]`).
fn materials_ron(materials: &[MaterialImportSettings]) -> String {
    let entries: Vec<String> = materials
        .iter()
        .map(|m| {
            let mode = match &m.mode {
                MaterialImportMode::Embedded => "Embedded".to_string(),
                MaterialImportMode::Extracted { asset, path } => format!(
                    "Extracted(asset: {}, path: {})",
                    ron_quote(&asset.to_string()),
                    ron_quote(path)
                ),
                MaterialImportMode::Replaced { asset, path } => format!(
                    "Replaced(asset: {}, path: {})",
                    ron_quote(&asset.to_string()),
                    ron_quote(path)
                ),
            };
            format!("(material: {}, mode: {mode})", ron_quote(&m.material))
        })
        .collect();
    format!("[{}]", entries.join(", "))
}

/// Reads the scene tree of a glTF/GLB, or of the GLB an FBX/OBJ converts to.
///
/// # Errors
///
/// The file does not read, convert or parse as glTF.
pub fn read_scene(glb_or_gltf: &[u8]) -> Result<SceneItems, String> {
    let gltf = gltf::Gltf::from_slice_without_validation(glb_or_gltf).map_err(|e| e.to_string())?;
    let document = gltf.document;
    // The importer's own keys (ADR-8376): one row per mesh-bearing node it cooks.
    let keys = regia_asset_pipeline::gltf_part_keys(glb_or_gltf)?;
    let meshes = keys
        .meshes
        .iter()
        .filter_map(|(key, _, mesh_index)| document.meshes().nth(*mesh_index).map(|mesh| (key, mesh)))
        .map(|(key, mesh)| {
            let triangles = mesh
                .primitives()
                .filter(|p| p.mode() == gltf::mesh::Mode::Triangles)
                .map(|p| {
                    p.indices()
                        .map(|a| a.count())
                        .or_else(|| p.get(&gltf::Semantic::Positions).map(|a| a.count()))
                        .unwrap_or(0)
                        / 3
                })
                .sum();
            MeshItem {
                name: key.clone(),
                primitives: mesh.primitives().count(),
                triangles,
                key: key.clone(),
                import: true,
            }
        })
        .collect();
    let materials = document
        .materials()
        .enumerate()
        .map(|(index, material)| {
            let pbr = material.pbr_metallic_roughness();
            let textures = [
                pbr.base_color_texture().is_some(),
                pbr.metallic_roughness_texture().is_some(),
                material.normal_texture().is_some(),
                material.occlusion_texture().is_some(),
                material.emissive_texture().is_some(),
            ]
            .into_iter()
            .filter(|t| *t)
            .count();
            MaterialItem {
                name: material
                    .name()
                    .filter(|n| !n.is_empty())
                    .map_or_else(|| format!("material_{index}"), str::to_string),
                base_color: pbr.base_color_factor(),
                textures,
                key: keys
                    .materials
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| index.to_string()),
                mode: MaterialImportMode::Embedded,
            }
        })
        .collect();
    let clips = document
        .animations()
        .map(|animation| {
            // The importer's own naming (`gltf_anim`: `anim_<index>` for an unnamed one).
            let name = animation
                .name()
                .filter(|n| !n.is_empty())
                .map_or_else(|| format!("anim_{}", animation.index()), str::to_string);
            let duration = animation
                .samplers()
                .filter_map(|s| s.input().max().and_then(|max| max.get(0).and_then(Value::as_f64)))
                .fold(0.0f64, f64::max) as f32;
            ClipItem {
                animation: name.clone(),
                duration,
                import: true,
                name,
                ranges: Vec::new(),
            }
        })
        .collect();
    Ok(SceneItems {
        meshes,
        materials,
        clips,
    })
}

/// The GLB bytes of a model the dialog reads: the file itself for glTF/GLB, its conversion for
/// FBX and OBJ (with the form's settings).
fn model_bytes(path: &Path, kind: ImporterKind, settings: Option<&str>) -> Result<Vec<u8>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    match kind {
        ImporterKind::Fbx => {
            let settings =
                regia_asset_pipeline::fbx_import::FbxImportSettings::try_from_meta_string(settings)
                    .map_err(|e| e.to_string())?;
            regia_asset_pipeline::fbx_import::convert_fbx_to_glb(&bytes, path.parent(), &settings)
                .map(|c| c.glb)
                .map_err(|e| e.to_string())
        }
        ImporterKind::Obj => {
            let settings =
                regia_asset_pipeline::obj_import::ObjImportSettings::try_from_meta_string(settings)
                    .map_err(|e| e.to_string())?;
            regia_asset_pipeline::obj_import::convert_obj_to_glb(&bytes, path.parent(), &settings)
                .map(|c| c.glb)
                .map_err(|e| e.to_string())
        }
        _ => Ok(bytes),
    }
}

/// Merges a `.meta`'s saved `clips` options into a freshly read scene tree (a re-import shows
/// what the file was imported with). Entries for animations the file no longer has are dropped.
fn merge_saved_clips(scene: &mut SceneItems, form: &SettingsObject) {
    let Some(text) = form.get(CLIPS_KEY) else {
        return;
    };
    let Ok(saved) = ron::from_str::<Vec<ClipImportSettings>>(text) else {
        return;
    };
    for clip in &mut scene.clips {
        if let Some(entry) = saved.iter().find(|s| s.animation == clip.animation) {
            clip.import = !entry.skip;
            if let Some(name) = entry.name.clone().filter(|n| !n.trim().is_empty()) {
                clip.name = name;
            }
            clip.ranges = entry
                .ranges
                .iter()
                .map(|r| RangeItem {
                    name: r.name.clone(),
                    start: r.start,
                    end: r.end,
                })
                .collect();
        }
    }
}

/// Merges a `.meta`'s saved `meshes` and `materials` options into a freshly read scene tree (a
/// re-import shows and keeps what the file was imported with, ADR-8376). Entries for keys the file
/// no longer has are dropped.
fn merge_saved_parts(scene: &mut SceneItems, form: &SettingsObject) {
    if let Some(saved) = form
        .get(MESHES_KEY)
        .and_then(|text| ron::from_str::<Vec<MeshImportSettings>>(text).ok())
    {
        for mesh in &mut scene.meshes {
            if let Some(entry) = saved.iter().find(|s| s.mesh == mesh.key) {
                mesh.import = !entry.skip;
            }
        }
    }
    if let Some(saved) = form
        .get(MATERIALS_KEY)
        .and_then(|text| ron::from_str::<Vec<MaterialImportSettings>>(text).ok())
    {
        for material in &mut scene.materials {
            if let Some(entry) = saved.iter().find(|s| s.material == material.key) {
                material.mode = entry.mode.clone();
            }
        }
    }
}

/// The project's material assets a part can be replaced with: every `.material.ron` under
/// `assets/` with a `.meta`, as `(project-relative path, id)`, sorted by path.
#[must_use]
pub fn project_materials(root: &Path) -> Vec<(String, regia_asset::AssetId)> {
    /// Most directory entries read (a huge project lists the first ones).
    const MAX_ENTRIES: usize = 200_000;
    let mut out = Vec::new();
    let mut stack = vec![root.join("assets")];
    let mut seen = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > MAX_ENTRIES {
                break;
            }
            let path = entry.path();
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(path);
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if !name.ends_with(".material.ron") {
                continue;
            }
            let Some(id) = std::fs::read(regia_asset::meta_path_for(&path))
                .ok()
                .and_then(|bytes| regia_asset::MetaSidecar::from_ron(&bytes).ok())
                .map(|meta| meta.id)
            else {
                continue;
            };
            if let Ok(rel) = path.strip_prefix(root) {
                out.push((rel.to_string_lossy().replace('\\', "/"), id));
            }
        }
    }
    out.sort();
    out
}

/// The default `.material.ron` an extracted material is written to: next to the model,
/// `<model stem>_<material key>.material.ron` (key characters other than letters, digits, `-` and
/// `_` become `_`).
#[must_use]
pub fn default_extract_path(model_dir: &str, model_stem: &str, key: &str) -> String {
    let safe: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let dir = model_dir.trim_end_matches('/');
    format!("{dir}/{model_stem}_{safe}.material.ron")
}

/// Why `path` cannot be an extracted material's file: it must be under `assets/`, end in
/// `.material.ron` and have no `..` component. `None` when it can.
fn extract_path_problem(path: &str) -> Option<String> {
    let lower = path.to_ascii_lowercase();
    if !path.starts_with("assets/") || !lower.ends_with(".material.ron") {
        return Some(format!(
            "{path:?}: an extracted material is a .material.ron under assets/"
        ));
    }
    path.split(['/', '\\'])
        .any(|c| c == "..")
        .then(|| format!("{path:?}: `..` is not allowed"))
}

/// The dialog (an edit-world resource).
#[derive(Resource, Default)]
pub struct ImportDialog {
    /// The window shows.
    pub open: bool,
    /// The project.
    pub root: Option<PathBuf>,
    /// The destination folder under `assets/` (new imports).
    pub folder: String,
    /// The files, in the order picked.
    pub files: Vec<DialogFile>,
    /// The file the dialog shows.
    pub current: usize,
    /// The last refusal: `(code, message)`.
    pub error: Option<(String, String)>,
    /// The last import's result.
    pub last_result: Option<Value>,
    /// Preview orbit yaw/pitch, radians.
    pub orbit: (f32, f32),
    /// The preview's rendered texture and what it shows: `(file, yaw, pitch)`.
    preview: Option<((usize, i32, i32), egui::TextureHandle)>,
    /// The loaded preview model: `(file index, model)`.
    preview_model: Option<(usize, Arc<crate::mesh_source::LoadedModel>)>,
    /// The preset picked in the dialog.
    pub preset_choice: Option<String>,
    /// A button asked to import: `Some(apply_to_all)`; run after drawing.
    requested: Option<bool>,
    /// The `.meta` files the window's last import wrote ahead of its background job, with that
    /// job: once it ends (done, failed or cancelled) every one whose destination never arrived is
    /// removed (review g18 row 6).
    prewritten: Option<(crate::editor_jobs::ImportJob, Vec<(PathBuf, PathBuf)>)>,
    /// The project's material assets the Replace picker lists ([`project_materials`]), read
    /// when the dialog opens.
    pub material_assets: Vec<(String, regia_asset::AssetId)>,
}

impl std::fmt::Debug for ImportDialog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImportDialog")
            .field("open", &self.open)
            .field("folder", &self.folder)
            .field("files", &self.files.len())
            .field("current", &self.current)
            .finish_non_exhaustive()
    }
}

/// Whether picked files open the dialog instead of importing at once: several files, or any
/// model (glTF/GLB, FBX, OBJ). The Content Browser's Import… and the OS drop path (3222) ask
/// this before they import.
#[must_use]
pub fn wants_dialog(sources: &[PathBuf]) -> bool {
    sources.len() > 1
        || sources.iter().any(|s| {
            ImporterKind::of_path(&s.to_string_lossy())
                .is_some_and(|k| matches!(k, ImporterKind::Gltf | ImporterKind::Fbx | ImporterKind::Obj))
        })
}

/// Builds the dialog's file entry for `source`.
fn dialog_file(root: &Path, source: &Path, reimport: Option<String>, saved: SettingsObject) -> DialogFile {
    let kind = ImporterKind::of_path(&source.to_string_lossy());
    let mut file = DialogFile {
        source: source.to_path_buf(),
        kind,
        ..DialogFile::default()
    };
    file.form = ImportSettingsPanel {
        open: true,
        root: Some(root.to_path_buf()),
        path: Some(reimport.clone().unwrap_or_else(|| file.name())),
        kind,
        form: saved.clone(),
        saved: if reimport.is_some() {
            saved
        } else {
            SettingsObject::default()
        },
        ..ImportSettingsPanel::default()
    };
    file.reimport = reimport;
    if let Some(kind) = kind.filter(|_| file.is_model()) {
        let settings = (!file.form.form.fields.is_empty()).then(|| file.form.form.to_ron());
        match model_bytes(source, kind, settings.as_deref()).and_then(|bytes| {
            let scene = read_scene(&bytes)?;
            Ok((bytes, scene))
        }) {
            Ok((bytes, mut scene)) => {
                merge_saved_clips(&mut scene, &file.form.form);
                merge_saved_parts(&mut scene, &file.form.form);
                // The tree owns the clip, mesh and material options now (`settings_text` writes
                // them back).
                file.form
                    .form
                    .fields
                    .retain(|(key, _)| !TREE_KEYS.contains(&key.as_str()));
                file.scene = scene;
                if kind != ImporterKind::Gltf {
                    file.preview_glb = write_preview_glb(root, source, &bytes);
                }
            }
            Err(problem) => file.problem = Some(problem),
        }
    }
    file
}

/// Writes an FBX/OBJ's converted GLB under the project state dir for the preview.
fn write_preview_glb(root: &Path, source: &Path, bytes: &[u8]) -> Option<PathBuf> {
    let dir = regia_core::brand::project_state_dir(root).join("import-preview");
    let stem = source.file_stem()?.to_string_lossy().into_owned();
    let hash = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        source.hash(&mut hasher);
        hasher.finish()
    };
    let path = dir.join(format!("{stem}-{hash:016x}.glb"));
    crate::atomic_write::write_atomic(&path, bytes)
        .map_err(|e| tracing::warn!(target: "regia_editor", "import preview: {e}"))
        .ok()?;
    Some(path)
}

impl ImportDialog {
    /// Opens the dialog over files to import into `folder` (under `assets/`).
    ///
    /// # Errors
    ///
    /// `invalid_params` for no file, or a file that is not readable.
    pub fn open_files(&mut self, root: &Path, folder: &str, sources: &[PathBuf]) -> Result<(), RpcError> {
        if sources.is_empty() {
            return Err(RpcError::invalid_params("at least one file is required"));
        }
        let mut files = Vec::new();
        for source in sources {
            if !source.is_file() {
                return Err(RpcError::invalid_params(format!(
                    "{} is not a readable file",
                    source.display()
                )));
            }
            files.push(dialog_file(root, source, None, SettingsObject::default()));
        }
        *self = Self {
            open: true,
            root: Some(root.to_path_buf()),
            folder: folder.trim_start_matches("assets/").trim_matches('/').to_string(),
            files,
            orbit: (0.6, 0.35),
            material_assets: project_materials(root),
            ..Self::default()
        };
        Ok(())
    }

    /// Opens the dialog over an imported model (Re-import with options…): its `.meta` settings
    /// and clip options fill the form and the tree.
    ///
    /// # Errors
    ///
    /// As `editor.import_settings_open` (`path_outside_project`, `asset_not_imported`, ...).
    pub fn open_reimport(&mut self, root: &Path, path: &str) -> Result<(), RpcError> {
        let mut reader = ImportSettingsPanel::default();
        reader.open_path(root, path)?;
        let rel = reader.path.clone().unwrap_or_default();
        let file = dialog_file(root, &root.join(&rel), Some(rel), reader.saved.clone());
        *self = Self {
            open: true,
            root: Some(root.to_path_buf()),
            files: vec![file],
            orbit: (0.6, 0.35),
            material_assets: project_materials(root),
            ..Self::default()
        };
        Ok(())
    }

    /// Whether the dialog re-imports a project source (rather than importing new files).
    #[must_use]
    pub fn is_reimport(&self) -> bool {
        self.files.iter().any(|f| f.reimport.is_some())
    }

    /// Copies the shown file's settings (without its per-file clips) to every other file of the
    /// same importer (Apply to all). Answers how many files took them.
    pub fn apply_to_all(&mut self) -> usize {
        let Some(current) = self.files.get(self.current) else {
            return 0;
        };
        let (kind, shared) = (current.kind, crate::import_presets::shareable(&current.form.form));
        let mut count = 0;
        for (index, file) in self.files.iter_mut().enumerate() {
            if index == self.current || file.kind != kind || kind.is_none() {
                continue;
            }
            file.form.form = crate::import_presets::applied(&file.form.form, &preset_of(&shared))
                .unwrap_or_else(|_| shared.clone());
            file.form.buffers.clear();
            file.form.invalid.clear();
            count += 1;
        }
        count
    }

    /// Applies the project preset `name` to the shown file (not to its clips).
    ///
    /// # Errors
    ///
    /// As [`crate::import_presets::apply_to_panel`].
    pub fn apply_preset(&mut self, name: &str) -> Result<(), RpcError> {
        let file = self
            .files
            .get_mut(self.current)
            .ok_or_else(|| reasoned("import_dialog_not_open", "the import dialog shows no file"))?;
        crate::import_presets::apply_to_panel(&mut file.form, name)?;
        self.preset_choice = Some(name.to_string());
        Ok(())
    }

    /// Applies agent `items` to the file at `index` (see `editor.import_with`).
    ///
    /// # Errors
    ///
    /// `import_item_unknown` (no such animation, mesh or material), `import_item_unsupported` (an
    /// unknown item kind, or mesh/material options on an FBX/OBJ), `import_material_not_found` (a
    /// replacement that is not a material asset of the project), `invalid_params` (a bad extract
    /// path or mode) or `asset_settings_invalid` (a bad range).
    pub fn apply_items(&mut self, index: usize, items: &[Value]) -> Result<(), RpcError> {
        let folder = self.folder.clone();
        let assets = self.material_assets.clone();
        let file = self
            .files
            .get_mut(index)
            .ok_or_else(|| reasoned("import_dialog_not_open", "the import dialog shows no file"))?;
        for item in items {
            let kind = item.get("kind").and_then(Value::as_str).unwrap_or("clip");
            match kind {
                "clip" => {}
                "mesh" | "material" => {
                    apply_part_item(file, &folder, &assets, kind, item)?;
                    continue;
                }
                _ => {
                    return Err(reasoned(
                        "import_item_unsupported",
                        format!("{kind} items take no options: the kinds are clip, mesh and material"),
                    ));
                }
            }
            let animation = item.get("animation").and_then(Value::as_str).ok_or_else(|| {
                RpcError::invalid_params("a clip item needs `animation` (its name in the file)")
            })?;
            let clip = file
                .scene
                .clips
                .iter_mut()
                .find(|c| c.animation == animation)
                .ok_or_else(|| {
                    reasoned(
                        "import_item_unknown",
                        format!("{} has no animation named {animation:?}", file.source.display()),
                    )
                })?;
            if let Some(import) = item.get("import").and_then(Value::as_bool) {
                clip.import = import;
            }
            if let Some(name) = item.get("name").and_then(Value::as_str) {
                clip.name = if name.trim().is_empty() {
                    clip.animation.clone()
                } else {
                    name.to_string()
                };
            }
            if let Some(ranges) = item.get("ranges") {
                let list = ranges.as_array().ok_or_else(|| {
                    RpcError::invalid_params("`ranges` is an array of { name, start, end }")
                })?;
                let mut parsed = Vec::new();
                for (i, range) in list.iter().enumerate() {
                    let seconds = |key: &str, frame_key: &str| {
                        range
                            .get(key)
                            .and_then(Value::as_f64)
                            .map(|v| v as f32)
                            .or_else(|| {
                                range
                                    .get(frame_key)
                                    .and_then(Value::as_f64)
                                    .map(|f| f as f32 / CLIP_FPS)
                            })
                    };
                    let (Some(start), Some(end)) =
                        (seconds("start", "start_frame"), seconds("end", "end_frame"))
                    else {
                        return Err(RpcError::invalid_params(format!(
                            "range {i}: `start` and `end` (seconds) or `start_frame` and `end_frame` are required"
                        )));
                    };
                    let name = range
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let check = ClipRangeSettings {
                        name: name.clone(),
                        start,
                        end,
                    };
                    if let Some(problem) = check.problem() {
                        return Err(reasoned("asset_settings_invalid", problem));
                    }
                    parsed.push(RangeItem { name, start, end });
                }
                clip.ranges = parsed;
            }
        }
        Ok(())
    }

    /// The dialog as the agent sees it (`editor.import_dialog_state`).
    #[must_use]
    pub fn state_json(&self) -> Value {
        let files: Vec<Value> = self
            .files
            .iter()
            .map(|file| {
                let scene = &file.scene;
                json!({
                    "source": file.source.display().to_string(),
                    "name": file.name(),
                    "reimport": file.reimport,
                    "importer": file.kind.map(ImporterKind::name),
                    "skipped": file.skipped,
                    "settings": file.settings_text(),
                    "invalid": file.form.invalid.iter().collect::<Vec<_>>(),
                    "problem": file.problem,
                    "preview": file.is_model() && file.problem.is_none(),
                    "scene": {
                        "meshes": scene.meshes.iter().map(|m| json!({
                            "name": m.name, "primitives": m.primitives, "triangles": m.triangles,
                            "key": m.key, "import": m.import,
                        })).collect::<Vec<_>>(),
                        "materials": scene.materials.iter().map(|m| json!({
                            "name": m.name, "base_color": m.base_color, "textures": m.textures,
                            "key": m.key, "mode": mode_name(&m.mode),
                            "path": match &m.mode {
                                MaterialImportMode::Embedded => None,
                                MaterialImportMode::Extracted { path, .. }
                                | MaterialImportMode::Replaced { path, .. } => Some(path.clone()),
                            },
                            "asset": m.mode.asset().map(|id| id.to_string()),
                        })).collect::<Vec<_>>(),
                        "clips": scene.clips.iter().map(|c| json!({
                            "animation": c.animation,
                            "duration": c.duration,
                            "frames": (c.duration * CLIP_FPS).round() as u64,
                            "import": c.import,
                            "name": c.name,
                            "ranges": c.ranges.iter().map(|r| json!({
                                "name": r.name, "start": r.start, "end": r.end,
                                "start_frame": (r.start * CLIP_FPS).round() as i64,
                                "end_frame": (r.end * CLIP_FPS).round() as i64,
                            })).collect::<Vec<_>>(),
                        })).collect::<Vec<_>>(),
                    },
                    "part_options": file.takes_part_options(),
                })
            })
            .collect();
        json!({
            "open": self.open,
            "mode": if self.is_reimport() { "reimport" } else { "import" },
            "folder": format!("assets/{}", self.folder).trim_end_matches('/').to_string(),
            "current": self.current,
            "files": files,
            "error": self.error.as_ref().map(|(code, message)| json!({ "code": code, "message": message })),
            "last_result": self.last_result,
            "material_assets": self.material_assets.iter().map(|(path, id)| json!({
                "path": path, "asset": id.to_string(),
            })).collect::<Vec<_>>(),
        })
    }
}

/// The project folder (`assets/...`) a file's model lands in: the re-imported source's folder, or
/// the dialog's destination folder.
fn model_dir(folder: &str, file: &DialogFile) -> String {
    match &file.reimport {
        Some(rel) => rel
            .rsplit_once('/')
            .map_or_else(|| "assets".to_string(), |(dir, _)| dir.to_string()),
        None => format!("assets/{folder}").trim_end_matches('/').to_string(),
    }
}

/// Applies one `mesh` or `material` item (ADR-8376) to `file`.
fn apply_part_item(
    file: &mut DialogFile,
    folder: &str,
    assets: &[(String, regia_asset::AssetId)],
    kind: &str,
    item: &Value,
) -> Result<(), RpcError> {
    if !file.takes_part_options() {
        return Err(reasoned(
            "import_item_unsupported",
            format!(
                "{}: mesh and material options are for glTF/GLB files",
                file.name()
            ),
        ));
    }
    if kind == "mesh" {
        let key = item.get("mesh").and_then(Value::as_str).ok_or_else(|| {
            RpcError::invalid_params("a mesh item needs `mesh` (its key in the scene tree)")
        })?;
        let name = file.name();
        let mesh = file
            .scene
            .meshes
            .iter_mut()
            .find(|m| m.key == key)
            .ok_or_else(|| reasoned("import_item_unknown", format!("{name} has no mesh {key:?}")))?;
        if let Some(import) = item.get("import").and_then(Value::as_bool) {
            mesh.import = import;
        }
        return Ok(());
    }
    let key = item.get("material").and_then(Value::as_str).ok_or_else(|| {
        RpcError::invalid_params("a material item needs `material` (its key in the scene tree)")
    })?;
    let dir = model_dir(folder, file);
    let stem = file
        .source
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = file.name();
    let material = file
        .scene
        .materials
        .iter_mut()
        .find(|m| m.key == key)
        .ok_or_else(|| reasoned("import_item_unknown", format!("{name} has no material {key:?}")))?;
    let path = item.get("path").and_then(Value::as_str);
    material.mode = match item.get("mode").and_then(Value::as_str).unwrap_or("embedded") {
        "embedded" => MaterialImportMode::Embedded,
        "extract" => {
            let path = path.map_or_else(|| default_extract_path(&dir, &stem, key), str::to_string);
            if let Some(problem) = extract_path_problem(&path) {
                return Err(RpcError::invalid_params(problem));
            }
            // Keep the id of an extraction already chosen for the same file.
            let asset = match &material.mode {
                MaterialImportMode::Extracted { asset, path: old } if *old == path => *asset,
                _ => regia_asset::AssetId::new_random(),
            };
            MaterialImportMode::Extracted { asset, path }
        }
        "replace" => {
            let path = path.ok_or_else(|| {
                RpcError::invalid_params("a replace item needs `path` (a material asset of the project)")
            })?;
            let (path, asset) = assets
                .iter()
                .find(|(p, _)| p == path)
                .cloned()
                .ok_or_else(|| {
                    reasoned(
                        "import_material_not_found",
                        format!(
                            "{path:?} is not a material asset of the project (a .material.ron under assets/ with a .meta)"
                        ),
                    )
                })?;
            MaterialImportMode::Replaced { asset, path }
        }
        other => {
            return Err(RpcError::invalid_params(format!(
                "material mode {other:?}: use embedded, extract or replace"
            )));
        }
    };
    Ok(())
}

/// The mode name the agent and the dialog use (`embedded`, `extract`, `replace`).
fn mode_name(mode: &MaterialImportMode) -> &'static str {
    match mode {
        MaterialImportMode::Embedded => "embedded",
        MaterialImportMode::Extracted { .. } => "extract",
        MaterialImportMode::Replaced { .. } => "replace",
    }
}

/// A transient preset holding `object` (Apply to all reuses the preset merge).
fn preset_of(object: &SettingsObject) -> regia_asset::import_presets::ImportPreset {
    regia_asset::import_presets::ImportPreset {
        name: "apply to all".into(),
        importer: String::new(),
        settings: if object.fields.is_empty() {
            "()".into()
        } else {
            object.to_ron()
        },
    }
}

// ---------------------------------------------------------------------------------------------
// Import (OK)
// ---------------------------------------------------------------------------------------------

/// What the import writes: every file not skipped, with its settings text, checked.
fn plan(dialog: &ImportDialog) -> Result<Vec<(&DialogFile, Option<String>)>, RpcError> {
    let mut out = Vec::new();
    for file in dialog.files.iter().filter(|f| !f.skipped) {
        if let Some(key) = file.form.invalid.iter().next() {
            return Err(reasoned(
                "asset_settings_invalid",
                format!(
                    "{}: `{key}` holds a value that was refused: fix it first",
                    file.name()
                ),
            ));
        }
        if !file.scene.meshes.is_empty() && file.scene.meshes.iter().all(|m| !m.import) {
            return Err(reasoned(
                "asset_settings_invalid",
                format!(
                    "{}: every mesh is skipped: import at least one, or skip the file",
                    file.name()
                ),
            ));
        }
        let text = file.settings_text();
        if let (Some(text), Some(kind)) = (&text, file.kind) {
            validate_settings(kind.sample_path(), text).map_err(|message| {
                reasoned("asset_settings_invalid", format!("{}: {message}", file.name()))
            })?;
        }
        out.push((file, text));
    }
    if out.is_empty() {
        return Err(reasoned(
            "import_dialog_nothing",
            "every file is skipped: nothing to import",
        ));
    }
    Ok(out)
}

/// Imports the dialog's files now (blocking): new files get their `.meta` written next to their
/// destination first, then the pipeline import copies them and cooks once; a re-import writes
/// the `.meta` and cooks once. Answers `{ imported, cooked, skipped, errors, settings }`.
///
/// # Errors
///
/// `import_dialog_not_open`, `asset_settings_invalid`, `import_dialog_nothing`,
/// `import_destination_exists`, `import_failed`, or what the settings writer refuses.
pub fn import_now(dialog: &ImportDialog, client: Option<&str>) -> Result<Value, RpcError> {
    let root = dialog
        .root
        .clone()
        .ok_or_else(|| reasoned("import_dialog_not_open", "the import dialog is not open"))?;
    let planned = plan(dialog)?;
    let settings: serde_json::Map<String, Value> = planned
        .iter()
        .map(|(file, text)| (file.name(), json!(text)))
        .collect();
    if dialog.is_reimport() {
        extract_for_reimport(&root, &planned)?;
        let (first, rest) = planned
            .split_first()
            .ok_or_else(|| reasoned("import_dialog_nothing", "nothing to re-import"))?;
        let path = first.0.reimport.clone().unwrap_or_default();
        let extras: Vec<(String, Option<String>)> = rest
            .iter()
            .map(|(file, text)| (file.reimport.clone().unwrap_or_default(), text.clone()))
            .collect();
        let mut out =
            crate::import_settings_panel::write_sources_and_cook(&root, &path, first.1.as_deref(), &extras)?;
        if let Some(object) = out.as_object_mut() {
            object.insert("settings".into(), Value::Object(settings));
        }
        return Ok(out);
    }
    let written = prewrite_metas(&root, &dialog.folder, &planned)?;
    if let Err(error) = extract_for_new(&root, &planned, &written) {
        remove_orphan_metas(&written);
        return Err(error);
    }
    let sources: Vec<PathBuf> = planned.iter().map(|(file, _)| file.source.clone()).collect();
    let report = crate::content::import_files(&root, &dialog.folder, &sources, client);
    // A `.meta` whose source did not arrive (a refused or failed copy) must not stay behind.
    remove_orphan_metas(&written);
    let report = report.map_err(|message| reasoned("import_failed", message))?;
    Ok(json!({
        "imported": report.imported,
        "cooked": report.cooked,
        "skipped": report.skipped,
        "errors": report.errors.iter().map(|(path, message)| json!({ "path": path, "message": message })).collect::<Vec<_>>(),
        "cancelled": report.cancelled,
        "settings": settings,
    }))
}

/// Removes each `(destination, .meta)` pair's `.meta` whose destination is not a file: a prewritten
/// sidecar of a copy that was refused, failed, cancelled or never started (otherwise the next
/// Import is refused with `import_destination_exists` although the asset is absent).
fn remove_orphan_metas(written: &[(PathBuf, PathBuf)]) {
    for (dest, meta) in written {
        if !dest.is_file() {
            let _ = std::fs::remove_file(meta);
        }
    }
}

/// Once the window's background import has ended, removes its prewritten metas whose files never
/// arrived ([`remove_orphan_metas`]). A job still running keeps them.
fn sweep_prewritten(dialog: &mut ImportDialog) {
    if dialog.prewritten.as_ref().is_some_and(|(job, _)| !job.running())
        && let Some((_, written)) = dialog.prewritten.take()
    {
        remove_orphan_metas(&written);
    }
}

/// Writes each new file's `.meta` (a fresh id, the importer the cook would name, the settings)
/// next to its destination, refusing before any write when a destination or its `.meta` exists.
/// Answers `(destination, .meta)` of every file written.
fn prewrite_metas(
    root: &Path,
    folder: &str,
    planned: &[(&DialogFile, Option<String>)],
) -> Result<Vec<(PathBuf, PathBuf)>, RpcError> {
    let dest_dir = root.join("assets").join(folder);
    let mut writes = Vec::new();
    for (file, text) in planned {
        let dest = dest_dir.join(file.name());
        let meta = regia_asset::meta_path_for(&dest);
        if dest.exists() || meta.exists() {
            return Err(reasoned(
                "import_destination_exists",
                format!(
                    "{} already exists in assets/{folder}: rename one of them, or Re-import with options",
                    file.name()
                ),
            ));
        }
        if let Some(text) = text {
            let ext = dest
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            let mut sidecar = regia_asset::MetaSidecar::new(
                regia_asset_pipeline::import_rules::importer_for_ext(&ext),
                regia_asset::AssetId::new_random(),
            );
            sidecar.settings = Some(text.clone());
            writes.push((dest, meta, sidecar));
        }
    }
    let mut written = Vec::new();
    for (dest, meta, sidecar) in writes {
        let bytes = sidecar.to_ron_string().map_err(|e| {
            reasoned(
                "import_failed",
                format!("cannot serialize {}: {e}", meta.display()),
            )
        })?;
        if let Err(e) = crate::atomic_write::write_atomic(&meta, bytes.as_bytes()) {
            for (_, done) in &written {
                let _ = std::fs::remove_file(done);
            }
            return Err(reasoned(
                "import_failed",
                format!("cannot write {}: {e}", meta.display()),
            ));
        }
        written.push((dest, meta));
    }
    Ok(written)
}

/// Writes the extracted materials of new files (ADR-8376): each file's model id is the one its
/// prewritten `.meta` holds (`written` pairs destination and `.meta`).
fn extract_for_new(
    root: &Path,
    planned: &[(&DialogFile, Option<String>)],
    written: &[(PathBuf, PathBuf)],
) -> Result<(), RpcError> {
    for (file, text) in planned {
        let Some((_, meta)) = written
            .iter()
            .find(|(dest, _)| dest.file_name() == file.source.file_name())
        else {
            continue;
        };
        let parent = read_meta(meta)?.id;
        extract_materials(root, file, text.as_deref(), parent, &[])?;
    }
    Ok(())
}

/// Writes the extracted materials of re-imported files, with the ids their `.meta` stores.
fn extract_for_reimport(root: &Path, planned: &[(&DialogFile, Option<String>)]) -> Result<(), RpcError> {
    for (file, text) in planned {
        let Some(rel) = &file.reimport else {
            continue;
        };
        let meta = read_meta(&regia_asset::meta_path_for(&root.join(rel)))?;
        extract_materials(root, file, text.as_deref(), meta.id, &meta.sub_assets)?;
    }
    Ok(())
}

fn read_meta(path: &Path) -> Result<regia_asset::MetaSidecar, RpcError> {
    std::fs::read(path)
        .map_err(|e| e.to_string())
        .and_then(|bytes| regia_asset::MetaSidecar::from_ron(&bytes).map_err(|e| e.to_string()))
        .map_err(|e| reasoned("import_failed", format!("cannot read {}: {e}", path.display())))
}

/// Writes `file`'s Extract materials (ADR-8376) that are not in the project yet: the material the
/// importer cooks from the file (the same import the cook runs, model id `parent`, the `.meta`'s
/// stored sub-asset ids, so its texture references are the model's texture sub-assets), as a
/// `.material.ron` with a `.meta` holding the chosen id. A file already there with that id is
/// kept as it is (a re-import never overwrites edits to it); any other file at the path is
/// refused (`import_destination_exists`). Answers the files written.
fn extract_materials(
    root: &Path,
    file: &DialogFile,
    settings: Option<&str>,
    parent: regia_asset::AssetId,
    stored: &[regia_asset::SubAssetEntry],
) -> Result<Vec<PathBuf>, RpcError> {
    let mut todo = Vec::new();
    for material in &file.scene.materials {
        let MaterialImportMode::Extracted { asset, path } = &material.mode else {
            continue;
        };
        if let Some(problem) = extract_path_problem(path) {
            return Err(RpcError::invalid_params(problem));
        }
        let dest = root.join(path);
        let meta = regia_asset::meta_path_for(&dest);
        if dest.exists() || meta.exists() {
            let ours = dest.is_file() && read_meta(&meta).is_ok_and(|m| m.id == *asset);
            if ours {
                continue;
            }
            return Err(reasoned(
                "import_destination_exists",
                format!(
                    "{path} already exists: pick another path to extract `{}` to",
                    material.key
                ),
            ));
        }
        todo.push((material.key.as_str(), *asset, dest, meta));
    }
    if todo.is_empty() {
        return Ok(Vec::new());
    }
    let _span = tracing::info_span!("import_dialog_extract_materials", file = %file.name()).entered();
    let ids = regia_asset_pipeline::subasset_remap::SubAssetIds::from_entries(parent, stored);
    let imported = regia_asset_pipeline::subasset_remap::with_ids(ids, || {
        regia_asset_pipeline::import_gltf_path(
            &file.source,
            parent,
            regia_asset_pipeline::Forward::Convert,
            regia_asset_pipeline::host_platform(),
            settings,
            regia_asset_pipeline::TextureCookQuality::Low,
        )
    })
    .map_err(|e| reasoned("import_failed", format!("{}: {e}", file.name())))?;
    let mut written = Vec::new();
    for (key, asset, dest, meta) in todo {
        let sub_path = format!("material/{key}");
        let ron = imported
            .sub_assets
            .iter()
            .find(|sub| sub.path == sub_path)
            .ok_or_else(|| {
                reasoned(
                    "import_item_unknown",
                    format!("{} has no material {key:?}", file.name()),
                )
            })
            .and_then(|sub| {
                regia_asset::CookedStandardMaterial::from_postcard(&sub.payload)
                    .map_err(|e| e.to_string())
                    .and_then(|cooked| cooked.authored.to_ron_string().map_err(|e| e.to_string()))
                    .map_err(|e| reasoned("import_failed", format!("material {key:?}: {e}")))
            })?;
        let sidecar = regia_asset::MetaSidecar::new("material_ron", asset)
            .to_ron_string()
            .map_err(|e| {
                reasoned(
                    "import_failed",
                    format!("cannot serialize {}: {e}", meta.display()),
                )
            })?;
        let write = |path: &Path, bytes: &[u8]| {
            crate::atomic_write::write_atomic(path, bytes)
                .map_err(|e| reasoned("import_failed", format!("cannot write {}: {e}", path.display())))
        };
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| reasoned("import_failed", format!("cannot create {}: {e}", dir.display())))?;
        }
        write(&meta, sidecar.as_bytes())?;
        if let Err(error) = write(&dest, ron.as_bytes()) {
            let _ = std::fs::remove_file(&meta);
            return Err(error);
        }
        written.push(dest);
    }
    Ok(written)
}

/// The dialog's Import from the window: new files import as a background Content ▸ Import job
/// (the Content Browser shows its progress and summary); a re-import runs as the Import settings
/// write job (`import_settings:<n>`). Answers the job id.
fn import_from_window(world: &mut World, dialog: &mut ImportDialog) -> Result<String, RpcError> {
    let root = dialog
        .root
        .clone()
        .ok_or_else(|| reasoned("import_dialog_not_open", "the import dialog is not open"))?;
    if let Some(reason) = crate::import_settings_panel::apply_busy_reason(world) {
        return Err(reasoned("tools_busy", format!("{reason}: import once it ends")));
    }
    let planned = plan(dialog)?;
    if dialog.is_reimport() {
        extract_for_reimport(&root, &planned)?;
        let mut plans = Vec::new();
        for (file, text) in &planned {
            plans.push(crate::asset_ops_agent::plan_import_settings(
                &root,
                file.reimport.as_deref().unwrap_or_default(),
                text.as_deref(),
            )?);
        }
        return crate::import_settings_panel::start_write_jobs(
            world,
            plans,
            crate::import_settings_panel::ApplyOrigin::Method,
            SettingsObject::default(),
            Vec::new(),
        );
    }
    let written = prewrite_metas(&root, &dialog.folder, &planned)?;
    if let Err(error) = extract_for_new(&root, &planned, &written) {
        remove_orphan_metas(&written);
        return Err(error);
    }
    let sources: Vec<PathBuf> = planned.iter().map(|(file, _)| file.source.clone()).collect();
    let job = match crate::editor_jobs::spawn_import(world, root, dialog.folder.clone(), sources, None) {
        Ok(job) => job,
        Err(message) => {
            // The worker never started: none of the metas just written has its file.
            remove_orphan_metas(&written);
            return Err(reasoned("import_failed", message));
        }
    };
    let id = job.id.clone();
    sweep_prewritten(dialog);
    dialog.prewritten = Some((job.clone(), written));
    world.insert_resource(crate::editor_jobs::ContentImport(job));
    Ok(id)
}

/// Opens the dialog over `sources` (Content ▸ Import… and the OS drop path call it when
/// [`wants_dialog`] says so).
///
/// # Errors
///
/// As [`ImportDialog::open_files`].
pub fn open_for_files(
    world: &mut World,
    root: &Path,
    folder: &str,
    sources: &[PathBuf],
) -> Result<(), RpcError> {
    let mut dialog = world.remove_resource::<ImportDialog>().unwrap_or_default();
    let out = dialog.open_files(root, folder, sources);
    world.insert_resource(dialog);
    out
}

/// Opens the dialog over the imported model `path` (Content ▸ Re-import with options…).
///
/// # Errors
///
/// As [`ImportDialog::open_reimport`].
pub fn open_for_reimport(world: &mut World, root: &Path, path: &str) -> Result<(), RpcError> {
    let mut dialog = world.remove_resource::<ImportDialog>().unwrap_or_default();
    let out = dialog.open_reimport(root, path);
    world.insert_resource(dialog);
    out
}

// ---------------------------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------------------------

/// The preview's side, in points (it scales with the UI scale like every point size).
const PREVIEW_SIDE: f32 = 256.0;

/// Draws the dialog when it is open (the Content Browser calls it each frame).
pub fn import_dialog_window(ctx: &egui::Context, world: &mut World) {
    let Some(mut dialog) = world.remove_resource::<ImportDialog>() else {
        return;
    };
    sweep_prewritten(&mut dialog);
    if dialog.open {
        let device = world.get_resource::<regia_render::RenderDevice>().cloned();
        let library = world
            .get_resource_or_insert_with(crate::mesh_source::MeshLibrary::default)
            .clone();
        let mut open = true;
        egui::Window::new(if dialog.is_reimport() {
            "Re-import with options"
        } else {
            "Import"
        })
        .id(egui::Id::new("import-dialog-window"))
        .open(&mut open)
        .resizable(true)
        .default_width(720.0)
        .show(ctx, |ui| {
            import_dialog_ui(ui, &mut dialog, device.as_ref(), &library)
        });
        dialog.open = open && dialog.open;
    }
    let requested = dialog.requested.take();
    if let Some(all) = requested {
        if all {
            dialog.apply_to_all();
        }
        match import_from_window(world, &mut dialog) {
            Ok(job) => {
                dialog.last_result = Some(json!({ "job": job }));
                dialog.open = false;
                dialog.error = None;
            }
            Err(error) => dialog.error = Some((code_of(&error), error.message)),
        }
    }
    world.insert_resource(dialog);
}

fn code_of(error: &RpcError) -> String {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or("import_failed")
        .to_string()
}

/// The dialog's contents: the file list, the preview and scene tree of the shown file, its
/// settings form and presets, and the Import / Apply to all / Cancel row.
pub fn import_dialog_ui(
    ui: &mut egui::Ui,
    dialog: &mut ImportDialog,
    device: Option<&regia_render::RenderDevice>,
    library: &crate::mesh_source::MeshLibrary,
) {
    if dialog.files.is_empty() {
        ui.weak("Content ▸ Import… with several files or a model opens this dialog.");
        return;
    }
    dialog.current = dialog.current.min(dialog.files.len() - 1);
    if !dialog.is_reimport() {
        ui.weak(format!(
            "{} file(s) into {}",
            dialog.files.len(),
            format!("assets/{}", dialog.folder).trim_end_matches('/')
        ));
    }
    // The file list: pick the shown file; each row's Skip leaves it out.
    if dialog.files.len() > 1 {
        let names: Vec<String> = dialog
            .files
            .iter()
            .map(|file| {
                if file.skipped {
                    format!("{} (skipped)", file.name())
                } else {
                    file.name()
                }
            })
            .collect();
        ui.horizontal_wrapped(|ui| {
            for (index, text) in names.into_iter().enumerate() {
                ui.selectable_value(&mut dialog.current, index, text);
            }
        });
    }
    let current = dialog.current;
    let orbit = dialog.orbit;
    let mut new_orbit = orbit;
    let mut preview_slot = (dialog.preview.take(), dialog.preview_model.take());
    {
        let assets = &dialog.material_assets;
        let dir = model_dir(&dialog.folder, &dialog.files[current]);
        let file = &mut dialog.files[current];
        ui.horizontal(|ui| {
            ui.strong(file.name());
            if let Some(kind) = file.kind {
                ui.weak(format!("{} importer", kind.name()));
            } else {
                ui.weak("imported as it is (no settings)");
            }
            let mut skipped = file.skipped;
            if crate::agent_parity::action_toggle(ui, &crate::agent_parity::IMPORT_DIALOG_SKIP, &mut skipped)
                .changed()
                && once_per_frame(ui, "import-dialog-skip")
            {
                file.skipped = skipped;
            }
        });
        if let Some(problem) = &file.problem {
            ui.colored_label(ui.visuals().warn_fg_color, problem);
        }
        if file.is_model() && file.problem.is_none() {
            ui.horizontal_top(|ui| {
                preview_ui(
                    ui,
                    file,
                    current,
                    &mut new_orbit,
                    &mut preview_slot,
                    device,
                    library,
                );
                ui.vertical(|ui| scene_tree_ui(ui, file, &dir, assets));
            });
        }
        if file.kind.is_some() {
            ui.separator();
            settings_section_ui(ui, file);
        }
    }
    (dialog.preview, dialog.preview_model) = preview_slot;
    dialog.orbit = new_orbit;
    preset_row_ui(ui, dialog);
    ui.separator();
    ui.horizontal(|ui| {
        let any = dialog.files.iter().any(|f| !f.skipped);
        if crate::agent_parity::action_enabled(ui, any, &crate::agent_parity::IMPORT_DIALOG_OK, None)
            .clicked()
        {
            dialog.requested = Some(false);
        }
        let same_kind = dialog
            .files
            .iter()
            .filter(|f| f.kind.is_some() && f.kind == dialog.files[current].kind)
            .count();
        if same_kind > 1
            && crate::agent_parity::action_enabled(
                ui,
                any,
                &crate::agent_parity::IMPORT_DIALOG_APPLY_ALL,
                None,
            )
            .on_hover_text(
                "Use these settings for every file of this importer (each keeps its own clips), then import",
            )
            .clicked()
        {
            dialog.requested = Some(true);
        }
        if crate::agent_parity::action_button(ui, &crate::agent_parity::IMPORT_DIALOG_CANCEL).clicked() {
            dialog.open = false;
        }
    });
    if let Some((code, message)) = &dialog.error {
        ui.colored_label(ui.visuals().error_fg_color, crate::data_panels::sentence(message))
            .on_hover_text(format!("Agent error code: {code}"));
    }
}

type PreviewSlot = (
    Option<((usize, i32, i32), egui::TextureHandle)>,
    Option<(usize, Arc<crate::mesh_source::LoadedModel>)>,
);

/// The orbit preview: rendered again only when the file or the orbit changes. Drag turns it; with
/// keyboard focus the arrow keys do.
fn preview_ui(
    ui: &mut egui::Ui,
    file: &DialogFile,
    index: usize,
    orbit: &mut (f32, f32),
    slot: &mut PreviewSlot,
    device: Option<&regia_render::RenderDevice>,
    library: &crate::mesh_source::MeshLibrary,
) {
    let side = PREVIEW_SIDE;
    let Some(device) = device else {
        ui.allocate_ui(egui::vec2(side, side), |ui| ui.weak("no preview (no GPU device)"));
        return;
    };
    let path = file.preview_glb.clone().unwrap_or_else(|| file.source.clone());
    if slot.1.as_ref().is_none_or(|(i, _)| *i != index) {
        slot.0 = None;
        slot.1 = library
            .model(None, &path.to_string_lossy())
            .map_err(|e| tracing::warn!(target: "regia_editor", "import preview: {e}"))
            .ok()
            .map(|model| (index, model));
    }
    let Some((_, model)) = &slot.1 else {
        ui.allocate_ui(egui::vec2(side, side), |ui| ui.weak("no preview for this file"));
        return;
    };
    // Whole milliradians: a texture per visible orbit step, not per float wobble.
    let key = (index, (orbit.0 * 1000.0) as i32, (orbit.1 * 1000.0) as i32);
    if slot.0.as_ref().is_none_or(|(k, _)| *k != key) {
        let pixels = (side * ui.ctx().pixels_per_point()).round().max(16.0) as u32;
        let _span = tracing::info_span!("import_dialog_preview").entered();
        slot.0 = crate::asset_viewer::render_model(
            device,
            &model.parts,
            (model.aabb_min, model.aabb_max),
            orbit.0,
            orbit.1,
            [pixels, pixels],
        )
        .map(|image| {
            (
                key,
                ui.ctx()
                    .load_texture("import-dialog-preview", image, egui::TextureOptions::LINEAR),
            )
        });
    }
    if let Some((_, texture)) = &slot.0 {
        let response = ui
            .add(
                egui::Image::new(texture)
                    .fit_to_exact_size(egui::vec2(side, side))
                    .sense(egui::Sense::click_and_drag()),
            )
            .on_hover_text("Drag, or focus and use the arrow keys, to turn the model");
        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Image, true, "Model preview"));
        let delta = response.drag_delta();
        orbit.0 -= delta.x * 0.01;
        orbit.1 = (orbit.1 + delta.y * 0.01).clamp(-1.4, 1.4);
        if response.has_focus() {
            let step = 0.1;
            ui.input(|input| {
                if input.key_pressed(egui::Key::ArrowLeft) {
                    orbit.0 += step;
                }
                if input.key_pressed(egui::Key::ArrowRight) {
                    orbit.0 -= step;
                }
                if input.key_pressed(egui::Key::ArrowUp) {
                    orbit.1 = (orbit.1 - step).clamp(-1.4, 1.4);
                }
                if input.key_pressed(egui::Key::ArrowDown) {
                    orbit.1 = (orbit.1 + step).clamp(-1.4, 1.4);
                }
            });
        }
    }
}

/// The scene tree: meshes with their Import toggle and materials with their Use choice (glTF/GLB,
/// ADR-8376; read-only for FBX/OBJ), animations with their clip options. `dir` is the model's
/// project folder (an Extract writes next to it), `assets` the Replace picker's materials.
fn scene_tree_ui(
    ui: &mut egui::Ui,
    file: &mut DialogFile,
    dir: &str,
    assets: &[(String, regia_asset::AssetId)],
) {
    let editable = file.takes_part_options();
    let stem = file
        .source
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    egui::ScrollArea::vertical()
        .id_salt("import-dialog-tree")
        .max_height(PREVIEW_SIDE)
        .show(ui, |ui| {
            egui::CollapsingHeader::new(format!("Meshes ({})", file.scene.meshes.len()))
                .id_salt("import-dialog-meshes")
                .default_open(true)
                .show(ui, |ui| {
                    for (index, mesh) in file.scene.meshes.iter_mut().enumerate() {
                        mesh_ui(ui, index, mesh, editable);
                    }
                });
            egui::CollapsingHeader::new(format!("Materials ({})", file.scene.materials.len()))
                .id_salt("import-dialog-materials")
                .default_open(true)
                .show(ui, |ui| {
                    for (index, material) in file.scene.materials.iter_mut().enumerate() {
                        material_ui(ui, index, material, editable, (dir, stem.as_str()), assets);
                    }
                });
            egui::CollapsingHeader::new(format!("Animations ({})", file.scene.clips.len()))
                .id_salt("import-dialog-clips")
                .default_open(true)
                .show(ui, |ui| {
                    for (index, clip) in file.scene.clips.iter_mut().enumerate() {
                        clip_ui(ui, index, clip);
                    }
                });
        });
}

/// One mesh row: the Import toggle (glTF/GLB) and what the node holds.
fn mesh_ui(ui: &mut egui::Ui, index: usize, mesh: &mut MeshItem, editable: bool) {
    ui.push_id(("import-dialog-mesh", index), |ui| {
        ui.horizontal(|ui| {
            if editable {
                let mut import = mesh.import;
                if crate::agent_parity::action_toggle(
                    ui,
                    &crate::agent_parity::IMPORT_DIALOG_MESH_IMPORT,
                    &mut import,
                )
                .on_hover_text(format!("Import the mesh `{}`", mesh.key))
                .changed()
                    && once_per_frame(ui, "import-dialog-mesh-import")
                {
                    mesh.import = import;
                }
            }
            let text = format!(
                "{} · {} part(s) · {} triangles",
                mesh.name, mesh.primitives, mesh.triangles
            );
            if mesh.import {
                ui.label(text);
            } else {
                ui.weak(format!("{text} (skipped)"));
            }
        });
    });
}

/// The words the dialog shows for a material mode.
fn mode_label(mode: &str) -> &'static str {
    match mode {
        "extract" => "Extract",
        "replace" => "Replace",
        _ => "Embedded",
    }
}

/// One material row: what it is, then (glTF/GLB) its Use choice: Embedded, Extract (to the file
/// shown) or Replace (with the asset picked from the project's materials).
fn material_ui(
    ui: &mut egui::Ui,
    index: usize,
    material: &mut MaterialItem,
    editable: bool,
    (dir, stem): (&str, &str),
    assets: &[(String, regia_asset::AssetId)],
) {
    ui.push_id(("import-dialog-material", index), |ui| {
        let c = material.base_color;
        ui.label(format!(
            "{} · base {:.2},{:.2},{:.2} · {} texture(s)",
            material.name, c[0], c[1], c[2], material.textures
        ));
        if !editable {
            return;
        }
        let current = mode_name(&material.mode);
        let mut choice = current;
        ui.horizontal(|ui| {
            ui.label("Use");
            for option in ["embedded", "extract", "replace"] {
                if option == "replace" && assets.is_empty() {
                    ui.weak("(no material asset in the project to replace it with)");
                    continue;
                }
                ui.selectable_value(&mut choice, option, mode_label(option));
            }
        });
        if choice != current && once_per_frame(ui, "import-dialog-material-mode") {
            material.mode = match choice {
                "extract" => MaterialImportMode::Extracted {
                    asset: regia_asset::AssetId::new_random(),
                    path: default_extract_path(dir, stem, &material.key),
                },
                "replace" => match assets.first() {
                    Some((path, asset)) => MaterialImportMode::Replaced {
                        asset: *asset,
                        path: path.clone(),
                    },
                    None => MaterialImportMode::Embedded,
                },
                _ => MaterialImportMode::Embedded,
            };
        }
        match &material.mode {
            MaterialImportMode::Extracted { path, .. } => {
                ui.weak(format!("written to {path} at Import"));
            }
            MaterialImportMode::Replaced { path, .. } => {
                let mut picked = path.clone();
                ui.weak("with");
                egui::ScrollArea::vertical()
                    .id_salt("import-dialog-material-assets")
                    .max_height(96.0)
                    .show(ui, |ui| {
                        for (asset_path, _) in assets {
                            ui.selectable_value(&mut picked, asset_path.clone(), asset_path.as_str());
                        }
                    });
                if picked != *path
                    && once_per_frame(ui, "import-dialog-material-asset")
                    && let Some((path, asset)) = assets.iter().find(|(p, _)| *p == picked)
                {
                    material.mode = MaterialImportMode::Replaced {
                        asset: *asset,
                        path: path.clone(),
                    };
                }
            }
            MaterialImportMode::Embedded => {}
        }
    });
}

/// One animation's clip options: Import, the clip name, and its split ranges (in frames).
fn clip_ui(ui: &mut egui::Ui, index: usize, clip: &mut ClipItem) {
    ui.push_id(("import-dialog-clip", index), |ui| {
        ui.horizontal(|ui| {
            let mut import = clip.import;
            if crate::agent_parity::action_toggle(
                ui,
                &crate::agent_parity::IMPORT_DIALOG_CLIP_IMPORT,
                &mut import,
            )
            .on_hover_text(format!("Import a clip from `{}`", clip.animation))
            .changed()
                && once_per_frame(ui, "import-dialog-clip-import")
            {
                clip.import = import;
            }
            ui.weak(format!(
                "{} · {} frames",
                clip.animation,
                (clip.duration * CLIP_FPS).round()
            ));
        });
        if !clip.import {
            return;
        }
        ui.horizontal(|ui| {
            let label = ui.label("Clip name");
            ui.add(egui::TextEdit::singleline(&mut clip.name).desired_width(160.0))
                .labelled_by(label.id);
        });
        let last = (clip.duration * CLIP_FPS).round().max(1.0);
        let mut remove = None;
        for (r, range) in clip.ranges.iter_mut().enumerate() {
            ui.push_id(r, |ui| {
                ui.horizontal(|ui| {
                    let label = ui.label("Clip");
                    ui.add(egui::TextEdit::singleline(&mut range.name).desired_width(100.0))
                        .labelled_by(label.id);
                    let mut start = (range.start * CLIP_FPS).round();
                    let mut end = (range.end * CLIP_FPS).round();
                    let from = ui.label("from frame");
                    ui.add(egui::DragValue::new(&mut start).range(0.0..=last))
                        .labelled_by(from.id);
                    let to = ui.label("to");
                    ui.add(egui::DragValue::new(&mut end).range(0.0..=last))
                        .labelled_by(to.id);
                    range.start = start / CLIP_FPS;
                    range.end = end / CLIP_FPS;
                    if crate::agent_parity::action_button(
                        ui,
                        &crate::agent_parity::IMPORT_DIALOG_CLIP_REMOVE_RANGE,
                    )
                    .clicked()
                        && once_per_frame(ui, "import-dialog-remove-range")
                    {
                        remove = Some(r);
                    }
                    if end <= start {
                        ui.colored_label(ui.visuals().warn_fg_color, "end must be after start");
                    }
                });
            });
        }
        if let Some(r) = remove {
            clip.ranges.remove(r);
        }
        if crate::agent_parity::action_button(ui, &crate::agent_parity::IMPORT_DIALOG_CLIP_ADD_RANGE)
            .on_hover_text("Cut another clip out of this take")
            .clicked()
            && once_per_frame(ui, "import-dialog-add-range")
        {
            let start = clip.ranges.last().map_or(0.0, |r| r.end);
            let end = clip.duration.max(start + 1.0 / CLIP_FPS);
            clip.ranges.push(RangeItem {
                name: format!("{}_{}", clip.name, clip.ranges.len()),
                start,
                end,
            });
        }
    });
}

/// True the first time this frame a click of `what` in `ui` is applied: egui runs the UI again when
/// a pass asks for a discard, and that pass sees the same click, so a toggle flipped twice did
/// nothing, Add range added two and Remove range removed the next row too (review g18 row 5).
/// The import itself already runs once, after the window (`dialog.requested`).
fn once_per_frame(ui: &egui::Ui, what: &str) -> bool {
    crate::mode2d::first_apply_this_frame(ui.ctx(), ui.id().with(what))
}

/// The shown file's importer settings form (the Import settings panel's grid).
fn settings_section_ui(ui: &mut egui::Ui, file: &mut DialogFile) {
    ui.strong("Importer settings");
    crate::import_settings_panel::settings_form_ui(ui, &mut file.form);
    if let Some((code, message)) = &file.form.error {
        ui.colored_label(ui.visuals().error_fg_color, crate::data_panels::sentence(message))
            .on_hover_text(format!("Agent error code: {code}"));
    }
}

/// The dialog's preset row: pick a project preset of the shown file's importer and apply it.
fn preset_row_ui(ui: &mut egui::Ui, dialog: &mut ImportDialog) {
    let (Some(root), Some(kind)) = (
        dialog.root.clone(),
        dialog.files.get(dialog.current).and_then(|f| f.kind),
    ) else {
        return;
    };
    let names = dialog.files[dialog.current]
        .form
        .preset_names
        .get_or_insert_with(|| crate::import_presets::names(&root, kind))
        .clone();
    if names.is_empty() {
        return;
    }
    ui.horizontal(|ui| {
        let label = ui.label("Preset");
        egui::ComboBox::from_id_salt("import-dialog-preset")
            .selected_text(dialog.preset_choice.clone().unwrap_or_else(|| "pick one".into()))
            .show_ui(ui, |ui| {
                for name in &names {
                    ui.selectable_value(&mut dialog.preset_choice, Some(name.clone()), name.as_str());
                }
            })
            .response
            .labelled_by(label.id);
        let chosen = dialog.preset_choice.clone().filter(|c| names.contains(c));
        if crate::agent_parity::action_enabled(
            ui,
            chosen.is_some(),
            &crate::agent_parity::IMPORT_DIALOG_PRESET,
            None,
        )
        .clicked()
            && once_per_frame(ui, "import-dialog-apply-preset")
            && let Some(name) = chosen
            && let Err(error) = dialog.apply_preset(&name)
        {
            dialog.error = Some((code_of(&error), error.message));
        }
    });
}

// ---------------------------------------------------------------------------------------------
// Agent methods
// ---------------------------------------------------------------------------------------------

fn project_root(world: &World) -> Result<PathBuf, RpcError> {
    world
        .get_resource::<EditorProject>()
        .and_then(|p| p.root.clone())
        .ok_or_else(|| {
            RpcError::not_found(
                "no project is open",
                "start the editor with --project <dir> (or open one in the project browser)",
            )
        })
}

/// `paths` (array) or `path` (string) as file paths.
fn paths_param(params: &Value) -> Result<Vec<String>, RpcError> {
    let mut out: Vec<String> = match params.get("paths") {
        Some(Value::Array(list)) => list
            .iter()
            .map(|p| {
                p.as_str()
                    .filter(|p| !p.is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| RpcError::invalid_params("`paths` holds non-empty path strings"))
            })
            .collect::<Result<_, _>>()?,
        Some(Value::Null) | None => Vec::new(),
        Some(_) => return Err(RpcError::invalid_params("`paths` is an array of file paths")),
    };
    if let Some(path) = params
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
    {
        out.insert(0, path.to_string());
    }
    Ok(out)
}

/// Opens a dialog model from `params`: `reimport` (a project source) or `paths` (files to import
/// into `folder`).
fn open_from_params(dialog: &mut ImportDialog, root: &Path, params: &Value) -> Result<(), RpcError> {
    if let Some(path) = params.get("reimport").and_then(Value::as_str) {
        return dialog.open_reimport(root, path);
    }
    let paths = paths_param(params)?;
    if paths.is_empty() {
        return Err(RpcError::invalid_params(
            "`paths` (files to import) or `reimport` (an imported model under assets/) is required",
        ));
    }
    let folder = params.get("folder").and_then(Value::as_str).unwrap_or_default();
    let sources: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
    dialog.open_files(root, folder, &sources)
}

/// `editor.import_dialog_open`.
fn import_dialog_open(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if params.get("close").and_then(Value::as_bool) == Some(true) {
        // The dialog's Cancel: nothing is imported.
        let mut dialog = world.remove_resource::<ImportDialog>().unwrap_or_default();
        dialog.open = false;
        let state = dialog.state_json();
        world.insert_resource(dialog);
        return Ok(state);
    }
    let root = project_root(world)?;
    let mut dialog = world.remove_resource::<ImportDialog>().unwrap_or_default();
    let out = open_from_params(&mut dialog, &root, params).map(|()| dialog.state_json());
    world.insert_resource(dialog);
    out
}

/// `editor.import_dialog_state`.
fn import_dialog_state(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Ok(world
        .get_resource::<ImportDialog>()
        .map_or_else(|| ImportDialog::default().state_json(), ImportDialog::state_json))
}

/// Applies `editor.import_with`'s options to a dialog model: `skip` (file names), `settings` (a
/// RON object for the current file), `preset`, `items`, `apply_to_all`.
fn apply_with_params(dialog: &mut ImportDialog, params: &Value) -> Result<(), RpcError> {
    if let Some(file) = params.get("file").and_then(Value::as_u64) {
        let file = file as usize;
        if file >= dialog.files.len() {
            return Err(RpcError::invalid_params(format!(
                "`file` {file} is out of range ({} file(s))",
                dialog.files.len()
            )));
        }
        dialog.current = file;
    }
    if let Some(Value::Array(skips)) = params.get("skip") {
        for name in skips.iter().filter_map(Value::as_str) {
            let file = dialog
                .files
                .iter_mut()
                .find(|f| f.name() == name)
                .ok_or_else(|| {
                    reasoned(
                        "import_item_unknown",
                        format!("no file named {name:?} in the dialog"),
                    )
                })?;
            file.skipped = true;
        }
    }
    let current = dialog.current;
    if let Some(text) = params.get("settings").and_then(Value::as_str) {
        let file = dialog
            .files
            .get_mut(current)
            .ok_or_else(|| reasoned("import_dialog_not_open", "the import dialog shows no file"))?;
        let object =
            SettingsObject::parse(text).map_err(|e| reasoned("asset_settings_invalid", e.message))?;
        file.form.form = crate::import_presets::applied(&file.form.form, &preset_of(&object))?;
    }
    if let Some(name) = params.get("preset").and_then(Value::as_str) {
        dialog.apply_preset(name)?;
    }
    if let Some(Value::Array(items)) = params.get("items") {
        dialog.apply_items(current, items)?;
    }
    if params.get("apply_to_all").and_then(Value::as_bool) == Some(true) {
        dialog.apply_to_all();
    }
    Ok(())
}

/// `editor.import_with`: one call that does what the dialog's Import does. With `paths` or
/// `reimport` it builds its own dialog model (the window is untouched); without them it imports
/// the open dialog (which then closes).
fn import_with(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let root = project_root(world)?;
    if let Some(reason) = crate::import_settings_panel::apply_busy_reason(world) {
        return Err(reasoned("tools_busy", format!("{reason}: import once it ends")));
    }
    let own = params.get("reimport").is_some() || !paths_param(params)?.is_empty();
    let mut dialog = if own {
        let mut dialog = ImportDialog::default();
        open_from_params(&mut dialog, &root, params)?;
        dialog
    } else {
        let dialog = world.remove_resource::<ImportDialog>().unwrap_or_default();
        if !dialog.open || dialog.files.is_empty() {
            world.insert_resource(dialog);
            return Err(reasoned(
                "import_dialog_not_open",
                "no import dialog is open: pass `paths` or `reimport`, or open one with editor.import_dialog_open",
            ));
        }
        dialog
    };
    let result = apply_with_params(&mut dialog, params).and_then(|()| import_now(&dialog, Some("agent")));
    if !own {
        match &result {
            Ok(out) => {
                dialog.open = false;
                dialog.error = None;
                dialog.last_result = Some(out.clone());
            }
            Err(error) => dialog.error = Some((code_of(error), error.message.clone())),
        }
        world.insert_resource(dialog);
    }
    let out = result?;
    crate::content_index::invalidate(world);
    Ok(out)
}

/// The dialog's agent methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.import_dialog_open",
                tool: "import_dialog_open",
                description: "Open the import dialog (what Content ▸ Import… does with several files or a \
                    model, and Re-import with options… on an imported glTF/FBX/OBJ): paths = files \
                    to import into folder, or reimport = an imported model under assets/; close: true \
                    is its Cancel. Returns the dialog state (editor.import_dialog_state).",
                params: &[
                    ParamSchema {
                        name: "paths",
                        kind: "array",
                        required: false,
                        description: "Files to import (outside the project).",
                    },
                    ParamSchema {
                        name: "folder",
                        kind: "string",
                        required: false,
                        description: "Destination folder under assets/ (default assets/).",
                    },
                    ParamSchema {
                        name: "reimport",
                        kind: "string",
                        required: false,
                        description: "An imported model (project-relative, with a .meta) to re-import with options.",
                    },
                    ParamSchema {
                        name: "close",
                        kind: "boolean",
                        required: false,
                        description: "true is the dialog's Cancel: close it, import nothing.",
                    },
                ],
                result: "the dialog state",
                mutating: true,
                permission: Permission::Assets,
            },
            import_dialog_open as Handler,
        ),
        (
            MethodSchema {
                name: "editor.import_dialog_state",
                tool: "import_dialog_state",
                description: "The import dialog: mode (import or reimport), destination folder, the \
                    shown file, and per file its importer, skipped, the settings it imports with, \
                    and its scene tree (meshes with parts and triangles, materials with base color \
                    and texture count, animations with duration, frames at 30 fps, import, clip name \
                    and split ranges). glTF/GLB meshes carry key and import, materials key, mode \
                    (embedded, extract, replace), path and asset; material_assets lists the project \
                    materials a replace can pick (ADR-8376).",
                params: &[],
                result: "{ open, mode, folder, current, files: [{ source, name, reimport, importer, skipped, settings, invalid, problem, preview, part_options, scene: { meshes: [{ name, key, import, primitives, triangles }], materials: [{ name, key, mode, path, asset, base_color, textures }], clips } }], error, last_result, material_assets: [{ path, asset }] }",
                mutating: false,
                permission: Permission::Read,
            },
            import_dialog_state as Handler,
        ),
        (
            MethodSchema {
                name: "editor.import_with",
                tool: "import_with",
                description: "Import with options, as the dialog's Import does: every file's .meta is \
                    written with its settings before the copy, then one cook (a re-import writes \
                    the .meta and cooks once). paths or reimport build the dialog model for this \
                    call; without them the open dialog is imported. Options: settings (RON object \
                    for the shown file), preset, items (clip options: { kind: clip, animation, \
                    import, name, ranges: [{ name, start, end } in seconds or start_frame/end_frame \
                    at 30 fps] }; glTF/GLB mesh options: { kind: mesh, mesh: <key>, import }; \
                    material options: { kind: material, material: <key>, mode: embedded | extract \
                    [path: assets/...material.ron] | replace path: <a project material> }), skip \
                    (file names), file (index of the file the options apply to), apply_to_all \
                    (copy the shown file's settings to every file of its importer).",
                params: &[
                    ParamSchema {
                        name: "paths",
                        kind: "array",
                        required: false,
                        description: "Files to import (outside the project).",
                    },
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: false,
                        description: "One file to import (before paths).",
                    },
                    ParamSchema {
                        name: "folder",
                        kind: "string",
                        required: false,
                        description: "Destination folder under assets/.",
                    },
                    ParamSchema {
                        name: "reimport",
                        kind: "string",
                        required: false,
                        description: "An imported model to re-import with options.",
                    },
                    ParamSchema {
                        name: "file",
                        kind: "integer",
                        required: false,
                        description: "Index of the file settings, preset and items apply to (default 0).",
                    },
                    ParamSchema {
                        name: "settings",
                        kind: "string",
                        required: false,
                        description: "A RON settings object for that file (its clips stay).",
                    },
                    ParamSchema {
                        name: "preset",
                        kind: "string",
                        required: false,
                        description: "A project preset of that file's importer.",
                    },
                    ParamSchema {
                        name: "items",
                        kind: "array",
                        required: false,
                        description: "Clip, mesh and material options (mesh and material: glTF/GLB only).",
                    },
                    ParamSchema {
                        name: "skip",
                        kind: "array",
                        required: false,
                        description: "File names to leave out.",
                    },
                    ParamSchema {
                        name: "apply_to_all",
                        kind: "boolean",
                        required: false,
                        description: "Copy that file's settings (not its clips) to every file of the same importer first.",
                    },
                ],
                result: "{ imported, cooked, skipped, errors: [{ path, message }], cancelled, settings: { <file>: <settings or null> } } (re-import: editor.set_import_settings' result plus settings)",
                mutating: true,
                permission: Permission::Assets,
            },
            import_with as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests;
