// SPDX-License-Identifier: MPL-2.0
//! The directory form of a level (spec 37 § 3, § 10 tests 1-5): round trips, deterministic reads,
//! cook equivalence, dirty-only saves and every read-time reason.

use std::path::{Path, PathBuf};

use regia_core::id::Guid;
use regia_ecs::reflect::AppTypeRegistry;
use regia_scene::components::{EntityGuid, Name, Static, Tags};
use regia_scene::cooked::CookedScene;
use regia_scene::level_dir::{
    self, ENTITIES_DIR, HEADER_FILE, LevelDirtySet, entity_file_text, entity_rel_path, read_level_dir,
    read_level_source, write_level_dir, write_level_dir_dirty,
};
use regia_scene::prelude::*;
use regia_test_utils::unique_temp_dir;

const TRANSFORM: &str = "regia_scene::transform::Transform";
const EXAMPLE: &str = include_str!("fixtures/level.example.ron");

fn raw(text: &str) -> Box<ron::value::RawValue> {
    ron::value::RawValue::from_boxed_ron(text.into()).expect("raw ron")
}

fn transform_at(x: f32) -> Box<ron::value::RawValue> {
    raw(&format!(
        "(translation: ({x:?}, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0))"
    ))
}

/// Deterministic pseudo-random guids (xorshift), never nil.
fn guids(seed: u64, n: usize) -> Vec<Guid> {
    let mut s = seed | 1;
    let mut next = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    (0..n)
        .map(|_| Guid::from_u128((u128::from(next()) << 64) | u128::from(next()) | 1))
        .collect()
}

/// A generated level: `n` entities, every fourth a child of an earlier one.
fn generated(n: usize) -> SceneFile {
    let ids = guids(0x5eed, n);
    let mut file = SceneFile::new();
    for (i, guid) in ids.iter().enumerate() {
        let mut entity = SceneEntity {
            guid: *guid,
            name: Some(format!("e{i}")),
            parent: (i % 4 == 3).then(|| ids[i / 2]),
            ..SceneEntity::default()
        };
        entity.components.insert(TRANSFORM.into(), transform_at(i as f32));
        file.entities.push(entity);
    }
    file.resources.insert(
        regia_scene::level::LEVEL_SETTINGS_PATH.into(),
        raw("(gravity: (0.0, -9.81, 0.0))"),
    );
    file
}

fn normalized(mut file: SceneFile) -> String {
    file.entities.sort_by_key(|e| e.guid);
    file.to_ron_string().expect("ron")
}

fn fixtures() -> Vec<(&'static str, SceneFile)> {
    vec![
        (
            "example",
            SceneFile::from_ron_str(EXAMPLE).expect("example parses"),
        ),
        (
            "generated",
            SceneFile::from_ron_str(&generated(2_000).to_ron_string().expect("ron")).expect("gen"),
        ),
    ]
}

fn read_all(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    for entry in walk(dir) {
        let bytes = std::fs::read(&entry).expect("read");
        out.push((entry, bytes));
    }
    out.sort();
    out
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

#[test]
fn split_then_merge_equals_the_normalized_original() {
    for (name, scene) in fixtures() {
        let tmp = unique_temp_dir("level-dir-rt");
        let file = tmp.join(format!("{name}.level.ron"));
        std::fs::write(&file, scene.to_ron_string().expect("ron")).expect("write");
        let dir = tmp.join(format!("{name}.level.d"));
        let report = level_dir::split_level_file(&file, &dir).expect("split");
        assert_eq!(report.written.len(), scene.entities.len() + 1, "{name}");
        let merged = tmp.join("merged.level.ron");
        level_dir::merge_level_dir(&dir, &merged).expect("merge");
        let merged_text = std::fs::read_to_string(&merged).expect("read merged");
        assert_eq!(
            merged_text,
            normalized(scene.clone()),
            "{name}: file -> dir -> file"
        );

        // merge -> split leaves every entity file byte-identical.
        let before = read_all(&dir);
        let again = level_dir::split_level_file(&merged, &dir).expect("split again");
        assert!(
            again.written.is_empty() && again.deleted.is_empty(),
            "{name}: {again:?}"
        );
        assert_eq!(again.unchanged, scene.entities.len() + 1);
        assert_eq!(read_all(&dir), before, "{name}: dir -> file -> dir");
    }
}

#[test]
fn prefab_instances_and_nested_parents_survive_the_round_trip() {
    let scene = SceneFile::from_ron_str(EXAMPLE).expect("example");
    assert!(scene.entities.iter().any(|e| e.prefab_instance.is_some()));
    assert!(scene.entities.iter().any(|e| e.parent.is_some()));
    let tmp = unique_temp_dir("level-dir-prefab");
    write_level_dir(&tmp.join("x.level.d"), &scene).expect("write");
    let (back, report) = read_level_dir(&tmp.join("x.level.d")).expect("read");
    assert_eq!(report.entities, scene.entities.len());
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    let mut expected = scene;
    expected.entities.sort_by_key(|e| e.guid);
    assert_eq!(back, expected);
}

#[test]
fn reads_do_not_depend_on_file_creation_order() {
    let scene = SceneFile::from_ron_str(&generated(300).to_ron_string().expect("ron")).expect("parse");
    let header = level_dir::header_text(&scene).expect("header");
    let mut results = Vec::new();
    for seed in [1_u64, 7, 99] {
        let root = unique_temp_dir("level-dir-shuffle").join("s.level.d");
        std::fs::create_dir_all(&root).expect("mkdir");
        std::fs::write(root.join(HEADER_FILE), &header).expect("header");
        // Fisher-Yates with a seeded xorshift.
        let mut order: Vec<usize> = (0..scene.entities.len()).collect();
        let mut s = seed;
        for i in (1..order.len()).rev() {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            order.swap(i, (s % (i as u64 + 1)) as usize);
        }
        for i in order {
            let entity = &scene.entities[i];
            let path = root.join(entity_rel_path(entity.guid));
            std::fs::create_dir_all(path.parent().expect("shard")).expect("shard dir");
            std::fs::write(&path, entity_file_text(entity).expect("text")).expect("write");
        }
        results.push(read_level_dir(&root).expect("read").0);
    }
    assert_eq!(results[0], results[1]);
    assert_eq!(results[1], results[2]);
    let guids: Vec<Guid> = results[0].entities.iter().map(|e| e.guid).collect();
    let mut sorted = guids.clone();
    sorted.sort();
    assert_eq!(guids, sorted, "entities are in guid order");
}

fn cook_registry() -> AppTypeRegistry {
    let registry = AppTypeRegistry::default();
    {
        let mut r = registry.write();
        regia_scene::components::register_primitives(&mut r);
        r.register::<Transform>();
        r.register::<Name>();
        r.register::<EntityGuid>();
        r.register::<Tags>();
        r.register::<Static>();
        r.register::<smallvec::SmallVec<[Name; 4]>>();
    }
    registry
}

#[test]
fn cooked_bytes_are_equal_for_both_forms() {
    let registry = cook_registry();
    for (name, scene) in fixtures() {
        let tmp = unique_temp_dir("level-dir-cook");
        let file = tmp.join(format!("{name}.level.ron"));
        std::fs::write(&file, scene.to_ron_string().expect("ron")).expect("write");
        let other = unique_temp_dir("level-dir-cook-d");
        let dir = other.join(format!("{name}.level.d"));
        write_level_dir(&dir, &scene).expect("write dir");
        let from_file = read_level_source(&file).expect("file form");
        let from_dir = read_level_source(&dir).expect("dir form");
        let (a, _) = CookedScene::cook(&from_file, &registry.read()).expect("cook file");
        let (b, _) = CookedScene::cook(&from_dir, &registry.read()).expect("cook dir");
        assert_eq!(a.to_bytes().expect("a"), b.to_bytes().expect("b"), "{name}");
    }
}

#[test]
fn saving_writes_only_what_changed() {
    let mut scene = generated(50);
    let tmp = unique_temp_dir("level-dir-dirty");
    let root = tmp.join("d.level.d");
    write_level_dir(&root, &scene).expect("first write");

    // Unchanged save touches nothing.
    let report = write_level_dir(&root, &scene).expect("noop");
    assert!(report.written.is_empty() && report.deleted.is_empty());
    assert_eq!(report.unchanged, 51);

    // One transform changed: exactly that file.
    let target = scene.entities[10].guid;
    scene.entities[10]
        .components
        .insert(TRANSFORM.into(), transform_at(123.0));
    let report = write_level_dir(&root, &scene).expect("one");
    assert_eq!(report.written, vec![root.join(entity_rel_path(target))]);

    // Dirty-set save with the same change only considers that guid.
    scene.entities[10]
        .components
        .insert(TRANSFORM.into(), transform_at(124.0));
    let dirty = LevelDirtySet {
        header: false,
        entities: [target].into(),
    };
    let report = write_level_dir_dirty(&root, &scene, &dirty).expect("dirty");
    assert_eq!(report.written, vec![root.join(entity_rel_path(target))]);
    assert_eq!(report.unchanged, 0);

    // A resource change: only level.ron.
    scene.resources.insert(
        regia_scene::level::LEVEL_SETTINGS_PATH.into(),
        raw("(gravity: (0.0, -1.0, 0.0))"),
    );
    let report = write_level_dir(&root, &scene).expect("header");
    assert_eq!(report.written, vec![root.join(HEADER_FILE)]);

    // Delete an entity alone in its shard: file and shard vanish.
    let lonely = Guid::from_u128(0xee00_0000_0000_0000_0000_0000_0000_0001);
    let mut extra = SceneEntity {
        guid: lonely,
        ..SceneEntity::default()
    };
    extra.components.insert(TRANSFORM.into(), transform_at(1.0));
    scene.entities.push(extra);
    write_level_dir(&root, &scene).expect("add");
    let lonely_path = root.join(entity_rel_path(lonely));
    assert!(lonely_path.is_file());
    scene.entities.retain(|e| e.guid != lonely);
    let report = write_level_dir(&root, &scene).expect("delete");
    assert_eq!(report.deleted, vec![lonely_path.clone()]);
    assert!(!lonely_path.exists());
    assert!(
        !root.join(ENTITIES_DIR).join("ee").exists(),
        "empty shard removed"
    );
}

#[cfg(unix)]
#[test]
fn a_failed_write_deletes_nothing_and_lists_what_was_written() {
    use std::os::unix::fs::PermissionsExt;
    let mk = |hi: u128| {
        let mut e = SceneEntity {
            guid: Guid::from_u128((hi << 120) | 1),
            ..SceneEntity::default()
        };
        e.components.insert(TRANSFORM.into(), transform_at(hi as f32));
        e
    };
    let tmp = unique_temp_dir("level-dir-fail");
    let root = tmp.join("f.level.d");
    let stale = mk(0xf0);
    let mut scene = SceneFile::new();
    scene.entities.push(stale.clone());
    write_level_dir(&root, &scene).expect("seed");
    let stale_path = root.join(entity_rel_path(stale.guid));

    scene.entities = vec![mk(0x01), mk(0x02), mk(0x03)];
    let blocked = root.join(ENTITIES_DIR).join("03");
    std::fs::create_dir_all(&blocked).expect("mkdir");
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o555)).expect("chmod");
    // Root ignores permissions; skip the assertion there rather than pass vacuously.
    if std::fs::write(blocked.join("probe"), b"x").is_ok() {
        eprintln!("running as a user that ignores directory permissions; test skipped");
        return;
    }
    let err = write_level_dir(&root, &scene).expect_err("third write fails");
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755)).expect("chmod back");
    assert_eq!(err.reason(), "level_io_error");
    // Named by path: the error type is used only in this unix-only test (no unused import elsewhere).
    let level_dir::LevelDirError::Io { written, .. } = err else {
        panic!("io error expected");
    };
    assert_eq!(
        written,
        vec![
            root.join(entity_rel_path(mk(0x01).guid)),
            root.join(entity_rel_path(mk(0x02).guid))
        ]
    );
    assert!(stale_path.is_file(), "nothing deleted");
}

/// Spec 37 § 3.4: "if any write fails nothing is deleted". The header is a write too: before the
/// fix the deletions ran ahead of the header, so a failed header write left the removed entity's
/// file already gone.
#[cfg(unix)]
#[test]
fn a_failed_header_write_deletes_nothing() {
    use std::os::unix::fs::PermissionsExt;
    let mk = |hi: u128| {
        let mut e = SceneEntity {
            guid: Guid::from_u128((hi << 120) | 1),
            ..SceneEntity::default()
        };
        e.components.insert(TRANSFORM.into(), transform_at(hi as f32));
        e
    };
    let tmp = unique_temp_dir("level-dir-header-fail");
    let root = tmp.join("h.level.d");
    let stale = mk(0xf0);
    let mut scene = SceneFile::new();
    scene.entities.push(stale.clone());
    write_level_dir(&root, &scene).expect("seed");
    let stale_path = root.join(entity_rel_path(stale.guid));
    let header_before = std::fs::read(root.join(HEADER_FILE)).expect("header");

    // Next save: the stale entity is gone, a new one arrives, and a resource changed (so the
    // header must be rewritten). The root is read-only, `entities/` is not: entity writes and
    // deletions could succeed, the header's temp file cannot be created.
    scene.entities = vec![mk(0x01)];
    scene.resources.insert(
        regia_scene::level::LEVEL_SETTINGS_PATH.into(),
        raw("(gravity: (0.0, -2.0, 0.0))"),
    );
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o555)).expect("chmod");
    if std::fs::write(root.join("probe"), b"x").is_ok() {
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).expect("chmod back");
        eprintln!("running as a user that ignores directory permissions; test skipped");
        return;
    }
    let result = write_level_dir(&root, &scene);
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).expect("chmod back");
    let err = result.expect_err("the header write fails");
    assert_eq!(err.reason(), "level_io_error");
    let level_dir::LevelDirError::Io { path, written, .. } = err else {
        panic!("io error expected");
    };
    assert_eq!(path, root.join(HEADER_FILE), "the header is what failed");
    assert_eq!(written, vec![root.join(entity_rel_path(mk(0x01).guid))]);
    assert!(
        stale_path.is_file(),
        "nothing deleted after a failed header write"
    );
    assert_eq!(
        std::fs::read(root.join(HEADER_FILE)).expect("header"),
        header_before,
        "the old header is intact"
    );
    // The retry, with the directory writable again, finishes the job.
    let report = write_level_dir(&root, &scene).expect("retry");
    assert_eq!(report.deleted, vec![stale_path.clone()]);
    assert!(!stale_path.exists());
}

fn one_entity_dir(tag: &str) -> (PathBuf, SceneFile) {
    let tmp = unique_temp_dir(tag);
    let root = tmp.join("v.level.d");
    let scene = generated(3);
    write_level_dir(&root, &scene).expect("write");
    (root, scene)
}

#[test]
fn read_time_reasons() {
    // entity_file_mismatch: a file renamed to another guid.
    let (root, scene) = one_entity_dir("level-dir-mismatch");
    let from = root.join(entity_rel_path(scene.entities[0].guid));
    let other = Guid::from_u128(scene.entities[0].guid.as_u128() ^ 0xff);
    let to = root.join(entity_rel_path(other));
    std::fs::rename(&from, &to).expect("rename");
    assert_eq!(
        read_level_dir(&root).expect_err("mismatch").reason(),
        "entity_file_mismatch"
    );

    // duplicate_entity: the same guid in its shard and a wrong one is impossible by name, so
    // copy into a wrong shard: the first is a warning (wrong shard), both present is a duplicate.
    let (root, scene) = one_entity_dir("level-dir-dup");
    let guid = scene.entities[1].guid;
    let right = root.join(entity_rel_path(guid));
    let hex = level_dir::guid_hex(guid);
    let wrong_shard = if hex.starts_with("00") { "01" } else { "00" };
    let wrong = root
        .join(ENTITIES_DIR)
        .join(wrong_shard)
        .join(format!("{hex}.entity.ron"));
    std::fs::create_dir_all(wrong.parent().expect("p")).expect("mkdir");
    std::fs::copy(&right, &wrong).expect("copy");
    let dup = read_level_dir(&root).expect_err("dup");
    assert_eq!(dup.reason(), "duplicate_entity");
    // Spec 37 § 3.3: both paths are listed.
    let message = dup.to_string();
    assert!(
        message.contains(&right.display().to_string()) && message.contains(&wrong.display().to_string()),
        "{message}"
    );
    // entity_wrong_shard: only the misplaced copy remains -> warning, and the next save fixes it.
    std::fs::remove_file(&right).expect("rm");
    let (read, report) = read_level_dir(&root).expect("wrong shard is a warning");
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.starts_with("entity_wrong_shard")),
        "{report:?}"
    );
    let saved = write_level_dir(&root, &read).expect("fix");
    assert!(saved.deleted.contains(&wrong) && right.is_file());

    // mixed_scene_versions.
    let (root, scene) = one_entity_dir("level-dir-mixed");
    let path = root.join(entity_rel_path(scene.entities[2].guid));
    let text = std::fs::read_to_string(&path).expect("read");
    let older = text.replace(
        &format!("scene_format_version: {}", SCENE_FORMAT_VERSION),
        &format!("scene_format_version: {}", SCENE_FORMAT_VERSION - 1),
    );
    assert_ne!(text, older);
    std::fs::write(&path, older).expect("write");
    assert_eq!(
        read_level_dir(&root).expect_err("mixed").reason(),
        "mixed_scene_versions"
    );
    // upgrade-project's remedy rewrites every file at the latest version.
    level_dir::upgrade_level_dir(&root).expect("upgrade");
    read_level_dir(&root).expect("readable after upgrade");

    // unsupported_version (a newer entity file whose body no longer matches).
    let (root, scene) = one_entity_dir("level-dir-newer");
    let path = root.join(entity_rel_path(scene.entities[0].guid));
    std::fs::write(
        &path,
        "EntityFile(format_version: 99, scene_format_version: 1, entity: \"?\")",
    )
    .expect("w");
    assert_eq!(
        read_level_dir(&root).expect_err("newer").reason(),
        "unsupported_version"
    );

    // unsupported_version: an entity file at a newer scene format than this build (the
    // directory wrapper version is current).
    let (root, scene) = one_entity_dir("level-dir-newer-scene");
    let path = root.join(entity_rel_path(scene.entities[0].guid));
    let text = std::fs::read_to_string(&path).expect("read");
    let newer = text.replace(
        &format!("scene_format_version: {}", SCENE_FORMAT_VERSION),
        &format!("scene_format_version: {}", SCENE_FORMAT_VERSION + 1),
    );
    assert_ne!(text, newer);
    std::fs::write(&path, newer).expect("write");
    assert_eq!(
        read_level_dir(&root).expect_err("newer scene").reason(),
        "unsupported_version"
    );

    // Junk under entities/ is a warning, never a failure.
    let (root, _) = one_entity_dir("level-dir-junk");
    std::fs::write(root.join(ENTITIES_DIR).join("notes.txt"), "x").expect("junk");
    let shard = walk(&root.join(ENTITIES_DIR))
        .into_iter()
        .find_map(|p| p.parent().map(Path::to_path_buf))
        .expect("a shard");
    std::fs::write(shard.join("x.entity.ron~"), "garbage").expect("junk2");
    let (_, report) = read_level_dir(&root).expect("junk ignored");
    assert_eq!(report.warnings.len(), 2, "{:?}", report.warnings);

    // ambiguous_level_form.
    let tmp = unique_temp_dir("level-dir-ambiguous");
    write_level_dir(&tmp.join("a.level.d"), &generated(1)).expect("dir");
    std::fs::write(
        tmp.join("a.level.ron"),
        generated(1).to_ron_string().expect("ron"),
    )
    .expect("file");
    let err = read_level_source(&tmp.join("a.level.ron")).expect_err("ambiguous");
    assert_eq!(err.reason(), "ambiguous_level_form");
    assert_eq!(
        level_dir::level_form(&tmp.join("a"))
            .expect_err("ambiguous")
            .reason(),
        "ambiguous_level_form"
    );
}

#[test]
fn garbage_entity_files_are_errors_not_panics() {
    let (root, scene) = one_entity_dir("level-dir-garbage");
    let path = root.join(entity_rel_path(scene.entities[0].guid));
    for junk in [
        "",
        "(",
        "EntityFile(",
        "\u{0}\u{1}",
        "EntityFile(format_version: 1)",
        "[1,2,3]",
    ] {
        std::fs::write(&path, junk).expect("write");
        let err = read_level_dir(&root).expect_err("garbage");
        assert_eq!(err.reason(), "level_parse_error", "{junk:?}");
    }
}
