// SPDX-License-Identifier: MPL-2.0
//! Sandbox, budget, isolation, determinism and host-API tests (`docs/specs/84-lua-mods.md` § 7).
//! No GPU, no goldens, no clocks: every budget is an instruction count or a byte count.

use std::collections::BTreeMap;

use regia_core::math::Vec3;
use regia_script_lua::{
    LuaErrorCode, LuaLimits, LuaMods, MAX_ERRORS_KEPT, ModFiles, ModStatus, ModWorld, NoWorld, ScriptValue,
};

fn manifest(id: &str, extra: &str) -> String {
    format!(
        r#"(format_version: 1, id: "{id}", name: "{id}", version: "1.0.0", entry: "main.lua", api_version: 1, {extra})"#
    )
}

fn mod_files(id: &str, main: &str) -> ModFiles {
    ModFiles::from_map([
        ("luamod.ron".to_owned(), manifest(id, "")),
        ("main.lua".to_owned(), main.to_owned()),
    ])
}

fn host() -> LuaMods {
    LuaMods::new(1234, LuaLimits::default())
}

/// Runs `chunk` in a throwaway sandbox and returns its first result.
fn eval(chunk: &str) -> ScriptValue {
    let outcome = host()
        .run_chunk(&mut NoWorld, None, chunk)
        .expect("scratch state");
    assert!(outcome.ok, "{chunk}: {:?}", outcome.error);
    outcome.results.into_iter().next().unwrap_or_default()
}

fn eval_err(chunk: &str) -> LuaErrorCode {
    let outcome = host()
        .run_chunk(&mut NoWorld, None, chunk)
        .expect("scratch state");
    assert!(!outcome.ok, "{chunk} should fail");
    outcome.error.expect("an error").code
}

#[test]
fn entity_id_is_index_v_generation() {
    let mut world = TestWorld::default();
    world.add("first", None);
    // Slot 1, generation 1.
    let bits = world.add("probe", None);
    assert_eq!(bits, (1u64 << 32) | 1);
    let mut mods = host();
    mods.load(mod_files("id", "function on_start() end"), &mut world)
        .expect("loads");
    let outcome = mods
        .run_chunk(&mut world, Some("id"), r#"return regia.find("probe"):id()"#)
        .expect("runs");
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(outcome.results, vec![ScriptValue::String("1v1".into())]);
}

#[test]
fn dangerous_libraries_and_functions_are_absent() {
    for name in [
        "io",
        "os",
        "debug",
        "package",
        "load",
        "loadstring",
        "dofile",
        "loadfile",
        "module",
        "newproxy",
        "gcinfo",
    ] {
        assert_eq!(
            eval(&format!("return {name} == nil")),
            ScriptValue::Bool(true),
            "{name}"
        );
    }
    assert_eq!(eval("return string.dump == nil"), ScriptValue::Bool(true));
    for name in [
        "print",
        "pcall",
        "xpcall",
        "pairs",
        "ipairs",
        "tostring",
        "setmetatable",
        "require",
    ] {
        assert_eq!(
            eval(&format!("return type({name})")),
            ScriptValue::String("function".into()),
            "{name}"
        );
    }
    for lib in ["string", "table", "math", "coroutine", "regia"] {
        assert_eq!(
            eval(&format!("return type({lib})")),
            ScriptValue::String("table".into()),
            "{lib}"
        );
    }
}

#[test]
fn collectgarbage_answers_count_only() {
    assert!(matches!(eval("return collectgarbage('count')"), ScriptValue::Number(n) if n > 0.0));
    assert_eq!(eval_err("collectgarbage('stop')"), LuaErrorCode::Runtime);
}

#[test]
fn precompiled_chunks_are_refused() {
    // "\27Lua" is the 5.1 bytecode signature; a text-mode load refuses it.
    let mut mods = host();
    let outcome = mods
        .run_chunk(&mut NoWorld, None, "\u{1b}Lua\u{51}\u{0}\u{1}\u{4}")
        .expect("scratch");
    assert_eq!(outcome.error.map(|e| e.code), Some(LuaErrorCode::Syntax));
    let files = ModFiles::from_map([
        ("luamod.ron".to_owned(), manifest("bytecode", "")),
        ("main.lua".to_owned(), "\u{1b}Lua\u{51}".to_owned()),
    ]);
    let err = mods.load(files, &mut NoWorld).expect_err("refused");
    assert_eq!(err.code, LuaErrorCode::Syntax);
}

#[test]
fn require_reaches_the_mods_own_files_only() {
    let files = ModFiles::from_map([
        ("luamod.ron".to_owned(), manifest("req", "")),
        (
            "main.lua".to_owned(),
            r#"
            local util = require("lib.util")
            local again = require("lib/util")
            assert(util == again, "require caches")
            result = util.double(21)
            ok_escape = pcall(require, "../secret")
            ok_missing = pcall(require, "nope")
            ok_self = pcall(require, "lib.selfish")
            "#
            .to_owned(),
        ),
        (
            "lib/util.lua".to_owned(),
            "local M = {} function M.double(x) return 2 * x end return M".to_owned(),
        ),
        (
            "lib/selfish.lua".to_owned(),
            "return require('lib.selfish')".to_owned(),
        ),
    ]);
    let mut mods = host();
    mods.load(files, &mut NoWorld).expect("loads");
    let globals = mods.globals("req", None, 512).expect("globals");
    let get = |k: &str| globals.iter().find(|g| g.key == k).and_then(|g| g.value.clone());
    assert_eq!(get("result"), Some(ScriptValue::Number(42.0)));
    assert_eq!(get("ok_escape"), Some(ScriptValue::Bool(false)));
    assert_eq!(get("ok_missing"), Some(ScriptValue::Bool(false)));
    assert_eq!(get("ok_self"), Some(ScriptValue::Bool(false)));
    let uncaught = mods
        .run_chunk(&mut NoWorld, Some("req"), "require('missing')")
        .expect("runs");
    assert_eq!(uncaught.error.map(|e| e.code), Some(LuaErrorCode::Require));
}

#[test]
fn an_infinite_loop_stops_at_the_instruction_budget() {
    assert_eq!(eval_err("while true do end"), LuaErrorCode::InstructionBudget);
}

#[test]
fn pcall_xpcall_and_coroutines_cannot_swallow_the_budget() {
    for chunk in [
        "while true do pcall(function() while true do end end) end",
        "while true do xpcall(function() while true do end end, function(e) return 'swallowed' end) end",
        "local co = coroutine.create(function() while true do end end) while true do coroutine.resume(co) end",
        "local f = coroutine.wrap(function() while true do end end) f()",
        "local co = coroutine.create(function() while true do end end) local ok = coroutine.resume(co) return ok",
        "local function f() return pcall(f) end while true do f() end",
    ] {
        assert_eq!(eval_err(chunk), LuaErrorCode::InstructionBudget, "{chunk}");
    }
}

#[test]
fn protected_calls_return_many_values_and_keep_nils() {
    // Lua 5.1's own pcall would overrun its C frame here; the wrappers pack the results.
    let many = "local t = {} for i = 1, 300 do t[i] = i end";
    assert_eq!(
        eval(&format!(
            "{many} return select('#', pcall(function() return unpack(t) end))"
        )),
        ScriptValue::Number(301.0)
    );
    assert_eq!(
        eval(&format!(
            "{many} return select('#', xpcall(function() return unpack(t) end, print))"
        )),
        ScriptValue::Number(301.0)
    );
    assert_eq!(
        eval("return select('#', pcall(function() return 1, nil, 3, nil end))"),
        ScriptValue::Number(5.0)
    );
    assert_eq!(
        eval("local ok, a, b = pcall(function(x, y) return y, x end, 'p', 'q') return ok and a .. b"),
        ScriptValue::String("qp".into())
    );
    assert_eq!(
        eval("local ok, e = pcall(error, 'boom', 0) return (not ok) and e"),
        ScriptValue::String("boom".into())
    );
    assert_eq!(
        eval(
            "local ok, e = xpcall(function() error('x', 0) end, function(m) return m .. '!' end) return (not ok) and e"
        ),
        ScriptValue::String("x!".into())
    );
    // Deep recursion through pcall ends in an ordinary, catchable error (no abort).
    assert_eq!(
        eval("local function f(n) local ok = pcall(f, n + 1) return n end return f(0)"),
        ScriptValue::Number(0.0)
    );
}

#[test]
fn a_growing_table_stops_at_the_memory_cap_even_under_pcall() {
    let limits = LuaLimits {
        memory_limit_bytes: 1024 * 1024,
        instruction_budget: 100_000_000,
        ..LuaLimits::default()
    };
    for chunk in [
        "local t = {} for i = 1, 1e9 do t[i] = {i} end",
        "local t = {} while true do pcall(function() for i = 1, 1e9 do t[#t + 1] = string.rep('x', 64) .. i end end) end",
        "local s = string.rep('x', 1e8)",
    ] {
        let mut mods = LuaMods::new(0, limits);
        let outcome = mods.run_chunk(&mut NoWorld, None, chunk).expect("scratch");
        assert_eq!(
            outcome.error.map(|e| e.code),
            Some(LuaErrorCode::Memory),
            "{chunk}"
        );
    }
}

#[test]
fn a_failing_mod_is_disabled_and_the_others_keep_running() {
    let mut mods = host();
    mods.load(
        mod_files(
            "a.good",
            "n = 0 function on_tick() n = n + 1 regia.emit('tick', n) end",
        ),
        &mut NoWorld,
    )
    .expect("good loads");
    mods.load(
        mod_files("b.bad", "function on_tick() error('boom') end"),
        &mut NoWorld,
    )
    .expect("bad loads");
    mods.load(
        mod_files("c.loop", "function on_tick() while true do end end"),
        &mut NoWorld,
    )
    .expect("loop loads");
    for _ in 0..3 {
        mods.call_tick(&mut NoWorld, 1.0 / 60.0);
    }
    let good = mods.get("a.good").expect("a");
    assert!(good.is_active());
    assert_eq!(good.counters().calls, 4, "load + three ticks");
    let bad = mods.get("b.bad").expect("b");
    assert!(matches!(
        bad.status(),
        ModStatus::Disabled {
            code: LuaErrorCode::Runtime,
            ..
        }
    ));
    assert_eq!(bad.counters().calls, 2, "load + the failing tick, then skipped");
    let looping = mods.get("c.loop").expect("c");
    assert!(matches!(
        looping.status(),
        ModStatus::Disabled {
            code: LuaErrorCode::InstructionBudget,
            ..
        }
    ));
    assert_eq!(looping.errors().count(), 1);
    let ticks: Vec<_> = mods.drain_emitted().into_iter().map(|e| e.value).collect();
    assert_eq!(ticks, [1.0, 2.0, 3.0].map(ScriptValue::Number));
    // A disabled mod refuses agent chunks with lua_disabled.
    let refused = mods
        .run_chunk(&mut NoWorld, Some("b.bad"), "return 1")
        .expect("runs");
    assert_eq!(refused.error.map(|e| e.code), Some(LuaErrorCode::Disabled));
}

#[test]
fn restart_rebuilds_a_disabled_mod_from_its_files() {
    let mut mods = host();
    mods.load(
        mod_files(
            "flaky",
            "count = (count or 0) + 1 function on_tick() error('once') end",
        ),
        &mut NoWorld,
    )
    .expect("loads");
    mods.call_tick(&mut NoWorld, 0.1);
    assert!(!mods.get("flaky").expect("mod").is_active());
    mods.restart("flaky", &mut NoWorld).expect("restarts");
    let m = mods.get("flaky").expect("mod");
    assert!(m.is_active());
    assert_eq!(m.errors().count(), 1, "the error ring survives a restart");
    let g = mods.globals("flaky", None, 512).expect("globals");
    assert_eq!(
        g.iter().find(|e| e.key == "count").and_then(|e| e.value.clone()),
        Some(ScriptValue::Number(1.0)),
        "a fresh state"
    );
    assert_eq!(
        mods.restart("missing", &mut NoWorld).map_err(|e| e.code),
        Err(LuaErrorCode::UnknownMod)
    );
}

#[test]
fn random_numbers_are_deterministic_per_seed_and_mod() {
    let src = "out = {} for i = 1, 8 do out[i] = math.random(1, 1000) end
               math.randomseed(99) r1 = math.random() math.randomseed(99) r2 = math.random()
               f = regia.random() k = math.random(6)";
    let run = |seed: u64, id: &str| {
        let mut mods = LuaMods::new(seed, LuaLimits::default());
        mods.load(mod_files(id, src), &mut NoWorld).expect("loads");
        mods.globals(id, None, 512)
            .expect("globals")
            .into_iter()
            .filter(|g| ["r1", "r2", "f", "k"].contains(&g.key.as_str()))
            .map(|g| (g.key, g.value))
            .collect::<Vec<_>>()
    };
    let list = |seed: u64, id: &str| {
        let mut mods = LuaMods::new(seed, LuaLimits::default());
        mods.load(mod_files(id, src), &mut NoWorld).expect("loads");
        mods.run_chunk(&mut NoWorld, Some(id), "return out")
            .expect("runs")
            .results
    };
    assert_eq!(run(5, "m.one"), run(5, "m.one"));
    assert_eq!(list(5, "m.one"), list(5, "m.one"));
    assert_ne!(list(5, "m.one"), list(6, "m.one"), "the seed matters");
    assert_ne!(list(5, "m.one"), list(5, "m.two"), "the mod id is the stream");
    let values = run(5, "m.one");
    let get = |k: &str| {
        values
            .iter()
            .find(|(key, _)| key == k)
            .and_then(|(_, v)| v.clone())
    };
    assert_eq!(get("r1"), get("r2"), "randomseed restarts the sequence");
    assert!(matches!(get("f"), Some(ScriptValue::Number(f)) if (0.0..1.0).contains(&f)));
    assert!(matches!(get("k"), Some(ScriptValue::Number(k)) if (1.0..=6.0).contains(&k) && k.fract() == 0.0));
    assert_eq!(eval_err("return math.random(5, 1)"), LuaErrorCode::Runtime);
}

#[test]
fn events_reach_subscribers_in_order_and_emits_reach_the_game() {
    let mut mods = host();
    mods.load(
        mod_files(
            "listener",
            "regia.on('hit', function(v) regia.emit('seen', { who = 'first', dmg = v.dmg }) end)
             regia.on('hit', function(v) regia.emit('seen', { who = 'second', dmg = v.dmg * 2 }) end)",
        ),
        &mut NoWorld,
    )
    .expect("loads");
    let mut payload = BTreeMap::new();
    payload.insert("dmg".to_owned(), ScriptValue::Number(5.0));
    mods.fire("hit", ScriptValue::Map(payload));
    mods.fire("ignored", ScriptValue::Nil);
    mods.deliver_events(&mut NoWorld);
    let seen: Vec<String> = mods
        .drain_emitted()
        .into_iter()
        .map(|e| {
            format!(
                "{}:{}",
                e.name,
                serde_json::to_string(&e.value.to_json()).unwrap_or_default()
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            r#"seen:{"dmg":5.0,"who":"first"}"#,
            r#"seen:{"dmg":10.0,"who":"second"}"#
        ]
    );
    assert_eq!(mods.get("listener").expect("mod").subscriptions(), ["hit"]);
}

#[test]
fn emits_with_functions_or_too_many_events_fail_as_values() {
    assert_eq!(
        eval_err("regia.emit('x', function() end)"),
        LuaErrorCode::BadValue
    );
    assert_eq!(eval_err("regia.emit('', 1)"), LuaErrorCode::BadValue);
    assert_eq!(
        eval_err("for i = 1, 1000 do regia.emit('spam', i) end"),
        LuaErrorCode::Runtime
    );
}

/// A world with a few named entities, generation-checked like the ECS.
#[derive(Default)]
struct TestWorld {
    /// Slot → (generation, alive, name, position, owner).
    slots: Vec<(u32, bool, String, Vec3, Option<String>)>,
}

impl TestWorld {
    fn bits(index: usize, generation: u32) -> u64 {
        index as u64 | (u64::from(generation) << 32)
    }
    fn add(&mut self, name: &str, owner: Option<&str>) -> u64 {
        if let Some(i) = self.slots.iter().position(|s| !s.1) {
            let s = &mut self.slots[i];
            s.0 += 1;
            *s = (s.0, true, name.to_owned(), Vec3::ZERO, owner.map(str::to_owned));
            return Self::bits(i, s.0);
        }
        self.slots
            .push((1, true, name.to_owned(), Vec3::ZERO, owner.map(str::to_owned)));
        Self::bits(self.slots.len() - 1, 1)
    }
    fn slot(&mut self, bits: u64) -> Option<&mut (u32, bool, String, Vec3, Option<String>)> {
        let index = (bits & 0xffff_ffff) as usize;
        let generation = (bits >> 32) as u32;
        self.slots.get_mut(index).filter(|s| s.1 && s.0 == generation)
    }
    fn kill(&mut self, bits: u64) {
        if let Some(s) = self.slot(bits) {
            s.1 = false;
        }
    }
}

impl ModWorld for TestWorld {
    fn find(&mut self, name: &str) -> Option<u64> {
        let i = self.slots.iter().position(|s| s.1 && s.2 == name)?;
        Some(Self::bits(i, self.slots[i].0))
    }
    fn alive(&mut self, entity: u64) -> bool {
        self.slot(entity).is_some()
    }
    fn name(&mut self, entity: u64) -> Option<String> {
        self.slot(entity).map(|s| s.2.clone())
    }
    fn position(&mut self, entity: u64) -> Option<Vec3> {
        self.slot(entity).map(|s| s.3)
    }
    fn set_position(&mut self, entity: u64, position: Vec3) -> bool {
        self.slot(entity).map(|s| s.3 = position).is_some()
    }
    fn spawn(&mut self, owner: &str, name: &str, position: Vec3) -> Option<u64> {
        let bits = self.add(name, Some(owner));
        self.set_position(bits, position);
        Some(bits)
    }
    fn despawn(&mut self, owner: &str, entity: u64) -> bool {
        let owned = self.slot(entity).is_some_and(|s| s.4.as_deref() == Some(owner));
        if owned {
            self.kill(entity);
        }
        owned
    }
}

#[test]
fn entity_handles_go_stale_safely_and_mods_despawn_only_their_own() {
    let mut world = TestWorld::default();
    let door = world.add("door", None);
    let mut mods = host();
    mods.load(
        mod_files(
            "handles",
            "door = regia.find('door')
             assert(door:alive() and door:name() == 'door')
             assert(door:set_position(regia.vec3(1, 2, 3)))
             assert(door:position() == regia.vec3(1, 2, 3))
             assert(not door:despawn(), 'a game entity is not the mod own')
             mine = regia.spawn('lamp', regia.vec3(0, 5, 0))
             assert(mine:position().y == 5)
             assert(regia.find('nobody') == nil)",
        ),
        &mut world,
    )
    .expect("loads");
    assert_eq!(world.position(door), Some(Vec3::new(1.0, 2.0, 3.0)));
    // Kill the door and reuse its slot with a new generation.
    world.kill(door);
    let reused = world.add("door", None);
    assert_eq!(reused & 0xffff_ffff, door & 0xffff_ffff, "same slot");
    let outcome = mods
        .run_chunk(
            &mut world,
            Some("handles"),
            "return door:alive(), door:name(), door:position(), door:set_position(regia.vec3(9, 9, 9)),
                    mine:despawn(), mine:alive(), tostring(regia.find('door') == door)",
        )
        .expect("runs");
    assert!(outcome.ok, "{:?}", outcome.error);
    assert_eq!(
        outcome.results,
        [
            ScriptValue::Bool(false),
            ScriptValue::Nil,
            ScriptValue::Nil,
            ScriptValue::Bool(false),
            ScriptValue::Bool(true),
            ScriptValue::Bool(false),
            ScriptValue::String("false".into()),
        ]
    );
    assert_eq!(
        world.position(reused),
        Some(Vec3::ZERO),
        "the new entity was not touched"
    );
    // Outside a call there is no world: handles kept in globals answer false/nil.
    let outcome = mods
        .run_chunk(&mut NoWorld, Some("handles"), "return door:alive()")
        .expect("runs");
    assert_eq!(outcome.results, [ScriptValue::Bool(false)]);
}

#[test]
fn spawns_per_call_are_capped() {
    let mut world = TestWorld::default();
    let mut mods = host();
    let outcome = mods
        .run_chunk(&mut world, None, "for i = 1, 100 do regia.spawn('x' .. i) end")
        .expect("scratch");
    assert_eq!(outcome.error.map(|e| e.code), Some(LuaErrorCode::Runtime));
    assert_eq!(world.slots.len(), 64);
}

/// Finding 6 (`sprint-deep-zai-script-lua`): a table holding both a number key and its text form
/// (`{[1] = 'a', ['1'] = 'b'}`) listed the key `"1"` twice in `lua.globals`. One key text, one
/// row, and the limit bounds distinct keys.
#[test]
fn globals_lists_a_key_once_even_when_number_and_text_forms_collide() {
    let mut mods = host();
    mods.load(mod_files("dupe", "t = { [1] = 'a', ['1'] = 'b' }"), &mut NoWorld)
        .expect("loads");
    let entries = mods.globals("dupe", Some("t"), 512).expect("lists");
    let ones = entries.iter().filter(|entry| entry.key == "1").count();
    assert_eq!(ones, 1, "{entries:?}");
    let mut keys: Vec<&str> = entries.iter().map(|entry| entry.key.as_str()).collect();
    keys.sort_unstable();
    let distinct = keys.len();
    keys.dedup();
    assert_eq!(keys.len(), distinct, "no key is listed twice: {entries:?}");
}

/// Finding 7 (`sprint-deep-zai-script-lua`): `lua.run` against a disabled mod never ran the
/// chunk, but reported the mod's last real call's instruction count and memory as if it had. A
/// call the host refused before it ran reports zero.
#[test]
fn a_refused_chunk_against_a_disabled_mod_reports_zero_counters() {
    let mut mods = host();
    mods.load(
        mod_files("stale", "function on_tick() error('boom') end"),
        &mut NoWorld,
    )
    .expect("loads");
    let spent = mods
        .run_chunk(
            &mut NoWorld,
            Some("stale"),
            "local s = 0 for i = 1, 2000 do s = s + i end return s",
        )
        .expect("runs");
    assert!(spent.ok, "{spent:?}");
    assert!(spent.instructions > 0, "the chunk used instructions");
    // The disabling call burns instructions before it fails, so both counters hold real values.
    let disabling = mods
        .run_chunk(
            &mut NoWorld,
            Some("stale"),
            "local s = 0 for i = 1, 2000 do s = s + i end error('boom')",
        )
        .expect("runs");
    assert!(!disabling.ok, "{disabling:?}");
    assert!(disabling.instructions > 0, "the disabling call used instructions");
    assert!(
        disabling.memory_bytes > 0,
        "the disabling call left memory in the state"
    );
    assert!(!mods.get("stale").expect("mod").is_active());
    let refused = mods
        .run_chunk(&mut NoWorld, Some("stale"), "return 1")
        .expect("runs");
    assert!(!refused.ok);
    assert_eq!(refused.error.map(|e| e.code), Some(LuaErrorCode::Disabled));
    assert_eq!(refused.instructions, 0, "a refused call used nothing");
    assert_eq!(refused.memory_bytes, 0, "a refused call allocated nothing");
}

/// Finding 8 (`sprint-deep-zai-script-lua`): probing a disabled mod must not fill its error ring
/// with `lua_disabled` repeats and scroll the disabling error out of `lua.errors`.
#[test]
fn probing_a_disabled_mod_keeps_the_disabling_error_in_the_ring() {
    let mut mods = host();
    mods.load(
        mod_files("noisy", "function on_tick() error('boom') end"),
        &mut NoWorld,
    )
    .expect("loads");
    mods.call_tick(&mut NoWorld, 0.1);
    assert!(!mods.get("noisy").expect("mod").is_active());
    for _ in 0..MAX_ERRORS_KEPT + 8 {
        let _ = mods.run_chunk(&mut NoWorld, Some("noisy"), "return 1");
    }
    let module = mods.get("noisy").expect("mod");
    let errors: Vec<_> = module.errors().collect();
    assert!(
        errors
            .iter()
            .any(|(_, e)| e.code == LuaErrorCode::Runtime && e.message.contains("boom")),
        "the disabling error is still in the ring: {errors:?}"
    );
    assert_eq!(
        errors
            .iter()
            .filter(|(_, e)| e.code == LuaErrorCode::Disabled)
            .count(),
        0,
        "no lua_disabled repeats: {errors:?}"
    );
}

#[test]
fn print_and_log_land_in_the_ring_and_the_call_log() {
    let mut mods = host();
    mods.load(
        mod_files("logger", "print('hello', 1, nil, true) regia.log('second')"),
        &mut NoWorld,
    )
    .expect("loads");
    assert_eq!(
        mods.get("logger").expect("mod").log(),
        ["hello\t1\tnil\ttrue", "second"]
    );
    let outcome = mods
        .run_chunk(&mut NoWorld, Some("logger"), "print('third') return 7")
        .expect("runs");
    assert_eq!(outcome.log, ["third"]);
    assert_eq!(outcome.results, [ScriptValue::Number(7.0)]);
}

#[test]
fn manifests_and_sizes_are_checked_before_any_lua_runs() {
    let mut mods = LuaMods::new(
        0,
        LuaLimits {
            max_mod_bytes: 256,
            ..LuaLimits::default()
        },
    );
    let big = ModFiles::from_map([
        ("luamod.ron".to_owned(), manifest("big", "")),
        ("main.lua".to_owned(), "x = 1".repeat(100)),
    ]);
    assert_eq!(
        mods.load(big, &mut NoWorld).map_err(|e| e.code),
        Err(LuaErrorCode::TooLarge)
    );
    let none = ModFiles::from_map([("main.lua", "x = 1")]);
    assert_eq!(
        mods.load(none, &mut NoWorld).map_err(|e| e.code),
        Err(LuaErrorCode::Manifest)
    );
    let newer = ModFiles::from_map([
        (
            "luamod.ron".to_owned(),
            manifest("newer", "").replace("format_version: 1", "format_version: 2"),
        ),
        ("main.lua".to_owned(), String::new()),
    ]);
    assert_eq!(
        mods.load(newer, &mut NoWorld).map_err(|e| e.code),
        Err(LuaErrorCode::Manifest)
    );
    let missing_entry = ModFiles::from_map([("luamod.ron".to_owned(), manifest("noentry", ""))]);
    assert_eq!(
        mods.load(missing_entry, &mut NoWorld).map_err(|e| e.code),
        Err(LuaErrorCode::Manifest)
    );
    mods.load(mod_files("dup", ""), &mut NoWorld).expect("first");
    assert_eq!(
        mods.load(mod_files("dup", ""), &mut NoWorld).map_err(|e| e.code),
        Err(LuaErrorCode::Manifest)
    );
    assert_eq!(
        mods.iter().map(|m| m.id()).collect::<Vec<_>>(),
        ["dup", "noentry"]
    );
}

#[test]
fn a_manifest_may_lower_its_budgets() {
    let files = ModFiles::from_map([
        (
            "luamod.ron".to_owned(),
            manifest("small", "instruction_budget: Some(5000),"),
        ),
        (
            "main.lua".to_owned(),
            "function on_tick() for i = 1, 100000 do end end".to_owned(),
        ),
    ]);
    let mut mods = host();
    mods.load(files, &mut NoWorld).expect("loads");
    assert_eq!(mods.get("small").expect("mod").limits().instruction_budget, 5000);
    mods.call_tick(&mut NoWorld, 0.1);
    assert!(matches!(
        mods.get("small").expect("mod").status(),
        ModStatus::Disabled {
            code: LuaErrorCode::InstructionBudget,
            ..
        }
    ));
}

#[test]
fn mods_run_in_id_order_whatever_the_load_order() {
    let mut mods = host();
    for id in ["zeta", "alpha", "mid"] {
        mods.load(
            mod_files(
                id,
                &format!("function on_update() regia.emit('order', '{id}') end"),
            ),
            &mut NoWorld,
        )
        .expect("loads");
    }
    mods.call_update(&mut NoWorld, 0.0);
    let order: Vec<_> = mods.drain_emitted().into_iter().map(|e| e.mod_id).collect();
    assert_eq!(order, ["alpha", "mid", "zeta"]);
}

#[test]
fn globals_reads_tables_by_path_without_running_lua() {
    let mut mods = host();
    mods.load(
        mod_files(
            "cfg",
            "config = { speed = 4, name = 'x', nested = { 1, 2, 3 } }
             setmetatable(config, { __index = function() error('must not run') end })",
        ),
        &mut NoWorld,
    )
    .expect("loads");
    let entries = mods.globals("cfg", Some("config"), 512).expect("path");
    let keys: Vec<_> = entries
        .iter()
        .map(|e| (e.key.as_str(), e.kind.as_str()))
        .collect();
    assert_eq!(
        keys,
        [("name", "string"), ("nested", "table"), ("speed", "number")]
    );
    assert_eq!(entries[1].len, Some(3));
    assert!(mods.globals("cfg", Some("config.speed"), 512).is_err());
    assert!(mods.globals("cfg", Some("config.missing"), 512).is_err());
    assert_eq!(
        mods.globals("nope", None, 512).map_err(|e| e.code),
        Err(LuaErrorCode::UnknownMod)
    );
}

/// Fuzz-style: deterministic generated chunks and values through the binding layer; nothing may
/// panic and the host must stay usable.
#[test]
fn generated_chunks_and_values_never_panic_the_host() {
    let mut rng = regia_core::math::Rng::new(0x5eed);
    let atoms = [
        "regia.vec3(1,2,3)",
        "regia.find('x')",
        "regia.spawn('s')",
        "{}",
        "{1,2}",
        "{a={b={}}}",
        "nil",
        "true",
        "1e308",
        "-0",
        "0/0",
        "'str'",
        "string.rep('a', 10)",
        "function() end",
        "coroutine.create(print)",
        "regia",
        "math.huge",
        "#{}",
    ];
    let calls = [
        "regia.emit('e', %)",
        "regia.on('e', %)",
        "regia.log(%)",
        "print(%)",
        "regia.find(%)",
        "regia.spawn(%, %)",
        "regia.vec3(%, %, %)",
        "(%):alive()",
        "(%):position()",
        "(%):set_position(%)",
        "regia.random(%)",
        "math.random(%, %)",
        "require(%)",
        "tostring(%)",
        "(%) + (%)",
        "(%) * (%)",
        "(%) == (%)",
        "setmetatable({}, %)",
        "regia.vec3(1,2,3):dot(%)",
        "error(%)",
    ];
    let mut mods = LuaMods::new(
        3,
        LuaLimits {
            instruction_budget: 50_000,
            memory_limit_bytes: 2 * 1024 * 1024,
            ..LuaLimits::default()
        },
    );
    let mut world = TestWorld::default();
    world.add("x", None);
    for _ in 0..400 {
        let mut chunk = String::new();
        for _ in 0..rng.range(1..4usize) {
            let mut call = calls[rng.range(0..calls.len())].to_owned();
            while let Some(at) = call.find('%') {
                call.replace_range(at..=at, atoms[rng.range(0..atoms.len())]);
            }
            chunk.push_str("pcall(function() return ");
            chunk.push_str(&call);
            chunk.push_str(" end) ");
        }
        chunk.push_str("return ");
        chunk.push_str(atoms[rng.range(0..atoms.len())]);
        let outcome = mods.run_chunk(&mut world, None, &chunk).expect("scratch state");
        assert!(outcome.ok || outcome.error.is_some(), "{chunk}");
    }
    // Garbage source text.
    for _ in 0..200 {
        let len = rng.range(0..64usize);
        let text: String = (0..len).map(|_| char::from(rng.range(0..128u32) as u8)).collect();
        let _ = mods.run_chunk(&mut world, None, &text).expect("scratch state");
    }
    mods.drain_emitted();
    assert_eq!(eval("return 1 + 1"), ScriptValue::Number(2.0));
}

// --- ADR-1957: work the instruction hook does not see, and work a delivery pass adds up ---

/// A world that says every `find` walked `scanned` entities, and counts the finds.
#[derive(Default)]
struct ScanWorld {
    scanned: u64,
    finds: u64,
    pending: u64,
}

impl ModWorld for ScanWorld {
    fn find(&mut self, _name: &str) -> Option<u64> {
        self.finds += 1;
        self.pending += self.scanned;
        None
    }
    fn take_scanned(&mut self) -> u64 {
        std::mem::take(&mut self.pending)
    }
    fn alive(&mut self, _entity: u64) -> bool {
        false
    }
    fn name(&mut self, _entity: u64) -> Option<String> {
        None
    }
    fn position(&mut self, _entity: u64) -> Option<Vec3> {
        None
    }
    fn set_position(&mut self, _entity: u64, _position: Vec3) -> bool {
        false
    }
    fn spawn(&mut self, _owner: &str, _name: &str, _position: Vec3) -> Option<u64> {
        Some(1)
    }
    fn despawn(&mut self, _owner: &str, _entity: u64) -> bool {
        false
    }
}

#[test]
fn world_scans_are_charged_to_the_budget() {
    let mut world = ScanWorld {
        scanned: 100_000,
        ..ScanWorld::default()
    };
    let outcome = host()
        .run_chunk(&mut world, None, "for i = 1, 200000 do regia.find('missing') end")
        .expect("scratch state");
    assert_eq!(
        outcome.error.map(|e| e.code),
        Some(LuaErrorCode::InstructionBudget)
    );
    // The 1 M budget buys ten scans of 100,000 entities, not 200,000 of them.
    assert_eq!(world.finds, 10);
}

#[test]
fn a_pattern_bomb_disables_the_mod_and_the_game_goes_on() {
    let mut mods = host();
    let id = mods
        .load(
            mod_files(
                "bomb",
                r#"function on_tick() return string.find(("a"):rep(60), ("a?"):rep(60) .. "b") end"#,
            ),
            &mut NoWorld,
        )
        .expect("loads");
    mods.call_tick(&mut NoWorld, 1.0 / 60.0);
    let status = mods.get(&id).expect("mod").status().clone();
    assert!(
        matches!(
            status,
            ModStatus::Disabled {
                code: LuaErrorCode::InstructionBudget,
                ..
            }
        ),
        "{status:?}"
    );
}

#[test]
fn a_script_sentence_about_memory_is_not_a_memory_trip() {
    assert_eq!(
        eval_err("error('not enough memory while caching levels')"),
        LuaErrorCode::Runtime
    );
    let later_pcall = eval(
        r#"
        pcall(error, 'not enough memory while caching levels', 0)
        local ok = pcall(error, 'second')
        return ok
        "#,
    );
    assert_eq!(later_pcall, ScriptValue::Bool(false));
}

#[test]
fn a_delivery_pass_shares_one_allowance_across_handlers() {
    // Each handler alone fits a call's budget; 64 of them on 256 events do not fit 16 calls' worth.
    let main = r#"
        for i = 1, 64 do
            regia.on('hit', function()
                local x = 0
                for j = 1, 100000 do x = x + j end
                ran = (ran or 0) + 1
            end)
        end
    "#;
    let mut mods = host();
    let id = mods.load(mod_files("busy", main), &mut NoWorld).expect("loads");
    for _ in 0..256 {
        mods.fire("hit", ScriptValue::Nil);
    }
    mods.deliver_events(&mut NoWorld);
    let module = mods.get(&id).expect("mod");
    assert!(
        matches!(
            module.status(),
            ModStatus::Disabled {
                code: LuaErrorCode::InstructionBudget,
                ..
            }
        ),
        "{:?}",
        module.status()
    );
    // At most what 16 M instructions buy, far from 64 * 256.
    assert!(module.counters().calls < 1000, "{}", module.counters().calls);
}

#[test]
fn no_op_handlers_still_cost_a_call_each() {
    let main = "for i = 1, 256 do regia.on('hit', function() end) end";
    let mut mods = host();
    let id = mods.load(mod_files("idle", main), &mut NoWorld).expect("loads");
    for _ in 0..1000 {
        mods.fire("hit", ScriptValue::Nil);
    }
    mods.deliver_events(&mut NoWorld);
    let module = mods.get(&id).expect("mod");
    assert!(!module.is_active());
    // 16 M instructions at one hook interval per call, plus the load.
    assert!(module.counters().calls <= 16_002, "{}", module.counters().calls);
}

#[test]
fn a_normal_mod_is_not_disabled_by_the_delivery_allowance() {
    let main = r#"
        regia.on('hit', function(v) regia.emit('seen', v) end)
        regia.on('hit', function(v) count = (count or 0) + 1 end)
    "#;
    let mut mods = host();
    let id = mods.load(mod_files("normal", main), &mut NoWorld).expect("loads");
    for i in 0..100 {
        mods.fire("hit", ScriptValue::Number(f64::from(i)));
    }
    mods.deliver_events(&mut NoWorld);
    assert!(mods.get(&id).expect("mod").is_active());
    assert_eq!(mods.drain_emitted().len(), 100);
}

#[test]
fn the_handlers_of_one_event_are_capped() {
    let code = eval_err("for i = 1, 257 do regia.on('hit', function() end) end");
    assert_eq!(code, LuaErrorCode::Runtime);
}

#[test]
fn emits_and_spawns_are_counted_across_a_delivery() {
    // 8 handlers, 64 events: each call stays under the call cap; the pass passes 16 calls' worth.
    let main = r#"
        for i = 1, 8 do
            regia.on('hit', function() for j = 1, 256 do regia.emit('out', j) end end)
        end
    "#;
    let mut mods = host();
    let id = mods.load(mod_files("loud", main), &mut NoWorld).expect("loads");
    for _ in 0..64 {
        mods.fire("hit", ScriptValue::Nil);
    }
    mods.deliver_events(&mut NoWorld);
    assert!(!mods.get(&id).expect("mod").is_active());
    assert!(mods.drain_emitted().len() <= 16 * 256);

    let main = r#"
        for i = 1, 8 do
            regia.on('hit', function() for j = 1, 64 do regia.spawn('x') end end)
        end
    "#;
    let mut mods = host();
    let id = mods
        .load(mod_files("spawner", main), &mut NoWorld)
        .expect("loads");
    for _ in 0..64 {
        mods.fire("hit", ScriptValue::Nil);
    }
    let mut world = ScanWorld::default();
    mods.deliver_events(&mut world);
    assert!(!mods.get(&id).expect("mod").is_active());
}

#[test]
fn one_string_in_many_slots_cannot_blow_up_the_rust_copy() {
    // 4,096 slots of one 64 KiB string: 64 KiB of Lua memory, 256 MiB as a Rust value.
    let chunk = r#"
        local s, t = ("a"):rep(65536), {}
        for i = 1, 4096 do t[i] = s end
        regia.emit('big', t)
    "#;
    assert_eq!(eval_err(chunk), LuaErrorCode::BadValue);
}

#[test]
fn the_bytes_a_call_emits_are_capped() {
    // Rebuilding nothing: 256 emits of a 1 MiB value would be 256 MiB of events from one call.
    let chunk = r#"
        local s, t = ("a"):rep(65536), {}
        for i = 1, 15 do t[i] = s end
        for i = 1, 256 do regia.emit('big', t) end
    "#;
    assert_eq!(eval_err(chunk), LuaErrorCode::BadValue);
}

#[test]
fn emitted_events_stop_growing_when_the_game_never_drains() {
    let main = r#"
        local s, t = ("a"):rep(65536), {}
        for i = 1, 15 do t[i] = s end
        function on_tick() regia.emit('big', t) end
    "#;
    let mut mods = host();
    mods.load(mod_files("chatty", main), &mut NoWorld).expect("loads");
    // One event a tick: a call emits at most `max_emit_bytes_per_call` (1 MiB, ADR-1943), which
    // is stricter than the memory limit ADR-1957 allowed, so ten of these no longer fit one call.
    for _ in 0..160 {
        mods.call_tick(&mut NoWorld, 1.0);
    }
    // 160 events of just under 1 MiB were emitted; 64 MiB of them are kept.
    let kept = mods.drain_emitted().len();
    assert!((60..=70).contains(&kept), "{kept}");
    mods.call_tick(&mut NoWorld, 1.0);
    assert_eq!(mods.drain_emitted().len(), 1);
}

#[test]
fn run_chunk_caps_its_source_and_its_results() {
    let long = "--".repeat(40_000);
    let err = host().run_chunk(&mut NoWorld, None, &long).expect_err("too long");
    // `lua_too_large`, the code the lua lane gave an oversized chunk (ADR-1943), not `lua_bad_value`.
    assert_eq!(err.code, LuaErrorCode::TooLarge);
    let outcome = host()
        .run_chunk(
            &mut NoWorld,
            None,
            r#"
            local s, t = ("a"):rep(65536), {}
            for i = 1, 15 do t[i] = s end
            return t, t, t
            "#,
        )
        .expect("scratch state");
    assert!(outcome.ok);
    assert!(matches!(outcome.results[0], ScriptValue::List(_)));
    assert_eq!(outcome.results[1], ScriptValue::String("<table>".to_owned()));
}

#[test]
fn the_scratch_owner_prefix_is_reserved() {
    let err = host()
        .load(mod_files("agent.scratch", ""), &mut NoWorld)
        .expect_err("reserved");
    assert_eq!(err.code, LuaErrorCode::Manifest);
}

#[test]
fn manifest_labels_are_capped() {
    let long = "x".repeat(257);
    let text = format!(
        r#"(format_version: 1, id: "a", name: "{long}", version: "1", entry: "main.lua", api_version: 1)"#
    );
    let files = ModFiles::from_map([
        ("luamod.ron".to_owned(), text),
        ("main.lua".to_owned(), String::new()),
    ]);
    let err = host().load(files, &mut NoWorld).expect_err("too long");
    assert_eq!(err.code, LuaErrorCode::Manifest);
}

#[test]
fn globals_returns_the_smallest_keys_of_a_big_table() {
    let main = "for i = 1, 5000 do _G['k' .. string.format('%05d', i)] = i end";
    let mut mods = host();
    let id = mods.load(mod_files("wide", main), &mut NoWorld).expect("loads");
    let keys: Vec<String> = mods
        .globals(&id, None, 3)
        .expect("globals")
        .into_iter()
        .map(|e| e.key)
        .collect();
    let mut all: Vec<String> = mods
        .globals(&id, None, 100_000)
        .expect("globals")
        .into_iter()
        .map(|e| e.key)
        .collect();
    all.truncate(3);
    assert_eq!(keys, all);
}

#[test]
fn a_hostile_pattern_stops_at_the_budget_instead_of_hanging_the_game() {
    // Each of these runs effectively forever in Lua 5.1's C matcher, which no hook can stop.
    for chunk in [
        r#"return string.find(string.rep("a", 60), string.rep("a?", 60) .. "b")"#,
        r#"return string.find(string.rep("a", 2000000), "^" .. string.rep("a-", 30) .. "b")"#,
        r#"return string.rep("a", 60):match(string.rep("a*", 20) .. "b")"#,
        r#"return string.gsub(string.rep("a", 60), string.rep("a?", 60) .. "b", "x")"#,
        r#"for x in string.gmatch(string.rep("a", 60), string.rep("a?", 60) .. "b") do end"#,
        r#"return pcall(string.find, string.rep("a", 60), string.rep("a?", 60) .. "b")"#,
        r#"return string.find(string.rep("a", 2000000), string.rep("a", 1000000) .. "b", 1, true)"#,
    ] {
        assert_eq!(eval_err(chunk), LuaErrorCode::InstructionBudget, "{chunk}");
    }
}

#[test]
fn the_budgeted_string_functions_still_work_as_methods() {
    assert_eq!(eval("return ('hello'):find('l+')"), ScriptValue::Number(3.0));
    assert_eq!(
        eval("return ('key=value'):match('=(.*)')"),
        ScriptValue::String("value".into())
    );
    assert_eq!(
        eval("return (('abc'):gsub('%w', '%0%0'))"),
        ScriptValue::String("aabbcc".into())
    );
    assert_eq!(
        eval("local n = 0 for w in ('a b c'):gmatch('%a') do n = n + 1 end return n"),
        ScriptValue::Number(3.0)
    );
    assert_eq!(
        eval("return (string.gsub('hello', 'l', { l = 'L' }))"),
        ScriptValue::String("heLLo".into())
    );
    assert_eq!(
        eval("return (string.gsub('hello', '(h)(e)', function(a, b) return b .. a end))"),
        ScriptValue::String("ehllo".into())
    );
}

#[test]
fn a_gsub_that_grows_past_the_memory_cap_fails_as_memory_under_pcall_too() {
    let limits = LuaLimits {
        instruction_budget: 1_000_000_000,
        ..LuaLimits::default()
    };
    let chunk = r#"return pcall(string.gsub, string.rep("a", 100000), "a", string.rep("b", 1000))"#;
    let outcome = LuaMods::new(1, limits)
        .run_chunk(&mut NoWorld, None, chunk)
        .expect("scratch");
    assert_eq!(outcome.error.map(|e| e.code), Some(LuaErrorCode::Memory));
    // Under the default budget the charge for the bytes copied stops it even sooner.
    assert_eq!(eval_err(chunk), LuaErrorCode::InstructionBudget);
}

#[test]
fn a_replacement_function_that_loops_is_stopped_by_the_hook() {
    assert_eq!(
        eval_err("return string.gsub('aaa', 'a', function() while true do end end)"),
        LuaErrorCode::InstructionBudget
    );
}

#[test]
fn handlers_share_one_budget_per_delivery() {
    let mut mods = LuaMods::new(
        1,
        LuaLimits {
            instruction_budget: 100_000,
            ..LuaLimits::default()
        },
    );
    mods.load(
        mod_files(
            "many",
            "for i = 1, 200 do regia.on('hit', function() for j = 1, 2000 do local x = j * 2 end end) end
             function on_tick() end",
        ),
        &mut NoWorld,
    )
    .expect("loads");
    mods.load(
        mod_files("other", "regia.on('hit', function() regia.emit('ok', 1) end)"),
        &mut NoWorld,
    )
    .expect("loads");
    mods.fire("hit", ScriptValue::Nil);
    mods.deliver_events(&mut NoWorld);
    assert!(matches!(
        mods.get("many").map(regia_script_lua::LuaMod::status),
        Some(ModStatus::Disabled {
            code: LuaErrorCode::InstructionBudget,
            ..
        })
    ));
    assert_eq!(mods.drain_emitted().len(), 1, "the other mod still ran");
    mods.call_tick(&mut NoWorld, 0.1);
}

#[test]
fn a_new_pass_is_a_new_budget() {
    let mut mods = LuaMods::new(
        1,
        LuaLimits {
            instruction_budget: 100_000,
            ..LuaLimits::default()
        },
    );
    mods.load(
        mod_files(
            "m",
            "regia.on('hit', function() for j = 1, 10000 do local x = j * 2 end end)",
        ),
        &mut NoWorld,
    )
    .expect("loads");
    for _ in 0..20 {
        mods.fire("hit", ScriptValue::Nil);
        mods.deliver_events(&mut NoWorld);
    }
    assert!(mods.get("m").is_some_and(regia_script_lua::LuaMod::is_active));
}

#[test]
fn emits_and_spawns_are_capped_across_a_delivery() {
    let mut mods = host();
    mods.load(
        mod_files(
            "emits",
            "for i = 1, 200 do regia.on('hit', function() for j = 1, 2 do regia.emit('x', j) end end) end",
        ),
        &mut NoWorld,
    )
    .expect("loads");
    mods.fire("hit", ScriptValue::Nil);
    mods.deliver_events(&mut NoWorld);
    assert!(matches!(
        mods.get("emits").map(regia_script_lua::LuaMod::status),
        Some(ModStatus::Disabled {
            code: LuaErrorCode::Runtime,
            ..
        })
    ));
    assert!(mods.drain_emitted().len() <= 256);
}

#[test]
fn one_event_takes_a_bounded_number_of_handlers() {
    assert_eq!(
        eval_err("for i = 1, 300 do regia.on('hit', function() end) end"),
        LuaErrorCode::Runtime
    );
}

#[test]
fn emitted_payloads_are_capped_in_bytes() {
    assert_eq!(
        eval_err(
            "local v = {} for i = 1, 16 do v[i] = string.rep('x', 60000) end
             for i = 1, 8 do regia.emit('big', v) end"
        ),
        LuaErrorCode::BadValue
    );
}

/// Audit wave (script_lua correctness): a rejected emit must not consume the call's emit count
/// or byte budget; the caps count what is actually queued.
#[test]
fn a_rejected_emit_does_not_eat_the_calls_emit_budget() {
    let outcome = host()
        .run_chunk(
            &mut NoWorld,
            None,
            r#"
                -- ~900,256 of the default 1 MiB byte cap is queued (15 strings of 60,000
                -- bytes; a single value string may be at most 64 KiB).
                local v = {} for i = 1, 15 do v[i] = string.rep("x", 60000) end
                regia.emit("e", v)
                -- Over the cap: refused and not queued.
                local w = {} for i = 1, 3 do w[i] = string.rep("y", 60000) end
                local refused = pcall(regia.emit, "e", w)
                -- Still room for a small event when the refused one counted for nothing.
                local small = pcall(regia.emit, "e", 1)
                return refused, small
            "#,
        )
        .expect("scratch");
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(
        outcome.results,
        vec![ScriptValue::Bool(false), ScriptValue::Bool(true)]
    );
}

/// A world of `entities` named entities that counts how often it is searched.
struct Crowded {
    entities: u64,
    finds: u64,
}

impl ModWorld for Crowded {
    fn find(&mut self, _: &str) -> Option<u64> {
        self.finds += 1;
        None
    }
    fn alive(&mut self, _: u64) -> bool {
        false
    }
    fn name(&mut self, _: u64) -> Option<String> {
        None
    }
    fn position(&mut self, _: u64) -> Option<Vec3> {
        None
    }
    fn set_position(&mut self, _: u64, _: Vec3) -> bool {
        false
    }
    fn spawn(&mut self, _: &str, _: &str, _: Vec3) -> Option<u64> {
        None
    }
    fn despawn(&mut self, _: &str, _: u64) -> bool {
        false
    }
    fn find_cost(&mut self) -> u64 {
        self.entities
    }
}

#[test]
fn regia_find_is_charged_by_the_size_of_the_world() {
    let mut world = Crowded {
        entities: 10_000,
        finds: 0,
    };
    let mut mods = host();
    let outcome = mods
        .run_chunk(&mut world, None, "for i = 1, 200000 do regia.find('missing') end")
        .expect("scratch");
    assert_eq!(
        outcome.error.map(|e| e.code),
        Some(LuaErrorCode::InstructionBudget)
    );
    assert!(
        world.finds * world.entities < 20_000_000,
        "{} finds of {} entities",
        world.finds,
        world.entities
    );
}

#[test]
fn repeating_an_empty_string_costs_nothing_and_still_checks_its_arguments() {
    // Lua 5.1 loops `n` times over nothing, seconds per call, where neither the hook nor the
    // memory cap sees it. Unfixed, this chunk stalls for minutes before the first hook.
    assert_eq!(
        eval("local s for i = 1, 10000 do s = string.rep('', 2147483647) end return s"),
        ScriptValue::String(String::new())
    );
    assert_eq!(
        eval("return ('ab'):rep(2) .. string.rep('c', 2) .. string.rep(1, 2)"),
        ScriptValue::String("ababcc11".into())
    );
    assert_eq!(eval_err("return string.rep('', 'x')"), LuaErrorCode::Runtime);
}

#[test]
fn sorting_is_charged_so_a_loop_of_sorts_stops_at_the_budget() {
    // The count alone must stop this: at three instructions a sort it would run for hours were the
    // comparisons not counted.
    let limits = LuaLimits {
        max_call_millis: 0,
        ..LuaLimits::default()
    };
    let mut mods = LuaMods::new(1, limits);
    let outcome = mods
        .run_chunk(
            &mut NoWorld,
            None,
            "local t = {} for i = 1, 50000 do t[i] = -i end while true do table.sort(t) end",
        )
        .expect("scratch");
    assert_eq!(
        outcome.error.map(|e| e.code),
        Some(LuaErrorCode::InstructionBudget)
    );
}

#[test]
fn sorting_still_sorts_and_cannot_be_caught_past_the_budget() {
    assert_eq!(
        eval("local t = { 3, 1, 2 } table.sort(t) return t[1] * 100 + t[2] * 10 + t[3]"),
        ScriptValue::Number(123.0)
    );
    assert_eq!(
        eval("local t = { 'a', 'c', 'b' } table.sort(t, function(x, y) return x > y end) return t[1]"),
        ScriptValue::String("c".into())
    );
    assert_eq!(
        eval("local t = {} table.sort(t) return #t"),
        ScriptValue::Number(0.0)
    );
    assert_eq!(eval("return (pcall(table.sort, 5))"), ScriptValue::Bool(false));
    assert_eq!(
        eval_err("local t = {} for i = 1, 90000 do t[i] = i end while true do pcall(table.sort, t) end"),
        LuaErrorCode::InstructionBudget
    );
}

#[test]
fn a_slow_mod_in_a_delivery_pass_does_not_trip_the_watchdog_of_the_mods_after_it() {
    // `b` runs into the watchdog, so the pass takes at least its limit before `c` has its turn.
    // `c` runs past a hook check, where its own clock is read.
    let limits = LuaLimits {
        instruction_budget: u64::MAX / 2,
        max_call_millis: 200,
        ..LuaLimits::default()
    };
    let mut mods = LuaMods::new(1, limits);
    let slow = "regia.on('go', function() while true do end end)";
    let quick = "regia.on('go', function() local n = 0 for i = 1, 5000 do n = n + i end end)";
    for (id, code) in [("a", quick), ("b", slow), ("c", quick)] {
        mods.load(mod_files(id, code), &mut NoWorld).expect("loads");
    }
    mods.fire("go", ScriptValue::Nil);
    mods.deliver_events(&mut NoWorld);
    let active = |mods: &LuaMods, id: &str| mods.get(id).is_some_and(regia_script_lua::LuaMod::is_active);
    assert!(!active(&mods, "b"), "the slow one is stopped");
    assert!(active(&mods, "a"), "the mod before it");
    assert!(active(&mods, "c"), "the mod after it");
}

#[test]
fn the_watchdog_stops_a_call_the_instruction_count_cannot_see() {
    // No instruction limit to speak of, so only the watchdog can end this. The outcome does not
    // depend on how fast the machine is.
    let limits = LuaLimits {
        instruction_budget: u64::MAX / 2,
        max_call_millis: 1,
        ..LuaLimits::default()
    };
    let mut mods = LuaMods::new(1, limits);
    let outcome = mods
        .run_chunk(&mut NoWorld, None, "while true do end")
        .expect("scratch");
    assert_eq!(
        outcome.error.map(|e| e.code),
        Some(LuaErrorCode::InstructionBudget)
    );
}

#[test]
fn only_lua_s_own_message_counts_as_out_of_memory() {
    assert_eq!(
        eval_err("error('not enough memory while caching levels', 0)"),
        LuaErrorCode::Runtime
    );
    // Caught, it stays an ordinary error and later `pcall`s work.
    assert_eq!(
        eval("pcall(error, 'not enough memory while caching levels', 0) return (pcall(error, 'x'))"),
        ScriptValue::Bool(false)
    );
}

#[test]
fn a_mod_cannot_take_the_scratch_states_id() {
    let mut mods = host();
    let error = mods
        .load(mod_files("agent.scratch", ""), &mut NoWorld)
        .expect_err("reserved");
    assert_eq!(error.code, LuaErrorCode::Manifest);
}

#[test]
fn run_chunk_caps_the_chunk_for_every_caller() {
    let chunk = " ".repeat(regia_script_lua::MAX_CHUNK_BYTES + 1);
    let error = host()
        .run_chunk(&mut NoWorld, None, &chunk)
        .expect_err("too long");
    assert_eq!(error.code, LuaErrorCode::TooLarge);
}

/// A table whose first element is the next level, with `width` distinct strings after it: walking
/// it holds every level's strings at once.
const WIDE_AND_DEEP: &str = r#"
local function level(depth, width)
  local t = { depth > 1 and level(depth - 1, width) or 0 }
  for i = 2, width do t[i] = depth .. ":" .. i end
  return t
end
local wide = level(4, 3500)
"#;

#[test]
fn a_value_holding_many_strings_at_once_is_an_error_not_a_panic() {
    // The binding's reference stack is small on Lua 5.1 and overrunning it panics, which
    // `panic=abort` turns into a dead game.
    let emit = format!("{WIDE_AND_DEEP} regia.emit('wide', wide)");
    let outcome = host().run_chunk(&mut NoWorld, None, &emit).expect("scratch");
    assert!(!outcome.ok, "over the item cap");
    assert_eq!(outcome.error.map(|e| e.code), Some(LuaErrorCode::BadValue));
    let returned = format!("{WIDE_AND_DEEP} return wide");
    let outcome = host().run_chunk(&mut NoWorld, None, &returned).expect("scratch");
    assert!(
        outcome.ok,
        "a result that cannot leave is described: {:?}",
        outcome.error
    );
}

#[test]
fn a_mod_returning_thousands_of_tables_stays_alive() {
    let chunk = "local t = {} for i = 1, 30000 do t[i] = { i } end regia.state.t = t return #t";
    assert_eq!(eval(chunk), ScriptValue::Number(30000.0));
}

/// The sandbox's whole surface, so a function the C libraries bring in cannot pass unnoticed: the
/// compat alias `string.gfind` kept the unbudgeted C matcher reachable after `gmatch` was replaced.
#[test]
fn the_libraries_hold_exactly_what_the_sandbox_reviewed() {
    let keys = |table: &str| {
        let chunk = format!(
            "local keys = {{}} for k in pairs({table}) do keys[#keys + 1] = tostring(k) end
             table.sort(keys) return table.concat(keys, ' ')"
        );
        eval(&chunk)
    };
    let text = |s: &str| ScriptValue::String(s.to_owned());
    assert_eq!(
        keys("_G"),
        text(
            "_G _VERSION assert collectgarbage coroutine error getfenv getmetatable ipairs math next \
             pairs pcall print rawequal rawget rawset regia require select setfenv setmetatable \
             string table tonumber tostring type unpack xpcall"
        )
    );
    assert_eq!(
        keys("string"),
        text("byte char find format gfind gmatch gsub len lower match rep reverse sub upper")
    );
    assert_eq!(
        keys("table"),
        text("concat foreach foreachi getn insert maxn remove setn sort")
    );
    assert_eq!(keys("coroutine"), text("create resume running status wrap yield"));
    assert_eq!(
        keys("math"),
        text(
            "abs acos asin atan atan2 ceil cos cosh deg exp floor fmod frexp huge ldexp log log10 \
             max min mod modf pi pow rad random randomseed sin sinh sqrt tan tanh"
        )
    );
}

#[test]
fn the_compat_gfind_alias_is_the_budgeted_matcher_too() {
    assert_eq!(
        eval_err(r#"for x in string.gfind(string.rep("a", 60), string.rep("a?", 60) .. "b") do end"#),
        LuaErrorCode::InstructionBudget
    );
    assert_eq!(
        eval("local n = 0 for w in string.gfind('a b c', '%a') do n = n + 1 end return n"),
        ScriptValue::Number(3.0)
    );
}

#[test]
fn an_empty_repeat_with_its_count_as_a_string_costs_nothing_either() {
    // Lua turns the string into a number inside `rep`, so a check of the argument's type alone
    // lets this through to the C loop.
    assert_eq!(
        eval("local s for i = 1, 10000 do s = string.rep('', '2147483647') end return s"),
        ScriptValue::String(String::new())
    );
    assert_eq!(eval_err("return string.rep('', {})"), LuaErrorCode::Runtime);
}

#[test]
fn gmatch_iterators_run_from_lua_and_keep_working_across_calls() {
    assert_eq!(
        eval(
            "local out = {} for k, v in ('a=1, b=2'):gmatch('(%w+)=(%w+)') do out[#out + 1] = k .. v end return table.concat(out, ' ')"
        ),
        ScriptValue::String("a1 b2".into())
    );
    assert_eq!(
        eval("local n = 0 for _ in ('abc'):gmatch('') do n = n + 1 end return n"),
        ScriptValue::Number(4.0)
    );
    assert_eq!(
        eval("local it = ('x'):gmatch('x') it() return it() == nil and it() == nil"),
        ScriptValue::Bool(true)
    );
    assert_eq!(
        eval("local n = 0 for _ in string.gmatch(123, 2) do n = n + 1 end return n"),
        ScriptValue::Number(1.0)
    );
    assert_eq!(
        eval("return (pcall(string.gmatch, {}, 'x'))"),
        ScriptValue::Bool(false)
    );
    assert_eq!(
        eval("return (pcall(string.gmatch, 'x'))"),
        ScriptValue::Bool(false)
    );

    let mut mods = host();
    mods.load(
        mod_files(
            "iter",
            "local it = ('one two'):gmatch('%a+') function on_update() last = it() end",
        ),
        &mut NoWorld,
    )
    .expect("loads");
    let last = |mods: &LuaMods| {
        let globals = mods.globals("iter", None, 512).expect("globals");
        globals
            .iter()
            .find(|g| g.key == "last")
            .and_then(|g| g.value.clone())
    };
    mods.call_update(&mut NoWorld, 0.0);
    assert_eq!(last(&mods), Some(ScriptValue::String("one".into())));
    mods.call_update(&mut NoWorld, 0.0);
    assert_eq!(last(&mods), Some(ScriptValue::String("two".into())));
}

#[test]
fn every_comparison_of_a_sort_without_a_comparator_is_counted() {
    // Lua 5.1's sort has a fixed pivot rule, so an input that makes it quadratic can be built: an
    // adversary comparator answers as late as it can and the order it settles on is that input
    // (McIlroy, "A Killer Adversary for Quicksort", 1999). In C that sort never reaches the hook.
    let limits = LuaLimits {
        instruction_budget: 2_000_000_000,
        max_call_millis: 0,
        ..LuaLimits::default()
    };
    let mut mods = LuaMods::new(1, limits);
    mods.load(mod_files("k", ""), &mut NoWorld).expect("loads");
    let setup = mods
        .run_chunk(
            &mut NoWorld,
            Some("k"),
            "local n = 3000
             local val, gas, solid, candidate, order = {}, n, 0, 0, {}
             for i = 1, n do val[i] = gas order[i] = i end
             table.sort(order, function(x, y)
               if val[x] == gas and val[y] == gas then
                 if x == candidate then val[x] = solid else val[y] = solid end
                 solid = solid + 1
               end
               if val[x] == gas then candidate = x elseif val[y] == gas then candidate = y end
               return val[x] < val[y]
             end)
             killer = val
             local copy, compared = {}, 0
             for i = 1, n do copy[i] = val[i] end
             table.sort(copy, function(a, b) compared = compared + 1 return a < b end)
             return compared",
        )
        .expect("runs");
    assert!(setup.ok, "{:?}", setup.error);
    let Some(ScriptValue::Number(compared)) = setup.results.first() else {
        panic!("no count: {:?}", setup.results);
    };
    assert!(*compared > 400_000.0, "the input is not quadratic: {compared}");
    let sorted = mods
        .run_chunk(
            &mut NoWorld,
            Some("k"),
            "local copy = {} for i = 1, #killer do copy[i] = killer[i] end table.sort(copy)",
        )
        .expect("runs");
    assert!(sorted.ok, "{:?}", sorted.error);
    assert!(
        sorted.instructions as f64 >= *compared,
        "{} instructions for {compared} comparisons",
        sorted.instructions
    );
}

/// Audit wave 5 (luasave H2): a heap that is near its cap is not a memory trip. With the live
/// heap filled past 15/16 of the cap (where the old watermark tripped), an ordinary error caught
/// by `pcall` or `xpcall` comes back as `false, message` and the mod keeps running.
#[test]
fn a_full_heap_does_not_turn_a_caught_error_into_a_memory_trip() {
    let limits = LuaLimits {
        memory_limit_bytes: 4 * 1024 * 1024,
        instruction_budget: 1_000_000_000,
        ..LuaLimits::default()
    };
    let chunk = r#"
        -- Lua 5.1 interns every string: each entry is made distinct so the heap grows.
        local t = {}
        local cap_kib = 4 * 1024
        while collectgarbage("count") < cap_kib * 0.95 do t[#t + 1] = string.rep("x", 40000) .. #t end
        local a, e1 = pcall(function() return missing.x end)
        local b, e2 = xpcall(function() return missing.y end, function(m) return "handled" end)
        return (not a) and type(e1) == "string" and (not b) and e2 == "handled"
            and collectgarbage("count") >= cap_kib * 0.94
    "#;
    let outcome = LuaMods::new(1, limits)
        .run_chunk(&mut NoWorld, None, chunk)
        .expect("scratch");
    assert!(outcome.ok, "{:?}", outcome.error);
    assert_eq!(outcome.results.first(), Some(&ScriptValue::Bool(true)));
}

/// Audit wave 5 (luasave H2): an `xpcall` handler that returns its own value cannot hide a memory
/// trip or a budget trip; the trip still ends the call as before.
#[test]
fn an_xpcall_handler_cannot_replace_a_trip() {
    let limits = LuaLimits {
        instruction_budget: 1_000_000_000,
        ..LuaLimits::default()
    };
    let memory = r#"return xpcall(function() return string.gsub(string.rep("a", 100000), "a", string.rep("b", 1000)) end, function() return "swallowed" end)"#;
    let outcome = LuaMods::new(1, limits)
        .run_chunk(&mut NoWorld, None, memory)
        .expect("scratch");
    assert_eq!(outcome.error.map(|e| e.code), Some(LuaErrorCode::Memory));
    let lua_heap = r#"return xpcall(function() local t = {} for i = 1, 1e9 do t[i] = {i} end end, function() return "swallowed" end)"#;
    let small = LuaLimits {
        memory_limit_bytes: 1024 * 1024,
        instruction_budget: 100_000_000,
        ..LuaLimits::default()
    };
    let outcome = LuaMods::new(1, small)
        .run_chunk(&mut NoWorld, None, lua_heap)
        .expect("scratch");
    assert_eq!(outcome.error.map(|e| e.code), Some(LuaErrorCode::Memory));
    // A handler that is not a function still behaves as Lua's own xpcall with it.
    assert_eq!(
        eval("local ok = xpcall(function() return 1 end, nil) return ok"),
        ScriptValue::Bool(true)
    );
}
