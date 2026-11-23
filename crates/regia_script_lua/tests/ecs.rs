// SPDX-License-Identifier: MPL-2.0
//! The plugin in an `App`: startup loading, `on_tick` in `FixedUpdate`, entity handles over the
//! ECS (slot reuse included), and the `lua.*` agent methods (`book/agents/lua-mods.md`).

use regia_agent::extension::AgentExtension;
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_scene::Transform;
use regia_scene::components::Name;
use regia_script_lua::{
    EcsModWorld, LuaComponent, LuaMods, LuaModsPlugin, LuaSpawned, ModFiles, ModWorld, ScriptValue,
    reset_vec3_userdata_creates, vec3_userdata_creates,
};
use serde_json::{Value, json};

fn mod_files(id: &str, main: &str) -> ModFiles {
    ModFiles::from_map([
        (
            "luamod.ron".to_owned(),
            format!(
                r#"(format_version: 1, id: "{id}", name: "{id}", version: "0.1.0", entry: "main.lua", api_version: 1)"#
            ),
        ),
        ("main.lua".to_owned(), main.to_owned()),
    ])
}

const MOVER: &str = r#"
local crate
function on_start()
  crate = regia.find("crate")
  regia.on("push", function(v) crate:set_position(crate:position() + regia.vec3(v.dx, 0, 0)) end)
end
function on_tick(dt)
  local p = crate:position()
  if p then crate:set_position(p + regia.vec3(0, 1, 0)) end
  regia.emit("moved", crate:position())
end
"#;

fn app_with(mods: Vec<ModFiles>) -> (App, Entity) {
    let mut app = App::new();
    let crate_entity = app
        .world_mut()
        .spawn((Name::new("crate"), Transform::from_translation(Vec3::ZERO)))
        .id();
    app.add_plugins(LuaModsPlugin::new(mods).with_seed(9));
    app.world_mut().run_schedule(Startup);
    (app, crate_entity)
}

fn call(app: &mut App, method: &str, params: Value) -> Value {
    let extension = app.world().resource::<AgentExtension>().clone();
    let (_, handler) = extension.method(method).expect("registered");
    handler(app.world_mut(), &params).expect("answers")
}

#[test]
fn the_plugin_ticks_mods_against_the_ecs_and_delivers_events() {
    let (mut app, crate_entity) = app_with(vec![mod_files("mover", MOVER)]);
    for _ in 0..3 {
        app.world_mut().run_schedule(FixedUpdate);
    }
    let y = app
        .world()
        .get::<Transform>(crate_entity)
        .expect("transform")
        .translation
        .y;
    assert_eq!(y, 3.0);
    app.world_mut().resource_mut::<LuaMods>().fire(
        "push",
        ScriptValue::Map(
            [("dx".to_owned(), ScriptValue::Number(2.0))]
                .into_iter()
                .collect(),
        ),
    );
    app.world_mut().run_schedule(FixedUpdate);
    let t = app
        .world()
        .get::<Transform>(crate_entity)
        .expect("transform")
        .translation;
    assert_eq!(t, Vec3::new(2.0, 4.0, 0.0), "tick, then the queued push");
    let moved = app.world_mut().resource_mut::<LuaMods>().drain_emitted();
    assert_eq!(moved.len(), 4);
    assert_eq!(
        moved[3].value,
        ScriptValue::Vec3([0.0, 4.0, 0.0]),
        "emitted before the push was delivered"
    );
}

#[test]
fn handles_to_despawned_and_reused_ecs_slots_are_stale() {
    let (mut app, crate_entity) = app_with(vec![mod_files(
        "keeper",
        "function on_start() kept = regia.find('crate') end",
    )]);
    // Reuse the slot: the despawn hands back the slot's next generation (the allocator batches
    // ordinary frees, so a plain spawn would not reuse it soon) and the new entity takes it.
    let next = app
        .world_mut()
        .despawn_no_free(crate_entity)
        .expect("the crate was alive");
    let reused = app
        .world_mut()
        .spawn_at(next, (Name::new("other"), Transform::from_translation(Vec3::ONE)))
        .expect("the slot is free")
        .id();
    assert_eq!(reused.index(), crate_entity.index());
    assert_ne!(reused, crate_entity);
    let r = call(
        &mut app,
        "lua.run",
        json!({ "mod": "keeper", "chunk": "return kept:alive(), kept:position(), kept:set_position(regia.vec3(5, 5, 5))" }),
    );
    assert_eq!(r["ok"], json!(true), "{r}");
    assert_eq!(r["results"], json!([false, null, false]));
    assert_eq!(
        app.world().get::<Transform>(reused).expect("t").translation,
        Vec3::ONE
    );
}

#[test]
fn mods_despawn_only_what_they_spawned() {
    let (mut app, crate_entity) = app_with(vec![mod_files("spawner", "")]);
    let r = call(
        &mut app,
        "lua.run",
        json!({ "mod": "spawner", "chunk": "local e = regia.spawn('lamp', regia.vec3(1, 2, 3)) lamp = e return regia.find('crate'):despawn(), e:name()" }),
    );
    assert_eq!(r["results"], json!([false, "lamp"]), "{r}");
    assert!(app.world().get_entity(crate_entity).is_ok());
    let mut q = app.world_mut().query::<(&Name, &LuaSpawned, &Transform)>();
    let spawned: Vec<_> = q
        .iter(app.world())
        .map(|(n, s, t)| (n.as_str().to_owned(), s.mod_id.clone(), t.translation))
        .collect();
    assert_eq!(
        spawned,
        [("lamp".to_owned(), "spawner".to_owned(), Vec3::new(1.0, 2.0, 3.0))]
    );
    let r = call(
        &mut app,
        "lua.run",
        json!({ "mod": "spawner", "chunk": "return lamp:despawn(), lamp:alive()" }),
    );
    assert_eq!(r["results"], json!([true, false]));
}

#[test]
fn agent_methods_list_run_inspect_and_restart() {
    let (mut app, _) = app_with(vec![
        mod_files("cfg", "config = { speed = 4 } regia.on('hit', function() end)"),
        mod_files("broken", "function on_tick() local x = nil; return x.y end"),
    ]);
    let extension = app.world().resource::<AgentExtension>().clone();
    for name in ["lua.list", "lua.run", "lua.globals", "lua.errors", "lua.restart"] {
        assert!(extension.method_schema(name).is_some(), "{name}");
    }
    assert!(extension.capabilities().iter().any(|(n, _)| *n == "lua_mods"));

    app.world_mut().run_schedule(FixedUpdate);
    let list = call(&mut app, "lua.list", json!({}));
    assert_eq!(list["plugin"], json!(true));
    assert_eq!(list["seed"], json!(9));
    assert_eq!(list["emitted_dropped"], json!(0));
    assert!(list["emitted_held"].is_number());
    let mods = list["mods"].as_array().expect("mods");
    assert_eq!(
        mods.iter().map(|m| m["id"].clone()).collect::<Vec<_>>(),
        [json!("broken"), json!("cfg")]
    );
    assert_eq!(mods[0]["status"], json!("disabled"));
    assert_eq!(mods[0]["code"], json!("lua_runtime"));
    assert_eq!(mods[1]["status"], json!("active"));
    assert_eq!(mods[1]["subscriptions"], json!(["hit"]));

    let globals = call(&mut app, "lua.globals", json!({ "mod": "cfg", "path": "config" }));
    assert_eq!(globals["path"], json!("config"));
    assert_eq!(
        globals["entries"],
        json!([{ "key": "speed", "type": "number", "value": 4.0 }])
    );
    let leaf = call(
        &mut app,
        "lua.globals",
        json!({ "mod": "cfg", "path": "config.speed" }),
    );
    assert_eq!(leaf["path"], json!("config.speed"));
    assert_eq!(leaf["entries"], json!([]));
    assert_eq!(leaf["error"]["code"], json!("lua_bad_value"));

    let errors = call(&mut app, "lua.errors", json!({}));
    let errors = errors["errors"].as_array().expect("errors");
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["mod"], json!("broken"));
    assert_eq!(errors[0]["call"], json!("on_tick"));

    let scratch = call(
        &mut app,
        "lua.run",
        json!({ "chunk": "print('hi') return 1 + 2, regia.find('crate') ~= nil, function() end" }),
    );
    assert_eq!(scratch["ok"], json!(true));
    assert_eq!(scratch["results"], json!([3.0, true, "<function>"]));
    assert_eq!(scratch["log"], json!(["hi"]));
    let budget = call(&mut app, "lua.run", json!({ "chunk": "while true do end" }));
    assert_eq!(budget["ok"], json!(false));
    assert_eq!(budget["error"]["code"], json!("lua_instruction_budget"));

    let restarted = call(&mut app, "lua.restart", json!({ "mod": "broken" }));
    assert_eq!(restarted["status"], json!("active"));

    let extension = app.world().resource::<AgentExtension>().clone();
    let (_, globals) = extension.method("lua.globals").expect("registered");
    let missing = globals(app.world_mut(), &json!({ "mod": "nope" })).expect_err("unknown mod");
    assert_eq!(
        missing.data.as_ref().and_then(|d| d.get("code")).cloned(),
        Some(json!("F0003"))
    );
    let bad = globals(app.world_mut(), &json!({})).expect_err("mod required");
    assert_eq!(
        bad.data.as_ref().and_then(|d| d.get("code")).cloned(),
        Some(json!("F0002"))
    );
    let (_, run) = extension.method("lua.run").expect("registered");
    let bad = run(app.world_mut(), &json!({ "chunk": 3 })).expect_err("chunk must be a string");
    assert_eq!(
        bad.data.as_ref().and_then(|d| d.get("code")).cloned(),
        Some(json!("F0002"))
    );
}

#[test]
fn without_the_plugin_resource_methods_report_plugin_false() {
    let (mut app, _) = app_with(Vec::new());
    app.world_mut().remove_resource::<LuaMods>();
    for method in ["lua.list", "lua.errors"] {
        assert_eq!(
            call(&mut app, method, json!({}))["plugin"],
            json!(false),
            "{method}"
        );
    }
    assert_eq!(
        call(&mut app, "lua.run", json!({ "chunk": "return 1" }))["plugin"],
        json!(false)
    );
}

#[test]
fn find_breaks_a_name_tie_by_the_lowest_entity_index() {
    let (mut app, first) = app_with(vec![mod_files("finder", "")]);
    // A second `crate` with a different archetype, so iteration order is not spawn order.
    let second = app
        .world_mut()
        .spawn((
            Name::new("crate"),
            LuaSpawned {
                mod_id: "finder".to_owned(),
            },
        ))
        .id();
    let lowest = first.index_u32().min(second.index_u32());
    let world = app.world_mut();
    let found = EcsModWorld::new(world).find("crate").expect("named");
    assert_eq!(Entity::from_bits(found).index_u32(), lowest);
    let mut scan = EcsModWorld::new(world);
    let _ = scan.find("crate");
    // The name index probes once. The old linear scan walked every named entity.
    assert_eq!(scan.take_scanned(), 1);
    assert_eq!(scan.take_scanned(), 0);
}

#[test]
fn every_lua_extension_method_has_a_book_page() {
    let (app, _) = app_with(Vec::new());
    let names = app.world().resource::<AgentExtension>().method_names();
    assert!(names.contains(&"lua.reload"), "{names:?}");
    let gaps = regia_test_utils::book::extension_gaps(
        &regia_test_utils::book::workspace_root(),
        &names,
        "regia_script_lua",
        &regia_agent::mentions_token,
    );
    assert!(gaps.is_empty(), "undocumented Lua methods:\n{}", gaps.join("\n"));
}

#[test]
fn find_among_equal_names_answers_the_same_entity_every_time() {
    use regia_script_lua::{EcsModWorld, ModWorld};

    let (mut app, first) = app_with(vec![mod_files("m", "")]);
    let world = app.world_mut();
    // The lowest entity index (ADR-1957), which is not the lowest entity bits: the ECS stores an
    // entity's row inverted.
    let mut lowest = first;
    for _ in 0..4 {
        let entity = world.spawn((Name::new("crate"), Transform::default())).id();
        if entity.index_u32() < lowest.index_u32() {
            lowest = entity;
        }
    }
    let mut ecs = EcsModWorld::new(world);
    assert_eq!(ecs.find("crate"), Some(lowest.to_bits()));
    // The name index answers a probe, not the world's entity count.
    assert_eq!(ecs.find_cost(), 1);
}

/// `regia.find` of one name must not walk, or be charged for, a pile of unrelated names.
#[test]
fn find_cost_does_not_follow_the_unrelated_name_pile() {
    fn measure(extra: u32) -> (u64, u64, bool, bool) {
        let (mut app, _) = app_with(vec![mod_files(
            "finder",
            r#"
            function on_tick()
              found = regia.find("target")
            end
            "#,
        )]);
        let world = app.world_mut();
        world.spawn((Name::new("target"), Transform::default()));
        for i in 0..extra {
            world.spawn((Name::new(format!("other{i}")), Transform::default()));
        }
        world.resource_scope::<LuaMods, _>(|world, mut mods| {
            let mut ecs = EcsModWorld::new(world);
            mods.call_tick(&mut ecs, 0.0);
            let scanned = ecs.take_scanned();
            let found = ecs.find("target").is_some();
            let module = mods.get("finder").expect("loaded");
            (
                scanned,
                module.counters().last_instructions,
                module.is_active(),
                found,
            )
        })
    }
    let small = measure(8);
    let large = measure(400);
    assert!(small.2 && large.2, "both stay active: {small:?} {large:?}");
    assert!(small.3 && large.3, "the target is visible: {small:?} {large:?}");
    assert_eq!(
        small.0, large.0,
        "scanned entities must not follow the unrelated name pile: {small:?} {large:?}"
    );
    assert!(
        small.0 <= 2,
        "one indexed probe (plus a stale rebuild), got {}",
        small.0
    );
    assert_eq!(
        small.1, large.1,
        "instructions must not follow the unrelated name pile: {small:?} {large:?}"
    );
}

/// A `Name` edited in place (an inspector rename, a game system writing `&mut Name`) fires no
/// insert observer. `regia.find` must see the new name and stop answering the old one
/// (correctness audit, lua-telemetry-matgraph).
#[test]
fn find_sees_a_name_edited_in_place() {
    let (mut app, crate_entity) = app_with(vec![mod_files("finder", "")]);
    let world = app.world_mut();
    assert_eq!(
        EcsModWorld::new(world).find("crate"),
        Some(crate_entity.to_bits())
    );
    *world.get_mut::<Name>(crate_entity).expect("named") = Name::new("barrel");
    let mut ecs = EcsModWorld::new(world);
    assert_eq!(
        ecs.find("barrel"),
        Some(crate_entity.to_bits()),
        "the renamed entity is found under its new name"
    );
    assert_eq!(ecs.find("crate"), None, "and no longer under the old one");

    // The same through a mod: a later tick of the app sees the rename too.
    app.world_mut().run_schedule(FixedUpdate);
    *app.world_mut().get_mut::<Name>(crate_entity).expect("named") = Name::new("keg");
    app.world_mut().run_schedule(FixedUpdate);
    let world = app.world_mut();
    assert_eq!(EcsModWorld::new(world).find("keg"), Some(crate_entity.to_bits()));
}

/// A script component whose declaration is edited in place is listed under the new declaration
/// only, and an ownership mark edited in place moves the owned count.
#[test]
fn lookups_follow_components_and_marks_edited_in_place() {
    let (mut app, _) = app_with(vec![mod_files("m", "")]);
    let world = app.world_mut();
    let entity = world
        .spawn((
            LuaComponent {
                decl: "old".to_owned(),
                props: std::collections::BTreeMap::new(),
            },
            LuaSpawned {
                mod_id: "first".to_owned(),
            },
        ))
        .id();
    {
        let mut ecs = EcsModWorld::new(world);
        assert_eq!(ecs.entities_with("old"), vec![entity.to_bits()]);
        assert_eq!(ecs.owned_count("first"), 1);
    }
    world.get_mut::<LuaComponent>(entity).expect("component").decl = "new".to_owned();
    world.get_mut::<LuaSpawned>(entity).expect("mark").mod_id = "second".to_owned();
    let mut ecs = EcsModWorld::new(world);
    assert_eq!(ecs.entities_with("new"), vec![entity.to_bits()]);
    assert!(ecs.entities_with("old").is_empty());
    assert_eq!(ecs.owned_count("first"), 0);
    assert_eq!(ecs.owned_count("second"), 1);
    assert_eq!(ecs.owned_count("second"), ecs.owned("second").len());
}

/// A component `on_update` looks at the entities that carry it, not every script component.
#[test]
fn a_component_update_visits_only_entities_that_carry_it() {
    let main = r#"
        regia.component("drifter", {
            props = { step = { type = "number", default = 1 } },
            on_update = function(e, dt) end,
        })
        function on_start()
            for i = 1, 3 do
                regia.spawn("d" .. i):add("drifter")
            end
        end
    "#;
    let (mut app, _) = app_with(vec![mod_files("drift", main)]);
    for _ in 0..40 {
        app.world_mut().spawn(LuaComponent {
            decl: "bystander".to_owned(),
            props: std::collections::BTreeMap::new(),
        });
    }
    app.world_mut().resource_scope::<LuaMods, _>(|world, mut mods| {
        let mut ecs = EcsModWorld::new(world);
        mods.call_update(&mut ecs, 0.016);
        let visits = ecs.take_component_visits();
        assert!(mods.get("drift").expect("loaded").is_active(), "the pass runs");
        assert_eq!(
            visits, 3,
            "the pass examines the three drifters, not the bystanders"
        );
    });
}

/// Temporary `vec3` values from a tick are collectable, and a stored one keeps its numbers.
#[test]
fn a_stored_vec3_stays_stable_and_gc_reclaims_temporaries() {
    let main = r#"
        function on_start()
            regia.state.home = regia.vec3(0, 1, 0)
        end
        function on_tick()
            local p = regia.find("crate"):position()
            regia.state.step = p + regia.vec3(0, 1, 0)
            local home = regia.state.home
            if home.x ~= 0 or home.y ~= 1 or home.z ~= 0 then
                error("stored vec3 changed")
            end
        end
    "#;
    let (mut app, _) = app_with(vec![mod_files("holder", main)]);
    app.world_mut().resource_scope::<LuaMods, _>(|world, mut mods| {
        let module = mods.get("holder").expect("loaded");
        assert!(module.is_active(), "load");
        let baseline = module.memory_after_gc();
        reset_vec3_userdata_creates();
        for _ in 0..30 {
            mods.call_tick(&mut EcsModWorld::new(world), 0.0);
        }
        let module = mods.get("holder").expect("loaded");
        assert!(module.is_active(), "the stored vec3 stayed put");
        let after = module.memory_after_gc();
        let created = vec3_userdata_creates();
        assert!(
            after <= baseline.saturating_add(65_536),
            "memory after gc {after} baseline {baseline} vec3 userdata created {created}"
        );
    });
}

mod saves {
    //! Through the real save game: a section of the file, every path, the agent methods.

    use regia_framework::save_flow::{load_from_store, save_to_store};
    use regia_framework::save_game::{SaveDirectory, SaveGame, SavePlugin, SaveSections, SaveSpec, Saveable};

    use super::*;

    const SCORER: &str = r#"
    function on_start()
      regia.state.score = 0
      regia.spawn("beacon", regia.vec3(1, 2, 3))
    end
    function on_tick() regia.state.score = regia.state.score + 1 end
    function on_load(info) regia.emit("loaded", info.state) end
    "#;

    fn app(dir: &std::path::Path, mods: Vec<ModFiles>) -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, SavePlugin::default()));
        app.add_plugins(LuaModsPlugin::new(mods).with_seed(9));
        app.insert_resource(SaveDirectory(dir.to_path_buf()));
        app.world_mut().run_schedule(Startup);
        app
    }

    fn score(app: &mut App) -> Value {
        call(
            app,
            "lua.run",
            json!({ "mod": "scorer", "chunk": "return regia.state.score" }),
        )["results"][0]
            .clone()
    }

    fn beacons(app: &mut App) -> usize {
        let world = app.world_mut();
        let mut query = world.query::<(&Name, &LuaSpawned)>();
        query.iter(world).filter(|(n, _)| n.as_str() == "beacon").count()
    }

    #[test]
    fn the_plugin_registers_its_section_once() {
        let dir = regia_test_utils::unique_temp_dir("lua_save_section");
        let app = app(&dir, Vec::new());
        assert_eq!(
            app.world().resource::<SaveSections>().keys().collect::<Vec<_>>(),
            [regia_script_lua::LUA_STATE_SECTION]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn state_goes_through_a_save_file_into_a_new_app() {
        let dir = regia_test_utils::unique_temp_dir("lua_save_file");
        let mut first = app(&dir, vec![mod_files("scorer", SCORER)]);
        first.world_mut().spawn((Saveable, Name::new("game_thing")));
        for _ in 0..4 {
            first.world_mut().run_schedule(FixedUpdate);
        }
        let saved = {
            let slot = SaveGame::slot(&SaveDirectory(dir.clone()), "Test");
            SaveGame::save(&slot, first.world_mut(), &SaveSpec::default()).expect("save")
        };
        assert!(saved.resource_count >= 1, "the section is in the file");

        let mut second = app(&dir, vec![mod_files("scorer", SCORER)]);
        assert_eq!(score(&mut second), json!(0.0));
        assert_eq!(beacons(&mut second), 1);
        let slot = SaveGame::slot(&SaveDirectory(dir.clone()), "Test");
        SaveGame::load(&slot, second.world_mut(), &SaveSpec::default()).expect("load");
        assert_eq!(score(&mut second), json!(4.0));
        assert_eq!(beacons(&mut second), 1, "the saved one replaced the fresh one");
        let emitted = second.world_mut().resource_mut::<LuaMods>().drain_emitted();
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].value, ScriptValue::String("carried".into()));

        let listed = call(&mut second, "lua.list", json!({}));
        assert_eq!(listed["last_restore"]["mods"][0]["id"], json!("scorer"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_save_flow_and_a_second_load_in_one_process_work_too() {
        let dir = regia_test_utils::unique_temp_dir("lua_save_flow");
        let mut app = app(&dir, vec![mod_files("scorer", SCORER)]);
        app.world_mut().run_schedule(FixedUpdate);
        save_to_store(app.world_mut(), "Quick").expect("save");
        let listed = call(&mut app, "lua.list", json!({}));
        assert!(listed["last_save"]["bytes"].as_u64().is_some_and(|b| b > 8));
        for _ in 0..3 {
            app.world_mut().run_schedule(FixedUpdate);
        }
        assert_eq!(score(&mut app), json!(4.0));
        load_from_store(app.world_mut(), "Quick").expect("load");
        assert_eq!(score(&mut app), json!(1.0));
        load_from_store(app.world_mut(), "Quick").expect("load again");
        assert_eq!(score(&mut app), json!(1.0));
        assert_eq!(beacons(&mut app), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_save_made_with_no_mods_loads_and_restarts_the_mods() {
        let dir = regia_test_utils::unique_temp_dir("lua_save_old");
        // A save from before the section: an app with no plugin writes it.
        let mut plain = App::new();
        plain.add_plugins((MinimalPlugins, SavePlugin::default()));
        plain.insert_resource(SaveDirectory(dir.clone()));
        save_to_store(plain.world_mut(), "Old").expect("save");

        let mut app = app(&dir, vec![mod_files("scorer", SCORER)]);
        app.world_mut().run_schedule(FixedUpdate);
        load_from_store(app.world_mut(), "Old").expect("load");
        assert_eq!(score(&mut app), json!(0.0));
        assert_eq!(beacons(&mut app), 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}

mod reload_methods {
    use super::*;

    const V1: &str = "function on_start() regia.state.n = 1 regia.spawn('lamp') end function on_load() end";

    #[test]
    fn lua_reload_answers_with_the_outcome() {
        let (mut app, _) = app_with(vec![mod_files("m", V1)]);
        let manifest = mod_files("m", "")
            .get("luamod.ron")
            .unwrap_or_default()
            .to_owned();
        let good = call(
            &mut app,
            "lua.reload",
            json!({ "mod": "m", "files": { "luamod.ron": manifest, "main.lua": "regia.state_version = 2 function on_migrate(s) s.n = s.n + 1 end function on_load() end" } }),
        );
        assert_eq!(good["plugin"], json!(true));
        assert_eq!(good["ok"], json!(true));
        assert_eq!(good["stage"], json!("done"));
        assert_eq!(good["state"], json!("migrated"));
        assert_eq!(good["from_version"], json!(1));
        assert_eq!(good["to_version"], json!(2));
        assert_eq!(good["status"], json!("active"));

        let bad = call(
            &mut app,
            "lua.reload",
            json!({ "mod": "m", "files": { "luamod.ron": manifest, "main.lua": "broken(" } }),
        );
        assert_eq!(bad["ok"], json!(false));
        assert_eq!(bad["stage"], json!("compile"));
        assert_eq!(bad["error"]["code"], json!("lua_syntax"));
        assert_eq!(bad["status"], json!("active"));

        let listed = call(&mut app, "lua.list", json!({}));
        assert_eq!(listed["mods"][0]["reloads"], json!(1));
        assert_eq!(listed["mods"][0]["failed_reloads"], json!(1));
        assert_eq!(listed["mods"][0]["last_reload"]["stage"], json!("compile"));
        let errors = call(&mut app, "lua.errors", json!({ "mod": "m" }));
        assert_eq!(errors["errors"][0]["call"], json!("reload"));

        let extension = app.world().resource::<AgentExtension>().clone();
        let (_, handler) = extension.method("lua.reload").expect("registered");
        assert!(handler(app.world_mut(), &json!({ "mod": "nobody" })).is_err());
        assert!(handler(app.world_mut(), &json!({})).is_err());
    }

    #[test]
    fn lua_state_shows_what_would_be_saved_or_why_not() {
        let (mut app, _) = app_with(vec![mod_files("m", V1)]);
        let plain = call(&mut app, "lua.state", json!({ "mod": "m", "tree": true }));
        assert_eq!(plain["ok"], json!(true));
        assert_eq!(plain["state_version"], json!(1));
        assert_eq!(plain["entities"], json!(1));
        assert_eq!(plain["tree"], json!({ "n": 1.0 }));
        let small = call(&mut app, "lua.state", json!({ "mod": "m" }));
        assert!(small.get("tree").is_none());

        call(
            &mut app,
            "lua.run",
            json!({ "mod": "m", "chunk": "regia.state.f = print" }),
        );
        let refused = call(&mut app, "lua.state", json!({ "mod": "m" }));
        assert_eq!(refused["ok"], json!(false));
        assert_eq!(refused["error"]["code"], json!("lua_bad_value"));

        let extension = app.world().resource::<AgentExtension>().clone();
        let (_, handler) = extension.method("lua.state").expect("registered");
        assert!(handler(app.world_mut(), &json!({ "mod": "nobody" })).is_err());
    }

    #[test]
    fn a_restart_during_a_reload_finds_what_it_just_spawned_not_what_is_about_to_go() {
        // No `on_load`, so the reload restarts the mod: the old lamp waits for the commit and has
        // the lower entity bits, and the new one has the same name.
        let v1 = "function on_start() regia.spawn('lamp') end";
        let v2 = "function on_start() regia.spawn('lamp') found = regia.find('lamp') ~= nil end";
        let (mut app, _) = app_with(vec![mod_files("m", v1)]);
        let files = mod_files("m", v2);
        let outcome = app.world_mut().resource_scope::<LuaMods, _>(|world, mut mods| {
            mods.reload("m", Some(files), &mut regia_script_lua::EcsModWorld::new(world))
        });
        assert!(outcome.is_ok_and(|outcome| outcome.ok));
        let seen = call(
            &mut app,
            "lua.run",
            json!({ "mod": "m", "chunk": "return found" }),
        );
        assert_eq!(seen["results"][0], json!(true));
        let world = app.world_mut();
        let mut query = world.query::<(&Name, &LuaSpawned)>();
        assert_eq!(
            query.iter(world).filter(|(n, _)| n.as_str() == "lamp").count(),
            1,
            "the old lamp went at the commit"
        );
    }

    #[test]
    fn lua_watch_reads_and_sets_the_interval() {
        let dir = regia_test_utils::unique_temp_dir("lua_watch_method");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let (manifest, main) = {
            let files = mod_files("m", V1);
            (
                files.get("luamod.ron").unwrap_or_default().to_owned(),
                files.get("main.lua").unwrap_or_default().to_owned(),
            )
        };
        std::fs::write(dir.join("luamod.ron"), &manifest).expect("write");
        std::fs::write(dir.join("main.lua"), &main).expect("write");
        let files = ModFiles::from_dir(&dir, 1024 * 1024).expect("reads");
        let (mut app, _) = app_with(vec![files]);
        assert_eq!(call(&mut app, "lua.watch", json!({}))["every_frames"], json!(0));
        let set = call(&mut app, "lua.watch", json!({ "every_frames": 1 }));
        assert_eq!(set["every_frames"], json!(1));
        assert_eq!(set["mods"], json!(["m"]));

        // The running app notices a save on the next frame.
        std::fs::write(dir.join("main.lua"), format!("{main}\nchanged = true")).expect("write");
        app.world_mut().run_schedule(Update);
        let seen = call(
            &mut app,
            "lua.run",
            json!({ "mod": "m", "chunk": "return changed" }),
        );
        assert_eq!(seen["results"][0], json!(true));

        let extension = app.world().resource::<AgentExtension>().clone();
        let (_, handler) = extension.method("lua.watch").expect("registered");
        assert!(handler(app.world_mut(), &json!({ "every_frames": -1 })).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_methods_answer_plugin_false_without_the_resource() {
        let (mut app, _) = app_with(Vec::new());
        app.world_mut().remove_resource::<LuaMods>();
        for (method, params) in [
            ("lua.reload", json!({ "mod": "m" })),
            ("lua.state", json!({ "mod": "m" })),
            ("lua.watch", json!({})),
        ] {
            assert_eq!(call(&mut app, method, params)["plugin"], json!(false), "{method}");
        }
    }
}

#[test]
fn the_plugin_builders_reach_the_inserted_host() {
    let limits = regia_script_lua::LuaLimits {
        instruction_budget: 12_345,
        ..regia_script_lua::LuaLimits::default()
    };
    let mut app = App::new();
    app.add_plugins(LuaModsPlugin::new(Vec::new()).with_watch(4).with_limits(limits));
    let mods = app.world().resource::<LuaMods>();
    assert_eq!(mods.watch_every(), 4);
    assert_eq!(mods.limits().instruction_budget, 12_345);
    let mut default_app = App::new();
    default_app.add_plugins(LuaModsPlugin::new(Vec::new()));
    assert_eq!(default_app.world().resource::<LuaMods>().watch_every(), 0);
}

/// `lua.entity_cap` (ADR-2418): reads the cap and what the mod owns in the ECS, lowers it for one
/// mod, refuses the next spawn with `lua_entity_cap`, and goes back to the host's with `null`.
#[test]
fn the_entity_cap_method_reads_sets_and_refuses() {
    let spawner = "function spawn_one() return pcall(regia.spawn, 'unit') end";
    let (mut app, _) = app_with(vec![mod_files("spawner", spawner)]);
    let read = call(&mut app, "lua.entity_cap", json!({ "mod": "spawner" }));
    assert_eq!(read["plugin"], json!(true));
    let host_cap = read["host_cap"].clone();
    assert_eq!(read["entity_cap"], host_cap);
    assert_eq!(read["owned"], json!(0));
    let set = call(&mut app, "lua.entity_cap", json!({ "mod": "spawner", "cap": 1 }));
    assert_eq!(set["entity_cap"], json!(1));
    assert_eq!(set["override"], json!(1));
    for expected in [true, false] {
        let ran = call(
            &mut app,
            "lua.run",
            json!({ "mod": "spawner", "chunk": "return spawn_one()" }),
        );
        assert_eq!(ran["results"][0], json!(expected), "{ran}");
    }
    let after = call(&mut app, "lua.entity_cap", json!({ "mod": "spawner" }));
    assert_eq!(after["owned"], json!(1));
    assert_eq!(after["refused"], json!(1));
    let listed = call(&mut app, "lua.list", json!({}));
    assert_eq!(listed["mods"][0]["entity_cap"], json!(1));
    assert_eq!(listed["mods"][0]["entity_cap_refused"], json!(1));
    let reset = call(
        &mut app,
        "lua.entity_cap",
        json!({ "mod": "spawner", "cap": null }),
    );
    assert_eq!(reset["entity_cap"], host_cap);
    assert_eq!(reset["override"], Value::Null);
    let extension = app.world().resource::<AgentExtension>().clone();
    let (_, handler) = extension.method("lua.entity_cap").expect("registered");
    assert!(handler(app.world_mut(), &json!({ "mod": "nope" })).is_err());
    assert!(handler(app.world_mut(), &json!({ "mod": "spawner", "cap": -1 })).is_err());
    assert!(handler(app.world_mut(), &json!({})).is_err());
}

/// Finding 5 (`sprint-deep-zai-script-lua`): an owned entity whose `Name` or `Transform` the
/// game removed used to be dropped from the save silently, and the mod's `on_load` could not
/// find it after a load with nothing in the report to say why. The save report now counts every
/// owned entity it leaves out.
#[test]
fn an_owned_entity_without_a_name_or_transform_is_counted_out_of_the_save() {
    let (mut app, _) = app_with(vec![mod_files(
        "pool",
        r#"
        function on_start()
          regia.spawn('lamp', regia.vec3(1, 2, 3))
          regia.spawn('sign', regia.vec3(4, 5, 6))
        end
        "#,
    )]);
    // A pooling system strips one entity's Transform and the other's Name.
    let mut lamp = None;
    let mut sign = None;
    let mut query = app.world_mut().query::<(Entity, &Name, &LuaSpawned)>();
    for (entity, name, spawned) in query.iter(app.world()) {
        if spawned.mod_id != "pool" {
            continue;
        }
        match name.as_str() {
            "lamp" => lamp = Some(entity),
            "sign" => sign = Some(entity),
            _ => {}
        }
    }
    app.world_mut()
        .entity_mut(lamp.expect("the lamp"))
        .remove::<Transform>();
    app.world_mut()
        .entity_mut(sign.expect("the sign"))
        .remove::<Name>();

    let (_, report) = app.world_mut().resource_scope::<LuaMods, _>(|world, mut mods| {
        let mut ecs = EcsModWorld::new(world);
        mods.save_state(&mut ecs)
    });
    let line = report
        .mods
        .iter()
        .find(|line| line.id == "pool")
        .expect("a line per mod");
    assert_eq!(
        line.skipped_entities, 2,
        "both stripped entities are counted, not dropped silently"
    );
    assert!(line.error.is_none(), "{line:?}");
    let state = app.world_mut().resource_scope::<LuaMods, _>(|world, mods| {
        let mut ecs = EcsModWorld::new(world);
        mods.peek_state("pool", &mut ecs)
    });
    assert_eq!(state.expect("the mod saves").entities.len(), 0);
}
