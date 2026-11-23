// SPDX-License-Identifier: MPL-2.0
//! Level templates: File ▸ New level opens a dialog that starts a level from a template
//! (`editor.new_level { template }`, `editor.level_templates`; `book/agents/level-templates.md`).
//!
//! Four built-in templates (Empty, Basic lit, Interior, Open world) plus every `.level.ron` under
//! the project's `templates/levels/` folder. A template never changes: a built-in is built from
//! code, a project template is read and copied into a new unsaved level with a fresh level id, so
//! File ▸ Save asks for a new path and the template file is not the open level's file.
//! `file.new_level` (the palette command) keeps its old behavior, an empty level.

use std::path::{Path, PathBuf};

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::id::Guid;
use regia_core::math::{Quat, Vec2, Vec3};
use regia_ecs::prelude::*;
use regia_render::{Primitive, PrimitiveShape};
use regia_scene::level::{LevelSettings, LevelTerrain, open_level, snapshot_level};
use regia_scene::prelude::{
    Level, LevelId, LevelLibrary, LevelManager, LoadMode, SceneEntity, SceneFile, Transform,
};
use serde_json::{Value, json};

use crate::command::{SpawnEntities, UndoStack};
use crate::level_io;

/// Folder under the project root whose `.level.ron` files are offered as templates.
pub const TEMPLATES_DIR: &str = "templates/levels";

/// Suffix of a template file.
const LEVEL_SUFFIX: &str = ".level.ron";

/// How deep the template folder is scanned.
const MAX_DEPTH: usize = 4;

/// The built-in templates: `(id, label, description)`.
pub const BUILTIN_TEMPLATES: &[(&str, &str, &str)] = &[
    ("empty", "Empty", "A level with nothing in it."),
    (
        "basic_lit",
        "Basic lit",
        "A sun, the default sky, a floor and a player start.",
    ),
    (
        "interior",
        "Interior",
        "An enclosed room (floor, ceiling, four walls) with two lights and a player start.",
    ),
    (
        "open_world",
        "Open world",
        "A sun and a player start on the project's terrain, or on a large floor when the project has none.",
    ),
];

/// One template the New level dialog offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateInfo {
    /// The id `editor.new_level { template }` takes: a built-in id, or `project:<path under
    /// templates/levels without .level.ron>`.
    pub id: String,
    /// The dialog's label.
    pub label: String,
    /// One sentence about what the template makes.
    pub description: String,
    /// True for the four built-ins.
    pub builtin: bool,
    /// The file of a project template.
    pub path: Option<PathBuf>,
    /// The entity kinds a new level from this template holds, in spawn order: `directional_light`,
    /// `point_light`, `player_start`, `plane` or `cuboid`. Empty for a project template (its
    /// content is whatever the file holds) and for `empty`.
    pub kinds: Vec<String>,
}

/// A refused template operation: a lowercase code and a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateError {
    /// The agent error code (`level_template_unknown`, `level_template_unreadable`, ...).
    pub code: &'static str,
    /// What went wrong.
    pub message: String,
}

impl TemplateError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The agent error.
    #[must_use]
    pub fn rpc(&self) -> RpcError {
        RpcError::new(
            INVALID_PARAMS,
            self.code,
            self.message.clone(),
            "list the templates with editor.level_templates",
        )
    }
}

/// What a new level from a template holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedLevel {
    /// The template id.
    pub template: String,
    /// The new level's id.
    pub level: LevelId,
    /// Entities spawned for a built-in, or the entities the project template's file holds.
    pub entities: usize,
    /// The terrain layer file the level names (an open-world level in a project with a terrain).
    pub terrain: Option<String>,
}

/// One entity of a built-in template.
struct Part {
    name: &'static str,
    kind: &'static str,
    at: Vec3,
    /// Euler X, Y, Z in degrees (lights).
    tilt_deg: [f32; 3],
    /// Size of a `plane` (X, Z) or a `cuboid`.
    size: Vec3,
    /// Whether the part carries a static collider matching its size.
    solid: bool,
}

impl Part {
    const fn new(name: &'static str, kind: &'static str, at: Vec3) -> Self {
        Self {
            name,
            kind,
            at,
            tilt_deg: [0.0; 3],
            size: Vec3::ONE,
            solid: false,
        }
    }

    const fn sized(mut self, size: Vec3) -> Self {
        self.size = size;
        self.solid = true;
        self
    }

    const fn tilted(mut self, x: f32, y: f32, z: f32) -> Self {
        self.tilt_deg = [x, y, z];
        self
    }
}

const fn v3(x: f32, y: f32, z: f32) -> Vec3 {
    Vec3::new(x, y, z)
}

fn sun() -> Part {
    Part::new("Sun", "directional_light", v3(0.0, 10.0, 0.0)).tilted(-50.0, -30.0, 0.0)
}

fn player_start() -> Part {
    Part::new("Player Start", "player_start", v3(0.0, 1.0, 0.0))
}

fn floor(size: f32) -> Part {
    Part::new("Floor", "plane", Vec3::ZERO).sized(v3(size, 0.0, size))
}

/// The parts of a built-in template. `with_terrain` only matters for `open_world`.
fn builtin_parts(id: &str, with_terrain: bool) -> Vec<Part> {
    match id {
        "basic_lit" => vec![sun(), floor(40.0), player_start()],
        "interior" => vec![
            floor(8.0),
            Part::new("Ceiling", "cuboid", v3(0.0, 3.1, 0.0)).sized(v3(8.4, 0.2, 8.4)),
            Part::new("Wall North", "cuboid", v3(0.0, 1.5, -4.1)).sized(v3(8.4, 3.0, 0.2)),
            Part::new("Wall South", "cuboid", v3(0.0, 1.5, 4.1)).sized(v3(8.4, 3.0, 0.2)),
            Part::new("Wall East", "cuboid", v3(4.1, 1.5, 0.0)).sized(v3(0.2, 3.0, 8.0)),
            Part::new("Wall West", "cuboid", v3(-4.1, 1.5, 0.0)).sized(v3(0.2, 3.0, 8.0)),
            Part::new("Ceiling Light", "point_light", v3(0.0, 2.6, 0.0)),
            Part::new("Corner Light", "point_light", v3(2.5, 2.2, 2.5)),
            player_start(),
        ],
        "open_world" => {
            let mut parts = vec![sun()];
            if !with_terrain {
                parts.push(floor(400.0));
            }
            parts.push(player_start());
            parts
        }
        _ => Vec::new(),
    }
}

/// `*.terrain.layers.ron` files under `assets/`, sorted, first match: the terrain an open-world
/// level names. Hidden folders (`.regia`) are skipped.
#[must_use]
pub fn project_terrain(root: &Path) -> Option<LevelTerrain> {
    let assets = root.join("assets");
    let mut found = Vec::new();
    collect_files(
        &assets,
        &assets,
        0,
        8,
        &|name| name.ends_with(".terrain.layers.ron"),
        &mut found,
    );
    found.sort();
    let rel = found.into_iter().next()?;
    // The map the side files are named after: the layer file's path without `.terrain.layers.ron`
    // (`maps/hills/hills.terrain.layers.ron` names `maps/hills/hills`; a bare `terrain.layers.ron`
    // names its folder).
    let map = match rel.strip_suffix(".terrain.layers.ron") {
        Some(map) => map.to_owned(),
        None => rel
            .strip_suffix("terrain.layers.ron")
            .map(|dir| dir.trim_end_matches('/').to_owned())
            .unwrap_or_default(),
    };
    let terrain = LevelTerrain {
        layers: rel,
        map,
        ..LevelTerrain::default()
    };
    terrain.validate().ok()?;
    Some(terrain)
}

/// Files under `dir` (relative to `base`, `/`-separated) whose name satisfies `keep`.
/// Symlinks and hidden entries are skipped.
fn collect_files(
    base: &Path,
    dir: &Path,
    depth: usize,
    max_depth: usize,
    keep: &dyn Fn(&str) -> bool,
    out: &mut Vec<String>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if depth < max_depth {
                collect_files(base, &entry.path(), depth + 1, max_depth, keep, out);
            }
        } else if kind.is_file()
            && keep(&name)
            && let Ok(rel) = entry.path().strip_prefix(base)
        {
            out.push(
                rel.components()
                    .map(|part| part.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/"),
            );
        }
    }
}

/// The templates for a project: the built-ins, then each `.level.ron` under `templates/levels/`
/// (sorted by path). With no project (`root` is `None`) only the built-ins, and Open world uses
/// the large floor.
#[must_use]
pub fn list(root: Option<&Path>) -> Vec<TemplateInfo> {
    let with_terrain = root.is_some_and(|root| project_terrain(root).is_some());
    let mut out: Vec<TemplateInfo> = BUILTIN_TEMPLATES
        .iter()
        .map(|(id, label, description)| TemplateInfo {
            id: (*id).to_owned(),
            label: (*label).to_owned(),
            description: (*description).to_owned(),
            builtin: true,
            path: None,
            kinds: builtin_parts(id, with_terrain)
                .iter()
                .map(|part| part.kind.to_owned())
                .collect(),
        })
        .collect();
    let Some(root) = root else {
        return out;
    };
    let base = root.join(TEMPLATES_DIR);
    let mut files = Vec::new();
    collect_files(
        &base,
        &base,
        0,
        MAX_DEPTH,
        &|name| name.ends_with(LEVEL_SUFFIX),
        &mut files,
    );
    files.sort();
    for rel in files {
        let stem = rel.strip_suffix(LEVEL_SUFFIX).unwrap_or(&rel).to_owned();
        out.push(TemplateInfo {
            id: format!("project:{stem}"),
            label: stem.rsplit('/').next().unwrap_or(&stem).to_owned(),
            description: format!("{TEMPLATES_DIR}/{rel}"),
            builtin: false,
            path: Some(base.join(&rel)),
            kinds: Vec::new(),
        });
    }
    out
}

/// The editor's project root, when a project is open.
fn project_root_of(world: &World) -> Option<PathBuf> {
    world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|project| project.root.clone())
}

/// Starts a new level from the template `template` in `world`, unloading the open level, and
/// resets the edit state. The new level is unsaved (no path) and not dirty. The caller checks
/// unsaved edits and an open prefab first ([`new_level_guard`]). The replacement is built and
/// checked before the open level is unloaded; on any error the open level stays current.
///
/// # Errors
///
/// `level_template_unknown` (not in [`list`]), `level_template_unreadable` and
/// `level_template_invalid` (a project template's file does not read or parse, or its level does
/// not open), and `level_template_failed` (a built-in entity did not build or open).
pub fn create_level(world: &mut World, template: &str) -> Result<CreatedLevel, TemplateError> {
    let root = project_root_of(world);
    let info = list(root.as_deref())
        .into_iter()
        .find(|info| info.id == template)
        .ok_or_else(|| {
            TemplateError::new(
                "level_template_unknown",
                format!("no level template `{template}`"),
            )
        })?;
    let created = match &info.path {
        Some(path) => copy_project_template(world, &info.id, path)?,
        None => build_builtin(world, &info.id, root.as_deref())?,
    };
    level_io::reset_edit_state(world);
    world.insert_resource(level_io::OpenLevel {
        id: created.level,
        path: None,
    });
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .mark_saved();
    Ok(created)
}

/// Why a new level cannot start right now, as the agent error `editor.new_level` answers with:
/// `level_dirty` (unsaved edits and no `discard`) or `prefab_open`.
///
/// # Errors
///
/// The refusal.
pub fn new_level_guard(world: &World, discard: bool) -> Result<(), RpcError> {
    if world
        .get_resource::<crate::prefab_edit::PrefabEditMode>()
        .is_some_and(|mode| mode.editing().is_some())
    {
        return Err(RpcError::new(
            INVALID_PARAMS,
            "prefab_open",
            "a prefab is being edited",
            "close it with editor.close_prefab before starting a new level",
        ));
    }
    if !discard && world.get_resource::<UndoStack>().is_some_and(UndoStack::is_dirty) {
        return Err(RpcError::new(
            INVALID_PARAMS,
            "level_dirty",
            "the open level has unsaved edits",
            "save it with editor.save_level, or pass discard: true",
        ));
    }
    Ok(())
}

/// Reads the template file and opens a copy of it as a new level (fresh id, no path).
fn copy_project_template(
    world: &mut World,
    template: &str,
    path: &Path,
) -> Result<CreatedLevel, TemplateError> {
    let text = std::fs::read_to_string(path).map_err(|err| {
        TemplateError::new(
            "level_template_unreadable",
            format!("cannot read {}: {err}", path.display()),
        )
    })?;
    let id = LevelId(Guid::new_random());
    let level = Level::from_ron_str(id, &text).map_err(|err| {
        TemplateError::new(
            "level_template_invalid",
            format!("{} is not a level: {err}", path.display()),
        )
    })?;
    let entities = level.scene.entities.len();
    swap_in(world, level).map_err(|err| {
        TemplateError::new(
            "level_template_invalid",
            format!("{} did not open: {err}", path.display()),
        )
    })?;
    let terrain = world
        .get_resource::<LevelSettings>()
        .and_then(|settings| settings.terrain.as_ref())
        .map(|terrain| terrain.layers.clone());
    Ok(CreatedLevel {
        template: template.to_owned(),
        level: id,
        entities,
        terrain,
    })
}

/// Builds a built-in template into a fresh level.
fn build_builtin(
    world: &mut World,
    template: &str,
    root: Option<&Path>,
) -> Result<CreatedLevel, TemplateError> {
    let terrain = if template == "open_world" {
        root.and_then(project_terrain)
    } else {
        None
    };
    let parts = builtin_parts(template, terrain.is_some());
    build_from_parts(world, template, &parts, terrain)
}

/// Builds `parts` into a fresh level (the body of [`build_builtin`]; tests pass a bad part). Every
/// part is built as scene data first; only a complete level replaces the open one.
fn build_from_parts(
    world: &mut World,
    template: &str,
    parts: &[Part],
    terrain: Option<LevelTerrain>,
) -> Result<CreatedLevel, TemplateError> {
    let mut scene = SceneFile::new();
    for part in parts {
        let entity =
            part_entity(world, part).map_err(|err| TemplateError::new("level_template_failed", err))?;
        scene.entities.push(entity);
    }
    let terrain_layers = terrain.as_ref().map(|terrain| terrain.layers.clone());
    let id = LevelId(Guid::new_random());
    let level = Level {
        id,
        settings: LevelSettings {
            terrain,
            ..LevelSettings::default()
        },
        scene,
    };
    swap_in(world, level).map_err(|err| TemplateError::new("level_template_failed", err))?;
    Ok(CreatedLevel {
        template: template.to_owned(),
        level: id,
        entities: parts.len(),
        terrain: terrain_layers,
    })
}

/// Opens `level` (not in the library yet) in place of the current level, or leaves the current
/// level open when it cannot (review g17 finding 4).
///
/// The level is validated before anything changes. The engine's `Replace` open unloads the
/// current level only after that, and when the new level then fails to spawn (a prefab that nests
/// itself) it spawns the replaced level back from the library. For that put-back the library
/// briefly holds the current level as it is now (unsaved edits and all), not the copy it was
/// loaded from; the loaded copy is back in the library afterwards either way. A level that was
/// put back has new entity ids, so the undo history starts over (the dirty flag is kept). On
/// failure the new level leaves the library again.
fn swap_in(world: &mut World, level: Level) -> Result<(), String> {
    let id = level.id;
    level.scene.validate().map_err(|err| err.to_string())?;
    let previous = world
        .get_resource::<LevelManager>()
        .and_then(LevelManager::current);
    let stashed = previous.and_then(|previous| match snapshot_level(world, previous) {
        Ok(now) => {
            let mut library = world.get_resource_or_insert_with::<LevelLibrary>(LevelLibrary::new);
            let loaded = library.remove(previous);
            library.insert(now);
            Some((previous, loaded))
        }
        Err(err) => {
            tracing::warn!(target: "regia_editor", "level {previous}: no snapshot for a put-back: {err}");
            None
        }
    });
    world
        .get_resource_or_insert_with::<LevelLibrary>(LevelLibrary::new)
        .insert(level);
    level_io::preload_level_prefabs(world, &[id]);
    let opened = open_level(world, id, LoadMode::Replace);
    {
        let mut library = world.get_resource_or_insert_with::<LevelLibrary>(LevelLibrary::new);
        if let Some((previous, loaded)) = stashed {
            library.remove(previous);
            if let Some(loaded) = loaded {
                library.insert(std::sync::Arc::unwrap_or_clone(loaded));
            }
        }
        if opened.is_err() {
            library.remove(id);
        }
    }
    let Err(err) = opened else {
        return Ok(());
    };
    if previous.is_some() {
        // The validation above passed, so the open got as far as the unload: the open level was
        // spawned back, with new entity ids.
        let dirty = world.get_resource::<UndoStack>().is_some_and(UndoStack::is_dirty);
        let open = world.get_resource::<level_io::OpenLevel>().cloned();
        level_io::reset_edit_state(world);
        if dirty {
            world.resource_mut::<UndoStack>().mark_dirty();
        }
        if let Some(open) = open {
            world.insert_resource(open);
        }
    }
    Err(err.to_string())
}

/// Builds one part as a scene entity record through the reflection-backed spawn command's
/// serializer. Nothing is spawned.
fn part_entity(world: &mut World, part: &Part) -> Result<SceneEntity, String> {
    let command = part_command(world, part)?;
    command
        .file
        .entities
        .into_iter()
        .next()
        .ok_or_else(|| format!("template part `{}` built no entity", part.name))
}

/// The reflection-backed spawn command for one part (never pushed to undo: the new level's history
/// starts empty).
fn part_command(world: &mut World, part: &Part) -> Result<SpawnEntities, String> {
    use regia_physics::{Collider, ColliderShape};
    let rotation = Quat::from_euler(
        regia_core::math::EulerRot::XYZ,
        part.tilt_deg[0].to_radians(),
        part.tilt_deg[1].to_radians(),
        part.tilt_deg[2].to_radians(),
    );
    let registry = world.resource::<AppTypeRegistry>().clone();
    {
        // Register on demand, like the Create menu: a world built without the owning plugin
        // still saves the entity.
        let mut registry = registry.write();
        registry.register::<Primitive>();
        registry.register::<Collider>();
        registry.register::<regia_render::PointLight>();
        registry.register::<regia_render::DirectionalLight>();
        registry.register::<regia_framework::player::PlayerStart>();
    }
    let guid = Guid::new_random();
    let transform = Transform {
        translation: part.at,
        rotation,
        ..Transform::default()
    };
    let mut command =
        SpawnEntities::new_entity(world, guid, part.name, transform).map_err(|e| e.to_string())?;
    let add = |command: SpawnEntities, value: &dyn PartialReflect| {
        command.with_component(world, value).map_err(|e| e.to_string())
    };
    command = match part.kind {
        "directional_light" => add(command, &regia_render::DirectionalLight::default())?,
        "point_light" => add(command, &regia_render::PointLight::default())?,
        "player_start" => add(command, &regia_framework::player::PlayerStart)?,
        "plane" => add(
            command,
            &Primitive::new(PrimitiveShape::Plane {
                size: Vec2::new(part.size.x, part.size.z),
                subdivisions: 0,
            }),
        )?,
        "cuboid" => add(
            command,
            &Primitive::new(PrimitiveShape::Cuboid { size: part.size }),
        )?,
        other => return Err(format!("unknown template part kind `{other}`")),
    };
    if part.solid {
        // A plane is a surface: its collider is a slab just under it. A cuboid collides as drawn.
        let (half, offset) = if part.kind == "plane" {
            (
                Vec3::new(part.size.x / 2.0, 0.05, part.size.z / 2.0),
                Transform::from_translation(Vec3::new(0.0, -0.05, 0.0)),
            )
        } else {
            (part.size / 2.0, Transform::default())
        };
        command = add(
            command,
            &Collider {
                shape: ColliderShape::Box { half_extents: half },
                offset,
                ..Collider::default()
            },
        )?;
    }
    Ok(command)
}

// ---------------------------------------------------------------------------------------------
// The dialog (File ▸ New level)
// ---------------------------------------------------------------------------------------------

/// State of the New level dialog.
#[derive(Resource, Debug, Clone, Default)]
pub struct NewLevelDialog {
    /// Whether the dialog is showing.
    pub open: bool,
    /// The highlighted template id.
    pub selected: String,
    /// Discard unsaved edits of the open level.
    pub discard: bool,
    /// The last refusal, shown under the list.
    pub status: String,
    /// The toast the shell shows after a level was created (taken by the shell).
    pub created: Option<String>,
}

/// Opens the dialog (File ▸ New level).
pub fn open_dialog(world: &mut World) {
    let mut dialog = world.remove_resource::<NewLevelDialog>().unwrap_or_default();
    dialog.open = true;
    dialog.status.clear();
    dialog.discard = false;
    if dialog.selected.is_empty() {
        dialog.selected = "empty".to_owned();
    }
    world.insert_resource(dialog);
}

/// Consumes this frame's Up and Down presses and returns the net rows to move (Down is positive).
pub(crate) fn arrow_steps(input: &mut egui::InputState) -> i32 {
    use egui::{Key, Modifiers};
    let down = i32::try_from(input.num_presses(Key::ArrowDown)).unwrap_or(0);
    let up = i32::try_from(input.num_presses(Key::ArrowUp)).unwrap_or(0);
    input.consume_key(Modifiers::NONE, Key::ArrowDown);
    input.consume_key(Modifiers::NONE, Key::ArrowUp);
    down - up
}

/// Moves the highlight by `delta` rows, wrapping nowhere (clamped).
#[must_use]
pub fn step_selection(templates: &[TemplateInfo], selected: &str, delta: i32) -> String {
    let at = templates.iter().position(|t| t.id == selected).unwrap_or(0) as i64;
    let last = templates.len().saturating_sub(1) as i64;
    let next = (at + i64::from(delta)).clamp(0, last) as usize;
    templates.get(next).map_or_else(String::new, |t| t.id.clone())
}

/// Draws the dialog over the shell and creates the level when the user confirms. Up and Down move
/// the highlight, Enter creates, Escape closes.
pub fn dialog_window(ctx: &egui::Context, world: &mut World) {
    let Some(mut dialog) = world.remove_resource::<NewLevelDialog>() else {
        return;
    };
    if dialog.open {
        let templates = list(project_root_of(world).as_deref());
        let dirty = world.get_resource::<UndoStack>().is_some_and(UndoStack::is_dirty);
        let mut open = true;
        let mut create = false;
        let mut cancel = false;
        egui::Window::new("New level")
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.label("Start from a template. The template itself is never changed.");
                // Read the focus before taking the input lock: egui's context is one lock.
                let nothing_focused = ui.memory(|memory| memory.focused().is_none());
                let (steps, enter, escape) = ui.input_mut(|input| {
                    use egui::{Key, Modifiers};
                    (
                        arrow_steps(input),
                        nothing_focused && input.key_pressed(Key::Enter),
                        input.consume_key(Modifiers::NONE, Key::Escape),
                    )
                });
                if steps != 0 {
                    dialog.selected = step_selection(&templates, &dialog.selected, steps);
                }
                for info in &templates {
                    let response = ui.selectable_label(dialog.selected == info.id, &info.label);
                    if response.clicked() {
                        dialog.selected = info.id.clone();
                    }
                    ui.indent(&info.id, |ui| {
                        ui.weak(&info.description);
                    });
                }
                if dirty {
                    ui.checkbox(&mut dialog.discard, "Discard unsaved changes in the open level");
                }
                if !dialog.status.is_empty() {
                    ui.colored_label(ui.visuals().error_fg_color, &dialog.status);
                }
                ui.horizontal(|ui| {
                    create = ui.button("Create level").clicked() || enter;
                    cancel = ui.button("Cancel").clicked() || escape;
                });
            });
        dialog.open &= open && !cancel;
        if create {
            match new_level_guard(world, dialog.discard)
                .map_err(|e| e.message.clone())
                .and_then(|()| create_level(world, &dialog.selected).map_err(|e| e.message))
            {
                Ok(created) => {
                    dialog.status.clear();
                    dialog.open = false;
                    dialog.created = Some(format!("New level from {}", created.template));
                }
                Err(message) => dialog.status = message,
            }
        }
    }
    world.insert_resource(dialog);
}

// ---------------------------------------------------------------------------------------------
// Agent protocol
// ---------------------------------------------------------------------------------------------

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

fn info_json(info: &TemplateInfo) -> Value {
    json!({
        "id": info.id,
        "label": info.label,
        "description": info.description,
        "builtin": info.builtin,
        "path": info.path.as_ref().map(|p| p.display().to_string()),
        "kinds": info.kinds,
    })
}

/// `editor.level_templates`.
fn editor_level_templates(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let templates = list(project_root_of(world).as_deref());
    Ok(json!({ "templates": templates.iter().map(info_json).collect::<Vec<_>>() }))
}

/// `editor.new_level { template?, discard? }`.
fn editor_new_level(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let template = match params.get("template") {
        None | Some(Value::Null) => "empty",
        Some(Value::String(id)) => id.as_str(),
        Some(_) => return Err(RpcError::invalid_params("`template` must be a string")),
    };
    let discard = match params.get("discard") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => return Err(RpcError::invalid_params("`discard` must be a boolean")),
    };
    new_level_guard(world, discard)?;
    let created = create_level(world, template).map_err(|e| e.rpc())?;
    Ok(json!({
        "template": created.template,
        "level": created.level.0.to_string(),
        "entities": created.entities,
        "terrain": created.terrain,
    }))
}

/// The level-template agent methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.level_templates",
                tool: "level_templates",
                description: "The templates File ▸ New level offers: Empty, Basic lit, Interior, Open world, and every .level.ron under the project's templates/levels/. Each lists the entity kinds a level from it holds.",
                params: &[],
                result: "{ templates: [{ id, label, description, builtin, path, kinds }] }",
                mutating: false,
                permission: Permission::Read,
            },
            editor_level_templates as Handler,
        ),
        (
            MethodSchema {
                name: "editor.new_level",
                tool: "new_level",
                description: "Start a new unsaved level, as File ▸ New level does: from a template (copied, never edited) or empty. Refuses with level_dirty while the open level has unsaved edits unless discard is true, and with prefab_open while a prefab is open.",
                params: &[
                    ParamSchema {
                        name: "template",
                        kind: "string",
                        required: false,
                        description: "A template id from editor.level_templates (default `empty`).",
                    },
                    ParamSchema {
                        name: "discard",
                        kind: "boolean",
                        required: false,
                        description: "Drop unsaved edits of the open level (default false).",
                    },
                ],
                result: "{ template, level, entities, terrain }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_new_level as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use regia_app::prelude::*;

    use super::*;
    use crate::command::EditorCommand;

    fn names(world: &mut World) -> Vec<String> {
        let mut names: Vec<String> = world
            .query::<&regia_scene::prelude::Name>()
            .iter(world)
            .map(|n| n.as_str().to_owned())
            .collect();
        names.sort();
        names
    }

    /// Review g17 finding 4: a built-in part that fails to build refuses the call before the
    /// open level is touched. The level stays current with the entity spawned into it after it
    /// opened, and nothing of the half-built template is left in the world.
    #[test]
    fn a_part_that_fails_leaves_the_open_level_current() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_scene::ScenePlugin);
        app.init_resource::<UndoStack>();
        let open = level_io::new_level(app.world_mut());
        let level = open.id;
        app.world_mut().insert_resource(open);
        app.update();
        let world = app.world_mut();
        let keep = SpawnEntities::new_entity(world, Guid::new_random(), "Keep", Transform::default());
        let Ok(mut keep) = keep else {
            panic!("the Keep entity builds");
        };
        keep.apply(world);
        let before = names(world);
        assert_eq!(before, ["Keep"]);

        let parts = [
            sun(),
            Part::new("Broken", "no_such_kind", Vec3::ZERO),
            player_start(),
        ];
        let Err(err) = build_from_parts(world, "basic_lit", &parts, None) else {
            panic!("an unknown part kind is refused");
        };
        assert_eq!(err.code, "level_template_failed", "{}", err.message);
        assert_eq!(
            world.resource::<LevelManager>().current(),
            Some(level),
            "the open level is still current"
        );
        assert_eq!(world.resource::<level_io::OpenLevel>().id, level);
        assert_eq!(names(world), before, "no Sun, no half-built level");
        assert!(world.resource::<LevelLibrary>().contains(level));
    }
}
