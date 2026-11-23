// SPDX-License-Identifier: MPL-2.0
//! Themes written before the rename to Regia Engine still load (ADR-1388, ADR-1429).
//!
//! The built-in themes were renamed with the engine. A game theme that `extends` a built-in by
//! its old name, and an agent call or saved setting that names one, must keep finding it. The
//! fixture is the v0.1.29 example theme, kept byte for byte.

use regia_core::brand::{LEGACY_THEME_PREFIX, THEME_PREFIX};
use regia_ui::theme_file::{Scheme, ThemeSet};

const LEGACY_GAME_THEME: &str = include_str!("fixtures/legacy-brand/my-game.theme.ron");

#[test]
fn a_theme_extending_a_legacy_builtin_name_resolves() {
    let legacy_parent = format!("{LEGACY_THEME_PREFIX}dark");
    let current_parent = format!("{THEME_PREFIX}dark");
    assert!(
        LEGACY_GAME_THEME.contains(&format!("extends: Some(\"{legacy_parent}\")")),
        "the fixture names the parent by its old name"
    );
    let mut set = ThemeSet::with_builtins();
    let name = set
        .load_ron(LEGACY_GAME_THEME, "my-game.theme.ron")
        .unwrap_or_else(|e| panic!("a v0.1.29 game theme loads: {e}"));
    assert_eq!(name, "my-game");
    let game = set.resolve("my-game", None).expect("resolves");
    assert_eq!(game.chain, ["my-game".to_owned(), current_parent.clone()]);
    assert_eq!(
        game.scheme,
        Scheme::Dark,
        "scheme inherited from the renamed parent"
    );
    let parent = set.resolve(&current_parent, None).expect("built-in");
    assert_eq!(
        game.color("color.surface"),
        parent.color("color.surface"),
        "unlisted tokens come from the renamed built-in"
    );
}

#[test]
fn legacy_builtin_names_find_the_renamed_builtins() {
    let set = ThemeSet::with_builtins();
    for suffix in ["dark", "light", "high-contrast", "light-high-contrast"] {
        let legacy = format!("{LEGACY_THEME_PREFIX}{suffix}");
        let current = format!("{THEME_PREFIX}{suffix}");
        let file = set
            .get(&legacy)
            .unwrap_or_else(|| panic!("{legacy} finds a built-in"));
        assert_eq!(file.name, current);
        let resolved = set.resolve(&legacy, None).expect("resolves");
        assert_eq!(resolved.chain.first(), Some(&current));
    }
    assert!(set.get(&format!("{LEGACY_THEME_PREFIX}no-such-theme")).is_none());
    assert!(set.resolve("no-such-theme", None).is_err());
}
