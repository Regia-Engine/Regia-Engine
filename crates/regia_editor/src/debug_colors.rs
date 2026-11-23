// SPDX-License-Identifier: MPL-2.0
//! Project debug palette in the extensible `[editor.debug_colors]` manifest table.
//! UI and agent edits share one atomic, undoable write; runtime code only sees `DebugColors`.

use crate::command::{EditorCommand, UndoStack};
use crate::data_panels::PanelError;
use crate::viewport_agent::Handler;
use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::math::Color;
use regia_ecs::prelude::*;
use regia_framework::debug_gizmos::DebugColors;
use serde_json::{Value, json};
use std::path::PathBuf;

const NAMES: [&str; 6] = ["collider", "trigger", "nav", "path", "contact", "sleeping_body"];
const LABELS: [&str; 6] = [
    "Collider",
    "Trigger",
    "Navigation",
    "Path",
    "Contact",
    "Sleeping body",
];
const HINT: &str = "see book/agents/viewport-flags.md";

#[derive(Resource, Default, Clone)]
struct Overrides(std::collections::BTreeSet<String>);

#[derive(Resource, Default)]
struct Loaded {
    root: Option<PathBuf>,
    frame: Option<u64>,
    error: Option<String>,
}

fn err(code: &str, message: impl Into<String>) -> PanelError {
    PanelError::new(code, message)
}

fn entries(colors: DebugColors) -> [Color; 6] {
    [
        colors.collider,
        colors.trigger,
        colors.nav,
        colors.path,
        colors.contact,
        colors.sleeping_body,
    ]
}

fn palette(colors: [Color; 6]) -> DebugColors {
    let [collider, trigger, nav, path, contact, sleeping_body] = colors;
    DebugColors {
        collider,
        trigger,
        nav,
        nav_links: nav,
        path,
        contact,
        sleeping_body,
    }
}

fn validate(value: &Value) -> Result<[f32; 3], PanelError> {
    let a = value.as_array().filter(|a| a.len() == 3).ok_or_else(|| {
        err(
            "debug_colors_bad_value",
            "color must be three linear RGB numbers in 0..=1",
        )
    })?;
    let mut rgb = [0.0; 3];
    for (out, v) in rgb.iter_mut().zip(a) {
        let n = v
            .as_f64()
            .filter(|n| n.is_finite() && (0.0..=1.0).contains(n))
            .ok_or_else(|| {
                err(
                    "debug_colors_bad_value",
                    "color must be three linear RGB numbers in 0..=1",
                )
            })?;
        *out = n as f32;
    }
    Ok(rgb)
}

fn colors_of(table: &toml::Value) -> Result<DebugColors, PanelError> {
    let mut colors = entries(DebugColors::default());
    let editor = table
        .get("editor")
        .map(|v| {
            v.as_table()
                .ok_or_else(|| err("debug_colors_read", "editor must be a table"))
        })
        .transpose()?;
    if let Some(values) = editor.and_then(|v| v.get("debug_colors")) {
        let values = values
            .as_table()
            .ok_or_else(|| err("debug_colors_read", "editor.debug_colors must be a table"))?;
        for (i, name) in NAMES.iter().enumerate() {
            if let Some(v) = values.get(*name) {
                let value = serde_json::to_value(v).map_err(|e| err("debug_colors_read", e.to_string()))?;
                let [r, g, b] = validate(&value)?;
                colors[i] = Color::rgb(r, g, b);
            }
        }
    }
    let mut out = palette(colors);
    if table
        .get("editor")
        .and_then(|t| t.get("debug_colors"))
        .and_then(|t| t.get("nav"))
        .is_none()
    {
        out.nav_links = DebugColors::default().nav_links;
    }
    Ok(out)
}

fn read(root: &std::path::Path) -> Result<(String, toml::Value), PanelError> {
    let text = std::fs::read_to_string(root.join("Regia.toml"))
        .map_err(|e| err("debug_colors_read", e.to_string()))?;
    let table = toml::from_str(&text).map_err(|e| err("debug_colors_read", e.to_string()))?;
    Ok((text, table))
}

/// Loads once per project; errors are displayed, never silently overwritten with defaults.
pub(crate) fn ensure(world: &mut World) -> Result<DebugColors, PanelError> {
    let root = crate::mesh_source::project_root(world);
    if world.get_resource::<Loaded>().is_some_and(|l| l.root == root) {
        if let Some(error) = world.resource::<Loaded>().error.as_ref() {
            return Err(err("debug_colors_read", error.clone()));
        }
        return Ok(world.get_resource::<DebugColors>().copied().unwrap_or_default());
    }
    let mut overrides = Overrides::default();
    let result = root.as_deref().map_or(Ok(DebugColors::default()), |r| {
        read(r).and_then(|(_, t)| {
            if let Some(values) = t
                .get("editor")
                .and_then(|v| v.get("debug_colors"))
                .and_then(toml::Value::as_table)
            {
                overrides.0.extend(values.keys().cloned());
            }
            colors_of(&t)
        })
    });
    world.insert_resource(overrides);
    world.insert_resource(Loaded {
        root,
        error: result.as_ref().err().map(|e| e.message.clone()),
        ..Default::default()
    });
    if let Ok(colors) = &result {
        world.insert_resource(*colors);
    } else {
        world.remove_resource::<DebugColors>();
    }
    result
}

// Undo only the keys this command changed when another settings writer has saved meanwhile.
// With no intervening save, restore the original bytes (including comments) exactly.
fn merged_undo_text(current: &str, desired: &str, expected: &str) -> Result<String, String> {
    if current == expected {
        return Ok(desired.to_owned());
    }
    let parse = |s: &str| toml::from_str::<toml::Value>(s).map_err(|e| e.to_string());
    let mut current = parse(current)?;
    let desired = parse(desired)?;
    let expected = parse(expected)?;
    let value = |t: &toml::Value, name: &str| {
        t.get("editor")
            .and_then(|t| t.get("debug_colors"))
            .and_then(|t| t.get(name))
            .cloned()
    };
    let root = current.as_table_mut().ok_or("manifest must be a table")?;
    let editor = root
        .entry("editor")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    let editor = editor.as_table_mut().ok_or("editor must be a table")?;
    let colors = editor
        .entry("debug_colors")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    let colors = colors.as_table_mut().ok_or("debug_colors must be a table")?;
    for name in NAMES {
        let want = value(&desired, name);
        if want != value(&expected, name) {
            if let Some(want) = want {
                colors.insert(name.to_owned(), want);
            } else {
                colors.remove(name);
            }
        }
    }
    toml::to_string_pretty(&current).map_err(|e| e.to_string())
}

struct ColorEdit {
    root: PathBuf,
    before: String,
    after: String,
    old: DebugColors,
    new: DebugColors,
}

impl ColorEdit {
    fn put(&self, world: &mut World, text: &str, expected: &str, colors: DebugColors) {
        let write = || -> Result<(), String> {
            let (current, _) = read(&self.root).map_err(|e| e.message)?;
            let text = merged_undo_text(&current, text, expected)?;
            crate::atomic_write::write_atomic(&self.root.join("Regia.toml"), text.as_bytes())
        };
        match write() {
            Ok(()) => {
                world.insert_resource(colors);
                world.remove_resource::<Loaded>();
                let _ = ensure(world);
                world.insert_resource(crate::project_settings_ext::ProjectSettingsSaved);
            }
            Err(e) => tracing::warn!(target: "regia_editor", "debug colors undo write: {e}"),
        }
    }
}

impl EditorCommand for ColorEdit {
    fn apply(&mut self, world: &mut World) {
        self.put(world, &self.after, &self.before, self.new);
    }
    fn revert(&mut self, world: &mut World) {
        self.put(world, &self.before, &self.after, self.old);
    }
    fn merge(&mut self, next: &dyn EditorCommand) -> bool {
        let Some(next) = (next as &dyn std::any::Any).downcast_ref::<Self>() else {
            return false;
        };
        if self.root != next.root || self.after != next.before {
            return false;
        }
        self.after.clone_from(&next.after);
        self.new = next.new;
        true
    }
    fn description(&self) -> String {
        "Project debug colors".to_owned()
    }
    fn changes_level_state(&self) -> bool {
        false
    }
}

fn set(world: &mut World, values: &Value) -> Result<bool, PanelError> {
    let root = crate::mesh_source::project_root(world)
        .ok_or_else(|| err("debug_colors_no_project", "open a project first"))?;
    let values = values
        .as_object()
        .ok_or_else(|| err("debug_colors_bad_value", "colors must be an object"))?;
    let (before, mut table) = read(&root)?;
    let old = colors_of(&table)?;
    let mut colors = entries(old);
    for (name, value) in values {
        let i = NAMES.iter().position(|n| *n == name).ok_or_else(|| {
            err(
                "debug_colors_unknown_color",
                format!("unknown debug color `{name}`"),
            )
        })?;
        let [r, g, b] = validate(value)?;
        colors[i] = Color::rgb(r, g, b);
    }
    let mut new = palette(colors);
    if !values.contains_key("nav") {
        new.nav_links = old.nav_links;
    }
    let already_authored = values.keys().all(|name| {
        table
            .get("editor")
            .and_then(|t| t.get("debug_colors"))
            .and_then(|t| t.get(name))
            .is_some()
    });
    if new == old && already_authored {
        return Ok(false);
    }
    let root_table = table
        .as_table_mut()
        .ok_or_else(|| err("debug_colors_read", "manifest root must be a table"))?;
    let editor = root_table
        .entry("editor")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    let editor = editor
        .as_table_mut()
        .ok_or_else(|| err("debug_colors_read", "editor must be a table"))?;
    let target = editor
        .entry("debug_colors")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    let target = target
        .as_table_mut()
        .ok_or_else(|| err("debug_colors_read", "editor.debug_colors must be a table"))?;
    for (name, c) in NAMES
        .iter()
        .zip(colors)
        .filter(|(name, _)| values.contains_key(**name))
    {
        target.insert(
            (*name).to_owned(),
            toml::Value::Array(
                [c.r, c.g, c.b]
                    .into_iter()
                    .map(|v| toml::Value::Float(f64::from(v)))
                    .collect(),
            ),
        );
    }
    let after = toml::to_string_pretty(&table).map_err(|e| err("debug_colors_write", e.to_string()))?;
    crate::atomic_write::write_atomic(&root.join("Regia.toml"), after.as_bytes())
        .map_err(|e| err("debug_colors_write", e))?;
    world.insert_resource(new);
    world.remove_resource::<Loaded>();
    let _ = ensure(world);
    world.insert_resource(crate::project_settings_ext::ProjectSettingsSaved);
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .push(Box::new(ColorEdit {
            root,
            before,
            after,
            old,
            new,
        }));
    Ok(true)
}

fn state(colors: DebugColors) -> Value {
    let map: serde_json::Map<String, Value> = NAMES
        .iter()
        .zip(entries(colors))
        .map(|(n, c)| ((*n).to_owned(), json!([c.r, c.g, c.b])))
        .collect();
    json!({"colors": map, "space": "linear_rgb", "path": "Regia.toml#editor.debug_colors"})
}

fn handle(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let op = params.get("op").and_then(Value::as_str).unwrap_or("get");
    let changed = match op {
        "get" => false,
        "set" => set(world, params.get("colors").unwrap_or(&Value::Null)).map_err(|e| e.rpc(HINT))?,
        _ => return Err(err("debug_colors_unknown_op", "op must be get or set").rpc(HINT)),
    };
    let colors = ensure(world).map_err(|e| e.rpc(HINT))?;
    let mut out = state(colors);
    out["changed"] = json!(changed);
    Ok(out)
}

/// Project Settings section: standard keyboard-focusable numeric RGB controls.
pub(crate) fn panel(ui: &mut egui::Ui, world: &mut World) {
    let _span = tracing::debug_span!("editor.debug_colors_panel").entered();
    egui::CollapsingHeader::new("Debug colors")
        .default_open(true)
        .show(ui, |ui| {
            let colors = match ensure(world) {
                Ok(c) => c,
                Err(e) => {
                    e.draw(ui);
                    return;
                }
            };
            ui.weak("Linear RGB · saved per project · Ctrl+Z undoes");
            let mut edits = serde_json::Map::new();
            for ((name, label), color) in NAMES.iter().zip(LABELS).zip(entries(colors)) {
                ui.push_id(name, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(label);
                        let mut rgb = [color.r, color.g, color.b];
                        let mut changed = false;
                        for (channel, value) in ["R", "G", "B"].into_iter().zip(&mut rgb) {
                            let r = ui.add(
                                egui::DragValue::new(value)
                                    .speed(0.01)
                                    .range(0.0..=1.0)
                                    .prefix(format!("{channel} ")),
                            );
                            let r = crate::a11y_audit::spoken_only(
                                r,
                                egui::WidgetType::DragValue,
                                &format!("{label} {channel}"),
                            );
                            changed |= r.changed();
                        }
                        if changed {
                            edits.insert((*name).to_owned(), json!(rgb));
                        }
                    })
                });
            }
            let frame = ui.ctx().cumulative_frame_nr();
            if !edits.is_empty()
                && world
                    .get_resource::<Loaded>()
                    .is_none_or(|l| l.frame != Some(frame))
            {
                if let Err(e) = set(world, &Value::Object(edits)) {
                    e.draw(ui);
                }
                if let Some(mut l) = world.get_resource_mut::<Loaded>() {
                    l.frame = Some(frame);
                }
            }
        });
}

/// Mirrors project colors into the play world; no IO after the initial load.
pub(crate) fn sync_play(edit: &mut World, mut play: Option<&mut World>) {
    crate::viewport_agent::sync_debug_flags(edit, play.as_deref_mut());
    // The Profiler's Lockstep tab reads play's tick profile and det debug switch (ADR-8562).
    crate::profiler_source::copy_lockstep_from_play(edit, play.as_deref_mut());
    if let Ok(colors) = ensure(edit)
        && let Some(play) = play
    {
        play.insert_resource(colors);
        play.insert_resource(edit.get_resource::<Overrides>().cloned().unwrap_or_default());
    }
}

/// Agent method for the shared project debug palette.
pub(crate) fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.debug_colors",
            tool: "debug_colors",
            description: "Get or set the project's linear RGB debug colors (collider, trigger, nav, path, contact, sleeping_body). Writes Regia.toml atomically with undo; used by edit and play views.",
            params: &[
                ParamSchema {
                    name: "op",
                    kind: "string",
                    required: false,
                    description: "get (default) or set.",
                },
                ParamSchema {
                    name: "colors",
                    kind: "object",
                    required: false,
                    description: "set: color names mapped to three finite linear RGB values in 0..=1.",
                },
            ],
            result: "{ colors: { collider, trigger, nav, path, contact, sleeping_body }, space, path, changed }",
            mutating: true,
            permission: Permission::EditScene,
        },
        handle as Handler,
    )]
}

/// An explicitly authored project token; absent keys keep the existing editor defaults.
pub(crate) fn override_color(world: &World, name: &str) -> Option<Color> {
    if !world.get_resource::<Overrides>()?.0.contains(name) {
        return None;
    }
    let i = NAMES.iter().position(|n| *n == name)?;
    Some(entries(*world.get_resource::<DebugColors>()?)[i])
}

/// Keeps independently saved debug preferences when a dirty general-settings draft is saved.
pub(crate) fn preserve_manifest_colors(
    table: &mut toml::Value,
    root: &std::path::Path,
) -> Result<(), String> {
    let (_, current) = read(root).map_err(|e| e.message)?;
    let colors = current.get("editor").and_then(|v| v.get("debug_colors")).cloned();
    if let Some(editor) = table.get_mut("editor").and_then(toml::Value::as_table_mut) {
        if let Some(colors) = colors {
            editor.insert("debug_colors".to_owned(), colors);
        } else {
            editor.remove("debug_colors");
        }
    } else if let Some(colors) = colors {
        let root = table.as_table_mut().ok_or("manifest must be a table")?;
        let editor = root
            .entry("editor")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        editor
            .as_table_mut()
            .ok_or("editor must be a table")?
            .insert("debug_colors".to_owned(), colors);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> (World, PathBuf) {
        let root = regia_test_utils::unique_temp_dir("w12-debug-colors");
        std::fs::write(
            root.join("Regia.toml"),
            "[project]\nname='test'\nid='org.regia.test'\n[unknown]\nkeep=7\n",
        )
        .expect("manifest");
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        (world, root)
    }

    #[test]
    fn w12_editor_sleeping_color_is_independent_of_the_collider_override() {
        let (mut edit, root) = project();
        set(&mut edit, &json!({"collider": [0.1, 0.2, 0.3]})).expect("collider color");
        let mut app = regia_app::App::new();
        app.add_plugins(regia_physics::PhysicsPlugin);
        app.world_mut().spawn((
            regia_scene::Transform::default(),
            regia_scene::GlobalTransform::IDENTITY,
            regia_physics::RigidBody::dynamic(),
            regia_physics::Collider::default(),
        ));
        app.run_schedule(regia_app::prelude::FixedUpdate);
        for (_, body) in app
            .world_mut()
            .resource_mut::<regia_physics::PhysicsWorld>()
            .bodies
            .iter_mut()
        {
            body.sleep();
        }
        let view = crate::views::ViewCamera::of(
            crate::views::ViewKind::Perspective,
            &crate::viewport::EditorCamera::default(),
        );
        let mut layout = crate::views::ViewportLayout::default();
        layout.show.colliders = true;
        let colors = |world: &World| {
            crate::overlays::play_debug_shapes(
                world,
                &view,
                regia_core::math::Vec2::new(320.0, 240.0),
                &layout,
            )
            .into_iter()
            .filter_map(|shape| match shape {
                crate::overlays::OverlayShape::Line { color, .. } => Some(color.to_array()),
                _ => None,
            })
            .collect::<Vec<_>>()
        };
        sync_play(&mut edit, Some(app.world_mut()));
        let defaults = colors(app.world());
        assert!(!defaults.is_empty());
        assert!(
            defaults
                .iter()
                .all(|c| *c == regia_agent::seeing_overlays::COLLIDER_COLOR)
        );
        set(&mut edit, &json!({"sleeping_body": [0.25, 0.5, 0.75]})).expect("sleeping color");
        sync_play(&mut edit, Some(app.world_mut()));
        let authored = colors(app.world());
        assert!(!authored.is_empty());
        assert!(
            authored
                .iter()
                .all(|c| *c == Color::rgb(0.25, 0.5, 0.75).to_srgb_u8())
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn w12_malformed_palette_is_reported_without_rewriting_the_manifest() {
        let (mut world, root) = project();
        let malformed = "editor = 7\n[project]\nname='test'\n";
        std::fs::write(root.join("Regia.toml"), malformed).expect("malformed manifest");
        assert!(ensure(&mut world).is_err());
        assert!(set(&mut world, &json!({"path":[0,0,0]})).is_err());
        assert_eq!(
            std::fs::read_to_string(root.join("Regia.toml")).expect("read"),
            malformed
        );
        assert!(!world.contains_resource::<DebugColors>());
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn w12_explicit_navigation_color_applies_to_links_even_at_the_default_value() {
        let (mut world, root) = project();
        let before = ensure(&mut world).expect("defaults");
        assert_ne!(before.nav, before.nav_links);
        let nav = before.nav;
        set(&mut world, &json!({"nav":[nav.r,nav.g,nav.b]})).expect("explicit nav");
        let after = ensure(&mut world).expect("saved palette");
        assert_eq!(after.nav, after.nav_links);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn w12_palette_undo_preserves_an_intervening_manifest_edit() {
        let (mut world, root) = project();
        let mut settings = crate::project_settings::ProjectSettingsState::default();
        settings.load(&root).expect("load");
        set(&mut world, &json!({"path": [0.25, 0.5, 0.75]})).expect("palette");
        settings.set_project_string("name", "kept");
        settings.save(&root).expect("settings");
        let mut undo = world.remove_resource::<UndoStack>().expect("undo stack");
        undo.undo(&mut world);
        let (_, table) = read(&root).expect("read");
        assert_eq!(table["project"]["name"].as_str(), Some("kept"));
        assert_eq!(colors_of(&table).expect("palette"), DebugColors::default());
        undo.redo(&mut world);
        let (_, table) = read(&root).expect("read");
        assert_eq!(table["project"]["name"].as_str(), Some("kept"));
        assert_eq!(
            colors_of(&table).expect("palette").path,
            Color::rgb(0.25, 0.5, 0.75)
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn w12_agent_flags_reach_a_running_play_world_and_preserve_gui_changes() {
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let open = crate::level_io::new_level(app.world_mut());
        app.insert_resource(open);
        app.insert_resource(crate::views::ViewportLayout::default());
        let mut session = crate::pie::start_play(app.world_mut(), None).expect("play session");
        session.app.world_mut().spawn((
            regia_scene::GlobalTransform::IDENTITY,
            regia_nav::NavAgentState {
                waypoints: vec![regia_core::math::Vec3::X],
                ..Default::default()
            },
        ));
        assert!(crate::overlays::play_window_debug_frame(session.app.world()).is_empty());
        crate::viewport_agent::set_viewport(app.world_mut(), &json!({"paths":true,"colliders":true}))
            .expect("agent flags");
        crate::pie_worlds::pump(app.world_mut(), Some(&mut session));
        assert_eq!(
            crate::overlays::play_window_debug_frame(session.app.world()).segment_count(),
            1
        );
        assert!(
            session
                .app
                .world()
                .resource::<crate::views::ViewportLayout>()
                .show
                .colliders
        );
        // A GUI change in the live world is not overwritten on the next shell frame.
        session
            .app
            .world_mut()
            .resource_mut::<crate::views::ViewportLayout>()
            .show
            .paths = false;
        crate::pie_worlds::pump(app.world_mut(), Some(&mut session));
        assert!(crate::overlays::play_window_debug_frame(session.app.world()).is_empty());
        crate::viewport_agent::set_viewport(app.world_mut(), &json!({"paths":true})).expect("paths");
        crate::viewport_agent::set_viewport(app.world_mut(), &json!({"game_view":true})).expect("game view");
        crate::pie_worlds::pump(app.world_mut(), Some(&mut session));
        assert!(
            session
                .app
                .world()
                .resource::<crate::views::ViewportLayout>()
                .show
                .paths
        );
        assert!(crate::overlays::play_window_debug_frame(session.app.world()).is_empty());
        crate::pie::stop_play(session);
    }

    #[test]
    fn w12_explicit_default_color_is_persisted_as_an_override() {
        let (mut world, root) = project();
        let c = DebugColors::default().collider;
        let values = json!({"collider": [c.r, c.g, c.b]});
        assert!(set(&mut world, &values).expect("first explicit value"));
        assert_eq!(override_color(&world, "collider"), Some(c));
        assert!(!set(&mut world, &values).expect("unchanged explicit value"));
        std::fs::remove_dir_all(root).expect("cleanup");
    }
    #[test]
    fn w12_bad_color_edit_is_atomic_and_project_switch_reloads() {
        let (mut world, root) = project();
        let before = std::fs::read(root.join("Regia.toml")).expect("read");
        for values in [
            json!({"path": [0.2, 0.3, 0.4], "unknown": [0, 0, 0]}),
            json!({"path": [1.1, 0, 0]}),
            json!({"path": [0, 0]}),
        ] {
            assert!(set(&mut world, &values).is_err());
            assert_eq!(std::fs::read(root.join("Regia.toml")).expect("read"), before);
            assert!(!world.contains_resource::<UndoStack>());
        }
        set(&mut world, &json!({"path": [0.25, 0.5, 0.75]})).expect("set");
        let (_, table) = read(&root).expect("manifest");
        assert_eq!(table["unknown"]["keep"].as_integer(), Some(7));
        assert_eq!(
            override_color(&world, "collider"),
            None,
            "unset tokens retain editor defaults"
        );
        let mut play = World::new();
        sync_play(&mut world, Some(&mut play));
        assert_eq!(play.resource::<DebugColors>().path, Color::rgb(0.25, 0.5, 0.75));
        let (other, other_root) = project();
        world.insert_resource(other.resource::<crate::agent_ext::EditorProject>().clone());
        assert_eq!(ensure(&mut world).expect("other"), DebugColors::default());
        std::fs::remove_dir_all(root).expect("cleanup");
        std::fs::remove_dir_all(other_root).expect("cleanup");
    }

    #[test]
    fn w12_general_settings_save_keeps_independently_written_colors() {
        let (mut world, root) = project();
        let mut state = crate::project_settings::ProjectSettingsState::default();
        state.load(&root).expect("load");
        state.set_project_string("name", "renamed");
        set(&mut world, &json!({"path": [0.25, 0.5, 0.75]})).expect("color edit");
        state.save(&root).expect("save general settings");
        let (_, table) = read(&root).expect("manifest");
        assert_eq!(
            colors_of(&table).expect("colors").path,
            Color::rgb(0.25, 0.5, 0.75)
        );
        assert_eq!(table["project"]["name"].as_str(), Some("renamed"));
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn w12_debug_color_controls_are_keyboard_operable_at_two_scales() {
        for scale in [1.0, 1.75] {
            let (mut world, root) = project();
            let ctx = egui::Context::default();
            ctx.set_zoom_factor(scale);
            let frame = |events, world: &mut World| {
                let mut output = ctx.run_ui(
                    egui::RawInput {
                        events,
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(1000.0, 900.0),
                        )),
                        ..Default::default()
                    },
                    |ui| {
                        egui::CentralPanel::default().show(ui, |ui| panel(ui, world));
                    },
                );
                output.textures_delta.clear();
            };
            let key = |key| egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            };
            frame(vec![], &mut world);
            let before = *world.resource::<DebugColors>();
            for _ in 0..24 {
                frame(vec![key(egui::Key::Tab)], &mut world);
                frame(vec![key(egui::Key::ArrowUp)], &mut world);
                if *world.resource::<DebugColors>() != before {
                    break;
                }
            }
            assert_ne!(
                *world.resource::<DebugColors>(),
                before,
                "Tab and Up edit a channel at {scale}"
            );
            assert!(world.resource::<UndoStack>().undo_depth() > 0);
            std::fs::remove_dir_all(root).expect("cleanup");
        }
    }
}
