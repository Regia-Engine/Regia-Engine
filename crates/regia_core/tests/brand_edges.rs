// SPDX-License-Identifier: MPL-2.0
//! `brand` rewrites at their edges: type paths inside generics, tuples, arrays and trait objects,
//! filter directives with odd spacing, and user-directory resolution when the legacy entry is not
//! a directory.

use std::borrow::Cow;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use regia_core::brand::{
    UserDirSource, current_log_filter, current_log_target, pick_legacy_path, rebrand_type_path,
    resolve_user_app_dir,
};

fn temp_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "regia-brand-{tag}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn rebrand(path: &str) -> String {
    rebrand_type_path(path, "regia_", "ferrite_", "regia", "ferrite").into_owned()
}

#[test]
fn type_paths_are_rewritten_inside_every_composite() {
    assert_eq!(
        rebrand("alloc::vec::Vec<ferrite_scene::Node>"),
        "alloc::vec::Vec<regia_scene::Node>"
    );
    assert_eq!(
        rebrand("(ferrite_a::X, ferrite_b::Y)"),
        "(regia_a::X, regia_b::Y)"
    );
    assert_eq!(rebrand("[ferrite_a::X; 3]"), "[regia_a::X; 3]");
    assert_eq!(rebrand("&'static ferrite::Thing"), "&'static regia::Thing");
    assert_eq!(
        rebrand("dyn ferrite_a::Trait + core::marker::Send"),
        "dyn regia_a::Trait + core::marker::Send"
    );
    assert_eq!(
        rebrand("<ferrite_a::X as ferrite_b::Trait>::Out"),
        "<regia_a::X as regia_b::Trait>::Out"
    );
    assert_eq!(
        rebrand("Result<ferrite::A, ferrite_x::B>"),
        "Result<regia::A, regia_x::B>"
    );
}

#[test]
fn only_the_crate_segment_is_rewritten() {
    // A module or type that happens to carry the legacy spelling after the crate is left alone.
    assert_eq!(rebrand("my_game::ferrite_util::X"), "my_game::ferrite_util::X");
    assert_eq!(rebrand("my_game::ferrite::X"), "my_game::ferrite::X");
    assert_eq!(
        rebrand("ferrite_scene::ferrite_inner::X"),
        "regia_scene::ferrite_inner::X"
    );
    // Not followed by `::`: a bare type or a prefix of a longer word.
    assert_eq!(rebrand("ferrite"), "ferrite");
    assert_eq!(rebrand("ferrite_scene"), "ferrite_scene");
    assert_eq!(rebrand("ferritex::X"), "ferritex::X");
    assert_eq!(rebrand("xferrite_a::X"), "xferrite_a::X");
}

#[test]
fn odd_strings_pass_through_untouched_and_borrowed() {
    for text in [
        "",
        "::",
        "<>",
        "é::ferrite_é::X",
        "日本語::X",
        "a b c",
        "ferrite_",
    ] {
        match rebrand_type_path(text, "regia_", "ferrite_", "regia", "ferrite") {
            Cow::Borrowed(same) => assert_eq!(same, text),
            Cow::Owned(changed) => panic!("{text:?} changed to {changed:?}"),
        }
    }
    // Non-ASCII around a rewritten crate keeps every character.
    assert_eq!(rebrand("é<ferrite_a::X>日"), "é<regia_a::X>日");
    // Identical spellings never allocate.
    assert!(matches!(
        rebrand_type_path("ferrite_a::X", "ferrite_", "ferrite_", "ferrite", "ferrite"),
        Cow::Borrowed(_)
    ));
}

#[test]
fn rebranding_is_idempotent() {
    for text in [
        "ferrite_a::X<ferrite::Y>",
        "regia_a::X",
        "plain",
        "(ferrite_a::X, regia_b::Y)",
    ] {
        let once = rebrand(text);
        assert_eq!(rebrand(&once), once, "{text}");
    }
}

#[test]
fn log_targets_name_only_whole_crates() {
    assert_eq!(current_log_target("ferrite").as_deref(), Some("regia"));
    assert_eq!(
        current_log_target("ferrite::boot").as_deref(),
        Some("regia::boot")
    );
    assert_eq!(
        current_log_target("ferrite_render::pass").as_deref(),
        Some("regia_render::pass")
    );
    assert_eq!(
        current_log_target("ferritex"),
        None,
        "a longer crate name is not the engine crate"
    );
    assert_eq!(current_log_target("regia_render"), None, "already current");
    assert_eq!(current_log_target(""), None);
    assert_eq!(current_log_target("my_game::ferrite"), None);
}

#[test]
fn log_filters_with_odd_spacing_and_directive_forms() {
    assert_eq!(current_log_filter("info"), "info");
    assert!(matches!(current_log_filter("info,warn"), Cow::Borrowed(_)));
    assert_eq!(current_log_filter(""), "");
    assert_eq!(
        current_log_filter("info,ferrite_render=debug"),
        "info,ferrite_render=debug,regia_render=debug"
    );
    // Whitespace around a directive is kept for the legacy entry and ignored for the mapping.
    assert_eq!(
        current_log_filter("info, ferrite_render=trace "),
        "info, ferrite_render=trace ,regia_render=trace"
    );
    // A span directive keeps its bracketed part.
    assert_eq!(
        current_log_filter("ferrite_scene[load]=debug"),
        "ferrite_scene[load]=debug,regia_scene[load]=debug"
    );
    // A bare target, empty directives and a longer unrelated crate.
    assert_eq!(current_log_filter("ferrite"), "ferrite,regia");
    assert_eq!(current_log_filter("info,,ferritex=debug"), "info,,ferritex=debug");
    assert_eq!(
        current_log_filter("ferrite::boot=warn"),
        "ferrite::boot=warn,regia::boot=warn"
    );
}

#[test]
fn pick_legacy_path_prefers_current_unless_only_legacy_exists() {
    let pick = |current: &str, legacy: &str, present: &[&str]| {
        let root = PathBuf::from("/p");
        pick_legacy_path(&root, current, legacy, |p| {
            present.iter().any(|n| p == root.join(n))
        })
    };
    assert_eq!(
        pick("new", "old", &["new", "old"]),
        (PathBuf::from("/p/new"), false)
    );
    assert_eq!(pick("new", "old", &["old"]), (PathBuf::from("/p/old"), true));
    assert_eq!(pick("new", "old", &[]), (PathBuf::from("/p/new"), false));
    assert_eq!(pick("same", "same", &[]), (PathBuf::from("/p/same"), false));
}

#[test]
fn a_legacy_entry_that_is_a_file_is_not_migrated() {
    let base = temp_dir("legacyfile");
    fs::create_dir_all(base.join("Ferrite")).expect("legacy root");
    fs::write(base.join("Ferrite/game"), b"not a directory").expect("file");
    let (path, source) = resolve_user_app_dir(&base, "game", "RegiaEngine", "Ferrite");
    assert_eq!(source, UserDirSource::Current);
    assert_eq!(path, base.join("RegiaEngine/game"));
    assert!(!path.exists(), "nothing was created or moved");
    assert!(base.join("Ferrite/game").is_file());
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn migration_failure_uses_the_legacy_directory_in_place() {
    let base = temp_dir("blocked");
    fs::create_dir_all(base.join("Ferrite/game")).expect("legacy dir");
    fs::write(base.join("Ferrite/game/save.dat"), b"data").expect("save");
    // `RegiaEngine` exists as a file, so the destination parent cannot be created.
    fs::write(base.join("RegiaEngine"), b"in the way").expect("blocker");
    let (path, source) = resolve_user_app_dir(&base, "game", "RegiaEngine", "Ferrite");
    assert_eq!(source, UserDirSource::LegacyInPlace);
    assert_eq!(path, base.join("Ferrite/game"));
    assert_eq!(fs::read(path.join("save.dat")).expect("untouched"), b"data");
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn migration_of_a_missing_everything_just_names_the_current_path() {
    let base = temp_dir("nothing");
    let (path, source) = resolve_user_app_dir(&base, "game", "RegiaEngine", "Ferrite");
    assert_eq!(
        (path.clone(), source),
        (base.join("RegiaEngine/game"), UserDirSource::Current)
    );
    assert!(!path.exists());
    let _ = fs::remove_dir_all(&base);
}
