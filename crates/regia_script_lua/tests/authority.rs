// SPDX-License-Identifier: MPL-2.0
//! Host and presentation runtimes, the bounded host command queue and the snapshot hook
//! (spec 84 § 15, ADR-8466). Requirements from DO27's script bridge (`ir-script/src/lib.rs` at
//! d6ed37677, requirements only): a presentation runtime has no way to queue a command for the
//! authority, the host queue is bounded and refuses instead of growing, refusals do not stop the
//! script, and publishing an unchanged snapshot costs no table rebuild.

use std::collections::BTreeMap;

use regia_script_lua::{
    DEFAULT_MAX_PENDING_COMMANDS, HostCommand, HostCommandRules, LuaErrorCode, LuaLimits, LuaMods,
    MAX_SNAPSHOT_BYTES, ModFiles, ModStatus, NoWorld, ScriptAuthority, ScriptValue,
};

/// One mod with `main` as its entry chunk.
fn mod_files(id: &str, version: &str, main: &str) -> ModFiles {
    ModFiles::from_map([
        (
            "luamod.ron".to_owned(),
            format!(
                r#"(format_version: 1, id: "{id}", name: "{id}", version: "{version}", entry: "main.lua", api_version: 1)"#
            ),
        ),
        ("main.lua".to_owned(), main.to_owned()),
    ])
}

fn rules(names: &[&str]) -> HostCommandRules {
    HostCommandRules::new(names.iter().copied())
}

fn host(authority: ScriptAuthority, rules: HostCommandRules) -> LuaMods {
    LuaMods::new(7, LuaLimits::default())
        .with_authority(authority)
        .with_host_commands(rules)
}

fn map(entries: &[(&str, ScriptValue)]) -> ScriptValue {
    ScriptValue::Map(
        entries
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect::<BTreeMap<_, _>>(),
    )
}

fn number_global(mods: &LuaMods, id: &str, name: &str) -> Option<f64> {
    mods.globals(id, None, 512)
        .expect("globals")
        .into_iter()
        .find(|e| e.key == name)
        .and_then(|e| match e.value {
            Some(ScriptValue::Number(n)) => Some(n),
            _ => None,
        })
}

fn string_global(mods: &LuaMods, id: &str, name: &str) -> Option<String> {
    mods.globals(id, None, 512)
        .expect("globals")
        .into_iter()
        .find(|e| e.key == name)
        .and_then(|e| match e.value {
            Some(ScriptValue::String(s)) => Some(s),
            _ => None,
        })
}

/// The C-25 fail-first case. A presentation runtime has no `regia.command`, even for a command
/// name the host registered; calling it is a script error, and nothing reaches the host queue.
/// Overwriting `regia.command` with a script function cannot reach the queue either.
#[test]
fn presentation_script_cannot_enqueue_host_commands() {
    let mut mods = host(ScriptAuthority::Presentation, rules(&["build"]));
    assert_eq!(mods.authority(), ScriptAuthority::Presentation);
    let main = r#"
        assert(regia.command == nil, "a presentation runtime has no regia.command")
        assert(regia.authority == "presentation")
        function try_command() return regia.command("build", { stem = "a" }) end
        function forge() regia.command = function() return true end; return regia.command("build", {}) end
    "#;
    mods.load(mod_files("hud", "1.0.0", main), &mut NoWorld)
        .expect("a presentation mod loads");
    let error = mods
        .call_function(&mut NoWorld, "hud", "try_command", ScriptValue::Nil)
        .expect_err("calling regia.command is an error");
    assert_eq!(error.code, LuaErrorCode::Runtime, "{error:?}");
    let forged = mods
        .call_function(&mut NoWorld, "hud", "forge", ScriptValue::Nil)
        .expect("a script function of its own runs");
    assert_eq!(forged, ScriptValue::Bool(true));
    assert!(mods.drain_host_commands().is_empty());
    assert_eq!(mods.pending_host_commands(), 0);
    assert_eq!(
        mods.get("hud").expect("loaded").host_commands_refused(),
        0,
        "nothing was even offered to the queue"
    );
}

/// A host runtime queues registered commands; the drain is in mod id order, then call order,
/// and takes everything once.
#[test]
fn host_script_queues_registered_commands_in_a_fixed_order() {
    let mut mods = host(ScriptAuthority::Host, rules(&["build", "order"]));
    // Loaded out of id order on purpose: the drain does not follow load order.
    let b = r#"
        assert(regia.authority == "host")
        function go() assert(regia.command("order", 3) == true) end
    "#;
    let a = r#"
        function go()
          assert(regia.command("build", { stem = "x" }) == true)
          assert(regia.command("order") == true)
        end
    "#;
    mods.load(mod_files("b", "1", b), &mut NoWorld).expect("b");
    mods.load(mod_files("a", "1", a), &mut NoWorld).expect("a");
    for id in ["b", "a"] {
        mods.call_function(&mut NoWorld, id, "go", ScriptValue::Nil)
            .expect("go runs");
    }
    assert_eq!(mods.pending_host_commands(), 3);
    let drained = mods.drain_host_commands();
    assert_eq!(
        drained,
        vec![
            HostCommand {
                mod_id: "a".into(),
                name: "build".into(),
                value: map(&[("stem", ScriptValue::String("x".into()))]),
            },
            HostCommand {
                mod_id: "a".into(),
                name: "order".into(),
                value: ScriptValue::Nil,
            },
            HostCommand {
                mod_id: "b".into(),
                name: "order".into(),
                value: ScriptValue::Number(3.0),
            },
        ]
    );
    assert!(mods.drain_host_commands().is_empty());
}

/// Every refusal is `false` plus a reason the script can read; the mod stays active and the
/// refusals are counted. The pending cap is a setting, not a constant.
#[test]
fn host_command_refusals_are_reasons_and_the_cap_is_a_setting() {
    assert_eq!(
        HostCommandRules::default().max_pending_per_mod,
        DEFAULT_MAX_PENDING_COMMANDS
    );
    let mut tight = rules(&["place"]);
    tight.max_pending_per_mod = 4;
    tight.max_command_bytes = 256;
    let mut mods = host(ScriptAuthority::Host, tight);
    let main = r#"
        reasons = {}
        local function note(ok, reason)
          if ok then return end
          reasons[#reasons + 1] = reason
        end
        function go()
          note(regia.command("set_health", 1))
          note(regia.command("place", { x = 0/0, z = 1 }))
          note(regia.command("place", { f = function() end }))
          note(regia.command("place", string.rep("a", 1000)))
          note(regia.command("", 1))
          for i = 1, 6 do note(regia.command("place", { x = i, z = 1 })) end
          return table.concat(reasons, ",")
        end
    "#;
    mods.load(mod_files("m", "1", main), &mut NoWorld).expect("loads");
    let reasons = mods
        .call_function(&mut NoWorld, "m", "go", ScriptValue::Nil)
        .expect("refusals do not raise");
    assert_eq!(
        reasons,
        ScriptValue::String(
            "unknown_command,bad_value,bad_value,too_large,unknown_command,queue_full,queue_full".into()
        )
    );
    let module = mods.get("m").expect("loaded");
    assert_eq!(module.status(), &ModStatus::Active);
    assert_eq!(module.host_commands_refused(), 7);
    assert_eq!(module.pending_host_commands(), 4);
    let drained = mods.drain_host_commands();
    assert_eq!(drained.len(), 4);
    assert_eq!(
        drained[0].value,
        map(&[("x", ScriptValue::Number(1.0)), ("z", ScriptValue::Number(1.0))])
    );
    // Draining frees the cap again.
    mods.call_function(&mut NoWorld, "m", "go", ScriptValue::Nil)
        .expect("runs again");
    assert_eq!(mods.pending_host_commands(), 4);
}

/// The commands of a call that fails are dropped with it; those of earlier calls stay.
#[test]
fn the_commands_of_a_failed_call_are_dropped() {
    let mut mods = host(ScriptAuthority::Host, rules(&["order"]));
    let main = r#"
        function good() regia.command("order", 1) end
        function bad() regia.command("order", 2) error("boom") end
        function on_tick(dt) regia.command("order", 3) error("tick boom") end
    "#;
    mods.load(mod_files("m", "1", main), &mut NoWorld).expect("loads");
    mods.call_function(&mut NoWorld, "m", "good", ScriptValue::Nil)
        .expect("good");
    assert!(
        mods.call_function(&mut NoWorld, "m", "bad", ScriptValue::Nil)
            .is_err()
    );
    // A failing callback disables the mod; its command goes with the call.
    mods.call_tick(&mut NoWorld, 0.1);
    assert!(matches!(
        mods.get("m").expect("loaded").status(),
        ModStatus::Disabled { .. }
    ));
    let drained = mods.drain_host_commands();
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].value, ScriptValue::Number(1.0));
}

/// Commands nobody drained yet survive a restart and a hot reload of their mod.
#[test]
fn pending_commands_survive_a_restart_and_a_reload() {
    let mut mods = host(ScriptAuthority::Host, rules(&["order"]));
    let main = "function go(n) regia.command('order', n) end";
    mods.load(mod_files("m", "1", main), &mut NoWorld).expect("loads");
    mods.call_function(&mut NoWorld, "m", "go", ScriptValue::Number(1.0))
        .expect("go");
    mods.restart("m", &mut NoWorld).expect("restarts");
    mods.call_function(&mut NoWorld, "m", "go", ScriptValue::Number(2.0))
        .expect("go");
    let outcome = mods
        .reload("m", Some(mod_files("m", "2", main)), &mut NoWorld)
        .expect("a known mod");
    assert!(outcome.ok, "{outcome:?}");
    mods.call_function(&mut NoWorld, "m", "go", ScriptValue::Number(3.0))
        .expect("go");
    let values: Vec<ScriptValue> = mods.drain_host_commands().into_iter().map(|c| c.value).collect();
    assert_eq!(
        values,
        vec![
            ScriptValue::Number(1.0),
            ScriptValue::Number(2.0),
            ScriptValue::Number(3.0)
        ]
    );
}

/// The snapshot hook: a changed snapshot calls `on_snapshot` with it and `regia.snapshot()`
/// returns the same table without another build; an unchanged one is a no-op (no hook, no
/// rebuild). Both runtimes receive snapshots.
#[test]
fn an_unchanged_snapshot_is_not_rebuilt_and_the_hook_sees_changes_only() {
    for authority in [ScriptAuthority::Host, ScriptAuthority::Presentation] {
        let mut mods = host(authority, HostCommandRules::default());
        let main = r#"
            hooks = 0
            seen_tick = -1
            same_table = false
            assert(regia.snapshot() == nil)
            function on_snapshot(snapshot)
              hooks = hooks + 1
              seen_tick = snapshot.tick
              same_table = rawequal(snapshot, regia.snapshot())
            end
            function read() return regia.snapshot().tick end
        "#;
        mods.load(mod_files("m", "1", main), &mut NoWorld).expect("loads");
        let snap = |tick: f64| map(&[("tick", ScriptValue::Number(tick))]);
        assert_eq!(mods.publish_snapshot(&mut NoWorld, snap(1.0)), Ok(true));
        assert_eq!(mods.snapshot_generation(), 1);
        assert_eq!(number_global(&mods, "m", "hooks"), Some(1.0));
        assert_eq!(number_global(&mods, "m", "seen_tick"), Some(1.0));
        let builds = mods.get("m").expect("loaded").snapshot_builds();
        assert_eq!(builds, 1, "{authority:?}");
        for _ in 0..3 {
            assert_eq!(mods.publish_snapshot(&mut NoWorld, snap(1.0)), Ok(false));
            assert_eq!(
                mods.call_function(&mut NoWorld, "m", "read", ScriptValue::Nil),
                Ok(ScriptValue::Number(1.0))
            );
        }
        assert_eq!(mods.snapshot_generation(), 1);
        assert_eq!(number_global(&mods, "m", "hooks"), Some(1.0));
        assert_eq!(mods.get("m").expect("loaded").snapshot_builds(), 1);
        assert_eq!(mods.publish_snapshot(&mut NoWorld, snap(2.0)), Ok(true));
        assert_eq!(number_global(&mods, "m", "hooks"), Some(2.0));
        assert_eq!(number_global(&mods, "m", "seen_tick"), Some(2.0));
        assert_eq!(mods.get("m").expect("loaded").snapshot_builds(), 2);
        let globals = mods.globals("m", None, 512).expect("globals");
        let same = globals
            .iter()
            .find(|e| e.key == "same_table")
            .and_then(|e| e.value.clone());
        assert_eq!(same, Some(ScriptValue::Bool(true)));
    }
}

/// A mod's writes to its snapshot table stay in that mod: another mod and the host see what was
/// published.
#[test]
fn snapshot_writes_stay_inside_the_mod() {
    let mut mods = host(ScriptAuthority::Host, HostCommandRules::default());
    let vandal = "function on_snapshot(s) s.score = 999; s.extra = true end";
    let reader = "function on_snapshot(s) seen = tostring(s.score) .. ':' .. tostring(s.extra) end";
    mods.load(mod_files("a_vandal", "1", vandal), &mut NoWorld)
        .expect("a");
    mods.load(mod_files("b_reader", "1", reader), &mut NoWorld)
        .expect("b");
    let published = map(&[("score", ScriptValue::Number(5.0))]);
    assert_eq!(mods.publish_snapshot(&mut NoWorld, published.clone()), Ok(true));
    assert_eq!(string_global(&mods, "b_reader", "seen").as_deref(), Some("5:nil"));
    assert_eq!(mods.snapshot(), Some(&published));
}

/// The usual host flow: the snapshot hook reads the state and queues a command from it.
#[test]
fn a_host_snapshot_hook_can_queue_a_command() {
    let mut mods = host(ScriptAuthority::Host, rules(&["order"]));
    let main = "function on_snapshot(s) if s.tick % 2 == 0 then regia.command('order', s.tick) end end";
    mods.load(mod_files("m", "1", main), &mut NoWorld).expect("loads");
    for tick in 1..=4 {
        let snapshot = map(&[("tick", ScriptValue::Number(f64::from(tick)))]);
        assert_eq!(mods.publish_snapshot(&mut NoWorld, snapshot), Ok(true));
    }
    let values: Vec<ScriptValue> = mods.drain_host_commands().into_iter().map(|c| c.value).collect();
    assert_eq!(values, vec![ScriptValue::Number(2.0), ScriptValue::Number(4.0)]);
}

/// An oversized snapshot is refused before any mod sees it; the last good one stays published.
#[test]
fn an_oversized_snapshot_is_refused() {
    let mut mods = host(ScriptAuthority::Host, HostCommandRules::default());
    mods.load(mod_files("m", "1", "function on_snapshot(s) end"), &mut NoWorld)
        .expect("loads");
    let small = map(&[("tick", ScriptValue::Number(1.0))]);
    assert_eq!(mods.publish_snapshot(&mut NoWorld, small.clone()), Ok(true));
    let chunk = "x".repeat(60_000);
    let count = usize::try_from(MAX_SNAPSHOT_BYTES).expect("fits") / chunk.len() + 1;
    let big = ScriptValue::List(vec![ScriptValue::String(chunk); count]);
    let error = mods.publish_snapshot(&mut NoWorld, big).expect_err("too large");
    assert_eq!(error.code, LuaErrorCode::TooLarge);
    assert_eq!(mods.snapshot(), Some(&small));
    assert_eq!(mods.snapshot_generation(), 1);
}

/// A snapshot hook that fails disables that mod only, like any callback; the others still get
/// the snapshot.
#[test]
fn a_failing_snapshot_hook_disables_only_its_mod() {
    let mut mods = host(ScriptAuthority::Host, HostCommandRules::default());
    mods.load(
        mod_files("a", "1", "function on_snapshot(s) error('no') end"),
        &mut NoWorld,
    )
    .expect("a");
    mods.load(
        mod_files("b", "1", "n = 0 function on_snapshot(s) n = n + 1 end"),
        &mut NoWorld,
    )
    .expect("b");
    assert_eq!(
        mods.publish_snapshot(&mut NoWorld, ScriptValue::Number(1.0)),
        Ok(true)
    );
    assert!(matches!(
        mods.get("a").expect("a").status(),
        ModStatus::Disabled { .. }
    ));
    assert_eq!(number_global(&mods, "b", "n"), Some(1.0));
}

/// The default authority is the host (a single-player game is its own authority), and the
/// default rules register no command: a script must be given its commands by the game.
#[test]
fn the_defaults_are_host_with_no_registered_commands() {
    let mut mods = LuaMods::new(1, LuaLimits::default());
    assert_eq!(mods.authority(), ScriptAuthority::Host);
    assert!(mods.host_command_rules().names.is_empty());
    let main = "function go() return select(2, regia.command('anything', 1)) end";
    mods.load(mod_files("m", "1", main), &mut NoWorld).expect("loads");
    assert_eq!(
        mods.call_function(&mut NoWorld, "m", "go", ScriptValue::Nil),
        Ok(ScriptValue::String("unknown_command".into()))
    );
}
