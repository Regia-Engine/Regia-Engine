// SPDX-License-Identifier: MPL-2.0
//! Edge cases found by the lua-edge review (sprint 2026-10-21): limits below a floor, restarts
//! and failed loads leaving entities, `math.random` and `string.gsub` argument rules, `require`
//! of a module that returns `false`, and the reset note of a version bump. No GPU, no clocks.

mod common;

use common::{TestWorld, host, mod_files};
use regia_script_lua::{LuaLimits, LuaMods, ScriptValue, StateCarry};

fn eval(mods: &mut LuaMods, world: &mut TestWorld, id: &str, chunk: &str) -> Result<ScriptValue, String> {
    let outcome = mods.run_chunk(world, Some(id), chunk).expect("the mod exists");
    if outcome.ok {
        Ok(outcome.results.into_iter().next().unwrap_or_default())
    } else {
        Err(outcome.error.map(|e| e.message).unwrap_or_default())
    }
}

fn one(main: &str) -> (LuaMods, TestWorld) {
    let mut world = TestWorld::default();
    let mut mods = host();
    mods.load(mod_files("m", "1.0.0", main), &mut world)
        .expect("loads");
    (mods, world)
}

#[test]
fn for_mod_never_panics_when_the_host_cap_is_below_the_floor() {
    let limits = LuaLimits {
        instruction_budget: 100,
        memory_limit_bytes: 1000,
        hook_interval: 1000,
        ..LuaLimits::default()
    };
    let out = limits.for_mod(Some(50), Some(10));
    // A request below the host's instruction cap is kept (the hook interval is narrowed to match).
    assert_eq!(out.instruction_budget, 50);
    assert!(out.hook_interval <= 50);
    // A memory request below the 64 KiB floor leaves the host cap standing (no clamp panic).
    assert_eq!(out.memory_limit_bytes, 1000);
    // Within range the request still wins.
    let out = LuaLimits::default().for_mod(Some(5_000), Some(1 << 20));
    assert_eq!((out.instruction_budget, out.memory_limit_bytes), (5_000, 1 << 20));
}

#[test]
fn restart_removes_the_entities_of_the_run_it_replaces() {
    let (mut mods, mut world) = one(r#"function on_start() regia.spawn("lamp", regia.vec3(0, 0, 0)) end"#);
    assert_eq!(world.names(), ["lamp"]);
    mods.restart("m", &mut world).expect("restarts");
    mods.restart("m", &mut world).expect("restarts");
    assert_eq!(world.names(), ["lamp"]);
    assert!(world.position_of("lamp").is_some());
}

/// A restart whose `on_start` fails still removes the entities of the run it replaces. Before
/// the fix the despawns waited in a journal that was dropped on the error, so the old lamp stayed
/// next to the failed run's own lamp (correctness audit, lua-telemetry-matgraph).
#[test]
fn a_failing_restart_still_removes_the_entities_of_the_run_it_replaces() {
    let (mut mods, mut world) = one(r#"function on_start()
            regia.spawn("lamp", regia.vec3(0, 0, 0))
            if regia.find("stop") then error("boom") end
        end"#);
    let old_lamp = world
        .entities
        .iter()
        .find(|(_, e)| e.name == "lamp")
        .map(|(id, _)| *id)
        .expect("the first run's lamp");
    world.add("stop", regia_core::math::Vec3::ZERO);
    assert!(mods.restart("m", &mut world).is_err(), "on_start fails");
    assert!(!world.entities.contains_key(&old_lamp), "the old run's lamp went");
    assert_eq!(
        world.names(),
        ["lamp", "stop"],
        "only the failed run's own spawn stays, as after a failed load"
    );
}

/// A fresh load whose entry chunk despawns an entity the mod already owned and then fails keeps
/// that despawn, as it keeps the chunk's spawns and component writes. Before the fix the despawn
/// waited in a journal that was dropped on the error. The entity is left by another `LuaMods` on
/// the same world (corr-lua-telemetry-matgraph LOW list, ADR-5895).
#[test]
fn a_failing_fresh_load_keeps_the_despawn_its_entry_chunk_made() {
    let (_first, mut world) = one(r#"regia.spawn("lamp", regia.vec3(0, 0, 0))"#);
    assert_eq!(world.names(), ["lamp"]);
    let mut second = host();
    let loaded = second.load(
        mod_files(
            "m",
            "1.0.0",
            r#"regia.find("lamp"):despawn()
            regia.spawn("bulb", regia.vec3(0, 0, 0))
            error("boom")"#,
        ),
        &mut world,
    );
    assert!(loaded.is_err(), "the entry chunk fails");
    assert_eq!(
        world.names(),
        ["bulb"],
        "the despawn stays like the spawn the failed chunk made"
    );
}

#[test]
fn a_failing_on_load_of_a_kept_save_leaves_no_entities() {
    let (mut first, mut world) = one(
        r#"function on_start() regia.spawn("lamp", regia.vec3(0, 0, 0)) end
        function on_load() end"#,
    );
    let (bytes, _) = first.save_state(&mut world);
    let mut world = TestWorld::default();
    let mut second = host();
    second.restore_state(Some(&bytes), &mut world);
    let loaded = second.load(
        mod_files("m", "1.0.0", "function on_load() error('no') end"),
        &mut world,
    );
    assert!(loaded.is_err());
    assert!(world.names().is_empty(), "{:?}", world.names());
    assert!(second.kept_state().any(|id| id == "m"));
}

#[test]
fn math_random_rejects_nil_and_an_infinite_span() {
    for chunk in [
        "return math.random(nil)",
        "return math.random(nil, 10)",
        "return math.random(1, nil)",
        "return math.random(-1e308, 1e308)",
        "return math.random(5, 1)",
    ] {
        // A failing call disables the mod, so each chunk gets its own.
        let (mut mods, mut world) = one("");
        assert!(eval(&mut mods, &mut world, "m", chunk).is_err(), "{chunk}");
    }
    let (mut mods, mut world) = one("");
    let r = eval(&mut mods, &mut world, "m", "return math.random(3, 7)").expect("ok");
    assert!(matches!(r, ScriptValue::Number(n) if (3.0..=7.0).contains(&n) && n.fract() == 0.0));
    assert!(eval(&mut mods, &mut world, "m", "return math.random()").is_ok());
}

#[test]
fn gsub_count_is_a_32_bit_int_like_lua_5_1() {
    let (mut mods, mut world) = one("");
    // 2^32 + 5 narrows to 5 in a C `int`.
    let r = eval(
        &mut mods,
        &mut world,
        "m",
        "return (select(2, string.gsub(('a'):rep(10), 'a', 'b', 2^32 + 5)))",
    );
    assert_eq!(r, Ok(ScriptValue::Number(5.0)));
}

#[test]
fn require_runs_a_module_again_when_it_returned_false() {
    let mut world = TestWorld::default();
    let files = regia_script_lua::ModFiles::from_map([
        ("luamod.ron".to_owned(), common::manifest("m", "1.0.0")),
        ("main.lua".to_owned(), "runs = 0".to_owned()),
        ("lib.lua".to_owned(), "runs = runs + 1 return false".to_owned()),
    ]);
    let mut mods = host();
    mods.load(files, &mut world).expect("loads");
    let r = eval(
        &mut mods,
        &mut world,
        "m",
        "require('lib') require('lib') return runs",
    );
    assert_eq!(r, Ok(ScriptValue::Number(2.0)));
}

#[test]
fn a_reset_on_a_version_bump_names_the_old_version() {
    let mut world = TestWorld::default();
    let mut mods = host();
    mods.load(
        mod_files("m", "1.0.0", "function on_start() regia.state.bad = print end"),
        &mut world,
    )
    .expect("loads");
    let outcome = mods
        .reload(
            "m",
            Some(mod_files(
                "m",
                "2.0.0",
                "regia.state_version = 2 function on_load() end",
            )),
            &mut world,
        )
        .expect("the mod exists");
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(outcome.state, Some(StateCarry::Reset { from: 1, to: 2 }));
}
