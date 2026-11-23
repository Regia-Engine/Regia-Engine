// SPDX-License-Identifier: MPL-2.0
//! The Lua host's fuzz targets for the agent registry (ADR-2084): `luamod` (the mod manifest) and `luastate` (the
//! mod state a save holds, through the whole save loader). This crate depends on `regia_agent`, so the built-in
//! registry cannot name them; `LuaModsPlugin` adds them to
//! `regia_agent::fuzz_targets::FuzzTargetExtensions`, and `fuzz.targets` / `fuzz.run` reach them like any other.

use regia_agent::fuzz_targets::{FuzzInput, FuzzTarget, FuzzTargetExtensions};
use regia_app::prelude::*;

use crate::host::LuaMods;
use crate::manifest::{LUAMOD_FORMAT_VERSION, LuaModManifest};
use crate::sandbox::LuaLimits;
use crate::state::LUASTATE_FORMAT_VERSION;
use crate::{ModFiles, NoWorld};

/// A manifest with every field, at this build's versions.
fn luamod_fixture() -> String {
    format!(
        r#"(
    format_version: {LUAMOD_FORMAT_VERSION},
    id: "example.lights",
    name: "Blinking lights",
    version: "1.0.0",
    entry: "main.lua",
    api_version: {api},
    instruction_budget: Some(200000),
    memory_limit_bytes: Some(8388608),
)
"#,
        api = crate::API_VERSION
    )
}

/// Parse and validate, then write the manifest back as RON.
fn luamod_load(text: &str) -> Result<String, String> {
    let manifest = LuaModManifest::parse(text).map_err(|e| e.to_string())?;
    ron::ser::to_string_pretty(&manifest, regia_core::ron_pretty()).map_err(|e| e.to_string())
}

/// A mod that keeps a counter, a nested table and a string in `regia.state`, and migrates old state.
const STATEFUL: &str = r#"
function on_start() regia.state.count = 0 regia.state.bag = { name = "lamp", sizes = { 1, 2, 3 } } end
function on_tick() regia.state.count = regia.state.count + 1 end
function on_migrate(old, from) return old end
"#;

const STATEFUL_MANIFEST: &str =
    r#"(format_version: 1, id: "a", name: "a", version: "1.0.0", entry: "main.lua", api_version: 1)"#;

/// A host with [`STATEFUL`] loaded, or why it did not load (an engine bug for a built-in mod).
fn stateful_host() -> Result<LuaMods, String> {
    let mut host = LuaMods::new(1234, LuaLimits::default());
    host.load(
        ModFiles::from_map([
            ("luamod.ron".to_owned(), STATEFUL_MANIFEST.to_owned()),
            ("main.lua".to_owned(), STATEFUL.to_owned()),
        ]),
        &mut NoWorld,
    )
    .map_err(|e| {
        format!(
            "{}the built-in fixture mod does not load: {e}",
            regia_agent::fuzz_targets::BUG_PREFIX
        )
    })?;
    Ok(host)
}

/// The `luastate` blob a host saves after a few ticks of [`STATEFUL`].
fn luastate_fixture() -> Vec<u8> {
    let Ok(mut host) = stateful_host() else {
        return Vec::new();
    };
    for _ in 0..3 {
        host.call_tick(&mut NoWorld, 0.1);
    }
    host.save_state(&mut NoWorld).0
}

/// The save loader: decode and check the blob (the two steps a load runs before it trusts a mod's state), restore it
/// into a running mod (rebuilds the Lua tables, runs `on_migrate` when the version differs), and save again: the
/// saved bytes are the re-encoding.
fn luastate_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let limits = LuaLimits::default();
    let file = crate::decode_luastate(bytes, &limits).map_err(|e| e.to_string())?;
    for state in &file.mods {
        state.check(&limits).map_err(|e| e.to_string())?;
    }
    let mut host = stateful_host()?;
    let report = host.restore_state(Some(bytes), &mut NoWorld);
    if let Some(error) = report.error {
        return Err(format!(
            "{}decode accepted a blob the loader refused: {error}",
            regia_agent::fuzz_targets::BUG_PREFIX
        ));
    }
    Ok(host.save_state(&mut NoWorld).0)
}

/// The Lua targets.
pub fn fuzz_targets() -> Vec<FuzzTarget> {
    vec![
        FuzzTarget {
            name: "lua.luamod",
            format: Some("luamod"),
            format_version: LUAMOD_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Text {
                fixture: luamod_fixture,
                load: luamod_load,
            },
        },
        FuzzTarget {
            name: "lua.luastate",
            format: Some("luastate"),
            format_version: LUASTATE_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: luastate_fixture,
                load: luastate_load,
            },
        },
    ]
}

/// Adds [`fuzz_targets`] to the app's `FuzzTargetExtensions`.
pub(crate) fn install(world: &mut World) {
    let mut extensions = world.get_resource_or_init::<FuzzTargetExtensions>();
    for target in fuzz_targets() {
        if let Err(e) = extensions.add(target) {
            tracing::warn!(target: "regia_script_lua", "fuzz target not added: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_adds_both_targets_once() {
        let mut world = World::new();
        install(&mut world);
        install(&mut world);
        let names: Vec<&str> = world
            .resource::<FuzzTargetExtensions>()
            .targets()
            .iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(names, ["lua.luamod", "lua.luastate"]);
    }
}
