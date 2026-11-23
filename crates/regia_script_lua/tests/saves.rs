// SPDX-License-Identifier: MPL-2.0
//! Mod state in save games (`docs/specs/84-lua-mods.md` § 11, ADR-1943): what a save holds, what a
//! load gives back, versions and migrators, caps, and one mod's failure staying that mod's.
//! No GPU, no goldens, no clocks.

mod common;

use common::{TestWorld, host, mod_files};
use regia_core::math::Vec3;
use regia_script_lua::{
    LUASTATE_FORMAT_VERSION, LUASTATE_MAGIC, LuaErrorCode, LuaLimits, LuaMods, ModFiles, ModStatus, ModWorld,
    NoWorld, ScriptValue, StateCarry, StateKey, StateValue, decode_luastate,
};

/// `value` as the mod's global `name` holds it.
fn global(mods: &mut LuaMods, id: &str, name: &str) -> ScriptValue {
    let outcome = mods
        .run_chunk(&mut NoWorld, Some(id), &format!("return {name}"))
        .expect("the mod exists");
    assert!(outcome.ok, "{name}: {:?}", outcome.error);
    outcome.results.into_iter().next().unwrap_or_default()
}

fn number(mods: &mut LuaMods, id: &str, name: &str) -> f64 {
    match global(mods, id, name) {
        ScriptValue::Number(n) => n,
        other => panic!("{name} is {other:?}"),
    }
}

const COUNTER: &str = r#"
started = 0
loaded = 0
function on_start() started = started + 1 regia.state.count = 0 end
function on_tick() regia.state.count = regia.state.count + 1 end
function on_load(info) loaded = loaded + 1 last = info end
"#;

#[test]
fn state_crosses_a_save_into_a_new_host_and_on_load_replaces_on_start() {
    let mut first = host();
    first
        .load(mod_files("a", "1.0.0", COUNTER), &mut NoWorld)
        .expect("loads");
    for _ in 0..5 {
        first.call_tick(&mut NoWorld, 0.1);
    }
    let (bytes, report) = first.save_state(&mut NoWorld);
    assert_eq!(&bytes[..4], LUASTATE_MAGIC);
    assert_eq!(report.mods.len(), 1);
    assert!(report.mods[0].bytes > 0 && report.mods[0].error.is_none());

    let mut second = host();
    second
        .load(mod_files("a", "1.0.0", COUNTER), &mut NoWorld)
        .expect("loads");
    assert_eq!(number(&mut second, "a", "regia.state.count"), 0.0);
    let restored = second.restore_state(Some(&bytes), &mut NoWorld);
    assert_eq!(restored.error, None);
    assert_eq!(restored.mods[0].state, Some(StateCarry::Carried));
    assert_eq!(number(&mut second, "a", "regia.state.count"), 5.0);
    assert_eq!(
        number(&mut second, "a", "started"),
        0.0,
        "on_start stays out of a restore"
    );
    assert_eq!(number(&mut second, "a", "loaded"), 1.0);
    let info = |mods: &mut LuaMods, field: &str| global(mods, "a", &format!("last.{field}"));
    assert_eq!(info(&mut second, "reason"), ScriptValue::String("save".into()));
    assert_eq!(info(&mut second, "state"), ScriptValue::String("carried".into()));
    assert_eq!(info(&mut second, "from_version"), ScriptValue::Nil);
    second.call_tick(&mut NoWorld, 0.1);
    assert_eq!(number(&mut second, "a", "regia.state.count"), 6.0);
}

#[test]
fn a_loaded_game_goes_on_exactly_like_the_run_it_was_saved_from() {
    const RANDOM: &str = r#"
    function on_start() regia.state.total = 0 regia.state.log = {} end
    function on_load() end
    function on_tick()
      local r = math.random(1000)
      regia.state.total = regia.state.total + r
      regia.state.log[#regia.state.log + 1] = r
      regia.emit("rolled", { r = r, total = regia.state.total, n = #regia.state.log })
    end
    "#;
    let run = |split: Option<u32>| {
        let mut mods = host();
        mods.load(mod_files("r", "1", RANDOM), &mut NoWorld)
            .expect("loads");
        for tick in 0..60 {
            if split == Some(tick) {
                let (bytes, _) = mods.save_state(&mut NoWorld);
                let mut other = host();
                other
                    .load(mod_files("r", "1", RANDOM), &mut NoWorld)
                    .expect("loads");
                // What the run emitted up to here carries over by hand: the load drops the queues.
                let mut before = mods.drain_emitted();
                other.restore_state(Some(&bytes), &mut NoWorld);
                other.drain_emitted();
                mods = other;
                before.extend(run_rest(&mut mods, tick));
                return before;
            }
            mods.call_tick(&mut NoWorld, 0.1);
        }
        mods.drain_emitted()
    };
    fn run_rest(mods: &mut LuaMods, from: u32) -> Vec<regia_script_lua::EmittedEvent> {
        for _ in from..60 {
            mods.call_tick(&mut NoWorld, 0.1);
        }
        mods.drain_emitted()
    }
    let straight = run(None);
    assert_eq!(straight.len(), 60);
    for split in [1, 17, 59] {
        assert_eq!(run(Some(split)), straight, "split at tick {split}");
    }
}

#[test]
fn the_state_keeps_every_plain_shape() {
    const SHAPES: &str = r#"
    regia.state.bytes = "\0\255binary"
    regia.state.vector = regia.vec3(1, 2, 3)
    regia.state.flag = false
    regia.state[10] = "number key"
    regia.state["10"] = "string key"
    regia.state[true] = "boolean key"
    local shared = { hits = 1 }
    regia.state.first, regia.state.second = shared, shared
    regia.state.loop = regia.state
    function on_load() end
    "#;
    let mut first = host();
    first
        .load(mod_files("s", "1", SHAPES), &mut NoWorld)
        .expect("loads");
    let (bytes, report) = first.save_state(&mut NoWorld);
    assert_eq!(report.mods[0].error, None);
    let mut second = host();
    second
        .load(mod_files("s", "1", "function on_load() end"), &mut NoWorld)
        .expect("loads");
    second.restore_state(Some(&bytes), &mut NoWorld);
    let check = r#"
      local s = regia.state
      s.first.hits = 2
      return s.bytes == "\0\255binary" and s.vector == regia.vec3(1, 2, 3) and s.flag == false
        and s[10] == "number key" and s["10"] == "string key" and s[true] == "boolean key"
        and s.second.hits == 2 and s.loop == s
    "#;
    let outcome = second.run_chunk(&mut NoWorld, Some("s"), check).expect("runs");
    assert_eq!(outcome.results, vec![ScriptValue::Bool(true)], "{outcome:?}");
}

#[test]
fn a_mod_replacing_or_dropping_the_state_table_is_read_through_the_host_reference() {
    let mut mods = host();
    mods.load(
        mod_files(
            "t",
            "1",
            "regia.state = { replaced = 1 } local r = regia regia = nil",
        ),
        &mut NoWorld,
    )
    .expect("loads");
    let (bytes, report) = mods.save_state(&mut NoWorld);
    assert_eq!(report.mods[0].error, None);
    let file = decode_luastate(&bytes, &LuaLimits::default()).expect("decodes");
    assert_eq!(file.mods[0].tree.tables[0].len(), 1);

    let mut mods = host();
    mods.load(mod_files("t", "1", "regia.state = nil"), &mut NoWorld)
        .expect("loads");
    let (bytes, report) = mods.save_state(&mut NoWorld);
    assert_eq!(report.mods[0].error, None);
    let file = decode_luastate(&bytes, &LuaLimits::default()).expect("decodes");
    assert!(file.mods[0].tree.tables[0].is_empty());

    let mut mods = host();
    mods.load(mod_files("t", "1", "regia.state = 5"), &mut NoWorld)
        .expect("loads");
    let (_, report) = mods.save_state(&mut NoWorld);
    assert_eq!(
        report.mods[0].error.as_ref().map(|e| e.code),
        Some(LuaErrorCode::BadValue)
    );
}

#[test]
fn entities_return_with_on_load_and_handles_stay_out_of_the_state() {
    const LAMPS: &str = r#"
    function on_start()
      regia.spawn("lamp", regia.vec3(1, 0, 0))
      regia.spawn("post", regia.vec3(2, 0, 0))
      regia.state.lamp_name = "lamp"
    end
    function on_tick()
      local e = regia.find(regia.state.lamp_name)
      if e then e:set_position(e:position() + regia.vec3(0, 1, 0)) end
    end
    function on_load() lamp = regia.find(regia.state.lamp_name) end
    "#;
    let mut world = TestWorld::default();
    world.add("game_entity", Vec3::ZERO);
    let mut first = host();
    first.load(mod_files("l", "1", LAMPS), &mut world).expect("loads");
    first.call_tick(&mut world, 0.1);
    let (bytes, report) = first.save_state(&mut world);
    assert_eq!(report.mods[0].error, None);

    // The same process goes on, then loads: what the mod did after the save is undone.
    first.call_tick(&mut world, 0.1);
    let extra = first
        .run_chunk(&mut world, Some("l"), "regia.spawn('later', regia.vec3(9, 9, 9))")
        .expect("runs");
    assert!(extra.ok);
    assert_eq!(world.names(), ["game_entity", "lamp", "later", "post"]);
    assert_eq!(world.position_of("lamp"), Some(Vec3::new(1.0, 2.0, 0.0)));
    first.restore_state(Some(&bytes), &mut world);
    assert_eq!(world.names(), ["game_entity", "lamp", "post"]);
    assert_eq!(world.position_of("lamp"), Some(Vec3::new(1.0, 1.0, 0.0)));

    let name = first
        .run_chunk(&mut world, Some("l"), "return lamp:name()")
        .expect("runs");
    assert_eq!(name.results, vec![ScriptValue::String("lamp".into())]);
}

/// Audit wave 5 (luasave): an owned entity whose position went non-finite is left out of the
/// save, so the save loads (the mod is not disabled) and the healthy entity comes back.
#[test]
fn an_entity_with_a_non_finite_position_is_left_out_of_the_save() {
    const TWO: &str = r#"
    function on_start()
      regia.spawn("lamp", regia.vec3(1, 0, 0))
      regia.spawn("ghost", regia.vec3(2, 0, 0))
    end
    function on_load() end
    "#;
    let mut world = TestWorld::default();
    let mut first = host();
    first.load(mod_files("n", "1", TWO), &mut world).expect("loads");
    let ghost = world
        .entities
        .values_mut()
        .find(|e| e.name == "ghost")
        .expect("ghost");
    ghost.position = Vec3::new(f32::NAN, 0.0, 0.0);
    let (bytes, report) = first.save_state(&mut world);
    assert_eq!(report.mods[0].error, None);
    let file = decode_luastate(&bytes, &LuaLimits::default()).expect("decodes");
    let names: Vec<&str> = file.mods[0].entities.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["lamp"]);
    let mut second = host();
    let mut fresh = TestWorld::default();
    second.load(mod_files("n", "1", TWO), &mut fresh).expect("loads");
    let restored = second.restore_state(Some(&bytes), &mut fresh);
    assert_eq!(restored.error, None);
    assert_eq!(restored.mods[0].error, None);
    assert_eq!(restored.mods[0].state, Some(StateCarry::Carried));
    assert!(matches!(
        second.get("n").map(regia_script_lua::LuaMod::status),
        Some(ModStatus::Active)
    ));
    assert_eq!(fresh.position_of("lamp"), Some(Vec3::new(1.0, 0.0, 0.0)));
    assert_eq!(fresh.position_of("ghost"), None);
}

/// Audit wave 6 (luasave H2): an entry with a NaN key (a crafted or damaged save: Lua cannot make
/// one) disabled the mod on load and was written back into every later save, so every later
/// load disabled it again. The restore still refuses it, but the next save holds the mod's own
/// state, and a load of that save runs the mod.
#[test]
fn a_corrupt_entry_is_not_written_into_the_next_save() {
    let mut first = host();
    first
        .load(mod_files("a", "1.0.0", COUNTER), &mut NoWorld)
        .expect("loads");
    first.call_tick(&mut NoWorld, 0.1);
    let (bytes, _) = first.save_state(&mut NoWorld);
    let mut file = decode_luastate(&bytes, &LuaLimits::default()).expect("decode");
    file.mods[0].tree.tables[0].push((StateKey::Number(f64::NAN), StateValue::Bool(true)));
    let mut bad = LUASTATE_MAGIC.to_vec();
    bad.extend_from_slice(&LUASTATE_FORMAT_VERSION.to_le_bytes());
    bad.extend_from_slice(&postcard::to_allocvec(&file).expect("encode"));

    let mut second = host();
    second
        .load(mod_files("a", "1.0.0", COUNTER), &mut NoWorld)
        .expect("loads");
    let restored = second.restore_state(Some(&bad), &mut NoWorld);
    let error = restored.mods[0].error.as_ref().expect("refused");
    assert!(error.message.contains("NaN"), "{error:?}");
    assert!(!restored.kept.contains(&"a".to_owned()));
    let (resaved, _) = second.save_state(&mut NoWorld);
    let again = decode_luastate(&resaved, &LuaLimits::default()).expect("decode");
    assert!(
        again.mods.iter().all(|m| !m.is_corrupt()),
        "the corrupt entry was written back"
    );

    let mut third = host();
    third
        .load(mod_files("a", "1.0.0", COUNTER), &mut NoWorld)
        .expect("loads");
    let report = third.restore_state(Some(&resaved), &mut NoWorld);
    assert!(report.mods[0].error.is_none(), "{report:?}");
}

/// Audit wave 6 (luasave M2): a mod that spawns at file scope and has `on_load` gained one entity
/// per load (the chunk's spawn survived beside the saved list) and one per hot reload (the new
/// chunk's spawn beside the kept entities). The saved list, and the kept entities, are the mod's
/// entities now.
#[test]
fn entry_chunk_spawns_do_not_pile_up_across_loads_and_reloads() {
    const FILE_SCOPE: &str = r#"
    regia.spawn("lamp", regia.vec3(0, 1, 0))
    function on_load() end
    "#;
    let lamps = |world: &TestWorld| world.names().iter().filter(|n| **n == "lamp").count();
    let mut world = TestWorld::default();
    let mut mods = host();
    mods.load(mod_files("m", "1", FILE_SCOPE), &mut world)
        .expect("loads");
    mods.call_tick(&mut world, 0.1);
    assert_eq!(lamps(&world), 1);
    let (bytes, _) = mods.save_state(&mut world);
    for load in 0..3 {
        let report = mods.restore_state(Some(&bytes), &mut world);
        assert!(report.mods[0].error.is_none(), "{report:?}");
        assert_eq!(lamps(&world), 1, "load {load}");
    }
    for reload in 0..3 {
        let outcome = mods
            .reload("m", Some(mod_files("m", "2", FILE_SCOPE)), &mut world)
            .expect("the mod exists");
        assert!(outcome.ok, "{outcome:?}");
        assert_eq!(lamps(&world), 1, "reload {reload}");
    }
}

/// Audit wave 5 (luasave H1): a restore that fails for a mod (here its saved entry is past the
/// restoring host's entity cap, so `check` refuses it) leaves none of that mod's entities in the
/// world: the ones it owned before the load go either way.
#[test]
fn a_failed_restore_leaves_none_of_the_mods_entities() {
    const LAMP: &str = r#"
    function on_start() regia.spawn("lamp", regia.vec3(1, 0, 0)) end
    function on_load() end
    "#;
    let mut world = TestWorld::default();
    world.add("game_entity", Vec3::ZERO);
    let mut first = host();
    first.load(mod_files("m", "1", LAMP), &mut world).expect("loads");
    first.call_tick(&mut world, 0.1);
    let (bytes, report) = first.save_state(&mut world);
    assert_eq!(report.mods[0].error, None);
    assert!(world.names().contains(&"lamp"));
    let strict = LuaLimits {
        max_state_entities: 0,
        ..LuaLimits::default()
    };
    // The restoring version spawns nothing itself: with a cap of 0, `regia.spawn` is refused
    // (audit v0.2.12), and this test is about the restore refusing the saved lamp.
    const LAMP_RESTORE: &str = "function on_start() end function on_load() end";
    let mut second = LuaMods::new(0, strict);
    second
        .load(mod_files("m", "1", LAMP_RESTORE), &mut world)
        .expect("loads");
    let restored = second.restore_state(Some(&bytes), &mut world);
    assert!(restored.mods[0].error.is_some(), "{restored:?}");
    assert!(
        world.entities.values().all(|e| e.owner.as_deref() != Some("m")),
        "{:?}",
        world.names()
    );
    assert_eq!(world.names(), ["game_entity"]);
}

/// Finding 1 (`sprint-deep-zai-script-lua`): every rebuild path but save-restore enforced the
/// script-component name-uniqueness invariant, so a save taken while a refused clash sat disabled
/// revived both declarations under one name on load. A restore now refuses the second
/// declaration like a load: a typed `lua_bad_value`, the mod stays disabled, its declaration is
/// not registered, its entities go and its saved entry follows the normal kept rules.
#[test]
fn a_restore_refuses_a_component_name_clash_like_a_load() {
    const BOTH: &str = r#"
        regia.component("mover", {
            props = { n = { type = "number", default = 0 } },
            on_update = function(e, dt) ticks = (ticks or 0) + 1 end,
        })
        function on_start()
            regia.spawn("walker", regia.vec3(0, 0, 0)):add("mover")
        end
    "#;
    let mut mods = host();
    let mut world = TestWorld::default();
    mods.load(mod_files("one", "1.0.0", BOTH), &mut world)
        .expect("the first mod loads");
    let refused = mods
        .load(mod_files("two", "1.0.0", BOTH), &mut world)
        .expect_err("the second declaration is refused");
    assert_eq!(refused.code.code(), "lua_bad_value");

    let (bytes, _) = mods.save_state(&mut world);
    let report = mods.restore_state(Some(&bytes), &mut world);

    // The typed refusal is on the report line; only one declaration exists.
    let error = report
        .mods
        .iter()
        .find(|line| line.id == "two")
        .expect("a line per mod")
        .error
        .as_ref()
        .expect("the restore refuses the clash");
    assert_eq!(error.code.code(), "lua_bad_value");
    assert!(error.message.contains("already declared"), "{error}");
    let one = report
        .mods
        .iter()
        .find(|line| line.id == "one")
        .expect("a line per mod");
    assert!(one.error.is_none(), "{one:?}");
    let names: Vec<String> = mods
        .iter()
        .flat_map(|module| module.components().into_iter().map(|decl| decl.name))
        .collect();
    assert_eq!(names, vec!["mover".to_owned()]);
    assert!(mods.get("one").expect("loaded").is_active());
    match mods.get("two").expect("kept, disabled").status() {
        ModStatus::Disabled { code, .. } => assert_eq!(code.code(), "lua_bad_value"),
        other => panic!("the restore left {other:?}"),
    }

    // The refused mod's spawn went; the survivor's hook ticks its one entity once per update,
    // not once per entity a second registration would have dragged in.
    let walkers = world
        .entities
        .values()
        .filter(|entity| entity.name == "walker")
        .count();
    assert_eq!(walkers, 1, "the refused mod's spawn goes");
    for _ in 0..3 {
        mods.call_update(&mut world, 1.0);
    }
    assert_eq!(number(&mut mods, "one", "ticks"), 3.0);
}

/// Finding 1, the other rebuild order: a restore rebuilds mods by id, so a refused mod whose id
/// sorts before the holder's is rebuilt while the holder is still the pre-restore one. The
/// holder keeps the name either way, and the refused mod stays refused.
#[test]
fn a_restore_keeps_the_holder_when_the_refused_mod_sorts_first() {
    const BOTH: &str = r#"
        regia.component("mover", {
            props = { n = { type = "number", default = 0 } },
            on_update = function(e, dt) ticks = (ticks or 0) + 1 end,
        })
        function on_start()
            regia.spawn("walker", regia.vec3(0, 0, 0)):add("mover")
        end
    "#;
    let mut mods = host();
    let mut world = TestWorld::default();
    mods.load(mod_files("zeta", "1.0.0", BOTH), &mut world)
        .expect("the holder loads first");
    let refused = mods
        .load(mod_files("alpha", "1.0.0", BOTH), &mut world)
        .expect_err("the second declaration is refused");
    assert_eq!(refused.code.code(), "lua_bad_value");

    let (bytes, _) = mods.save_state(&mut world);
    let report = mods.restore_state(Some(&bytes), &mut world);

    let alpha = report
        .mods
        .iter()
        .find(|line| line.id == "alpha")
        .expect("a line per mod");
    let error = alpha.error.as_ref().expect("the restore refuses the clash");
    assert_eq!(error.code.code(), "lua_bad_value");
    let zeta = report
        .mods
        .iter()
        .find(|line| line.id == "zeta")
        .expect("a line per mod");
    assert!(zeta.error.is_none(), "{zeta:?}");
    let declared: Vec<(String, String)> = mods
        .iter()
        .flat_map(|module| {
            let id = module.id().to_owned();
            module
                .components()
                .into_iter()
                .map(move |decl| (id.clone(), decl.name))
        })
        .collect();
    assert_eq!(declared, vec![("zeta".to_owned(), "mover".to_owned())]);
    assert!(mods.get("zeta").expect("loaded").is_active());
    assert!(!mods.get("alpha").expect("kept, disabled").is_active());
    let walkers = world
        .entities
        .values()
        .filter(|entity| entity.name == "walker")
        .count();
    assert_eq!(walkers, 1, "only the holder's spawn is in the world");
    for _ in 0..3 {
        mods.call_update(&mut world, 1.0);
    }
    assert_eq!(number(&mut mods, "zeta", "ticks"), 3.0);
}

/// Finding 2 (`sprint-deep-zai-script-lua`): a kept-entry load whose revive succeeded but whose
/// declaration clashed used to leave the revive's world writes in place and drop the saved entry
/// it had adopted. It now behaves like the failed revive beside it and the fresh path's rollback:
/// the entities go and the entry waits in `kept` again.
#[test]
fn a_refused_kept_entry_load_keeps_the_world_and_the_saved_entry() {
    const HOLDER: &str = r#"
        regia.component("mover", { props = { n = { type = "number", default = 0 } } })
    "#;
    const MOVER: &str = r#"
        regia.component("mover", { props = { n = { type = "number", default = 0 } } })
        function on_start()
            regia.spawn("probe", regia.vec3(1, 2, 3))
        end
    "#;
    // A save holding the mover mod's entry, made while it was the only mod.
    let mut first = host();
    let mut world = TestWorld::default();
    first
        .load(mod_files("mover", "1.0.0", MOVER), &mut world)
        .expect("loads");
    let (bytes, _) = first.save_state(&mut world);

    // A fresh host restores the save without the mod: the entry waits in `kept`.
    let mut mods = host();
    let mut world = TestWorld::default();
    let report = mods.restore_state(Some(&bytes), &mut world);
    assert_eq!(report.kept, vec!["mover".to_owned()]);

    // Another mod takes the name first, then the kept-entry load revives and meets the clash.
    mods.load(mod_files("holder", "1.0.0", HOLDER), &mut world)
        .expect("the name was free");
    let refused = mods
        .load(mod_files("mover", "1.0.0", MOVER), &mut world)
        .expect_err("the revived declaration clashes");
    assert_eq!(refused.code.code(), "lua_bad_value");
    assert!(refused.message.contains("already declared"), "{refused}");

    // The revive's world writes went and the saved entry still waits for the next save.
    assert!(world.find("probe").is_none(), "the revived spawn goes");
    let kept: Vec<&str> = mods.kept_state().collect();
    assert_eq!(kept, vec!["mover"], "the entry is kept again");
    match mods.get("mover").expect("kept, disabled").status() {
        ModStatus::Disabled { code, .. } => assert_eq!(code.code(), "lua_bad_value"),
        other => panic!("the load left {other:?}"),
    }
    let (_, save) = mods.save_state(&mut world);
    let line = save
        .mods
        .iter()
        .find(|line| line.id == "mover")
        .expect("a line per mod");
    assert!(line.bytes > 0, "the kept entry is written into the next save");
}

/// Finding 3 (`sprint-deep-zai-script-lua`): `lua.run` without `mod` runs in a throwaway
/// sandbox, but its spawns used to stay in the world under `agent.scratch`, growing it silently
/// until the entity cap refused every later scratch spawn. The sandbox now takes its spawns with
/// it when it goes: nothing a scratch call leaves persists in the world.
#[test]
fn a_scratch_sandbox_takes_its_spawns_with_it() {
    let mut world = TestWorld::default();
    let mut mods = host();
    let outcome = mods
        .run_chunk(&mut world, None, "local e = regia.spawn('probe') return e:name()")
        .expect("runs");
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(outcome.results, [ScriptValue::String("probe".to_owned())]);
    assert!(world.entities.is_empty(), "no scratch entity persists");
    // A later scratch call cannot find the earlier call's spawn: nothing of it is left.
    let outcome = mods
        .run_chunk(&mut world, None, "return regia.find('probe') ~= nil")
        .expect("runs");
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(outcome.results, [ScriptValue::Bool(false)]);
}

#[test]
fn a_mod_with_no_on_load_restarts_with_its_state() {
    // The guide's first mod: it knows nothing of saves and holds a handle in a local.
    const PLAIN: &str = r#"
    local lamp
    function on_start()
      lamp = regia.spawn("lamp", regia.vec3(0, 3, 0))
      regia.state.starts = (regia.state.starts or 0) + 1
    end
    function on_tick()
      if lamp:alive() then lamp:set_position(lamp:position() + regia.vec3(0, 1, 0)) end
    end
    "#;
    let mut world = TestWorld::default();
    let mut mods = host();
    mods.load(mod_files("p", "1", PLAIN), &mut world).expect("loads");
    mods.call_tick(&mut world, 0.1);
    let (bytes, _) = mods.save_state(&mut world);
    mods.call_tick(&mut world, 0.1);
    let report = mods.restore_state(Some(&bytes), &mut world);
    assert_eq!(report.mods[0].state, Some(StateCarry::Carried));
    assert_eq!(
        world.names(),
        ["lamp"],
        "one lamp: the old one went, on_start made one"
    );
    assert_eq!(world.position_of("lamp"), Some(Vec3::new(0.0, 3.0, 0.0)));
    assert_eq!(
        number(&mut mods, "p", "regia.state.starts"),
        2.0,
        "state then on_start"
    );
    mods.call_tick(&mut world, 0.1);
    assert!(mods.get("p").is_some_and(regia_script_lua::LuaMod::is_active));
}

#[test]
fn a_mod_the_save_does_not_know_starts_fresh() {
    let mut world = TestWorld::default();
    let mut mods = host();
    mods.load(mod_files("a", "1", COUNTER), &mut world)
        .expect("loads");
    let (bytes, _) = mods.save_state(&mut world);
    mods.load(mod_files("b", "1", COUNTER), &mut world)
        .expect("loads");
    mods.call_tick(&mut world, 0.1);
    let report = mods.restore_state(Some(&bytes), &mut world);
    assert_eq!(report.mods[0].state, Some(StateCarry::Carried));
    assert_eq!(report.mods[1].state, Some(StateCarry::Fresh));
    assert_eq!(number(&mut mods, "b", "started"), 1.0);
    assert_eq!(number(&mut mods, "b", "loaded"), 0.0);
    assert_eq!(number(&mut mods, "b", "regia.state.count"), 0.0);
}

#[test]
fn a_save_with_no_mod_state_restarts_every_mod_and_drops_the_queues() {
    let mut world = TestWorld::default();
    let mut mods = host();
    mods.load(
        mod_files(
            "a",
            "1",
            "regia.emit('at_load', 1) function on_start() regia.spawn('x') end",
        ),
        &mut world,
    )
    .expect("loads");
    mods.run_chunk(&mut world, Some("a"), "regia.state.count = 5")
        .expect("runs");
    mods.fire("queued", ScriptValue::Nil);
    let report = mods.restore_state(None, &mut world);
    assert_eq!(report.mods[0].state, Some(StateCarry::Fresh));
    assert_eq!(world.names(), ["x"]);
    assert_eq!(global(&mut mods, "a", "regia.state.count"), ScriptValue::Nil);
    let emitted = mods.drain_emitted();
    assert_eq!(emitted.len(), 1, "only the restart's own event: {emitted:?}");
    mods.fire("seen", ScriptValue::Nil);
    mods.deliver_events(&mut world);
}

const V1: &str = r#"
function on_start() regia.state.gold = 7 end
function on_load() end
"#;

fn saved_v1() -> Vec<u8> {
    let mut mods = host();
    mods.load(mod_files("m", "1.0.0", V1), &mut NoWorld)
        .expect("loads");
    mods.save_state(&mut NoWorld).0
}

#[test]
fn on_migrate_turns_an_old_state_into_the_new_shape() {
    const V3: &str = r#"
    regia.state_version = 3
    function on_migrate(old, from, to)
      seen_from, seen_to = from, to
      return { coins = old.gold * 100, schema = "new" }
    end
    function on_load(info) last = info end
    "#;
    let mut mods = host();
    mods.load(mod_files("m", "2.0.0", V3), &mut NoWorld)
        .expect("loads");
    let report = mods.restore_state(Some(&saved_v1()), &mut NoWorld);
    assert_eq!(
        report.mods[0].state,
        Some(StateCarry::Migrated { from: 1, to: 3 })
    );
    assert_eq!(number(&mut mods, "m", "regia.state.coins"), 700.0);
    assert_eq!(global(&mut mods, "m", "regia.state.gold"), ScriptValue::Nil);
    assert_eq!(number(&mut mods, "m", "seen_from"), 1.0);
    assert_eq!(number(&mut mods, "m", "seen_to"), 3.0);
    assert_eq!(
        global(&mut mods, "m", "last.state"),
        ScriptValue::String("migrated".into())
    );
    assert_eq!(number(&mut mods, "m", "last.from_version"), 1.0);
    assert_eq!(number(&mut mods, "m", "last.to_version"), 3.0);

    // The migrated state saves with the new version.
    let (bytes, _) = mods.save_state(&mut NoWorld);
    let file = decode_luastate(&bytes, &LuaLimits::default()).expect("decodes");
    assert_eq!(file.mods[0].state_version, 3);
}

#[test]
fn on_migrate_may_edit_in_place_and_returning_nothing_keeps_that() {
    const V2: &str = r#"
    regia.state_version = 2
    function on_migrate(old) old.gold = old.gold + 1 end
    function on_load() end
    "#;
    let mut mods = host();
    mods.load(mod_files("m", "2", V2), &mut NoWorld).expect("loads");
    mods.restore_state(Some(&saved_v1()), &mut NoWorld);
    assert_eq!(number(&mut mods, "m", "regia.state.gold"), 8.0);
}

#[test]
fn a_version_change_with_no_migrator_resets_and_says_so() {
    const V2: &str = r#"
    regia.state_version = 2
    regia.state.silver = 1
    function on_load(info) last = info end
    "#;
    let mut mods = host();
    mods.load(mod_files("m", "2", V2), &mut NoWorld).expect("loads");
    let report = mods.restore_state(Some(&saved_v1()), &mut NoWorld);
    assert_eq!(report.mods[0].state, Some(StateCarry::Reset { from: 1, to: 2 }));
    assert_eq!(global(&mut mods, "m", "regia.state.gold"), ScriptValue::Nil);
    assert_eq!(number(&mut mods, "m", "regia.state.silver"), 1.0);
    assert_eq!(
        global(&mut mods, "m", "last.state"),
        ScriptValue::String("reset".into())
    );
}

#[test]
fn a_failed_migration_disables_that_mod_and_keeps_its_saved_entry() {
    const BAD: &str = r#"
    regia.state_version = 2
    function on_migrate() error("cannot read this version") end
    "#;
    let bytes = saved_v1();
    let mut mods = host();
    mods.load(mod_files("m", "2", BAD), &mut NoWorld).expect("loads");
    mods.load(mod_files("other", "1", COUNTER), &mut NoWorld)
        .expect("loads");
    let report = mods.restore_state(Some(&bytes), &mut NoWorld);
    let line = &report.mods[0];
    assert_eq!(line.state, None);
    assert_eq!(line.error.as_ref().map(|e| e.code), Some(LuaErrorCode::Runtime));
    assert!(matches!(
        mods.get("m").map(regia_script_lua::LuaMod::status),
        Some(ModStatus::Disabled { .. })
    ));
    assert_eq!(
        report.mods[1].state,
        Some(StateCarry::Fresh),
        "the other mod is fine"
    );

    // Every later save still holds the v1 state, so a fixed mod can pick it up.
    for _ in 0..3 {
        let (bytes, _) = mods.save_state(&mut NoWorld);
        let file = decode_luastate(&bytes, &LuaLimits::default()).expect("decodes");
        let entry = file.mods.iter().find(|m| m.id == "m").expect("kept");
        assert_eq!(entry.state_version, 1);
        assert_eq!(
            entry.tree.tables[0].len(),
            1,
            "gold, not what the disabled state holds"
        );
    }

    // A hot reload with the fixed migrator takes the kept entry.
    const FIXED: &str = r#"
    regia.state_version = 2
    function on_migrate(old) old.gold = old.gold + 1 end
    function on_load() end
    "#;
    let outcome = mods
        .reload("m", Some(mod_files("m", "3", FIXED)), &mut NoWorld)
        .expect("exists");
    assert_eq!(
        outcome.state,
        Some(StateCarry::Migrated { from: 1, to: 2 }),
        "{outcome:?}"
    );
    assert_eq!(number(&mut mods, "m", "regia.state.gold"), 8.0);
    assert_eq!(mods.kept_state().count(), 0);
}

#[test]
fn a_running_mods_own_state_replaces_an_old_kept_entry() {
    let bad = "regia.state_version = 2 function on_migrate() error('no') end";
    let mut mods = host();
    mods.load(mod_files("m", "2", bad), &mut NoWorld).expect("loads");
    mods.restore_state(Some(&saved_v1()), &mut NoWorld);
    assert_eq!(mods.kept_state().count(), 1);
    mods.restart("m", &mut NoWorld).expect("restarts");
    mods.run_chunk(&mut NoWorld, Some("m"), "regia.state.mine = 1")
        .expect("runs");
    let (bytes, _) = mods.save_state(&mut NoWorld);
    let file = decode_luastate(&bytes, &LuaLimits::default()).expect("decodes");
    assert_eq!(file.mods[0].state_version, 2);
    assert_eq!(mods.kept_state().count(), 0);
}

#[test]
fn a_migrator_must_leave_plain_data() {
    const BAD: &str = r#"
    regia.state_version = 2
    function on_migrate() return { f = print } end
    "#;
    let mut mods = host();
    mods.load(mod_files("m", "2", BAD), &mut NoWorld).expect("loads");
    let report = mods.restore_state(Some(&saved_v1()), &mut NoWorld);
    assert_eq!(
        report.mods[0].error.as_ref().map(|e| e.code),
        Some(LuaErrorCode::BadValue)
    );
    assert!(mods.get("m").is_some_and(|m| !m.is_active()));
}

#[test]
fn a_function_in_the_state_leaves_that_mod_out_and_the_save_goes_on() {
    let mut mods = host();
    mods.load(mod_files("a_good", "1", COUNTER), &mut NoWorld)
        .expect("loads");
    mods.load(
        mod_files("bad", "1", "regia.state.f = function() end"),
        &mut NoWorld,
    )
    .expect("loads");
    mods.load(
        mod_files("handle", "1", "regia.state.e = regia.find('x')"),
        &mut NoWorld,
    )
    .expect("loads");
    mods.call_tick(&mut NoWorld, 0.1);
    let (bytes, report) = mods.save_state(&mut NoWorld);
    assert_eq!(report.mods[0].error, None);
    let error = report.mods[1].error.as_ref().expect("refused");
    assert_eq!(error.code, LuaErrorCode::BadValue);
    assert!(error.message.contains("regia.state.f"), "{}", error.message);
    assert!(
        mods.get("bad").is_some_and(regia_script_lua::LuaMod::is_active),
        "not a call error"
    );
    assert_eq!(
        mods.get("bad").map(|m| m.errors().count()),
        Some(1),
        "but on the record"
    );
    let file = decode_luastate(&bytes, &LuaLimits::default()).expect("decodes");
    assert_eq!(file.mods.len(), 2);
}

#[test]
fn an_entity_handle_in_the_state_is_refused_with_advice() {
    let mut world = TestWorld::default();
    world.add("x", Vec3::ZERO);
    let mut mods = host();
    mods.load(mod_files("h", "1", "regia.state.e = regia.find('x')"), &mut world)
        .expect("loads");
    let (_, report) = mods.save_state(&mut world);
    let error = report.mods[0].error.as_ref().expect("refused");
    assert_eq!(error.code, LuaErrorCode::BadValue);
    assert!(error.message.contains("regia.state.e"), "{}", error.message);
    assert!(error.message.contains("regia.find"), "{}", error.message);
}

#[test]
fn on_save_runs_first_and_a_failure_saves_the_state_as_it_stood() {
    let mut mods = host();
    mods.load(
        mod_files(
            "flush",
            "1",
            "local n = 41 function on_save() regia.state.n = n + 1 regia.emit('saving', 1) end",
        ),
        &mut NoWorld,
    )
    .expect("loads");
    mods.load(
        mod_files(
            "loops",
            "1",
            "regia.state.gold = 7 function on_save() while true do end end",
        ),
        &mut NoWorld,
    )
    .expect("loads");
    mods.load(mod_files("zz_last", "1", COUNTER), &mut NoWorld)
        .expect("loads");
    let (bytes, report) = mods.save_state(&mut NoWorld);
    assert_eq!(report.mods[0].error, None);
    assert_eq!(
        report.mods[1].error.as_ref().map(|e| e.code),
        Some(LuaErrorCode::InstructionBudget)
    );
    assert!(
        report.mods[1].bytes > 0,
        "the failure is reported, the state still saves"
    );
    assert_eq!(report.mods[2].error, None);
    assert!(
        mods.get("loops").is_some_and(|m| !m.is_active()),
        "a call error disables"
    );
    assert_eq!(mods.drain_emitted().len(), 1);
    let file = decode_luastate(&bytes, &LuaLimits::default()).expect("decodes");
    assert_eq!(
        file.mods.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["flush", "loops", "zz_last"]
    );

    // What a player keeps: the gold survives the bug in `on_save`.
    let mut next = host();
    next.load(mod_files("loops", "1", "function on_load() end"), &mut NoWorld)
        .expect("loads");
    let restored = next.restore_state(Some(&bytes), &mut NoWorld);
    assert_eq!(restored.mods[0].state, Some(StateCarry::Carried));
    assert_eq!(number(&mut next, "loops", "regia.state.gold"), 7.0);
}

#[test]
fn caps_bound_one_mod_and_all_of_them() {
    let big = |id: &str| {
        mod_files(
            id,
            "1",
            "for i = 1, 40 do regia.state[i] = string.rep('x', 1024) end",
        )
    };
    let limits = LuaLimits {
        max_state_bytes: 16 * 1024,
        ..LuaLimits::default()
    };
    let mut mods = LuaMods::new(1, limits);
    mods.load(big("a"), &mut NoWorld).expect("loads");
    let (_, report) = mods.save_state(&mut NoWorld);
    assert_eq!(
        report.mods[0].error.as_ref().map(|e| e.code),
        Some(LuaErrorCode::TooLarge)
    );

    let limits = LuaLimits {
        max_state_total_bytes: 64 * 1024,
        ..LuaLimits::default()
    };
    let mut mods = LuaMods::new(1, limits);
    for id in ["a", "b", "c"] {
        mods.load(big(id), &mut NoWorld).expect("loads");
    }
    let (bytes, report) = mods.save_state(&mut NoWorld);
    assert_eq!(report.mods[0].error, None);
    assert_eq!(
        report.mods[2].error.as_ref().map(|e| e.code),
        Some(LuaErrorCode::TooLarge)
    );
    assert!(bytes.len() as u64 <= limits.max_state_total_bytes + 16);

    let limits = LuaLimits {
        max_state_entities: 2,
        ..LuaLimits::default()
    };
    // Audit v0.2.12: a mod used to reach the cap by legal spawns and then lose its whole entry at
    // every save. The spawn past the cap is refused (`lua_too_large`, catchable), so the save
    // keeps the state and the entities the mod owns.
    let mut world = TestWorld::default();
    let mut mods = LuaMods::new(1, limits);
    mods.load(
        mod_files(
            "e",
            "1",
            "function on_start()
               regia.state.refused = 0
               for i = 1, 3 do
                 local ok, err = pcall(regia.spawn, 'x')
                 if not ok then regia.state.refused = regia.state.refused + 1; regia.state.why = tostring(err) end
               end
             end",
        ),
        &mut world,
    )
    .expect("loads");
    assert_eq!(
        number(&mut mods, "e", "regia.state.refused"),
        1.0,
        "the third spawn"
    );
    assert_eq!(world.names(), ["x", "x"]);
    let (_, report) = mods.save_state(&mut world);
    assert_eq!(report.mods[0].error, None, "{report:?}");
    assert!(report.mods[0].bytes > 0, "the entry is saved");
    // An uncaught refusal is an ordinary failed call.
    let mut world = TestWorld::default();
    let mut mods = LuaMods::new(1, limits);
    let err = mods
        .load(
            mod_files(
                "f",
                "1",
                "function on_start() for i = 1, 3 do regia.spawn('x') end end",
            ),
            &mut world,
        )
        .expect_err("refused");
    assert_eq!(err.code, LuaErrorCode::TooLarge, "{err:?}");
}

#[test]
fn a_state_at_its_memory_cap_can_still_save() {
    let limits = LuaLimits {
        memory_limit_bytes: 256 * 1024,
        ..LuaLimits::default()
    };
    let mut mods = LuaMods::new(1, limits);
    mods.load(
        mod_files(
            "full",
            "1",
            "pcall(function() while true do regia.state[#regia.state + 1] = {} end end)",
        ),
        &mut NoWorld,
    )
    .expect_err("fills up");
    let (bytes, _) = mods.save_state(&mut NoWorld);
    assert!(!bytes.is_empty());
    assert_eq!(
        mods.get("full").map(regia_script_lua::LuaMod::is_active),
        Some(false)
    );
}

#[test]
fn a_saved_cap_lower_on_load_refuses_that_entry_only() {
    let mut first = host();
    first
        .load(
            mod_files(
                "big",
                "1",
                "function on_load() end for i = 1, 50 do regia.state[i] = i end",
            ),
            &mut NoWorld,
        )
        .expect("loads");
    let (bytes, _) = first.save_state(&mut NoWorld);
    let limits = LuaLimits {
        max_state_items: 10,
        ..LuaLimits::default()
    };
    let mut second = LuaMods::new(1, limits);
    second
        .load(mod_files("big", "1", "function on_load() end"), &mut NoWorld)
        .expect("loads");
    let report = second.restore_state(Some(&bytes), &mut NoWorld);
    assert_eq!(
        report.mods[0].error.as_ref().map(|e| e.code),
        Some(LuaErrorCode::TooLarge)
    );
    assert_eq!(second.get("big").map(|m| m.errors().count()), Some(1));
    assert_eq!(
        second.get("big").map(regia_script_lua::LuaMod::is_active),
        Some(false)
    );
}

#[test]
fn the_per_mod_byte_cap_holds_on_load_too() {
    let big = "function on_load() end for i = 1, 40 do regia.state[i] = string.rep('x', 1024) end";
    let small = "function on_load() end regia.state.n = 1";
    let mut first = host();
    first
        .load(mod_files("big", "1", big), &mut NoWorld)
        .expect("loads");
    first
        .load(mod_files("small", "1", small), &mut NoWorld)
        .expect("loads");
    let (bytes, _) = first.save_state(&mut NoWorld);

    // Inside the items and total caps; only the bytes of one mod are over.
    let limits = LuaLimits {
        max_state_bytes: 16 * 1024,
        ..LuaLimits::default()
    };
    let mut second = LuaMods::new(1, limits);
    second
        .load(mod_files("big", "1", "function on_load() end"), &mut NoWorld)
        .expect("loads");
    second
        .load(mod_files("small", "1", "function on_load() end"), &mut NoWorld)
        .expect("loads");
    let report = second.restore_state(Some(&bytes), &mut NoWorld);
    assert_eq!(
        report.mods[0].error.as_ref().map(|e| e.code),
        Some(LuaErrorCode::TooLarge)
    );
    assert_eq!(report.mods[1].state, Some(StateCarry::Carried));
    assert_eq!(number(&mut second, "small", "regia.state.n"), 1.0);
}

#[test]
fn a_save_exactly_at_the_total_cap_reads_back() {
    let code = "function on_load() end regia.state.s = string.rep('x', 500)";
    let mut probe = host();
    probe
        .load(mod_files("a", "1", code), &mut NoWorld)
        .expect("loads");
    probe
        .load(mod_files("b", "1", code), &mut NoWorld)
        .expect("loads");
    let (_, report) = probe.save_state(&mut NoWorld);
    let exact: u64 = report.mods.iter().map(|line| line.bytes).sum();

    let limits = LuaLimits {
        max_state_total_bytes: exact,
        ..LuaLimits::default()
    };
    let mut mods = LuaMods::new(1, limits);
    mods.load(mod_files("a", "1", code), &mut NoWorld).expect("loads");
    mods.load(mod_files("b", "1", code), &mut NoWorld).expect("loads");
    let (bytes, report) = mods.save_state(&mut NoWorld);
    assert_eq!(report.mods.iter().filter(|line| line.error.is_some()).count(), 0);
    let restored = mods.restore_state(Some(&bytes), &mut NoWorld);
    assert_eq!(restored.error, None, "a save the host wrote is a save it reads");
    assert_eq!(restored.mods[1].state, Some(StateCarry::Carried));
}

/// Keeps `regia.state` in a local at the top of the chunk, the first thing a modder writes.
const LOCAL_STATE: &str = r#"
local s = regia.state
s.n = s.n or 0
function on_tick() s.n = s.n + 1 end
function same_table() return s == regia.state end
function on_load() end
"#;

#[test]
fn a_local_kept_by_the_chunk_stays_the_state_after_a_load() {
    let mut first = host();
    first
        .load(mod_files("m", "1", LOCAL_STATE), &mut NoWorld)
        .expect("loads");
    for _ in 0..3 {
        first.call_tick(&mut NoWorld, 0.1);
    }
    first
        .run_chunk(&mut NoWorld, Some("m"), "regia.state.me = regia.state")
        .expect("the mod exists");
    let (bytes, _) = first.save_state(&mut NoWorld);

    let mut second = host();
    second
        .load(mod_files("m", "1", LOCAL_STATE), &mut NoWorld)
        .expect("loads");
    second.restore_state(Some(&bytes), &mut NoWorld);
    assert_eq!(global(&mut second, "m", "same_table()"), ScriptValue::Bool(true));
    assert_eq!(
        global(&mut second, "m", "regia.state.me == regia.state"),
        ScriptValue::Bool(true)
    );
    for _ in 0..2 {
        second.call_tick(&mut NoWorld, 0.1);
    }
    assert_eq!(
        number(&mut second, "m", "regia.state.n"),
        5.0,
        "ticks reach the saved state"
    );
    let (bytes, _) = second.save_state(&mut NoWorld);

    let mut third = host();
    third
        .load(mod_files("m", "1", LOCAL_STATE), &mut NoWorld)
        .expect("loads");
    third.restore_state(Some(&bytes), &mut NoWorld);
    assert_eq!(
        number(&mut third, "m", "regia.state.n"),
        5.0,
        "and the next save holds them"
    );
}

#[test]
fn a_local_kept_by_the_chunk_stays_the_state_after_a_migration() {
    for migrate in [
        "function on_migrate(old) return { n = old.n + 100, me = nil } end",
        "function on_migrate(old) old.n = old.n + 100 end",
    ] {
        let mut first = host();
        first
            .load(mod_files("m", "1", "regia.state.n = 1"), &mut NoWorld)
            .expect("loads");
        let (bytes, _) = first.save_state(&mut NoWorld);

        let code = format!("regia.state_version = 2 {LOCAL_STATE} {migrate}");
        let mut second = host();
        second
            .load(mod_files("m", "2", &code), &mut NoWorld)
            .expect("loads");
        second.restore_state(Some(&bytes), &mut NoWorld);
        assert_eq!(
            global(&mut second, "m", "same_table()"),
            ScriptValue::Bool(true),
            "{migrate}"
        );
        second.call_tick(&mut NoWorld, 0.1);
        assert_eq!(number(&mut second, "m", "regia.state.n"), 102.0, "{migrate}");
    }
}

#[test]
fn a_chunk_that_gave_the_state_a_metatable_does_not_keep_it_through_a_load() {
    let plain = "function on_load() end regia.state.n = 3";
    let mut first = host();
    first
        .load(mod_files("m", "1", plain), &mut NoWorld)
        .expect("loads");
    let (bytes, _) = first.save_state(&mut NoWorld);

    let wrapped = "function on_load() end regia.state = setmetatable({}, {})";
    let mut second = host();
    second
        .load(mod_files("m", "1", wrapped), &mut NoWorld)
        .expect("loads");
    second.restore_state(Some(&bytes), &mut NoWorld);
    assert_eq!(number(&mut second, "m", "regia.state.n"), 3.0);
    let (_, report) = second.save_state(&mut NoWorld);
    assert_eq!(report.mods[0].error, None, "the restored state is plain");
}

#[test]
fn a_disabled_mod_saves_what_it_has_and_comes_back_on_load() {
    let mut mods = host();
    mods.load(
        mod_files(
            "d",
            "1",
            "function on_start() regia.state.kept = 3 end function on_tick() error('boom') end function on_load() end",
        ),
        &mut NoWorld,
    )
    .expect("loads");
    mods.call_tick(&mut NoWorld, 0.1);
    assert!(mods.get("d").is_some_and(|m| !m.is_active()));
    let (bytes, report) = mods.save_state(&mut NoWorld);
    assert_eq!(report.mods[0].error, None);
    let restored = mods.restore_state(Some(&bytes), &mut NoWorld);
    assert_eq!(restored.mods[0].state, Some(StateCarry::Carried));
    assert!(mods.get("d").is_some_and(regia_script_lua::LuaMod::is_active));
    assert_eq!(number(&mut mods, "d", "regia.state.kept"), 3.0);
}

#[test]
fn the_entry_of_a_mod_that_is_not_running_waits_for_it() {
    let mut first = host();
    first
        .load(mod_files("away", "1", COUNTER), &mut NoWorld)
        .expect("loads");
    first
        .load(mod_files("here", "1", COUNTER), &mut NoWorld)
        .expect("loads");
    for _ in 0..4 {
        first.call_tick(&mut NoWorld, 0.1);
    }
    let (bytes, _) = first.save_state(&mut NoWorld);

    // The player turned `away` off: its state stays in the save through two rounds.
    let mut second = host();
    second
        .load(mod_files("here", "1", COUNTER), &mut NoWorld)
        .expect("loads");
    let report = second.restore_state(Some(&bytes), &mut NoWorld);
    assert_eq!(report.kept, ["away"]);
    assert_eq!(second.kept_state().collect::<Vec<_>>(), ["away"]);
    let (bytes, report) = second.save_state(&mut NoWorld);
    assert_eq!(report.kept, ["away"]);

    // Back on, loaded after the save: it adopts the entry.
    let mut third = host();
    third
        .load(mod_files("here", "1", COUNTER), &mut NoWorld)
        .expect("loads");
    third.restore_state(Some(&bytes), &mut NoWorld);
    third
        .load(mod_files("away", "1", COUNTER), &mut NoWorld)
        .expect("loads");
    assert_eq!(number(&mut third, "away", "regia.state.count"), 4.0);
    assert_eq!(third.kept_state().count(), 0);
}

#[test]
fn a_damaged_blob_starts_every_mod_fresh_and_says_why() {
    let mut mods = host();
    mods.load(mod_files("a", "1", COUNTER), &mut NoWorld)
        .expect("loads");
    mods.call_tick(&mut NoWorld, 0.1);
    let (mut bytes, _) = mods.save_state(&mut NoWorld);
    bytes.truncate(bytes.len() / 2);
    let report = mods.restore_state(Some(&bytes), &mut NoWorld);
    assert_eq!(report.error.as_ref().map(|e| e.code), Some(LuaErrorCode::State));
    assert_eq!(report.mods[0].state, Some(StateCarry::Fresh));
    assert_eq!(mods.last_restore(), Some(&report));
    assert_eq!(number(&mut mods, "a", "regia.state.count"), 0.0);
}

#[test]
fn saves_of_the_same_state_are_the_same_bytes() {
    let a = "regia.state.a = 1 regia.state.b = { 2 } regia.state.c = 'x'";
    let b = "regia.state.c = 'x' regia.state.b = { 2 } regia.state.gone = 1 regia.state.a = 1 regia.state.gone = nil";
    let save = |main: &str| {
        let mut mods = host();
        mods.load(mod_files("s", "1", main), &mut NoWorld).expect("loads");
        mods.save_state(&mut NoWorld).0
    };
    assert_eq!(save(a), save(b));
}

#[test]
fn restoring_in_a_loop_leaves_the_same_state() {
    let files: ModFiles = mod_files("a", "1", COUNTER);
    let mut mods = host();
    mods.load(files, &mut NoWorld).expect("loads");
    mods.call_tick(&mut NoWorld, 0.1);
    let (bytes, _) = mods.save_state(&mut NoWorld);
    for _ in 0..32 {
        mods.restore_state(Some(&bytes), &mut NoWorld);
    }
    assert_eq!(mods.save_state(&mut NoWorld).0, bytes);
}
