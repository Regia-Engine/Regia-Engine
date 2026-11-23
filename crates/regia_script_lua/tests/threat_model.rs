// SPDX-License-Identifier: MPL-2.0
//! The sandbox's environment functions (`docs/security/threat-model.md`, ADR-3206). `getfenv` and
//! `setfenv` stay available for mods, so what they can reach is the whole of the question.

use regia_script_lua::{LuaLimits, LuaMods, NoWorld, ScriptValue};

fn eval(chunk: &str) -> ScriptValue {
    let outcome = LuaMods::new(1234, LuaLimits::default())
        .run_chunk(&mut NoWorld, None, chunk)
        .expect("scratch state");
    assert!(outcome.ok, "{chunk}: {:?}", outcome.error);
    outcome.results.into_iter().next().unwrap_or_default()
}

/// No environment a mod can name holds a library the sandbox removed, whichever way it asks.
#[test]
fn no_environment_a_mod_can_reach_holds_a_removed_library() {
    for name in ["os", "io", "debug", "package", "dofile", "loadfile", "newproxy"] {
        for how in [
            format!("return _G.{name}"),
            format!("return getfenv(0).{name}"),
            format!("return getfenv(1).{name}"),
            format!("return getfenv(print).{name}"),
            format!("return getfenv(string.rep).{name}"),
            format!("return getfenv(coroutine.create).{name}"),
            format!("return getfenv(pcall).{name}"),
            format!("return getfenv(xpcall).{name}"),
            format!("local f = function() end; return getfenv(f).{name}"),
        ] {
            assert_eq!(eval(&how), ScriptValue::Nil, "{how}");
        }
    }
}

/// `setfenv` on the budget's `pcall` and `xpcall` wrappers must not disarm them: they hold what they
/// need as upvalues, so a mod that empties their environment still cannot catch the budget's trip.
#[test]
fn setfenv_on_the_budget_wrappers_does_not_let_a_mod_outlive_its_budget() {
    let chunk = r#"
        pcall(setfenv, pcall, {})
        pcall(setfenv, xpcall, {})
        pcall(function() while true do end end)
        return "survived"
    "#;
    let outcome = LuaMods::new(1234, LuaLimits::default())
        .run_chunk(&mut NoWorld, None, chunk)
        .expect("scratch state");
    assert!(
        !outcome.ok,
        "the mod caught its own budget trip: {:?}",
        outcome.results
    );
}
