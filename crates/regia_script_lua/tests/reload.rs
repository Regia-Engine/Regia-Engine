// SPDX-License-Identifier: MPL-2.0
//! Lua hot reload (`docs/specs/84-lua-mods.md` § 12, ADR-1943): state carried across, every failure
//! rolled back to the running version with the world as it was, and the directory watch.

mod common;

use common::{TestWorld, host, manifest, mod_files};
use regia_core::math::Vec3;
use regia_script_lua::{
    LuaErrorCode, LuaLimits, LuaMod, LuaMods, ModFiles, ModStatus, NoWorld, ReloadOutcome, ReloadStage,
    ScriptValue, StateCarry, mod_file_body_reads, reset_mod_file_body_reads,
};

const OLD: &str = r#"
version_tag = "old"
function on_start() regia.state.count = 0 regia.on("ping", function() regia.emit("pong", "old") end) end
function on_tick() regia.state.count = regia.state.count + 1 end
function on_load() end
"#;

const NEW: &str = r#"
version_tag = "new"
regia.on("ping", function() regia.emit("pong", "new") end)
function on_tick() regia.state.count = regia.state.count + 10 end
function on_load(info) last_reason, last_state = info.reason, info.state end
"#;

fn eval(mods: &mut LuaMods, world: &mut TestWorld, chunk: &str) -> ScriptValue {
    let outcome = mods.run_chunk(world, Some("m"), chunk).expect("the mod exists");
    assert!(outcome.ok, "{chunk}: {:?}", outcome.error);
    outcome.results.into_iter().next().unwrap_or_default()
}

fn started(world: &mut TestWorld) -> LuaMods {
    let mut mods = host();
    mods.load(mod_files("m", "1.0.0", OLD), world).expect("loads");
    mods.call_tick(world, 0.1);
    mods
}

fn reload(mods: &mut LuaMods, world: &mut TestWorld, main: &str) -> ReloadOutcome {
    mods.reload("m", Some(mod_files("m", "2.0.0", main)), world)
        .expect("the mod exists")
}

/// The old version runs, its state is as it was, and it answers events.
fn assert_rolled_back(
    mods: &mut LuaMods,
    world: &mut TestWorld,
    outcome: &ReloadOutcome,
    stage: ReloadStage,
) {
    assert!(!outcome.ok, "{outcome:?}");
    assert_eq!(outcome.stage, stage, "{outcome:?}");
    assert_eq!(outcome.state, None);
    assert_eq!(outcome.version_after, "1.0.0");
    assert!(outcome.error.is_some());
    assert_eq!(
        eval(mods, world, "return version_tag"),
        ScriptValue::String("old".into())
    );
    assert_eq!(
        eval(mods, world, "return regia.state.count"),
        ScriptValue::Number(1.0)
    );
    assert!(
        mods.get("m").is_some_and(LuaMod::is_active),
        "the old version still runs"
    );
    assert_eq!(
        mods.get("m").map(|m| m.manifest().version.clone()).as_deref(),
        Some("1.0.0")
    );
    mods.drain_emitted();
    mods.fire("ping", ScriptValue::Nil);
    mods.deliver_events(world);
    let events = mods.drain_emitted();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].value, ScriptValue::String("old".into()));
    let counters = mods.get("m").map(LuaMod::counters).unwrap_or_default();
    assert_eq!((counters.reloads, counters.failed_reloads), (0, 1));
    let last_error = mods
        .get("m")
        .and_then(|m| m.errors().last().map(|(call, _)| call.to_owned()));
    assert_eq!(last_error.as_deref(), Some("reload"));
}

#[test]
fn new_code_takes_over_with_the_state_and_the_old_handlers_are_gone() {
    let mut world = TestWorld::default();
    let mut mods = started(&mut world);
    let outcome = reload(&mut mods, &mut world, NEW);
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(outcome.stage, ReloadStage::Done);
    assert_eq!(outcome.state, Some(StateCarry::Carried));
    assert_eq!(
        (outcome.version_before.as_str(), outcome.version_after.as_str()),
        ("1.0.0", "2.0.0")
    );
    assert_eq!(mods.get("m").and_then(LuaMod::last_reload), Some(&outcome));
    assert_eq!(mods.get("m").map(|m| m.counters().reloads), Some(1));
    assert_eq!(
        eval(&mut mods, &mut world, "return version_tag"),
        ScriptValue::String("new".into())
    );
    assert_eq!(
        eval(&mut mods, &mut world, "return last_reason"),
        ScriptValue::String("reload".into())
    );
    assert_eq!(
        eval(&mut mods, &mut world, "return last_state"),
        ScriptValue::String("carried".into())
    );
    mods.call_tick(&mut world, 0.1);
    assert_eq!(
        eval(&mut mods, &mut world, "return regia.state.count"),
        ScriptValue::Number(11.0)
    );
    mods.fire("ping", ScriptValue::Nil);
    mods.deliver_events(&mut world);
    let events = mods.drain_emitted();
    assert_eq!(events.len(), 1, "one handler, the new one: {events:?}");
    assert_eq!(events[0].value, ScriptValue::String("new".into()));
}

#[test]
fn the_random_numbers_go_on_across_a_reload() {
    let code = "function on_load() end function roll() return math.random(1000000) end";
    let run = |reload_at: Option<u32>| {
        let mut mods = host();
        let mut world = TestWorld::default();
        mods.load(mod_files("m", "1", code), &mut world).expect("loads");
        (0..20)
            .map(|i| {
                if reload_at == Some(i) {
                    let outcome = mods
                        .reload("m", Some(mod_files("m", "2", code)), &mut world)
                        .expect("exists");
                    assert!(outcome.ok);
                }
                eval(&mut mods, &mut world, "return roll()")
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(run(None), run(Some(9)));
}

#[test]
fn a_syntax_error_rolls_back_and_names_the_file_and_line() {
    let mut world = TestWorld::default();
    let mut mods = started(&mut world);
    let outcome = reload(&mut mods, &mut world, "function on_tick(\n  oops\n");
    assert_rolled_back(&mut mods, &mut world, &outcome, ReloadStage::Compile);
    let error = outcome.error.expect("error");
    assert_eq!(error.code, LuaErrorCode::Syntax);
    assert!(error.message.contains("m/main.lua:"), "{}", error.message);
}

#[test]
fn a_syntax_error_in_a_module_nothing_required_yet_rolls_back() {
    let mut world = TestWorld::default();
    let mut mods = started(&mut world);
    let files = ModFiles::from_map([
        ("luamod.ron", manifest("m", "2.0.0")),
        ("main.lua", NEW.to_owned()),
        ("lib/later.lua", "this is not lua".to_owned()),
    ]);
    let outcome = mods.reload("m", Some(files), &mut world).expect("exists");
    assert_rolled_back(&mut mods, &mut world, &outcome, ReloadStage::Compile);
    assert!(
        outcome
            .error
            .is_some_and(|e| e.message.contains("m/lib/later.lua")),
        "the module is named"
    );
}

#[test]
fn a_runtime_error_undoes_what_the_new_code_did_to_the_world() {
    const BREAKS: &str = r#"
    local game = regia.find("game_entity")
    game:set_position(regia.vec3(50, 50, 50))
    game:set_position(regia.vec3(60, 60, 60))
    regia.spawn("from_new_version")
    regia.emit("from_new_version", 1)
    error("the new version is broken")
    "#;
    let mut world = TestWorld::default();
    world.add("game_entity", Vec3::new(1.0, 2.0, 3.0));
    let mut mods = started(&mut world);
    mods.drain_emitted();
    let before = world.clone();
    let outcome = reload(&mut mods, &mut world, BREAKS);
    assert_eq!(world.entities, before.entities, "positions back, spawn gone");
    assert_eq!(
        mods.drain_emitted().len(),
        0,
        "events of the dropped version never reach the game"
    );
    assert_rolled_back(&mut mods, &mut world, &outcome, ReloadStage::Entry);
    assert!(
        outcome
            .error
            .is_some_and(|e| e.message.contains("the new version is broken"))
    );
}

/// Audit wave 5 (luasave M2): the old version's `on_save` reads the world through the reload's
/// journal, after the new entry chunk moved an entity. The reload then fails; the world is rolled
/// back, and so is what that `on_save` copied into the old version's `regia.state`.
#[test]
fn a_failed_reload_puts_back_the_old_state_its_on_save_changed() {
    const SAVES_POSITION: &str = r#"
    version_tag = "old"
    function on_start() regia.state.x = 0 end
    function on_save() regia.state.x = regia.find("game_entity"):position().x end
    "#;
    const MOVES_THEN_FAILS: &str = r#"
    regia.find("game_entity"):set_position(regia.vec3(99, 0, 0))
    function on_load() error("no") end
    "#;
    let mut world = TestWorld::default();
    world.add("game_entity", Vec3::new(1.0, 0.0, 0.0));
    let mut mods = host();
    mods.load(mod_files("m", "1.0.0", SAVES_POSITION), &mut world)
        .expect("loads");
    mods.call_tick(&mut world, 0.1);
    assert_eq!(
        eval(&mut mods, &mut world, "return regia.state.x"),
        ScriptValue::Number(0.0)
    );
    let outcome = reload(&mut mods, &mut world, MOVES_THEN_FAILS);
    assert!(!outcome.ok, "{outcome:?}");
    assert_eq!(world.position_of("game_entity"), Some(Vec3::new(1.0, 0.0, 0.0)));
    assert_eq!(
        eval(&mut mods, &mut world, "return regia.state.x"),
        ScriptValue::Number(0.0),
        "the old state is as it was before the reload"
    );
    assert_eq!(
        eval(&mut mods, &mut world, "return version_tag"),
        ScriptValue::String("old".into())
    );
}

/// A trial that attaches and removes a component in one entry chunk, then fails: the rollback
/// must net the two writes to nothing, not re-attach the trial's values (audit wave: journal
/// order, spec 84 § 14).
#[test]
fn a_failed_reload_rolls_back_a_trial_add_then_remove_to_nothing() {
    const ADDS_REMOVES_BREAKS: &str = r#"
        regia.component("mover", { props = { speed = { type = "number", default = 2 } } })
        local e = regia.find("game_entity")
        e:add("mover")
        e:remove("mover")
        error("boom")
    "#;
    let mut world = TestWorld::default();
    let entity = world.add("game_entity", Vec3::ZERO);
    let mut mods = started(&mut world);
    let before = world.clone();
    let outcome = reload(&mut mods, &mut world, ADDS_REMOVES_BREAKS);
    assert!(!outcome.ok, "{outcome:?}");
    assert_eq!(world.entities, before.entities, "the entity is exactly as it was");
    assert!(
        world.entities[&entity].component.is_none(),
        "an add then a remove in one failed trial nets to nothing"
    );
}

/// A trial that edits a component's values and then removes it, in one failed reload: the
/// rollback must restore the component with the values it had before the trial, not the
/// removal-time capture of the trial's edit.
#[test]
fn a_failed_reload_restores_the_values_a_trial_edited_then_removed() {
    const OLD_ATTACHED: &str = r#"
        version_tag = "old"
        regia.component("mover", { props = { speed = { type = "number", default = 1 } } })
        function on_start()
            local e = regia.find("game_entity")
            e:add("mover")
        end
    "#;
    const EDITS_REMOVES_BREAKS: &str = r#"
        version_tag = "new"
        regia.component("mover", { props = { speed = { type = "number", default = 1 } } })
        local e = regia.find("game_entity")
        e:set_prop("mover", "speed", 99)
        e:remove("mover")
        function on_load() error("no") end
    "#;
    let mut world = TestWorld::default();
    let entity = world.add("game_entity", Vec3::ZERO);
    let mut mods = host();
    mods.load(mod_files("m", "1.0.0", OLD_ATTACHED), &mut world)
        .expect("loads");
    assert_eq!(
        world.entities[&entity].component,
        Some((
            "mover".to_owned(),
            vec![("speed".to_owned(), ScriptValue::Number(1.0))]
        )),
        "the old version attached the component"
    );
    let outcome = reload(&mut mods, &mut world, EDITS_REMOVES_BREAKS);
    assert!(!outcome.ok, "{outcome:?}");
    assert_eq!(
        world.entities[&entity].component,
        Some((
            "mover".to_owned(),
            vec![("speed".to_owned(), ScriptValue::Number(1.0))]
        )),
        "the component is back with the pre-trial value, not the trial's 99"
    );
    assert_eq!(
        eval(&mut mods, &mut world, "return version_tag"),
        ScriptValue::String("old".into()),
        "the old version still runs"
    );
    assert!(mods.get("m").is_some_and(LuaMod::is_active));
    let last_error = mods
        .get("m")
        .and_then(|m| m.errors().last().map(|(call, _)| call.to_owned()));
    assert_eq!(last_error.as_deref(), Some("reload"));
}

#[test]
fn a_failing_on_load_rolls_back_at_the_last_stage() {
    const BREAKS: &str = r#"
    function on_load() regia.spawn("from_on_load") error("no") end
    "#;
    let mut world = TestWorld::default();
    let mut mods = started(&mut world);
    let before = world.clone();
    let outcome = reload(&mut mods, &mut world, BREAKS);
    assert_eq!(world.entities, before.entities);
    assert_rolled_back(&mut mods, &mut world, &outcome, ReloadStage::Start);
}

#[test]
fn a_failing_migrator_rolls_back() {
    const BREAKS: &str = r#"
    regia.state_version = 2
    function on_migrate() error("bad data") end
    "#;
    let mut world = TestWorld::default();
    let mut mods = started(&mut world);
    let outcome = reload(&mut mods, &mut world, BREAKS);
    assert_rolled_back(&mut mods, &mut world, &outcome, ReloadStage::State);
    assert_eq!(outcome.error.map(|e| e.code), Some(LuaErrorCode::Runtime));
}

#[test]
fn a_migrator_leaving_a_function_in_the_state_rolls_back() {
    const BREAKS: &str = r#"
    regia.state_version = 2
    function on_migrate() return { f = print } end
    "#;
    let mut world = TestWorld::default();
    let mut mods = started(&mut world);
    let outcome = reload(&mut mods, &mut world, BREAKS);
    assert_rolled_back(&mut mods, &mut world, &outcome, ReloadStage::State);
    assert_eq!(outcome.error.map(|e| e.code), Some(LuaErrorCode::BadValue));
}

#[test]
fn a_bad_state_version_rolls_back() {
    let mut world = TestWorld::default();
    let mut mods = started(&mut world);
    let outcome = reload(&mut mods, &mut world, "regia.state_version = 1.5");
    assert_rolled_back(&mut mods, &mut world, &outcome, ReloadStage::State);
    assert_eq!(outcome.error.map(|e| e.code), Some(LuaErrorCode::BadValue));
}

#[test]
fn the_budgets_hold_for_the_new_version_and_roll_back() {
    let mut world = TestWorld::default();
    let mut mods = started(&mut world);
    let outcome = reload(&mut mods, &mut world, "while true do end");
    assert_rolled_back(&mut mods, &mut world, &outcome, ReloadStage::Entry);
    assert_eq!(
        outcome.error.map(|e| e.code),
        Some(LuaErrorCode::InstructionBudget)
    );
}

#[test]
fn a_memory_bomb_in_the_new_version_rolls_back() {
    let mut world = TestWorld::default();
    let limits = LuaLimits {
        memory_limit_bytes: 1024 * 1024,
        ..LuaLimits::default()
    };
    let mut mods = LuaMods::new(1, limits);
    mods.load(mod_files("m", "1.0.0", OLD), &mut world)
        .expect("loads");
    mods.call_tick(&mut world, 0.1);
    let outcome = reload(
        &mut mods,
        &mut world,
        "local t = {} while true do t[#t + 1] = {} end",
    );
    assert_rolled_back(&mut mods, &mut world, &outcome, ReloadStage::Entry);
    assert!(
        matches!(
            outcome.error.map(|e| e.code),
            Some(LuaErrorCode::Memory | LuaErrorCode::InstructionBudget)
        ),
        "whichever cap trips first"
    );
}

#[test]
fn a_version_change_with_no_migrator_resets_the_state_and_keeps_going() {
    const V2: &str = r#"
    regia.state_version = 2
    regia.state.fresh = true
    function on_load(info) last_state = info.state end
    "#;
    let mut world = TestWorld::default();
    let mut mods = started(&mut world);
    let outcome = reload(&mut mods, &mut world, V2);
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(outcome.state, Some(StateCarry::Reset { from: 1, to: 2 }));
    assert_eq!(
        eval(&mut mods, &mut world, "return regia.state.count"),
        ScriptValue::Nil
    );
    assert_eq!(
        eval(&mut mods, &mut world, "return last_state"),
        ScriptValue::String("reset".into())
    );
}

#[test]
fn a_version_change_with_a_migrator_carries_the_state() {
    const V2: &str = r#"
    regia.state_version = 2
    function on_migrate(old, from, to) return { total = old.count + 100, from = from, to = to } end
    function on_load() end
    "#;
    let mut world = TestWorld::default();
    let mut mods = started(&mut world);
    let outcome = reload(&mut mods, &mut world, V2);
    assert_eq!(outcome.state, Some(StateCarry::Migrated { from: 1, to: 2 }));
    assert_eq!(
        eval(&mut mods, &mut world, "return regia.state.total"),
        ScriptValue::Number(101.0)
    );
    assert_eq!(
        eval(&mut mods, &mut world, "return regia.state.to"),
        ScriptValue::Number(2.0)
    );
}

#[test]
fn a_state_that_is_not_plain_is_reset_with_a_note() {
    let mut world = TestWorld::default();
    let mut mods = started(&mut world);
    eval(&mut mods, &mut world, "regia.state.bad = print");
    let outcome = reload(&mut mods, &mut world, NEW);
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(outcome.state, Some(StateCarry::Reset { from: 1, to: 1 }));
    assert!(outcome.note.is_some_and(|n| n.contains("regia.state.bad")));
}

const SPAWNER: &str = r#"
function on_start() lamp = regia.spawn("lamp", regia.vec3(1, 0, 0)) regia.state.starts = (regia.state.starts or 0) + 1 end
"#;

#[test]
fn entities_stay_with_on_load() {
    let mut world = TestWorld::default();
    let mut mods = host();
    mods.load(mod_files("m", "1", SPAWNER), &mut world)
        .expect("loads");
    let outcome = reload(
        &mut mods,
        &mut world,
        "function on_load() lamp = regia.find('lamp') end",
    );
    assert!(outcome.ok);
    assert_eq!(world.names(), ["lamp"]);
    assert_eq!(
        eval(&mut mods, &mut world, "return lamp:name()"),
        ScriptValue::String("lamp".into())
    );
}

#[test]
fn a_mod_with_no_on_load_restarts_with_its_state_and_a_failure_brings_its_entities_back() {
    let mut world = TestWorld::default();
    let mut mods = host();
    mods.load(mod_files("m", "1", SPAWNER), &mut world)
        .expect("loads");
    eval(&mut mods, &mut world, "lamp:set_position(regia.vec3(7, 7, 7))");

    let breaks = format!("{SPAWNER} local on = on_start function on_start() on() error('no') end");
    let before = world.clone();
    let outcome = reload(&mut mods, &mut world, &breaks);
    assert!(!outcome.ok);
    assert_eq!(
        world.entities, before.entities,
        "the old lamp is still there, the new one is gone"
    );
    assert_eq!(
        eval(&mut mods, &mut world, "return lamp:name()"),
        ScriptValue::String("lamp".into())
    );

    let outcome = reload(&mut mods, &mut world, SPAWNER);
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(world.names(), ["lamp"], "the old one went, on_start made one");
    assert_eq!(world.position_of("lamp"), Some(Vec3::new(1.0, 0.0, 0.0)));
    assert_eq!(
        eval(&mut mods, &mut world, "return regia.state.starts"),
        ScriptValue::Number(2.0)
    );
}

#[test]
fn a_despawn_in_the_new_code_waits_for_the_commit() {
    const DESPAWNS: &str = r#"
    function on_load()
      local e = regia.find("lamp")
      assert(e:despawn())
      assert(not e:alive() and e:position() == nil and regia.find("lamp") == nil)
    end
    "#;
    let mut world = TestWorld::default();
    let mut mods = host();
    mods.load(mod_files("m", "1", SPAWNER), &mut world)
        .expect("loads");
    let before = world.clone();

    let breaks = format!("{DESPAWNS} local f = on_load function on_load() f() error('no') end");
    let outcome = reload(&mut mods, &mut world, &breaks);
    assert!(!outcome.ok);
    assert_eq!(
        world.entities, before.entities,
        "rolled back: the lamp was never removed"
    );

    let outcome = reload(&mut mods, &mut world, DESPAWNS);
    assert!(outcome.ok, "{outcome:?}");
    assert!(world.names().is_empty());
}

#[test]
fn only_the_mods_own_entities_can_go_during_a_reload() {
    const GRABS: &str = r#"
    function on_load()
      assert(regia.find("game_entity"):despawn() == false)
      assert(regia.find("other_lamp"):despawn() == false)
    end
    "#;
    let mut world = TestWorld::default();
    world.add("game_entity", Vec3::ZERO);
    let mut mods = host();
    mods.load(mod_files("m", "1", SPAWNER), &mut world)
        .expect("loads");
    mods.load(
        mod_files("other", "1", "function on_start() regia.spawn('other_lamp') end"),
        &mut world,
    )
    .expect("loads");
    let outcome = reload(&mut mods, &mut world, GRABS);
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(world.names(), ["game_entity", "lamp", "other_lamp"]);
}

/// The old version with an `on_save` that fails after touching the world.
fn old_with_a_broken_on_save() -> String {
    format!(
        "{OLD} function on_save() regia.spawn('in_save') regia.emit('in_save', 1) error('flush failed') end"
    )
}

#[test]
fn a_failing_on_save_cannot_block_the_reload_that_fixes_it() {
    let mut world = TestWorld::default();
    let mut mods = host();
    mods.load(mod_files("m", "1.0.0", &old_with_a_broken_on_save()), &mut world)
        .expect("loads");
    mods.call_tick(&mut world, 0.1);
    let outcome = reload(&mut mods, &mut world, NEW);
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(outcome.state, Some(StateCarry::Carried));
    let note = outcome.note.as_deref().unwrap_or_default();
    assert!(note.contains("flush failed"), "{note}");
    assert_eq!(
        eval(&mut mods, &mut world, "return regia.state.count"),
        ScriptValue::Number(1.0),
        "the state as it stood"
    );
    let calls: Vec<String> = mods
        .get("m")
        .map(|m| m.errors().map(|(call, _)| call.to_owned()).collect())
        .unwrap_or_default();
    assert_eq!(
        calls,
        ["on_save"],
        "the failure is on record, as a failure of on_save"
    );
}

#[test]
fn a_failing_on_save_never_disables_the_version_a_failed_reload_leaves_running() {
    let mut world = TestWorld::default();
    let mut mods = host();
    mods.load(mod_files("m", "1.0.0", &old_with_a_broken_on_save()), &mut world)
        .expect("loads");
    mods.call_tick(&mut world, 0.1);
    let before = world.clone();
    let outcome = reload(&mut mods, &mut world, "function on_load() error('later') end");
    assert_eq!(world.entities, before.entities);
    assert_rolled_back(&mut mods, &mut world, &outcome, ReloadStage::Start);
}

#[test]
fn a_new_version_that_fails_its_entry_chunk_never_runs_the_old_on_save() {
    let mut world = TestWorld::default();
    let code = format!("{OLD} function on_save() regia.state.flushed = true end");
    let mut mods = host();
    mods.load(mod_files("m", "1.0.0", &code), &mut world)
        .expect("loads");
    mods.call_tick(&mut world, 0.1);
    let outcome = reload(&mut mods, &mut world, "error('at the top')");
    assert_rolled_back(&mut mods, &mut world, &outcome, ReloadStage::Entry);
    assert_eq!(
        eval(&mut mods, &mut world, "return regia.state.flushed"),
        ScriptValue::Nil,
        "the old version saw nothing of it"
    );
}

#[test]
fn a_failed_reload_leaves_the_old_versions_random_numbers_alone() {
    let code = format!("{OLD} function on_save() regia.state.roll = math.random() end");
    let mut world = TestWorld::default();
    let mut control = host();
    control
        .load(mod_files("m", "1.0.0", &code), &mut world)
        .expect("loads");
    let mut mods = host();
    mods.load(mod_files("m", "1.0.0", &code), &mut world)
        .expect("loads");
    let outcome = reload(&mut mods, &mut world, "function on_load() error('no') end");
    assert!(!outcome.ok);
    assert_eq!(
        eval(&mut mods, &mut world, "return math.random()"),
        eval(&mut control, &mut world, "return math.random()"),
        "the same next number as a run with no reload"
    );
}

#[test]
fn the_random_numbers_go_on_when_the_state_cannot_cross() {
    let code = "math.random() regia.state.f = print function on_load() end";
    let mut world = TestWorld::default();
    let mut control = host();
    control
        .load(mod_files("m", "1.0.0", code), &mut world)
        .expect("loads");
    let mut mods = host();
    mods.load(mod_files("m", "1.0.0", code), &mut world)
        .expect("loads");
    let outcome = reload(&mut mods, &mut world, "function on_load() end");
    assert_eq!(outcome.state.map(StateCarry::name), Some("reset"));
    assert_eq!(
        eval(&mut mods, &mut world, "return math.random()"),
        eval(&mut control, &mut world, "return math.random()")
    );
}

#[test]
fn a_local_kept_by_the_chunk_stays_the_state_across_a_reload() {
    let chunk = "local s = regia.state s.n = s.n or 0 function on_tick() s.n = s.n + 1 end function same() return s == regia.state end function on_load() end";
    let mut world = TestWorld::default();
    let mut mods = host();
    mods.load(mod_files("m", "1.0.0", chunk), &mut world)
        .expect("loads");
    mods.call_tick(&mut world, 0.1);
    assert!(reload(&mut mods, &mut world, chunk).ok);
    assert_eq!(
        eval(&mut mods, &mut world, "return same()"),
        ScriptValue::Bool(true)
    );
    mods.call_tick(&mut world, 0.1);
    assert_eq!(
        eval(&mut mods, &mut world, "return regia.state.n"),
        ScriptValue::Number(2.0),
        "the tick reached the state a save reads"
    );
}

#[test]
fn on_save_of_the_old_version_runs_before_the_state_crosses() {
    let mut world = TestWorld::default();
    let code = "local live = 41 function on_save() regia.state.live = live + 1 regia.emit('flushed', 1) end function on_load() end";
    let mut mods = host();
    mods.load(mod_files("m", "1", code), &mut world).expect("loads");
    let outcome = reload(&mut mods, &mut world, "function on_load() end");
    assert!(outcome.ok);
    assert_eq!(
        mods.drain_emitted().len(),
        1,
        "the old version's last event reaches the game"
    );
    assert_eq!(
        eval(&mut mods, &mut world, "return regia.state.live"),
        ScriptValue::Number(42.0)
    );
}

#[test]
fn a_disabled_mod_comes_back_with_a_good_reload_and_stays_down_with_a_bad_one() {
    let mut world = TestWorld::default();
    let code = "function on_start() regia.state.n = 5 end function on_tick() error('bug') end function on_load() end";
    let mut mods = host();
    mods.load(mod_files("m", "1", code), &mut world).expect("loads");
    mods.call_tick(&mut world, 0.1);
    assert!(mods.get("m").is_some_and(|m| !m.is_active()));

    let outcome = reload(&mut mods, &mut world, "syntax error here");
    assert!(!outcome.ok);
    assert!(matches!(
        mods.get("m").map(LuaMod::status),
        Some(ModStatus::Disabled {
            code: LuaErrorCode::Runtime,
            ..
        })
    ));

    let outcome = reload(
        &mut mods,
        &mut world,
        "function on_load() end function on_tick() end",
    );
    assert!(outcome.ok, "{outcome:?}");
    assert!(mods.get("m").is_some_and(LuaMod::is_active));
    assert_eq!(
        eval(&mut mods, &mut world, "return regia.state.n"),
        ScriptValue::Number(5.0)
    );
    mods.call_tick(&mut world, 0.1);
    assert!(mods.get("m").is_some_and(LuaMod::is_active), "the fix holds");
    assert_eq!(
        mods.get("m").map(|m| m.errors().count()),
        Some(2),
        "the history stays"
    );
}

#[test]
fn bad_files_are_refused_before_anything_runs() {
    let mut world = TestWorld::default();
    let mut mods = started(&mut world);
    let check = |mods: &mut LuaMods, world: &mut TestWorld, files: ModFiles, code: LuaErrorCode| {
        let outcome = mods.reload("m", Some(files), world).expect("exists");
        assert!(!outcome.ok);
        assert_eq!(outcome.stage, ReloadStage::Files, "{outcome:?}");
        assert_eq!(outcome.error.map(|e| e.code), Some(code));
    };
    check(
        &mut mods,
        &mut world,
        mod_files("other", "1", NEW),
        LuaErrorCode::Manifest,
    );
    let no_manifest = ModFiles::from_map([("main.lua", NEW)]);
    check(&mut mods, &mut world, no_manifest, LuaErrorCode::Manifest);
    let no_entry = ModFiles::from_map([("luamod.ron", manifest("m", "2"))]);
    check(&mut mods, &mut world, no_entry, LuaErrorCode::Manifest);
    let huge = mod_files("m", "2", &"x".repeat(5 * 1024 * 1024));
    check(&mut mods, &mut world, huge, LuaErrorCode::TooLarge);
    assert_eq!(mods.get("m").map(|m| m.counters().failed_reloads), Some(4));

    let unknown = mods.reload("nobody", None, &mut world).map_err(|e| e.code);
    assert_eq!(unknown.err(), Some(LuaErrorCode::UnknownMod));
    let no_dir = mods.reload("m", None, &mut world).expect("exists");
    assert_eq!(no_dir.error.map(|e| e.code), Some(LuaErrorCode::Io));
}

#[test]
fn many_reloads_leave_one_state_and_the_same_memory() {
    let mut world = TestWorld::default();
    let mut mods = started(&mut world);
    for round in 0..200 {
        let code = if round % 2 == 0 { NEW } else { "broken(" };
        let outcome = reload(&mut mods, &mut world, code);
        assert_eq!(outcome.ok, round % 2 == 0);
    }
    let counters = mods.get("m").map(LuaMod::counters).unwrap_or_default();
    assert_eq!((counters.reloads, counters.failed_reloads), (100, 100));
    assert!(counters.memory_bytes < 256 * 1024, "{counters:?}");
    assert_eq!(
        eval(&mut mods, &mut world, "return regia.state.count"),
        ScriptValue::Number(1.0)
    );
}

fn write_mod(dir: &std::path::Path, main: &str) {
    std::fs::create_dir_all(dir).expect("mkdir");
    std::fs::write(dir.join("luamod.ron"), manifest("m", "1.0.0")).expect("write");
    let tmp = dir.join("main.lua.tmp");
    std::fs::write(&tmp, main).expect("write");
    std::fs::rename(&tmp, dir.join("main.lua")).expect("rename");
}

#[test]
fn a_directory_mod_reloads_from_disk() {
    let dir = regia_test_utils::unique_temp_dir("lua_reload_dir");
    write_mod(&dir, OLD);
    let mut world = TestWorld::default();
    let mut mods = host();
    let files = ModFiles::from_dir(&dir, 1024 * 1024).expect("reads");
    mods.load(files, &mut world).expect("loads");
    mods.call_tick(&mut world, 0.1);
    write_mod(&dir, NEW);
    let outcome = mods.reload("m", None, &mut world).expect("exists");
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(
        eval(&mut mods, &mut world, "return regia.state.count"),
        ScriptValue::Number(1.0)
    );
    assert_eq!(
        eval(&mut mods, &mut world, "return version_tag"),
        ScriptValue::String("new".into())
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn the_watch_reloads_on_change_reports_a_bad_save_once_and_recovers() {
    let dir = regia_test_utils::unique_temp_dir("lua_reload_watch");
    write_mod(&dir, OLD);
    let mut world = TestWorld::default();
    let mut mods = host();
    let files = ModFiles::from_dir(&dir, 1024 * 1024).expect("reads");
    mods.load(files, &mut world).expect("loads");
    mods.call_tick(&mut world, 0.1);

    assert!(mods.poll_files(&mut world).is_empty(), "nothing changed");
    write_mod(&dir, "broken(");
    let outcomes = mods.poll_files(&mut world);
    assert_eq!(outcomes.len(), 1);
    assert!(!outcomes[0].ok);
    assert!(
        mods.poll_files(&mut world).is_empty(),
        "the same bad files report once"
    );
    assert_eq!(
        eval(&mut mods, &mut world, "return version_tag"),
        ScriptValue::String("old".into())
    );

    write_mod(&dir, NEW);
    let outcomes = mods.poll_files(&mut world);
    assert_eq!(outcomes.len(), 1);
    assert!(outcomes[0].ok, "{:?}", outcomes[0]);
    assert!(mods.poll_files(&mut world).is_empty());
    assert_eq!(
        eval(&mut mods, &mut world, "return version_tag"),
        ScriptValue::String("new".into())
    );

    // A directory that went away leaves the running version alone.
    std::fs::remove_dir_all(&dir).ok();
    assert!(mods.poll_files(&mut world).is_empty());
    assert!(mods.get("m").is_some_and(LuaMod::is_active));
}

#[test]
fn the_watch_looks_every_n_frames_and_is_off_by_default() {
    let dir = regia_test_utils::unique_temp_dir("lua_reload_every");
    write_mod(&dir, OLD);
    let mut world = TestWorld::default();
    let mut mods = host();
    let files = ModFiles::from_dir(&dir, 1024 * 1024).expect("reads");
    mods.load(files, &mut world).expect("loads");
    write_mod(&dir, NEW);
    for _ in 0..10 {
        assert!(mods.tick_watch(&mut world).is_empty(), "off");
    }
    mods.set_watch(3);
    assert_eq!(mods.watch_every(), 3);
    assert!(mods.tick_watch(&mut world).is_empty());
    assert!(mods.tick_watch(&mut world).is_empty());
    assert_eq!(mods.tick_watch(&mut world).len(), 1, "third frame");
    write_mod(&dir, OLD);
    assert!(mods.tick_watch(&mut world).is_empty());
    assert!(mods.tick_watch(&mut world).is_empty());
    assert_eq!(mods.tick_watch(&mut world).len(), 1);
    std::fs::remove_dir_all(&dir).ok();
}

/// An unchanged directory is recognized from size and mtime. Bodies are read only when those change.
#[test]
fn an_unchanged_directory_is_not_reread() {
    let dir = regia_test_utils::unique_temp_dir("lua_watch_stamp");
    write_mod(&dir, OLD);
    let mut world = TestWorld::default();
    let mut mods = host();
    let files = ModFiles::from_dir(&dir, 1024 * 1024).expect("reads");
    mods.load(files, &mut world).expect("loads");
    reset_mod_file_body_reads();
    assert!(mods.poll_files(&mut world).is_empty(), "nothing changed");
    assert_eq!(
        mod_file_body_reads(),
        0,
        "an unchanged directory reads no file bodies"
    );
    write_mod(&dir, NEW);
    let outcomes = mods.poll_files(&mut world);
    assert_eq!(outcomes.len(), 1, "a content change still reloads");
    assert!(outcomes[0].ok, "{:?}", outcomes[0]);
    assert!(
        mod_file_body_reads() > 0,
        "a real change still reads the file bodies"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Moves the modification time of every file `write_mod` wrote, text unchanged.
fn touch_mod(dir: &std::path::Path, seconds_ahead: u64) {
    let when = std::time::SystemTime::now() + std::time::Duration::from_secs(seconds_ahead);
    for name in ["luamod.ron", "main.lua"] {
        let file = std::fs::File::options()
            .write(true)
            .open(dir.join(name))
            .expect("open for touch");
        file.set_modified(when).expect("set mtime");
    }
}

/// A save that changes no text moves the stamp. The watch reads the bodies once, finds the same
/// text, and peeks again from the next poll on, for the running files and for a rejected save
/// alike. Before the fix the old stamp stayed and every later poll reread every body.
#[test]
fn an_unchanged_save_reads_the_bodies_once() {
    let dir = regia_test_utils::unique_temp_dir("lua_watch_touch");
    write_mod(&dir, OLD);
    let mut world = TestWorld::default();
    let mut mods = host();
    let files = ModFiles::from_dir(&dir, 1024 * 1024).expect("reads");
    mods.load(files, &mut world).expect("loads");
    touch_mod(&dir, 10);
    reset_mod_file_body_reads();
    assert!(mods.poll_files(&mut world).is_empty(), "same text, no reload");
    assert!(mod_file_body_reads() > 0, "the moved stamp reads the bodies once");
    reset_mod_file_body_reads();
    assert!(mods.poll_files(&mut world).is_empty());
    assert_eq!(mod_file_body_reads(), 0, "the refreshed stamp peeks again");

    write_mod(&dir, "this is not lua (");
    let outcomes = mods.poll_files(&mut world);
    assert_eq!(outcomes.len(), 1);
    assert!(!outcomes[0].ok, "a broken save is rejected: {:?}", outcomes[0]);
    touch_mod(&dir, 20);
    reset_mod_file_body_reads();
    assert!(
        mods.poll_files(&mut world).is_empty(),
        "the rejected text is not retried"
    );
    assert!(mod_file_body_reads() > 0);
    reset_mod_file_body_reads();
    assert!(mods.poll_files(&mut world).is_empty());
    assert_eq!(
        mod_file_body_reads(),
        0,
        "the rejected save's refreshed stamp peeks again"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_mod_in_memory_is_never_watched() {
    let mut mods = host();
    mods.load(mod_files("m", "1", OLD), &mut NoWorld).expect("loads");
    assert!(mods.poll_files(&mut NoWorld).is_empty());
}
