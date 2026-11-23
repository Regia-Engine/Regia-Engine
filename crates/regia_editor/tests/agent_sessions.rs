// SPDX-License-Identifier: MPL-2.0
//! Test-first coverage of the hosted-agent surface (P-368–370).
use regia_app::prelude::*;
use regia_editor::editor_prefs::{EditorPreferences, PrefValue};
use serde_json::json;

#[test]
fn agent_sessions_surface_is_registered_and_disabled_by_default() {
    let mut world = World::new();
    regia_editor::agent_ext::install(&mut world);
    let ext = world.resource::<regia_agent::extension::AgentExtension>();
    let (_, handler) = ext.method("editor.agent_sessions").expect("hosted-agent method");
    let reply = handler(&mut world, &json!({"action":"list"})).expect("list");
    assert_eq!(reply["sessions"], json!([]));
    assert_eq!(reply["commands"], json!([]));
}

#[test]
fn agent_sessions_commands_are_user_preferences() {
    let mut prefs = EditorPreferences::default();
    assert_eq!(prefs.text("agents.commands"), "");
    prefs
        .set(
            "agents.commands",
            &PrefValue::Text(r#"[{"name":"Local","command":["agent","--stdio"]}]"#.into()),
        )
        .expect("agent command preference");
}

#[test]
fn agent_panel_is_keyboard_discoverable() {
    assert!(
        regia_editor::editor_layout::BUILT_IN_PANELS
            .iter()
            .any(|p| p.title() == "Agent"),
        "Agent panel in panel registry"
    );
}

#[test]
fn agent_sessions_executable_configuration_is_owner_only() {
    let mut world = World::new();
    world.insert_resource(EditorPreferences::default());
    regia_editor::agent_ext::install(&mut world);
    let (_, set) = world
        .resource::<regia_agent::extension::AgentExtension>()
        .method("editor.prefs_set")
        .expect("preference surface");
    let error = set(
        &mut world,
        &json!({"values":{"agents.commands":r#"[{"name":"Injected","command":["program"]}]"#}}),
    )
    .expect_err("the protocol must not choose a program for the editor to execute");
    assert_eq!(error.data.expect("error detail")["code"], "pref_owner_only");
    assert_eq!(world.resource::<EditorPreferences>().text("agents.commands"), "");
}
