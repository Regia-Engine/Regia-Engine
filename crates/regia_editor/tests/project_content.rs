// SPDX-License-Identifier: MPL-2.0
//! Public-surface tests for the editor's project, content, drag-and-drop, selection and autosave
//! helpers: the error paths and edge cases the in-module tests do not reach. No GPU, no clocks;
//! every test works in its own unique temp directory.

use std::path::{Path, PathBuf};
use std::time::Duration;

use regia_app::prelude::App;
use regia_ecs::prelude::*;
use regia_editor::autosave::{self, RecoveryCandidate};
use regia_editor::command::UndoStack;
use regia_editor::content::{
    self, ImportReport, NewAssetKind, create_asset, delete_asset, delete_asset_force, find_referencers,
};
use regia_editor::dnd::{ContentKind, EntityDrag, kind_name, kind_of_path};
use regia_editor::level_io::OpenLevel;
use regia_editor::project::{self, BrowserAction};
use regia_editor::selection::{SelectMode, Selection};

/// A fresh directory unique to this process and call.
fn temp_dir(label: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("regia-editor-pc-{label}-{n}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// A directory that looks like a project: `Regia.toml` plus `assets/`.
fn fake_project(label: &str, toml: &str) -> PathBuf {
    let root = temp_dir(label);
    std::fs::create_dir_all(root.join("assets")).expect("assets");
    std::fs::write(root.join("Regia.toml"), toml).expect("toml");
    root
}

/// Redirects the recent-projects list into `dir` for the current test thread.
fn isolate_recents(dir: &Path) -> PathBuf {
    let file = dir.join("recents.txt");
    project::set_recent_file_for_tests(file.clone());
    file
}

fn scene_app() -> App {
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app
}

// ---- dnd ----

#[test]
fn drag_kinds_follow_the_file_extension() {
    assert_eq!(kind_of_path("assets/maps/a.level.ron"), ContentKind::Level);
    assert_eq!(kind_of_path("assets/props/b.prefab.ron"), ContentKind::Prefab);
    assert_eq!(kind_of_path("assets/models/c.gltf"), ContentKind::Mesh);
    assert_eq!(kind_of_path("assets/models/d.glb"), ContentKind::Mesh);
    // A plain `.ron` file, a texture, a folder and the empty string are all "other".
    assert_eq!(kind_of_path("assets/x.ron"), ContentKind::Other);
    assert_eq!(kind_of_path("assets/t.png"), ContentKind::Other);
    assert_eq!(kind_of_path("assets/maps"), ContentKind::Other);
    assert_eq!(kind_of_path(""), ContentKind::Other);
    // Matching is on the suffix, so the double extension must be complete.
    assert_eq!(kind_of_path("assets/a.level.ron.bak"), ContentKind::Other);
    assert_eq!(kind_of_path("assets/levelron"), ContentKind::Other);
}

#[test]
fn drag_kind_names_are_stable_and_distinct() {
    let names = [
        kind_name(ContentKind::Level),
        kind_name(ContentKind::Prefab),
        kind_name(ContentKind::Mesh),
        kind_name(ContentKind::Other),
    ];
    assert_eq!(names, ["level", "prefab", "mesh", "other"]);
}

#[test]
fn entity_drag_round_trips_a_live_entity() {
    let mut world = World::new();
    let entity = world.spawn_empty().id();
    let drag = EntityDrag {
        entity: entity.to_bits(),
    };
    assert_eq!(drag.entity(), Some(entity));
}

// ---- selection ----

#[test]
fn add_of_a_selected_entity_keeps_one_copy_and_moves_the_primary() {
    let mut world = World::new();
    let a = world.spawn_empty().id();
    let b = world.spawn_empty().id();
    let mut selection = Selection::default();
    selection.select(a, SelectMode::Replace);
    selection.select(b, SelectMode::Add);
    selection.select(a, SelectMode::Add);
    assert_eq!(selection.entities(), &[a, b], "no duplicate entry");
    assert_eq!(selection.primary(), Some(a), "the last click is primary");
    assert!(selection.contains(a) && selection.contains(b));
    assert_eq!(selection.len(), 2);
}

#[test]
fn toggling_a_non_primary_entity_keeps_the_primary() {
    let mut world = World::new();
    let a = world.spawn_empty().id();
    let b = world.spawn_empty().id();
    let c = world.spawn_empty().id();
    let mut selection = Selection::default();
    selection.select(a, SelectMode::Replace);
    selection.select(b, SelectMode::Add);
    selection.select(c, SelectMode::Add);
    selection.select(a, SelectMode::Toggle);
    assert_eq!(selection.entities(), &[b, c]);
    assert_eq!(selection.primary(), Some(c));
    // Toggling the primary falls back to the newest remaining entity.
    selection.select(c, SelectMode::Toggle);
    assert_eq!(selection.primary(), Some(b));
    // Toggling the last one empties the selection and leaves no primary.
    selection.select(b, SelectMode::Toggle);
    assert!(selection.is_empty());
    assert_eq!(selection.primary(), None);
}

#[test]
fn replace_with_the_same_entity_is_idempotent() {
    let mut world = World::new();
    let a = world.spawn_empty().id();
    let mut selection = Selection::default();
    selection.select(a, SelectMode::Replace);
    selection.select(a, SelectMode::Replace);
    assert_eq!(selection.entities(), &[a]);
    assert_eq!(selection.primary(), Some(a));
}

#[test]
fn retain_alive_moves_a_dead_primary_to_the_newest_survivor() {
    let mut world = World::new();
    let a = world.spawn_empty().id();
    let b = world.spawn_empty().id();
    let c = world.spawn_empty().id();
    let mut selection = Selection::default();
    selection.select(a, SelectMode::Replace);
    selection.select(b, SelectMode::Add);
    selection.select(c, SelectMode::Add);
    world.get_entity_mut(c).expect("alive").despawn();
    selection.retain_alive(&world);
    assert_eq!(selection.entities(), &[a, b]);
    assert_eq!(selection.primary(), Some(b));
    // Everything dead: nothing selected, no primary.
    world.get_entity_mut(a).expect("alive").despawn();
    world.get_entity_mut(b).expect("alive").despawn();
    selection.retain_alive(&world);
    assert!(selection.is_empty());
    assert_eq!(selection.primary(), None);
}

#[test]
fn select_all_skips_editor_hidden_entities() {
    let mut world = World::new();
    let shown = world.spawn_empty().id();
    let hidden = world.spawn(regia_editor::visibility::EditorHidden).id();
    let mut selection = Selection::default();
    selection.select_all(&world);
    assert!(selection.contains(shown));
    assert!(
        !selection.contains(hidden),
        "internal entities are not selectable"
    );
}

// ---- content: new assets ----

#[test]
fn asset_kind_names_round_trip_and_unknown_names_are_rejected() {
    for kind in [
        NewAssetKind::Material,
        NewAssetKind::Prefab,
        NewAssetKind::Level,
        NewAssetKind::Folder,
    ] {
        assert_eq!(NewAssetKind::parse(kind.as_str()), Ok(kind));
    }
    let err = NewAssetKind::parse("texture").expect_err("unknown kind");
    assert!(err.contains("unknown asset kind"), "{err}");
    assert!(NewAssetKind::parse("").is_err());
    assert!(
        NewAssetKind::parse("Material").is_err(),
        "names are case sensitive"
    );
    assert_eq!(NewAssetKind::Folder.extension(), None);
    assert_eq!(NewAssetKind::Material.extension(), Some("material.ron"));
    assert_eq!(NewAssetKind::Prefab.extension(), Some("prefab.ron"));
    assert_eq!(NewAssetKind::Level.extension(), Some("level.ron"));
}

#[test]
fn create_asset_writes_each_kind_and_returns_the_project_relative_path() {
    let root = temp_dir("create-kinds");
    let material = create_asset(&root, NewAssetKind::Material, "mats/rock.material.ron").expect("material");
    assert_eq!(material, "assets/mats/rock.material.ron");
    assert!(root.join(&material).is_file());

    let prefab = create_asset(&root, NewAssetKind::Prefab, "props/crate.prefab.ron").expect("prefab");
    let text = std::fs::read_to_string(root.join(&prefab)).expect("read prefab");
    assert!(
        text.contains("crate"),
        "the root entity is named after the file: {text}"
    );

    let level = create_asset(&root, NewAssetKind::Level, "maps/arena.level.ron").expect("level");
    assert!(root.join(&level).is_file());

    let folder = create_asset(&root, NewAssetKind::Folder, "props/rocks").expect("folder");
    assert_eq!(folder, "assets/props/rocks");
    assert!(root.join(&folder).is_dir());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn create_asset_accepts_an_assets_prefix_and_backslashes() {
    let root = temp_dir("create-prefix");
    let made = create_asset(&root, NewAssetKind::Level, "assets\\maps\\b.level.ron").expect("level");
    assert_eq!(made, "assets/maps/b.level.ron");
    assert!(root.join("assets/maps/b.level.ron").is_file());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn create_asset_refuses_bad_paths_and_leaves_no_files() {
    let root = temp_dir("create-bad");
    for path in [
        "",
        "/",
        "assets/",
        "a/../b.level.ron",
        "../x.level.ron",
        ".hidden/x.level.ron",
    ] {
        let err = create_asset(&root, NewAssetKind::Level, path).expect_err(path);
        assert!(err.contains("invalid asset path"), "{path:?}: {err}");
    }
    // Empty segments (double slash) are refused too.
    assert!(create_asset(&root, NewAssetKind::Level, "a//b.level.ron").is_err());
    // The extension must match the kind.
    let err = create_asset(&root, NewAssetKind::Level, "maps/a.prefab.ron").expect_err("wrong extension");
    assert!(err.contains(".level.ron"), "{err}");
    assert!(create_asset(&root, NewAssetKind::Material, "m.ron").is_err());
    // Nothing was created: not even the parent folder for the refused path.
    assert!(!root.join("assets/maps").exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn create_asset_never_overwrites() {
    let root = temp_dir("create-dup");
    let first = create_asset(&root, NewAssetKind::Material, "m.material.ron").expect("first");
    let before = std::fs::read(root.join(&first)).expect("read");
    let err = create_asset(&root, NewAssetKind::Material, "m.material.ron").expect_err("duplicate");
    assert!(err.contains("already exists"), "{err}");
    assert_eq!(std::fs::read(root.join(&first)).expect("read"), before);
    // A folder cannot be created over an existing file either.
    assert!(create_asset(&root, NewAssetKind::Folder, "m.material.ron").is_err());
    let _ = std::fs::remove_dir_all(&root);
}

// ---- content: referencers and deletion ----

#[test]
fn find_referencers_lists_text_assets_that_mention_the_file() {
    let root = temp_dir("refs");
    create_asset(&root, NewAssetKind::Material, "mats/rock.material.ron").expect("material");
    std::fs::write(
        root.join("assets/uses.prefab.ron"),
        "(material: \"mats/rock.material.ron\")",
    )
    .expect("prefab");
    std::fs::create_dir_all(root.join("assets/sub")).expect("sub");
    std::fs::write(
        root.join("assets/sub/also.level.ron"),
        "// mats/rock.material.ron",
    )
    .expect("level");
    // Non-ron files, hidden files and `.meta` sidecars are not scanned.
    std::fs::write(root.join("assets/notes.txt"), "mats/rock.material.ron").expect("txt");
    std::fs::write(root.join("assets/.hidden.ron"), "mats/rock.material.ron").expect("hidden");

    let found = find_referencers(&root, "assets/mats/rock.material.ron");
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(found.iter().any(|p| p.ends_with("uses.prefab.ron")), "{found:?}");
    assert!(found.iter().any(|p| p.ends_with("also.level.ron")), "{found:?}");
    let mut sorted = found.clone();
    sorted.sort();
    assert_eq!(found, sorted, "the result is sorted");
    assert!(find_referencers(&root, "assets/nothing.png").is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn delete_asset_refuses_referenced_files_and_reports_who_refers() {
    let root = temp_dir("delete-ref");
    let made = create_asset(&root, NewAssetKind::Material, "rock.material.ron").expect("material");
    std::fs::write(
        root.join("assets/user.prefab.ron"),
        "(material: \"rock.material.ron\")",
    )
    .expect("prefab");

    let referencers = delete_asset(&root, &made).expect_err("referenced");
    assert_eq!(referencers.len(), 1, "{referencers:?}");
    assert!(referencers[0].ends_with("user.prefab.ron"));
    assert!(root.join(&made).is_file(), "a refused delete keeps the file");

    // The forced path deletes it anyway.
    delete_asset_force(&root, &made).expect("force");
    assert!(!root.join(&made).exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn delete_asset_removes_an_unreferenced_file_with_its_meta_sidecar() {
    let root = temp_dir("delete-ok");
    let made = create_asset(&root, NewAssetKind::Material, "lonely.material.ron").expect("material");
    let full = root.join(&made);
    let meta = regia_asset::meta_path_for(&full);
    std::fs::write(&meta, "meta").expect("meta");

    delete_asset(&root, &made).expect("delete");
    assert!(!full.exists());
    assert!(!meta.exists(), "the sidecar goes with the asset");
    let _ = std::fs::remove_dir_all(&root);
}

// The referencer list holds `Path::display` strings, which use backslashes on Windows, so the
// self-reference filter (a `/` comparison) is only exact on Unix paths.
#[cfg(not(windows))]
#[test]
fn delete_asset_ignores_a_self_reference() {
    let root = temp_dir("delete-self");
    std::fs::create_dir_all(root.join("assets")).expect("assets");
    // A file whose own text mentions its path is not a referencer of itself.
    std::fs::write(root.join("assets/self.level.ron"), "// self.level.ron").expect("self");
    delete_asset(&root, "assets/self.level.ron").expect("self reference does not block");
    assert!(!root.join("assets/self.level.ron").exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn delete_asset_removes_folders_with_their_contents() {
    let root = temp_dir("delete-folder");
    let folder = create_asset(&root, NewAssetKind::Folder, "props/rocks").expect("folder");
    std::fs::write(root.join("assets/props/rocks/a.txt"), "x").expect("file");
    delete_asset(&root, &folder).expect("folder delete");
    assert!(!root.join(&folder).exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn deleting_something_that_does_not_exist_is_an_empty_refusal() {
    let root = temp_dir("delete-missing");
    let err = delete_asset(&root, "assets/ghost.png").expect_err("missing");
    assert!(err.is_empty(), "no referencers and no message: {err:?}");
    assert!(delete_asset_force(&root, "assets/ghost.png").is_err());
    let _ = std::fs::remove_dir_all(&root);
}

// ---- content: import and timestamps ----

#[test]
fn import_with_no_sources_is_an_error() {
    let root = temp_dir("import-empty");
    let err = content::import_files(&root, "models", &[], None).expect_err("nothing to import");
    assert!(err.contains("nothing to import"), "{err}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn import_reports_unreadable_and_unsupported_sources_without_cooking() {
    let root = temp_dir("import-bad");
    let source_dir = temp_dir("import-bad-src");
    let notes = source_dir.join("notes.txt");
    std::fs::write(&notes, "hello").expect("notes");
    let missing = source_dir.join("missing.png");

    let report = content::import_files(&root, "assets/models/", &[notes, missing], None).expect("report");
    assert!(report.imported.is_empty());
    assert_eq!(report.cooked, 0);
    assert_eq!(report.errors.len(), 2, "{:?}", report.errors);
    assert!(
        report
            .errors
            .iter()
            .any(|(_, m)| m.contains("unsupported extension")),
        "{:?}",
        report.errors
    );
    assert!(
        report
            .errors
            .iter()
            .any(|(_, m)| m.contains("not a readable file")),
        "{:?}",
        report.errors
    );
    assert!(
        !root.join("assets/models/notes.txt").exists(),
        "rejected files are not copied"
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&source_dir);
}

#[test]
fn import_refuses_to_overwrite_an_existing_asset() {
    let root = temp_dir("import-dup");
    let source_dir = temp_dir("import-dup-src");
    std::fs::create_dir_all(root.join("assets/tex")).expect("dir");
    std::fs::write(root.join("assets/tex/a.png"), "original").expect("existing");
    let source = source_dir.join("a.png");
    std::fs::write(&source, "replacement").expect("source");

    let report = content::import_files(&root, "tex", &[source], None).expect("report");
    assert!(report.imported.is_empty());
    assert_eq!(report.errors.len(), 1);
    assert!(
        report.errors[0].1.contains("already exists"),
        "{:?}",
        report.errors
    );
    assert_eq!(
        std::fs::read_to_string(root.join("assets/tex/a.png")).expect("read"),
        "original"
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&source_dir);
}

#[test]
fn importable_extensions_are_the_pipeline_sources() {
    let extensions = content::importable_extensions();
    for ext in [
        "glb", "gltf", "png", "ron", "fbx", "obj", "ply", "stl", "psd", "dds", "tga", "bmp", "wav", "ogg",
        "flac", "mp3", "usd", "usda", "usdc", "usdz",
    ] {
        assert!(extensions.contains(&ext), "{ext}");
    }
    assert!(!extensions.contains(&"exe"), "only pipeline sources");
}

#[test]
fn import_report_summary_counts_and_flags_failures() {
    let mut report = ImportReport::default();
    assert_eq!(report.summary(), "imported 0 (cooked 0, up-to-date 0)");
    report.imported.push("assets/a.png".into());
    report.cooked = 1;
    report.skipped = 3;
    assert_eq!(report.summary(), "imported 1 (cooked 1, up-to-date 3)");
    report.errors.push(("b.png".into(), "bad".into()));
    assert!(report.summary().contains("1 failed"), "{}", report.summary());
}

#[test]
fn rfc3339_formats_known_instants_including_leap_days_and_the_past() {
    assert_eq!(content::rfc3339_from_unix(0), "1970-01-01T00:00:00Z");
    assert_eq!(content::rfc3339_from_unix(-1), "1969-12-31T23:59:59Z");
    assert_eq!(content::rfc3339_from_unix(951_782_400), "2000-02-29T00:00:00Z");
    assert_eq!(content::rfc3339_from_unix(1_000_000_000), "2001-09-09T01:46:40Z");
    assert_eq!(content::rfc3339_from_unix(1_709_164_800), "2024-02-29T00:00:00Z");
    assert_eq!(content::rfc3339_from_unix(1_709_251_199), "2024-02-29T23:59:59Z");
    assert_eq!(content::rfc3339_from_unix(1_709_251_200), "2024-03-01T00:00:00Z");
    // The "now" form has the same shape.
    let now = content::rfc3339_utc_now();
    assert_eq!(now.len(), 20, "{now}");
    assert!(now.ends_with('Z') && now.as_bytes()[10] == b'T', "{now}");
}

// ---- project ----

#[test]
fn startup_level_needs_a_manifest_key_and_an_existing_file() {
    let none = temp_dir("startup-none");
    assert_eq!(project::startup_level(&none), None, "no Regia.toml");

    let no_key = fake_project("startup-nokey", "[project]\nname = \"x\"\n");
    assert_eq!(project::startup_level(&no_key), None);

    let missing = fake_project(
        "startup-missing",
        "[project]\nstartup_level = \"maps/a.level.ron\"\n",
    );
    assert_eq!(project::startup_level(&missing), None, "the file does not exist");

    let broken = fake_project("startup-broken", "this is = = not toml");
    assert_eq!(project::startup_level(&broken), None);

    let wrong_type = fake_project("startup-type", "[project]\nstartup_level = 3\n");
    assert_eq!(project::startup_level(&wrong_type), None);

    let good = fake_project(
        "startup-good",
        "[project]\nstartup_level = \"maps/a.level.ron\"\n",
    );
    create_asset(&good, NewAssetKind::Level, "maps/a.level.ron").expect("level");
    assert_eq!(
        project::startup_level(&good),
        Some(good.join("assets").join("maps/a.level.ron"))
    );
    for dir in [none, no_key, missing, broken, wrong_type, good] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn open_project_rejects_non_projects_without_touching_the_world() {
    let scratch = temp_dir("open-bad");
    isolate_recents(&scratch);
    let mut app = scene_app();

    let err = project::open_project(app.world_mut(), &scratch.join("nope")).expect_err("not a dir");
    assert!(err.contains("is not a directory"), "{err}");

    let err = project::open_project(app.world_mut(), &scratch).expect_err("no manifest");
    assert!(err.contains("no Regia.toml"), "{err}");

    assert!(
        app.world()
            .get_resource::<regia_editor::agent_ext::EditorProject>()
            .is_none(),
        "a failed open does not register a project"
    );
    assert!(!project::recent_file().exists(), "and does not remember it");
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn open_project_opens_the_startup_level_and_marks_the_history_saved() {
    let scratch = temp_dir("open-ok");
    isolate_recents(&scratch);
    let root = fake_project(
        "open-ok-proj",
        "[project]\nstartup_level = \"maps/main.level.ron\"\n",
    );
    create_asset(&root, NewAssetKind::Level, "maps/main.level.ron").expect("level");
    let mut app = scene_app();

    let opened = project::open_project(app.world_mut(), &root).expect("open");
    assert_eq!(opened.warning, None);
    let canonical = root.canonicalize().expect("canonical");
    assert_eq!(opened.root, canonical);

    let editor_project = app.world().resource::<regia_editor::agent_ext::EditorProject>();
    assert_eq!(editor_project.root.as_deref(), Some(canonical.as_path()));
    let open = app.world().resource::<OpenLevel>();
    assert_eq!(
        open.path.as_deref(),
        Some(canonical.join("assets/maps/main.level.ron").as_path())
    );
    assert!(
        !app.world().resource::<UndoStack>().is_dirty(),
        "a fresh project is clean"
    );
    assert_eq!(project::load_recents(), vec![canonical]);
    let _ = std::fs::remove_dir_all(&scratch);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn open_project_with_a_broken_startup_level_is_a_warning_not_an_error() {
    let scratch = temp_dir("open-warn");
    isolate_recents(&scratch);
    let root = fake_project("open-warn-proj", "[project]\nstartup_level = \"bad.level.ron\"\n");
    std::fs::write(root.join("assets/bad.level.ron"), "this is not a level").expect("bad level");
    let mut app = scene_app();

    let opened = project::open_project(app.world_mut(), &root).expect("the project itself opens");
    assert!(opened.warning.is_some(), "the level failure is reported");
    // Opening a project starts from a fresh untitled level; the broken startup level does not
    // replace it.
    let open = app
        .world()
        .get_resource::<OpenLevel>()
        .expect("the fresh untitled level");
    assert!(
        open.path.is_none(),
        "the broken level is not open: {:?}",
        open.path
    );
    assert!(
        app.world()
            .get_resource::<regia_editor::agent_ext::EditorProject>()
            .is_some(),
        "the project is open"
    );
    let _ = std::fs::remove_dir_all(&scratch);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn recent_projects_dedupe_to_the_front_cap_at_twelve_and_drop_dead_entries() {
    let scratch = temp_dir("recents");
    let file = isolate_recents(&scratch);
    assert!(project::load_recents().is_empty(), "no file yet");
    assert_eq!(project::recent_file(), file);

    let projects: Vec<PathBuf> = (0..13)
        .map(|i| fake_project(&format!("recent-{i}"), "[project]\n"))
        .collect();
    for root in &projects {
        project::remember_recent(root);
    }
    let recents = project::load_recents();
    assert_eq!(recents.len(), 12, "capped");
    assert_eq!(recents[0], projects[12], "newest first");
    assert!(!recents.contains(&projects[0]), "the oldest fell off");

    // Remembering an existing entry moves it to the front without duplicating it.
    project::remember_recent(&projects[5]);
    let recents = project::load_recents();
    assert_eq!(recents.len(), 12);
    assert_eq!(recents[0], projects[5]);
    assert_eq!(recents.iter().filter(|p| **p == projects[5]).count(), 1);

    // A project whose manifest disappeared is dropped on the next read.
    std::fs::remove_file(projects[12].join("Regia.toml")).expect("remove manifest");
    assert!(!project::load_recents().contains(&projects[12]));

    // Blank lines and whitespace in the file are tolerated.
    std::fs::write(&file, format!("\n  {}  \n\n", projects[3].display())).expect("rewrite");
    assert_eq!(project::load_recents(), vec![projects[3].clone()]);

    for dir in projects {
        let _ = std::fs::remove_dir_all(dir);
    }
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn create_and_open_rejects_bad_names_and_opens_a_new_blank_project() {
    let scratch = temp_dir("create-open");
    isolate_recents(&scratch);
    let mut app = scene_app();

    for bad in ["", "9lives", "fn", "has\"quote"] {
        let result = project::create_and_open(app.world_mut(), bad, &scratch, "blank");
        assert!(result.is_err(), "name {bad:?} must be refused");
    }
    assert!(
        app.world()
            .get_resource::<regia_editor::agent_ext::EditorProject>()
            .is_none(),
        "no project opened for a refused name"
    );

    let opened = project::create_and_open(app.world_mut(), "Space Rocks", &scratch, "blank").expect("create");
    assert!(opened.root.join("Regia.toml").is_file());
    assert!(
        opened
            .root
            .starts_with(scratch.canonicalize().expect("canonical"))
    );
    assert!(
        app.world()
            .get_resource::<regia_editor::agent_ext::EditorProject>()
            .is_some(),
        "the new project is open"
    );
    // Creating over the same name must not clobber the project.
    let before = std::fs::read(opened.root.join("Regia.toml")).expect("manifest");
    assert!(project::create_and_open(app.world_mut(), "Space Rocks", &scratch, "blank").is_err());
    assert_eq!(
        std::fs::read(opened.root.join("Regia.toml")).expect("manifest"),
        before
    );
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn browser_actions_compare_by_value() {
    assert_eq!(BrowserAction::Create, BrowserAction::Create);
    assert_eq!(
        BrowserAction::Open(PathBuf::from("a")),
        BrowserAction::Open(PathBuf::from("a"))
    );
    assert_ne!(BrowserAction::Open(PathBuf::from("a")), BrowserAction::Create);
}

// ---- autosave ----

fn open_at(path: Option<PathBuf>) -> OpenLevel {
    OpenLevel {
        id: regia_scene::prelude::LevelId(regia_core::id::Guid::new_random()),
        path,
    }
}

#[test]
fn autosave_paths_follow_the_level_stem() {
    let root = temp_dir("autosave-paths");
    let untitled = autosave::autosave_path(&root, &open_at(None));
    assert_eq!(untitled, autosave::autosave_dir(&root).join("untitled.level.ron"));
    let named = autosave::autosave_path(&root, &open_at(Some(root.join("assets/maps/arena.level.ron"))));
    assert_eq!(named, autosave::autosave_dir(&root).join("arena.level.ron"));
    assert!(autosave::autosave_dir(&root).starts_with(&root));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn recovery_candidates_ignore_missing_dirs_and_non_ron_files() {
    let root = temp_dir("recovery-scan");
    assert!(autosave::recovery_candidates(&root).is_empty(), "no autosave dir");
    assert!(project::recovery_for(&root).is_empty());

    let dir = autosave::autosave_dir(&root);
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(dir.join("stray.txt"), "x").expect("txt");
    std::fs::write(dir.join("untitled.level.ron"), "()").expect("ron");
    let found = autosave::recovery_candidates(&root);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].autosave.ends_with("untitled.level.ron"));
    assert_eq!(found[0].original, None, "an untitled autosave has no original");
    assert_eq!(project::recovery_for(&root).len(), 1, "and is always offered");

    autosave::clear_all_autosaves(&root);
    assert!(!dir.exists());
    assert!(autosave::recovery_candidates(&root).is_empty());
    // Clearing twice is harmless.
    autosave::clear_all_autosaves(&root);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn find_level_by_stem_searches_nested_folders_and_misses_cleanly() {
    let root = temp_dir("stem");
    let made = create_asset(&root, NewAssetKind::Level, "deep/er/castle.level.ron").expect("level");
    assert_eq!(
        autosave::find_level_by_stem(&root, "castle"),
        Some(root.join(&made))
    );
    assert_eq!(autosave::find_level_by_stem(&root, "nowhere"), None);
    // The stem is matched whole, not as a prefix.
    assert_eq!(autosave::find_level_by_stem(&root, "cast"), None);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn recovery_is_offered_only_when_the_autosave_is_newer_or_the_original_is_gone() {
    let root = temp_dir("offer");
    let original = root.join("original.level.ron");
    std::fs::write(&original, "()").expect("original");
    let original_time = std::fs::metadata(&original)
        .and_then(|m| m.modified())
        .expect("mtime");
    let candidate = |original: Option<PathBuf>, modified| RecoveryCandidate {
        autosave: root.join("autosave.level.ron"),
        original,
        modified,
    };
    let step = Duration::from_secs(10);

    assert!(autosave::should_offer_recovery(&candidate(
        Some(original.clone()),
        original_time + step
    )));
    assert!(
        !autosave::should_offer_recovery(&candidate(Some(original.clone()), original_time)),
        "an equal timestamp means the save already caught up"
    );
    assert!(!autosave::should_offer_recovery(&candidate(
        Some(original.clone()),
        original_time - step
    )));
    assert!(
        autosave::should_offer_recovery(&candidate(
            Some(root.join("gone.level.ron")),
            original_time - step
        )),
        "the original vanished: the autosave is the only copy"
    );
    assert!(autosave::should_offer_recovery(&candidate(
        None,
        original_time - step
    )));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn clear_autosave_after_save_removes_the_old_and_new_names() {
    let root = temp_dir("clear-after-save");
    let before = open_at(None);
    let saved_path = root.join("assets/maps/new.level.ron");
    let dir = autosave::autosave_dir(&root);
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(dir.join("untitled.level.ron"), "()").expect("untitled");
    std::fs::write(dir.join("new.level.ron"), "()").expect("new");
    std::fs::write(dir.join("other.level.ron"), "()").expect("other");

    autosave::clear_autosave_after_save(&root, &before, &saved_path);
    assert!(!dir.join("untitled.level.ron").exists());
    assert!(!dir.join("new.level.ron").exists());
    assert!(
        dir.join("other.level.ron").exists(),
        "other levels' autosaves stay"
    );

    // Clearing an autosave that does not exist is harmless.
    autosave::clear_autosave(&root, &before);
    let _ = std::fs::remove_dir_all(&root);
}

// ---- scripts ----

#[test]
fn create_script_reports_missing_pieces_without_writing() {
    let root = temp_dir("script-bad");
    let err = regia_editor::scripts::create_script(&root, "Door").expect_err("no game crate");
    assert!(err.contains("cannot read"), "{err}");

    std::fs::create_dir_all(root.join("game/src")).expect("dir");
    std::fs::write(root.join("game/src/lib.rs"), "//! no marker here\n").expect("lib");
    let err = regia_editor::scripts::create_script(&root, "Door").expect_err("no marker");
    assert!(err.contains("REGIA:SCRIPTS"), "{err}");
    assert!(
        !root.join("game/src/scripts").exists(),
        "nothing written on a refused create"
    );

    std::fs::write(root.join("game/src/lib.rs"), "// REGIA:SCRIPTS\n").expect("lib");
    for bad in ["", "9door", "fn", "bad\"name"] {
        assert!(
            regia_editor::scripts::create_script(&root, bad).is_err(),
            "{bad:?} is not a usable script name"
        );
    }
    let lib = std::fs::read_to_string(root.join("game/src/lib.rs")).expect("lib");
    assert_eq!(lib, "// REGIA:SCRIPTS\n", "refused names leave lib.rs untouched");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn create_script_normalizes_the_name_and_registers_above_the_marker() {
    let root = temp_dir("script-ok");
    std::fs::create_dir_all(root.join("game/src")).expect("dir");
    std::fs::write(root.join("game/src/lib.rs"), "//! game\n// REGIA:SCRIPTS\n").expect("lib");

    let path = regia_editor::scripts::create_script(&root, "Boss Fight").expect("script");
    assert!(
        path.ends_with("game/src/scripts/boss_fight.rs"),
        "{}",
        path.display()
    );
    assert!(path.is_file());
    let lib = std::fs::read_to_string(root.join("game/src/lib.rs")).expect("lib");
    let module = lib.find("mod boss_fight;").expect("module registered");
    let marker = lib.find("// REGIA:SCRIPTS").expect("marker kept");
    assert!(
        module < marker,
        "registered above the marker so the next script lands after it: {lib}"
    );

    // A second script goes in too, and the first stays registered exactly once.
    regia_editor::scripts::create_script(&root, "Door").expect("second");
    let lib = std::fs::read_to_string(root.join("game/src/lib.rs")).expect("lib");
    assert_eq!(lib.matches("mod boss_fight;").count(), 1);
    assert_eq!(lib.matches("mod door;").count(), 1);
    assert_eq!(lib.matches("// REGIA:SCRIPTS").count(), 1);
    let _ = std::fs::remove_dir_all(&root);
}
