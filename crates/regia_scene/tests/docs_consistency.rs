// SPDX-License-Identifier: MPL-2.0
//! Pins the type paths and format version that the specs and the user guide quote to the real
//! constants, so a doc that names a key the loader does not read fails here.

use std::path::PathBuf;

fn read(rel: &str) -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

#[test]
fn spec_14_uses_the_type_paths_the_loader_resolves() {
    let spec = read("docs/specs/14-scene-and-framework.md");
    assert!(spec.contains(&format!("\"{}\"", regia_scene::level::LEVEL_SETTINGS_PATH)));
    assert!(spec.contains("\"regia_scene::transform::Transform\""));
    assert!(!spec.contains("\"regia_scene::Transform\""));
    assert!(!spec.contains("regia_framework::LevelSettings"));
}

#[test]
fn spec_78_streaming_key_is_the_hook_key() {
    let spec = read("docs/specs/78-world-streaming.md");
    assert!(spec.contains("regia_scene::streaming::LevelStreaming"));
    assert!(!spec.contains("`regia_scene::LevelStreaming`"));
}

#[test]
fn user_guide_names_the_current_scene_format_version() {
    let guide = read("book/user-guide/scenes.md");
    let version = regia_scene::format::SCENE_FORMAT_VERSION;
    let want = format!("current scene format is version {version}");
    assert!(guide.contains(&want), "scenes.md must say: {want}");
    let format_rs = read("crates/regia_scene/src/format.rs");
    let banner = format_rs.split("use std::").next().expect("format.rs banner");
    let banner_version = format!("version {version}");
    assert!(
        banner.contains(&banner_version),
        "format.rs module comment must say {banner_version}"
    );
    let schema = read("docs/formats/scene-files.md");
    let example = schema
        .split("```ron")
        .nth(1)
        .expect("scene-files.md schema fence");
    assert!(
        example.contains(&format!("format_version: {version}")),
        "the schema example must show format_version: {version}"
    );
}

#[test]
fn level_module_docs_name_the_loader_key() {
    let src = read("crates/regia_scene/src/level.rs");
    let banner = src.split("use std::").next().expect("level.rs banner");
    assert!(
        banner.contains("LEVEL_SETTINGS_PATH"),
        "the module comment must name LEVEL_SETTINGS_PATH"
    );
    assert!(
        !banner.contains("`regia_scene::LevelSettings`"),
        "the module comment must not present `regia_scene::LevelSettings` as the resources key"
    );
}

#[test]
fn spec_78_omitted_hlod_fade_is_zero_everywhere() {
    let spec = read("docs/specs/78-world-streaming.md");
    assert!(
        !spec.contains("8 desktop, 0 Android"),
        "an omitted fade is not 8 frames on desktop"
    );
    assert!(
        !spec.contains("default 8 on desktop"),
        "an omitted fade is not 8 frames on desktop"
    );
    assert!(
        spec.contains("absent means 0 on every platform"),
        "spec 78 must say an omitted fade is 0 on every platform"
    );
}
