// SPDX-License-Identifier: MPL-2.0
//! Abilities inspector tests: a fire-bolt fixture world (the plugin, a cost, a cooldown and a burn), the
//! play-world pump, the edit-world agent calls, the overlay group, and headless drawing.

use regia_app::prelude::*;
use regia_framework::abilities::{
    AbilityAsset, AbilityCounters, AbilityPlugin, ApplyEffect, Attribute, EffectAsset, Lifetime, Magnitude,
    ModOp, Modifier,
};
use regia_framework::tags::Tag;
use serde_json::{Value, json};

use super::*;
use crate::data_panels::test_ui::draw_frames;

fn step(app: &mut App) {
    let before = app.world().resource::<AbilityCounters>().tick;
    for _ in 0..4 {
        app.update();
        if app.world().resource::<AbilityCounters>().tick > before {
            return;
        }
    }
    panic!("no fixed step");
}

fn modifier(attr: &str, op: ModOp, amount: f32) -> Modifier {
    Modifier {
        attribute: attr.to_owned(),
        op,
        amount: Magnitude::Const(amount),
        per_stack: false,
        when: None,
    }
}

fn fire_bolt_app() -> (App, Entity) {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    enable_fixed_step(&mut app, 60.0);
    app.add_plugins(AbilityPlugin::default());
    let mut cost = EffectAsset::new(Lifetime::Instant);
    cost.base_edits = vec![modifier("mana", ModOp::AddPre, -10.0)];
    let mut cd = EffectAsset::new(Lifetime::Timed(Magnitude::Const(0.5)));
    cd.status_tags = vec![Tag::new("Cooldown.FireBolt").expect("tag")];
    let mut burn = EffectAsset::new(Lifetime::Timed(Magnitude::Const(1.0)));
    burn.live_mods = vec![modifier("armor", ModOp::AddPre, -2.0)];
    let mut bolt = AbilityAsset::new("fire_bolt");
    bolt.cost = Some("effects/mana_10.effect.ron".into());
    bolt.cooldown = Some("effects/cd_fire_bolt.effect.ron".into());
    bolt.max_run_ticks = Some(2);
    {
        let mut assets = app.world_mut().resource_mut::<AbilityAssets>();
        assets.insert_effect("effects/mana_10.effect.ron", cost);
        assets.insert_effect("effects/cd_fire_bolt.effect.ron", cd);
        assets.insert_effect("effects/burn.effect.ron", burn);
        assets.insert_ability("abilities/fire_bolt.ability.ron", bolt);
    }
    let hero = app
        .world_mut()
        .spawn((
            Attributes::from_list([
                Attribute::new("mana", 50.0, 0.0, 100.0),
                Attribute::new("armor", 10.0, 0.0, 100.0),
            ])
            .expect("attrs"),
            Transform::from_translation(Vec3::new(1.0, 0.0, 2.0)),
        ))
        .id();
    step(&mut app);
    (app, hero)
}

fn method(name: &str) -> Handler {
    methods()
        .into_iter()
        .find(|(m, _)| m.name == name)
        .map(|(_, h)| h)
        .expect("registered")
}

fn reason(e: &RpcError) -> String {
    e.data
        .as_ref()
        .and_then(|d| d["reason"].as_str())
        .unwrap_or_default()
        .to_owned()
}

fn attribute<'a>(state: &'a Value, name: &str) -> &'a Value {
    state["attributes"]
        .as_array()
        .and_then(|a| a.iter().find(|x| x["name"] == name))
        .expect("attribute")
}

#[test]
fn the_panel_draws_headless_with_and_without_a_subject() {
    let mut world = World::new();
    assert!(!draw_frames(&mut world, 2, abilities_panel).is_empty());
    let (mut app, _hero) = fire_bolt_app();
    let world = app.world_mut();
    let mut panel = AbilitiesPanel::default();
    pump_world(&mut panel, world, Source::Play, None);
    world.insert_resource(panel);
    assert!(!draw_frames(world, 2, abilities_panel).is_empty());
}

/// The play-world flow: queued buttons run at the pump; grant, start, cooldown, burn, remove.
#[test]
fn play_world_pump_runs_queued_operations_and_reads_live_state() {
    let (mut app, hero) = fire_bolt_app();
    let mut panel = AbilitiesPanel::default();
    pump_world(&mut panel, app.world_mut(), Source::Play, None);
    assert_eq!(
        panel.snapshot.entity,
        Some(hero),
        "the first listed entity is inspected"
    );
    assert!(panel.snapshot.available);
    assert_eq!(panel.snapshot.abilities, ["abilities/fire_bolt.ability.ron"]);

    panel
        .pending
        .push(json!({ "op": "grant", "ability": "abilities/fire_bolt.ability.ron" }));
    panel
        .pending
        .push(json!({ "op": "set_base", "attribute": "armor", "base": 20.0 }));
    pump_world(&mut panel, app.world_mut(), Source::Play, None);
    assert!(panel.pending.is_empty());
    let v = panel.json();
    assert_eq!(v["grants"][0]["state"], "ready");
    assert_eq!(attribute(&v, "armor")["base"], 20.0);
    assert_eq!(v["last"]["op"], "set_base");
    let grant = v["grants"][0]["id"].as_u64().expect("grant id");

    panel.pending.push(json!({ "op": "activate", "grant": grant }));
    pump_world(&mut panel, app.world_mut(), Source::Play, None);
    assert_eq!(panel.json()["last"]["answer"]["started"], true);
    for _ in 0..4 {
        step(&mut app);
    }
    pump_world(&mut panel, app.world_mut(), Source::Play, None);
    let v = panel.json();
    assert_eq!(attribute(&v, "mana")["base"], 40.0, "the cost was paid");
    assert_eq!(v["grants"][0]["state"], "cooling");
    assert_eq!(v["entities"][0]["state"], "cooling");

    // A burn lowers armor; the breakdown names it; Remove takes it off.
    ab::apply_effect(app.world_mut(), ApplyEffect::new("effects/burn.effect.ron", hero)).expect("burn");
    step(&mut app);
    pump_world(&mut panel, app.world_mut(), Source::Play, None);
    let v = panel.json();
    assert_eq!(attribute(&v, "armor")["current"], 18.0);
    let lines = attribute(&v, "armor")["breakdown"]
        .as_array()
        .expect("lines")
        .len();
    assert!(lines >= 3, "base, the modifier and current: {v}");
    let burn = v["effects"]
        .as_array()
        .and_then(|e| e.iter().find(|x| x["asset"] == "effects/burn.effect.ron"))
        .and_then(|x| x["id"].as_u64())
        .expect("burn is live");
    panel.pending.push(json!({ "op": "remove_effect", "id": burn }));
    pump_world(&mut panel, app.world_mut(), Source::Play, None);
    step(&mut app);
    pump_world(&mut panel, app.world_mut(), Source::Play, None);
    let v = panel.json();
    assert!(
        v["effects"]
            .as_array()
            .is_some_and(|e| e.iter().all(|x| x["asset"] != "effects/burn.effect.ron")),
        "{v}"
    );
    assert_eq!(attribute(&v, "armor")["current"], 20.0);

    // Revoke empties the book.
    panel.pending.push(json!({ "op": "revoke", "grant": grant }));
    pump_world(&mut panel, app.world_mut(), Source::Play, None);
    assert_eq!(panel.json()["last"]["ok"], true);
}

#[test]
fn edit_world_calls_run_at_once_and_errors_are_coded() {
    let (mut app, hero) = fire_bolt_app();
    let world = app.world_mut();
    let call = |world: &mut World, params: Value| method("editor.abilities_panel_call")(world, &params);
    let gone = world.spawn_empty().id();
    world.despawn(gone);
    let v = call(
        world,
        json!({ "op": "set_base", "attribute": "mana", "base": 75.0 }),
    )
    .expect("set");
    assert_eq!(v["queued"], false);
    assert_eq!(v["answer"]["new"], 75.0);
    assert_eq!(
        world.get::<Attributes>(hero).map(|a| a.list()[0].base),
        Some(75.0)
    );
    for (params, expected) in [
        (
            json!({ "op": "activate", "grant": 1 }),
            "abilities_panel_not_playing",
        ),
        (json!({ "op": "fly" }), "abilities_panel_bad_op"),
        (
            json!({ "op": "set_base", "attribute": "luck", "base": 1.0 }),
            "unknown_attribute",
        ),
        (
            json!({ "op": "set_base", "attribute": "mana", "base": "x" }),
            "attribute_not_finite",
        ),
        (json!({ "op": "remove_effect", "id": 999 }), "effect_id_not_found"),
        (
            json!({ "op": "grant", "ability": "abilities/none.ability.ron" }),
            "asset_not_found",
        ),
        (
            json!({ "op": "revoke", "grant": "x" }),
            "abilities_panel_bad_value",
        ),
        (
            json!({ "op": "set_base", "entity": gone.to_bits(), "attribute": "mana", "base": 1.0 }),
            "entity_not_found",
        ),
    ] {
        let e = call(world, params.clone()).expect_err("refused");
        let r = reason(&e);
        // `remove_effect` of an unknown id answers the framework's own reason.
        assert!(
            r == expected || (expected == "effect_id_not_found" && r.contains("not_found")),
            "{params}: {r}"
        );
    }
    let v = method("editor.abilities_panel_get")(world, &json!({})).expect("get");
    assert_eq!(v["last"]["ok"], false);
    assert_eq!(
        attribute(&v, "mana")["base"],
        75.0,
        "refused calls changed nothing"
    );
    // Without the plugin, grants are refused as unavailable.
    let mut bare = World::new();
    let e = bare
        .spawn(Attributes::from_list([Attribute::new("hp", 1.0, 0.0, 1.0)]).expect("attrs"))
        .id();
    let err = call(
        &mut bare,
        json!({ "op": "grant", "entity": e.to_bits(), "ability": "a" }),
    )
    .expect_err("no plugin");
    assert_eq!(reason(&err), "abilities_panel_unavailable");
    let v = method("editor.abilities_panel_get")(&mut bare, &json!({})).expect("get");
    assert_eq!(v["available"], false);
    assert_eq!(v["entity"], e.to_bits());
}

#[test]
fn a_play_session_queues_calls_for_the_pump() {
    let (mut app, _hero) = fire_bolt_app();
    let world = app.world_mut();
    world.insert_resource(crate::pie::PieStatus {
        mode: Some(crate::pie::PlayMode::Playing),
        ..Default::default()
    });
    let v = method("editor.abilities_panel_call")(
        world,
        &json!({ "op": "set_base", "attribute": "mana", "base": 1.0 }),
    )
    .expect("queued");
    assert_eq!(v, json!({ "queued": true, "pending": 1 }));
    assert_eq!(world.resource::<AbilitiesPanel>().pending.len(), 1);
}

#[test]
fn the_overlay_group_is_off_by_default_and_draws_rings_and_effect_ticks() {
    let (mut app, hero) = fire_bolt_app();
    ab::apply_effect(app.world_mut(), ApplyEffect::new("effects/burn.effect.ron", hero)).expect("burn");
    step(&mut app);
    let world = app.world_mut();
    method("editor.abilities_panel_get")(world, &json!({})).expect("get");
    assert!(abilities_overlay_segments(world).is_empty(), "off by default");
    let v =
        method("editor.abilities_panel_select")(world, &json!({ "overlay": true, "entity": hero.to_bits() }))
            .expect("select");
    assert_eq!(v["overlay"], true);
    assert_eq!(v["pinned"], hero.to_bits());
    let segments = abilities_overlay_segments(world);
    assert_eq!(segments.len(), 16 + 1, "a 16-sided ring and one effect tick");
    let ring = segments[0];
    assert_eq!(ring.2, ring_color("ready"));
    assert!(
        (ring.0 - Vec3::new(1.0, 0.0, 2.0)).length() < 0.7,
        "the ring sits at the entity"
    );
    let e = method("editor.abilities_panel_select")(world, &json!({ "overlay": 1 })).expect_err("bad");
    assert_eq!(reason(&e), "abilities_panel_bad_value");
}

#[test]
fn every_method_is_registered_with_its_class() {
    let list = methods();
    assert_eq!(list.len(), 4);
    for (schema, _) in &list {
        assert!(schema.name.starts_with("editor.abilities_panel_"));
        assert_eq!(schema.mutating, schema.name != "editor.abilities_panel_get");
    }
}

/// Spec 53 § 6: the assets open with live validation (`abilities.validate`'s checks) and save only when valid.
#[test]
fn the_asset_editor_validates_live_and_saves_only_valid_text() {
    let dir = regia_test_utils::unique_temp_dir("abilities-panel-asset");
    std::fs::create_dir_all(dir.join("assets/effects")).expect("dir");
    std::fs::create_dir_all(dir.join("assets/abilities")).expect("dir");
    let mut cd = EffectAsset::new(Lifetime::Timed(Magnitude::Const(0.5)));
    cd.status_tags = vec![Tag::new("Cooldown.Bolt").expect("tag")];
    let cd = ab::to_ron(&cd).expect("effect text");
    std::fs::write(dir.join("assets/effects/cd.effect.ron"), cd).expect("write");
    let mut bolt = AbilityAsset::new("bolt");
    bolt.cooldown = Some("effects/cd.effect.ron".into());
    let valid = ab::to_ron(&bolt).expect("ability text");
    std::fs::write(dir.join("assets/abilities/bolt.ability.ron"), &valid).expect("write");
    let mut world = World::new();
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(dir.clone()),
    });
    let asset = |world: &mut World, params: Value| method("editor.abilities_panel_asset")(world, &params);
    // The edit world has no ability plugin: the cooldown effect is read from the project.
    let v = asset(&mut world, json!({ "path": "abilities/bolt.ability.ron" })).expect("open");
    assert_eq!(v["asset"]["ok"], true, "{v}");
    assert_eq!(v["asset"]["dirty"], false);
    // A typo is caught as it is typed, with its place.
    let typo = valid.replacen("cooldown", "cooldwn", 1);
    let v = asset(&mut world, json!({ "text": typo })).expect("text");
    assert_eq!(v["asset"]["ok"], false);
    let first = &v["asset"]["diagnostics"][0];
    assert!(
        first["code"] == "unknown_field" || first["code"] == "parse_error",
        "{first}"
    );
    assert!(first["span"]["line"].as_u64().is_some());
    let e = asset(&mut world, json!({ "save": true })).expect_err("invalid");
    assert_eq!(reason(&e), "abilities_panel_invalid_asset");
    // A reference to an effect that exists nowhere is an error too.
    let dangling = valid.replace("effects/cd.effect.ron", "effects/none.effect.ron");
    let v = asset(&mut world, json!({ "text": dangling })).expect("text");
    assert_eq!(v["asset"]["ok"], false, "{v}");
    // Fixed text saves, atomically.
    let renamed = valid.replace("\"bolt\"", "\"bolt_two\"");
    let v = asset(&mut world, json!({ "text": renamed, "save": true })).expect("save");
    assert_eq!(v["written"], "abilities/bolt.ability.ron");
    assert_eq!(v["asset"]["dirty"], false);
    assert_eq!(
        std::fs::read_to_string(dir.join("assets/abilities/bolt.ability.ron")).expect("read"),
        renamed
    );
    assert!(!draw_frames(&mut world, 2, abilities_panel).is_empty());
    for (params, expected) in [
        (json!({ "path": "../x.effect.ron" }), "abilities_panel_bad_path"),
        (
            json!({ "path": "effects/none.effect.ron" }),
            "abilities_panel_file_not_found",
        ),
        (json!({ "path": "effects/cd.txt" }), "abilities_panel_bad_path"),
        (json!({ "text": 3 }), "abilities_panel_bad_value"),
    ] {
        let e = asset(&mut world, params.clone()).expect_err("refused");
        assert_eq!(reason(&e), expected, "{params}");
    }
    let mut bare = World::new();
    let e = asset(&mut bare, json!({})).expect_err("nothing open");
    assert_eq!(reason(&e), "abilities_panel_no_asset");
    let e = asset(&mut bare, json!({ "path": "effects/cd.effect.ron" })).expect_err("no project");
    assert_eq!(reason(&e), "abilities_panel_no_project");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The `abilities.*` agent method `name` of `world` (installed on first use).
fn agent_op(world: &mut World, name: &str, params: Value) -> Result<Value, RpcError> {
    if world
        .get_resource::<regia_agent::extension::AgentExtension>()
        .and_then(|x| x.method(name))
        .is_none()
    {
        regia_agent::abilities::install_ability_methods(world);
    }
    let handler = world
        .resource::<regia_agent::extension::AgentExtension>()
        .method(name)
        .map(|(_, handler)| handler)
        .expect("installed");
    handler(world, &params)
}

/// The guard attribute set (health and armor).
fn guard_set() -> ab::AttributesAsset {
    ab::AttributesAsset {
        format_version: ab::ATTRIBUTES_FORMAT_VERSION,
        description: "guard".into(),
        attributes: vec![
            ab::AttributeDecl {
                name: "health".into(),
                base: 80.0,
                min: 0.0,
                max: 100.0,
                max_from: None,
                derived: None,
            },
            ab::AttributeDecl {
                name: "armor".into(),
                base: 5.0,
                min: 0.0,
                max: 50.0,
                max_from: None,
                derived: None,
            },
        ],
    }
}

/// An attribute set the fixture loads (`attrs/guard.attrs.ron`).
fn with_guard_set(world: &mut World) {
    world
        .resource_mut::<AbilityAssets>()
        .insert_attributes("attrs/guard.attrs.ron", guard_set());
}

/// The ability state of `e` a parity check compares: attributes (name, base, current, min, max)
/// and live effects (asset, level, stacks, remaining ticks).
fn ability_state(world: &World, e: Entity) -> Value {
    json!({
        "attributes": world.get::<Attributes>(e).map(|a| a.list().iter().map(|x| json!([x.name, x.base, x.current, x.min, x.max])).collect::<Vec<_>>()),
        "effects": world.get::<AppliedEffects>(e).map(|l| l.list.iter().map(|x| json!([x.asset, x.level, x.stacks, x.remaining_ticks])).collect::<Vec<_>>()),
    })
}

/// rev-claude-64 (5) finding 1: a set the world has not loaded is read from the project. The panel takes
/// the asset key under `assets/` (`attrs/guard.attrs.ron`) and the same path with its `assets/` prefix
/// (`assets/attrs/guard.attrs.ron`, the spelling `abilities.add_attributes` reads from the project root);
/// on the base the prefixed spelling looked for `assets/assets/...` and answered `asset_not_found`. With
/// the prefixed spelling the panel and the agent read the same file into the same world state.
#[test]
fn an_unloaded_set_is_read_from_the_project_with_or_without_the_assets_prefix() {
    let root = regia_test_utils::unique_temp_dir("abilities-panel-unloaded-set");
    std::fs::create_dir_all(root.join("assets/attrs")).expect("dir");
    std::fs::write(
        root.join("assets/attrs/guard.attrs.ron"),
        ab::to_ron(&guard_set()).expect("ron"),
    )
    .expect("write");
    let in_project = |world: &mut World| {
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        world.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(root.clone()),
        });
    };

    let (mut agent_app, _) = fire_bolt_app();
    let agent_world = agent_app.world_mut();
    in_project(agent_world);
    let guard = agent_world.spawn(regia_scene::prelude::Name::new("guard")).id();
    agent_op(
        agent_world,
        "abilities.add_attributes",
        json!({ "entity": guard.to_bits(), "asset": "assets/attrs/guard.attrs.ron" }),
    )
    .expect("the agent reads <root>/assets/attrs/guard.attrs.ron");
    let agent_state = ability_state(agent_world, guard);
    assert_eq!(
        agent_state["attributes"].as_array().map(Vec::len),
        Some(2),
        "{agent_state}"
    );

    for asset in ["assets/attrs/guard.attrs.ron", "attrs/guard.attrs.ron"] {
        let (mut app, _) = fire_bolt_app();
        let world = app.world_mut();
        world.init_resource::<crate::command::UndoStack>();
        in_project(world);
        let guard = world.spawn(regia_scene::prelude::Name::new("guard")).id();
        let v = method("editor.abilities_panel_call")(
            world,
            &json!({ "op": "add_attributes", "entity": guard.to_bits(), "asset": asset }),
        )
        .unwrap_or_else(|e| panic!("{asset}: {e:?}"));
        assert_eq!(v["answer"]["attributes"], 2, "{asset}: {v}");
        assert_eq!(
            ability_state(world, guard),
            agent_state,
            "{asset}: the agent's world state"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// Spec 53 § 6 parity: the panel adds an attribute set and applies an effect like the agent's
/// `abilities.add_attributes` and `abilities.apply_effect` (the same world state), each as one undo
/// step in the edit world, and its buttons are drawn.
#[test]
fn the_panel_adds_attribute_sets_and_applies_effects_like_the_agent_as_one_undo_step() {
    // Through the agent ops.
    let (mut agent_app, _) = fire_bolt_app();
    let agent_world = agent_app.world_mut();
    with_guard_set(agent_world);
    let guard = agent_world.spawn(regia_scene::prelude::Name::new("guard")).id();
    agent_op(
        agent_world,
        "abilities.add_attributes",
        json!({ "entity": guard.to_bits(), "asset": "attrs/guard.attrs.ron" }),
    )
    .expect("agent add");
    agent_op(
        agent_world,
        "abilities.apply_effect",
        json!({ "target": guard.to_bits(), "effect": "effects/burn.effect.ron", "level": 2 }),
    )
    .expect("agent apply");
    let agent_state = ability_state(agent_world, guard);

    // Through the panel, in the edit world.
    let (mut app, _) = fire_bolt_app();
    let world = app.world_mut();
    world.init_resource::<crate::command::UndoStack>();
    with_guard_set(world);
    let guard = world.spawn(regia_scene::prelude::Name::new("guard")).id();
    let call = |world: &mut World, params: Value| method("editor.abilities_panel_call")(world, &params);
    let v = call(
        world,
        json!({ "op": "add_attributes", "entity": guard.to_bits(), "asset": "attrs/guard.attrs.ron" }),
    )
    .expect("panel add");
    assert_eq!(v["answer"]["attributes"], 2, "{v}");
    let v = call(
        world,
        json!({ "op": "apply_effect", "entity": guard.to_bits(), "effect": "effects/burn.effect.ron", "level": 2 }),
    )
    .expect("panel apply");
    assert_eq!(v["answer"]["outcome"], "applied", "{v}");
    assert_eq!(
        ability_state(world, guard),
        agent_state,
        "the agent's world state"
    );

    // One undo step each: the effect goes first, then the attribute set.
    assert_eq!(
        world
            .resource::<crate::command::UndoStack>()
            .undo_descriptions()
            .len(),
        2
    );
    assert!(crate::command::undo_world(world));
    assert!(
        world
            .get::<AppliedEffects>(guard)
            .is_none_or(|l| l.list.is_empty()),
        "undo removed the effect"
    );
    assert!(world.get::<Attributes>(guard).is_some());
    assert!(crate::command::undo_world(world));
    assert!(world.get::<Attributes>(guard).is_none(), "undo removed the set");
    assert!(crate::command::redo_world(world));
    assert!(crate::command::redo_world(world));
    assert_eq!(ability_state(world, guard), agent_state, "redo restores both");

    // Refusals keep their codes and push nothing.
    for (params, expected) in [
        (
            json!({ "op": "add_attributes", "entity": guard.to_bits(), "asset": "attrs/none.attrs.ron" }),
            "asset_not_found",
        ),
        (
            json!({ "op": "apply_effect", "entity": guard.to_bits(), "effect": "effects/none.effect.ron" }),
            "effect_not_found",
        ),
        (
            json!({ "op": "apply_effect", "entity": guard.to_bits(), "effect": "effects/burn.effect.ron", "level": -1 }),
            "abilities_panel_bad_value",
        ),
    ] {
        let e = call(world, params.clone()).expect_err("refused");
        assert_eq!(reason(&e), expected, "{params}");
    }
    assert_eq!(
        world
            .resource::<crate::command::UndoStack>()
            .undo_descriptions()
            .len(),
        2
    );

    // The buttons are drawn for an inspected entity.
    method("editor.abilities_panel_select")(world, &json!({ "entity": guard.to_bits() })).expect("pin");
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let mut tree = None;
    for _ in 0..3 {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 2400.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| abilities_panel(ui, world));
        output.textures_delta.clear();
        tree = output.platform_output.accesskit_update;
    }
    let tree = tree.expect("accesskit is on");
    for label in ["Add attributes", "Apply effect"] {
        assert!(
            tree.nodes
                .iter()
                .any(|(_, n)| n.label() == Some(label) && n.role() == egui::accesskit::Role::Button),
            "the panel draws `{label}`"
        );
    }
}
