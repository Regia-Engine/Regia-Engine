// SPDX-License-Identifier: MPL-2.0
//! Memory a mod can reach past its Lua memory cap (`docs/specs/84-lua-mods.md` § 5). A Lua string is
//! interned, so a table of a million references to one long string costs the mod a few MiB, and every
//! host function that copies those strings onto the Rust heap multiplies them.
//!
//! One test in this binary, so nothing else allocates while it measures. Bytes, not clocks.

use regia_script_lua::{LuaLimits, LuaMods, ModFiles, NoWorld};

use regia_test_utils::alloc_counter::{self, CountingAlloc};

// The shared counting allocator (ADR-3291): the live bytes of the process and their peak.
#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

/// Live bytes of the process.
fn live() -> usize {
    alloc_counter::process_live_bytes().max(0) as usize
}

const MIB: usize = 1024 * 1024;
/// Room for a state at its 16 MiB cap, what the host keeps of it, and the test's own text.
const SLACK: usize = 48 * MIB;

fn mod_files(id: &str, main: &str) -> ModFiles {
    let manifest = format!(
        r#"(format_version: 1, id: "{id}", name: "{id}", version: "1.0.0", entry: "main.lua", api_version: 1)"#
    );
    ModFiles::from_map([
        ("luamod.ron".to_owned(), manifest),
        ("main.lua".to_owned(), main.to_owned()),
    ])
}

/// Runs `step`, then returns how far the live bytes peaked above where they started.
fn peak_of(step: impl FnOnce()) -> usize {
    let ((), peak) = alloc_counter::process_peak_bytes(step);
    peak as usize
}

#[test]
fn nothing_a_mod_does_copies_its_strings_past_the_rust_heap_bounds() {
    // Iterators a mod keeps: each one held a copy of its subject, for as long as Lua kept it.
    let mut mods = LuaMods::new(1, LuaLimits::default());
    let base = live();
    mods.load(
        mod_files(
            "iter",
            "local s, kept = string.rep('x', 1048576), {}
             for i = 1, 500 do kept[i] = string.gmatch(s, 'x') end
             hold = kept",
        ),
        &mut NoWorld,
    )
    .expect("loads");
    let retained = live().saturating_sub(base);
    assert!(retained < SLACK, "gmatch iterators keep {} MiB", retained / MIB);
    drop(mods);

    // `regia.log` with thousands of references to one long string.
    let chunk = "local s, t = string.rep('x', 1048576), {}
                 for i = 1, 2000 do t[i] = s end
                 regia.log(unpack(t))";
    let peak = peak_of(|| {
        let outcome = LuaMods::new(1, LuaLimits::default())
            .run_chunk(&mut NoWorld, None, chunk)
            .expect("scratch");
        assert!(outcome.ok, "{:?}", outcome.error);
    });
    assert!(peak < SLACK, "regia.log peaks at {} MiB", peak / MIB);

    // An event payload of the same kind: converted in full before its size was looked at.
    let chunk = "local s, t = string.rep('x', 60000), {}
                 for i = 1, 4000 do t[i] = s end
                 return pcall(regia.emit, 'e', t)";
    let peak = peak_of(|| {
        let outcome = LuaMods::new(1, LuaLimits::default())
            .run_chunk(&mut NoWorld, None, chunk)
            .expect("scratch");
        assert!(outcome.ok, "{:?}", outcome.error);
    });
    assert!(peak < SLACK, "regia.emit peaks at {} MiB", peak / MIB);

    // `lua.run` results: each is checked alone, and a chunk can return thousands.
    let chunk = "local s, t = string.rep('x', 60000), {}
                 for i = 1, 200 do t[i] = s end
                 return t, t, t, t, t, t, t, t, t, t, t, t, t, t, t, t, t, t, t, t";
    let peak = peak_of(|| {
        let outcome = LuaMods::new(1, LuaLimits::default())
            .run_chunk(&mut NoWorld, None, chunk)
            .expect("scratch");
        assert!(outcome.ok, "{:?}", outcome.error);
    });
    assert!(peak < SLACK, "lua.run results peak at {} MiB", peak / MIB);

    // `lua.globals` with a small limit, on a table of references to one long string.
    let mut mods = LuaMods::new(1, LuaLimits::default());
    mods.load(
        mod_files(
            "wide",
            "local s = string.rep('x', 60000)
             wide = {}
             for i = 1, 50000 do wide[i] = s end",
        ),
        &mut NoWorld,
    )
    .expect("loads");
    let peak = peak_of(|| {
        let listed = mods.globals("wide", Some("wide"), 8).expect("globals");
        assert_eq!(listed.len(), 8);
    });
    assert!(peak < SLACK, "lua.globals peaks at {} MiB", peak / MIB);
    drop(mods);

    // Events no game drains: the plugin never does, so nothing else bounds them.
    let mut mods = LuaMods::new(1, LuaLimits::default());
    let base = live();
    mods.load(
        mod_files(
            "chatty",
            "local v = {}
             for i = 1, 16 do v[i] = string.rep('x', 60000) .. i end
             function on_update() regia.emit('e', v) end",
        ),
        &mut NoWorld,
    )
    .expect("loads");
    for _ in 0..200 {
        mods.call_update(&mut NoWorld, 0.0);
    }
    let held = live().saturating_sub(base);
    assert!(
        held < 64 * MIB + SLACK,
        "undrained events hold {} MiB",
        held / MIB
    );
    assert!(mods.emitted_dropped() > 0, "200 MiB of events fit in the bounds");
    assert_eq!(mods.emitted_held() as u64 + mods.emitted_dropped(), 200);
    let held_events = mods.emitted_held();
    assert_eq!(mods.drain_emitted().len(), held_events);
    assert_eq!(mods.emitted_held(), 0);
    mods.call_update(&mut NoWorld, 0.0);
    assert_eq!(
        mods.drain_emitted().len(),
        1,
        "the queue works again once drained"
    );
}
