// SPDX-License-Identifier: MPL-2.0
//! `asset.export_glb` through the registered `AgentExtension` handler, as dispatch calls it
//! (book/agents/export-glb.md, ADR-2554).

use regia_agent::extension::AgentExtension;
use regia_app::prelude::*;
use serde_json::json;

#[test]
fn export_glb_is_registered_and_writes_a_file() {
    let root = regia_test_utils::unique_temp_dir("editor_export_glb_call");
    std::fs::create_dir_all(root.join("assets/maps")).expect("dir");
    std::fs::write(
        root.join("assets/maps/a.level.ron"),
        r#"SceneFile(format_version: 1, entities: [
            (guid: "00000000-0000-0000-0000-000000000001", name: "Cube", components: {
                "regia_render::runtime::MeshSource": (mesh: "builtin:cube", material: ""),
            }),
        ])"#,
    )
    .expect("level");
    let mut world = World::new();
    world.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(root.clone()),
    });
    regia_editor::agent_ext::install(&mut world);
    let extension = world.resource::<AgentExtension>().clone();
    let (_, handler) = extension.method("asset.export_glb").expect("registered");
    let answer = handler(
        &mut world,
        &json!({ "level": "assets/maps/a.level.ron", "out": "export/a.glb" }),
    )
    .expect("export");
    assert_eq!(answer["ok"], true);
    assert_eq!(answer["meshes"], 1);
    let bytes = std::fs::read(root.join("export/a.glb")).expect("file");
    assert_eq!(&bytes[0..4], b"glTF");
}
