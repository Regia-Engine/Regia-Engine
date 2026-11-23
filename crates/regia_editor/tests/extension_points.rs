// SPDX-License-Identifier: MPL-2.0
//! Extension points of the editor (ADR-7791, spec addendum § 20.1-20.3): commands with categories
//! and default shortcuts (palette, Keymap, conflicts), typed settings, selection predicates on
//! menu entries, viewport tools with handles and undo, overlays, build and launch hooks, and a
//! custom inspector. One linked test extension in a headless editor; no window, no GPU.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia_agent::extension::AgentExtension;
use regia_agent::protocol::RpcError;
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_ecs::reflect::{PartialReflect, TypePath};
use regia_editor::command::{EditorCommand, UndoStack};
use regia_editor::editor_prefs::PrefValue;
use regia_editor::extension::actions::{self, ActionPlace, MenuSelection, SelectionKind, SelectionPredicate};
use regia_editor::extension::hooks::{BuildHookDesc, HookInput, HookOutput, HookStage};
use regia_editor::extension::settings::{SettingDesc, SettingScope};
use regia_editor::extension::viewport_tools::{
    OverlayCorner, OverlayDesc, ToolDrag, ToolPhase, ViewportToolDesc,
};
use regia_editor::extension::{
    ActionDesc, EditorExtensionPlugin, InspectorCustomization, ToolExtensionContext,
};
use regia_editor::handles::Handle;
use regia_framework::extension::discover::MANIFEST_FILE;
use regia_framework::extension::{
    ExtensionContext, ExtensionError, ExtensionRegistry, ExtensionState, RegiaExtension,
};
use regia_scene::prelude::Name;
use serde_json::{Value, json};

const ID: &str = "points";

/// Where the test tool's one handle sits.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Default)]
struct Lift(Vec3);

/// How often `points.greet` ran.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Greets(u32);

/// A reflected value the custom inspector draws.
#[derive(Reflect, Debug, Clone, PartialEq, Default)]
struct Tint {
    level: f32,
}

struct SetLift {
    old: Vec3,
    new: Vec3,
}

impl EditorCommand for SetLift {
    fn apply(&mut self, world: &mut World) {
        world.insert_resource(Lift(self.new));
    }

    fn revert(&mut self, world: &mut World) {
        world.insert_resource(Lift(self.old));
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        "Lift".to_owned()
    }
}

#[derive(Default)]
struct Points;

fn greet(world: &mut World) -> Result<String, String> {
    let mut greets = world.get_resource_or_init::<Greets>();
    greets.0 += 1;
    Ok(format!("Greeted {}", greets.0))
}

fn mark(world: &mut World) -> Result<String, String> {
    let selection = world.get_resource::<MenuSelection>().cloned().unwrap_or_default();
    Ok(format!(
        "Marked {} entities, {} assets",
        selection.entities.len(),
        selection.assets.len()
    ))
}

impl RegiaExtension for Points {
    fn id(&self) -> &str {
        ID
    }

    fn build(&self, cx: &mut ExtensionContext<'_>) -> Result<(), ExtensionError> {
        let mut editor = ToolExtensionContext::new(cx)?;
        editor.register_editor_action(ActionDesc {
            category: "Points".into(),
            shortcut: "Ctrl+Alt+G".into(),
            ..ActionDesc::new(format!("{ID}.greet"), "Greet", Arc::new(greet))
        })?;
        // A default shortcut a built-in command already has: listed as a conflict, never run.
        editor.register_editor_action(ActionDesc {
            shortcut: "Ctrl+Z".into(),
            ..ActionDesc::new(format!("{ID}.clash"), "Clash", Arc::new(greet))
        })?;
        editor.register_editor_action(ActionDesc {
            places: vec![ActionPlace::Outliner, ActionPlace::Viewport],
            enabled_when: Some(SelectionPredicate {
                kinds: vec![SelectionKind::Entity],
                components: vec!["Name".into()],
                min_count: 1,
                max_count: Some(1),
                ..SelectionPredicate::default()
            }),
            ..ActionDesc::new(format!("{ID}.mark"), "Mark", Arc::new(mark))
        })?;
        editor.register_editor_action(ActionDesc {
            places: vec![ActionPlace::ContentBrowser],
            visible_when: Some(SelectionPredicate {
                kinds: vec![SelectionKind::Asset],
                asset_suffixes: vec![".ron".into()],
                min_count: 1,
                ..SelectionPredicate::default()
            }),
            ..ActionDesc::new(format!("{ID}.mark_asset"), "Mark asset", Arc::new(mark))
        })?;
        editor.register_setting(SettingDesc {
            key: format!("{ID}.speed"),
            title: "Speed".into(),
            description: "How fast points move.".into(),
            scope: SettingScope::User,
            default: PrefValue::Float(2.0),
            min: Some(0.0),
            max: Some(10.0),
            choices: Vec::new(),
        })?;
        editor.register_setting(SettingDesc {
            key: format!("{ID}.mode"),
            title: "Mode".into(),
            description: "Which points to show.".into(),
            scope: SettingScope::Project,
            default: PrefValue::Text("all".into()),
            min: None,
            max: None,
            choices: vec!["all".into(), "selected".into()],
        })?;
        editor.register_viewport_tool(ViewportToolDesc {
            id: format!("{ID}.lift"),
            title: "Lift".into(),
            icon: "↑".into(),
            handles: Arc::new(|world: &World| {
                let at = world.get_resource::<Lift>().copied().unwrap_or_default().0;
                vec![Handle::Point { pos: at }]
            }),
            drag: Arc::new(|world: &mut World, drag: &ToolDrag| {
                let old = drag.position - drag.total;
                world.insert_resource(Lift(drag.position));
                (drag.phase == ToolPhase::End && drag.total != Vec3::ZERO).then(|| {
                    Box::new(SetLift {
                        old,
                        new: drag.position,
                    }) as Box<dyn EditorCommand>
                })
            }),
        })?;
        editor.register_overlay(OverlayDesc {
            id: format!("{ID}.hud"),
            title: "Points HUD".into(),
            corner: OverlayCorner::TopRight,
            draw: Arc::new(|ui: &mut egui::Ui, world: &mut World| {
                let at = world.get_resource::<Lift>().copied().unwrap_or_default().0;
                ui.label(format!("Lift at {:.1}", at.y));
            }),
        })?;
        editor.register_build_hook(BuildHookDesc {
            id: format!("{ID}.stamp"),
            title: "Stamp".into(),
            stage: HookStage::BeforeCook,
            run: Arc::new(|input: &HookInput, output: &mut HookOutput| {
                if input.profile == "shipping" {
                    return Err("shipping builds are vetoed".into());
                }
                output.log.push("stamped".into());
                output
                    .files
                    .push(("generated/stamp.txt".into(), input.profile.clone().into_bytes()));
                Ok(())
            }),
        })?;
        editor.register_build_hook(BuildHookDesc {
            id: format!("{ID}.readme"),
            title: "Readme".into(),
            stage: HookStage::AfterPackage,
            run: Arc::new(|input: &HookInput, output: &mut HookOutput| {
                output.files.push((
                    "README.txt".into(),
                    format!("{} build", input.target).into_bytes(),
                ));
                Ok(())
            }),
        })?;
        editor.register_build_hook(BuildHookDesc {
            id: format!("{ID}.veto"),
            title: "Veto".into(),
            stage: HookStage::BeforeLaunch,
            run: Arc::new(|input: &HookInput, _: &mut HookOutput| {
                if input.target == "android" {
                    Err("no phones today".into())
                } else {
                    Ok(())
                }
            }),
        })?;
        editor.register_inspector(InspectorCustomization {
            type_path: Tint::type_path().to_owned(),
            draw: Arc::new(|ui: &mut egui::Ui, value: &mut dyn PartialReflect| {
                ui.label("Tint slider");
                let Some(tint) = value.try_downcast_mut::<Tint>() else {
                    return false;
                };
                tint.level = 0.5;
                true
            }),
        })?;
        Ok(())
    }
}

struct Project(PathBuf);

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
    std::fs::write(path, text).expect("write");
}

fn project(tag: &str) -> Project {
    let dir = regia_test_utils::unique_temp_dir(tag);
    write(
        &dir.join("Regia.toml"),
        "[project]\nname = \"points\"\n\n[extensions]\nenabled = [\"points\"]\n\n[extensions.grants]\npoints = [\"editor_panel\"]\n",
    );
    write(
        &dir.join("extensions/points").join(MANIFEST_FILE),
        "format_version = 1\nid = \"points\"\nname = \"Points tools\"\nversion = \"0.1.0\"\ncapabilities = [\"editor_panel\"]\n\n[[code]]\ncrate = \"regia_ext_points\"\naudience = [\"editor\"]\n",
    );
    write(&dir.join("assets/data/table.ron"), "()\n");
    Project(dir)
}

fn editor(project: &Project) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(EditorExtensionPlugin {
        project: Some(project.0.clone()),
        target_dir: Some(project.0.join("no-target")),
        engine_dir: None,
        linked: vec![Arc::new(Points)],
    });
    let world = app.world_mut();
    world.init_resource::<regia_editor::anim::AnimEditor>();
    world.init_resource::<UndoStack>();
    regia_editor::agent_ext::install(world);
    world.insert_resource(regia_editor::agent_ext::EditorProject {
        root: Some(project.0.clone()),
    });
    app.update();
    assert_eq!(
        app.world().resource::<ExtensionRegistry>().state(ID),
        Some(ExtensionState::Active),
        "{}",
        app.world().resource::<ExtensionRegistry>().list_json(None)
    );
    app
}

fn call(app: &mut App, method: &str, params: Value) -> Result<Value, RpcError> {
    let extension = app.world().resource::<AgentExtension>().clone();
    let (_, handler) = extension
        .method(method)
        .unwrap_or_else(|| panic!("{method} is not registered"));
    handler(app.world_mut(), &params)
}

fn code(result: Result<Value, RpcError>) -> String {
    match result {
        Ok(value) => panic!("expected an error, got {value}"),
        Err(error) => error.data.as_ref().expect("data")["code"]
            .as_str()
            .expect("code")
            .to_owned(),
    }
}

fn key_press(key: egui::Key, modifiers: egui::Modifiers) -> egui::RawInput {
    egui::RawInput {
        events: vec![
            egui::Event::ModifiersChanged(modifiers),
            egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            },
        ],
        ..Default::default()
    }
}

/// Runs one egui frame and drops its font atlas upload (nothing paints).
fn frame(ctx: &egui::Context, input: egui::RawInput, run: impl FnMut(&mut egui::Ui)) {
    let mut output = ctx.run_ui(input, run);
    output.textures_delta.clear();
}

#[test]
fn commands_have_categories_and_shortcuts_in_the_palette_and_the_keymap() {
    let p = project("ext-points-commands");
    let mut app = editor(&p);

    // The palette ranks extension commands with the built-in ones, under their category.
    let rows = actions::command_defs(app.world());
    let greet = rows.iter().find(|d| d.id == "points.greet").expect("greet row");
    assert_eq!(
        (greet.category, greet.title, greet.shortcut),
        ("Points", "Greet", "Ctrl+Alt+G")
    );
    let palette = regia_editor::command_palette::CommandPalette {
        extension_rows: rows.clone(),
        ..Default::default()
    };
    let ranked = palette.rank("points greet");
    assert_eq!(
        ranked.first().map(|r| r.def.id),
        Some("points.greet"),
        "{ranked:?}"
    );
    // A command without a category is filed under its extension.
    let clash = rows.iter().find(|d| d.id == "points.clash").expect("clash row");
    assert_eq!(clash.category, "Points tools");

    // editor.command runs it by id.
    assert_eq!(
        regia_editor::command_palette::run(app.world_mut(), "points.greet").as_deref(),
        Some("Greeted 1")
    );

    // The default shortcut runs it; Ctrl+Z stays undo's, and the clash is listed.
    let ctx = egui::Context::default();
    let mut status = None;
    frame(
        &ctx,
        key_press(
            egui::Key::G,
            egui::Modifiers {
                alt: true,
                ctrl: true,
                command: true,
                ..Default::default()
            },
        ),
        |ui| status = actions::dispatch_shortcuts(ui.ctx(), app.world_mut()),
    );
    assert_eq!(status.as_deref(), Some("Greeted 2"));
    let ui = call(&mut app, "editor.extension_ui", json!({})).expect("ui");
    let listed = ui["actions"]
        .as_array()
        .expect("actions")
        .iter()
        .find(|a| a["id"] == "points.greet")
        .expect("greet listed")
        .clone();
    assert_eq!(listed["category"], "Points");
    assert_eq!(listed["shortcut"], "Ctrl+Alt+G");
    let conflicts = ui["shortcut_conflicts"].as_array().expect("conflicts");
    assert!(
        conflicts
            .iter()
            .any(|c| c["id"] == "points.clash" && c["with"].as_str().is_some_and(|w| w.contains("edit.undo"))),
        "{conflicts:?}"
    );
    let ctx = egui::Context::default();
    let mut status = None;
    frame(
        &ctx,
        key_press(
            egui::Key::Z,
            egui::Modifiers {
                ctrl: true,
                command: true,
                ..Default::default()
            },
        ),
        |ui| status = actions::dispatch_shortcuts(ui.ctx(), app.world_mut()),
    );
    assert_eq!(status, None, "a conflicting default never runs");

    // Rebinding: conflict-checked, then saved with the keymap.
    assert_eq!(
        code(call(
            &mut app,
            "editor.extension_shortcut",
            json!({ "id": "points.greet", "keys": "Ctrl+Z" })
        )),
        "extension_shortcut_conflict"
    );
    assert_eq!(
        code(call(
            &mut app,
            "editor.extension_shortcut",
            json!({ "id": "points.greet", "keys": "Ctrl+Shift+P" })
        )),
        "extension_shortcut_reserved"
    );
    assert_eq!(
        code(call(
            &mut app,
            "editor.extension_shortcut",
            json!({ "id": "points.nope", "keys": "Ctrl+Alt+H" })
        )),
        "extension_action_not_found"
    );
    let config = regia_test_utils::unique_temp_dir("ext-points-keymap");
    app.world_mut()
        .insert_resource(regia_editor::editor_prefs::EditorConfigPaths::at(&config));
    regia_editor::editor_keymap::load_keymap(app.world_mut());
    let bound = call(
        &mut app,
        "editor.extension_shortcut",
        json!({ "id": "points.greet", "keys": "ctrl+alt+h" }),
    )
    .expect("bind");
    assert_eq!(bound["shortcut"], "Ctrl+Alt+H");
    assert_eq!(bound["saved"], true);
    let file = std::fs::read_to_string(config.join("editor_keymap.ron")).expect("keymap file");
    assert!(file.contains("\"points.greet\": \"Ctrl+Alt+H\""), "{file}");
    // A cleared shortcut stays cleared (saved as an empty chord) and the default no longer runs.
    call(
        &mut app,
        "editor.extension_shortcut",
        json!({ "id": "points.greet", "clear": true }),
    )
    .expect("clear");
    let ctx = egui::Context::default();
    let mut status = None;
    frame(
        &ctx,
        key_press(
            egui::Key::G,
            egui::Modifiers {
                alt: true,
                ctrl: true,
                command: true,
                ..Default::default()
            },
        ),
        |ui| status = actions::dispatch_shortcuts(ui.ctx(), app.world_mut()),
    );
    assert_eq!(status, None);
    let file = std::fs::read_to_string(config.join("editor_keymap.ron")).expect("keymap file");
    assert!(file.contains("\"points.greet\": \"\""), "{file}");
    let _ = std::fs::remove_dir_all(&config);
}

#[test]
fn menu_entries_follow_their_selection_predicates() {
    let p = project("ext-points-predicates");
    let mut app = editor(&p);
    let named = app.world_mut().spawn(Name::new("a")).id();
    let other = app.world_mut().spawn(Name::new("b")).id();
    let bare = app.world_mut().spawn_empty().id();

    let state = |app: &App, id: &str, selection: &MenuSelection| {
        let (_, action) = actions::actions(app.world())
            .into_iter()
            .find(|(_, a)| a.id == id)
            .expect("action");
        actions::action_state(app.world(), &action, selection)
    };
    let entities = |list: &[Entity]| MenuSelection {
        entities: list.to_vec(),
        assets: Vec::new(),
    };
    // Enabled for exactly one entity with a Name; shown regardless.
    assert_eq!(state(&app, "points.mark", &entities(&[])), (true, false));
    assert_eq!(state(&app, "points.mark", &entities(&[named])), (true, true));
    assert_eq!(
        state(&app, "points.mark", &entities(&[named, other])),
        (true, false)
    );
    assert_eq!(state(&app, "points.mark", &entities(&[bare])), (true, false));
    // Shown only for .ron assets.
    let asset = |path: &str| MenuSelection {
        entities: Vec::new(),
        assets: vec![path.to_owned()],
    };
    assert_eq!(
        state(&app, "points.mark_asset", &asset("assets/data/table.ron")),
        (true, true)
    );
    assert_eq!(
        state(&app, "points.mark_asset", &asset("assets/a.png")),
        (false, false)
    );
    assert_eq!(
        state(&app, "points.mark_asset", &entities(&[named])),
        (false, false)
    );

    // Running follows the predicate: the level selection is what the main menu, the palette, the
    // shortcuts and the agent use.
    assert_eq!(
        code(call(
            &mut app,
            "editor.extension_action",
            json!({ "id": "points.mark" })
        )),
        "extension_action_disabled"
    );
    app.world_mut()
        .get_resource_or_insert_with(regia_editor::selection::Selection::default)
        .select(named, regia_editor::selection::SelectMode::Replace);
    let ran = call(
        &mut app,
        "editor.extension_action",
        json!({ "id": "points.mark" }),
    )
    .expect("mark");
    assert_eq!(ran["message"], "Marked 1 entities, 0 assets");
    // The agent can run an action as the Content Browser's menu does, on assets.
    let ran = call(
        &mut app,
        "editor.extension_action",
        json!({ "id": "points.mark_asset", "assets": ["assets/data/table.ron"] }),
    )
    .expect("mark asset");
    assert_eq!(ran["message"], "Marked 0 entities, 1 assets");

    // The context menus list the entries of their place.
    let ui = call(&mut app, "editor.extension_ui", json!({})).expect("ui");
    let mark = ui["actions"]
        .as_array()
        .expect("actions")
        .iter()
        .find(|a| a["id"] == "points.mark")
        .expect("mark")
        .clone();
    assert_eq!(mark["places"], json!(["outliner", "viewport"]));
    assert_eq!(mark["enabled"], true);
    assert_eq!(mark["enabled_when"]["components"], json!(["Name"]));
    let in_outliner: Vec<String> = actions::place_actions(app.world(), ActionPlace::Outliner)
        .into_iter()
        .map(|(_, a)| a.id)
        .collect();
    assert_eq!(in_outliner, ["points.mark"]);
}

#[test]
fn settings_are_typed_scoped_and_readable_through_the_api() {
    let p = project("ext-points-settings");
    let mut app = editor(&p);
    let listed = call(&mut app, "editor.extension_settings", json!({})).expect("list");
    let rows = listed["settings"].as_array().expect("settings");
    assert_eq!(rows.len(), 2, "{rows:?}");
    let speed = rows.iter().find(|r| r["key"] == "points.speed").expect("speed");
    assert_eq!(speed["scope"], "user");
    assert_eq!(speed["value"], 2.0);
    assert_eq!(speed["default"], 2.0);
    assert_eq!(speed["min"], 0.0);
    assert_eq!(speed["max"], 10.0);
    assert_eq!(speed["extension"], "Points tools");
    assert_eq!(speed["description"], "How fast points move.");

    // User scope: kept with the editor preferences.
    call(
        &mut app,
        "editor.extension_settings",
        json!({ "key": "points.speed", "value": 4.5 }),
    )
    .expect("set");
    assert_eq!(
        regia_editor::extension::settings::get(app.world(), "points.speed"),
        Some(PrefValue::Float(4.5))
    );
    assert_eq!(
        app.world()
            .resource::<regia_editor::editor_prefs::EditorPreferences>()
            .extra("extension.points.speed"),
        Some(&PrefValue::Float(4.5))
    );
    assert_eq!(
        code(call(
            &mut app,
            "editor.extension_settings",
            json!({ "key": "points.speed", "value": 11.0 })
        )),
        "extension_setting_invalid"
    );
    assert_eq!(
        code(call(
            &mut app,
            "editor.extension_settings",
            json!({ "key": "points.speed", "value": "fast" })
        )),
        "extension_setting_invalid"
    );
    assert_eq!(
        code(call(
            &mut app,
            "editor.extension_settings",
            json!({ "key": "points.nope", "value": 1 })
        )),
        "extension_setting_not_found"
    );

    // Project scope: written to Regia.toml's [extensions.settings.points], where the extension's
    // `ExtensionContext::settings` reads it.
    call(
        &mut app,
        "editor.extension_settings",
        json!({ "key": "points.mode", "value": "selected" }),
    )
    .expect("set");
    assert_eq!(
        code(call(
            &mut app,
            "editor.extension_settings",
            json!({ "key": "points.mode", "value": "some" })
        )),
        "extension_setting_invalid"
    );
    let manifest = std::fs::read_to_string(p.0.join("Regia.toml")).expect("manifest");
    let table: toml::Table = manifest.parse().expect("toml");
    assert_eq!(
        table["extensions"]["settings"]["points"]["mode"].as_str(),
        Some("selected"),
        "{manifest}"
    );
    assert_eq!(
        table["extensions"]["grants"]["points"].as_array().map(Vec::len),
        Some(1),
        "the rest of Regia.toml stays"
    );
    assert_eq!(
        regia_editor::extension::settings::get(app.world(), "points.mode"),
        Some(PrefValue::Text("selected".into()))
    );
    let reset = call(
        &mut app,
        "editor.extension_settings",
        json!({ "key": "points.mode", "reset": true }),
    )
    .expect("reset");
    assert_eq!(reset["value"], "all");
    let manifest = std::fs::read_to_string(p.0.join("Regia.toml")).expect("manifest");
    assert!(!manifest.contains("selected"), "{manifest}");
}

#[test]
fn a_viewport_tool_drags_its_handle_as_one_undo_step_and_overlays_draw() {
    let p = project("ext-points-tools");
    let mut app = editor(&p);
    let listed = call(&mut app, "editor.extension_tools", json!({})).expect("tools");
    assert_eq!(
        listed["tools"],
        json!([{ "id": "points.lift", "title": "Lift", "icon": "↑", "owner": "points", "active": false }])
    );
    assert_eq!(
        listed["overlays"],
        json!([{ "id": "points.hud", "title": "Points HUD", "corner": "top_right", "owner": "points", "shown": true }])
    );
    assert_eq!(
        code(call(
            &mut app,
            "editor.extension_tools",
            json!({ "drag": { "handle": 0, "to": [0.0, 2.0, 0.0] } })
        )),
        "extension_tool_inactive"
    );
    assert_eq!(
        code(call(
            &mut app,
            "editor.extension_tools",
            json!({ "activate": "points.nope" })
        )),
        "extension_tool_not_found"
    );
    let active = call(
        &mut app,
        "editor.extension_tools",
        json!({ "activate": "points.lift" }),
    )
    .expect("activate");
    assert_eq!(active["active"], "points.lift");
    assert_eq!(active["handles"], json!([[0.0, 0.0, 0.0]]));
    assert_eq!(
        code(call(
            &mut app,
            "editor.extension_tools",
            json!({ "drag": { "handle": 3, "to": [0.0, 2.0, 0.0] } })
        )),
        "extension_tool_handle_unknown"
    );
    let dragged = call(
        &mut app,
        "editor.extension_tools",
        json!({ "drag": { "handle": 0, "to": [0.0, 2.0, 0.0], "steps": 4 } }),
    )
    .expect("drag");
    assert_eq!(dragged["undo_entry"], true);
    assert_eq!(app.world().resource::<Lift>().0, Vec3::new(0.0, 2.0, 0.0));
    assert_eq!(
        app.world().resource::<UndoStack>().undo_depth(),
        1,
        "one drag is one undo step"
    );
    assert!(regia_editor::command::undo_world(app.world_mut()));
    assert_eq!(app.world().resource::<Lift>().0, Vec3::ZERO);

    let text = call(
        &mut app,
        "editor.extension_tools",
        json!({ "overlay_text": "points.hud" }),
    )
    .expect("overlay");
    assert_eq!(text["text"], json!(["Lift at 0.0"]));
    let hidden = call(
        &mut app,
        "editor.extension_tools",
        json!({ "overlay": "points.hud", "shown": false }),
    )
    .expect("hide");
    assert_eq!(hidden["overlays"][0]["shown"], false);
    assert_eq!(
        code(call(
            &mut app,
            "editor.extension_tools",
            json!({ "overlay": "points.nope", "shown": true })
        )),
        "extension_overlay_not_found"
    );

    // The tool palette entry is a focusable menu entry: Tab reaches it and Enter turns it off.
    call(
        &mut app,
        "editor.extension_tools",
        json!({ "activate": "points.lift" }),
    )
    .expect("activate");
    let ctx = egui::Context::default();
    let mut toggled = false;
    for input in [
        key_press(egui::Key::Tab, egui::Modifiers::NONE),
        egui::RawInput::default(),
        key_press(egui::Key::Enter, egui::Modifiers::NONE),
    ] {
        frame(&ctx, input, |ui| {
            toggled |= regia_editor::extension::viewport_tools::tool_buttons(ui, app.world_mut())
        });
    }
    assert!(toggled, "the keyboard toggled the tool");
    let after = call(&mut app, "editor.extension_tools", json!({})).expect("tools");
    assert_eq!(after["active"], Value::Null);
}

#[test]
fn build_and_launch_hooks_add_files_log_and_abort() {
    let p = project("ext-points-hooks");
    let mut app = editor(&p);
    let ui = call(&mut app, "editor.extension_ui", json!({})).expect("ui");
    assert_eq!(
        ui["hooks"],
        json!([
            { "id": "points.readme", "title": "Readme", "stage": "after_package", "owner": "points" },
            { "id": "points.stamp", "title": "Stamp", "stage": "before_cook", "owner": "points" },
            { "id": "points.veto", "title": "Veto", "stage": "before_launch", "owner": "points" },
        ])
    );

    // Before cook: a shipping package is refused before anything runs.
    let refused = regia_editor::package_job::start_package(
        app.world_mut(),
        &regia_editor::package_job::PackageRequest {
            target: "linux".into(),
            profile: "shipping".into(),
            out: None,
            dry_run: false,
        },
    )
    .expect_err("vetoed");
    assert_eq!(refused.reason, "package_hook_aborted");
    assert!(
        refused
            .message
            .contains("points.stamp: shipping builds are vetoed"),
        "{refused:?}"
    );
    assert!(!p.0.join("generated/stamp.txt").exists());

    // The stage alone: the file lands in the project and the log line names the hook.
    let report = regia_editor::extension::hooks::run_stage(
        app.world(),
        &HookInput {
            stage: HookStage::BeforeCook,
            project: p.0.clone(),
            target: "linux".into(),
            profile: "dev".into(),
            out: None,
        },
    );
    assert_eq!(report.aborted, None);
    assert_eq!(report.log, ["points.stamp: stamped"]);
    assert_eq!(
        std::fs::read_to_string(p.0.join("generated/stamp.txt")).expect("stamp"),
        "dev"
    );
    // After package: relative to the output folder.
    let out = p.0.join("dist/linux");
    let report = regia_editor::extension::hooks::run_stage(
        app.world(),
        &HookInput {
            stage: HookStage::AfterPackage,
            project: p.0.clone(),
            target: "linux".into(),
            profile: "dev".into(),
            out: Some(out.clone()),
        },
    );
    assert_eq!(report.files, ["README.txt"]);
    assert_eq!(
        std::fs::read_to_string(out.join("README.txt")).expect("readme"),
        "linux build"
    );

    // Before launch: the abort is the launch's refusal.
    let error =
        regia_editor::launch::launch(app.world_mut(), regia_editor::launch::LaunchTarget::Android, None)
            .expect_err("vetoed");
    assert_eq!(error.code, "launch_hook_aborted");
    assert!(
        error.message.contains("points.veto: no phones today"),
        "{error:?}"
    );
}

#[test]
fn a_registered_inspector_draws_and_edits_its_type() {
    let p = project("ext-points-inspector");
    let app = editor(&p);
    let (owner, draw) =
        regia_editor::extension::panel::inspector_for(app.world(), Tint::type_path()).expect("customized");
    assert_eq!(owner, "points");
    let mut tint = Tint { level: 0.0 };
    let ctx = egui::Context::default();
    let mut changed = false;
    let mut output = ctx.run_ui(egui::RawInput::default(), |ui| changed |= draw(ui, &mut tint));
    output.textures_delta.clear();
    assert!(changed);
    assert_eq!(tint.level, 0.5);
    assert!(regia_editor::extension::panel::inspector_for(app.world(), "other::Type").is_none());
    let info = app
        .world()
        .resource::<ExtensionRegistry>()
        .info_json(ID)
        .expect("info");
    assert_eq!(info["registrations"]["inspector"], json!([Tint::type_path()]));
    assert_eq!(info["registrations"]["viewport_tool"], json!(["points.lift"]));
    assert_eq!(info["registrations"]["overlay"], json!(["points.hud"]));
    assert_eq!(
        info["registrations"]["editor_setting"],
        json!(["points.mode", "points.speed"])
    );
    assert_eq!(
        info["registrations"]["build_hook"],
        json!(["points.readme", "points.stamp", "points.veto"])
    );
}

/// Tab presses (one per frame) then Enter, after an idle frame that lets the grid lay out.
fn tab_then_enter(tabs: usize) -> Vec<egui::RawInput> {
    let mut frames = vec![egui::RawInput::default()];
    for _ in 0..tabs {
        frames.push(key_press(egui::Key::Tab, egui::Modifiers::NONE));
    }
    frames.push(egui::RawInput::default());
    frames.push(key_press(egui::Key::Enter, egui::Modifiers::NONE));
    frames
}

#[test]
fn the_keymap_rows_are_operable_from_the_keyboard() {
    let p = project("ext-points-keymap-keys");
    let mut app = editor(&p);
    // Rows go by id: `points.clash` first, whose default (Ctrl+Z) conflicts. Tab reaches its
    // chord field, then Set, then Clear; Enter on Clear removes the shortcut and the conflict.
    let ctx = egui::Context::default();
    for input in tab_then_enter(3) {
        frame(&ctx, input, |ui| actions::keymap_section(ui, app.world_mut()));
    }
    let ui = call(&mut app, "editor.extension_ui", json!({})).expect("ui");
    let clash = ui["actions"]
        .as_array()
        .expect("actions")
        .iter()
        .find(|a| a["id"] == "points.clash")
        .expect("clash")
        .clone();
    assert_eq!(clash["shortcut"], "", "{clash}");
    assert_eq!(ui["shortcut_conflicts"], json!([]));
}

#[test]
fn the_settings_rows_are_operable_from_the_keyboard() {
    let p = project("ext-points-settings-keys");
    let mut app = editor(&p);
    call(
        &mut app,
        "editor.extension_settings",
        json!({ "key": "points.speed", "value": 4.5 }),
    )
    .expect("set");
    // The user section has one row: the speed field, then its Reset button (enabled: not the
    // default). Tab, Tab, Enter resets it.
    let ctx = egui::Context::default();
    for input in tab_then_enter(2) {
        frame(&ctx, input, |ui| {
            regia_editor::extension::settings::settings_section(ui, app.world_mut(), SettingScope::User, "")
        });
    }
    assert_eq!(
        regia_editor::extension::settings::get(app.world(), "points.speed"),
        Some(PrefValue::Float(2.0))
    );
}
