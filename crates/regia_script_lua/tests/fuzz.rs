// SPDX-License-Identifier: MPL-2.0
//! `luamod.ron` under seeded mutation (ADR-1917): `LuaModManifest::parse` returns a manifest or an error,
//! never panics or hangs. The same for the `luastate` blob a save holds (ADR-1943): `decode_luastate` plus
//! each entry's `ModState::check`, the two steps a load runs before it trusts a mod's state. This is the registry's `luamod` target: `regia_script_lua` depends on
//! `regia_agent`, so the registry cannot reach it, and the registry test
//! (`regia_agent/tests/fuzz_targets.rs`) names this test for it.

use regia_script_lua::{
    API_VERSION, LUAMOD_FORMAT_VERSION, LuaLimits, LuaModManifest, LuaMods, LuaStateFile, ModFiles, NoWorld,
    decode_luastate,
};
use regia_test_utils::fuzz::{fuzz_loader, fuzz_text_loader, fuzz_text_values_loader};

/// A manifest with every field, at this build's versions.
fn fixture() -> String {
    format!(
        r#"(
    format_version: {LUAMOD_FORMAT_VERSION},
    id: "example.lights",
    name: "Blinking lights",
    version: "1.0.0",
    entry: "main.lua",
    api_version: {API_VERSION},
    instruction_budget: Some(200000),
    memory_limit_bytes: Some(8388608),
)
"#
    )
}

#[test]
fn luamod_manifest_never_panics() {
    let fixture = fixture();
    LuaModManifest::parse(&fixture).expect("the fixture manifest parses and validates");
    fuzz_text_loader(&fixture, 0x1A_0D, 2000, LuaModManifest::parse);
    fuzz_text_values_loader(&fixture, 0x1A_0E, 1000, LuaModManifest::parse);
}

/// A mod that keeps a counter, a nested table and a string in `regia.state`.
const STATEFUL: &str = r#"
function on_start() regia.state.count = 0 regia.state.bag = { name = "lamp", sizes = { 1, 2, 3 } } end
function on_tick() regia.state.count = regia.state.count + 1 end
"#;

/// The `luastate` blob a host saves after a few ticks of [`STATEFUL`].
fn luastate_fixture() -> Vec<u8> {
    let manifest =
        r#"(format_version: 1, id: "a", name: "a", version: "1.0.0", entry: "main.lua", api_version: 1)"#;
    let mut host = LuaMods::new(1234, LuaLimits::default());
    host.load(
        ModFiles::from_map([
            ("luamod.ron".to_owned(), manifest.to_owned()),
            ("main.lua".to_owned(), STATEFUL.to_owned()),
        ]),
        &mut NoWorld,
    )
    .expect("the fixture mod loads");
    for _ in 0..3 {
        host.call_tick(&mut NoWorld, 0.1);
    }
    host.save_state(&mut NoWorld).0
}

/// What a load does with untrusted state bytes before it uses them.
fn luastate_load(bytes: &[u8]) -> Result<LuaStateFile, String> {
    let limits = LuaLimits::default();
    let file = decode_luastate(bytes, &limits).map_err(|e| e.to_string())?;
    for state in &file.mods {
        state.check(&limits).map_err(|e| e.to_string())?;
    }
    Ok(file)
}

#[test]
fn luastate_never_panics() {
    let fixture = luastate_fixture();
    let file = luastate_load(&fixture).expect("the fixture state decodes and checks");
    assert_eq!(file.mods.len(), 1, "one mod saved");
    fuzz_loader(&fixture, 0x1A_57, 2000, luastate_load);
}

/// ADR-2084: the registry targets `lua.luamod` and `lua.luastate` (the second runs the whole save loader:
/// decode, check, restore into a running mod, save again) load their fixtures, survive the time-boxed harness, and
/// hold the registry's round-trip property over many seeds.
#[test]
fn registry_targets_run_clean() {
    use regia_agent::fuzz_targets::{FuzzInput, check_round_trip_bytes, check_round_trip_text, run};
    let targets = regia_script_lua::fuzz::fuzz_targets();
    assert_eq!(targets.len(), 2);
    for t in &targets {
        match t.input {
            FuzzInput::Text { fixture, load } => {
                let canonical = load(&fixture()).expect("the fixture loads");
                check_round_trip_text(load, &canonical).expect("the fixture round trips");
                fuzz_text_loader(&fixture(), 0x1A_60, 400, load);
                fuzz_text_values_loader(&fixture(), 0x1A_61, 400, load);
            }
            FuzzInput::Bytes { fixture, load } => {
                let canonical = load(&fixture()).expect("the fixture loads");
                check_round_trip_bytes(load, &canonical).expect("the fixture round trips");
                fuzz_loader(&fixture(), 0x1A_62, 400, load);
            }
        }
        for seed in 1..=40 {
            let report = run(t, None, seed, 25).expect("built-in fixture");
            assert!(report.is_clean(), "{} seed {seed}: {report:?}", t.name);
        }
    }
}

/// The Lua soak (ADR-2084): both registry targets over `REGIA_FUZZ_SOAK_SEEDS` seeds (default 3000) of 4 mutations
/// each; every finding is listed with its seed (`fuzz.run` reproduces it). Ignored in the gate; `cargo test -p
/// regia_script_lua --test fuzz -- --ignored lua_soak --nocapture`.
#[test]
#[ignore = "long soak; run by hand"]
fn lua_soak_over_many_seeds() {
    let seeds: u64 = std::env::var("REGIA_FUZZ_SOAK_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000);
    let mut findings = Vec::new();
    for t in regia_script_lua::fuzz::fuzz_targets() {
        let (mut accepted, mut rejected) = (0u64, 0u64);
        for seed in 1..=seeds {
            let report = regia_agent::fuzz_targets::run(&t, None, seed, 4).expect("built-in fixture");
            accepted += u64::from(report.accepted);
            rejected += u64::from(report.rejected);
            if !report.is_clean() {
                findings.push(format!("{} seed {seed}: {:?}", t.name, report.first_failure));
            }
        }
        eprintln!(
            "lua soak {}: {seeds} seeds, accepted {accepted}, rejected {rejected}",
            t.name
        );
    }
    assert!(findings.is_empty(), "findings:\n{}", findings.join("\n"));
}
