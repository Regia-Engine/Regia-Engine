// SPDX-License-Identifier: MPL-2.0
//! The kind lint at each strictness, unclassified detection, and the project map's stability
//! (spec 101 §§ 5-6).

use std::path::Path;

use super::*;
use crate::kind::KindSource;
use crate::kind::file::KindFile;
use crate::kind::map::{MAP_JSON, MAP_MARKDOWN, build_map, write_map};
use crate::kind::scan::scan_project;
use crate::kind::{KindInfo, KindRegistry};

const CRATE_KIND: &str = r##"(
    format_version: 1,
    name: "Crate",
    icon: "crate",
    color: "#c08a3e",
    description: "A breakable supply crate",
    category: Some("Props"),
    required: ["regia_scene::transform::Transform", "my_game::Health"],
)"##;

const PREFAB_GUID: &str = "8b6f2a11-0000-4000-8000-000000000001";

fn crate_prefab() -> String {
    r#"SceneFile(
    format_version: 8,
    entities: [
        (
            guid: "00000000-0000-4000-8000-0000000000a1",
            name: "Crate",
            components: {
                "regia_scene::transform::Transform": (),
                "regia_scene::kind::KindTag": ("Crate"),
                "my_game::Health": (100),
            },
        ),
        (
            guid: "00000000-0000-4000-8000-0000000000a2",
            name: "Lid",
            parent: Some("00000000-0000-4000-8000-0000000000a1"),
            components: {
                "my_game::Hinge": (),
            },
        ),
    ],
)
"#
    .to_string()
}

/// A level with: a crate instance (inherits the kind), a lamp-trigger (two families, no kind), an
/// enemy spawner with a game component and no kind, a part under a kinded entity, a plain light,
/// a tagged entity missing a required component, and an unknown tag.
fn level() -> String {
    format!(
        r#"SceneFile(
    format_version: 8,
    entities: [
        (guid: "00000000-0000-4000-8000-000000000001", name: "Crate A",
            components: {{ "regia_scene::transform::Transform": () }},
            prefab_instance: Some((prefab: "asset:{PREFAB_GUID}"))),
        (guid: "00000000-0000-4000-8000-000000000002", name: "Lamp trigger",
            components: {{
                "regia_render::light::PointLight": (),
                "regia_physics::components::Collider": (sensor: true),
            }}),
        (guid: "00000000-0000-4000-8000-000000000003", name: "Spawner",
            components: {{ "my_game::SpawnTable": () }}),
        (guid: "00000000-0000-4000-8000-000000000004", name: "Spawner part",
            parent: Some("00000000-0000-4000-8000-000000000005"),
            components: {{ "my_game::Wheel": () }}),
        (guid: "00000000-0000-4000-8000-000000000005", name: "Cart",
            components: {{
                "regia_scene::kind::KindTag": ("Crate"),
                "regia_scene::transform::Transform": (),
            }}),
        (guid: "00000000-0000-4000-8000-000000000006", name: "Sun",
            components: {{ "regia_render::light::DirectionalLight": () }}),
        (guid: "00000000-0000-4000-8000-000000000007", name: "Ghost",
            components: {{ "regia_scene::kind::KindTag": ("Nope") }}),
    ],
)
"#
    )
}

/// Writes a small project to a fresh temp directory and returns its root, which removes itself
/// on drop (a failing assertion must not leave the folder behind on the shared gate hosts).
fn project() -> regia_test_utils::TempDir {
    let root = regia_test_utils::TempDir::new("kind_lint_project");
    let write = |rel: &str, text: &str| {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, text).expect("write");
    };
    write("assets/kinds/crate.kind.ron", CRATE_KIND);
    write("assets/prefabs/crate.prefab.ron", &crate_prefab());
    write(
        "assets/prefabs/crate.prefab.ron.meta",
        &format!("(format_version: 1, id: \"{PREFAB_GUID}\", importer: \"prefab\")"),
    );
    write("assets/levels/yard.level.ron", &level());
    write("Regia.toml", "[project]\nname = \"Kinds\"\n");
    root
}
fn codes(findings: &[Finding], entity_suffix: &str) -> Vec<String> {
    findings
        .iter()
        .filter(|f| f.entity.ends_with(entity_suffix))
        .map(|f| f.code.clone())
        .collect()
}

#[test]
fn off_reports_nothing() {
    let root = project();
    let files = scan_project(&root);
    let catalog = KindCatalog::new(None, &files);
    assert!(lint_project(&files, &catalog, Strictness::Off).is_empty());
}

#[test]
fn warn_and_deny_report_the_same_findings_at_their_severity() {
    let root = project();
    let files = scan_project(&root);
    assert!(files.errors.is_empty(), "{:?}", files.errors);
    assert_eq!(files.prefabs.len(), 1);
    assert!(files.prefabs[0].guid.is_some(), "guid from the .meta");
    let catalog = KindCatalog::new(None, &files);
    let warn = lint_project(&files, &catalog, Strictness::Warn);
    let deny = lint_project(&files, &catalog, Strictness::Deny);
    assert!(!has_errors(&warn), "warn never fails: {warn:#?}");
    assert!(has_errors(&deny));
    assert_eq!(warn.len(), deny.len());

    // The crate instance inherits its kind and its required components through the prefab.
    assert!(codes(&warn, "000000000001").is_empty(), "{warn:#?}");
    // A light and a trigger on one entity: unclassified (kind_missing) plus the suggestion.
    assert_eq!(codes(&warn, "000000000002"), ["kind_missing", "kind_two_roles"]);
    let suggestion = warn
        .iter()
        .find(|f| f.code == "kind_two_roles")
        .expect("suggestion");
    assert_eq!(suggestion.severity, Severity::Note);
    assert!(suggestion.message.contains(TWO_ROLES_SUGGESTION));
    assert!(suggestion.message.contains("light + trigger"));
    // Game components and no kind.
    assert_eq!(codes(&warn, "000000000003"), ["kind_missing"]);
    // A part of a kinded entity is exempt.
    assert!(codes(&warn, "000000000004").is_empty());
    // A tagged entity lacking a required component.
    assert_eq!(codes(&warn, "000000000005"), ["kind_required_missing"]);
    let missing = warn
        .iter()
        .find(|f| f.code == "kind_required_missing")
        .expect("finding");
    assert!(missing.message.contains("my_game::Health"));
    // One engine role only: exempt.
    assert!(codes(&warn, "000000000006").is_empty());
    // An unknown tag.
    assert_eq!(codes(&warn, "000000000007"), ["kind_unknown"]);
    // The prefab file itself is clean (its root carries what the kind requires).
    assert!(!warn.iter().any(|f| f.file.ends_with(".prefab.ron")), "{warn:#?}");
    for f in &deny {
        let expected = if f.code == "kind_two_roles" {
            Severity::Note
        } else {
            Severity::Error
        };
        assert_eq!(f.severity, expected, "{f}");
    }
}

#[test]
fn an_instance_override_of_the_tag_is_noted_and_used() {
    let root = project();
    let level = level().replace(
        r#"prefab_instance: Some((prefab: "asset:8b6f2a11-0000-4000-8000-000000000001"))"#,
        r#"prefab_instance: Some((prefab: "asset:8b6f2a11-0000-4000-8000-000000000001", overrides: [(target: "00000000-0000-4000-8000-0000000000a1", component: "regia_scene::kind::KindTag", path: "", value: ("Barrel"))]))"#,
    );
    std::fs::write(root.join("assets/levels/yard.level.ron"), level).expect("write");
    let files = scan_project(&root);
    let catalog = KindCatalog::new(None, &files);
    let warn = lint_project(&files, &catalog, Strictness::Warn);
    assert_eq!(
        codes(&warn, "000000000001"),
        ["kind_overridden", "kind_unknown"],
        "{warn:#?}"
    );
}

#[test]
fn code_markers_count_as_kinds_when_the_catalog_knows_them() {
    let root = project();
    let level = level().replace(
        r#""my_game::SpawnTable": ()"#,
        r#""my_game::SpawnTable": (), "my_game::Spawner": ()"#,
    );
    std::fs::write(root.join("assets/levels/yard.level.ron"), level).expect("write");
    let files = scan_project(&root);
    let mut registry = KindRegistry::default();
    registry.register_data(KindInfo {
        name: "Spawner".into(),
        icon: "spawn".into(),
        color: [0, 0, 0],
        description: String::new(),
        category: None,
        required: Vec::new(),
        source: KindSource::Data {
            path: "x.kind.ron".into(),
            marker: Some("my_game::Spawner".into()),
        },
    });
    let catalog = KindCatalog::new(Some(&registry), &files);
    let warn = lint_project(&files, &catalog, Strictness::Warn);
    assert!(codes(&warn, "000000000003").is_empty(), "{warn:#?}");
}

#[test]
fn strictness_parses() {
    assert_eq!("warn".parse::<Strictness>(), Ok(Strictness::Warn));
    assert_eq!(Strictness::default(), Strictness::Off);
    assert!("strict".parse::<Strictness>().is_err());
}

fn describe(root: &Path) -> (String, String) {
    let files = scan_project(root);
    let catalog = KindCatalog::new(None, &files);
    let map = build_map("Kinds", &files, &catalog, None, &[]);
    let out = root.join("map-out");
    write_map(&out, &map).expect("write map");
    (
        std::fs::read_to_string(out.join(MAP_MARKDOWN)).expect("md"),
        std::fs::read_to_string(out.join(MAP_JSON)).expect("json"),
    )
}

#[test]
fn describe_output_is_stable_across_two_runs() {
    let root = project();
    let first = describe(&root);
    let second = describe(&root);
    assert_eq!(first, second, "two runs give the same bytes");
    let (md, json) = first;
    assert!(md.contains("| Crate | Props | crate | #c08a3e |"), "{md}");
    assert!(
        md.contains("`assets/levels/yard.level.ron` (2)"),
        "crate instance + cart: {md}"
    );
    assert!(md.contains("Systems are not listed"), "offline map says so");
    assert!(!json.contains(&root.display().to_string()), "no absolute paths");
    let map: crate::kind::map::ProjectMap = serde_json::from_str(&json).expect("json parses");
    assert_eq!(map.map_version, 1);
    assert_eq!(map.levels[0].unclassified, 1, "the lamp trigger");
    assert_eq!(map.prefabs[0].kind.as_deref(), Some("Crate"));
    assert_eq!(map.prefabs[0].levels.len(), 1);
    assert!(
        map.where_to_look
            .iter()
            .any(|e| e.what == "kind" && e.file == "assets/kinds/crate.kind.ron")
    );
    let health = map
        .components
        .iter()
        .find(|c| c.type_path == "my_game::Health")
        .expect("component row");
    assert_eq!(health.required_by, ["Crate"]);
    assert_eq!(health.on_prefabs, ["assets/prefabs/crate.prefab.ron"]);
}

#[test]
fn recorded_code_kinds_are_read_back_from_a_live_map() {
    let root = project();
    let files = scan_project(&root);
    let mut registry = KindRegistry::default();
    registry.register_data(KindInfo {
        name: "Spawner".into(),
        icon: "spawn".into(),
        color: [1, 2, 3],
        description: "Spawns".into(),
        category: None,
        required: Vec::new(),
        source: KindSource::Data {
            path: "x".into(),
            marker: None,
        },
    });
    // A live map lists a code kind; the next offline scan learns its marker from it.
    let mut catalog = KindCatalog::new(Some(&registry), &files);
    catalog.add(KindInfo {
        name: "Turret".into(),
        icon: "weapon".into(),
        color: [9, 9, 9],
        description: "Shoots".into(),
        category: Some("AI".into()),
        required: vec!["my_game::Ammo".into()],
        source: KindSource::Code {
            marker: "my_game::Turret".into(),
        },
    });
    let map = build_map("Kinds", &files, &catalog, None, &[]);
    write_map(&root, &map).expect("write");
    let again = scan_project(&root);
    assert_eq!(again.recorded_kinds.len(), 1);
    assert_eq!(again.recorded_kinds[0].name, "Turret");
    let offline = KindCatalog::new(None, &again);
    assert_eq!(
        offline.by_marker("my_game::Turret").map(|k| k.name.as_str()),
        Some("Turret")
    );
    let _ = KindFile::from_info(&again.recorded_kinds[0]);
}

/// A code kind known to offline tools through its `.kind.ron` `marker` (spec 101 § 2.3).
fn marker_kind(name: &str, marker: &str) -> KindInfo {
    KindInfo {
        name: name.into(),
        icon: "kind".into(),
        color: [0, 0, 0],
        description: String::new(),
        category: None,
        required: Vec::new(),
        source: KindSource::Data {
            path: format!("assets/kinds/{name}.kind.ron"),
            marker: Some(marker.into()),
        },
    }
}

/// Spec 101 § 5: two code markers on one record are `kind_conflict` (warning at `warn`, error at
/// `deny`; the first kind by name is the one checked), and a group node (no role, no game
/// component) is exempt from `kind_missing`.
#[test]
fn two_markers_in_a_file_conflict_and_group_nodes_are_exempt() {
    let root = project();
    let level = level().replace(
        r#""my_game::SpawnTable": ()"#,
        r#""my_game::Spawner": (), "my_game::Turret": ()"#,
    );
    let level = level.replace(
        "    ],\n)\n",
        "        (guid: \"00000000-0000-4000-8000-000000000008\", name: \"Group\",\n            components: { \"regia_scene::transform::Transform\": () }),\n    ],\n)\n",
    );
    assert!(level.contains("Group"), "the group record was added");
    std::fs::write(root.join("assets/levels/yard.level.ron"), level).expect("write");
    let files = scan_project(&root);
    assert!(files.errors.is_empty(), "{:?}", files.errors);
    let mut registry = KindRegistry::default();
    registry.register_data(marker_kind("Spawner", "my_game::Spawner"));
    let mut turret = marker_kind("Turret", "my_game::Turret");
    turret.required = vec!["my_game::Ammo".into()];
    registry.register_data(turret);
    let catalog = KindCatalog::new(Some(&registry), &files);

    let warn = lint_project(&files, &catalog, Strictness::Warn);
    assert_eq!(codes(&warn, "000000000003"), ["kind_conflict"], "{warn:#?}");
    let conflict = warn.iter().find(|f| f.code == "kind_conflict").expect("conflict");
    assert_eq!(conflict.severity, Severity::Warning);
    assert!(
        conflict.message.contains("`Spawner`") && conflict.message.contains("Turret"),
        "{}",
        conflict.message
    );
    // `Spawner` (first by name) is the kind checked, so Turret's required `Ammo` is not reported.
    assert!(
        !warn
            .iter()
            .any(|f| f.code == "kind_required_missing" && f.entity.ends_with("000000000003"))
    );
    assert!(
        codes(&warn, "000000000008").is_empty(),
        "a group node is exempt: {warn:#?}"
    );

    let deny = lint_project(&files, &catalog, Strictness::Deny);
    let conflict = deny.iter().find(|f| f.code == "kind_conflict").expect("conflict");
    assert_eq!(conflict.severity, Severity::Error);
}
