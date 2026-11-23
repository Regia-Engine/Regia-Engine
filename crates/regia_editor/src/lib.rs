// SPDX-License-Identifier: MPL-2.0
//! The editor: docking UI, viewport, outliner, inspector, content browser, output log, PIE,
//! hot reload host.
//!
//! Specifications: `docs/specs/19-editor.md`, `docs/specs/20-hot-reload.md`. M10 brings the
//! editor shell ([`shell`]): an `egui_dock` layout with the Viewport, Outliner, Inspector,
//! Content Browser and Output Log panels, all editing through the [`command`] module's undo
//! stack and exposed to the agent protocol by [`agent_ext`] (`book/agents/editor.md`).
//! [`HotReloadHost`] builds a salted game dylib, snapshots reflected state, swaps, never
//! unloads the old mapping (ADR-015).
//!
//! [`anim`] is the animation editor (`docs/specs/17-animation.md` § 5): anim graph, clip and
//! skeleton panels, partly ported from Fyrox (MIT, `docs/ported-sources.toml`).
//!
//! This crate provides [`inspector_rows`] (flat inspector rows) and [`HotReloadHost`].

// `MeshLibrary` (a `Resource`, so `Send + Sync`) holds `CpuMaterial`s, which can hold a counted
// texture (`MaterialImage::Asset`, ADR-7330) and with it a `wgpu::Device`; proving that type
// `Sync` walks wgpu-core's pipeline and bind-group types past the default depth of 128 (E0275,
// ADR-7422). No unsafe and no lint: only the compiler's search depth.
#![recursion_limit = "256"]

pub mod agent_ext;
pub mod anim;
pub mod asset_panels;
pub mod asset_panels_agent;
pub mod asset_viewer;
pub mod asset_watch;
pub mod audio_gizmo;
pub mod autosave;
pub mod bake_panel;
pub mod bake_runner;
pub mod budget;
pub mod code_reload;
pub mod command;
pub mod command_palette;
pub mod content;
pub mod content_index;
pub mod dcc_feed;
pub mod dnd;
pub mod engine;
// Extensions: dynamic libraries behind the ABI stamp, editor registries, the panel (specs/57).
pub mod extension;
mod file_dialog;
pub mod gltf_hierarchy;
pub mod handles;
pub mod hierarchy_dnd;
mod hot_reload;
pub mod inspector;
pub mod kit;
pub mod kit_view;
pub mod layers_panel;
pub mod level_io;
pub mod lightmap_panel;
pub mod log_panel;
pub mod lua_inspector;
pub mod lua_mods;
pub mod material_editor;
pub mod mesh_source;
pub mod model_report;
pub mod outliner;
pub mod outliner_folders;
pub mod outliner_query;
pub mod overlays;
pub mod pane_bench;
pub mod pane_texture;
pub mod picking;
pub mod pie;
pub mod pie_net;
pub mod placed_create;
pub mod prefab_edit;
// The procedural graph editor and placement tools (spec 67, ADR-1675).
pub mod procgen;
pub mod project;
pub mod project_settings;
pub mod reference;
mod release_agent;
pub mod render;
mod scaffold;
pub mod scalar_overlay_agent;
pub mod scatter_brush;
pub mod scripts;
pub mod selection;
pub mod seq_curve_panel;
pub mod shapes;
pub mod shell;
pub mod shot_panel;
pub mod static_moves;
pub mod terrain_tool;
pub mod tests_panel;
pub mod theme;
pub mod theme_panel;
pub mod theme_panel_agent;
/// Project Settings → XR (spec 58 § 6).
pub mod xr_settings;
pub mod xr_simulator;

pub mod asset_search;
pub mod asset_validate_agent;
pub mod atlas_agent;
pub mod fbx_agent;
pub mod save_validation;
// The BVH importer's inspector (`editor.bvh_inspect`, ADR-2532).
pub mod bvh_agent;
// The PLY, STL and PSD importers' inspectors (`editor.ply_inspect` and friends, ADR-3244).
pub mod fracture_agent;
pub mod fracture_panel;
pub mod fracture_tools;
pub mod hierarchy_agent;
pub mod hierarchy_ops;
pub mod mesh_image_agent;
pub mod svg_agent;
pub mod thumb_cache;
pub mod thumbnails;
pub mod treemap;
pub mod ui_capture;
pub mod ui_roles;
pub mod undo_agent;
pub mod viewport;
pub mod viewport_agent;
pub mod viewport_blur;
pub mod viewport_instance;
pub mod viewport_instance_agent;
pub mod views;
pub mod visibility;
pub mod window_capture;
// Out-of-process editor tools: manifests, scoped tokens, the Tools panel (ADR-1523).
pub mod tools;
// Agent `quit.request` in the edit world (ADR-1582).
pub mod chunks_panel;
mod path_norm;
pub mod quit;
// The gameplay debug panels: state trees, smart objects, camera assets and rig graphs (ADR-2030).
pub mod camera_graph;
pub mod camera_panel;
pub mod gameplay_panel;
pub mod smart_object_panel;
pub mod spacemouse;
pub mod state_tree_panel;
// The 2D mode: tile and auto-tiled terrain painting in the front view (ADR-1889).
pub mod mode2d;
// The UI designer: WYSIWYG `.ui.ron` editing with view-model bindings (ADR-1811).
pub mod ui_anim_agent;
pub mod ui_anim_doc;
pub mod ui_anim_panel;
pub mod ui_designer;
pub mod ui_designer_agent;
pub mod ui_designer_gpu;
pub mod ui_designer_panel;
pub mod ui_designer_preview;
pub mod ui_inspector;
// Gameplay data panels: tables, abilities, montage timeline, ragdoll (ADR-2031).
pub mod abilities_panel;
pub mod audio_mixer_panel;
pub mod cue_editor_panel;
pub mod data_panels;
pub mod montage_panel;
pub mod ragdoll_panel;
pub mod tables_panel;
// The Video panel: preview, transport, stream info, decode stats and the boot movie list (ADR-2720).
pub mod video_panel;
// The section strip of the Sequence Curves tab: clip, audio and sub-sequence sections (ADR-2135).
pub mod seq_sections;
// Sequence authoring: tracks, bindings, new sequences, keys from the scene, preview (ADR-9710).
pub mod seq_tracks;

/// The glTF and OBJ importer fuzz targets the editor adds to the agent's fuzz registry (ADR-2084).
pub mod import_fuzz;
// The material graph editor: node canvas, live preview, `editor.matgraph_*` (ADR-2318).
pub mod matgraph_editor;
// The editor baseline (ADR-2321): preferences, keymap and layout persistence, Project Settings.
pub mod baseline_agent;
pub mod baseline_fuzz;
pub mod diagnostics_agent;
pub mod diagnostics_panel;
pub mod editor_keymap;
pub mod editor_layout;
pub mod editor_prefs;
/// Prefab variants: create, register and edit `.variant.ron` files (ADR-2323).
pub mod prefab_variant_edit;
pub mod prefs_panel;
/// The Profiler panel: flame view, GPU passes, capture files (ADR-2323).
pub mod profiler_panel;
pub mod project_settings_ext;
pub mod trace_timeline;

use std::collections::HashMap;

pub use hot_reload::{
    CompileState, GameExports, GameLibraryError, HotReloadConfig, HotReloadHost, HotReloaded,
    LoadedGameLibrary, ReloadPhase, ReloadReport, dylib_extension, host_abi_hash, load_game_exports,
    load_game_library, process_links_dynamic_std, reflected_count, reflected_number, toast_line,
    with_game_owner,
};

use crate::engine::ecs::prelude::*;
use crate::engine::ecs::reflect::AppTypeRegistry;

/// One row in the debug inspector: an entity and its components.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectorRow {
    /// Entity identifier as a stable `u64` (the result of [`Entity::to_bits`]).
    pub entity: u64,
    /// Component type names, sorted. Empty when the type registry is unavailable.
    pub components: Vec<String>,
}

/// Collects every entity in `world`, sorted by entity id, with its component type names.
///
/// Component names come from the [`AppTypeRegistry`] resource in the world. When the registry is
/// missing or a component's type is not registered for reflection, that component is omitted from
/// the list. Callers that need a count rather than names can use `row.components.len()`.
pub fn inspector_rows(world: &World) -> Vec<InspectorRow> {
    let registry = world.get_resource::<AppTypeRegistry>();
    let registry_read = registry.map(|r| r.read());

    // Map TypeId → type_path for every type in the registry.
    let mut type_names: HashMap<std::any::TypeId, String> = HashMap::new();
    if let Some(ref reg) = registry_read {
        for registration in reg.iter() {
            type_names.insert(
                registration.type_id(),
                registration.type_info().type_path().to_string(),
            );
        }
    }

    let components = world.components();
    let archetypes = world.archetypes();

    // Collect (entity_bits, component_names) pairs, then sort.
    let mut rows: Vec<(u64, Vec<String>)> = Vec::new();

    for archetype in archetypes.iter() {
        let comp_ids = archetype.components();

        // Resolve component names once per archetype (shared by all entities in it).
        let mut comp_names: Vec<String> = Vec::new();
        for comp_id in comp_ids {
            if let Some(info) = components.get_info(*comp_id)
                && let Some(type_id) = info.type_id()
                && let Some(name) = type_names.get(&type_id)
            {
                comp_names.push(name.clone());
            }
        }
        comp_names.sort();

        for entity_ref in archetype.entities() {
            let entity = entity_ref.id();
            rows.push((entity.to_bits(), comp_names.clone()));
        }
    }

    rows.sort_by_key(|(id, _)| *id);
    rows.into_iter()
        .map(|(entity, components)| InspectorRow { entity, components })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::ecs::prelude::{Component, Entity, Reflect};

    #[derive(Component, Reflect, Default)]
    #[reflect(Component)]
    struct Marker;

    #[test]
    fn inspector_rows_finds_spawned_entities() {
        let mut app = crate::engine::app::App::new();
        app.register_type::<Marker>();

        // Count rows before spawning.
        let before = inspector_rows(app.world()).len();

        // Spawn two entities.
        let e1 = app.world_mut().spawn(Marker).id();
        let e2 = app.world_mut().spawn(Marker).id();

        let rows = inspector_rows(app.world());
        // App::new() creates internal entities (schedules, registry, etc.);
        // we verify that exactly two more entities exist now.
        assert_eq!(rows.len(), before + 2, "two more entities after spawn");

        // Find the rows for our two spawned entities.
        let bits = |e: Entity| e.to_bits();
        let row1 = rows.iter().find(|r| r.entity == bits(e1)).expect("e1 not found");
        let row2 = rows.iter().find(|r| r.entity == bits(e2)).expect("e2 not found");

        // Both should report the Marker component.
        for row in [row1, row2] {
            assert!(
                row.components.iter().any(|n| n.contains("Marker")),
                "entity {}: expected Marker in {:?}",
                row.entity,
                row.components
            );
        }

        // Rows are sorted by entity id.
        for w in rows.windows(2) {
            assert!(w[0].entity <= w[1].entity, "rows must be sorted");
        }
    }

    #[test]
    fn inspector_rows_empty_world() {
        let app = crate::engine::app::App::new();
        let rows = inspector_rows(app.world());
        // Internal entities exist, but no user-spawned ones.
        // Just verify the function runs without error and returns sorted rows.
        for w in rows.windows(2) {
            assert!(w[0].entity <= w[1].entity);
        }
    }
}

// The Agent Changes panel and the `agent.open_in_editor` hook (specs/23 § 5, ADR-2346).
pub mod agent_changes;

// The Terrain panel: the level's terrain, heightmap import/export, grass density, scatter rules (ADR-2411).
pub mod terrain_panel;

// Provenance and review badges in the Content Browser (ADR-2428).
pub mod content_provenance;
// `editor.asset_provenance`, `editor.asset_repair_metas`, `editor.asset_subasset_changes` (ADR-2428).
pub mod asset_provenance_agent;

// The headless editor (`--headless`: no window, offscreen views) and the `gen.*`, `editor.open_level`,
// `editor.headless_status` and `agent.shutdown` methods (ADR-2412).
pub mod headless;

// One ECS world and one view per play-in-editor client (`editor.pie_net_clients`, ADR-2623).
pub mod pie_worlds;

// The behavior tree editor: outline panel, live node status, `editor.bt_*` (ADR-2408).
pub mod bt_editor;
// The Motion Matching debug panel (spec 52 § 6, ADR-2409).
pub mod mm_panel;

// Outliner row icons: kind glyph and color, derived role, unclassified marker (spec 101, ADR-2488).
pub mod kind_icons;
// File → Export selection to glTF… (ADR-2554, book/agents/export-glb.md).
pub mod export_glb;

// Viewport and panel tools (ADR-2696): vertex snap, drop to floor, collider handles, World Settings,
// Output Log links.
pub mod collider_handles;
pub mod log_links;
pub mod surface_tools;
pub mod world_settings;
// UI components in the designer: make component, instance parameters, palette (spec 111, ADR-3147).
pub mod ui_designer_component;
// UI components' agent methods (`editor.ui_designer_make_component`, ..., spec 111, ADR-3147).
pub mod ui_designer_component_agent;
// The VFX editor: module stacks, curves, live preview, per-emitter cost (spec 26 § 3, ADR-3171).
pub mod vfx_editor;

// The state tree editor: node view, palette, inspector, live active path, `editor.statetree_*` (ADR-3342).
pub mod state_tree_editor;
// The Logic Graph panel: node canvas, palette, inspector, live activity, `editor.logic_*` (spec 102 P2, ADR-7224).
pub mod logic_graph_editor;

// Per-client play-in-editor conditioner presets (`editor.pie_net_client_conditioner`, ADR-3996).
pub mod pie_client_conditioner;

// UI designer pictures: the inspector picker and the previews of `.ui.ron` images named by asset
// path or icon (ADR-3148, book/agents/ui-image-assets.md).
pub mod ui_designer_images;

// Atomic file writes for editor saves (`.animgraph.ron`, `.animedit.ron`, capture PNGs) with a
// Windows-friendly rename retry (sprint audit g, finding 1). Public helper shared by the
// editor's save paths (documented as public in the v0.1.28 integration release notes).
pub mod atomic_write;
pub mod gltf_ext_agent;
// The USD importer on the agent protocol (`editor.usd_inspect`, ADR-3235).
pub mod usd_agent;

// The conversation graph editor (spec 56 § 13, ADR-3298).
pub mod conversation_panel;
// The Localization panel: key-by-locale table, edits, gather, usages (ADR-3260).
pub mod loc_panel;
// The cloth inspector and paint mode, `editor.cloth_panel_*` (spec 55 § 6, ADR-2884).
pub mod cloth_panel;
// Asset file operations as agent calls (specs/23 § 3.3, `book/agents/asset-ops.md`, ADR-5706):
// `editor.edit_asset`, `editor.reimport`, `editor.move_asset`, `editor.delete_asset`,
// `editor.set_import_settings`.
pub mod asset_ops_agent;

// Editor crash reports: the game panic hook, play-in-editor `pie` reports, and
// `editor.crash_reports` / `editor.crash_report_read` (ADR-3931).
pub mod crash_reports;
// Play-in-editor with separate server and client processes (`editor.pie_processes`, ADR-2682).
pub mod pie_process;
// The Look-dev panel: a reference path trace of the level from the editor (specs/27 § 1.1, ADR-5896).
pub mod lookdev_panel;
// Prefab break link and file-backed apply override (spec 14 § A3, ADR-7203): `editor.prefab_break_link`.
pub mod prefab_link;
// The editor feeds the per-level dirty set and saves only it (spec 37 § 3.4, ADR-7203).
pub mod level_dirty;
// `editor.layer_delete`: the `layer_in_use` refusal (spec 37 § 4.4, ADR-7203).
pub mod layer_delete_agent;
// Viewport tools and menu actions of specs/19 §§ 2-3 (ADR-7204): CPU mesh picking, view presets
// with eased moves, pivot modes, snap toggle, Alt+drag duplicate, File/Edit menu items, clipboard,
// Outliner visibility and lock; `editor.view_preset`, `editor.menu_invoke`, ... (book/agents/viewport-tools.md).
pub mod viewport_ops;
pub mod viewport_ops_agent;
// Data assets (`.data.ron`) and the Data Asset panel, `editor.data_asset_*` (specs/19 § 2, ADR-7234).
pub mod data_asset;
// Project Settings pages for physics layers, packaging, localization and audio buses, and the
// New script wizard (specs/19 § 2, § 5, ADR-7234).
pub mod creators;
// The physics layer matrix editor: config/physics.ron in Project Settings and
// `editor.physics_layers_get` / `editor.physics_layers_edit` (ADR-7233).
pub mod physics_matrix;
// Play-in-editor modes and robustness (specs/19 § 1, § 4, § 7; ADR-7205; `book/agents/pie-modes.md`):
// game panics with a backtrace, "keep simulation changes", the `EditorOnly` save filter.
pub mod editor_jobs;
pub mod editor_only;
pub mod panic_trace;
pub mod pie_keep;
pub mod play_input;
// UI designer direct manipulation, typed widget rows, multi-select and clipboard: transform and
// snap, align, zoom and pan, `widget.<field>` keys, group, copy and paste (ADR-9730, spec 81 § 4.7-4.9).
pub mod ui_designer_canvas;
pub mod ui_designer_multi;
pub mod ui_designer_widget;
// Layout variants in the UI designer (P-271, ADR-7858).
pub mod ui_designer_variants;
// The Vehicle Tuning panel: `.vehicle.ron` sections, plots, live Apply in play, recorded runs
// (`editor.vehicle_tuning_*`, ADR-9760).
pub mod vehicle_tuning;
pub mod vehicle_tuning_panel;
pub mod vehicle_tuning_plots;
// The editor's view state per project: File ▸ Recent and per level the camera, bookmarks and
// Outliner locks in `.regia/editor_view.ron` (ADR-7275, book/agents/viewport-tools.md).
pub mod view_state;
// Large-project scaling (ADR-7306): per-panel draw times for soak scripts (`editor.panel_timings`,
// book/agents/editor-panel-timings.md).
pub mod panel_timings;
// Long lists drawn a screenful at a time (ADR-7306).
pub mod virtual_list;
// The sequencer timeline of the Sequence Curves tab: dope sheet, markers, cuts, transport (ADR-7322).
pub mod seq_timeline;
// The Profiler panel's model and sources (ADR-9740).
pub mod profiler_model;
pub mod profiler_source;
// Editor agent parity: the UI action registry, the button allowlist and `editor.actions` (ADR-7307).
pub mod agent_parity;
// Document autosave and recovery of the asset editors (`specs/19` § 7, ADR-9770).
pub mod doc_autosave;
// The asset editors registered for document autosave (ADR-9770).
pub mod doc_autosave_providers;
// Cancellable background tasks: the game build's row and Cancel, and `editor.cancel_task` on the
// content import job (`specs/19` § 7, ADR-9770, ADR-7381).
pub mod tasks;
// `editor.autosave_*`, `editor.recovery_*` and `editor.cancel_task` (ADR-9770).
pub mod autosave_agent;
// Editor accessibility: accessible names, theme contrast, the AccessKit audit (`editor.a11y_contrast`, ADR-9810).
pub mod a11y_audit;
// The Profiler panel's per-thread Timeline tab and the timelines' shared geometry (ADR-7325).
pub mod thread_timeline;
pub mod timeline_geometry;
// Document autosave of the UI animation editor (`specs/19` § 7, ADR-9770, ADR-7340).
pub(crate) mod ui_anim_autosave;
// Keyboard routes and named list views for the painted canvases (graphs, sequencer, profiler; ADR-7383).
pub mod canvas_keys;
// Document autosave of the sequence editor: per-document dirty flags (`specs/19` § 7, ADR-7382).
pub mod seq_autosave;
// Document autosave of prefab edit mode (`specs/19` § 7, ADR-7382).
pub(crate) mod prefab_autosave;
// The separate play window (specs/19 § 4, ADR-7272; `book/agents/pie-modes.md`).
pub mod play_window;
// The play window's letterbox blit for a chosen play resolution (ADR-7283).
pub mod play_blit;
// Audit rc1-sec-paths-z: every agent document path through one rule (ADR-7388).
#[cfg(test)]
mod doc_path_tests;
// Undo, duplicate and respawn of entities added on top of prefab instances (ADR-7412).
#[cfg(test)]
mod undo_prefab_added_tests;
// The Shots panel's shot list as a document: path, saved copy, dirty, save and open (ADR-8730).
pub mod shot_document;
// Content ▸ Import settings…: the typed import-settings panel (`editor.import_settings_*`, ADR-7469).
pub mod import_settings_panel;
// Create prefab from selection: Outliner menu item and `editor.create_prefab` (ADR-7468).
pub mod prefab_create;
// The menu tree, `editor.menu_list`, the Tools/Play/Help menus and Rebuild asset database (ADR-7475).
pub mod menus;
// File ▸ Package for platform and Tools ▸ Clear caches as cancellable `regia` jobs (ADR-7475).
pub mod package_job;
// Help ▸ Documentation, Agent guide and About (ADR-7475).
pub mod about;
// Project Settings ▸ Icon: assets/branding/icon.png (ADR-7475).
pub mod project_icon;
// Launch standalone and Launch on Android device (`editor.launch`, `specs/19` § 4, ADR-8980).
pub mod launch;
// Android device screen mirroring with tap forwarding (P-299, ADR-7836).
pub mod device_mirror;
// The navigation preview: Show ▸ Build navigation preview, `editor.nav_preview` (ADR-7593).
pub mod nav_preview;
// Show ▸ Post volumes: every post volume's outline in the viewport (ADR-7593).
pub mod volume_overlay;
// The viewport's resolution preview: the play window's letterbox frame and safe-area guides (ADR-7744).
pub mod resolution_preview;
// Break a placed level as one undo step (`editor.break_placed`, spec 37 § 6.5, ADR-7614).
pub mod placed_break;
// The Inspector's prefab-variant overrides with Clear override (spec 83 § 2.4, ADR-7614); declared
// here so `inspector/mod.rs` carries only the one-line hook.
#[path = "inspector/variant.rs"]
pub mod inspector_variant;
// Tools ▸ Bake reflection captures, a cancellable job writing the level's `.fcap` (ADR-7592).
pub mod capture_bake;
// The selected reflection capture's influence box and blend margin helper (ADR-7592).
pub mod capture_helper;
// One asset-open dispatcher for the Content Browser, log links and editor.open_asset (ADR-7725).
pub mod asset_open;
// Sequence ▸ Render movie…, a cancellable editor job running `render.out_job` in process (ADR-7726).
pub mod render_out_job;
// Terrain ▸ New terrain and `editor.terrain_create`: create, cook and name a terrain (ADR-7727).
pub mod terrain_create;
// The smart-object definition editor (`.smartobject.ron` forms, `editor.smart_object_def_*`, ADR-7736).
pub mod smart_object_def_panel;
// Slot and entrance handles of the open smart-object definition in the viewport (ADR-7736).
pub mod smart_object_handles;
// Named layouts, workspaces, maximize, full-screen viewport and pane changes (ADR-7753).
pub mod workspaces;
// Editor feature profiles (`.editor_profile.ron`, ADR-7753).
pub mod feature_profiles;
// The main window's state per project (`editor_window_state.ron`, ADR-7753).
pub mod window_state;
// The motion-database editor: schema form, clip list, tag chips, in-memory rebuild, `editor.mm_db_*` (spec 52 § 6, ADR-7740).
pub mod mm_db_editor;
// The Biome editor: typed forms over a `.biome.ron` (spec 65 § 3, w11-biome-procgen-tooling).
pub mod biome_panel;
// Incremental level autosave: entity snapshots kept between ticks by change tick (ADR-7748).
pub mod autosave_incremental;
// Fuzz targets of the editor state files: editor_view.ron, the autosave index, tool manifests (w11 item 5).
pub mod state_fuzz;
// The editor window's screen-reader bridge: egui's AccessKit tree to the OS adapter, reader requests back (ADR-7746).
pub mod a11y_bridge;
// The status bar's indicators and the main toolbar's Quick settings (`editor.status_bar`, ADR-7754).
pub mod shell_status;
// The content and log drawers over the dock area (`editor.drawer`, ADR-7754).
pub mod drawers;
// The console line under the Output Log (`editor.console_run`, `editor.console_complete`, ADR-7754).
pub mod console;
// Viewport camera navigation: invert, speed steps, orbit around the selection, zoom toward the
// cursor, navigation presets, camera to selection (ADR-7755).
pub mod camera_nav;
// Its agent surface (ADR-7755).
pub mod camera_nav_agent;
// Content Browser navigation: folder tree, breadcrumbs, history, favorites, browser settings (ADR-7759).
pub mod content_favorites;
// Content Browser collections: static lists and saved searches, personal or shared (ADR-7759).
pub mod content_collections;
// Universal search over assets, entities, commands, settings and agent methods (ADR-7766).
pub mod universal_search;
// Saved universal searches, personal and project-shared (ADR-7766).
pub mod saved_searches;
// Rich tooltips for menu items, palette entries, preferences and Inspector fields (ADR-7767).
pub mod tooltips;
// Pilot an entity, look through scene cameras, the viewport's Camera menu (ADR-7768).
pub mod viewport_pilot;
// Picture-in-picture preview of the selected scene camera (ADR-7768).
pub mod camera_preview;
// File ▸ Screenshot…: high-resolution viewport screenshots (ADR-7768).
pub mod screenshot;
// Pick from the stack under the cursor, select similar and the selection root (P-073, P-065,
// P-074; ADR-7769).
pub mod pick_stack;
// The viewport's measure tool (P-069, ADR-7769).
pub mod measure_tool;
// The viewport pane badges: realtime off, non-default pane settings with reset (ADR-7770).
pub mod viewport_badges;
// Window ▸ Rendering Debug: view modes, render overrides, pass timings, legend (ADR-7770).
pub mod render_debug_panel;
// The shared preview scene of the asset editors: lighting presets, environment intensity, floor and
// auto-rotate, `editor.preview_scene` (ADR-7771).
pub mod preview_scene;
// Outliner rows (ADR-7772): configuration warnings, change kind in place, columns, tags, row
// colors, badges and the unique-name marker.
pub mod change_kind;
pub mod config_checks;
pub mod outliner_columns;
// The editor's persisted files as frozen formats: the live shapes behind regia_core::editor_formats (ADR-7902).
pub mod user_formats;
// The Bulk Edit panel: a spreadsheet of entities or data assets by one type's fields, TSV copy
// and paste, fill down, one undo step per commit (`editor.bulk_edit_*`, ADR-7787).
pub mod bulk_edit_panel;
// OS file drops onto the editor (spec addendum § 8.7, ADR-7788).
pub mod os_drop;
// Window ▸ Asset Jobs (spec addendum § 8.9, ADR-7788).
pub mod asset_jobs_panel;
// Export with dependencies and Import package (spec addendum § 8.9, ADR-7788).
pub mod asset_package;
// Window ▸ Project Health: one ranked list of the project's checks (ADR-7789).
pub mod project_health;
// The prefab Overrides window and Edit in place (UI parity addendum § 9.1-9.2, ADR-7790).
pub mod prefab_overrides_panel;
// Asset and graph diff against a git revision, and its read-only window (P-182, ADR-7799).
pub mod asset_diff;
pub mod diff_view;
// The World Map: a streamed level's cells loaded and unloaded in the edit world, saved per cell
// (`editor.world_*`, P-241, ADR-7802).
pub mod world_edit;
pub mod world_panel;

// Project debug palette, shared by settings and the agent (W12).
mod debug_colors;
/// Modular kit palette and grid painting.
pub mod kit_paint;
/// Terrain rectangle selection, clipboard and heightmap interchange.
pub mod terrain_region;

// Polygon BSP blockout evaluation (ADR-7809).
pub mod csg;

/// Blockout brush authoring and background baking.
pub mod blockout;

/// Isolated, sample-driven UI designer runtime and source inspection.
pub mod ui_designer_interactive;

/// Runtime-rendered theme gallery and document preview pages.
pub mod theme_gallery;
// Content Browser selection list, bulk actions, retarget / delete with replace, rich tooltip (ADR-7822).
pub mod content_actions;
// The Save all dialog: every dirty document with checkboxes, used by File ▸ Save all and the close (ADR-7822).
pub mod save_all_dialog;
// The Outliner and Inspector on a launched or separate-process game (ADR-7833).
pub mod remote_world;
// Live link: edit-world edits mirrored into a running session by entity guid (ADR-7833).
pub mod live_link;

/// Undo history dock panel.
pub mod history_panel;

/// Recovery startup policy, markers and banner.
pub mod safe_mode;

/// Frozen-frame inspection and output previews.
pub mod frame_debugger;
// Revision control: git status badges, the Revision Control window, `editor.vcs_*` (ADR-7837).
pub mod vcs;
// View ▸ Color Vision for the editor chrome and the viewport (`editor.color_vision`, ADR-7838).
pub mod chrome_color_vision;

/// Named project launch recipes and their dialog.
pub mod launch_profiles;
/// Platform setup checks and actionable fixes.
pub mod platforms_panel;

/// Desktop and Android package byte reports.
pub mod size_report_panel;

// Selected gameplay inspection and bounded visual-log playback (P-319/P-320).
pub mod debug_overlay_selected;
pub mod visual_log_panel;

/// Session activity timeline (ADR-7841).
pub mod activity_panel;
/// Level review notes and their editor controls (ADR-7841).
pub mod review_notes;
// Floated panels as OS windows; single-window mode (ADR-7821).
pub mod os_windows;
// Level variants: File ▸ New level variant…, diff saves, override marks, Revert to base (ADR-7823).
pub mod level_variant;
// The Terrain panel's non-destructive edit layers (P-236, ADR-7825).
pub mod terrain_layers;
// Play start options (player start, editor camera, a chosen start), time scale and mute (ADR-7829).
pub mod pie_controls;
// Device simulation in the play window: presets with cutout and safe area, touch emulation, a virtual gamepad (ADR-7829).
pub mod play_device;
// Game dock tabs: a separate-process play session shown in the editor (ADR-7831).
pub mod pie_process_view;

// Explicit LAN admission and editor presence (ADR-7842).
pub mod collab;

/// The in-editor hosted Agent panel.
pub mod agent_panel;
/// Hosted agent session lifecycle and protocol.
pub mod agent_sessions;
// The project browser's per-project tags, thumbnails and search, and the New level dialog's
// templates (`editor.recent_projects`, `editor.recent_projects_edit`, `editor.new_level`,
// `editor.level_templates`; ADR-7856, `book/agents/project-browser.md`).
pub mod level_templates;
pub mod recent_meta;
// The samples browser: Help ▸ Samples… and `editor.samples` (w13). (A `///` here would join the
// module's own docs and resolve their links in this scope: rustdoc -D warnings.)
pub mod samples_browser;
// Import presets and the import dialog (P-150/P-151, ADR-7904).
pub mod import_dialog;
pub mod import_presets;

/// Read-only glTF geometry and rig diagnostics over the agent protocol.
pub mod gltf_lint_agent;

// Source-material check/fix extension methods (ADR-8061).
mod gltf_material_lint_agent;

/// Read-only source-lint policy and rule catalog (ADR-8248).
pub mod content_lint_agent;

/// Cross-project user asset library.
pub mod content_library;

/// The Profiler's GPU resource browser with previews (ADR-8142).
pub mod gpu_browser;
/// The Profiler's Memory tab: categories with current and peak bytes, named snapshots and their
/// diff (ADR-8142).
pub mod memory_browser;
// Panel instances: several Outliners, Content Browsers and Inspectors (P-003, ADR-8105 (a)); the
// module documents itself (see `samples_browser`).
pub mod panel_instances;
/// Prefab edit mode's logic actions: Open graph / New graph for this prefab (P-165, ADR-8566).
pub mod prefab_logic;
// The Profiler's percent-of-frame mode, subsystem modules, monitors, loads and marks (P-304, P-307,
// P-312, P-314, P-318; ADR-8141).
pub mod profiler_modules;
/// Asset panels on the shared undo history (P-322 part b).
mod shared_panel_history;
/// Utility panels: UI-designer documents with a logic graph or Lua driver as dock panels (ADR-8189).
pub mod utility_panel;
/// Movable viewport control groups and per-user presets.
pub mod viewport_overlays;
// Version history of levels and documents, project duplicate (File ▸ Version History, ADR-7928).
pub mod version_history;

/// Source-lint jobs, Problems rows, and threshold edits (ADR-8391).
pub mod content_lint_jobs;
/// The dev transport between the editor and a game it launched on an Android device (ADR-8472).
pub mod device_transport;
/// The viewport's fog of war debug view (Show ▸ Fog of war; audit gap FW-1).
pub mod fog_debug;
/// Package-hygiene policy, waivers and audit review (ADR-8399).
pub mod hygiene_policy;
// The graph toolkit every node-graph panel shares (P-186, w13-ui-graph-toolkit).
pub mod graph_kit;
// Content-chunk closure settings: content mode and entry roots, previews, unshipped pages (CT-26, ADR-8543).
pub mod closure_settings;
#[cfg(test)]
mod first_use_tests;
/// Preview renders off the UI thread: the Material Graph and rig previews, the camera preview warm-up (ADR-8584).
pub mod preview_worker;
// Default positions of the floating tool windows: a cascade inside the dock area (ADR-8589).
pub mod tool_windows;
// Terrain ▸ Import source (ADR-8541): count-source preview, staged off-thread import, guarded apply,
// cancel and reimport.
pub mod terrain_source_import;
// Terrain ▸ Overview (ADR-8596): registered overview bake as a cancellable job.
pub mod terrain_overview_job;
// The Match Rules panel: rule sets, playlists, the Game Rules picker (ADR-8738).
pub mod match_rules_panel;
// Play-in-editor match controls, the live match view and the match-rules overlays (ADR-8738).
pub mod match_rules_play;
// The editor's window and taskbar icon and its desktop app id (ADR-20000).
pub mod app_icon;
