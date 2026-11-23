// SPDX-License-Identifier: MIT-0
//! The `net-game` project template (`crates/regia_templates/templates/net_game`) is this sample with
//! its names replaced: `Net Game` by `{{name}}` and `net_game` by `{{crate_name}}`. This test keeps
//! the two identical, so the template's code is the code CI compiles and runs here (ADR-2039).
//! After editing the sample, copy each file over with those two replacements.

use std::path::Path;

/// Sample file, template file.
const FILES: &[(&str, &str)] = &[
    ("src/lib.rs", "game/src/lib.rs"),
    ("src/lobby.rs", "game/src/lobby.rs"),
    ("src/view.rs", "game/src/view.rs"),
    ("src/hud.rs", "game/src/hud.rs"),
    ("src/server.rs", "game/src/server.rs"),
    ("tests/e2e.rs", "game/tests/e2e.rs"),
    ("tests/hosted.rs", "game/tests/hosted.rs"),
    ("assets/levels/arena.level.ron", "assets/levels/arena.level.ron"),
    ("src/bin/server.rs", "server/src/main.rs"),
    ("src/bin/client.rs", "client/src/main.rs"),
];

/// Sample file, block of it that the template leaves out on purpose. Each block must appear in the
/// sample exactly once; everything else still has to match line for line. The Steam template-match
/// leg is the sample's test tool for the owner's two-account run (ADR-5375), not template code: the
/// template has no `steam` feature, so a generated project would warn on the `cfg` (ADR-5550).
const SAMPLE_ONLY: &[(&str, &str)] = &[
    (
        "src/lib.rs",
        "// The template match over Steam, the N08 leg of the owner's two-account run (ADR-5375).\n\
         #[cfg(feature = \"steam\")]\n\
         pub mod steam_leg;\n",
    ),
    // The windowed client's Steam readout for the owner's overlay check (ADR-7317): sample only,
    // for the same reason as the Steam leg.
    (
        "src/lib.rs",
        "// The windowed client's SteamPlugin and its HUD line for the overlay check (ADR-7317).\n\
         #[cfg(feature = \"steam\")]\n\
         pub mod steam_overlay;\n",
    ),
    ("src/bin/client.rs", CLIENT_STEAM_DOC),
    ("src/bin/client.rs", CLIENT_STEAM_LIST),
    ("src/bin/client.rs", CLIENT_STEAM_PLUGIN),
];

/// The client's Steam paragraph (ADR-7317).
const CLIENT_STEAM_DOC: &str = "//!
//! Steam builds (sample only, ADR-7317): with `--features steam` the windowed client also runs
//! `regia::steam::SteamPlugin` and a HUD line with the overlay state (`net_game::steam_overlay`);
//! `--features steamworks` talks to the running Steam client, `--features steam` alone runs on
//! the mock backend. `--list-adapters` prints the adapters numbered for `REGIA_GPU_INDEX` and exits.
";

/// The client's `--list-adapters` (ADR-7317).
const CLIENT_STEAM_LIST: &str =
    "    // Steam builds (sample only, ADR-7317): `--list-adapters` prints the adapters and exits.
    #[cfg(feature = \"steam\")]
    if let Some(code) = net_game::steam_overlay::list_adapters_flag(&mut raw) {
        return code;
    }
";

/// The client's SteamPlugin (ADR-7317).
const CLIENT_STEAM_PLUGIN: &str =
    "            // Steam builds (sample only, ADR-7317): SteamPlugin and the HUD's overlay line.
            #[cfg(feature = \"steam\")]
            app.add_plugins(net_game::steam_overlay::NetGameSteam::for_this_build());
";

#[test]
fn the_template_is_this_sample_with_its_names_replaced() {
    let sample = Path::new(env!("CARGO_MANIFEST_DIR"));
    let template = sample.join("../../crates/regia_templates/templates/net_game");
    for (ours, theirs) in FILES {
        let mut ours_text = std::fs::read_to_string(sample.join(ours)).unwrap_or_default();
        for (file, block) in SAMPLE_ONLY.iter().filter(|(file, _)| file == ours) {
            assert_eq!(
                ours_text.matches(block).count(),
                1,
                "{file} has its sample-only block once"
            );
            ours_text = ours_text.replacen(block, "", 1);
        }
        let theirs_text = std::fs::read_to_string(template.join(theirs)).unwrap_or_default();
        assert!(!ours_text.is_empty(), "{ours} is readable");
        assert!(!theirs_text.is_empty(), "template {theirs} is readable");
        let back = theirs_text
            .replace("{{crate_name}}", "net_game")
            .replace("{{name}}", "Net Game");
        if back != ours_text {
            let line = back
                .lines()
                .zip(ours_text.lines())
                .position(|(a, b)| a != b)
                .unwrap_or(back.lines().count().min(ours_text.lines().count()));
            panic!("template {theirs} differs from {ours} at line {}", line + 1);
        }
    }
}
