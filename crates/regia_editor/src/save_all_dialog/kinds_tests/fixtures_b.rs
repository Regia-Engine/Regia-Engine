// SPDX-License-Identifier: MPL-2.0
//! Save all fixtures (see `super::Fixture`).

use std::path::{Path, PathBuf};

use regia_ecs::prelude::*;
use serde_json::{Value, json};

use super::Fixture;

/// Writes `text` to `root/rel` (creating its directory) and returns the absolute path.
fn write(root: &Path, rel: &str, text: &str) -> PathBuf {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    std::fs::write(&path, text).expect("write");
    path
}

/// Calls the editor agent method `method`.
fn call(world: &mut World, method: &str, params: Value) -> Value {
    let (_, handler) = crate::agent_ext::all_methods()
        .into_iter()
        .find(|(schema, _)| schema.name == method)
        .unwrap_or_else(|| panic!("{method} is installed"));
    handler(world, &params).unwrap_or_else(|e| panic!("{method}: {}", e.message))
}

/// An effect opened in the VFX editor with its particle budget changed.
fn vfx(app: &mut regia_app::App, root: &Path) -> PathBuf {
    use crate::vfx_editor::{VfxChange, edit_effect, open_effect, template};

    let rel = "assets/fx/save_all.vfx.ron";
    let text = template("save_all").to_ron().expect("effect text");
    let path = write(root, rel, &text);
    let world = app.world_mut();
    open_effect(world, rel).expect("open the effect");
    assert!(
        edit_effect(world, &VfxChange::Budget(2000)).expect("edit the budget"),
        "the budget edit changes the effect"
    );
    path
}

/// A conversation opened in the conversation editor with one line's text key changed.
fn conversation(app: &mut regia_app::App, root: &Path) -> PathBuf {
    use crate::conversation_panel::ConvEditor;
    use crate::conversation_panel::edit::ConvOp;

    const CONV: &str = r#"(
    format_version: 1,
    id: "save_all",
    lines: [
        (id: "greet", speaker: "guard", text: "save-all-greet"),
    ],
    steps: [
        Line("greet"),
        End,
    ],
)
"#;
    let path = write(root, "assets/conversations/save_all.conv.ron", CONV);
    let world = app.world_mut();
    world
        .get_resource_or_insert_with(ConvEditor::default)
        .open(&path, Some(root))
        .expect("open the conversation");
    crate::conversation_panel::edit(
        world,
        "set line",
        None,
        &[ConvOp::SetLine {
            line: "greet".into(),
            text: Some("save-all-hello".into()),
            speaker: None,
            note: None,
            emotion: None,
        }],
    )
    .expect("edit the line");
    path
}

/// An animation graph (written by the editor's own save) opened with its clip's speed changed.
fn anim_graph(app: &mut regia_app::App, root: &Path) -> PathBuf {
    use crate::anim::{AnimEditor, NodeKind, Scope};
    use regia_animation::graph::ClipNode;

    let clip = |speed: f32| {
        NodeKind::Clip(ClipNode {
            clip: "Walk".into(),
            looping: true,
            speed,
            sync_group: None,
        })
    };
    let path = root.join("assets/anim/save_all.animgraph.ron");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    let mut seed = AnimEditor::default();
    seed.new_graph(Some(path.clone()));
    let id = seed
        .docs
        .graph
        .add_node(clip(1.0), Scope::Root, [0.0, 0.0])
        .expect("add a clip");
    seed.docs
        .graph
        .set_output(&Scope::Root, Some(id))
        .expect("output");
    seed.save_graph(None).expect("write the graph");
    let world = app.world_mut();
    world
        .get_resource_or_insert_with(AnimEditor::default)
        .open_graph(&path)
        .expect("open the graph");
    crate::anim::edit(world, "speed", None, |docs| {
        let id = docs
            .graph
            .output
            .ok_or_else(|| "the graph has no output".to_string())?;
        docs.graph.set_kind(id, clip(1.5))
    })
    .expect("edit the clip speed");
    path
}

/// A UI animation library opened in the UI Animation editor with an animation added.
fn ui_anim(app: &mut regia_app::App, root: &Path) -> PathBuf {
    use crate::ui_anim_doc::{AnimOp, UiAnimDoc, UiAnimEditor};

    let path = root.join("assets/ui/panel.uianim.ron");
    let mut doc = UiAnimDoc::new_document(Some(path.clone()));
    doc.apply(AnimOp::AddAnimation {
        name: "panel::fade".into(),
        property: Some("opacity".into()),
    })
    .expect("add");
    doc.save(None).expect("write the library");
    let mut doc = UiAnimDoc::open(&path).expect("open the library");
    doc.apply(AnimOp::AddAnimation {
        name: "panel::pulse".into(),
        property: None,
    })
    .expect("edit");
    app.world_mut()
        .get_resource_or_insert_with(UiAnimEditor::default)
        .set_doc(doc);
    path
}

/// A `.seq.ron` opened by the Sequence Curves panel with a key moved.
fn sequence(app: &mut regia_app::App, root: &Path) -> PathBuf {
    use crate::seq_curve_panel::{SeqCurvePanel, edit_selected, open_sequence};
    use regia_framework::sequencer::edit::KeyEdit;

    const SEQ: &str = r#"(version: 1, name: "intro", tracks: [
        (name: "fade", target: "ui", property: "opacity",
            keys: Float([(time: 0.0, value: 0.0, ease: Bezier), (time: 2.0, value: 1.0)])),
    ])"#;
    let rel = "assets/cine/intro.seq.ron";
    let path = write(root, rel, SEQ);
    let world = app.world_mut();
    let mut panel = SeqCurvePanel::default();
    open_sequence(world, &mut panel, rel).expect("open the sequence");
    edit_selected(
        world,
        &mut panel,
        KeyEdit::Move {
            index: 1,
            time: 1.5,
            value: 0.75,
        },
    )
    .expect("move a key");
    world.insert_resource(panel);
    path
}

/// The entity named `name` (scene `Name`).
fn named(world: &mut World, name: &str) -> Entity {
    let mut query = world.query::<(Entity, &regia_scene::prelude::Name)>();
    query
        .iter(world)
        .find(|(_, n)| n.as_str() == name)
        .map(|(e, _)| e)
        .unwrap_or_else(|| panic!("no entity named {name}"))
}

/// A prefab in prefab edit mode with a child added under its root.
fn prefab(app: &mut regia_app::App, root: &Path) -> PathBuf {
    use crate::command::{EditorCommand as _, SpawnEntities, UndoStack};
    use crate::prefab_edit::{PrefabEditMode, enter_prefab_edit};

    const PREFAB: &str = r#"( format_version: 1, entities: [
        ( guid: "8b6f2a11-0000-4000-8000-000000000001", name: "Root",
          components: { "regia_scene::transform::Transform": () } ),
        ( guid: "8b6f2a11-0000-4000-8000-000000000002", name: "Child", parent: Some("8b6f2a11-0000-4000-8000-000000000001"),
          components: { "regia_scene::transform::Transform": ( translation: (0.0, 1.0, 0.0) ) } ),
    ] )"#;
    let path = write(root, "assets/prefabs/crate.prefab.ron", PREFAB);
    let world = app.world_mut();
    let open = crate::level_io::new_level(world);
    world.insert_resource(open);
    world.insert_resource(UndoStack::new());
    world.insert_resource(PrefabEditMode::default());
    enter_prefab_edit(world, &path).expect("enter prefab edit");
    // Add a child under the root (the outliner's path: a command on the undo stack).
    let parent = named(world, "Root");
    let mut spawn = SpawnEntities::new_entity(
        world,
        regia_core::id::Guid::new_random(),
        "Added",
        regia_scene::prelude::Transform::default(),
    )
    .expect("spawn");
    spawn.apply(world);
    let added = named(world, "Added");
    world.entity_mut(added).insert(ChildOf(parent));
    world.resource_mut::<UndoStack>().push(Box::new(spawn));
    path
}

/// A shot list opened in the Shots panel with a fade added from the timeline.
fn shots(app: &mut regia_app::App, root: &Path) -> PathBuf {
    let rel = "assets/cine/intro.shots.ron";
    crate::content::create_asset(root, crate::content::NewAssetKind::ShotList, rel).expect("shot list");
    let world = app.world_mut();
    world.insert_resource(regia_framework::shots::ShotPlayer::default());
    call(world, "editor.shot_panel_open", json!({ "path": rel }));
    call(
        world,
        "editor.seq_cut_edit",
        json!({ "op": "fade", "shot": 0, "fade_in": 0.5 }),
    );
    root.join(rel)
}

/// The fixtures of this file.
pub(super) fn fixtures() -> Vec<(&'static str, Fixture)> {
    vec![
        ("vfx", vfx as Fixture),
        ("conversation", conversation as Fixture),
        ("anim_graph", anim_graph as Fixture),
        ("ui_anim", ui_anim as Fixture),
        ("sequence", sequence as Fixture),
        ("prefab", prefab as Fixture),
        ("shots", shots as Fixture),
    ]
}
