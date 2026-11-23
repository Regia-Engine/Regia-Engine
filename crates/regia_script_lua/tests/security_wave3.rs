// SPDX-License-Identifier: MPL-2.0
//! The v0.5 security review of mods (ADR-5870): component update passes share one budget, and a
//! Lua mod's files are measured before they are read from `mods://`.

mod common;

use common::{TestWorld, mod_files};
use regia_asset::mods_vfs::{ModSource, ModsVfs};
use regia_script_lua::{FilesError, LuaLimits, LuaMods, ModFiles, ModStatus};
use regia_test_utils::unique_temp_dir;

/// ADR-5870 § 3: each component's `on_update` pass began a fresh instruction budget and watchdog,
/// so a mod declaring 64 components with a hook each ran 64 full budgets per frame (event
/// delivery already shared one). Eight passes that each fit the budget alone, and together do
/// not, now end the mod at the budget.
#[test]
fn component_passes_share_one_budget_per_update() {
    const BUDGET: u64 = 100_000;
    let limits = LuaLimits {
        instruction_budget: BUDGET,
        ..LuaLimits::default()
    };
    // Each hook runs about 30,000 loop instructions: one fits the budget, eight do not.
    let mut main = String::new();
    for d in 1..=8 {
        main.push_str(&format!(
            r#"regia.component("burn{d}", {{
                props = {{ n = {{ type = "number", default = 0 }} }},
                on_update = function(e, dt) for i = 1, 30000 do end passes = (passes or 0) + 1 end,
            }})
            "#
        ));
    }
    main.push_str(
        r#"function on_start()
            for d = 1, 8 do regia.spawn("e" .. d, regia.vec3(0, 0, 0)):add("burn" .. d) end
        end"#,
    );
    let mut mods = LuaMods::new(1, limits);
    let mut world = TestWorld::default();
    mods.load(mod_files("burn", "1.0.0", &main), &mut world)
        .expect("loads");
    assert!(world.position_of("e8").is_some(), "every component has a carrier");

    // One hook alone fits: a control mod with a single burning component stays active.
    let single = r#"regia.component("burn1", {
            props = { n = { type = "number", default = 0 } },
            on_update = function(e, dt) for i = 1, 30000 do end end,
        })
        function on_start() regia.spawn("e", regia.vec3(0, 0, 0)):add("burn1") end"#;
    let mut control = LuaMods::new(1, limits);
    let mut control_world = TestWorld::default();
    control
        .load(mod_files("one", "1.0.0", single), &mut control_world)
        .expect("loads");
    control.call_update(&mut control_world, 0.016);
    let one = control.get("one").expect("loaded");
    assert!(one.is_active(), "{:?}", one.errors().last());
    assert!(one.counters().last_instructions <= BUDGET);

    mods.call_update(&mut world, 0.016);
    let module = mods.get("burn").expect("loaded");
    match module.status() {
        ModStatus::Disabled { code, .. } => assert_eq!(code.code(), "lua_instruction_budget"),
        other => panic!("eight passes ran on eight budgets: {other:?}"),
    }
    assert!(module.counters().peak_instructions <= BUDGET + 1_000);
}

/// ADR-5870 § 4: `ModFiles::from_mods_vfs` read each file whole before it compared the total with
/// `max_bytes`, so a Workshop item's multi-GiB `main.lua` (or a pak entry decompressing to it)
/// was allocated before the refusal. The size the mount recorded is checked first: here the
/// file is changed after the mount, and the refusal is `TooLarge` without the read that would
/// have seen the change.
#[test]
fn a_lua_file_over_the_cap_is_refused_before_it_is_read() {
    let dir = unique_temp_dir("lua-mods-vfs-cap");
    std::fs::write(dir.join("luamod.ron"), common::manifest("big.mod", "1.0.0")).expect("manifest");
    let big = vec![b'-'; 2 << 20];
    std::fs::write(dir.join("main.lua"), &big).expect("main");
    let mut vfs = ModsVfs::new();
    vfs.mount("big.mod", ModSource::Dir(dir.clone())).expect("mount");
    // Under a cap that holds it, the mod reads and loads (the file is one long comment).
    let files = ModFiles::from_mods_vfs(&vfs, "big.mod", 8 << 20).expect("reads");
    common::host()
        .load(files, &mut TestWorld::default())
        .expect("loads");
    // Same size, other bytes: a read now refuses it as changed.
    std::fs::write(dir.join("main.lua"), vec![b'+'; 2 << 20]).expect("rewrite");
    match ModFiles::from_mods_vfs(&vfs, "big.mod", 1 << 20) {
        Err(FilesError::TooLarge { max, .. }) => assert_eq!(max, 1 << 20),
        other => panic!("expected TooLarge before the read, got {other:?}"),
    }
    // Under a cap that holds it, the read happens and sees the change.
    assert!(matches!(
        ModFiles::from_mods_vfs(&vfs, "big.mod", 8 << 20),
        Err(FilesError::Io { .. })
    ));
    let _ = std::fs::remove_dir_all(&dir);
}
