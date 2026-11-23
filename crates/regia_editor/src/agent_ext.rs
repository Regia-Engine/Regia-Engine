// SPDX-License-Identifier: MPL-2.0
//! The editor's agent-protocol surface: selection, gizmo transforms and undo/redo, exposed as
//! an extension of the shared method table (`specs/19` § 1, `specs/23` § 3.2).
//!
//! The handlers run on the edited world inside dispatch's exclusive context, exactly like the
//! built-in `scene.*` tools — same permissions, audit log and provenance. Register with
//! [`install`] before the first frame.

use serde_json::{Value, json};

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::ecs::entity::Entity;
use regia_ecs::prelude::*;

use crate::command::EditorCommand as _;
use crate::command::{SetTransform, UndoStack};
use crate::level_io::{self, OpenLevel};
use crate::selection::{SelectMode, Selection};
use crate::viewport::{GizmoMode, GizmoState};

/// Installs the editor methods on the world's [`regia_agent::extension::AgentExtension`]
/// resource (creating it when absent).
pub fn install(world: &mut World) {
    world.insert_resource(regia_agent::server::RequestLifecycle {
        idle: crate::undo_agent::request_idle,
        before: crate::undo_agent::transaction_guard,
    });
    world.insert_resource(regia_agent::SceneEditHook {
        edit: crate::undo_agent::scene_edit,
    });
    crate::mesh_source::register_level_types(world);
    crate::blockout::register(world);
    crate::activity_panel::install(world);
    // `procgen.detach` writes generated meshes as glTF assets (spec 62 § 7.3).
    world.insert_resource(regia_procgen::world::ProcMeshWriter(std::sync::Arc::new(
        |mesh, name, path| {
            let bytes = regia_asset_pipeline::generated_mesh_export::generated_mesh_glb(name, mesh)
                .map_err(|e| e.to_string())?;
            regia_procgen::pgout::write_atomic(path, &bytes).map_err(|e| e.to_string())
        },
    )));
    let mut extension = world
        .get_resource::<regia_agent::extension::AgentExtension>()
        .cloned()
        .unwrap_or_default();
    for (method, handler) in all_methods() {
        extension.register(method, handler);
    }
    crate::dcc_feed::register(&mut extension);
    for (method, handler) in crate::release_agent::methods() {
        extension.register(method, handler);
    }
    // `asset.export_glb` (ADR-2554): needs the asset pipeline, which runtime-safe regia_agent cannot link.
    let (method, handler) = crate::export_glb::agent_method();
    extension.register(method, handler);
    // `asset.gltf_extensions` (ADR-3016): the same reason.
    let (method, handler) = crate::gltf_ext_agent::agent_method();
    extension.register(method, handler);
    let (method, handler) = crate::gltf_lint_agent::agent_method();
    extension.register(method, handler);
    extension.add_capability(
        "gltf_lint",
        "asset.gltf_lint reports source geometry and rig channel warnings; see book/agents/gltf-lint.md.",
    );
    for (method, handler) in crate::gltf_material_lint_agent::methods() {
        extension.register(method, handler);
    }
    extension.add_capability(
        "gltf_material_lint",
        "Source-material check/fix. Default profile standard preserves valid glTF appearance; compatibility changes it. asset.gltf_material_lint and asset.gltf_material_fix; see book/agents/gltf-material-lint.md.",
    );
    for (method, handler) in crate::content_lint_agent::methods() {
        extension.register(method, handler);
    }
    for (method, handler) in crate::content_lint_jobs::agent_methods() {
        extension.register(method, handler);
    }
    extension.add_capability(
        "content_lint",
        "Source-lint discovery and jobs: asset.content_lint_rules, asset.content_lint_policy, asset.content_lint_start, asset.content_lint_cancel, asset.content_lint_status, asset.content_lint_read, asset.content_lint_thresholds, asset.content_lint_fix_preview, and asset.content_lint_apply_fix. Parses off the editor thread. See book/agents/content-lint.md.",
    );
    extension.add_capability(
        "package_hygiene_editor",
        "Project hygiene policy and audit review: editor.package_hygiene_get, editor.package_hygiene_set, editor.package_hygiene_audit, editor.package_hygiene_audit_status and editor.package_hygiene_audit_cancel; see book/agents/package-hygiene.md.",
    );
    for (method, handler) in crate::tools::agent::deferred_methods() {
        extension.register_deferred(method, handler);
    }
    for (method, param, shape) in crate::tools::agent::param_shapes() {
        extension.register_param_shape(method, param, shape);
    }
    extension.add_capability(
        "editor_tools",
        "The editor hosts external tools: editor.tool_* methods, <id>.tool.toml manifests, tool.<id>.<name> provided methods.",
    );
    extension.register_param_shape(
        "editor.frame_capture",
        "action",
        json!({"type":"string", "enum":["start", "state", "resume"]}),
    );
    // Pass-boundary copies of a capture (ADR-7860).
    extension.register_param_shape(
        "editor.frame_capture",
        "passes",
        json!({"type":"array", "items":{"type":"string"}}),
    );
    extension.register_param_shape(
        "editor.frame_capture",
        "budget_bytes",
        json!({"type":"integer", "minimum":0}),
    );
    extension.register_param_shape(
        "editor.frame_pass",
        "index",
        json!({"type":"integer", "minimum":0}),
    );
    // The capture source (ADR-8224).
    extension.register_param_shape(
        "editor.frame_capture",
        "source",
        json!({"type":"string", "enum":["viewport", "play_window"]}),
    );
    // Draw records of a capture (ADR-8224).
    extension.register_param_shape(
        "editor.frame_draw",
        "index",
        json!({"type":"integer", "minimum":0}),
    );
    extension.register_param_shape(
        "editor.frame_draw",
        "step",
        json!({"type":"string", "enum":["next", "previous"]}),
    );
    world.insert_resource(extension);
    // `agent.open_in_editor` reaches the editor through this hook (ADR-2346).
    world.insert_resource(regia_agent::open_in_editor::OpenInEditorHook(
        crate::agent_changes::open_target,
    ));
    // `agent.hot_reload` reaches the editor through this hook (ADR-5707).
    world.insert_resource(regia_agent::hot_reload::HotReloadHook(
        crate::code_reload::agent_hot_reload,
    ));
    // `import.materialx_report` runs the pipeline's MaterialX importer (ADR-3341).
    world.insert_resource(regia_agent::materialx_report::MaterialXReporter(
        regia_asset_pipeline::materialx_import::agent_report,
    ));
    crate::procgen::install(world);
    if !world.contains_resource::<crate::tools::ToolHost>() {
        world.insert_resource(crate::tools::ToolHost::new());
    }
}

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

/// Every editor method with its handler.
#[must_use]
pub fn all_methods() -> Vec<(MethodSchema, Handler)> {
    let mut methods = vec![
        (editor_select_schema(), editor_select as Handler),
        // Select by asset (spec 19 § 3): the Content Browser's Select in level.
        (editor_select_by_asset_schema(), editor_select_by_asset as Handler),
        (
            MethodSchema {
                name: "editor.get_selection",
                tool: "get_selection",
                description: "The editor's current selection.",
                params: &[],
                result: "{ entities: [{entity, name}], primary }",
                mutating: false,
                permission: Permission::Read,
            },
            editor_get_selection as Handler,
        ),
        (
            MethodSchema {
                name: "editor.set_gizmo",
                tool: "set_gizmo",
                description: "Set the viewport gizmo mode, snapping and reference space.",
                params: &[
                    ParamSchema {
                        name: "mode",
                        kind: "string",
                        required: false,
                        description: "\"translate\" | \"rotate\" | \"scale\" | \"select\" (no gizmo).",
                    },
                    ParamSchema {
                        name: "snap",
                        kind: "boolean",
                        required: false,
                        description: "Snap to grid/angle/scale steps.",
                    },
                    ParamSchema {
                        name: "space",
                        kind: "string",
                        required: false,
                        description: "The gizmo's axes: \"world\" | \"local\" | \"parent\" | \"view\" (ADR-7756).",
                    },
                ],
                result: "{ mode, snap, space }",
                mutating: false,
                permission: Permission::Read,
            },
            editor_set_gizmo as Handler,
        ),
        (
            MethodSchema {
                name: "editor.set_transform",
                tool: "set_transform",
                description: "Set an entity's Transform (the gizmo's edit), undoable. Pass \
                    `entities` (an array of ids) to set the same channels on every entity: an \
                    omitted channel keeps that entity's own value, and one undo restores all of \
                    them. `entity` and `entities` together are entity_and_entities.",
                params: &[
                    ParamSchema {
                        name: "entity",
                        kind: "number",
                        required: false,
                        description: "Entity id (from scene.query_entities). Omit when `entities` is set.",
                    },
                    ParamSchema {
                        name: "entities",
                        kind: "array",
                        required: false,
                        description: "Entity ids. One compound undo. Empty is entities_empty.",
                    },
                    ParamSchema {
                        name: "translation",
                        kind: "array",
                        required: false,
                        description: "[x, y, z].",
                    },
                    ParamSchema {
                        name: "rotation",
                        kind: "array",
                        required: false,
                        description: "[x, y, z, w] quaternion.",
                    },
                    ParamSchema {
                        name: "scale",
                        kind: "array",
                        required: false,
                        description: "[x, y, z].",
                    },
                ],
                result: "{ entity, translation, rotation, scale, undoable }; with entities also \
                    { entities, skipped }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_set_transform as Handler,
        ),
        (
            MethodSchema {
                name: "editor.set_property",
                tool: "set_property",
                description: "Set a reflected field of a component through the inspector's undoable \
                    edit path (a SetProperty on the undo stack). Paths use reflect syntax and reach \
                    into nested structs, lists, arrays, tuples and enum variants: \
                    \"waypoints[1].speed\", \"mode.0.radius\" (Option/tuple variant field), \
                    \"slots[2]\". Writing a whole list/map/set replaces it exactly (shorter lists \
                    shrink). Pass `entities` to write the field on every selected entity that has \
                    the component (one undo). Vector lanes and color channels are per axis; \
                    rotation.rx/ry/rz are Euler degrees and keep each entity's other axes. \
                    `entity` and `entities` together are entity_and_entities.",
                params: &[
                    ParamSchema {
                        name: "entity",
                        kind: "number",
                        required: false,
                        description: "Entity id (from scene.query_entities). Omit when `entities` is set.",
                    },
                    ParamSchema {
                        name: "entities",
                        kind: "array",
                        required: false,
                        description: "Entity ids. One compound undo. Empty is entities_empty; \
                            not an array is entities_not_array.",
                    },
                    ParamSchema {
                        name: "component",
                        kind: "string",
                        required: true,
                        description: "Component type name, short or full (\"Transform\").",
                    },
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: false,
                        description: "Reflect path inside the component; \"\" or absent = the whole component.",
                    },
                    ParamSchema {
                        name: "value",
                        kind: "any",
                        required: false,
                        description: "The new value in the reflect JSON form scene.get_entity returns. \
                            Required unless `expr` is given.",
                    },
                    ParamSchema {
                        name: "expr",
                        kind: "string",
                        required: false,
                        description: "Instead of `value`, on a number field (one entity): an Inspector \
                            expression evaluated against the current value in the unit the Inspector \
                            shows: \"2*pi\", \"+=10\", \"*2\", \"5cm\", \"90deg\" \
                            (book/agents/inspector.md). Errors: value_and_expr, expr_not_number, \
                            expr_with_entities and the expr_* codes.",
                    },
                    ParamSchema {
                        name: "dry_run",
                        kind: "boolean",
                        required: false,
                        description: "Validate and preview without applying.",
                    },
                ],
                result: "{ entity, component, path, before, after, applied, undoable }; with \
                    entities also { entities, skipped, befores }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_set_property as Handler,
        ),
        (
            MethodSchema {
                name: "editor.undo",
                tool: "undo",
                description: "Undo the last editor command.",
                params: &[],
                result: "{ undone, remaining, description, dirty }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_undo as Handler,
        ),
        (
            MethodSchema {
                name: "editor.redo",
                tool: "redo",
                description: "Redo the last undone editor command.",
                params: &[],
                result: "{ redone, remaining, description, dirty }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_redo as Handler,
        ),
        (
            MethodSchema {
                name: "editor.save_level",
                tool: "save_level",
                description: "Save the open level to disk.",
                params: &[ParamSchema {
                    name: "path",
                    kind: "string",
                    required: false,
                    description: "Project-relative path; default: the level's last path.",
                }],
                result: "{ path, bytes, terrain_tiles_recooked, terrain_holes_file, terrain_paint_file, tilemap_saved, terrain_scatter_file, terrain_grass_density_file }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_save_level as Handler,
        ),
        (
            MethodSchema {
                name: "editor.play",
                tool: "play",
                description: "Start play-in-editor: a copy of the edit world simulated in the \
                    editor (specs/19 § 4). `mode: simulate` runs physics, AI and scripts with no \
                    local player spawned or possessed (the editor camera stays free); `players` \
                    joins that many local players for split-screen. The request is applied on the \
                    next editor frame; poll editor.play_state for the result.",
                params: &[
                    ParamSchema {
                        name: "mode",
                        kind: "string",
                        required: false,
                        description: "\"play\" (default) or \"simulate\"; another value is `play_mode_unknown`.",
                    },
                    ParamSchema {
                        name: "window",
                        kind: "boolean",
                        required: false,
                        description: "Play in a separate window (the game camera in a second OS window; closing it \
                            stops play). With 2 or more `players` the window composites one split-screen pane per \
                            player. A host with no window system (headless, CI) answers `play_window_unavailable`.",
                    },
                    ParamSchema {
                        name: "players",
                        kind: "number",
                        required: false,
                        description: "Local players, 1..=8 (default 1: the game joins its own). 2 or more joins \
                            that many through PlayerManager, one split-screen viewport each: the play window and a \
                            possessed viewport composite one pane per player (a game without cameras gets a stand-in \
                            camera each); ignored by simulate. Out of range is `play_players_out_of_range`.",
                    },
                    ParamSchema {
                        name: "preset",
                        kind: "string",
                        required: false,
                        description: "The play window's resolution for this session (with `window: true`): `window` \
                            (the window's size), `hd_720`, `hd_1080`, `phone_9_16_portrait`, `phone_9_19_5_portrait`, \
                            `phone_9_19_5_landscape`, `tablet_3_4_portrait` or `tablet_3_4_landscape`. The picture is \
                            scaled to fit the window with bars; clicks map back to play pixels. Default: the \
                            `play.resolution` preference. Unknown is `play_resolution_invalid`; without `window: true` \
                            it is `play_resolution_needs_window`.",
                    },
                    ParamSchema {
                        name: "size",
                        kind: "array",
                        required: false,
                        description: "A custom play resolution `[width, height]`, 16..=16384 each (with `window: true`; \
                            not with `preset`). Otherwise as `preset`.",
                    },
                    ParamSchema {
                        name: "separate_processes",
                        kind: "boolean",
                        required: false,
                        description: "Set the Net tab's separate-processes option first: with a \
                            listen or dedicated mode the server and clients run as processes \
                            (editor.pie_processes).",
                    },
                    ParamSchema {
                        name: "start",
                        kind: "string",
                        required: false,
                        description: "Where the player starts (ADR-7829): `player_start` (the game's spawn rules \
                            pick a level player start), `camera` (the player spawns at the editor camera's pose; \
                            a play world with no player gets its cameras placed there) or `entity` (one player \
                            start, by `entity`). Default: the Play dropdown's choice. Unknown is \
                            `play_start_unknown`; a guid that is not a player start is `play_start_not_player_start`.",
                    },
                    ParamSchema {
                        name: "entity",
                        kind: "string",
                        required: false,
                        description: "The player start's entity guid, with `start: \"entity\"`.",
                    },
                    ParamSchema {
                        name: "time_scale",
                        kind: "number",
                        required: false,
                        description: "The session's starting gameplay time scale, 0.1..=4 (default 1; pause is \
                            editor.pause). Out of range is `play_time_scale_out_of_range`. Change it while \
                            playing with editor.play_controls.",
                    },
                    ParamSchema {
                        name: "mute",
                        kind: "boolean",
                        required: false,
                        description: "Start with the play world's audio muted (default false; the editor's own \
                            sounds are not affected).",
                    },
                    ParamSchema {
                        name: "device",
                        kind: "object",
                        required: false,
                        description: "Device simulation in the play window (with `window: true`; ADR-7829): \
                            `{ preset: \"phone\"|\"handheld\"|null, landscape, touch, gamepad, safe_area_outline }`. A preset renders at \
                            that device's screen and gives the game its safe-area insets (the UI designer's device \
                            data); `landscape` turns it; `touch` makes left drags single touches (Ctrl at the press \
                            adds a mirrored second finger for pinch); `gamepad` puts a virtual pad over the picture \
                            whose buttons the mouse or keyboard presses. `safe_area_outline` shows the safe-area boundary. `null` turns it off. Unknown preset: \
                            `play_device_preset_unknown`; without `window: true`: `play_device_needs_window`.",
                    },
                ],
                result: "{ queued: true, mode, players, window: \"play_window\"|\"viewport\", \
                    resolution: { preset, size: [w, h]|null }, start, time_scale, mute, hint }",
                mutating: true,
                permission: Permission::Play,
            },
            editor_play as Handler,
        ),
        (
            MethodSchema {
                name: "editor.pause",
                tool: "pause",
                description: "Pause the running play-in-editor session.",
                params: &[],
                result: "{ queued: true }",
                mutating: true,
                permission: Permission::Play,
            },
            editor_pause as Handler,
        ),
        (
            MethodSchema {
                name: "editor.resume",
                tool: "resume",
                description: "Resume the paused play-in-editor session.",
                params: &[],
                result: "{ queued: true }",
                mutating: true,
                permission: Permission::Play,
            },
            editor_resume as Handler,
        ),
        (
            MethodSchema {
                name: "editor.stop",
                tool: "stop",
                description: "Stop play-in-editor. The edit world is restored exactly (it was \
                    never touched); play-world edits are discarded.",
                params: &[],
                result: "{ queued: true }",
                mutating: true,
                permission: Permission::Play,
            },
            editor_stop as Handler,
        ),
        (
            MethodSchema {
                name: "editor.step",
                tool: "step",
                description: "Advance the paused play-in-editor session by `frames` frames.",
                params: &[ParamSchema {
                    name: "frames",
                    kind: "number",
                    required: false,
                    description: "Whole frames to simulate while paused, 1..10000 (default 1); other values are `invalid_params`.",
                }],
                result: "{ queued: true }",
                mutating: true,
                permission: Permission::Play,
            },
            editor_step as Handler,
        ),
        (
            MethodSchema {
                name: "editor.play_state",
                tool: "play_state",
                description: "The play-in-editor state: mode, frames, entity count, start time \
                    and the game crate (when one is open).",
                params: &[],
                result: "{ mode: \"playing\"|\"paused\"|\"stopped\", kind: \"play\"|\"simulate\"|null, frames, entities, \
                    start_elapsed_ms, game?, panic?, panic_backtrace?, window, players, viewports: [{ player, rect }], \
                    kept?, play_window: { open, title, size: [w, h], position: [x, y]|null, captured, error, \
                    resolution: { preset, size: [w, h]|null }, render_size: [w, h], picture: [x, y, w, h], \
                    players, panes: [{ player, rect: [x, y, w, h] }], \
                    counters: { windows_opened, windows_closed, surface_configures, surface_recreations, \
                    frames_presented, frames_skipped } } }",
                mutating: false,
                permission: Permission::Read,
            },
            editor_play_state as Handler,
        ),
        (
            MethodSchema {
                name: "editor.static_move_warnings",
                tool: "static_move_warnings",
                description: "How many times Static entities moved at runtime since play \
                    started (specs/14 A1): the marker promises a fixed transform, and the play \
                    world counts every broken promise (one log warning per entity). Remove the \
                    Static marker or stop moving the entity.",
                params: &[],
                result: "{ count, note }",
                mutating: false,
                permission: Permission::Read,
            },
            editor_static_move_warnings as Handler,
        ),
        (
            MethodSchema {
                name: "editor.keep_simulation_changes",
                tool: "keep_simulation_changes",
                description: "Copy the play-world state of the selected entities (or of `guids`) back to \
                    the edit world as one undo step (specs/19 § 4): every reflected component the edit \
                    entity has whose play value differs. Applied on the next editor frame; \
                    editor.play_state's `kept` reports what was copied. `not_playing` without a session.",
                params: &[ParamSchema {
                    name: "guids",
                    kind: "array",
                    required: false,
                    description: "Entity guids to keep (default: the play world's selection).",
                }],
                result: "{ queued: true, hint }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_keep_simulation_changes as Handler,
        ),
        (
            MethodSchema {
                name: "editor.import",
                tool: "import",
                description: "Import source files into assets/ and cook them into the dev cache \
                    (specs/19 § 2). glTF/GLB and PNG cook through the pipeline; provenance \
                    records the client (specs/23 § 5).",
                params: &[
                    ParamSchema {
                        name: "sources",
                        kind: "array",
                        required: true,
                        description: "Absolute paths of the files to import.",
                    },
                    ParamSchema {
                        name: "folder",
                        kind: "string",
                        required: false,
                        description: "Destination folder under assets/ (default: assets/).",
                    },
                    ParamSchema {
                        name: "async",
                        kind: "boolean",
                        required: false,
                        description: "Run the pipeline import as a background job and answer `{ job }` at once \
                            (default false). editor.cancel_job stops it and removes what it had copied.",
                    },
                ],
                result: "{ imported: [paths], cooked, skipped, errors } | { job } with async",
                mutating: true,
                permission: Permission::Assets,
            },
            editor_import as Handler,
        ),
        (
            MethodSchema {
                name: "editor.create_asset",
                tool: "create_asset",
                description: "Create a project asset, as the Content Browser's New menu does: a default \
                    material, a prefab with one named root, an empty level, an anim graph playing one \
                    looping clip, a sound cue with no variations, a UI layout with a full-size column \
                    root, a data asset holding a type's default value, an empty material graph, a \
                    pass-through material function, a sequence with no tracks, a shot list of one \
                    still shot, a material instance of a parent graph with no overrides, a blank procedural \
                    graph, a VFX system of one emitter, an empty conversation, curve table, chooser or data \
                    table, or a folder (editor.asset_kinds lists them).",
                params: &[
                    ParamSchema {
                        name: "kind",
                        kind: "string",
                        required: true,
                        description: "\"material\" | \"prefab\" | \"level\" | \"anim_graph\" | \
                            \"sound_cue\" | \"ui_layout\" | \"data_asset\" | \"material_graph\" | \
                            \"material_function\" | \"sequence\" | \"shot_list\" | \"attribute_set\" | \
                            \"effect\" | \"ability\" | \"material_instance\" | \"procgen_graph\" | \
                            \"vfx_system\" | \"conversation\" | \"curve_table\" | \"chooser\" | \
                            \"data_table\" | \"montage\" | \"cloth\" | \"soft_body\" | \"camera\" | \"smart_object\" | \"tilemap\" | \
                            \"motion_database\" | \"biome\" | \"logic_graph\" | \"folder\".",
                    },
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: true,
                        description: "Project-relative path under assets/, with the kind's extension \
                            (.material.ron, .prefab.ron, .level.ron, .animgraph.ron, .cue.ron, .ui.ron, .data.ron, \
                            .matgraph.ron, .matfunc.ron, .seq.ron, .shots.ron, .matinst.ron, .pgraph.ron, .vfx.ron, \
                            .conv.ron, .curves.ron, .chooser.ron, .table.ron).",
                    },
                    ParamSchema {
                        name: "type",
                        kind: "string",
                        required: false,
                        description: "data_asset: the registered type path (see editor.data_asset_types); \
                            data_table: the row struct's type path written as `row_type` (optional).",
                    },
                    ParamSchema {
                        name: "parent",
                        kind: "string",
                        required: false,
                        description: "material_instance only: the parent graph's project-relative \
                            .matgraph.ron path (must exist).",
                    },
                ],
                result: "{ path }",
                mutating: true,
                permission: Permission::Assets,
            },
            editor_create_asset as Handler,
        ),
        (
            MethodSchema {
                name: "editor.rename_asset",
                tool: "rename_asset",
                description: "Move/rename a project asset in the content browser (specs/12 § 9, \
                    M11): the source and its `.meta` move together (the asset id never changes), \
                    files that reference the old path are rewritten, and the move is one undo \
                    step (editor.undo).",
                params: &[
                    ParamSchema {
                        name: "from",
                        kind: "string",
                        required: true,
                        description: "Project-relative path under assets/ of the existing asset.",
                    },
                    ParamSchema {
                        name: "to",
                        kind: "string",
                        required: true,
                        description: "New project-relative path under assets/ (must not exist).",
                    },
                ],
                result: "{ from, to, fixed_referencers: [paths] }",
                mutating: true,
                permission: Permission::Assets,
            },
            editor_rename_asset as Handler,
        ),
        (
            MethodSchema {
                name: "editor.list_assets",
                tool: "list_assets",
                description: "The project's asset files (the content browser's scan).",
                params: &[
                    ParamSchema {
                        name: "folder",
                        kind: "string",
                        required: false,
                        description: "Only this folder under assets/ (default: everywhere).",
                    },
                    ParamSchema {
                        name: "search",
                        kind: "string",
                        required: false,
                        description: "Search query: words (any case), -word, type:<name>, ext:<suffix>.",
                    },
                ],
                result: "{ assets: [{ path, kind }], count }",
                mutating: false,
                permission: Permission::Read,
            },
            editor_list_assets as Handler,
        ),
        (
            MethodSchema {
                name: "editor.spawn_prefab",
                tool: "spawn_prefab",
                description: "Spawn an instance of a .prefab.ron from the project's assets into \
                    the open level (undoable).",
                params: &[
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: true,
                        description: "Project-relative prefab path, e.g. assets/rock.prefab.ron.",
                    },
                    ParamSchema {
                        name: "translation",
                        kind: "array",
                        required: false,
                        description: "[x, y, z] for the instance root (default: origin).",
                    },
                ],
                result: "{ path, entity }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_spawn_prefab as Handler,
        ),
        (
            MethodSchema {
                name: "editor.open_prefab",
                tool: "open_prefab",
                description: "Enter prefab edit mode: the edit world switches to the prefab's \
                    isolated scene (the open level is stashed; the undo history is cleared).",
                params: &[ParamSchema {
                    name: "path",
                    kind: "string",
                    required: true,
                    description: "Project-relative .prefab.ron to edit.",
                }],
                result: "{ editing }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_open_prefab as Handler,
        ),
        (
            MethodSchema {
                name: "editor.save_prefab",
                tool: "save_prefab",
                description: "Save the edited prefab: write the file, re-bake its template and \
                    re-instantiate every live instance of it.",
                params: &[],
                result: "{ saved, instances_updated }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_save_prefab as Handler,
        ),
        (
            MethodSchema {
                name: "editor.close_prefab",
                tool: "close_prefab",
                description: "Leave prefab edit mode and restore the stashed level.",
                params: &[ParamSchema {
                    name: "save",
                    kind: "boolean",
                    required: false,
                    description: "Save before closing (default true).",
                }],
                result: "{ closed }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_close_prefab as Handler,
        ),
        (
            MethodSchema {
                name: "editor.prefab_mode",
                tool: "prefab_mode",
                description: "Which prefab is being edited, when the editor is in prefab edit \
                    mode, whether it has unsaved edits (`dirty`), and whether it was opened in place \
                    with the level drawn around it (`in_context`, editor.prefab_edit_in_place).",
                params: &[],
                result: "{ editing: path?, dirty, in_context }",
                mutating: false,
                permission: Permission::Read,
            },
            editor_prefab_mode as Handler,
        ),
        (
            MethodSchema {
                name: "editor.new_project",
                tool: "new_project",
                description: "Create a Blank project (the same scaffolding as `regia new`) and open it.",
                params: &[
                    ParamSchema {
                        name: "name",
                        kind: "string",
                        required: true,
                        description: "Display name. The folder and crate name are its snake_case form.",
                    },
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: false,
                        description: "Parent directory (default: the current directory).",
                    },
                    ParamSchema {
                        name: "template",
                        kind: "string",
                        required: false,
                        description: "Template id: `blank`, `first_person`, `third_person`, or \
                                     `top_down` (same set as `regia new` minus `quick`).",
                    },
                ],
                result: "{ root, warning }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_new_project as Handler,
        ),
        (
            MethodSchema {
                name: "editor.open_project",
                tool: "open_project",
                description: "Open a Regia Engine project directory and its startup level, when one exists.",
                params: &[ParamSchema {
                    name: "path",
                    kind: "string",
                    required: true,
                    description: "Project directory (contains Regia.toml).",
                }],
                result: "{ root, warning }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_open_project as Handler,
        ),
        (
            MethodSchema {
                name: "editor.recent_projects",
                tool: "recent_projects",
                description: "Project directories opened most recently that still contain Regia.toml, with the browser's tags, last-opened time, engine version and thumbnail; filtered by a search query (`tag:name` terms match tags) and a tag.",
                params: &[
                    ParamSchema {
                        name: "query",
                        kind: "string",
                        required: false,
                        description: "Search words (name or path) and `tag:name` terms.",
                    },
                    ParamSchema {
                        name: "tag",
                        kind: "string",
                        required: false,
                        description: "Only projects holding this tag.",
                    },
                ],
                result: "{ projects: [paths], entries: [{ path, name, tags, last_opened, engine_version, thumbnail }], tags: [every tag in use] }",
                mutating: false,
                permission: Permission::Read,
            },
            editor_recent_projects as Handler,
        ),
        (
            MethodSchema {
                name: "editor.create_script",
                tool: "create_script",
                description: "Create game/src/scripts/<name>.rs from a template and register it at the \
                    `// REGIA:SCRIPTS` marker, as the New script wizard's Create does (specs/19 § 5).",
                params: &[
                    ParamSchema {
                        name: "name",
                        kind: "string",
                        required: true,
                        description: "Script name. It becomes a snake_case module.",
                    },
                    ParamSchema {
                        name: "template",
                        kind: "string",
                        required: false,
                        description: "\"empty\" (default) or \"component\" (a reflected component struct).",
                    },
                ],
                result: "{ path }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_create_script as Handler,
        ),
        (
            MethodSchema {
                name: "editor.edit_material",
                tool: "edit_material",
                description: "Edit a .material.ron's base colour, metallic, roughness, emissive colour and \
                    alpha mode, then save it (the material editor's write path).",
                params: &[
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: true,
                        description: "Project-relative .material.ron.",
                    },
                    ParamSchema {
                        name: "base_color",
                        kind: "array",
                        required: false,
                        description: "Linear RGBA, up to 4 numbers. Omitted channels stay.",
                    },
                    ParamSchema {
                        name: "metallic",
                        kind: "number",
                        required: false,
                        description: "Metallic factor.",
                    },
                    ParamSchema {
                        name: "roughness",
                        kind: "number",
                        required: false,
                        description: "Perceptual roughness.",
                    },
                    ParamSchema {
                        name: "emissive",
                        kind: "array",
                        required: false,
                        description: "Linear RGB emissive colour, up to 3 numbers in 0..=64 (the panel's \
                            colour times its brightness boost). Omitted channels stay.",
                    },
                    ParamSchema {
                        name: "alpha_mode",
                        kind: "string",
                        required: false,
                        description: "\"opaque\", \"mask\" or \"blend\" (the panel's Render state).",
                    },
                ],
                result: "{ path, base_color, metallic, roughness, emissive, alpha_mode }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_edit_material as Handler,
        ),
    ];
    methods.extend(crate::viewport_agent::methods());
    methods.extend(crate::theme::methods());
    // Independent viewports (`editor.viewport_instance*`, `editor.viewport_link`).
    methods.extend(crate::viewport_instance_agent::methods());
    // Asset validators and the dependency graph (`editor.asset_validate`, `editor.asset_graph`).
    methods.extend(crate::asset_validate_agent::methods());
    // The Problems, Reference graph and Size map panels (spec 32 § 8).
    methods.extend(crate::asset_panels_agent::methods());
    // The Layers panel and placed-level editing (spec 37).
    methods.extend(crate::layers_panel::methods());
    methods.extend(crate::placed_create::methods());
    // The Tests panel (spec 41 § 6).
    methods.extend(crate::tests_panel::methods());
    // The Agent Changes panel (specs/23 § 5, ADR-2346).
    methods.extend(crate::agent_changes::methods());
    methods.extend(crate::scalar_overlay_agent::methods());
    // The Theme panel (`editor.theme_panel_*`).
    methods.extend(crate::theme_panel_agent::methods());
    // Project Settings -> XR (`editor.xr_settings`).
    methods.extend(crate::xr_settings::methods());
    // The XR simulator in play (`editor.xr_simulator`).
    methods.extend(crate::xr_simulator::methods());
    // The Terrain panel's scatter brush class picker (`editor.terrain_scatter_brush`).
    methods.extend(crate::scatter_brush::methods());
    // The Shots panel (`editor.shot_panel_*`).
    methods.extend(crate::shot_panel::methods());
    // The Content Chunks panel (`editor.content_chunks_*`, spec 40 § 6).
    methods.extend(crate::chunks_panel::methods());
    methods.extend(crate::closure_settings::methods());
    // Play-in-editor with several clients and the Net panel (`editor.pie_net_*`).
    methods.extend(crate::pie_net::methods());
    // The Sequence Curves panel (`editor.seq_curve_*`, ADR-1880).
    methods.extend(crate::seq_curve_panel::methods());
    // Its section strip (`editor.seq_sections_state`, `editor.seq_section_edit`, ADR-2135).
    methods.extend(crate::seq_sections::methods());
    // Its authoring rows (`editor.seq_track_edit`, `editor.seq_binding_edit`, `editor.seq_new`,
    // `editor.seq_key`, `editor.seq_preview`, ADR-9710).
    methods.extend(crate::seq_tracks::methods());
    // Its timeline (`editor.seq_keys`, `editor.seq_marker_edit`, `editor.seq_cut_edit`,
    // `editor.seq_transport`, `editor.seq_timeline_state`, ADR-7322).
    methods.extend(crate::seq_timeline::methods());

    methods.extend(crate::model_report::methods());
    // The kit viewer (`editor.open_kit`).
    methods.extend(crate::kit_view::methods());
    // The Bake runner panel (`editor.bake_gi`, `editor.bake_status`, `editor.bake_gi_cancel`).
    methods.extend(crate::bake_runner::methods());
    // The command palette (`editor.command`).
    methods.extend(crate::command_palette::methods());
    // The UI action registry and allowlist (`editor.actions`, ADR-7307).
    methods.extend(crate::agent_parity::methods());
    // The Terrain panel's scatter class edits (`editor.scatter_class`, ADR-7307).
    methods.extend(crate::terrain_panel::methods());
    // Budget meters for the kit viewer and prefab mode (`editor.budget`).
    methods.extend(crate::budget::methods());
    // Prefab variants (`editor.create_prefab_variant`, `editor.prefab_variant`,
    // `editor.set_variant_override`; ADR-2323).
    methods.extend(crate::prefab_variant_edit::methods());
    // Prefab break link (`editor.prefab_break_link`, spec 14 § A3, ADR-7203).
    methods.extend(crate::prefab_link::methods());
    methods.extend(crate::prefab_overrides_panel::methods());
    methods.extend(crate::prefab_create::methods());
    methods.extend(crate::shapes::methods());
    methods.extend(crate::blockout::methods());
    // Layer delete with the `layer_in_use` refusal (`editor.layer_delete`, spec 37 § 4.4).
    methods.extend(crate::layer_delete_agent::methods());
    // The Content Browser's cached index (`editor.content_index`).
    methods.extend(crate::content_index::methods());
    // Content Browser favorites and collections (`editor.content_favorites`, `editor.collection_*`; ADR-7759).
    methods.extend(crate::content_favorites::methods());
    methods.extend(crate::content_collections::methods());
    // Per-panel draw times for large-project soaks (`editor.panel_timings`, ADR-7306).
    methods.extend(crate::panel_timings::methods());
    // The Outliner's listed rows, open/close and reveal (`editor.outliner_rows`, ADR-7306).
    methods.extend(crate::outliner::methods());
    // The Content Browser's thumbnail cache (`editor.asset_thumbnails`, ADR-1329).
    methods.extend(crate::thumb_cache::methods());
    // Outliner drag and drop (`editor.reparent`).
    methods.extend(crate::hierarchy_dnd::methods());
    methods.extend(crate::handles::methods());
    // The Bake panel (`editor.bake_*`, `book/agents/baked-gi-editor.md`).
    methods.extend(crate::bake_panel::methods());
    // The Fracture panel (`editor.fracture_panel_*`, `book/agents/fracture.md`).
    methods.extend(crate::fracture_panel::methods());
    // Its supports, test break, bond view and Place (ADR-9750).
    methods.extend(crate::fracture_tools::methods());
    // The gameplay debug panels (`book/agents/editor-gameplay-panels.md`, ADR-2030).
    methods.extend(crate::state_tree_panel::methods());
    methods.extend(crate::debug_overlay_selected::methods());
    methods.extend(crate::visual_log_panel::methods());
    methods.extend(crate::smart_object_panel::methods());
    methods.extend(crate::camera_panel::methods());
    // The animation editor's surface (`editor.anim_*`, `editor.capture_panel`).
    methods.extend(crate::anim::agent::methods());
    // The procedural graph editor (`editor.procgen_*`, `book/agents/procgen-editor.md`).
    methods.extend(crate::procgen::agent::methods());
    // Undo history and the structural edits (`editor.history`, `editor.spawn`, …).
    methods.extend(crate::undo_agent::methods());
    // Viewport tools and the File/Edit menu items (ADR-7204, book/agents/viewport-tools.md).
    methods.extend(crate::viewport_ops_agent::methods());
    // The viewport's pick stack, select similar, selection root and measure tool (ADR-7769).
    methods.extend(crate::pick_stack::methods());
    methods.extend(crate::measure_tool::methods());
    // The view state kept per project (`editor.view_state`, ADR-7275).
    methods.extend(crate::view_state::methods());
    // The viewport pane badges (`editor.viewport_badges`, ADR-7770).
    methods.extend(crate::viewport_badges::methods());
    // The Outliner's hierarchy edits (`editor.hierarchy`, `editor.group`, `editor.select_hierarchy`;
    // `editor.reparent` is `hierarchy_dnd`'s, `editor.delete`/`editor.duplicate` are `undo_agent`'s, ADR-1354).
    methods.extend(crate::hierarchy_agent::methods());
    // The lightmap UV atlas generator (`editor.atlas_generate`, ADR-1483).
    methods.extend(crate::atlas_agent::methods());
    // The Lightmaps panel (`editor.lightmap_bake`, `editor.lightmap_status`, ..., ADR-2024).
    methods.extend(crate::lightmap_panel::methods());
    // Editor tools (ADR-1523); `editor.tool_call` and `editor.tool_poll` are deferred, see `install`.
    methods.extend(crate::tools::agent::methods());
    // The UI designer (`editor.ui_designer_*`, ADR-1811).
    methods.extend(crate::ui_designer_agent::methods());
    methods.extend(crate::ui_inspector::methods());
    // UI components in the designer (`editor.ui_designer_make_component`, ..., ADR-3147).
    methods.extend(crate::ui_designer_component_agent::methods());
    // Layout variants in the designer (`editor.ui_designer_variant`, ADR-7858).
    methods.extend(crate::ui_designer_variants::methods());
    // The UI animation editor (`editor.ui_anim_*`, ADR-3092).
    methods.extend(crate::ui_anim_agent::methods());
    // The pre-fractured destruction preview (`editor.fracture_preview`, ADR-1567).
    methods.extend(crate::fracture_agent::methods());
    // The FBX and OBJ importers' inspectors (`editor.fbx_inspect`, `editor.obj_inspect`, ADR-1602).
    methods.extend(crate::fbx_agent::methods());
    // The BVH importer's inspector (`editor.bvh_inspect`, ADR-2532).
    methods.extend(crate::bvh_agent::methods());
    // The SVG importer's report (`editor.svg_report`, ADR-3350).
    methods.extend(crate::svg_agent::methods());
    // The PLY, STL and PSD importers' inspectors (ADR-3244).
    methods.extend(crate::mesh_image_agent::methods());
    // The 2D mode (`editor.mode2d_*`, `editor.tile_stroke`, `editor.tilemap_save`, ADR-1889).
    methods.extend(crate::mode2d::methods());
    // The gameplay data panels (`editor.tables_panel_*`, ..., `book/agents/editor-data-panels.md`, ADR-2031).
    methods.extend(crate::tables_panel::methods());
    methods.extend(crate::abilities_panel::methods());
    // The audio mixer dock and the sound cue editor (`editor.audio_mixer_*`, `editor.cue_edit`, ADR-2530).
    methods.extend(crate::audio_mixer_panel::methods());
    methods.extend(crate::cue_editor_panel::methods());
    methods.extend(crate::montage_panel::methods());
    // The Video panel (`editor.video_open`, `editor.video_seek`, `editor.video_state`, ADR-2720).
    methods.extend(crate::video_panel::methods());
    // The Look-dev panel (`editor.lookdev_trace`, `editor.lookdev_status`, `editor.lookdev_cancel`, ADR-5896).
    methods.extend(crate::lookdev_panel::methods());
    methods.extend(crate::ragdoll_panel::methods());
    // The Motion Matching debug panel (`editor.mm_panel_*`, spec 52 § 6, ADR-2409).
    methods.extend(crate::mm_panel::methods());
    // The material graph editor (`editor.matgraph_*`, ADR-2318).
    methods.extend(crate::matgraph_editor::agent::methods());
    // The Material panel's shape preview (`editor.material_preview`, ADR-9700).
    methods.extend(crate::matgraph_editor::agent_s2::material_methods());
    // The editor baseline: preferences, keymap, layout, Project Settings (ADR-2321).
    methods.extend(crate::baseline_agent::methods());
    methods.extend(crate::project_settings_ext::methods());
    // The physics layer matrix (`editor.physics_layers_get`, `editor.physics_layers_edit`, ADR-7233).
    methods.extend(crate::physics_matrix::methods());
    // Inspector buttons and component actions (`editor.component_actions`,
    // `editor.run_component_action`, ADR-7785).
    methods.extend(crate::inspector::actions::methods());
    // The Diagnostics window: GPU tab and trace timeline (`editor.diagnostics`, ..., ADR-2771).
    methods.extend(crate::diagnostics_agent::methods());
    // The status bar, the drawers and the console line (ADR-7754).
    methods.extend(crate::shell_status::methods());
    methods.extend(crate::drawers::methods());
    methods.extend(crate::console::methods());
    // Provenance, the provenance policy and asset repair (`editor.asset_provenance`, ..., ADR-2428).
    methods.extend(crate::asset_provenance_agent::methods());
    // The behavior tree editor (`editor.bt_*`, ADR-2408).
    methods.extend(crate::bt_editor::agent::methods());
    // Viewport and panel tools (ADR-2696).
    methods.extend(crate::surface_tools::methods());
    methods.extend(crate::collider_handles::methods());
    methods.extend(crate::world_settings::methods());
    methods.extend(crate::log_links::methods());
    // The VFX editor (`editor.vfx_open`, `editor.vfx_state`, `editor.vfx_edit`, `editor.vfx_preview`, ADR-3171).
    methods.extend(crate::vfx_editor::agent::methods());
    // The asset file operations (`editor.edit_asset`, `editor.reimport`, `editor.move_asset`,
    // `editor.delete_asset`, `editor.set_import_settings`; `book/agents/asset-ops.md`).
    methods.extend(crate::asset_ops_agent::methods());
    // Content Browser selection, bulk actions, retarget, asset details, Save all (ADR-7822).
    methods.extend(crate::content_actions::methods());
    methods.extend(crate::save_all_dialog::methods());
    // Cancellable editor jobs (`editor.cancel_job`, specs/19 § 7, ADR-7205).
    methods.extend(crate::editor_jobs::methods());
    // The state tree editor (`editor.statetree_*`, ADR-3342).
    methods.extend(crate::state_tree_editor::agent::methods());
    // Data assets and the creators (`editor.data_asset_*`, `editor.project_*_edit`, ADR-7234).
    methods.extend(crate::data_asset::methods());
    methods.extend(crate::creators::methods());
    // The project browser's tags and the New level templates (ADR-7856).
    methods.extend(crate::recent_meta::methods());
    methods.extend(crate::samples_browser::methods());
    methods.extend(crate::level_templates::methods());
    // Content ▸ Import settings… (`editor.import_settings_*`, ADR-7469).
    methods.extend(crate::import_settings_panel::methods());
    // The Logic Graph panel (`editor.logic_*`, spec 102 § 10, ADR-7224).
    methods.extend(crate::logic_graph_editor::agent::methods());
    // The Logic Graph debugger and authoring helpers (ADR-8021).
    methods.extend(crate::logic_graph_editor::debugger_agent::methods());
    // Find usages across the project (`editor.logic_find_usages`, w13 P-183, ADR-8565).
    methods.extend(crate::logic_graph_editor::usages::methods());
    // The Inspector's pins, lock, pinned windows and component types (`book/agents/inspector.md`).
    methods.extend(crate::inspector::pinned::methods());
    methods.extend(crate::inspector::categories::methods());
    // The Inspector's field tools: copy/paste, reset, list moves (`book/agents/inspector.md`).
    methods.extend(crate::inspector::clipboard::methods());

    methods.push((
        MethodSchema {
            name: "editor.inspect_selection",
            tool: "inspect_selection",
            description: "Merged component values for the selection (or for `entities`). A field \
                that is the same on every holder has `mixed: false` and its value; a field that \
                differs has `mixed: true` and `value: null`. Vector lanes, Euler axes (rx, ry, rz) \
                and color channels (r, g, b, a) are separate. A component on only some entities \
                reports `present` and `selected`; it stays editable on the entities that have it.",
            params: &[
                ParamSchema {
                    name: "entities",
                    kind: "array",
                    required: false,
                    description: "Entity ids. Absent uses the editor selection. `[]` is an empty \
                        selection, not an error.",
                },
                ParamSchema {
                    name: "search",
                    kind: "string",
                    required: false,
                    description: "Keep only fields matching this fuzzy query, as the Inspector's \
                        search box (name, path, or component then field: lgtclr finds a light's \
                        color); components left with no field are dropped.",
                },
                ParamSchema {
                    name: "modified_only",
                    kind: "boolean",
                    required: false,
                    description: "Keep only fields that differ from their default (the prefab's \
                        value for a prefab instance) on at least one entity.",
                },
                ParamSchema {
                    name: "raw",
                    kind: "boolean",
                    required: false,
                    description: "The Inspector's raw view: mark @Hidden fields `hidden: true` and \
                        structural components `structural: true`.",
                },
            ],
            result: "{ count, entities, primary, components: [{ component, type_path, present, \
                selected, editable, structural?, fields: [{ path, mixed, value, axes?, enabled?, \
                condition?, hidden? }] }], filter? }; a field whose @EditCondition is false has \
                `enabled: false` and its `condition`",
            mutating: false,
            permission: Permission::Read,
        },
        editor_inspect_selection as Handler,
    ));
    // The USD importer's inspector (`editor.usd_inspect`, ADR-3235).
    methods.extend(crate::usd_agent::methods());
    // The conversation graph editor (`editor.conversation_*`, ADR-3298).
    methods.extend(crate::conversation_panel::agent::methods());
    // The Localization panel (`editor.loc_*`, `book/agents/editor-localization.md`, ADR-3260).
    methods.extend(crate::loc_panel::methods());
    // The cloth inspector and paint mode (`editor.cloth_panel_*`, spec 55 § 6, ADR-2884).
    methods.extend(crate::cloth_panel::methods());
    // Editor crash reports (`editor.crash_reports`, `editor.crash_report_read`, ADR-3931).
    methods.extend(crate::crash_reports::methods());
    // The Vehicle Tuning panel (`editor.vehicle_tuning_*`, `book/agents/vehicles.md`, ADR-9760).
    methods.extend(crate::vehicle_tuning::methods());
    // The Retarget panel (`editor.retarget_*`, `book/agents/retargeting.md`, ADR-7801).
    methods.extend(crate::anim::retarget_panel::methods());
    // The Profiler panel's sources and captures (`editor.profiler_*`, ADR-9740).
    methods.extend(crate::profiler_source::methods());
    // The Profiler's percent mode, modules, monitors, loads and marks (`editor.profiler_units`,
    // `_modules`, `_monitors`, `_loads`, `_marks`; ADR-8141).
    methods.extend(crate::profiler_modules::methods());
    // Autosave, crash recovery and background-task cancel (`editor.autosave_*`, `editor.recovery_*`,
    // `editor.cancel_task`, ADR-9770).
    methods.extend(crate::autosave_agent::methods());
    // Version history and project duplicate (`editor.versions`, `editor.version_*`,
    // `editor.duplicate_project`, ADR-7928).
    methods.extend(crate::version_history::methods());
    // Editor theme contrast against WCAG AA (`editor.a11y_contrast`, ADR-9810).
    methods.extend(crate::a11y_audit::methods());
    // The menu tree, Tools, packaging, Help and the project icon (`editor.menu_list`,
    // `editor.package_start`, ..., `book/agents/editor-menus.md`, ADR-7475).
    methods.extend(crate::menus::methods());
    methods.extend(crate::package_job::methods());
    // Package hygiene policy and audit review (`editor.package_hygiene_*`, ADR-8399).
    methods.extend(crate::hygiene_policy::methods());
    methods.extend(crate::about::methods());
    methods.extend(crate::project_icon::methods());
    // Inspector asset fields: picker choices and drop verdicts (`editor.asset_field_choices`, ADR-7472).
    methods.extend(crate::inspector::asset_field_agent::methods());
    // Launch standalone and on an Android device (`editor.launch*`, ADR-8980).
    methods.extend(crate::launch::methods());
    methods.extend(crate::device_mirror::methods());
    // The viewport's navigation preview (`editor.nav_preview`, ADR-7593).
    methods.extend(crate::nav_preview::methods());
    // The viewport's resolution preview (`editor.viewport_resolution_preview`, ADR-7744).
    methods.extend(crate::resolution_preview::methods());
    methods.extend(crate::viewport_overlays::methods());
    // Break a placed level as one undo step (`editor.break_placed`, spec 37 § 6.5, ADR-7614).
    methods.extend(crate::placed_break::methods());
    // Tools ▸ Bake reflection captures (`editor.bake_captures*`, ADR-7592).
    methods.extend(crate::capture_bake::methods());
    // Reflection capture box and blend-margin handles (`editor.capture_handle*`, ADR-7663).
    methods.extend(crate::capture_helper::methods());
    // What extensions add to the editor UI (`editor.extension_*`, ADR-9850).
    methods.extend(crate::extension::actions::methods());
    // One asset-open dispatcher: editor.open_asset, editor.asset_kinds (ADR-7725).
    methods.extend(crate::asset_open::methods());
    // Sequence ▸ Render movie… (`editor.render_movie*`, ADR-7726).
    methods.extend(crate::render_out_job::methods());
    // The Sequence Curves panel's Restore on stop row (`editor.seq_restore_on_stop`, ADR-7726).
    methods.extend(crate::seq_tracks::restore::methods());
    // The smart-object definition editor and its slot handles (`editor.smart_object_def_*`,
    // `editor.smart_object_slot_handle*`, ADR-7736).
    methods.extend(crate::smart_object_def_panel::methods());
    methods.extend(crate::smart_object_handles::methods());
    // Named layouts, workspaces, maximize, pane changes and feature profiles (ADR-7753).
    methods.extend(crate::workspaces::methods());
    methods.extend(crate::feature_profiles::methods());
    // The motion-database editor (`editor.mm_db_*`, spec 52 § 6, ADR-7740).
    methods.extend(crate::mm_db_editor::agent::methods());
    // The Biome editor (`editor.biome_panel_*`, w11-biome-procgen-tooling).
    methods.extend(crate::biome_panel::methods());
    // The level's changed-on-disk prompt (`editor.level_reload_resolve`, ADR-7747).
    methods.extend(crate::asset_watch::methods());
    // The editor window's screen-reader bridge (`editor.a11y_tree`, `editor.a11y_action`, ADR-7746).
    #[cfg(feature = "accesskit")]
    methods.extend(crate::a11y_bridge::methods());
    // Viewport camera navigation (`editor.camera_to_selection`, `editor.selection_to_camera`,
    // `editor.viewport_layout`, ADR-7755).
    methods.extend(crate::camera_nav_agent::methods());
    // Universal search, saved searches and the selection history (ADR-7766).
    methods.extend(crate::universal_search::methods());
    methods.extend(crate::saved_searches::methods());
    methods.extend(crate::selection::history_methods());
    // Keymap lookup and presets (`editor.keymap_lookup`, `editor.keymap_preset_*`, ADR-7767).
    methods.extend(crate::editor_keymap::methods());
    // Pilot, look-through, the camera preview and high-resolution screenshots (`editor.pilot`,
    // `editor.cameras`, `editor.look_through`, `editor.camera_preview`, `editor.screenshot`, ADR-7768).
    methods.extend(crate::viewport_pilot::methods());
    methods.extend(crate::camera_preview::methods());
    methods.extend(crate::screenshot::methods());
    // The asset editors' preview scene and the mesh helper actions (`editor.preview_scene`,
    // `editor.mesh_helper`, ADR-7771).
    methods.extend(crate::preview_scene::methods());
    methods.extend(crate::asset_viewer::mesh_helper_methods());
    // Outliner rows: configuration warnings, change kind, columns, tags, unique names (ADR-7772).
    methods.extend(crate::config_checks::methods());
    methods.extend(crate::change_kind::methods());
    methods.extend(crate::outliner_columns::methods());
    // Inspector pickers: asset field set, make unique, referencer count, curves, swatches (ADR-7786).
    methods.extend(crate::inspector::picker_agent::methods());
    methods.extend(crate::inspector::inline_asset::methods());
    // The color field's eyedropper (`editor.color_eyedropper`, P-133, ADR-8106).
    methods.extend(crate::inspector::color_eyedropper::methods());
    // The Bulk Edit grid (`editor.bulk_edit_*`, `book/agents/bulk-edit.md`, ADR-7787).
    methods.extend(crate::bulk_edit_panel::methods());
    // OS file drops (`editor.import_files`, spec addendum § 8.7, ADR-7788).
    methods.extend(crate::os_drop::methods());
    // The Asset Jobs window (`editor.asset_jobs`, ADR-7788).
    methods.extend(crate::asset_jobs_panel::methods());
    // Thumbnail refresh and capture from the viewport (`editor.thumbnail_refresh`, `editor.thumbnail_capture`).
    methods.extend(crate::thumb_cache::refresh_capture_methods());
    // Asset packages (`editor.export_package`, `editor.import_package`).
    methods.extend(crate::asset_package::methods());
    // Window ▸ Project Health and the pre-commit hook (`editor.project_health`,
    // `editor.precommit_hook`, ADR-7789).
    methods.extend(crate::project_health::methods());
    // Extension settings, viewport tools and overlays (`editor.extension_settings`,
    // `editor.extension_tools`, ADR-7791).
    methods.extend(crate::extension::settings::methods());
    methods.extend(crate::extension::viewport_tools::methods());
    // Asset and graph diff against a git revision (`editor.asset_diff`, ADR-7799).
    methods.extend(crate::asset_diff::methods());
    // The World Map: load and unload a streamed level's cells in the editor (`editor.world_*`,
    // `book/agents/world-streaming-editor.md`, ADR-7802).
    methods.extend(crate::world_edit::methods());
    methods.extend(crate::debug_colors::methods());
    methods.extend(crate::terrain_region::methods());
    methods.extend(crate::kit_paint::methods());
    // The remote Outliner/Inspector and the live link (`editor.remote_*`, `editor.live_link`, ADR-7833).
    methods.extend(crate::remote_world::methods());
    methods.extend(crate::live_link::methods());
    methods.extend(crate::safe_mode::methods());
    // Frame debugger capture and pass previews (ADR-7824).
    methods.extend(crate::frame_debugger::methods());
    // Revision control (`editor.vcs_*`, `book/agents/revision-control.md`, ADR-7837).
    methods.extend(crate::vcs::agent::methods());
    methods.extend(crate::launch_profiles::methods());
    methods.extend(crate::platforms_panel::methods());
    methods.extend(crate::size_report_panel::methods());
    methods.extend(crate::review_notes::methods());
    methods.extend(crate::activity_panel::methods());
    methods.extend(crate::utility_panel::methods());
    // Floated panels as OS windows (`editor.panel_float`, `editor.panel_instances`, ADR-7821).
    methods.extend(crate::os_windows::methods());
    // Level variants (`editor.new_level_variant`, `editor.level_variant_state`,
    // `editor.level_variant_revert`; ADR-7823).
    methods.extend(crate::level_variant::methods());
    methods.extend(crate::collab::methods());
    methods.extend(crate::agent_sessions::methods());
    // File locks (`editor.vcs_lock*`, ADR-7864).
    methods.extend(crate::vcs::locks::methods());
    // Import presets and the import dialog (`editor.import_presets`, `editor.import_dialog_state`,
    // `editor.import_with`, ADR-7904).
    methods.extend(crate::import_presets::methods());
    methods.extend(crate::import_dialog::methods());
    methods.extend(crate::content_library::methods());
    // The Profiler's Memory tab: snapshots and the GPU resource browser (`editor.memory_snapshot`,
    // `editor.gpu_resources`; ADR-8142).
    methods.extend(crate::memory_browser::methods());
    methods.extend(crate::gpu_browser::methods());
    // Panel instances (`editor.panel_new_instance`, P-003, ADR-8105 (a)).
    methods.extend(crate::panel_instances::methods());
    // Prefab edit mode's logic actions (`editor.prefab_logic`, P-165, ADR-8566).
    methods.extend(crate::prefab_logic::methods());
    // The Match Rules panel and the Match strip (`editor.rules_*`, `editor.playlist_edit`,
    // `editor.match_control`, `editor.match_rules_state`; ADR-8738).
    methods.extend(crate::match_rules_panel::methods());
    methods
}

/// Schema row of `editor.select`.
fn editor_select_schema() -> MethodSchema {
    MethodSchema {
        name: "editor.select",
        tool: "select",
        description: "Change the editor selection.",
        params: &[
            ParamSchema {
                name: "entities",
                kind: "array",
                required: true,
                description: "Entity ids; `[]` clears the selection.",
            },
            ParamSchema {
                name: "mode",
                kind: "string",
                required: false,
                description: "\"set\" (default) | \"add\" | \"toggle\".",
            },
        ],
        result: "{ entities, primary }",
        mutating: true,
        permission: Permission::EditScene,
    }
}

/// `editor.select`.
fn editor_select(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let items = params
        .get("entities")
        .and_then(Value::as_array)
        .ok_or_else(|| RpcError::invalid_params("`entities` (array of ids) is required"))?;
    // Every element must be an entity id: dropping the others turned `["primary"]` into an
    // empty list, which cleared the selection and reported success (audit v033b).
    let ids: Vec<u64> = items
        .iter()
        .map(|item| {
            item.as_u64().ok_or_else(|| {
                RpcError::invalid_params(format!("`entities` holds {item}, which is not an entity id"))
            })
        })
        .collect::<Result<_, _>>()?;
    let mode = match params.get("mode").and_then(Value::as_str).unwrap_or("set") {
        "set" => SelectMode::Replace,
        "add" => SelectMode::Add,
        "toggle" => SelectMode::Toggle,
        other => {
            return Err(RpcError::invalid_params(format!(
                "mode {other:?} (set, add, toggle)"
            )));
        }
    };
    let mut selection = world
        .get_resource_or_insert_with::<Selection>(Selection::default)
        .clone();
    for (index, id) in ids.iter().enumerate() {
        // Ids from before an undo still select the entity the undo brought back.
        let entity = crate::command::resolve_entity(world, *id).ok_or_else(|| {
            RpcError::not_found(format!("entity {id} does not exist"), "use scene.query_entities")
        })?;
        if crate::visibility::is_hidden(world, entity) {
            return Err(RpcError::invalid_params(format!(
                "entity {id} is editor-internal and not selectable"
            )));
        }
        // "set" replaces the selection once (the first id) and adds the rest, so the whole list
        // stays selected with the last id primary; add and toggle apply per id as before.
        let step = if mode == SelectMode::Replace && index > 0 {
            SelectMode::Add
        } else {
            mode
        };
        selection.select(entity, step);
    }
    if ids.is_empty() && mode == SelectMode::Replace {
        selection.clear();
    }
    let out = selection_json(world, &selection);
    world.insert_resource(selection);
    Ok(out)
}

/// Schema row of `editor.select_by_asset` (spec 19 § 3).
fn editor_select_by_asset_schema() -> MethodSchema {
    MethodSchema {
        name: "editor.select_by_asset",
        tool: "select_by_asset",
        description: "Select every level entity that references an asset: each reflected string \
            field equal to the path (with or without its assets/ prefix — a mesh's source, its \
            material override, a sound emitter's sound) and every prefab instance of the path's \
            asset id. The Content Browser's Select in level runs this. An unknown path answers \
            ok with count 0.",
        params: &[
            ParamSchema {
                name: "path",
                kind: "string",
                required: true,
                description: "The asset's project-relative path (assets/…).",
            },
            ParamSchema {
                name: "mode",
                kind: "string",
                required: false,
                description: "\"set\" (default) | \"add\" | \"toggle\", as editor.select.",
            },
        ],
        result: "{ path, count, entities, primary }",
        mutating: true,
        permission: Permission::EditScene,
    }
}

/// `editor.select_by_asset`: selects what [`crate::content::entities_using_asset`] finds, with
/// `editor.select`'s modes (a "set" replaces the whole selection, it does not loop the
/// per-entity `Replace` event).
fn editor_select_by_asset(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.trim().is_empty())
        .ok_or_else(|| RpcError::invalid_params("`path` (the asset's project-relative path) is required"))?;
    let mode = match params.get("mode").and_then(Value::as_str).unwrap_or("set") {
        "set" => SelectMode::Replace,
        "add" => SelectMode::Add,
        "toggle" => SelectMode::Toggle,
        other => {
            return Err(RpcError::invalid_params(format!(
                "mode {other:?} (set, add, toggle)"
            )));
        }
    };
    let found = crate::content::entities_using_asset(world, path);
    let count = found.len();
    let mut selection = world
        .get_resource_or_insert_with::<Selection>(Selection::default)
        .clone();
    if mode == SelectMode::Replace {
        selection.clear();
    }
    for entity in &found {
        selection.select(
            *entity,
            if mode == SelectMode::Replace {
                SelectMode::Add
            } else {
                mode
            },
        );
    }
    let out = json!({
        "path": path,
        "count": count,
        "entities": selection_json(world, &selection)["entities"],
        "primary": selection.primary().map(|e| e.to_bits()),
    });
    world.insert_resource(selection);
    Ok(out)
}

/// `editor.get_selection`.
fn editor_get_selection(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let _ = params;
    let selection = world
        .get_resource_or_insert_with::<Selection>(Selection::default)
        .clone();
    Ok(selection_json(world, &selection))
}

/// The selection as protocol JSON.
fn selection_json(world: &World, selection: &Selection) -> Value {
    json!({
        "entities": selection.entities().iter().map(|e| json!({
            "entity": e.to_bits(),
            "name": world
                .get::<regia_scene::prelude::Name>(*e)
                .map(|n| n.as_str().to_string())
                .or_else(|| {
                    world
                        .get::<regia_ecs::ecs::name::Name>(*e)
                        .map(|n| n.as_str().to_string())
                }),
        })).collect::<Vec<_>>(),
        "primary": selection.primary().map(|e| e.to_bits()),
    })
}

/// `editor.set_gizmo`.
fn editor_set_gizmo(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut gizmo = world
        .get_resource_or_insert_with::<GizmoState>(GizmoState::default)
        .clone();
    if let Some(mode) = params.get("mode").and_then(Value::as_str) {
        gizmo.mode = GizmoMode::from_name(mode).ok_or_else(|| {
            RpcError::invalid_params(format!("mode {mode:?} (translate, rotate, scale, select)"))
        })?;
    }
    // The reference space (ADR-7756).
    if let Some(space) = params.get("space").filter(|v| !v.is_null()) {
        let name = space.as_str().unwrap_or_default();
        gizmo.space = crate::viewport::GizmoSpace::from_name(name).ok_or_else(|| {
            RpcError::new(
                regia_agent::protocol::INVALID_PARAMS,
                "gizmo_space_unknown",
                format!("space {space} (world, local, parent, view)"),
                "pass one of world, local, parent, view",
            )
        })?;
    }
    if let Some(snap) = params.get("snap").and_then(Value::as_bool) {
        gizmo.snap = snap;
    }
    let out = json!({
        "mode": gizmo.mode.name(),
        "snap": gizmo.snap,
        "space": gizmo.space.name(),
    });
    world.insert_resource(gizmo);
    Ok(out)
}

/// Whether every `f32`/`f64` in `value` is finite.
///
/// JSON `1e39` is a finite `f64` that the reflect deserializer stores as `f32::INFINITY`.
/// `editor.set_transform` already refuses that cast. A reflected component (and a vector or
/// quaternion stored opaquely) has to be walked, or `editor.set_property` writes the infinity.
pub(crate) fn reflected_is_finite(value: &dyn regia_ecs::reflect::PartialReflect) -> bool {
    if let Some(finite) = reflected_leaf_finite(value) {
        return finite;
    }
    use regia_ecs::reflect::ReflectRef;
    match value.reflect_ref() {
        ReflectRef::Struct(fields) => fields.iter_fields().all(|(_, field)| reflected_is_finite(field)),
        ReflectRef::TupleStruct(fields) => fields.iter_fields().all(reflected_is_finite),
        ReflectRef::Tuple(fields) => fields.iter_fields().all(reflected_is_finite),
        ReflectRef::List(list) => list.iter().all(reflected_is_finite),
        ReflectRef::Array(array) => array.iter().all(reflected_is_finite),
        ReflectRef::Map(map) => map
            .iter()
            .all(|(key, item)| reflected_is_finite(key) && reflected_is_finite(item)),
        ReflectRef::Set(set) => set.iter().all(reflected_is_finite),
        ReflectRef::Enum(value) => value
            .iter_fields()
            .all(|field| reflected_is_finite(field.value())),
        ReflectRef::Opaque(_) => true,
    }
}

/// `Some` when `value` is a float or a glam leaf this check understands.
fn reflected_leaf_finite(value: &dyn regia_ecs::reflect::PartialReflect) -> Option<bool> {
    use regia_core::math::{Mat3, Mat4, Quat, Vec2, Vec3, Vec3A, Vec4};
    if let Some(number) = value.try_downcast_ref::<f32>() {
        return Some(number.is_finite());
    }
    if let Some(number) = value.try_downcast_ref::<f64>() {
        return Some(number.is_finite());
    }
    if let Some(vector) = value.try_downcast_ref::<Vec2>() {
        return Some(vector.is_finite());
    }
    if let Some(vector) = value.try_downcast_ref::<Vec3>() {
        return Some(vector.is_finite());
    }
    if let Some(vector) = value.try_downcast_ref::<Vec3A>() {
        return Some(vector.is_finite());
    }
    if let Some(vector) = value.try_downcast_ref::<Vec4>() {
        return Some(vector.is_finite());
    }
    if let Some(rotation) = value.try_downcast_ref::<Quat>() {
        return Some(rotation.is_finite());
    }
    if let Some(matrix) = value.try_downcast_ref::<Mat3>() {
        return Some(matrix.is_finite());
    }
    if let Some(matrix) = value.try_downcast_ref::<Mat4>() {
        return Some(matrix.is_finite());
    }
    None
}

/// One JSON number as a finite `f32`: a number outside the `f32` range (`1e39`) would cast to
/// infinity and land on a `Transform` or an override (audit v033b), so it is refused.
pub(crate) fn finite_f32(item: &Value, name: &str) -> Result<f32, RpcError> {
    let value = item
        .as_f64()
        .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be numbers")))? as f32;
    if value.is_finite() {
        Ok(value)
    } else {
        Err(RpcError::invalid_params(format!(
            "`{name}` must be finite numbers within the f32 range"
        )))
    }
}

/// Reads an `[x, y, z]` parameter.
pub(crate) fn vec3_param(params: &Value, name: &str) -> Result<Option<regia_core::math::Vec3>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) if items.len() == 3 => {
            let mut out = [0.0f32; 3];
            for (i, item) in items.iter().enumerate() {
                out[i] = finite_f32(item, name)?;
            }
            Ok(Some(regia_core::math::Vec3::from(out)))
        }
        Some(_) => Err(RpcError::invalid_params(format!("`{name}` must be [x, y, z]"))),
    }
}

/// Reads an `[x, y, z, w]` quaternion parameter.
pub(crate) fn quat_param(params: &Value, name: &str) -> Result<Option<regia_core::math::Quat>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) if items.len() == 4 => {
            let mut out = [0.0f32; 4];
            for (i, item) in items.iter().enumerate() {
                out[i] = finite_f32(item, name)?;
            }
            Ok(
                Some(regia_core::math::Quat::from_xyzw(out[0], out[1], out[2], out[3])).map(|q| {
                    if q.length_squared() < 1e-8 {
                        regia_core::math::Quat::IDENTITY
                    } else {
                        q.normalize()
                    }
                }),
            )
        }
        Some(_) => Err(RpcError::invalid_params(format!("`{name}` must be [x, y, z, w]"))),
    }
}

/// `editor.set_transform` — the gizmo edit as a protocol call.
fn editor_set_transform(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if conflicting_entity_params(params) {
        return Err(with_reason(
            RpcError::invalid_params("pass `entity` or `entities`, not both"),
            "entity_and_entities",
        ));
    }
    if params.get("entities").is_some_and(|value| !value.is_null()) {
        return editor_set_transform_many(world, params);
    }
    let entity = crate::undo_agent::live_entity(world, params, "entity")?;
    regia_agent::require_not_feed_owned(world, entity)?;
    let current = world
        .get::<regia_scene::prelude::Transform>(entity)
        .copied()
        .ok_or_else(|| {
            RpcError::not_found(
                format!("entity {} has no Transform", entity.to_bits()),
                "add a Transform first (scene.add_component)",
            )
        })?;
    let translation = vec3_param(params, "translation")?;
    let rotation = quat_param(params, "rotation")?;
    let scale = vec3_param(params, "scale")?;
    let new = regia_scene::prelude::Transform {
        translation: translation.unwrap_or(current.translation),
        rotation: rotation.unwrap_or(current.rotation),
        scale: scale.unwrap_or(current.scale),
    };
    let mut command = SetTransform::capture(world, entity, new);
    command.apply(world);
    // A call is a discrete step: it never merges into a gizmo drag or an earlier call.
    let undoable = crate::undo_agent::push_discrete(world, Box::new(command));
    Ok(json!({
        "entity": entity.to_bits(),
        "translation": [new.translation.x, new.translation.y, new.translation.z],
        "rotation": [new.rotation.x, new.rotation.y, new.rotation.z, new.rotation.w],
        "scale": [new.scale.x, new.scale.y, new.scale.z],
        "undoable": undoable,
    }))
}

/// The path of the first field the JSON `value` leaves out, walked against the reflected type
/// `info` in bevy_reflect's serde layout (structs as objects, tuples as arrays, newtype structs
/// and single-field tuple variants inline, `Option` as `null`/the value, other enums externally
/// tagged); `None` when nothing is missing. `prefix` is the path of `value` itself. Needed because
/// the reflect deserializer fills omitted fields of `#[reflect(Default)]` types with defaults.
pub(crate) fn first_missing_field(
    value: &Value,
    info: &regia_ecs::reflect::TypeInfo,
    registry: &regia_ecs::reflect::TypeRegistry,
    prefix: &str,
) -> Option<String> {
    use regia_ecs::reflect::TypeInfo;
    use regia_ecs::reflect::enums::VariantInfo;
    use regia_ecs::reflect::serde::SerializationData;
    let join = |name: &str| {
        if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}.{name}")
        }
    };
    let index = |i: usize| format!("{prefix}[{i}]");
    let of = |type_id: std::any::TypeId| registry.get_type_info(type_id);
    let skipped = |i: usize| {
        registry
            .get_type_data::<SerializationData>(info.type_id())
            .is_some_and(|data| data.is_field_skipped(i))
    };
    match info {
        TypeInfo::Struct(info) => {
            let object = value.as_object()?;
            info.iter()
                .enumerate()
                .find_map(|(i, field)| match object.get(field.name()) {
                    None if !skipped(i) => Some(join(field.name())),
                    None => None,
                    Some(inner) => {
                        first_missing_field(inner, of(field.type_id())?, registry, &join(field.name()))
                    }
                })
        }
        TypeInfo::TupleStruct(info) if info.field_len() == 1 && !skipped(0) => {
            let field = info.field_at(0)?;
            first_missing_field(value, of(field.type_id())?, registry, &join("0"))
        }
        TypeInfo::TupleStruct(info) => {
            let items = value.as_array()?;
            let wanted = (0..info.field_len()).filter(|i| !skipped(*i)).count();
            if items.len() < wanted {
                return Some(join(&items.len().to_string()));
            }
            items
                .iter()
                .zip(info.iter())
                .enumerate()
                .find_map(|(i, (inner, field))| {
                    first_missing_field(inner, of(field.type_id())?, registry, &join(&i.to_string()))
                })
        }
        TypeInfo::Tuple(info) => {
            let items = value.as_array()?;
            if items.len() < info.field_len() {
                return Some(join(&items.len().to_string()));
            }
            items
                .iter()
                .zip(info.iter())
                .enumerate()
                .find_map(|(i, (inner, field))| {
                    first_missing_field(inner, of(field.type_id())?, registry, &join(&i.to_string()))
                })
        }
        TypeInfo::List(info) => {
            let item = of(info.item_ty().id())?;
            let items = value.as_array()?;
            items
                .iter()
                .enumerate()
                .find_map(|(i, inner)| first_missing_field(inner, item, registry, &index(i)))
        }
        TypeInfo::Array(info) => {
            let item = of(info.item_ty().id())?;
            let items = value.as_array()?;
            if items.len() < info.capacity() {
                return Some(index(items.len()));
            }
            items
                .iter()
                .enumerate()
                .find_map(|(i, inner)| first_missing_field(inner, item, registry, &index(i)))
        }
        TypeInfo::Set(info) => {
            let item = of(info.value_ty().id())?;
            value
                .as_array()?
                .iter()
                .find_map(|inner| first_missing_field(inner, item, registry, prefix))
        }
        TypeInfo::Map(info) => {
            let item = of(info.value_ty().id())?;
            value
                .as_object()?
                .values()
                .find_map(|inner| first_missing_field(inner, item, registry, prefix))
        }
        TypeInfo::Enum(info) => {
            if info.type_path_table().module_path() == Some("core::option") {
                if value.is_null() {
                    return None;
                }
                let VariantInfo::Tuple(some) = info.variant("Some")? else {
                    return None;
                };
                return first_missing_field(value, of(some.field_at(0)?.type_id())?, registry, &join("0"));
            }
            let (name, payload) = value.as_object()?.iter().next()?;
            match info.variant(name)? {
                VariantInfo::Struct(variant) => {
                    let object = payload.as_object()?;
                    variant.iter().find_map(|field| match object.get(field.name()) {
                        None => Some(join(field.name())),
                        Some(inner) => {
                            first_missing_field(inner, of(field.type_id())?, registry, &join(field.name()))
                        }
                    })
                }
                VariantInfo::Tuple(variant) if variant.field_len() == 1 => {
                    first_missing_field(payload, of(variant.field_at(0)?.type_id())?, registry, &join("0"))
                }
                VariantInfo::Tuple(variant) => {
                    let items = payload.as_array()?;
                    if items.len() < variant.field_len() {
                        return Some(join(&items.len().to_string()));
                    }
                    items
                        .iter()
                        .zip(variant.iter())
                        .enumerate()
                        .find_map(|(i, (inner, field))| {
                            first_missing_field(inner, of(field.type_id())?, registry, &join(&i.to_string()))
                        })
                }
                VariantInfo::Unit(_) => None,
            }
        }
        TypeInfo::Opaque(_) => None,
    }
}

/// A reflected value as JSON (the reflect serializer's form, like `scene.get_entity`).
fn reflect_json(value: &dyn PartialReflect, registry: &regia_ecs::reflect::TypeRegistry) -> Value {
    let serializer = regia_ecs::reflect::serde::TypedReflectSerializer::new(value, registry);
    serde_json::to_value(&serializer).unwrap_or(Value::Null)
}

/// `editor.set_property`: the inspector's edit path for agents — a `SetProperty` command on the
/// undo stack (undoable with `editor.undo`, recorded in the agent audit like every mutation).
fn editor_set_property(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use serde::de::DeserializeSeed as _;
    if conflicting_entity_params(params) {
        return Err(with_reason(
            RpcError::invalid_params("pass `entity` or `entities`, not both"),
            "entity_and_entities",
        ));
    }
    if params.get("entities").is_some_and(|value| !value.is_null()) {
        if params.get("expr").is_some_and(|expr| !expr.is_null()) {
            return Err(with_reason(
                RpcError::invalid_params("`expr` edits one entity; pass `value` with `entities`"),
                "expr_with_entities",
            ));
        }
        return editor_set_property_many(world, params);
    }
    let entity = crate::undo_agent::live_entity(world, params, "entity")?;
    regia_agent::require_not_feed_owned(world, entity)?;
    let component = params
        .get("component")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`component` (type name, e.g. \"Transform\") is required"))?
        .to_string();
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // `expr` (ADR-7785): an Inspector expression on a number field, evaluated against its
    // current value in the unit the Inspector shows.
    let expr_value;
    let value = match params.get("expr").filter(|expr| !expr.is_null()) {
        Some(expr) => {
            if params.get("value").is_some_and(|value| !value.is_null()) {
                return Err(with_reason(
                    RpcError::invalid_params("pass `value` or `expr`, not both"),
                    "value_and_expr",
                ));
            }
            let expr = expr
                .as_str()
                .ok_or_else(|| RpcError::wrong_type("expr", "a string"))?;
            expr_value = crate::inspector::expr::property_expr_value(world, entity, &component, &path, expr)
                .map_err(|(code, message)| with_reason(RpcError::invalid_params(message), code))?;
            &expr_value
        }
        None => params
            .get("value")
            .ok_or_else(|| RpcError::invalid_params("`value` (or `expr`) is required"))?,
    };
    let dry_run = params.get("dry_run").and_then(Value::as_bool).unwrap_or(false);
    let current = crate::command::read_property(world, entity, &component, &path).map_err(|err| {
        RpcError::not_found(
            err.to_string(),
            "see editor/scene get_entity for component and field names; list elements are `name[i]`",
        )
    })?;
    let registry = world.resource::<regia_ecs::reflect::AppTypeRegistry>().clone();
    let registry = registry.read();
    let field_type = current
        .get_represented_type_info()
        .map(|info| info.type_id())
        .and_then(|type_id| registry.get(type_id))
        .ok_or_else(|| RpcError::internal(format!("`{component}.{path}` has no registered type")))?;
    // The reflect deserializer fills fields the JSON omits with the type's defaults (and
    // `try_apply` would treat a partial dynamic value as a patch): a whole value must spell out
    // every field (`specs/23`: no silent guesses).
    if let Some(missing) = first_missing_field(value, field_type.type_info(), &registry, "") {
        return Err(RpcError::invalid_params(format!(
            "value does not fit `{path}` ({}): `{missing}` is missing; give every field (see `before` from a dry run)",
            field_type.type_info().type_path()
        )));
    }
    let new_value = regia_ecs::reflect::serde::TypedReflectDeserializer::new(field_type, &registry)
        .deserialize(value)
        .map_err(|err| {
            RpcError::invalid_params(format!(
                "value does not fit `{path}` ({}): {err}",
                field_type.type_info().type_path()
            ))
        })?;
    if !reflected_is_finite(new_value.as_ref()) {
        return Err(RpcError::invalid_params(
            "value has a non-finite number (inf, nan, or outside the f32 range)",
        ));
    }
    let before = reflect_json(current.as_ref(), &registry);
    let after = reflect_json(new_value.as_ref(), &registry);
    drop(registry);
    let mut undoable = world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .undo_depth();
    if !dry_run {
        let mut command = crate::command::SetProperty::capture(world, entity, &component, &path, new_value)
            .map_err(|err| RpcError::invalid_params(err.to_string()))?;
        // Agent edits are discrete steps: never merged into a neighbouring edit.
        command.coalesce = false;
        command.apply(world);
        undoable = crate::undo_agent::push_discrete(world, Box::new(command));
    }
    Ok(json!({
        "entity": entity.to_bits(),
        "component": component,
        "path": path,
        "before": before,
        "after": after,
        "applied": !dry_run,
        "undoable": undoable,
    }))
}

/// `entity` and `entities` are both set (and neither is null).
fn conflicting_entity_params(params: &Value) -> bool {
    let entity = params.get("entity").is_some_and(|value| !value.is_null());
    let entities = params.get("entities").is_some_and(|value| !value.is_null());
    entity && entities
}

/// Puts `reason` on an error's data object (lowercase snake, part of the protocol).
fn with_reason(mut err: RpcError, reason: &str) -> RpcError {
    if let Some(Value::Object(data)) = err.data.as_mut() {
        data.insert("reason".into(), Value::String(reason.to_string()));
    }
    err
}

/// Maps a multi-edit failure onto the protocol. A dead entity stays `not_found` with no reason.
fn edit_error(err: crate::inspector::EditError) -> RpcError {
    let code = err.code;
    let message = err.message;
    let mapped = match code {
        "prefab_apply_failed" => RpcError::internal(message),
        "not_found" => RpcError::not_found(
            message,
            "use scene.query_entities; an undo may have removed it (editor.history shows the stack)",
        ),
        "component_on_none"
        | "path_unknown"
        | "override_absent"
        | "not_prefab_instance"
        | "prefab_template_missing" => {
            RpcError::not_found(message, "see editor.inspect_selection for the merged fields")
        }
        _ => RpcError::invalid_params(message),
    };
    if code == "not_found" {
        mapped
    } else {
        with_reason(mapped, code)
    }
}

/// The `entities` array: non-empty ids, each alive.
fn entities_param(world: &World, params: &Value) -> Result<Vec<Entity>, RpcError> {
    let value = params
        .get("entities")
        .ok_or_else(|| RpcError::invalid_params("`entities` is required"))?;
    let Some(items) = value.as_array() else {
        return Err(with_reason(
            RpcError::invalid_params("`entities` must be an array of entity ids"),
            "entities_not_array",
        ));
    };
    if items.is_empty() {
        return Err(with_reason(
            RpcError::invalid_params("`entities` is empty"),
            "entities_empty",
        ));
    }
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let Some(bits) = item.as_u64() else {
            return Err(RpcError::invalid_params("`entities` elements must be entity ids"));
        };
        let entity = crate::command::resolve_entity(world, bits).ok_or_else(|| {
            RpcError::not_found(
                format!("entity {bits} does not exist"),
                "use scene.query_entities; an undo may have removed it (editor.history shows the stack)",
            )
        })?;
        out.push(entity);
    }
    Ok(out)
}

/// `editor.set_transform` with `entities`.
fn editor_set_transform_many(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entities = entities_param(world, params)?;
    let translation = vec3_param(params, "translation")?;
    let rotation = quat_param(params, "rotation")?;
    let scale = vec3_param(params, "scale")?;
    let report = crate::inspector::set_transforms(world, &entities, translation, rotation, scale)
        .map_err(edit_error)?;
    let first_bits = report.edited.first().copied().unwrap_or(0);
    let shown = crate::command::resolve_entity(world, first_bits)
        .and_then(|entity| world.get::<regia_scene::prelude::Transform>(entity).copied())
        .unwrap_or(regia_scene::prelude::Transform::IDENTITY);
    Ok(json!({
        "entity": first_bits,
        "entities": report.edited,
        "skipped": report.skipped,
        "translation": [shown.translation.x, shown.translation.y, shown.translation.z],
        "rotation": [shown.rotation.x, shown.rotation.y, shown.rotation.z, shown.rotation.w],
        "scale": [shown.scale.x, shown.scale.y, shown.scale.z],
        "undoable": report.undo_depth,
    }))
}

/// `editor.set_property` with `entities`.
fn editor_set_property_many(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entities = entities_param(world, params)?;
    let component = params
        .get("component")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`component` (type name, e.g. \"Transform\") is required"))?
        .to_string();
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let value = params
        .get("value")
        .ok_or_else(|| RpcError::invalid_params("`value` is required"))?;
    let dry_run = params.get("dry_run").and_then(Value::as_bool).unwrap_or(false);
    let decoded = crate::inspector::multi::decode_edit_value(world, &entities, &component, &path, value)
        .map_err(edit_error)?;
    let preview =
        crate::inspector::multi::preview_edit(world, &entities, &component, &path, decoded.as_ref())
            .map_err(edit_error)?;
    let mut undoable = world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .undo_depth();
    if !dry_run {
        let report =
            crate::inspector::edit_value(world, &entities, &component, &path, decoded).map_err(edit_error)?;
        undoable = report.undo_depth;
    }
    let befores: Vec<Value> = preview
        .befores
        .iter()
        .zip(&preview.edited)
        .map(|(before, bits)| json!({ "entity": bits, "before": before }))
        .collect();
    let before = preview.befores.first().cloned().unwrap_or(Value::Null);
    Ok(json!({
        "entity": preview.first,
        "entities": preview.edited,
        "skipped": preview.skipped,
        "component": component,
        "path": path,
        "before": before,
        "after": preview.after,
        "befores": befores,
        "applied": !dry_run,
        "undoable": undoable,
    }))
}

/// `editor.inspect_selection`: merged component values for the selection, with mixed flags.
fn editor_inspect_selection(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if let Some(value) = params.get("entities") {
        let Some(items) = value.as_array() else {
            return Err(with_reason(
                RpcError::invalid_params("`entities` must be an array of entity ids"),
                "entities_not_array",
            ));
        };
        let mut entities = Vec::with_capacity(items.len());
        for item in items {
            let Some(bits) = item.as_u64() else {
                return Err(RpcError::invalid_params("`entities` elements must be entity ids"));
            };
            let entity = crate::command::resolve_entity(world, bits).ok_or_else(|| {
                RpcError::not_found(
                    format!("entity {bits} does not exist"),
                    "use scene.query_entities",
                )
            })?;
            entities.push(entity);
        }
        let primary = entities.last().copied();
        let mut inspected = crate::inspector::inspect_entities(world, &entities, primary);
        crate::inspector::condition::annotate_inspection(world, &mut inspected, raw_param(params)?);
        return crate::inspector::search::filter_inspection(world, inspected, params);
    }
    let selection = world
        .get_resource_or_insert_with::<Selection>(Selection::default)
        .clone();
    let mut inspected = crate::inspector::inspect_entities(world, selection.entities(), selection.primary());
    crate::inspector::condition::annotate_inspection(world, &mut inspected, raw_param(params)?);
    crate::inspector::search::filter_inspection(world, inspected, params)
}

/// `raw` of `editor.inspect_selection` (default false).
fn raw_param(params: &Value) -> Result<bool, RpcError> {
    match params.get("raw") {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(on)) => Ok(*on),
        Some(_) => Err(RpcError::invalid_params("`raw` must be a boolean")),
    }
}

/// `editor.undo`.
fn editor_undo(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let description = world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .next_undo();
    let done = crate::command::undo_world(world);
    let stack = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
    Ok(json!({
        "undone": done,
        "remaining": stack.undo_depth(),
        "description": description,
        "dirty": stack.is_dirty(),
    }))
}

/// `editor.redo`.
fn editor_redo(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let description = world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .next_redo();
    let done = crate::command::redo_world(world);
    let stack = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
    Ok(json!({
        "redone": done,
        "remaining": stack.redo_depth(),
        "description": description,
        "dirty": stack.is_dirty(),
    }))
}

/// Pixel size for `editor.capture_panel`: default, then clamp to 64..=4096 (the offscreen
/// renderer's limit, `anim::capture::render_ui_offscreen`).
pub(crate) fn capture_dim(params: &Value, name: &str, default: u32) -> u32 {
    let raw = params
        .get(name)
        .and_then(Value::as_u64)
        .unwrap_or(u64::from(default));
    u32::try_from(raw).unwrap_or(default).clamp(64, 4096)
}

/// The `frames` of `editor.step`: a whole number in `1..=`[`MAX_STEP_FRAMES`], default 1.
pub(crate) fn step_frames(params: &Value) -> Result<u32, RpcError> {
    let Some(value) = params.get("frames").filter(|v| !v.is_null()) else {
        return Ok(1);
    };
    match value.as_u64() {
        Some(n) if (1..=u64::from(MAX_STEP_FRAMES)).contains(&n) => Ok(n as u32),
        _ => Err(RpcError::invalid_params(format!(
            "`frames` must be a whole number from 1 to {MAX_STEP_FRAMES}"
        ))),
    }
}

/// Most frames one `editor.step` call may queue.
pub(crate) const MAX_STEP_FRAMES: u32 = 10_000;

/// The GPU the viewport capture draws with.
///
/// Prefers a device the shell (or an earlier capture) already stored on the world, so
/// pipelines compile once. Otherwise builds a headless device and stores it.
pub(crate) fn viewport_device(world: &mut World) -> Result<regia_render::RenderDevice, RpcError> {
    if let Some(device) = world.get_resource::<regia_render::RenderDevice>().cloned() {
        return Ok(device);
    }
    // A headless editor without an adapter answers `editor_headless` (ADR-2412).
    if let Some(err) = crate::headless::no_adapter_error(world) {
        return Err(err);
    }
    match crate::render::headless_device() {
        Ok(device) => {
            world.insert_resource(device.clone());
            Ok(device)
        }
        Err(err) => Err(RpcError::new(
            regia_agent::protocol::UNSUPPORTED,
            "F0011",
            format!("no GPU for the editor viewport: {err}"),
            "a Vulkan adapter (lavapipe or hardware) is required",
        )),
    }
}

/// `editor.save_level`.
pub(crate) fn editor_save_level(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    // A prefab open in place: nothing is written and the prefab's undo history keeps its dirty
    // flag (ADR-7835).
    if let Some((reason, refusal)) = crate::prefab_edit::level_save_refusal_reason(world) {
        let mut err = RpcError::invalid_params(refusal);
        err.data = Some(json!({ "reason": reason }));
        return Err(err);
    }
    let open = world.get_resource::<OpenLevel>().cloned().ok_or_else(|| {
        RpcError::not_found(
            "no level is open in the editor",
            "open a level first (File → Open, or double-click one in the Content Browser)",
        )
    })?;
    let path = match params.get("path").and_then(Value::as_str) {
        None => open.path.clone(),
        Some(p) => Some(document_request_path(world, "path", p)?),
    };
    let Some(path) = path else {
        return Err(RpcError::invalid_params(
            "the level has no path yet; pass `path` for a save-as",
        ));
    };
    // A level someone else locks (git LFS) refuses the save with `locked_by_other` (ADR-7864).
    crate::vcs::locks::guard_save(world, &path).map_err(|e| crate::vcs::locks::rpc_error(&e))?;
    // The level, the re-cooked tiles of sculpted terrains (the asset watcher reloads them), and
    // the holes and painted weights next to the level.
    let terrain = level_io::save_level_and_terrain(world, &open, &path)
        .map_err(|err| RpcError::internal(format!("save failed: {err}")))?;
    // The 2D mode's map, when it has unsaved strokes (ADR-1889).
    let tilemap_saved = crate::mode2d::save_dirty_tilemap(world)
        .map_err(|err| RpcError::internal(format!("tile map save failed: {err}")))?
        .map(|(saved, _)| saved.display().to_string());
    let written = match std::fs::metadata(&path) {
        Ok(meta) => meta.len(),
        Err(_) => 0,
    };
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .mark_saved();
    if let Some(root) = world
        .get_resource::<EditorProject>()
        .and_then(|project| project.root.clone())
    {
        crate::autosave::clear_autosave_after_save(&root, &open, &path);
    }
    let mut open = open;
    open.path = Some(path.clone());
    world.insert_resource(open);
    // Tile-map failure returned above, so the open path is this save's path.
    crate::terrain_tool::commit_terrain_save(world);
    Ok(json!({
        "path": path.display().to_string(),
        "bytes": written,
        "terrain_tiles_recooked": terrain.tiles_recooked,
        "terrain_holes_file": terrain.holes_file.map(|file| file.display().to_string()),
        "terrain_paint_file": terrain.paint_file.map(|file| file.display().to_string()),
        "tilemap_saved": tilemap_saved,
        "terrain_scatter_file": terrain.scatter_file.map(|file| file.display().to_string()),
        "terrain_grass_density_file": terrain.grass_density_file.map(|file| file.display().to_string()),
    }))
}

/// Queues a PIE request (`editor.play`/`pause`/`resume`/`stop`/`step`). The editor loop applies
/// it on the next frame; agents poll `editor.play_state` for the result (specs/23 § 4: nothing
/// blocks the protocol thread).
fn queue_pie(world: &mut World, request: crate::pie::PieRequest) -> Result<Value, RpcError> {
    world
        .get_resource_or_insert_with::<crate::pie::PieRequests>(Default::default)
        .push(request);
    Ok(json!({
        "queued": true,
        "hint": "poll editor.play_state — the editor applies the request on its next frame",
    }))
}

/// The error of an `editor.play` option (`play_mode_unknown`, `play_players_out_of_range`,
/// `play_window_unavailable`).
pub(crate) fn play_option_error(reason: &str, message: impl Into<String>) -> RpcError {
    with_reason(
        RpcError::new(
            regia_agent::protocol::INVALID_PARAMS,
            "F0002",
            message,
            "see book/agents/pie-modes.md",
        ),
        reason,
    )
}

/// `editor.play`'s options (`window: true` only where the host has a window system).
fn play_options(world: &World, params: &Value) -> Result<crate::pie::PlayOptions, RpcError> {
    let mut options = crate::pie::PlayOptions::default();
    if let Some(mode) = params.get("mode").filter(|v| !v.is_null()) {
        let name = mode.as_str().unwrap_or("");
        options.kind = crate::pie::PlayKind::parse(name).ok_or_else(|| {
            play_option_error(
                "play_mode_unknown",
                format!("`mode` must be \"play\" or \"simulate\", not {mode}"),
            )
        })?;
    }
    if let Some(players) = params.get("players").filter(|v| !v.is_null()) {
        options.players = players
            .as_u64()
            .and_then(|n| u8::try_from(n).ok())
            .filter(|n| (1..=crate::pie::MAX_PIE_PLAYERS).contains(n))
            .ok_or_else(|| {
                play_option_error(
                    "play_players_out_of_range",
                    format!(
                        "`players` must be a whole number 1..={}, not {players}",
                        crate::pie::MAX_PIE_PLAYERS
                    ),
                )
            })?;
    }
    options.resolution = play_resolution_param(params)?;
    crate::pie_controls::play_params(world, params, &mut options)?;
    let window = params.get("window").and_then(Value::as_bool) == Some(true);
    if options.resolution.is_some() && !window {
        return Err(play_option_error(
            crate::play_window::PLAY_RESOLUTION_NEEDS_WINDOW,
            "`preset` and `size` choose the play window's resolution: pass `window: true` too \
             (the viewport renders at its own size)",
        ));
    }
    if window {
        // A separate window needs a window system: the headless editor and CI have none.
        if !world.contains_resource::<crate::play_window::PlayWindowSupport>() {
            return Err(play_option_error(
                crate::play_window::PLAY_WINDOW_UNAVAILABLE,
                crate::play_window::NO_WINDOW_SYSTEM,
            ));
        }
        options.window = true;
    }
    Ok(options)
}

/// `editor.play`'s `preset` / `size` (ADR-7283): checked before the window system, so a bad value
/// is named on every host.
fn play_resolution_param(params: &Value) -> Result<Option<crate::play_window::PlayResolution>, RpcError> {
    use crate::play_window::{PLAY_RESOLUTION_INVALID, PlayResolution, RESOLUTION_PRESETS};
    let preset = params.get("preset").filter(|v| !v.is_null());
    let size = params.get("size").filter(|v| !v.is_null());
    match (preset, size) {
        (None, None) => Ok(None),
        (Some(_), Some(_)) => Err(play_option_error(
            PLAY_RESOLUTION_INVALID,
            "pass `preset` or `size`, not both",
        )),
        (Some(preset), None) => {
            let id = preset.as_str().unwrap_or("");
            PlayResolution::preset(id).map(Some).ok_or_else(|| {
                let ids: Vec<&str> = RESOLUTION_PRESETS.iter().map(|p| p.id).collect();
                play_option_error(
                    PLAY_RESOLUTION_INVALID,
                    format!(
                        "unknown play resolution preset {preset}; presets: {}",
                        ids.join(", ")
                    ),
                )
            })
        }
        (None, Some(size)) => {
            let sides: Option<Vec<u32>> = size.as_array().and_then(|items| {
                items
                    .iter()
                    .map(|v| v.as_u64().and_then(|n| u32::try_from(n).ok()))
                    .collect()
            });
            sides
                .filter(|s| s.len() == 2)
                .and_then(|s| PlayResolution::custom(s[0], s[1]))
                .map(Some)
                .ok_or_else(|| {
                    play_option_error(
                        PLAY_RESOLUTION_INVALID,
                        format!(
                            "`size` must be [width, height], whole numbers {}..={}, not {size}",
                            crate::play_window::MIN_RESOLUTION,
                            crate::play_window::MAX_RESOLUTION
                        ),
                    )
                })
        }
    }
}

/// `{ preset, size }` of a play resolution (`size` null for the window's own).
fn resolution_json(resolution: crate::play_window::PlayResolution) -> Value {
    json!({ "preset": resolution.preset_id(), "size": resolution.size() })
}

/// `editor.play`.
fn editor_play(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    // `separate_processes`: the Net tab's option, set before play starts (ADR-2682).
    if let Some(on) = params.get("separate_processes").and_then(Value::as_bool) {
        world
            .get_resource_or_insert_with::<crate::pie_net::PieNet>(Default::default)
            .separate_processes = on;
    }
    let options = play_options(world, params)?;
    // Device simulation (ADR-7829): checked before anything is queued.
    if let Some(device) = crate::play_device::device_param(params, options.window)? {
        world.insert_resource(device);
    }
    // Default options queue the plain request, as the toolbar's Play does.
    let request = if options == crate::pie::PlayOptions::default() {
        crate::pie::PieRequest::Play
    } else {
        crate::pie::PieRequest::PlayWith(options)
    };
    let mut answer = queue_pie(world, request)?;
    answer["mode"] = json!(options.kind.as_str());
    answer["players"] = json!(options.players);
    answer["window"] = json!(if options.window { "play_window" } else { "viewport" });
    // The session's own resolution, else the one the preferences give the next window.
    let resolution = options.resolution.unwrap_or_else(|| {
        world
            .get_resource::<crate::editor_prefs::EditorPreferences>()
            .map(crate::play_window::PlayResolution::from_prefs)
            .unwrap_or_default()
    });
    answer["resolution"] = resolution_json(resolution);
    crate::pie_controls::play_answer(world, &options, &mut answer);
    Ok(answer)
}

/// A split-screen pane in play-window pixels (ADR-7466): the rect the composite draws on the
/// `render_size` target (`regia_render::pixel_viewport`'s floor-and-clamp, the window path's
/// rule), scaled into the letterboxed `picture` (`[x, y, w, h]`) the target is shown in. At the
/// picture's own size (no scaling) it is exactly the drawn rect offset by the picture origin.
fn play_window_pane_rect(rect: [f32; 4], render_size: [u32; 2], picture: [u32; 4]) -> [u32; 4] {
    let [rw, rh] = [render_size[0].max(1), render_size[1].max(1)];
    let [px, py, pw, ph] = picture;
    let [x, y, w, h] = regia_render::pixel_viewport(rect, rw, rh);
    // Scale the edges (not the size) so neighboring panes share their border pixel.
    let scale = |at: u32, picture: u32, render: u32| {
        u32::try_from(u64::from(at) * u64::from(picture) / u64::from(render)).unwrap_or(picture)
    };
    let (x0, x1) = (scale(x, pw, rw), scale(x + w, pw, rw));
    let (y0, y1) = (scale(y, ph, rh), scale(y + h, ph, rh));
    [px + x0, py + y0, x1.saturating_sub(x0), y1.saturating_sub(y0)]
}

/// `editor.play_state`'s `play_window` (closed with the default size before any window opened).
fn play_window_json(world: &World) -> Value {
    let status = world
        .get_resource::<crate::play_window::PlayWindowStatus>()
        .cloned()
        .unwrap_or_else(|| {
            let geometry = crate::play_window::PlayWindowGeometry::default();
            let resolution = world
                .get_resource::<crate::editor_prefs::EditorPreferences>()
                .map(crate::play_window::PlayResolution::from_prefs)
                .unwrap_or_default();
            let size = [geometry.width, geometry.height];
            let render_size = resolution.render_size(size);
            crate::play_window::PlayWindowStatus {
                size,
                resolution,
                render_size,
                picture: crate::play_window::Letterbox::fit(render_size, size).to_array(),
                ..Default::default()
            }
        });
    // The session's split-screen players (ADR-7395): the play window composites one pane each
    // when it is open; the panes are the rects the composite draws (ADR-7466).
    let player_views = world
        .get_resource::<crate::pie::PieStatus>()
        .map_or_else(Vec::new, |status| status.player_views.clone());
    let panes: Vec<Value> = if status.open && player_views.len() > 1 {
        player_views
            .iter()
            .map(|view| {
                json!({
                    "player": view.player,
                    "rect": play_window_pane_rect(view.rect, status.render_size, status.picture),
                })
            })
            .collect()
    } else {
        Vec::new()
    };
    let counters = status.counters;
    json!({
        "open": status.open,
        "title": crate::play_window::PLAY_WINDOW_TITLE,
        "size": status.size,
        "position": status.position,
        "captured": status.captured,
        "error": status.error,
        "resolution": resolution_json(status.resolution),
        "render_size": status.render_size,
        "picture": status.picture,
        "players": player_views.len(),
        "panes": panes,
        "counters": {
            "windows_opened": counters.windows_opened,
            "windows_closed": counters.windows_closed,
            "surface_configures": counters.surface_configures,
            "surface_recreations": counters.surface_recreations,
            "frames_presented": counters.frames_presented,
            "frames_skipped": counters.frames_skipped,
        },
    })
}

/// `editor.keep_simulation_changes`.
fn editor_keep_simulation_changes(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let playing = world
        .get_resource::<crate::pie::PieStatus>()
        .is_some_and(|status| status.mode.is_some());
    if !playing {
        return Err(play_option_error(
            "not_playing",
            "keep simulation changes needs a running play session (editor.play first)",
        ));
    }
    let guids = match params.get("guids").filter(|v| !v.is_null()) {
        None => None,
        Some(list) => {
            let items = list
                .as_array()
                .ok_or_else(|| RpcError::invalid_params("`guids` must be an array of guid strings"))?;
            let mut guids = Vec::with_capacity(items.len());
            for item in items {
                let text = item.as_str().unwrap_or("");
                let guid = text
                    .parse::<regia_core::id::Guid>()
                    .map_err(|_| RpcError::invalid_params(format!("`guids`: {item} is not a guid")))?;
                guids.push(guid);
            }
            Some(guids)
        }
    };
    world.remove_resource::<crate::pie::LastKeep>();
    queue_pie(world, crate::pie::PieRequest::KeepSimulationChanges(guids))
}

/// `editor.pause`.
fn editor_pause(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    queue_pie(world, crate::pie::PieRequest::Pause)
}

/// `editor.resume`.
fn editor_resume(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    queue_pie(world, crate::pie::PieRequest::Resume)
}

/// `editor.stop`.
fn editor_stop(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    queue_pie(world, crate::pie::PieRequest::Stop)
}

/// `editor.step`.
fn editor_step(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let frames = step_frames(params)?;
    queue_pie(world, crate::pie::PieRequest::Step(frames))
}

/// `editor.play_state`.
fn editor_play_state(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let status = world
        .get_resource_or_insert_with::<crate::pie::PieStatus>(Default::default)
        .clone();
    Ok(json!({
        "mode": status.mode.map_or("stopped", crate::pie::PlayMode::as_str),
        "frames": status.frames,
        "entities": status.entities,
        "start_elapsed_ms": status.start_elapsed_ms,
        "game": status.game,
        "panic": status.panic,
        "kind": status.kind.map(crate::pie::PlayKind::as_str),
        "panic_backtrace": status.panic_backtrace,
        "window": status.window,
        "play_window": play_window_json(world),
        "players": status.player_views.len(),
        "viewports": status
            .player_views
            .iter()
            .map(|view| json!({ "player": view.player, "rect": view.rect }))
            .collect::<Vec<_>>(),
        "time_scale": status.time_scale,
        "muted": status.muted,
        "start": status.start.map(crate::pie_controls::start_json),
        "device": crate::play_device::device_state_json(world),
        "kept": world.get_resource::<crate::pie::LastKeep>().map(|kept| match kept {
            crate::pie::LastKeep::Kept(report) => json!({
                "entities": report.entities.iter().map(ToString::to_string).collect::<Vec<_>>(),
                "components": report.components,
                "copied": report.copied,
                "missing": report.missing.iter().map(ToString::to_string).collect::<Vec<_>>(),
                "undo_step": report.undo_step,
            }),
            crate::pie::LastKeep::Refused(err) => json!({ "error": err.code, "message": err.message }),
        }),
    }))
}

/// `editor.static_move_warnings`.
fn editor_static_move_warnings(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let warnings = world
        .get_resource_or_insert_with::<crate::pie::PieStatus>(Default::default)
        .clone()
        .static_move_warnings;
    Ok(json!({
        "count": warnings,
        "note": "Static entities moved at runtime since play started (specs/14 A1); \
                 remove the Static marker or stop moving them",
    }))
}

/// `editor.import`.
fn editor_import(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let sources = params
        .get("sources")
        .and_then(Value::as_array)
        .ok_or_else(|| RpcError::invalid_params("`sources` (array of file paths) is required"))?;
    if sources.is_empty() || sources.iter().any(|value| value.as_str().is_none()) {
        return Err(RpcError::invalid_params(
            "`sources` must be an array of file-path strings",
        ));
    }
    let sources: Vec<std::path::PathBuf> = sources
        .iter()
        .filter_map(Value::as_str)
        .map(std::path::PathBuf::from)
        .collect();
    let folder = params.get("folder").and_then(Value::as_str).unwrap_or("");
    let client = params.get("client").and_then(Value::as_str);
    let root = project_root(world)?;
    // The destination folder (the import trims a leading `/`) must stay in `assets/`, symlinks
    // included (ADR-1816). The sources are files from anywhere by design.
    let dest = folder.trim_start_matches('/');
    if !dest.is_empty() {
        assets_request_path(&root, "folder", dest)?;
    }
    if params.get("async").and_then(Value::as_bool) == Some(true) {
        // Extension importers run here (they need the world); the pipeline import is the job.
        let (rest, from_extensions) = crate::extension::host::run_importers(world, &root, folder, &sources);
        for (path, message) in &from_extensions.errors {
            tracing::error!(target: "regia_editor", "import {path}: {message}");
        }
        if rest.is_empty() {
            return Ok(json!({
                "imported": from_extensions.imported,
                "cooked": 0,
                "skipped": 0,
                "errors": from_extensions
                    .errors
                    .iter()
                    .map(|(path, message)| json!({ "path": path, "error": message }))
                    .collect::<Vec<_>>(),
            }));
        }
        return crate::editor_jobs::start_import_job(
            world,
            root,
            folder.to_string(),
            rest,
            client.map(str::to_string),
        );
    }
    // File types only an extension's importer takes (specs/57 § 3.3.2) go there first.
    match crate::extension::host::import_with_extensions(world, &root, folder, &sources, client) {
        Ok(report) => {
            for (path, message) in &report.errors {
                tracing::error!(target: "regia_editor", "import {path}: {message}");
            }
            let errors: Vec<Value> = report
                .errors
                .iter()
                .map(|(path, message)| json!({ "path": path, "error": message }))
                .collect();
            Ok(json!({
                "imported": report.imported,
                "cooked": report.cooked,
                "skipped": report.skipped,
                "errors": errors,
            }))
        }
        Err(message) => Err(RpcError::internal(format!("import failed: {message}"))),
    }
}

/// `editor.create_asset`.
fn editor_create_asset(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let kind = params
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            RpcError::invalid_params(
                "`kind` must be material, prefab, level, anim_graph, sound_cue, ui_layout, data_asset, \
                 material_graph, material_function, sequence, shot_list, attribute_set, effect, ability, material_instance, \
                 procgen_graph, vfx_system, conversation, curve_table, chooser, data_table, \
                 montage, cloth, soft_body, camera, smart_object, tilemap, motion_database, biome, logic_graph or folder",
            )
        })
        .and_then(|name| crate::content::NewAssetKind::parse(name).map_err(RpcError::invalid_params))?;
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`path` (project-relative, under assets/) is required"))?;
    let root = project_root(world)?;
    regia_agent::req_path::project_path_in(&root, "path", path)?;
    // A material instance needs its parent graph (spec 80 § 11.5).
    if kind == crate::content::NewAssetKind::MaterialInstance {
        let parent = params
            .get("parent")
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
            .ok_or_else(|| {
                RpcError::invalid_params(
                    "`parent` (the parent graph's project-relative .matgraph.ron path) is required for a material instance",
                )
            })?;
        let parent_rel = parent.replace('\\', "/");
        let parent_rel = parent_rel
            .trim_start_matches("assets/")
            .trim_matches('/')
            .to_string();
        regia_agent::req_path::project_path_in(&root, "parent", &format!("assets/{parent_rel}"))?;
        return match crate::content::create_material_instance(&root, path, &parent_rel) {
            Ok(created) => Ok(json!({ "path": created })),
            Err(message) => Err(RpcError::invalid_params(format!(
                "cannot create {path}: {message}"
            ))),
        };
    }
    let type_path = params.get("type").and_then(Value::as_str);
    let registry = world
        .get_resource::<regia_ecs::reflect::AppTypeRegistry>()
        .cloned();
    let guard = registry.as_ref().map(|r| r.read());
    match crate::content::create_asset_of_type(&root, kind, path, type_path, guard.as_deref()) {
        Ok(created) => Ok(json!({ "path": created })),
        Err(message) => Err(RpcError::invalid_params(format!(
            "cannot create {path}: {message}"
        ))),
    }
}

/// `editor.rename_asset`.
fn editor_rename_asset(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let from = params
        .get("from")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`from` (project-relative, under assets/) is required"))?;
    let to = params
        .get("to")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`to` (project-relative, under assets/) is required"))?;
    let root = project_root(world)?;
    regia_agent::req_path::project_path_in(&root, "from", from)?;
    regia_agent::req_path::project_path_in(&root, "to", to)?;
    // `move_asset` acts on `assets/<p>` when the prefix is left out: confine that path too, so
    // a symlink under `assets/` cannot lead the move out of the project (review rev-kimi-9 #8).
    for (key, rel) in [("from", from), ("to", to)] {
        let in_assets = regia_asset::project_ops::asset_project_path(rel).map_err(|e| {
            with_reason(
                RpcError::invalid_params(format!("cannot move: {e}")),
                "asset_move_invalid_path",
            )
        })?;
        regia_agent::req_path::project_path_in(&root, key, &in_assets)?;
    }
    match regia_asset::project_ops::move_asset(&root, from, to) {
        Ok(outcome) => {
            crate::content::push_move_undo(world, root, from.to_string(), to.to_string());
            crate::content_index::invalidate(world);
            Ok(json!({
                "from": from,
                "to": to,
                "fixed_referencers": outcome.fixed_referencers,
            }))
        }
        Err(regia_asset::project_ops::MoveError::SourceMissing(path)) => Err(RpcError::not_found(
            format!("cannot move {path}: the source does not exist"),
            "list the project with editor.list_assets and check the path",
        )),
        Err(regia_asset::project_ops::MoveError::DestinationExists(path)) => Err(with_reason(
            RpcError::invalid_params(format!("cannot move to {path}: the destination already exists")),
            "asset_move_destination_exists",
        )),
        Err(regia_asset::project_ops::MoveError::InvalidPath(path)) => Err(with_reason(
            RpcError::invalid_params(format!("cannot move: invalid asset path {path}")),
            "asset_move_invalid_path",
        )),
        Err(regia_asset::project_ops::MoveError::Io { message, .. }) => {
            Err(RpcError::internal(format!("move failed: {message}")))
        }
    }
}

/// `editor.list_assets`.
fn editor_list_assets(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let root = project_root(world)?;
    let mut state = crate::content::ContentBrowserState {
        project_root: Some(root),
        search: params
            .get("search")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        ..Default::default()
    };
    // The schema's default is the whole tree. A folder narrows to that folder's direct
    // children, matching the content browser; search still matches everywhere.
    let folder = params
        .get("folder")
        .and_then(Value::as_str)
        .filter(|folder| !folder.is_empty())
        .map(|folder| folder.replace('\\', "/"));
    let assets = if let Some(folder) = folder {
        state.folder = folder;
        state.visible_assets()
    } else {
        state.assets()
    };
    let out: Vec<Value> = assets
        .iter()
        .map(|(path, kind)| json!({ "path": path, "kind": crate::dnd::kind_name(*kind) }))
        .collect();
    let count = out.len();
    Ok(json!({ "assets": out, "count": count }))
}

/// `editor.spawn_prefab`.
fn editor_spawn_prefab(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`path` (project-relative .prefab.ron) is required"))?;
    let translation = vec3_param(params, "translation")?.unwrap_or_default();
    let root = project_root(world)?;
    let full = assets_request_path(&root, "path", path)?;
    match crate::content::spawn_prefab_asset(world, &full, translation) {
        Ok(entity) => Ok(json!({ "path": path, "entity": entity.to_bits() })),
        Err(message) => Err(RpcError::internal(format!("prefab spawn failed: {message}"))),
    }
}

/// `editor.open_prefab`.
fn editor_open_prefab(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`path` (project-relative .prefab.ron) is required"))?;
    let root = project_root(world)?;
    let full = assets_request_path(&root, "path", path)?;
    match crate::prefab_edit::enter_prefab_edit(world, &full) {
        Ok(()) => Ok(json!({ "editing": path })),
        Err(message) => Err(RpcError::invalid_params(format!(
            "cannot open prefab {path}: {message}"
        ))),
    }
}

/// `editor.save_prefab`.
fn editor_save_prefab(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    match crate::prefab_edit::save_prefab(world) {
        Ok(updated) => Ok(json!({ "saved": true, "instances_updated": updated })),
        Err(message) => Err(RpcError::internal(format!("prefab save failed: {message}"))),
    }
}

/// `editor.close_prefab`.
fn editor_close_prefab(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let save = params.get("save").and_then(Value::as_bool).unwrap_or(true);
    match crate::prefab_edit::exit_prefab_edit(world, save) {
        Ok(()) => Ok(json!({ "closed": true })),
        Err(message) => Err(RpcError::internal(format!("prefab close failed: {message}"))),
    }
}

/// `editor.new_project`.
fn editor_new_project(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`name` is required"))?;
    let template = params.get("template").and_then(Value::as_str).unwrap_or("blank");
    let parent = params.get("path").and_then(Value::as_str).unwrap_or(".");
    match crate::project::create_and_open(world, name, std::path::Path::new(parent), template) {
        Ok(opened) => Ok(opened_json(&opened)),
        Err(message) => Err(RpcError::invalid_params(message)),
    }
}

/// `editor.open_project`.
fn editor_open_project(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`path` (project directory) is required"))?;
    match crate::project::open_project(world, std::path::Path::new(path)) {
        Ok(opened) => Ok(opened_json(&opened)),
        Err(message) => Err(RpcError::invalid_params(message)),
    }
}

/// `editor.recent_projects`.
fn editor_recent_projects(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    crate::recent_meta::recent_projects_json(params)
}

/// `editor.create_script`.
fn editor_create_script(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`name` is required"))?;
    let template = match params.get("template").and_then(Value::as_str) {
        Some(name) => crate::creators::ScriptTemplate::parse(name)
            .map_err(|e| e.rpc("templates: empty, component (see editor.script_wizard)"))?,
        None => crate::creators::ScriptTemplate::Empty,
    };
    let root = project_root(world)?;
    match crate::scripts::create_script_from(&root, name, template) {
        Ok(path) => Ok(json!({ "path": path.display().to_string() })),
        Err(message) => Err(RpcError::invalid_params(message)),
    }
}

/// `editor.edit_material`.
///
/// Values are checked before anything is written: colour channels, metallic and roughness must be
/// finite and within `0..=1`. When the Material panel has the same file open with unsaved edits the
/// call is refused (`material_unsaved_in_panel`): writing the file would be overwritten by the
/// panel's next save. When the panel has it open and clean, the panel adopts the saved material as
/// one undo step (audit v033b: the call used to edit a private copy the panel never saw).
fn editor_edit_material(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`path` (project-relative .material.ron) is required"))?;
    let root = project_root(world)?;
    let full = assets_request_path(&root, "path", path)?;
    let unit = |name: &str, item: &Value| -> Result<f32, RpcError> {
        let value = finite_f32(item, name)?;
        if (0.0..=1.0).contains(&value) {
            Ok(value)
        } else {
            Err(RpcError::invalid_params(format!(
                "`{name}` must be within 0..=1, got {value}"
            )))
        }
    };
    let color = match params.get("base_color") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) if items.len() <= 4 => Some(
            items
                .iter()
                .map(|item| unit("base_color", item))
                .collect::<Result<Vec<f32>, _>>()?,
        ),
        Some(_) => {
            return Err(RpcError::invalid_params(
                "`base_color` must be an array of up to 4 numbers",
            ));
        }
    };
    let metallic = params
        .get("metallic")
        .filter(|v| !v.is_null())
        .map(|v| unit("metallic", v))
        .transpose()?;
    let roughness = params
        .get("roughness")
        .filter(|v| !v.is_null())
        .map(|v| unit("roughness", v))
        .transpose()?;
    // The panel's emissive colour button and brightness boost (ADR-7747): linear RGB, boost <= 64.
    let emissive = match params.get("emissive") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) if items.len() <= 3 => Some(
            items
                .iter()
                .map(|item| {
                    let value = finite_f32(item, "emissive")?;
                    if (0.0..=64.0).contains(&value) {
                        Ok(value)
                    } else {
                        Err(RpcError::invalid_params(format!(
                            "`emissive` channels must be within 0..=64, got {value}"
                        )))
                    }
                })
                .collect::<Result<Vec<f32>, _>>()?,
        ),
        Some(_) => {
            return Err(RpcError::invalid_params(
                "`emissive` must be an array of up to 3 numbers",
            ));
        }
    };
    let alpha_mode = match params.get("alpha_mode") {
        None | Some(Value::Null) => None,
        Some(Value::String(mode)) => Some(match mode.as_str() {
            "opaque" => regia_asset::AlphaMode::Opaque,
            "mask" => regia_asset::AlphaMode::Mask,
            "blend" => regia_asset::AlphaMode::Blend,
            other => {
                return Err(RpcError::invalid_params(format!(
                    "`alpha_mode` must be \"opaque\", \"mask\" or \"blend\", got {other:?}"
                )));
            }
        }),
        Some(other) => {
            return Err(RpcError::invalid_params(format!(
                "`alpha_mode` must be a string, got {other}"
            )));
        }
    };
    let panel = world
        .get_resource::<crate::material_editor::MaterialPanelStatus>()
        .cloned()
        .unwrap_or_default();
    let panel_has_it = panel
        .path
        .as_deref()
        .is_some_and(|open| crate::material_editor::same_material_file(open, &full));
    if panel_has_it && panel.dirty {
        let mut err = RpcError::invalid_params(format!(
            "{path} has unsaved edits in the Material panel: save or undo them there first"
        ));
        if let Some(data) = err.data.as_mut() {
            data["material_error"] = json!("material_unsaved_in_panel");
        }
        return Err(err);
    }
    let mut editor = crate::material_editor::MaterialEditorState::default();
    editor.open(&full).map_err(RpcError::invalid_params)?;
    editor.edit(|material| {
        if let Some(color) = &color {
            for (channel, value) in material.base.base_color.iter_mut().zip(color) {
                *channel = *value;
            }
        }
        if let Some(value) = metallic {
            material.base.metallic = value;
        }
        if let Some(value) = roughness {
            material.base.roughness = value;
        }
        if let Some(emissive) = &emissive {
            for (channel, value) in material.base.emissive.iter_mut().zip(emissive) {
                *channel = *value;
            }
        }
        if let Some(mode) = alpha_mode {
            material.render_state.alpha_mode = mode;
        }
    });
    editor
        .save()
        .map_err(|message| RpcError::internal(format!("material save failed: {message}")))?;
    if panel_has_it {
        world.insert_resource(crate::material_editor::MaterialPanelReload {
            path: full.clone(),
            material: editor.material.clone(),
        });
    }
    Ok(json!({
        "path": path,
        "base_color": editor.material.base.base_color,
        "metallic": editor.material.base.metallic,
        "roughness": editor.material.base.roughness,
        "emissive": editor.material.base.emissive,
        "alpha_mode": format!("{:?}", editor.material.render_state.alpha_mode).to_lowercase(),
    }))
}

/// Shared result of opening a project.
fn opened_json(opened: &crate::project::OpenedProject) -> Value {
    json!({
        "root": opened.root.display().to_string(),
        "warning": opened.warning,
    })
}

/// `editor.prefab_mode`.
fn editor_prefab_mode(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let editing = world
        .get_resource::<crate::prefab_edit::PrefabEditMode>()
        .and_then(|mode| mode.editing().map(|p| p.display().to_string()));
    // Unsaved edits in the open prefab (ADR-7382): what the autosave keeps and a save writes.
    let dirty = crate::prefab_edit::is_dirty(world);
    // Opened in place (`editor.prefab_edit_in_place`): the level is drawn around it, read-only.
    let in_context = world
        .get_resource::<crate::prefab_edit::PrefabEditMode>()
        .is_some_and(crate::prefab_edit::PrefabEditMode::in_context);
    Ok(json!({ "editing": editing, "dirty": dirty, "in_context": in_context }))
}

/// The project root from the editor's resources.
pub(crate) fn project_root(world: &World) -> Result<std::path::PathBuf, RpcError> {
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

/// The root agent paths resolve against: the open project, else the agent's project root (the
/// asset server's root or the working directory).
pub(crate) fn agent_root(world: &World) -> std::path::PathBuf {
    world
        .get_resource::<EditorProject>()
        .and_then(|p| p.root.clone())
        .unwrap_or_else(|| regia_agent::req_path::project_root(world))
}

/// A request path under the agent's allowed roots (project, temp dir, working directory,
/// `output_roots`), relative paths joined to [`agent_root`]; `path_outside_project` otherwise
/// (ADR-1816).
pub(crate) fn allowed_request_path(
    world: &World,
    key: &str,
    path: &str,
) -> Result<std::path::PathBuf, RpcError> {
    regia_agent::req_path::allowed_path_in(world, &agent_root(world), key, path)
}

/// The one rule for a request path naming a document an editor tool opens, reads or saves
/// (ADR-7388): relative under [`agent_root`], absolute only under it or the temp directory, never
/// `..`, a drive letter, a UNC form, NUL or a symlink leading out (`path_outside_project`). Output
/// paths (`out`) keep [`allowed_request_path`].
pub(crate) fn document_request_path(
    world: &World,
    key: &str,
    path: &str,
) -> Result<std::path::PathBuf, RpcError> {
    regia_agent::req_path::document_path_in(&agent_root(world), key, path)
}

/// A request path naming a file under the project's `assets/` (an optional `assets/` prefix is
/// dropped): absolute, drive, UNC, `..` and symlinked paths are `path_outside_project` (ADR-1816).
///
/// The caller's own spelling is judged before `assets/` is prefixed, so `/tmp/x` is refused instead
/// of resolving to `assets/tmp/x` (review w8-outliner-place): the one rule of
/// `asset_ops_agent::assets_request_path`, which this delegates to.
pub(crate) fn assets_request_path(
    root: &std::path::Path,
    key: &str,
    path: &str,
) -> Result<std::path::PathBuf, RpcError> {
    crate::asset_ops_agent::assets_request_path(root, key, path)
}

/// The project root the editor runs in (content browser + save paths).
#[derive(regia_ecs::prelude::Resource, Debug, Clone)]
pub struct EditorProject {
    /// The project directory (contains `assets/`).
    pub root: Option<std::path::PathBuf>,
}

/// Typed (non-JSON) entry points over the same handlers the protocol dispatches to — used by
/// the editor integration tests. Not a stable API.
#[doc(hidden)]
pub mod test_support {
    use super::*;
    use crate::command::{CommandError, SetProperty, SetTransform};

    /// `editor.select`.
    pub fn select(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        editor_select(world, params)
    }

    /// `editor.set_transform`.
    pub fn set_transform(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        editor_set_transform(world, params)
    }

    /// `editor.set_property`.
    pub fn set_property(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        editor_set_property(world, params)
    }

    /// `editor.inspect_selection`.
    pub fn inspect_selection(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        editor_inspect_selection(world, params)
    }

    /// `editor.undo` → whether anything was undone.
    pub fn undo(world: &mut World) -> bool {
        editor_undo(world, &json!({}))
            .map(|v| v["undone"].as_bool().unwrap_or(false))
            .unwrap_or(false)
    }

    /// `editor.redo` → whether anything was redone.
    pub fn redo(world: &mut World) -> bool {
        editor_redo(world, &json!({}))
            .map(|v| v["redone"].as_bool().unwrap_or(false))
            .unwrap_or(false)
    }

    /// `editor.save_level`.
    pub fn save_level(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        editor_save_level(world, params)
    }

    /// `editor.play` → queue the request.
    pub fn play(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        editor_play(world, params)
    }

    /// `editor.pause` → queue the request.
    pub fn pause(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        editor_pause(world, params)
    }

    /// `editor.resume` → queue the request.
    pub fn resume(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        editor_resume(world, params)
    }

    /// `editor.stop` → queue the request.
    pub fn stop(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        editor_stop(world, params)
    }

    /// `editor.step` → queue the request.
    pub fn step(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        editor_step(world, params)
    }

    /// `editor.play_state` → the current status.
    pub fn play_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        editor_play_state(world, params)
    }

    /// `editor.static_move_warnings` → the play world's Static-move counter.
    pub fn static_move_warnings(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        editor_static_move_warnings(world, params)
    }

    /// `editor.import`.
    pub fn import(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        editor_import(world, params)
    }

    /// `editor.create_asset`.
    pub fn create_asset(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        editor_create_asset(world, params)
    }

    /// `editor.list_assets`.
    pub fn list_assets(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        editor_list_assets(world, params)
    }

    /// `editor.spawn_prefab`.
    pub fn spawn_prefab(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        editor_spawn_prefab(world, params)
    }

    /// `editor.new_project`.
    pub fn new_project(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        editor_new_project(world, params)
    }

    /// `editor.open_project`.
    pub fn open_project(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        editor_open_project(world, params)
    }

    /// `editor.recent_projects`.
    pub fn recent_projects(world: &mut World) -> Result<Value, RpcError> {
        editor_recent_projects(world, &json!({}))
    }

    /// `editor.create_script`.
    pub fn create_script(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        editor_create_script(world, params)
    }

    /// `editor.edit_material`.
    pub fn edit_material(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        editor_edit_material(world, params)
    }

    /// `editor.open_prefab`.
    pub fn open_prefab(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        editor_open_prefab(world, params)
    }

    /// `editor.save_prefab`.
    pub fn save_prefab(world: &mut World) -> Result<Value, RpcError> {
        editor_save_prefab(world, &json!({}))
    }

    /// `editor.close_prefab`.
    pub fn close_prefab(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        editor_close_prefab(world, params)
    }

    /// Builds (does not apply) a `SetProperty` command — the inspector's edit path.
    ///
    /// # Errors
    /// See [`CommandError`].
    pub fn capture_set_property(
        world: &World,
        entity: Entity,
        component: &str,
        path: &str,
        value: Box<dyn PartialReflect>,
    ) -> Result<SetProperty, CommandError> {
        SetProperty::capture(world, entity, component, path, value)
    }

    /// Builds (does not apply) a `SetTransform` command — the gizmo's edit path.
    #[must_use]
    pub fn capture_set_transform(
        world: &World,
        entity: Entity,
        new: regia_scene::prelude::Transform,
    ) -> SetTransform {
        SetTransform::capture(world, entity, new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_app::App;
    use regia_ecs::reflect::Reflect;
    use regia_scene::prelude::{Name, Transform};

    #[test]
    fn step_frames_rejects_instead_of_silently_clamping() {
        assert!(step_frames(&json!({})).is_ok_and(|n| n == 1));
        assert!(step_frames(&json!({ "frames": 20 })).is_ok_and(|n| n == 20));
        assert!(step_frames(&json!({ "frames": 10_000 })).is_ok_and(|n| n == 10_000));
        for bad in [json!(20_000), json!(30.5), json!(0), json!(-1), json!("5")] {
            assert!(step_frames(&json!({ "frames": bad })).is_err(), "{bad}");
        }
    }

    fn app() -> App {
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        app.world_mut().spawn((Name::new("Rock"), Transform::default()));
        app
    }

    /// sprint-w13-ui-live-bugs (2): `editor.select` in the default "set" mode keeps every id of the
    /// list (it kept only the last), the last is primary, a repeated id is listed once, and "add"
    /// and "toggle" behave as before.
    #[test]
    fn editor_select_set_keeps_every_id() {
        let mut app = app();
        let world = app.world_mut();
        let ids: Vec<u64> = (0..3)
            .map(|i| {
                world
                    .spawn((Name::new(format!("Prop{i}")), Transform::default()))
                    .id()
                    .to_bits()
            })
            .collect();
        let ids_of = |out: &Value| -> Vec<u64> {
            out["entities"]
                .as_array()
                .expect("entities")
                .iter()
                .map(|e| e["entity"].as_u64().expect("id"))
                .collect()
        };
        let out = editor_select(world, &json!({ "entities": [ids[0], ids[1], ids[2]] })).expect("set");
        assert_eq!(ids_of(&out), ids, "every id stays selected: {out}");
        assert_eq!(out["primary"], json!(ids[2]), "the last id is primary");
        assert_eq!(world.resource::<Selection>().len(), 3);

        // A new "set" replaces the old list; a repeated id is listed once.
        let out = editor_select(world, &json!({ "entities": [ids[1], ids[0], ids[1]] })).expect("set");
        assert_eq!(ids_of(&out), vec![ids[1], ids[0]], "{out}");
        assert_eq!(out["primary"], json!(ids[1]));

        // "add" extends, "toggle" flips each id, and `[]` clears.
        let out = editor_select(world, &json!({ "entities": [ids[2]], "mode": "add" })).expect("add");
        assert_eq!(ids_of(&out), vec![ids[1], ids[0], ids[2]]);
        let out =
            editor_select(world, &json!({ "entities": [ids[0], ids[1]], "mode": "toggle" })).expect("toggle");
        assert_eq!(ids_of(&out), vec![ids[2]]);
        let out = editor_select(world, &json!({ "entities": [] })).expect("clear");
        assert!(ids_of(&out).is_empty());
    }

    /// `editor.rename_asset` through the agent surface: the move keeps references, lands on the
    /// undo stack as one file-only step, and `editor.undo` moves everything back (spec 12 § 9).
    #[test]
    fn rename_asset_through_the_surface_fixes_references_and_undoes() {
        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let dir = regia_test_utils::unique_temp_dir("editor-agent-rename");
        std::fs::create_dir_all(dir.join("assets/props")).expect("dir");
        std::fs::write(dir.join("assets/props/barrel.prefab.ron"), "(prefab)").expect("prefab");
        std::fs::write(
            dir.join("assets/maps.level.ron"),
            "(barrel: \"props/barrel.prefab.ron\")",
        )
        .expect("level");
        app.world_mut().insert_resource(EditorProject {
            root: Some(dir.clone()),
        });

        // Through the registration, so the method name the agent calls is the one tested.
        let (_, rename) = all_methods()
            .into_iter()
            .find(|(schema, _)| schema.name == "editor.rename_asset")
            .expect("editor.rename_asset is registered");
        let out = rename(
            app.world_mut(),
            &json!({ "from": "assets/props/barrel.prefab.ron", "to": "assets/models/barrel.prefab.ron" }),
        )
        .expect("rename");
        assert_eq!(out["from"], "assets/props/barrel.prefab.ron");
        assert_eq!(out["to"], "assets/models/barrel.prefab.ron");
        assert_eq!(
            out["fixed_referencers"].as_array().expect("fixed"),
            &vec![serde_json::json!("assets/maps.level.ron")]
        );
        assert!(dir.join("assets/models/barrel.prefab.ron").exists());
        assert!(!dir.join("assets/props/barrel.prefab.ron").exists());
        assert!(
            std::fs::read_to_string(dir.join("assets/maps.level.ron"))
                .expect("level")
                .contains("models/barrel.prefab.ron")
        );

        // One undo step, described as a move; undoing moves the file back and restores the path.
        let undo = editor_undo(app.world_mut(), &json!({})).expect("undo");
        assert_eq!(undo["undone"], true);
        assert!(
            undo["description"]
                .as_str()
                .expect("description")
                .contains("Move"),
            "{undo}"
        );
        assert!(dir.join("assets/props/barrel.prefab.ron").exists());
        assert!(
            std::fs::read_to_string(dir.join("assets/maps.level.ron"))
                .expect("level")
                .contains("props/barrel.prefab.ron")
        );

        // The refused cases keep their error codes (book/agents/content-browser.md).
        assert!(
            editor_rename_asset(
                app.world_mut(),
                &json!({ "from": "assets/ghost.png", "to": "assets/other.png" })
            )
            .is_err()
        );
        assert!(
            editor_rename_asset(
                app.world_mut(),
                &json!({ "from": "assets/props/barrel.prefab.ron", "to": "../escape.png" })
            )
            .is_err()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review rev-kimi-9 findings 8 and 9: `editor.rename_asset` checks the path it actually
    /// moves (`assets/<p>` when the prefix is left out), so a symlink under `assets/` cannot carry
    /// the file out of the project, and every refusal names its `data.reason`.
    #[test]
    fn rename_asset_checks_the_moved_path_and_names_each_refusal() {
        let mut app = App::new();
        let dir = regia_test_utils::unique_temp_dir("editor-agent-rename-refusals");
        std::fs::create_dir_all(dir.join("assets/props")).expect("dir");
        std::fs::write(dir.join("assets/props/barrel.prefab.ron"), "(prefab)").expect("prefab");
        std::fs::write(dir.join("assets/props/crate.prefab.ron"), "(prefab)").expect("prefab");
        app.world_mut().insert_resource(EditorProject {
            root: Some(dir.clone()),
        });
        let data = |e: &RpcError, key: &str| {
            e.data
                .as_ref()
                .and_then(|d| d.get(key))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };

        #[cfg(unix)]
        {
            let outside = regia_test_utils::unique_temp_dir("editor-agent-rename-outside");
            std::os::unix::fs::symlink(&outside, dir.join("assets/evil")).expect("link");
            let err = editor_rename_asset(
                app.world_mut(),
                &json!({ "from": "props/barrel.prefab.ron", "to": "evil/barrel.prefab.ron" }),
            )
            .expect_err("a symlink under assets/ leads out of the project");
            assert_eq!(
                data(&err, "reason").as_deref(),
                Some("path_outside_project"),
                "{err:?}"
            );
            assert!(
                dir.join("assets/props/barrel.prefab.ron").is_file(),
                "nothing moved"
            );
            assert!(
                !outside.join("barrel.prefab.ron").exists(),
                "nothing left the project"
            );
            let _ = std::fs::remove_dir_all(&outside);
        }

        let err = editor_rename_asset(
            app.world_mut(),
            &json!({ "from": "assets/props/barrel.prefab.ron", "to": "assets/props/crate.prefab.ron" }),
        )
        .expect_err("taken");
        assert_eq!(data(&err, "code").as_deref(), Some("F0002"), "{err:?}");
        assert_eq!(
            data(&err, "reason").as_deref(),
            Some("asset_move_destination_exists"),
            "{err:?}"
        );

        let err = editor_rename_asset(
            app.world_mut(),
            &json!({ "from": "assets/props", "to": "assets/props/sub" }),
        )
        .expect_err("into itself");
        assert_eq!(data(&err, "code").as_deref(), Some("F0002"), "{err:?}");
        assert_eq!(
            data(&err, "reason").as_deref(),
            Some("asset_move_invalid_path"),
            "{err:?}"
        );

        let err = editor_rename_asset(
            app.world_mut(),
            &json!({ "from": "assets/props/barrel.prefab.ron", "to": "../escape.prefab.ron" }),
        )
        .expect_err("escape");
        assert_eq!(data(&err, "code").as_deref(), Some("F0002"), "{err:?}");
        assert_eq!(
            data(&err, "reason").as_deref(),
            Some("path_outside_project"),
            "{err:?}"
        );

        let err = editor_rename_asset(
            app.world_mut(),
            &json!({ "from": "assets/ghost.png", "to": "assets/other.png" }),
        )
        .expect_err("missing");
        assert_eq!(data(&err, "code").as_deref(), Some("F0003"), "{err:?}");
        assert!(dir.join("assets/props/barrel.prefab.ron").is_file());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The anim panel captures at a documented default of 1100 px and the offscreen renderer
    /// allows up to 4096 (`anim::capture::render_ui_offscreen`), so `capture_dim` must not clamp
    /// the default to 1024.
    #[test]
    fn capture_dim_reaches_the_documented_anim_graph_default() {
        assert_eq!(capture_dim(&json!({}), "width", 1100), 1100);
        assert_eq!(capture_dim(&json!({ "width": 1100 }), "width", 1100), 1100);
        assert_eq!(capture_dim(&json!({ "width": 99_999 }), "width", 1100), 4096);
        assert_eq!(capture_dim(&json!({ "width": 1 }), "width", 1100), 64);
    }

    /// integ-v048e (ADR-5922): `editor.static_move_warnings` (fixrev4-static-moves) answers the
    /// play world's Static-move counter through the registered method, 0 before play.
    #[test]
    fn static_move_warnings_reports_the_play_counter_by_method_name() {
        let handler = all_methods()
            .into_iter()
            .find(|(schema, _)| schema.name == "editor.static_move_warnings")
            .map(|(_, handler)| handler)
            .expect("editor.static_move_warnings is registered");
        let mut app = app();
        let answer = handler(app.world_mut(), &json!({})).expect("answer");
        assert_eq!(answer["count"], json!(0), "{answer}");
        app.world_mut()
            .resource_mut::<crate::pie::PieStatus>()
            .static_move_warnings = 3;
        let answer = handler(app.world_mut(), &json!({})).expect("answer");
        assert_eq!(answer["count"], json!(3), "{answer}");
        assert!(answer["note"].as_str().is_some_and(|n| n.contains("Static")));
    }

    #[test]
    fn installing_the_editor_surface_registers_the_editor_tools_marker() {
        let mut app = app();
        install(app.world_mut());
        let extension = app.world().resource::<regia_agent::extension::AgentExtension>();
        assert!(
            extension
                .capabilities()
                .iter()
                .any(|(name, _)| *name == "editor_tools"),
            "{:?}",
            extension.capabilities()
        );
    }

    fn rock(world: &World) -> Entity {
        for entity_ref in world.iter_entities() {
            let entity = entity_ref.id();
            if let Some(name) = world.get::<Name>(entity)
                && name.as_str() == "Rock"
            {
                return entity;
            }
        }
        panic!("rock not found");
    }

    #[test]
    fn editor_internal_entities_are_not_selectable() {
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let window = app.world_mut().spawn(regia_platform::PrimaryWindow).id();
        crate::visibility::mark_internal_entities(app.world_mut());
        let err = editor_select(
            app.world_mut(),
            &json!({ "entities": [window.to_bits()], "mode": "set" }),
        )
        .expect_err("editor-internal entities are rejected");
        assert!(
            err.message.contains("editor-internal"),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn select_and_read_back() {
        let mut app = app();
        let rock = rock(app.world());
        let out = editor_select(
            app.world_mut(),
            &json!({ "entities": [rock.to_bits()], "mode": "set" }),
        )
        .expect("select");
        assert_eq!(out["primary"], rock.to_bits());

        let got = editor_get_selection(app.world_mut(), &json!({})).expect("get");
        assert_eq!(got["primary"], rock.to_bits());

        // Invalid mode and dead entities are clean errors.
        assert!(
            editor_select(
                app.world_mut(),
                &json!({ "entities": [rock.to_bits()], "mode": "warp" })
            )
            .is_err()
        );
        assert!(
            editor_select(
                app.world_mut(),
                &json!({ "entities": [1u64 << 40], "mode": "set" })
            )
            .is_err()
        );
    }

    #[test]
    fn set_transform_is_undoable() {
        let mut app = app();
        let rock = rock(app.world());
        let before = app.world().get::<Transform>(rock).expect("t").translation;

        let out = editor_set_transform(
            app.world_mut(),
            &json!({
                "entity": rock.to_bits(),
                "translation": [1.0, 2.0, 3.0],
                "rotation": [0.0, 0.0, 0.0, 1.0],
            }),
        )
        .expect("set");
        assert_eq!(out["undoable"], 1);
        assert_eq!(
            app.world().get::<Transform>(rock).expect("t").translation,
            regia_core::math::Vec3::new(1.0, 2.0, 3.0)
        );

        let undone = editor_undo(app.world_mut(), &json!({})).expect("undo");
        assert_eq!(undone["undone"], true);
        assert_eq!(
            app.world().get::<Transform>(rock).expect("t").translation,
            before,
            "undo restored the transform"
        );

        let redone = editor_redo(app.world_mut(), &json!({})).expect("redo");
        assert_eq!(redone["redone"], true);
        assert_eq!(
            app.world().get::<Transform>(rock).expect("t").translation,
            regia_core::math::Vec3::new(1.0, 2.0, 3.0)
        );
    }

    /// Two calls on the same entity are two steps (a `SetTransform` merges same-entity edits,
    /// which used to fold the second call into the first while reporting depth 2).
    #[test]
    fn set_transform_calls_are_discrete_steps() {
        let mut app = app();
        let rock = rock(app.world());
        let before = app.world().get::<Transform>(rock).expect("t").translation;
        for (x, depth) in [(1.0, 1), (2.0, 2)] {
            let out = editor_set_transform(
                app.world_mut(),
                &json!({ "entity": rock.to_bits(), "translation": [x, 0.0, 0.0] }),
            )
            .expect("set");
            assert_eq!(out["undoable"], depth, "{out}");
        }
        assert_eq!(app.world().resource::<UndoStack>().undo_depth(), 2);
        let undone = editor_undo(app.world_mut(), &json!({})).expect("undo");
        assert_eq!(undone["description"], "Transform gizmo");
        assert_eq!(
            app.world().get::<Transform>(rock).expect("t").translation.x,
            1.0,
            "one undo steps back one call"
        );
        editor_undo(app.world_mut(), &json!({})).expect("undo");
        assert_eq!(app.world().get::<Transform>(rock).expect("t").translation, before);
    }

    #[test]
    fn two_agent_transform_edits_are_two_undo_steps() {
        let mut app = app();
        let rock = rock(app.world());
        let before = app.world().get::<Transform>(rock).expect("t").translation;
        let set = |app: &mut App, x: f32| {
            editor_set_transform(
                app.world_mut(),
                &json!({ "entity": rock.to_bits(), "translation": [x, 0.0, 0.0] }),
            )
            .expect("set")
        };
        assert_eq!(set(&mut app, 1.0)["undoable"], 1);
        assert_eq!(set(&mut app, 2.0)["undoable"], 2);
        editor_undo(app.world_mut(), &json!({})).expect("undo");
        assert_eq!(
            app.world().get::<Transform>(rock).expect("t").translation,
            regia_core::math::Vec3::new(1.0, 0.0, 0.0)
        );
        editor_undo(app.world_mut(), &json!({})).expect("undo");
        assert_eq!(app.world().get::<Transform>(rock).expect("t").translation, before);
    }

    #[test]
    fn gizmo_mode_round_trip() {
        let mut app = app();
        let out = editor_set_gizmo(app.world_mut(), &json!({ "mode": "rotate", "snap": true })).expect("set");
        assert_eq!(out["mode"], "rotate");
        assert_eq!(out["snap"], true);
        assert!(editor_set_gizmo(app.world_mut(), &json!({ "mode": "warp" })).is_err());
    }

    #[test]
    fn save_level_reports_the_path() {
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let open = crate::level_io::new_level(app.world_mut());
        app.world_mut().insert_resource(open.clone());
        let dir = std::env::temp_dir().join(format!("regia-editor-agent-{}-{}", line!(), std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("save.level.ron");
        let out =
            editor_save_level(app.world_mut(), &json!({ "path": path.display().to_string() })).expect("save");
        assert_eq!(out["path"], path.display().to_string());
        assert!(path.is_file(), "the level file exists");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn viewport_settings_round_trip() {
        let mut app = app();
        let out = crate::viewport_agent::set_viewport(
            app.world_mut(),
            &json!({ "realtime": false, "grid": false }),
        )
        .expect("set");
        assert_eq!(out["realtime"], false);
        assert_eq!(out["grid"], false);
        assert_eq!(out["labels"], true, "untouched settings persist");
        assert!(crate::viewport_agent::set_viewport(app.world_mut(), &json!({})).is_ok());
        // The collider and light-radius overlays (ADR-7217): off until switched on.
        let out = crate::viewport_agent::set_viewport(app.world_mut(), &json!({})).expect("read");
        assert_eq!(
            (out["colliders"].clone(), out["light_radii"].clone()),
            (json!(false), json!(false))
        );
        let out = crate::viewport_agent::set_viewport(
            app.world_mut(),
            &json!({ "colliders": true, "light_radii": true }),
        )
        .expect("set");
        assert_eq!(
            (out["colliders"].clone(), out["light_radii"].clone()),
            (json!(true), json!(true))
        );
        let show = app.world().resource::<crate::views::ViewportLayout>().show;
        assert!(show.colliders && show.light_radii);
    }

    #[test]
    fn overlay_blur_is_off_by_default_and_validated() {
        let mut app = app();
        let set = crate::viewport_agent::set_viewport;
        assert_eq!(
            set(app.world_mut(), &json!({})).expect("read")["overlay_blur"],
            0.0
        );
        assert_eq!(
            set(app.world_mut(), &json!({ "overlay_blur": 8.0 })).expect("set")["overlay_blur"],
            8.0
        );
        assert_eq!(
            set(app.world_mut(), &json!({})).expect("kept")["overlay_blur"],
            8.0
        );
        for bad in [json!(65.0), json!(-1.0), json!("lots"), json!(null)] {
            assert!(
                set(app.world_mut(), &json!({ "overlay_blur": bad })).is_err(),
                "{bad}"
            );
        }
        assert_eq!(
            set(app.world_mut(), &json!({})).expect("kept")["overlay_blur"],
            8.0
        );
    }

    #[test]
    fn play_requests_queue_and_state_polls() {
        let mut app = app();
        // Stopped state.
        let state = editor_play_state(app.world_mut(), &json!({})).expect("state");
        assert_eq!(state["mode"], "stopped");

        // Every PIE method queues; nothing runs inside the call.
        for call in [editor_play, editor_pause, editor_resume, editor_stop] {
            let out = call(app.world_mut(), &json!({})).expect("queued");
            assert_eq!(out["queued"], true);
        }
        let out = editor_step(app.world_mut(), &json!({ "frames": 2 })).expect("queued");
        assert_eq!(out["queued"], true);
        let queued = app.world().resource::<crate::pie::PieRequests>().queue.clone();
        assert_eq!(
            queued,
            vec![
                crate::pie::PieRequest::Play,
                crate::pie::PieRequest::Pause,
                crate::pie::PieRequest::Resume,
                crate::pie::PieRequest::Stop,
                crate::pie::PieRequest::Step(2),
            ]
        );

        // Draining through the shared apply path makes the status observable. Check the
        // intermediate states, not just the tail: a stopped session reports an empty status.
        let step = |world: &mut World, slot: &mut Option<crate::pie::PieSession>, request| {
            let _ = crate::pie::apply_request(world, slot, request, None);
            crate::pie::sync_status(world, slot.as_ref());
            editor_play_state(world, &json!({})).expect("state")
        };
        let mut slot = None;
        let world = app.world_mut();
        // Play: the copy exists and ran a settled frame (no wall-clock bound under parallel
        // test load — see blank_project.rs / level_build.rs).
        let state = step(world, &mut slot, crate::pie::PieRequest::Play);
        assert_eq!(state["mode"], "playing");
        assert!(
            state["entities"].as_u64().unwrap_or(0) > 0,
            "the run played a copy of the edit world: {state}"
        );
        assert!(
            state["frames"].as_u64().unwrap_or(0) >= 1,
            "PIE has not run a settled frame: {state}"
        );
        // Pause → Resume → Stop: the request state machine walks the modes in order.
        let state = step(world, &mut slot, crate::pie::PieRequest::Pause);
        assert_eq!(state["mode"], "paused");
        let state = step(world, &mut slot, crate::pie::PieRequest::Resume);
        assert_eq!(state["mode"], "playing");
        let state = step(world, &mut slot, crate::pie::PieRequest::Stop);
        assert_eq!(
            state["mode"], "stopped",
            "the drain sequence ended with Stop: {state}"
        );
        assert_eq!(state["entities"], 0, "no play world while stopped");
        assert!(slot.is_none());
    }

    #[test]
    fn import_create_list_and_prefab_spawn_through_the_surface() {
        // A scratch project with the editor resources the handlers read.
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let dir = std::env::temp_dir().join(format!(
            "regia-editor-agent-assets-{}-{}",
            line!(),
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("assets")).expect("dir");
        app.world_mut().insert_resource(EditorProject {
            root: Some(dir.clone()),
        });

        // Create: material, prefab, level, folder — through the agent surface.
        for (kind, path) in [
            ("material", "assets/paint/blue.material.ron"),
            ("prefab", "assets/props/barrel.prefab.ron"),
            ("level", "assets/maps/start.level.ron"),
            ("folder", "assets/props/nested"),
        ] {
            let out =
                editor_create_asset(app.world_mut(), &json!({ "kind": kind, "path": path })).expect("create");
            assert_eq!(out["path"], path, "{kind}");
        }
        assert!(
            editor_create_asset(app.world_mut(), &json!({ "kind": "warp", "path": "assets/x" })).is_err()
        );
        assert!(
            editor_create_asset(
                app.world_mut(),
                &json!({ "kind": "material", "path": "assets/paint/blue.prefab.ron" })
            )
            .is_err(),
            "wrong extension refuses"
        );

        // Import: a real PNG source cooks through the pipeline.
        let mut png = Vec::new();
        {
            use image::ImageEncoder;
            let encoder = image::codecs::png::PngEncoder::new(&mut png);
            encoder
                .write_image(&[10u8, 20, 30, 255], 1, 1, image::ExtendedColorType::Rgba8)
                .expect("png");
        }
        let source = dir.join("incoming.png");
        std::fs::write(&source, &png).expect("source");
        let out = editor_import(
            app.world_mut(),
            &json!({ "sources": [source.display().to_string()], "folder": "textures", "client": "test-agent" }),
        )
        .expect("import");
        assert_eq!(out["imported"][0], "assets/textures/incoming.png", "{out}");
        assert!(out["errors"].as_array().is_some_and(|e| e.is_empty()), "{out}");
        // The meta carries agent provenance.
        let meta = std::fs::read(dir.join("assets/textures/incoming.png.meta")).expect("meta");
        let meta = regia_asset::MetaSidecar::from_ron(&meta).expect("parse");
        let provenance = meta.provenance.expect("provenance");
        assert_eq!(provenance.tool, regia_asset::ProvenanceTool::Agent);
        assert_eq!(provenance.client.as_deref(), Some("test-agent"));

        // List: the browser scan through the surface.
        let out = editor_list_assets(app.world_mut(), &json!({ "search": ".prefab.ron" })).expect("list");
        let paths: Vec<&str> = out["assets"]
            .as_array()
            .expect("assets")
            .iter()
            .filter_map(|a| a["path"].as_str())
            .collect();
        assert_eq!(paths, ["assets/props/barrel.prefab.ron"], "{out}");

        // Prefab spawn: an undoable instance with a translation.
        let out = editor_spawn_prefab(
            app.world_mut(),
            &json!({ "path": "assets/props/barrel.prefab.ron", "translation": [1.0, 0.0, -2.0] }),
        )
        .expect("spawn prefab");
        let entity_bits = out["entity"].as_u64().expect("entity id");
        assert_ne!(entity_bits, 0);
        let undo = app.world().resource::<crate::command::UndoStack>().undo_depth();
        assert!(undo >= 1, "the prefab spawn is undoable");
        // Missing prefab → a clean error.
        let err = editor_spawn_prefab(
            app.world_mut(),
            &json!({ "path": "assets/props/ghost.prefab.ron" }),
        )
        .expect_err("missing prefab");
        assert!(err.message.contains("prefab spawn failed"), "{err:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A reflected component for `editor.set_property`.
    #[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
    #[reflect(Component, Default)]
    struct Marker {
        value: f32,
    }

    /// A non-string in `sources` is invalid. The call imports nothing, including the string
    /// that follows the bad element.
    #[test]
    fn import_rejects_a_non_string_source_and_copies_nothing() {
        let dir = regia_test_utils::unique_temp_dir("editor_import_bad_source");
        std::fs::create_dir_all(dir.join("assets")).expect("dir");
        let mut png = Vec::new();
        {
            use image::ImageEncoder;
            let encoder = image::codecs::png::PngEncoder::new(&mut png);
            encoder
                .write_image(&[10u8, 20, 30, 255], 1, 1, image::ExtendedColorType::Rgba8)
                .expect("png");
        }
        let source = dir.join("a.png");
        std::fs::write(&source, &png).expect("source");
        let mut app = app();
        app.world_mut().insert_resource(EditorProject {
            root: Some(dir.clone()),
        });
        let result = editor_import(
            app.world_mut(),
            &json!({ "sources": [1, source.display().to_string()], "folder": "textures" }),
        );
        assert!(
            matches!(&result, Err(err) if err.code == regia_agent::protocol::INVALID_PARAMS),
            "a non-string source must be invalid_params, got {result:?}"
        );
        assert!(
            !dir.join("assets/textures/a.png").exists(),
            "nothing is copied when a source element is not a string"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A fresh scratch directory (pid + counter, never a fixed name).
    fn scratch_dir(label: &str) -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-editor-agent-{label}-{n}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn marker_app() -> (App, Entity) {
        let mut app = app();
        app.register_type::<Marker>();
        let rock = rock(app.world());
        app.world_mut().entity_mut(rock).insert(Marker { value: 1.0 });
        (app, rock)
    }

    fn marker_value(app: &App, entity: Entity) -> f32 {
        app.world().get::<Marker>(entity).expect("marker").value
    }

    #[test]
    fn get_selection_is_empty_before_any_select() {
        let mut app = app();
        let out = editor_get_selection(app.world_mut(), &json!({})).expect("get");
        assert_eq!(out["entities"], json!([]));
        assert_eq!(out["primary"], Value::Null);
    }

    #[test]
    fn set_gizmo_without_params_reports_and_keeps_the_state() {
        let mut app = app();
        editor_set_gizmo(app.world_mut(), &json!({ "mode": "scale", "snap": true })).expect("set");
        // A call that names nothing changes nothing.
        let out = editor_set_gizmo(app.world_mut(), &json!({})).expect("read back");
        assert_eq!(out["mode"], "scale");
        assert_eq!(out["snap"], true);
        // A rejected mode leaves the stored state alone.
        assert!(editor_set_gizmo(app.world_mut(), &json!({ "mode": "warp", "snap": false })).is_err());
        let out = editor_set_gizmo(app.world_mut(), &json!({})).expect("read back");
        assert_eq!(out["mode"], "scale");
        assert_eq!(out["snap"], true);
    }

    #[test]
    fn undo_and_redo_report_an_empty_history() {
        let mut app = app();
        let undone = editor_undo(app.world_mut(), &json!({})).expect("undo");
        assert_eq!(undone["undone"], false);
        assert_eq!(undone["remaining"], 0);
        let redone = editor_redo(app.world_mut(), &json!({})).expect("redo");
        assert_eq!(redone["redone"], false);
        assert_eq!(redone["remaining"], 0);
    }

    #[test]
    fn set_property_applies_undoes_and_a_new_edit_drops_redo() {
        let (mut app, rock) = marker_app();
        let set = |app: &mut App, value: f64| {
            editor_set_property(
                app.world_mut(),
                &json!({ "entity": rock.to_bits(), "component": "Marker", "path": "value", "value": value }),
            )
        };
        let first = set(&mut app, 5.0).expect("set 5");
        assert_eq!(first["before"], json!(1.0));
        assert_eq!(first["after"], json!(5.0));
        assert_eq!(first["applied"], true);
        assert_eq!(first["undoable"], 1);
        assert_eq!(marker_value(&app, rock), 5.0);

        // Agent edits are discrete steps: two sets on one field are two undo entries.
        let second = set(&mut app, 7.0).expect("set 7");
        assert_eq!(second["undoable"], 2, "agent edits never coalesce");

        let undone = editor_undo(app.world_mut(), &json!({})).expect("undo");
        assert_eq!(undone["remaining"], 1);
        assert_eq!(marker_value(&app, rock), 5.0);
        assert_eq!(app.world().resource::<UndoStack>().redo_depth(), 1);

        // A new edit after an undo drops the redo branch.
        set(&mut app, 9.0).expect("set 9");
        assert_eq!(app.world().resource::<UndoStack>().redo_depth(), 0);
        let redone = editor_redo(app.world_mut(), &json!({})).expect("redo");
        assert_eq!(redone["redone"], false);
        assert_eq!(marker_value(&app, rock), 9.0);

        editor_undo(app.world_mut(), &json!({})).expect("undo");
        editor_undo(app.world_mut(), &json!({})).expect("undo");
        assert_eq!(marker_value(&app, rock), 1.0, "full undo restores the original");
    }

    #[test]
    fn set_property_dry_run_changes_nothing() {
        let (mut app, rock) = marker_app();
        let out = editor_set_property(
            app.world_mut(),
            &json!({
                "entity": rock.to_bits(),
                "component": "Marker",
                "path": "value",
                "value": 3.0,
                "dry_run": true,
            }),
        )
        .expect("dry run");
        assert_eq!(out["applied"], false);
        assert_eq!(out["before"], json!(1.0));
        assert_eq!(out["after"], json!(3.0));
        assert_eq!(marker_value(&app, rock), 1.0);
        assert_eq!(out["undoable"], 0, "a dry run pushes no history");
        assert!(
            app.world()
                .get_resource::<UndoStack>()
                .is_none_or(|stack| stack.undo_depth() == 0)
        );
    }

    #[test]
    fn set_property_rejects_bad_input_cleanly() {
        let (mut app, rock) = marker_app();
        let bits = rock.to_bits();
        let cases = [
            json!({ "component": "Marker", "path": "value", "value": 1.0 }),
            json!({ "entity": bits, "path": "value", "value": 1.0 }),
            json!({ "entity": bits, "component": "Marker", "path": "value" }),
            json!({ "entity": bits, "component": "Marker", "path": "value", "value": "text" }),
            json!({ "entity": bits, "component": "Nope", "path": "value", "value": 1.0 }),
            json!({ "entity": bits, "component": "Marker", "path": "missing", "value": 1.0 }),
            json!({ "entity": 1u64 << 40, "component": "Marker", "path": "value", "value": 1.0 }),
        ];
        for case in cases {
            let err = editor_set_property(app.world_mut(), &case).expect_err("rejected");
            assert!(
                err.code == regia_agent::protocol::INVALID_PARAMS
                    || err.code == regia_agent::protocol::NOT_FOUND,
                "{case}: {err:?}"
            );
        }
        assert_eq!(
            marker_value(&app, rock),
            1.0,
            "no rejected call changed the value"
        );
        assert!(
            app.world()
                .get_resource::<UndoStack>()
                .is_none_or(|stack| stack.undo_depth() == 0)
        );
    }

    /// `1e39` is a finite JSON number that casts to `f32::INFINITY`. `editor.set_transform`
    /// refuses it; `editor.set_property` (one entity and `entities`) must too, or the value
    /// lands on a component and poisons later math.
    #[test]
    fn set_property_refuses_a_number_past_the_f32_range() {
        let (mut app, rock) = marker_app();
        let bits = rock.to_bits();
        let before = app.world().get::<Transform>(rock).expect("t").translation;
        let cases = [
            json!({ "entity": bits, "component": "Marker", "path": "value", "value": 1e39 }),
            json!({ "entities": [bits], "component": "Marker", "path": "value", "value": 1e39 }),
            json!({
                "entity": bits,
                "component": "Transform",
                "path": "translation.x",
                "value": 1e39
            }),
            json!({
                "entities": [bits],
                "component": "Transform",
                "path": "translation.x",
                "value": 1e39
            }),
            json!({
                "entities": [bits],
                "component": "Transform",
                "path": "rotation.ry",
                "value": 1e39
            }),
        ];
        for case in cases {
            let err = editor_set_property(app.world_mut(), &case).expect_err("non-finite f32");
            assert_eq!(err.code, regia_agent::protocol::INVALID_PARAMS, "{case}: {err:?}");
            let reason = err.data.as_ref().and_then(|d| d["reason"].as_str());
            assert!(
                reason.is_none_or(|r| r == "value_mismatch" || r == "non_finite"),
                "{case}: {err:?}"
            );
        }
        assert_eq!(marker_value(&app, rock), 1.0);
        assert_eq!(app.world().get::<Transform>(rock).expect("t").translation, before);
        assert!(
            app.world()
                .get_resource::<UndoStack>()
                .is_none_or(|stack| stack.undo_depth() == 0)
        );
    }

    #[test]
    fn save_level_without_an_open_level_or_path_is_a_clean_error() {
        let mut app = app();
        let err = editor_save_level(app.world_mut(), &json!({})).expect_err("no level");
        assert_eq!(err.code, regia_agent::protocol::NOT_FOUND);

        let open = crate::level_io::new_level(app.world_mut());
        app.world_mut().insert_resource(open);
        let err = editor_save_level(app.world_mut(), &json!({})).expect_err("no path yet");
        assert_eq!(err.code, regia_agent::protocol::INVALID_PARAMS);
        assert!(err.message.contains("path"), "{err:?}");
    }

    /// ADR-1816: a save-as path outside the project and the allowed roots, through a symlink, or
    /// with `..` or a drive prefix is `path_outside_project`, and nothing is written.
    #[test]
    fn save_level_refuses_paths_outside_the_project() {
        let mut app = app();
        let dir = scratch_dir("save-escape");
        app.world_mut().insert_resource(EditorProject {
            root: Some(dir.clone()),
        });
        let open = crate::level_io::new_level(app.world_mut());
        app.world_mut().insert_resource(open);
        let bad = vec!["../x.level.ron".to_string(), "C:x.level.ron".to_string()];
        #[cfg(unix)]
        {
            // Rebound here so the binding is not a needless `mut` on Windows.
            let mut bad = bad;
            bad.push("/etc/regia-never.level.ron".to_string());
            // A link out of every allowed root (the temp dir is one, so not a second scratch dir).
            std::os::unix::fs::symlink("/usr", dir.join("out")).expect("link");
            bad.push("out/a.level.ron".to_string());
            for p in &bad {
                let err = editor_save_level(app.world_mut(), &json!({ "path": p })).expect_err(p);
                let reason = err.data.as_ref().and_then(|d| d["reason"].as_str());
                assert_eq!(reason, Some("path_outside_project"), "{p}: {err:?}");
            }
        }
        #[cfg(not(unix))]
        for p in &bad {
            assert!(
                editor_save_level(app.world_mut(), &json!({ "path": p })).is_err(),
                "{p}"
            );
        }
    }

    #[test]
    fn save_level_resolves_against_the_project_clears_dirty_and_remembers_the_path() {
        let mut app = app();
        let dir = scratch_dir("save");
        app.world_mut().insert_resource(EditorProject {
            root: Some(dir.clone()),
        });
        let open = crate::level_io::new_level(app.world_mut());
        app.world_mut().insert_resource(open);
        let mut stack = UndoStack::new();
        stack.mark_dirty();
        app.world_mut().insert_resource(stack);

        let out = editor_save_level(app.world_mut(), &json!({ "path": "assets/maps/a.level.ron" }))
            .expect("save as");
        let expected = dir.join("assets").join("maps").join("a.level.ron");
        // Compared as paths: the reply is a native host path (`\` on Windows, ADR-3150).
        assert_eq!(
            out["path"].as_str().map(std::path::Path::new),
            Some(expected.as_path())
        );
        assert!(expected.is_file(), "the level file exists under the project root");
        assert!(out["bytes"].as_u64().is_some_and(|bytes| bytes > 0));
        assert!(
            !app.world().resource::<UndoStack>().is_dirty(),
            "a save clears dirty"
        );
        assert_eq!(
            app.world().resource::<OpenLevel>().path.as_deref(),
            Some(expected.as_path()),
            "the level remembers where it was saved"
        );

        // A second save with no path writes to the remembered one.
        std::fs::remove_file(&expected).expect("remove");
        let again = editor_save_level(app.world_mut(), &json!({})).expect("save again");
        assert_eq!(again["path"], expected.display().to_string());
        assert!(expected.is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_script_needs_a_project_and_a_name() {
        let mut app = app();
        let err = editor_create_script(app.world_mut(), &json!({ "name": "spin" })).expect_err("no project");
        assert_eq!(err.code, regia_agent::protocol::NOT_FOUND);
        let err = editor_create_script(app.world_mut(), &json!({ "name": "" })).expect_err("empty name");
        assert_eq!(err.code, regia_agent::protocol::INVALID_PARAMS);
        let err = editor_create_script(app.world_mut(), &json!({})).expect_err("no name");
        assert_eq!(err.code, regia_agent::protocol::INVALID_PARAMS);
    }

    #[test]
    fn create_script_registers_once_and_refuses_a_duplicate() {
        let mut app = app();
        let dir = scratch_dir("script");
        std::fs::create_dir_all(dir.join("game/src")).expect("game dir");
        std::fs::write(dir.join("game/src/lib.rs"), "// REGIA:SCRIPTS\n").expect("lib");
        app.world_mut().insert_resource(EditorProject {
            root: Some(dir.clone()),
        });

        let out = editor_create_script(app.world_mut(), &json!({ "name": "spin" })).expect("create");
        let path = std::path::PathBuf::from(out["path"].as_str().expect("path"));
        assert!(path.is_file(), "the script file exists");
        let lib = std::fs::read_to_string(dir.join("game/src/lib.rs")).expect("lib");
        assert!(lib.contains("mod spin;"), "{lib}");
        assert!(
            lib.contains("// REGIA:SCRIPTS"),
            "the marker stays for the next script"
        );

        let err = editor_create_script(app.world_mut(), &json!({ "name": "spin" })).expect_err("duplicate");
        assert_eq!(err.code, regia_agent::protocol::INVALID_PARAMS);
        assert_eq!(
            lib,
            std::fs::read_to_string(dir.join("game/src/lib.rs")).expect("lib")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prefab_mode_reports_none_until_a_prefab_is_open() {
        let mut app = app();
        let out = editor_prefab_mode(app.world_mut(), &json!({})).expect("mode");
        assert_eq!(out["editing"], Value::Null);
    }

    #[test]
    fn install_registers_every_method() {
        let methods = all_methods();
        assert!(methods.len() >= 6, "editor surface: {} methods", methods.len());
        let names: Vec<&str> = methods.iter().map(|(method, _)| method.name).collect();
        for required in [
            "editor.play",
            "editor.import",
            "editor.new_project",
            "editor.open_project",
            "editor.recent_projects",
            "editor.create_script",
            "editor.edit_material",
            "editor.open_prefab",
            "editor.capture_viewport",
            "editor.anim_add_node",
            "editor.anim_transition",
            "editor.anim_debug",
            "editor.capture_panel",
            "editor.get_theme",
            "editor.set_theme",
            "editor.bake_get",
            "editor.bake_set",
            "editor.bake_start",
            "editor.bake_cancel",
        ] {
            assert!(names.contains(&required), "missing {required} in {names:?}");
        }
        for (method, _) in &methods {
            assert!(method.name.starts_with("editor."), "{}", method.name);
            assert!(!method.tool.is_empty(), "an MCP tool name exists");
        }
    }

    /// ADR-3285: a method that changes state another call can observe is not `read`.
    /// These six were declared `read` (or `play` but not mutating) while a later call
    /// saw the new camera, link, selection, panel row or queued run.
    #[test]
    fn state_changing_editor_methods_are_not_read() {
        let methods = all_methods();
        let expect = [
            ("editor.viewport_instance_set", "edit_scene"),
            ("editor.viewport_link", "edit_scene"),
            ("editor.viewport_instance_remove", "edit_scene"),
            ("editor.tests_panel_open", "edit_scene"),
            ("editor.agent_changes_select", "edit_scene"),
            ("editor.tests_panel_run", "play"),
        ];
        let read_only = regia_agent::AgentPermissions {
            config: regia_agent::AgentConfig::fail_closed(),
            source: None,
        };
        for (name, class) in expect {
            let schema = &methods
                .iter()
                .find(|(method, _)| method.name == name)
                .unwrap_or_else(|| panic!("{name} is registered"))
                .0;
            assert!(schema.mutating, "{name} is rate-limited");
            assert_eq!(schema.permission.name(), class, "{name}");
            assert_ne!(schema.permission, Permission::Read, "{name}");
            assert!(
                !read_only.allows(schema.permission),
                "{name} is denied to a read-only client"
            );
        }
    }

    /// `editor.static_move_warnings` (specs/14 A1) answers the play status's count: 0 before
    /// play, then what the play-world guard mirrored into `PieStatus`.
    #[test]
    fn static_move_warnings_answers_the_play_status_count() {
        let (_, handler) = all_methods()
            .into_iter()
            .find(|(schema, _)| schema.name == "editor.static_move_warnings")
            .expect("editor.static_move_warnings is registered");
        let mut world = World::new();
        assert_eq!(handler(&mut world, &json!({})).expect("answer")["count"], 0);
        world.resource_mut::<crate::pie::PieStatus>().static_move_warnings = 3;
        assert_eq!(handler(&mut world, &json!({})).expect("answer")["count"], 3);
    }

    /// Review w8-outliner-place: `assets_request_path` used to prefix `assets/` before judging the
    /// caller's spelling, so `/tmp/x` became `assets//tmp/x` and resolved inside `assets/tmp/`.
    /// Every caller (`editor.spawn_prefab`, `editor.open_prefab`, `editor.edit_material` and the
    /// `editor.import` folder) now refuses an absolute, root-relative or UNC path as
    /// `path_outside_project`, and nothing under `assets/tmp/` is touched.
    #[test]
    fn assets_paths_refuse_absolute_and_unc_forms_through_every_caller() {
        let mut app = app();
        let dir = regia_test_utils::unique_temp_dir("editor-agent-assets-abs");
        // The file the old prefixing resolved `/tmp/w9.*` to: it must stay as it is.
        std::fs::create_dir_all(dir.join("assets/tmp")).expect("dir");
        let decoy = "keep: not a real asset";
        for name in ["w9.prefab.ron", "w9.material.ron"] {
            std::fs::write(dir.join("assets/tmp").join(name), decoy).expect("decoy");
        }
        app.world_mut().insert_resource(EditorProject {
            root: Some(dir.clone()),
        });
        let method = |name: &str| {
            all_methods()
                .into_iter()
                .find(|(schema, _)| schema.name == name)
                .map(|(_, handler)| handler)
                .unwrap_or_else(|| panic!("{name} is registered"))
        };
        let reason = |e: &RpcError| {
            e.data
                .as_ref()
                .and_then(|d| d.get("reason"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let forms = |ext: &str| {
            vec![
                format!("/tmp/w9.{ext}"),
                format!("\\tmp\\w9.{ext}"),
                format!("\\\\server\\share\\w9.{ext}"),
                format!("assets//tmp/w9.{ext}"),
            ]
        };
        for (name, ext) in [
            ("editor.spawn_prefab", "prefab.ron"),
            ("editor.open_prefab", "prefab.ron"),
            ("editor.edit_material", "material.ron"),
        ] {
            let handler = method(name);
            for path in forms(ext) {
                let err = handler(
                    app.world_mut(),
                    &json!({ "path": path, "base_color": [1.0, 0.0, 0.0, 1.0] }),
                )
                .expect_err(&format!("{name} {path:?} is not under assets/"));
                assert_eq!(
                    reason(&err).as_deref(),
                    Some("path_outside_project"),
                    "{name} {path:?}: {err:?}"
                );
            }
        }
        // `editor.import` trims a leading `/` from its folder by design (an assets-relative folder),
        // so the root-relative and UNC spellings are the ones to refuse.
        let import = method("editor.import");
        let source = dir.join("source.txt");
        std::fs::write(&source, "x").expect("source");
        for folder in ["\\tmp", "\\\\server\\share", "assets//tmp", "assets/\\tmp"] {
            let err = import(
                app.world_mut(),
                &json!({ "sources": [source.display().to_string()], "folder": folder }),
            )
            .expect_err(&format!("import folder {folder:?} is not under assets/"));
            assert_eq!(
                reason(&err).as_deref(),
                Some("path_outside_project"),
                "import {folder:?}: {err:?}"
            );
        }
        for name in ["w9.prefab.ron", "w9.material.ron"] {
            assert_eq!(
                std::fs::read_to_string(dir.join("assets/tmp").join(name)).expect("decoy"),
                decoy,
                "{name} untouched"
            );
        }
        assert_eq!(
            std::fs::read_dir(dir.join("assets/tmp")).expect("dir").count(),
            2,
            "nothing imported into assets/tmp"
        );
        // The relative forms still resolve (the refusal is not a blanket one).
        assert!(assets_request_path(&dir, "path", "tmp/w9.prefab.ron").is_ok());
        assert!(assets_request_path(&dir, "path", "assets/tmp/w9.prefab.ron").is_ok());
        assert!(assets_request_path(&dir, "path", "assets\\tmp\\w9.prefab.ron").is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
