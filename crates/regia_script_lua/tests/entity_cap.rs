// SPDX-License-Identifier: MPL-2.0
//! The per-mod entity cap (ADR-2418): `LuaLimits::max_entities_per_mod` and
//! `LuaMods::set_entity_cap`. A spawn past the cap is refused with `lua_entity_cap`; the cap holds
//! per mod, across restarts and reloads, and a despawn makes room again. No GPU, no clocks.

mod common;

use common::{TestWorld, mod_files};
use regia_core::math::Vec3;
use regia_script_lua::{
    DEFAULT_MAX_ENTITIES_PER_MOD, LuaErrorCode, LuaLimits, LuaMods, ModStatus, ScriptValue,
};

fn run(mods: &mut LuaMods, world: &mut TestWorld, id: &str, chunk: &str) -> Vec<ScriptValue> {
    let outcome = mods.run_chunk(world, Some(id), chunk).expect("the mod exists");
    assert!(outcome.ok, "{chunk}: {:?}", outcome.error);
    outcome.results
}

/// Spawns `n` entities under `pcall`; returns how many were refused and the last error text.
const SPAWNER: &str = "
function spawn_many(n)
  local refused, why = 0, nil
  for i = 1, n do
    local ok, err = pcall(regia.spawn, 'unit')
    if not ok then refused = refused + 1; why = tostring(err) end
  end
  return refused, why
end
";

/// A spawn under the cap, with nothing excluded, uses the owned count and does not list ids.
#[test]
fn a_spawn_under_the_cap_does_not_list_every_owned_entity() {
    let mut world = TestWorld::default();
    let mut mods = host(8);
    mods.load(
        mod_files("a", "1", "function on_start() regia.spawn('seed') end"),
        &mut world,
    )
    .expect("loads");
    world.owned_calls = 0;
    let results = run(&mut mods, &mut world, "a", "return regia.spawn('unit')");
    assert!(
        matches!(results.first(), Some(ScriptValue::Entity(_))),
        "the spawn returned a handle: {results:?}"
    );
    assert!(world.names().contains(&"unit"));
    assert_eq!(
        world.owned_calls, 0,
        "a spawn under an empty exclusion list uses the count, not the id list"
    );
}

fn host(cap: usize) -> LuaMods {
    LuaMods::new(
        5,
        LuaLimits {
            max_entities_per_mod: cap,
            ..LuaLimits::default()
        },
    )
}

#[test]
fn a_spawn_past_the_cap_is_refused_with_its_code() {
    assert_eq!(
        LuaLimits::default().max_entities_per_mod,
        DEFAULT_MAX_ENTITIES_PER_MOD
    );
    assert_eq!(
        common::host().limits().max_entities_per_mod,
        DEFAULT_MAX_ENTITIES_PER_MOD
    );
    let mut world = TestWorld::default();
    let mut mods = host(3);
    mods.load(mod_files("a", "1", SPAWNER), &mut world)
        .expect("loads");
    let results = run(&mut mods, &mut world, "a", "return spawn_many(5)");
    assert_eq!(
        results.first(),
        Some(&ScriptValue::Number(2.0)),
        "two of five refused"
    );
    let Some(ScriptValue::String(why)) = results.get(1) else {
        panic!("no error text: {results:?}");
    };
    assert!(
        why.contains("lua_entity_cap") || why.contains("entity cap"),
        "{why}"
    );
    assert_eq!(world.names(), ["unit", "unit", "unit"]);
    assert_eq!(
        world.position_of("unit"),
        Some(Vec3::ZERO),
        "spawned at the origin"
    );
    let module = mods.get("a").expect("loaded");
    assert_eq!(module.entity_cap(), 3);
    assert_eq!(module.entity_cap_refused(), 2);
    assert!(module.is_active(), "a caught refusal leaves the mod running");

    // An uncaught refusal fails the call with the code and disables the mod like any error.
    let mut world = TestWorld::default();
    let mut mods = host(1);
    let err = mods
        .load(
            mod_files(
                "b",
                "1",
                "function on_start() regia.spawn('x') regia.spawn('y') end",
            ),
            &mut world,
        )
        .expect_err("the second spawn is refused");
    assert_eq!(err.code, LuaErrorCode::EntityCap, "{err:?}");
    assert_eq!(err.code.code(), "lua_entity_cap");
    assert!(matches!(
        mods.get("b").map(|m| m.status().clone()),
        Some(ModStatus::Disabled {
            code: LuaErrorCode::EntityCap,
            ..
        })
    ));
}

#[test]
fn the_cap_counts_what_a_mod_owns_now_per_mod() {
    let mut world = TestWorld::default();
    let mut mods = host(2);
    mods.load(mod_files("a", "1", SPAWNER), &mut world).expect("a");
    mods.load(mod_files("b", "1", SPAWNER), &mut world).expect("b");
    assert_eq!(
        run(&mut mods, &mut world, "a", "return spawn_many(2)")[0],
        ScriptValue::Number(0.0)
    );
    // Another mod's entities do not count against this one.
    assert_eq!(
        run(&mut mods, &mut world, "b", "return spawn_many(2)")[0],
        ScriptValue::Number(0.0)
    );
    assert_eq!(
        run(&mut mods, &mut world, "a", "return spawn_many(1)")[0],
        ScriptValue::Number(1.0)
    );
    // A despawn makes room.
    run(
        &mut mods,
        &mut world,
        "a",
        "local e = regia.find('unit'); assert(e:despawn()); return true",
    );
    assert_eq!(
        run(&mut mods, &mut world, "a", "return spawn_many(1)")[0],
        ScriptValue::Number(0.0)
    );
    assert_eq!(
        run(&mut mods, &mut world, "a", "return spawn_many(1)")[0],
        ScriptValue::Number(1.0)
    );
}

#[test]
fn a_mods_own_cap_is_lower_only_and_survives_restart_and_reload() {
    let mut world = TestWorld::default();
    let mut mods = host(10);
    mods.load(mod_files("a", "1", SPAWNER), &mut world).expect("a");
    assert_eq!(mods.set_entity_cap("a", Some(1)), Ok(1));
    // At most the host's cap.
    assert_eq!(mods.set_entity_cap("a", Some(50)), Ok(10));
    assert_eq!(mods.set_entity_cap("a", Some(1)), Ok(1));
    assert_eq!(
        mods.set_entity_cap("nope", Some(1)).map_err(|e| e.code),
        Err(LuaErrorCode::UnknownMod)
    );
    assert_eq!(
        run(&mut mods, &mut world, "a", "return spawn_many(3)")[0],
        ScriptValue::Number(2.0)
    );

    mods.restart("a", &mut world).expect("restart");
    let module = mods.get("a").expect("a");
    assert_eq!(module.entity_cap(), 1, "the cap survives a restart");
    assert_eq!(module.entity_cap_refused(), 2, "so does the refusal count");

    // A reload's new entry chunk already runs under the mod's cap.
    let outcome = mods
        .reload(
            "a",
            Some(mod_files(
                "a",
                "2",
                &format!("{SPAWNER}\nrefused_at_load = spawn_many(5)"),
            )),
            &mut world,
        )
        .expect("reload");
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(mods.get("a").map(|m| m.entity_cap()), Some(1));
    // Under the host's cap of 10 none of the five would be refused; under the mod's cap of 1 at
    // most one goes through.
    let Some(ScriptValue::Number(refused)) = run(&mut mods, &mut world, "a", "return refused_at_load")
        .first()
        .cloned()
    else {
        panic!("refused_at_load is not a number");
    };
    assert!(refused >= 4.0, "{refused}");
    // Back to the host's cap.
    assert_eq!(mods.set_entity_cap("a", None), Ok(10));
    assert_eq!(
        run(&mut mods, &mut world, "a", "return spawn_many(3)")[0],
        ScriptValue::Number(0.0)
    );
}

#[test]
fn a_cap_of_zero_refuses_every_spawn_and_older_limits_get_the_default() {
    let mut world = TestWorld::default();
    let mut mods = host(0);
    mods.load(mod_files("a", "1", SPAWNER), &mut world).expect("a");
    assert_eq!(
        run(&mut mods, &mut world, "a", "return spawn_many(4)")[0],
        ScriptValue::Number(4.0)
    );
    assert!(world.names().is_empty());
    // Limits serialized before the cap existed read back with the default cap.
    let mut value = serde_json::to_value(LuaLimits::default()).expect("serialize");
    value
        .as_object_mut()
        .expect("object")
        .remove("max_entities_per_mod");
    let limits: LuaLimits = serde_json::from_value(value).expect("older limits");
    assert_eq!(limits.max_entities_per_mod, DEFAULT_MAX_ENTITIES_PER_MOD);
}

/// Review finding (ADR-2418): a mod at its cap whose entry chunk spawns still reloads and loads a
/// save. The previous incarnation's entities go (or are taken over) before the mod starts, so they
/// do not count while the new one is built.
#[test]
fn a_mod_at_its_cap_still_reloads_and_loads_a_save() {
    let entry = "regia.spawn('u') regia.spawn('u')";
    let mut world = TestWorld::default();
    let mut mods = host(2);
    mods.load(mod_files("full", "1", entry), &mut world)
        .expect("loads at its cap");
    assert_eq!(world.names(), ["u", "u"]);

    // Reload without on_load: the old entities go, the new entry chunk's two stay.
    let outcome = mods
        .reload("full", Some(mod_files("full", "2", entry)), &mut world)
        .expect("reload");
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(world.names(), ["u", "u"]);

    // Reload with on_load: the old entities stay, what the entry chunk spawned goes.
    let with_load = format!("{entry}\nfunction on_load(info) end");
    let outcome = mods
        .reload("full", Some(mod_files("full", "3", &with_load)), &mut world)
        .expect("reload");
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(world.names(), ["u", "u"]);

    // A save and a load into the same world: the entry chunk runs before the old entities go.
    let (bytes, _) = mods.save_state(&mut world);
    let report = mods.restore_state(Some(&bytes), &mut world);
    assert!(
        mods.get("full").is_some_and(regia_script_lua::LuaMod::is_active),
        "{report:?}"
    );
    assert_eq!(world.names(), ["u", "u"]);
    // After all that the cap still holds for new spawns.
    let outcome = mods
        .run_chunk(&mut world, Some("full"), "return pcall(regia.spawn, 'x')")
        .expect("runs");
    assert_eq!(outcome.results.first(), Some(&ScriptValue::Bool(false)));
}
