// SPDX-License-Identifier: MPL-2.0
//! `LuaMods::call_function` and the `regia.emit(name, value, target)` target (spec 84 as amended by
//! spec 102 § 4.4, ADR-7268).

use std::collections::BTreeMap;

use regia_script_lua::{LuaErrorCode, LuaLimits, LuaMods, ModFiles, ModStatus, NoWorld, ScriptValue};

fn mods() -> LuaMods {
    let files = ModFiles::from_map([
        (
            "luamod.ron",
            r#"(format_version: 1, id: "doors", name: "Doors", version: "1.0.0", entry: "main.lua", api_version: 1)"#,
        ),
        (
            "main.lua",
            r#"
            function speed(args) return args.speed * 2 end
            function quiet(args) end
            function boom(args) error("kaboom") end
            function aim(args) regia.emit("aimed", args.n, args.who) regia.emit("plain", 0) end
            function misaim(args) regia.emit("bad", 0, 42) end
            "#,
        ),
    ]);
    let mut mods = LuaMods::new(7, LuaLimits::default());
    mods.load(files, &mut NoWorld).expect("the mod loads");
    mods
}

fn table(entries: &[(&str, ScriptValue)]) -> ScriptValue {
    ScriptValue::Map(
        entries
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect::<BTreeMap<_, _>>(),
    )
}

#[test]
fn call_function_returns_the_value_and_an_error_leaves_the_mod_enabled() {
    let mut mods = mods();
    assert_eq!(
        mods.call_function(
            &mut NoWorld,
            "doors",
            "speed",
            table(&[("speed", ScriptValue::Number(3.0))])
        ),
        Ok(ScriptValue::Number(6.0))
    );
    assert_eq!(
        mods.call_function(&mut NoWorld, "doors", "quiet", table(&[])),
        Ok(ScriptValue::Nil)
    );
    let err = mods
        .call_function(&mut NoWorld, "doors", "boom", table(&[]))
        .expect_err("the Lua error comes back");
    assert_eq!(err.code, LuaErrorCode::Runtime);
    let missing = mods
        .call_function(&mut NoWorld, "doors", "nothing", table(&[]))
        .expect_err("no such function");
    assert_eq!(missing.code, LuaErrorCode::Runtime);
    assert_eq!(
        mods.call_function(&mut NoWorld, "ghost", "speed", table(&[]))
            .expect_err("no such mod")
            .code,
        LuaErrorCode::UnknownMod
    );
    assert_eq!(
        mods.get("doors").map(|m| m.status().clone()),
        Some(ModStatus::Active)
    );
    // Still callable after the error.
    assert_eq!(
        mods.call_function(
            &mut NoWorld,
            "doors",
            "speed",
            table(&[("speed", ScriptValue::Number(1.0))])
        ),
        Ok(ScriptValue::Number(2.0))
    );
}

#[test]
fn emit_carries_an_optional_entity_target() {
    let mut mods = mods();
    let who = ScriptValue::Entity(0x0000_0001_0000_0005);
    mods.call_function(
        &mut NoWorld,
        "doors",
        "aim",
        table(&[("n", ScriptValue::Number(1.0)), ("who", who)]),
    )
    .expect("calls");
    let events = mods.drain_emitted();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].name, "aimed");
    assert_eq!(events[0].target, Some(0x0000_0001_0000_0005));
    assert_eq!(events[1].name, "plain");
    assert_eq!(events[1].target, None, "no target");
    // A target that is not an entity is a bad value.
    let err = mods
        .call_function(&mut NoWorld, "doors", "misaim", table(&[]))
        .expect_err("refused");
    assert_eq!(err.code, LuaErrorCode::BadValue);
    assert!(mods.drain_emitted().is_empty());
}
