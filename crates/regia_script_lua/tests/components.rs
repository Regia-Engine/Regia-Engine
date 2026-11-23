// SPDX-License-Identifier: MPL-2.0
//! Script components end to end: declaration, attach, per-entity edits, hot-reload reconcile,
//! rollback, cross-mod conflicts and a scene round trip (`docs/specs/84-lua-mods.md` § 13,
//! ADR-3356).

mod common;

use common::{TestWorld, host, mod_files};
use regia_core::math::Vec3;
use regia_script_lua::{LuaComponent, LuaLimits, LuaMods, ModStatus, ModWorld, ScriptValue};

/// A mod whose `mover` component declares every property type.
const MOVER: &str = r#"
    regia.component("mover", {
        props = {
            speed  = { type = "number", default = 2.0, min = 0.0, max = 20.0 },
            active = { type = "bool", default = true },
            label  = { type = "string", default = "mover" },
            offset = { type = "vec3", default = regia.vec3(0, 1, 0) },
            tint   = { type = "color", default = { 1.0, 0.5, 0.25 } },
            target = { type = "entity" },
            mesh   = { type = "asset", filter = "mesh" },
            mode   = { type = "enum", options = { "walk", "run" }, default = "walk" },
        },
    })
    function on_start()
        local e = regia.spawn("walker", regia.vec3(0, 0, 0))
        e:add("mover")
    end
"#;

#[test]
fn attach_applies_defaults_and_scripts_edit_and_read_values() {
    let mut mods = host();
    let mut world = TestWorld::default();
    mods.load(mod_files("props", "1.0.0", MOVER), &mut world)
        .expect("the mod loads");
    let entity = world.find("walker").expect("the spawned walker");

    let values: Vec<(String, ScriptValue)> = world
        .component_props(entity, "mover")
        .expect("the component is attached");
    let props: std::collections::BTreeMap<String, ScriptValue> = values.into_iter().collect();
    assert_eq!(props["speed"], ScriptValue::Number(2.0));
    assert_eq!(props["active"], ScriptValue::Bool(true));
    assert_eq!(props["label"], ScriptValue::String("mover".into()));
    assert_eq!(props["offset"], ScriptValue::Vec3([0.0, 1.0, 0.0]));
    assert_eq!(
        props["tint"],
        ScriptValue::List(vec![
            ScriptValue::Number(1.0),
            ScriptValue::Number(0.5),
            ScriptValue::Number(0.25),
            ScriptValue::Number(1.0),
        ])
    );
    assert_eq!(props["target"], ScriptValue::Nil);
    assert_eq!(props["mesh"], ScriptValue::String(String::new()));
    assert_eq!(props["mode"], ScriptValue::String("walk".into()));

    // A script edits a value; the edit is clamped to the declared range.
    let outcome = mods
        .run_chunk(
            &mut world,
            Some("props"),
            r#"local e = regia.find("walker")
               e:set_prop("mover", "speed", 99.0)
               return e:props("mover").speed"#,
        )
        .expect("the chunk runs");
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(outcome.results, vec![ScriptValue::Number(20.0)]);
    let values: std::collections::BTreeMap<String, ScriptValue> = world
        .component_props(entity, "mover")
        .expect("attached")
        .into_iter()
        .collect();
    assert_eq!(values["speed"], ScriptValue::Number(20.0));
}

#[test]
fn set_prop_rejects_unknown_properties_and_foreign_components() {
    let mut mods = host();
    let mut world = TestWorld::default();
    mods.load(mod_files("props", "1.0.0", MOVER), &mut world)
        .expect("the mod loads");
    for chunk in [
        // Unknown property.
        r#"local ok = pcall(function() regia.find("walker"):set_prop("mover", "nope", 1) end) return ok"#,
        // Wrong type.
        r#"local ok = pcall(function() regia.find("walker"):set_prop("mover", "speed", "fast") end) return ok"#,
        // Enum value outside the options.
        r#"local ok = pcall(function() regia.find("walker"):set_prop("mover", "mode", "fly") end) return ok"#,
        // A component this mod did not declare.
        r#"local ok = pcall(function() regia.find("walker"):add("elsewhere") end) return ok"#,
    ] {
        let outcome = mods.run_chunk(&mut world, Some("props"), chunk).expect("ran");
        assert!(outcome.ok, "{outcome:?}");
        assert_eq!(outcome.results, vec![ScriptValue::Bool(false)], "{chunk}");
    }
    // Refused writes leave values alone and the mod keeps running.
    assert!(mods.get("props").expect("loaded").is_active());
    let entity = world.find("walker").expect("the spawned walker");
    let values: std::collections::BTreeMap<String, ScriptValue> = world
        .component_props(entity, "mover")
        .expect("attached")
        .into_iter()
        .collect();
    assert_eq!(values["speed"], ScriptValue::Number(2.0));
}

#[test]
fn on_update_runs_once_per_entity_carrying_the_component() {
    let main = r#"
        regia.component("drifter", {
            props = { step = { type = "vec3", default = regia.vec3(1, 0, 0) } },
            on_update = function(e, dt)
                local p = e:props("drifter")
                e:set_position(e:position() + p.step * dt)
            end,
        })
        function on_start()
            self_a = regia.spawn("a", regia.vec3(0, 0, 0))
            self_a:add("drifter")
            local b = regia.spawn("b", regia.vec3(0, 0, 0))
            b:add("drifter")
            b:set_prop("drifter", "step", regia.vec3(0, 2, 0))
            plain = regia.spawn("plain", regia.vec3(9, 9, 9))
        end
    "#;
    let mut mods = host();
    let mut world = TestWorld::default();
    mods.load(mod_files("drift", "1.0.0", main), &mut world)
        .expect("the mod loads");
    mods.call_update(&mut world, 0.5);
    assert_eq!(world.position_of("a"), Some(Vec3::new(0.5, 0.0, 0.0)));
    assert_eq!(world.position_of("b"), Some(Vec3::new(0.0, 1.0, 0.0)));
    assert_eq!(
        world.position_of("plain"),
        Some(Vec3::new(9.0, 9.0, 9.0)),
        "an entity without the component is untouched"
    );
}

/// An `on_update` attached to nobody must not accumulate instruction charges.
#[test]
fn an_empty_component_update_does_not_spend_the_instruction_budget() {
    let main = r#"
        regia.component("idle", {
            props = { n = { type = "number", default = 0 } },
            on_update = function(e, dt) end,
        })
    "#;
    let mut mods = LuaMods::new(
        1,
        LuaLimits {
            instruction_budget: 2_000,
            ..LuaLimits::default()
        },
    );
    let mut world = TestWorld::default();
    mods.load(mod_files("idle", "1.0.0", main), &mut world)
        .expect("the mod loads");
    assert!(
        mods.get("idle").expect("loaded").is_active(),
        "load stays inside the budget"
    );
    for _ in 0..40 {
        mods.call_update(&mut world, 0.016);
    }
    assert!(
        mods.get("idle").expect("loaded").is_active(),
        "an empty on_update must not accumulate scan charges: {:?}",
        mods.get("idle").expect("loaded").status()
    );
}

/// Spec 84 § 14: "An empty pass does not touch the instruction budget." A declaration with an
/// `on_update` that no entity carries makes no call at all: the mod's call counter and its last
/// call's instruction count stay as the global hooks left them, and a mod whose whole budget is
/// below the pass's base charge (64) keeps running (ADR-5816: the integ-v047 merge had put the
/// charge back on empty passes).
#[test]
fn an_empty_component_pass_makes_no_call_and_charges_nothing() {
    let main = r#"
        regia.component("idle", {
            props = { n = { type = "number", default = 0 } },
            on_update = function(e, dt) end,
        })
    "#;
    let mut mods = host();
    let mut world = TestWorld::default();
    mods.load(mod_files("idle", "1.0.0", main), &mut world)
        .expect("the mod loads");
    let before = mods.get("idle").expect("loaded").counters();
    for _ in 0..3 {
        mods.call_update(&mut world, 0.016);
    }
    let after = mods.get("idle").expect("loaded").counters();
    assert_eq!(after.calls, before.calls, "an empty component pass is not a call");
    assert_eq!(after.last_instructions, before.last_instructions);

    // A host budget below the base charge: the empty pass must not disable the mod.
    let mut tight = LuaMods::new(
        1,
        LuaLimits {
            instruction_budget: 60,
            hook_interval: 1,
            ..LuaLimits::default()
        },
    );
    let mut world = TestWorld::default();
    tight
        .load(mod_files("idle", "1.0.0", main), &mut world)
        .expect("the declaration fits a 60-instruction budget");
    tight.call_update(&mut world, 0.016);
    let module = tight.get("idle").expect("loaded");
    assert!(
        module.is_active(),
        "an empty pass under a small budget keeps the mod: {:?}",
        module.status()
    );
}

/// `set_prop` writes the named field. The other fields, including a large string, stay unread.
#[test]
fn set_prop_writes_one_field_not_the_whole_map() {
    let main = r#"
        regia.component("bag", {
            props = {
                n = { type = "number", default = 0 },
                blob = { type = "string", default = "" },
            },
        })
        function on_start()
            local e = regia.spawn("item")
            e:add("bag")
            e:set_prop("bag", "blob", string.rep("x", 4000))
        end
    "#;
    let mut mods = host();
    let mut world = TestWorld::default();
    mods.load(mod_files("bag", "1.0.0", main), &mut world)
        .expect("the mod loads");
    world.field_writes = 0;
    let outcome = mods
        .run_chunk(
            &mut world,
            Some("bag"),
            r#"return regia.find("item"):set_prop("bag", "n", 3)"#,
        )
        .expect("the chunk runs");
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(outcome.results, vec![ScriptValue::Bool(true)]);
    assert_eq!(
        world.field_writes, 1,
        "one field write, not every property of the component"
    );
    let entity = world.find("item").expect("item");
    let values: std::collections::BTreeMap<String, ScriptValue> = world
        .component_props(entity, "bag")
        .expect("attached")
        .into_iter()
        .collect();
    assert_eq!(values["n"], ScriptValue::Number(3.0));
    assert_eq!(
        values["blob"],
        ScriptValue::String("x".repeat(4000)),
        "the untouched string is still there"
    );
}

#[test]
fn a_hot_reload_keeps_edited_values_and_reconciles_the_declaration() {
    // `on_load` keeps the mod's entities across the reload (spec 84 § 12).
    let keeping = format!("{MOVER}\nfunction on_load(info) end");
    let mut mods = host();
    let mut world = TestWorld::default();
    mods.load(mod_files("props", "1.0.0", &keeping), &mut world)
        .expect("the mod loads");
    let entity = world.find("walker").expect("the spawned walker");
    mods.run_chunk(
        &mut world,
        Some("props"),
        r#"local e = regia.find("walker")
           e:set_prop("mover", "speed", 7.0)
           e:set_prop("mover", "label", "edited")"#,
    )
    .expect("the chunk runs");

    // The reload changes `speed`'s default, drops `label`, adds `fresh` and keeps the rest.
    let mut updated = keeping.clone();
    updated = updated.replace("default = 2.0", "default = 4.0");
    updated = updated.replace(
        "            label  = { type = \"string\", default = \"mover\" },\n",
        "            fresh  = { type = \"bool\", default = true },\n",
    );
    let outcome = mods
        .reload("props", Some(mod_files("props", "1.1.0", &updated)), &mut world)
        .expect("the reload runs");
    assert!(outcome.ok, "{outcome:?}");

    let values: std::collections::BTreeMap<String, ScriptValue> = world
        .component_props(entity, "mover")
        .expect("still attached")
        .into_iter()
        .collect();
    assert_eq!(
        values["speed"],
        ScriptValue::Number(7.0),
        "the edited value survives"
    );
    assert!(!values.contains_key("label"), "the removed prop goes");
    assert_eq!(
        values["fresh"],
        ScriptValue::Bool(true),
        "a new prop gets its default"
    );
    assert_eq!(values["active"], ScriptValue::Bool(true), "untouched props stay");
}

#[test]
fn a_failed_reload_rolls_back_component_writes() {
    // `on_load` keeps the mod's entities across a reload; the trial's removal must roll back.
    let keeping = format!("{MOVER}\nfunction on_load(info) end");
    let mut mods = host();
    let mut world = TestWorld::default();
    mods.load(mod_files("props", "1.0.0", &keeping), &mut world)
        .expect("the mod loads");
    let entity = world.find("walker").expect("the spawned walker");

    // The new version removes a component and then fails in its entry chunk.
    let broken = format!("{keeping}\nregia.find(\"walker\"):remove(\"mover\")\nerror('boom')");
    let outcome = mods
        .reload("props", Some(mod_files("props", "1.1.0", &broken)), &mut world)
        .expect("the reload runs");
    assert!(!outcome.ok);
    // The trial's removal is rolled back; the old version's component stays as it was.
    let values: std::collections::BTreeMap<String, ScriptValue> = world
        .component_props(entity, "mover")
        .expect("the trial's removal was undone")
        .into_iter()
        .collect();
    assert_eq!(values["speed"], ScriptValue::Number(2.0));
    assert!(mods.get("props").expect("loaded").is_active());
}

/// Audit wave (script_lua correctness): the component pass begins its own budget. A global
/// `on_update` that returns with the instruction budget exactly spent must not disable the mod
/// through the next pass's world-scan pre-charge, and the scan must not be thrown away either.
#[test]
fn a_component_pass_is_a_fresh_budgeted_call_not_charged_against_the_previous_call() {
    const BUDGET: u64 = 100_000;
    let limits = LuaLimits {
        instruction_budget: BUDGET,
        hook_interval: 1,
        ..LuaLimits::default()
    };
    // The same burn in the probe and the real mod: with hook_interval 1 the instruction count
    // is exact and affine in the loop bound (constant overhead c plus n * p), so two probes
    // solve for the bound n that leaves the global call's `used` inside the window where the
    // buggy pre-charge (64 + component_cost / 8 = 64 here) overflows the budget.
    let probe_main = "function on_update(dt) local s = 0 for i = 1, burn_n do s = s + i end end";
    let mut probe = LuaMods::new(2, limits);
    probe
        .load(mod_files("cal", "1.0.0", probe_main), &mut TestWorld::default())
        .expect("loads");
    let mut cal_world = TestWorld::default();
    let mut used_for = |probe: &mut LuaMods, n: u32| {
        probe
            .run_chunk(&mut TestWorld::default(), Some("cal"), &format!("burn_n = {n}"))
            .expect("sets the bound");
        probe.call_update(&mut cal_world, 0.1);
        probe
            .get("cal")
            .map(|m| m.counters().last_instructions)
            .unwrap_or_default()
    };
    // used(n) = c + n * p, measured exactly with two bounds.
    let (c, u1) = (used_for(&mut probe, 0), used_for(&mut probe, 4_000));
    let p = (u1 - c) / 4_000;
    assert!(
        p > 0 && p <= 65,
        "per-iteration cost {p} must fit the 65-wide window"
    );
    let n = (BUDGET - c) / p;
    let used = c + n * p;
    assert!(
        used <= BUDGET && used + 65 > BUDGET,
        "used={used} must sit in the window (BUDGET - 65, BUDGET]"
    );
    // The affine model holds for the real bound too (it is the same code path).
    assert_eq!(used_for(&mut probe, n as u32), used, "used(n) is exact");

    // The real mod: a trivial component declaration with an on_update hook plus the budget-
    // filling global. No entity carries the component; the 64-instruction scan charge alone
    // must not overflow what the global call left.
    let main = format!(
        r#"regia.component("mover", {{ props = {{ speed = {{ type = "number", default = 1 }} }},
            on_update = function(e, dt) hooks = (hooks or 0) + 1 end }})
           function on_update(dt) local s = 0 for i = 1, {n} do s = s + i end end"#
    );
    let mut mods = LuaMods::new(1, limits);
    let mut world = TestWorld::default();
    mods.load(mod_files("m", "1.0.0", &main), &mut world)
        .expect("loads");
    mods.call_update(&mut world, 0.1);
    let module = mods.get("m").expect("loaded");
    assert!(
        module.is_active(),
        "the mod survives its update: {:?}",
        module.errors().last()
    );
}

#[test]
fn two_mods_cannot_declare_the_same_component() {
    let mut mods = host();
    let mut world = TestWorld::default();
    mods.load(mod_files("one", "1.0.0", MOVER), &mut world)
        .expect("the first mod loads");
    let err = mods
        .load(mod_files("two", "1.0.0", MOVER), &mut world)
        .expect_err("the second mod is refused");
    assert_eq!(err.code.code(), "lua_bad_value");
    assert!(err.message.contains("already declared"), "{err}");
    // The refused mod stays loaded and disabled, and its declaration is not registered.
    let two = mods.get("two").expect("kept, disabled");
    assert!(!two.is_active());
    match two.status() {
        ModStatus::Disabled { code, .. } => assert_eq!(code.code(), "lua_bad_value"),
        other => panic!("expected disabled, got {other:?}"),
    }
    let names: Vec<String> = mods
        .iter()
        .flat_map(|module| module.components().into_iter().map(|decl| decl.name))
        .collect();
    assert_eq!(names, vec!["mover".to_owned()]);
    let walkers = world.entities.values().filter(|e| e.name == "walker").count();
    assert_eq!(walkers, 1, "the refused load's spawn is rolled back");
    let again = mods
        .restart("two", &mut world)
        .expect_err("restart checks the same name");
    assert_eq!(again.code.code(), "lua_bad_value");
    let two = mods.get("two").expect("still loaded");
    assert!(!two.is_active());
    match two.status() {
        ModStatus::Disabled { code, .. } => assert_eq!(code.code(), "lua_bad_value"),
        other => panic!("restart left {other:?}"),
    }
    let names: Vec<String> = mods
        .iter()
        .flat_map(|module| module.components().into_iter().map(|decl| decl.name))
        .collect();
    assert_eq!(names, vec!["mover".to_owned()]);
}

#[test]
fn component_on_update_scan_is_charged_inside_its_own_call() {
    // A declaration needs at least one property (`bad_declarations_are_refused`, ADR-5517): each
    // `tick` below carries one unused `n` so the hook, not the declaration, is under test.
    let main = r#"
        regia.component("tick", {
            props = { n = { type = "number", default = 0 } },
            on_update = function(e, dt) end,
        })
    "#;
    let mut mods = host();
    let mut world = TestWorld::default();
    mods.load(mod_files("tick", "1.0.0", main), &mut world)
        .expect("loads");
    for _ in 0..20_000 {
        mods.call_update(&mut world, 0.016);
    }
    assert!(
        mods.get("tick").expect("loaded").is_active(),
        "an empty carrier set must not accumulate scan charges across frames"
    );

    // Measure the scan, then give a hook a budget the scan fills exactly so the next
    // frame used to trip on the leftover counter.
    let probe = r#"
        regia.component("tick", {
            props = { n = { type = "number", default = 0 } },
            on_update = function(e, dt) end,
        })
        function on_start()
            regia.spawn("e", regia.vec3(0, 0, 0)):add("tick")
        end
    "#;
    let mut probe_mods = host();
    let mut probe_world = TestWorld::default();
    probe_mods
        .load(mod_files("tick", "1.0.0", probe), &mut probe_world)
        .expect("probe loads");
    probe_mods.call_update(&mut probe_world, 0.016);
    let scan = probe_mods
        .get("tick")
        .expect("probe")
        .counters()
        .last_instructions;
    assert!(scan > 0, "the scan is charged inside the call");
    let budget = scan + 10 * 1000;
    let limits = LuaLimits {
        instruction_budget: budget,
        ..LuaLimits::default()
    };
    let mut filled = None;
    for steps in (1_500..3_500).step_by(25) {
        let body = format!(
            r#"
                ticks = 0
                regia.component("tick", {{
                    props = {{ n = {{ type = "number", default = 0 }} }},
                    on_update = function(e, dt)
                        local i = 0
                        while i < {steps} do i = i + 1 end
                        ticks = ticks + 1
                    end,
                }})
                function on_start()
                    regia.spawn("e", regia.vec3(0, 0, 0)):add("tick")
                end
            "#
        );
        let mut mods = LuaMods::new(1, limits);
        let mut world = TestWorld::default();
        if mods.load(mod_files("tick", "1.0.0", &body), &mut world).is_err() {
            continue;
        }
        mods.call_update(&mut world, 0.016);
        let module = mods.get("tick").expect("loaded");
        if !module.is_active() {
            continue;
        }
        let used = module.counters().last_instructions;
        if used + scan > budget {
            filled = Some((steps, mods, world));
            break;
        }
    }
    let (steps, mut mods, mut world) = filled.expect("a loop length that fills the instruction budget");
    assert!(
        mods.get("tick").expect("loaded").is_active(),
        "steps {steps} stayed inside the budget"
    );
    mods.call_update(&mut world, 0.016);
    let module = mods.get("tick").expect("loaded");
    assert!(
        module.is_active(),
        "the next frame still runs a hook that used the whole budget (steps {steps})"
    );
    let outcome = mods
        .run_chunk(&mut world, Some("tick"), "return ticks")
        .expect("reads ticks");
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(outcome.results, vec![ScriptValue::Number(2.0)]);
}

#[test]
fn a_declaration_error_disables_the_mod() {
    let mut mods = host();
    let mut world = TestWorld::default();
    let err = mods
        .load(
            mod_files(
                "bad",
                "1.0.0",
                r#"regia.component("broken", { props = { speed = { type = "fast" } } })"#,
            ),
            &mut world,
        )
        .expect_err("the declaration is refused");
    assert_eq!(err.code.code(), "lua_bad_value");
    assert!(!mods.get("bad").expect("kept, disabled").is_active());
}

/// The brief's end-to-end check: edited props survive a scene round trip, then a hot reload.
#[test]
fn agent_methods_read_and_merge_component_props() {
    use regia_agent::extension::AgentExtension;
    use regia_app::prelude::{App, Entity, Startup};
    use serde_json::{Value, json};

    let main = r#"
        regia.component("mover", {
            props = {
                speed  = { type = "number", default = 2.0, min = 0.0, max = 20.0 },
                mode   = { type = "enum", options = { "walk", "run" } },
                offset = { type = "vec3", default = regia.vec3(0, 1, 0) },
                target = { type = "entity" },
                tint   = { type = "color", default = { 1.0, 0.5, 0.25, 1.0 } },
            },
        })
        function on_start()
            local e = regia.spawn("walker", regia.vec3(0, 0, 0))
            e:add("mover")
        end
    "#;
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.add_plugins(regia_script_lua::LuaModsPlugin::new(vec![mod_files(
        "props", "1.0.0", main,
    )]));
    app.world_mut().run_schedule(Startup);

    let call = |app: &mut App, method: &str, params: Value| -> Value {
        let extension = app.world().resource::<AgentExtension>().clone();
        let (_, handler) = extension.method(method).expect("registered");
        handler(app.world_mut(), &params).expect("answers")
    };

    // The schema of the declared component.
    let listed = call(&mut app, "lua.components", json!({}));
    assert_eq!(listed["plugin"], json!(true));
    assert_eq!(listed["components"][0]["name"], json!("mover"));
    // A Lua table keeps no declaration order: the schema lists the properties sorted by name
    // (ADR-5517), so a property is found by name, never by its position in the source.
    let schema = listed["components"][0]["props"].as_array().expect("a props list");
    let names: Vec<&str> = schema.iter().filter_map(|p| p["name"].as_str()).collect();
    assert_eq!(names, ["mode", "offset", "speed", "target", "tint"]);
    let speed = schema
        .iter()
        .find(|p| p["name"] == json!("speed"))
        .expect("speed in the schema");
    assert_eq!(speed["type"], json!("number"));
    assert_eq!(speed["max"], json!(20.0));
    let offset = schema
        .iter()
        .find(|p| p["name"] == json!("offset"))
        .expect("offset in the schema");
    assert_eq!(offset["type"], json!("vec3"));

    let entity = {
        let mut query = app
            .world_mut()
            .query::<(Entity, &regia_scene::components::Name)>();
        query
            .iter(app.world_mut())
            .find(|(_, name)| name.as_str() == "walker")
            .map(|(entity, _)| entity.to_bits())
            .expect("the walker")
    };

    // Defaults are visible before any edit.
    let got = call(
        &mut app,
        "lua.props.get",
        json!({ "entity": entity, "component": "mover" }),
    );
    assert_eq!(got["props"]["speed"], json!(2.0));
    assert_eq!(got["props"]["mode"], json!("walk"));
    assert_eq!(got["defaults"]["speed"], json!(2.0));

    // A merge: one property changes, the other stays; the edit clamps to the range.
    let set = call(
        &mut app,
        "lua.props.set",
        json!({ "entity": entity, "component": "mover", "props": { "speed": 99.0 } }),
    );
    assert_eq!(set["props"]["speed"], json!(20.0), "the range clamps the edit");
    assert_eq!(set["props"]["mode"], json!("walk"), "untouched props stay");

    // The shapes `lua.props.get` emits for a vec3 and an entity are accepted by set.
    let shaped = call(
        &mut app,
        "lua.props.set",
        json!({
            "entity": entity,
            "component": "mover",
            "props": {
                "offset": { "vec3": [0.0, 2.0, 0.0] },
                "target": { "entity": entity },
            },
        }),
    );
    assert_eq!(shaped["props"]["offset"], json!({ "vec3": [0.0, 2.0, 0.0] }));
    assert_eq!(shaped["props"]["target"], json!({ "entity": entity }));
    assert_eq!(
        shaped["props"]["tint"],
        json!([1.0, 0.5, 0.25, 1.0]),
        "an untouched colour keeps its declaration default"
    );
    let got_back = call(
        &mut app,
        "lua.props.get",
        json!({ "entity": entity, "component": "mover" }),
    );
    let round = call(
        &mut app,
        "lua.props.set",
        json!({
            "entity": entity,
            "component": "mover",
            "props": got_back["props"],
        }),
    );
    assert_eq!(round["props"]["offset"], got_back["props"]["offset"]);
    assert_eq!(round["props"]["target"], got_back["props"]["target"]);
    assert_eq!(round["props"]["tint"], got_back["props"]["tint"]);

    let list = call(&mut app, "lua.props.list", json!({ "entity": entity }));
    assert_eq!(list["components"][0]["component"], json!("mover"));
    assert_eq!(list["components"][0]["props"]["speed"], json!(20.0));

    // Unknown properties and unknown components are refused with F0002-style errors.
    let extension = app.world().resource::<AgentExtension>().clone();
    let (_, handler) = extension.method("lua.props.set").expect("registered");
    let bad_prop = handler(
        app.world_mut(),
        &json!({ "entity": entity, "component": "mover", "props": { "nope": 1 } }),
    )
    .expect_err("an unknown property is refused");
    assert_eq!(bad_prop.data.as_ref().expect("data")["code"], json!("F0002"));
    let bad_decl = handler(
        app.world_mut(),
        &json!({ "entity": entity, "component": "elsewhere", "props": {} }),
    )
    .expect_err("an unknown component is refused");
    assert_eq!(bad_decl.data.as_ref().expect("data")["code"], json!("F0003"));
}

/// The brief's end-to-end check: edited props survive a scene round trip, then a hot reload.
#[test]
fn scene_round_trip_keeps_edited_props_then_reload_reconciles() {
    use regia_app::prelude::{App, Entity};
    use regia_core::id::Guid;
    use regia_scene::ScenePlugin;
    use regia_scene::components::EntityGuid;
    use regia_scene::format::SceneFile;
    use regia_scene::prelude::Name;
    use regia_scene::spawn::SnapshotFilter;

    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    // The test drives LuaMods by hand (no plugin), so register the component types the scene
    // codec needs here, as `LuaModsPlugin` would.
    app.register_type::<LuaComponent>()
        .register_type::<regia_script_lua::LuaValue>();
    let mut mods = host();
    let mut lua_world = TestWorld::default();
    mods.load(mod_files("props", "1.0.0", MOVER), &mut lua_world)
        .expect("the mod loads");
    let decl = mods
        .get("props")
        .expect("loaded")
        .components()
        .into_iter()
        .find(|d| d.name == "mover")
        .expect("the declaration");

    // A designer places an entity and attaches the component with edited values.
    let mut component = LuaComponent::from_decl(&decl);
    component
        .props
        .insert("speed".to_owned(), regia_script_lua::LuaValue::Number(12.5));
    component.props.insert(
        "label".to_owned(),
        regia_script_lua::LuaValue::String("boss".into()),
    );
    let entity = app
        .world_mut()
        .spawn((
            Name::new("Walker"),
            regia_scene::Transform::default(),
            EntityGuid(Guid::new_random()),
            component,
        ))
        .id();

    // Scene round trip: snapshot → RON → spawn into a fresh world.
    let file =
        regia_scene::spawn::snapshot_scene(app.world_mut(), &SnapshotFilter::default()).expect("snapshot");
    let text = file.to_ron_string().expect("ron");
    println!("SCENE RON:\n{text}");
    assert!(text.contains("mover"), "the component is in the scene: {text}");
    assert!(text.contains("boss"), "the edited value is in the scene");

    let mut loaded = App::new();
    loaded.add_plugins(ScenePlugin);
    loaded
        .register_type::<LuaComponent>()
        .register_type::<regia_script_lua::LuaValue>();
    let file = SceneFile::from_ron_str(&text).expect("parse");
    let spawned = regia_scene::spawn::spawn_scene(
        loaded.world_mut(),
        &file,
        &regia_scene::spawn::SpawnOptions::default(),
    )
    .expect("spawn");
    assert_eq!(spawned.warnings, Vec::<String>::new());

    let mut query = loaded.world_mut().query::<(Entity, &LuaComponent)>();
    let found: Vec<(u64, LuaComponent)> = query
        .iter(loaded.world_mut())
        .map(|(e, c)| (e.to_bits(), c.clone()))
        .collect();
    assert_eq!(found.len(), 1);
    let (_, round_tripped) = &found[0];
    assert_eq!(round_tripped.decl, "mover");
    let values: std::collections::BTreeMap<String, ScriptValue> =
        round_tripped.values().into_iter().collect();
    assert_eq!(
        values["speed"],
        ScriptValue::Number(12.5),
        "the edit survives the round trip"
    );
    assert_eq!(values["label"], ScriptValue::String("boss".into()));
    assert_eq!(
        values["mode"],
        ScriptValue::String("walk".into()),
        "defaults fill the rest"
    );
    let _ = entity;

    // Then a hot reload against the live world: the declaration changes underneath the scene.
    let mut ecs_world = regia_script_lua::EcsModWorld::new(loaded.world_mut());
    let mut updated = MOVER.to_owned();
    updated = updated.replace("default = 2.0", "default = 4.0");
    updated = updated.replace(
        "            label  = { type = \"string\", default = \"mover\" },\n",
        "            fresh  = { type = \"bool\", default = true },\n",
    );
    let outcome = mods
        .reload(
            "props",
            Some(mod_files("props", "1.1.0", &updated)),
            &mut ecs_world,
        )
        .expect("the reload runs");
    assert!(outcome.ok, "{outcome:?}");

    let reloaded = loaded
        .world()
        .get::<LuaComponent>(Entity::try_from_bits(found[0].0).expect("the entity"))
        .expect("the component stays attached");
    let values: std::collections::BTreeMap<String, ScriptValue> = reloaded.values().into_iter().collect();
    assert_eq!(
        values["speed"],
        ScriptValue::Number(12.5),
        "the edited value survives the reload"
    );
    assert_eq!(values["fresh"], ScriptValue::Bool(true), "the new prop defaults");
    assert!(!values.contains_key("label"), "the removed prop goes");
}

/// Spec 84 § 14: a mod declares at most 64 components. The 64th loads; a 65th fails the load
/// with `lua_bad_value` and leaves the mod disabled.
#[test]
fn a_mod_declares_at_most_64_components() {
    let declare = |n: usize| {
        (0..n)
            .map(|i| format!("regia.component(\"c{i}\", {{ props = {{ n = {{ type = \"number\" }} }} }})"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let mut mods = host();
    let mut world = TestWorld::default();
    mods.load(mod_files("many", "1.0.0", &declare(64)), &mut world)
        .expect("64 components load");
    assert_eq!(mods.get("many").expect("loaded").components().len(), 64);

    let mut mods = host();
    let err = mods
        .load(mod_files("toomany", "1.0.0", &declare(65)), &mut world)
        .expect_err("a 65th component is refused");
    assert_eq!(err.code.code(), "lua_bad_value");
    assert!(err.message.contains("at most 64"), "{err}");
    match mods.get("toomany").expect("kept, disabled").status() {
        ModStatus::Disabled { code, .. } => assert_eq!(code.code(), "lua_bad_value"),
        other => panic!("expected disabled, got {other:?}"),
    }
}
