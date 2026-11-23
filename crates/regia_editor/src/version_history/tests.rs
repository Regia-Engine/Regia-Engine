// SPDX-License-Identifier: MPL-2.0
//! Tests of the version history (ADR-7928): the store, pruning, the autosave and save hooks, the
//! read-only preview, the undoable restore and the window's keyboard path.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use regia_app::prelude::App;

use super::*;
use crate::doc_autosave::{self, DocAutosaveRegistry, DocProvider, DocSnapshot};

const FLUSH: Duration = Duration::from_secs(60);

fn temp_project(label: &str) -> PathBuf {
    let dir = regia_test_utils::unique_temp_dir(&format!("version-history-{label}"));
    std::fs::create_dir_all(dir.join("assets/maps")).expect("dir");
    std::fs::write(dir.join("Regia.toml"), "[project]\nname = \"history\"\n").expect("manifest");
    dir
}

fn level_target(root: &Path) -> VersionTarget {
    VersionTarget::level(root, Some(&root.join("assets/maps/main.level.ron")))
}

fn autosave(target: &VersionTarget, text: &str) -> VersionWrite {
    VersionWrite {
        target: target.clone(),
        source: VersionSource::Autosave,
        bytes: VersionBytes::Bytes(text.as_bytes().to_vec()),
    }
}

fn save(target: &VersionTarget, text: &str) -> VersionWrite {
    VersionWrite {
        source: VersionSource::Save,
        ..autosave(target, text)
    }
}

fn texts(root: &Path, target: &VersionTarget) -> Vec<(VersionSource, String)> {
    versions_of(root, target)
        .into_iter()
        .map(|v| (v.source, std::fs::read_to_string(&v.file).expect("read")))
        .collect()
}

/// P-325: the last N autosaves and the last N saves stay per target, newest first; the oldest go.
#[test]
fn the_last_n_autosaves_and_saves_stay_per_target() {
    let root = temp_project("keep-n");
    let policy = HistoryPolicy {
        keep_autosaves: 3,
        keep_saves: 2,
        max_bytes: 0,
    };
    let level = level_target(&root);
    for n in 0..5 {
        record(&root, &autosave(&level, &format!("auto {n}")), &policy).expect("record");
        record(&root, &save(&level, &format!("save {n}")), &policy).expect("record");
    }
    let kept = texts(&root, &level);
    assert_eq!(
        kept,
        vec![
            (VersionSource::Save, "save 4".to_string()),
            (VersionSource::Autosave, "auto 4".to_string()),
            (VersionSource::Save, "save 3".to_string()),
            (VersionSource::Autosave, "auto 3".to_string()),
            (VersionSource::Autosave, "auto 2".to_string()),
        ],
        "newest first; 3 autosaves and 2 saves"
    );
    // Another target keeps its own count.
    let other = VersionTarget::level(&root, Some(&root.join("assets/maps/other.level.ron")));
    record(&root, &autosave(&other, "other"), &policy).expect("record");
    assert_eq!(versions_of(&root, &other).len(), 1);
    assert_eq!(versions_of(&root, &level).len(), 5);
    assert_eq!(targets(&root).len(), 2);
    let _ = std::fs::remove_dir_all(&root);
}

/// The same bytes twice (an autosave tick with nothing new, a revert) keep one version.
#[test]
fn identical_bytes_are_kept_once() {
    let root = temp_project("dedupe");
    let policy = HistoryPolicy::default();
    let level = level_target(&root);
    assert!(
        record(&root, &autosave(&level, "same"), &policy)
            .expect("first")
            .is_some()
    );
    assert!(
        record(&root, &autosave(&level, "same"), &policy)
            .expect("second")
            .is_none()
    );
    // The other source is its own list.
    assert!(
        record(&root, &save(&level, "same"), &policy)
            .expect("save")
            .is_some()
    );
    assert!(
        record(&root, &autosave(&level, "changed"), &policy)
            .expect("changed")
            .is_some()
    );
    assert_eq!(versions_of(&root, &level).len(), 3);
    let _ = std::fs::remove_dir_all(&root);
}

/// The size cap removes the oldest versions anywhere first, never the one just written; a target
/// left without versions loses its folder.
#[test]
fn the_size_cap_prunes_the_oldest_first() {
    let root = temp_project("cap");
    let unlimited = HistoryPolicy {
        keep_autosaves: 100,
        keep_saves: 100,
        max_bytes: 0,
    };
    let old = VersionTarget::level(&root, Some(&root.join("assets/maps/old.level.ron")));
    let level = level_target(&root);
    record(&root, &autosave(&old, &"a".repeat(100)), &unlimited).expect("record");
    for n in 0..3 {
        record(
            &root,
            &autosave(&level, &format!("{n}{}", "b".repeat(99))),
            &unlimited,
        )
        .expect("record");
    }
    assert_eq!(store_size(&root), 400);
    let capped = HistoryPolicy {
        max_bytes: 250,
        ..unlimited
    };
    let newest = record(&root, &autosave(&level, &"c".repeat(100)), &capped)
        .expect("record")
        .expect("written");
    assert!(store_size(&root) <= 250, "{}", store_size(&root));
    assert!(newest.file.is_file(), "the version just written stays");
    assert!(versions_of(&root, &old).is_empty(), "the oldest went first");
    assert!(
        !history_dir(&root).join(old.id()).exists(),
        "an empty target folder is removed"
    );
    let left: Vec<String> = texts(&root, &level)
        .into_iter()
        .map(|(_, t)| t[..1].to_string())
        .collect();
    assert_eq!(left, vec!["c", "2"], "then the oldest of the level");
    let _ = std::fs::remove_dir_all(&root);
}

/// Versions of two targets written in the same millisecond leave in write order under the size cap,
/// whatever order the target ids sort in.
#[test]
fn same_millisecond_versions_leave_in_write_order() {
    let root = temp_project("cap-same-ms");
    let unlimited = HistoryPolicy {
        keep_autosaves: 100,
        keep_saves: 100,
        max_bytes: 0,
    };
    let a = level_target(&root);
    let b = VersionTarget::level(&root, Some(&root.join("assets/maps/b.level.ron")));
    // Write the target whose id sorts LAST first, so an id tie-break would pick the wrong one.
    let (first, second) = if a.id() > b.id() { (a, b) } else { (b, a) };
    record(&root, &autosave(&first, &"f".repeat(100)), &unlimited).expect("record");
    record(&root, &autosave(&second, &"s".repeat(100)), &unlimited).expect("record");
    // Give every version the same millisecond, keeping its sequence number.
    for target in [&first, &second] {
        for v in versions_of(&root, target) {
            let stem = v.file.file_stem().and_then(|s| s.to_str()).expect("stem");
            let rest = stem.split_once('-').expect("ms-").1;
            let renamed = v
                .file
                .with_file_name(format!("{:013}-{rest}.{VERSION_EXTENSION}", 1));
            std::fs::rename(&v.file, renamed).expect("rename");
        }
    }
    let capped = HistoryPolicy {
        max_bytes: 150,
        ..unlimited
    };
    let report = prune(&root, &capped, None);
    assert_eq!(report.removed, 1);
    assert!(versions_of(&root, &first).is_empty(), "the first written went");
    assert_eq!(versions_of(&root, &second).len(), 1, "the second written stays");
    let _ = std::fs::remove_dir_all(&root);
}

/// The policy follows the preferences, and 0 keeps nothing of that source.
#[test]
fn the_policy_follows_the_preferences() {
    let mut prefs = crate::editor_prefs::EditorPreferences::default();
    assert_eq!(HistoryPolicy::from_prefs(&prefs), HistoryPolicy::default());
    prefs
        .set(PREF_KEEP_AUTOSAVES, &crate::editor_prefs::PrefValue::Int(0))
        .expect("set");
    prefs
        .set(PREF_MAX_MB, &crate::editor_prefs::PrefValue::Int(3))
        .expect("set");
    let policy = HistoryPolicy::from_prefs(&prefs);
    assert_eq!(policy.keep_autosaves, 0);
    assert_eq!(policy.max_bytes, 3 * 1024 * 1024);
    let root = temp_project("policy");
    let level = level_target(&root);
    assert!(
        record(&root, &autosave(&level, "x"), &policy)
            .expect("record")
            .is_none()
    );
    assert!(versions_of(&root, &level).is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

/// An id from an agent names a file inside the history folder or nothing.
#[test]
fn a_forged_version_id_names_no_file() {
    let root = temp_project("forged");
    let level = level_target(&root);
    let version = record(&root, &autosave(&level, "x"), &HistoryPolicy::default())
        .expect("record")
        .expect("written");
    assert_eq!(
        find(&root, &version.id).map(|v| v.file),
        Some(version.file.clone())
    );
    for forged in [
        "../x",
        "0123456789abcdef/../../Regia",
        "/etc/passwd",
        "0123456789ABCDEF/0000000000001-000001-save",
        &format!("{}/x-y-autosave", level.id()),
        &format!("{}/{}", level.id(), "0000000000001-000001-nope"),
        "",
    ] {
        assert!(find(&root, forged).is_none(), "{forged}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

fn app_with_level(root: &Path) -> App {
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    let world = app.world_mut();
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(root.to_path_buf()),
    });
    world.init_resource::<crate::command::UndoStack>();
    world.init_resource::<crate::selection::Selection>();
    let mut open = crate::level_io::new_level(world);
    open.path = Some(root.join("assets/maps/main.level.ron"));
    world.insert_resource(open);
    doc_autosave::install(world);
    app
}

fn spawn(world: &mut World, name: &str) {
    use crate::command::EditorCommand as _;
    let mut command = crate::command::SpawnEntities::new_entity(
        world,
        regia_core::id::Guid::new_random(),
        name,
        regia_scene::prelude::Transform::default(),
    )
    .expect("spawn");
    command.apply(world);
    world
        .resource_mut::<crate::command::UndoStack>()
        .push(Box::new(command));
}

fn names(world: &mut World) -> Vec<String> {
    let mut query = world.query::<&regia_scene::prelude::Name>();
    let mut out: Vec<String> = query.iter(world).map(|n| n.as_str().to_string()).collect();
    out.sort();
    out
}

fn tick_now(world: &mut World, root: &Path) {
    doc_autosave::tick(
        world,
        doc_autosave::TickInput {
            project_root: root,
            now: Instant::now(),
            force: true,
        },
    );
    doc_autosave::flush(world, FLUSH);
}

/// P-325: the level autosave (on the worker) and a manual save (`save_level_and_terrain`, the path
/// every save takes) each keep a version of the level, with the bytes they wrote.
#[test]
fn the_autosave_and_a_save_keep_level_versions() {
    let root = temp_project("level-hooks");
    let mut app = app_with_level(&root);
    let world = app.world_mut();
    spawn(world, "FirstRock");
    tick_now(world, &root);
    let level = level_target(&root);
    let kept = versions_of(&root, &level);
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(kept[0].source, VersionSource::Autosave);
    let autosaved = std::fs::read_to_string(crate::autosave::autosave_path(
        &root,
        &world.resource::<crate::level_io::OpenLevel>().clone(),
    ))
    .expect("autosave file");
    assert_eq!(
        std::fs::read_to_string(&kept[0].file).expect("version"),
        autosaved
    );

    let open = world.resource::<crate::level_io::OpenLevel>().clone();
    let path = open.path.clone().expect("path");
    crate::level_io::save_level_and_terrain(world, &open, &path).expect("save");
    doc_autosave::flush(world, FLUSH);
    let kept = versions_of(&root, &level);
    assert_eq!(kept[0].source, VersionSource::Save, "{kept:?}");
    assert_eq!(
        std::fs::read(&kept[0].file).expect("version"),
        std::fs::read(&path).expect("level"),
        "the save version holds the saved file"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The restore is a new undoable change: the open level gets the version's contents, the file on
/// disk is untouched and dirty, and undo brings the replaced contents back.
#[test]
fn a_level_version_restores_as_one_undo_entry() {
    let root = temp_project("level-restore");
    let mut app = app_with_level(&root);
    let world = app.world_mut();
    spawn(world, "OldRock");
    let open = world.resource::<crate::level_io::OpenLevel>().clone();
    let path = open.path.clone().expect("path");
    crate::level_io::save_level_and_terrain(world, &open, &path).expect("save");
    doc_autosave::flush(world, FLUSH);
    world.resource_mut::<crate::command::UndoStack>().mark_saved();
    let saved = std::fs::read(&path).expect("level");
    let version = versions_of(&root, &level_target(&root))[0].clone();

    spawn(world, "NewRock");
    assert_eq!(names(world), vec!["NewRock", "OldRock"]);
    let depth = world.resource::<crate::command::UndoStack>().undo_depth();
    let outcome = restore(world, &version.id).expect("restore");
    assert!(outcome.undo.starts_with("Restore "), "{}", outcome.undo);
    assert_eq!(names(world), vec!["OldRock"], "the version's contents");
    let stack = world.resource::<crate::command::UndoStack>();
    assert_eq!(stack.undo_depth(), depth + 1, "one undo entry");
    assert!(stack.is_dirty(), "unsaved until a save");
    assert_eq!(std::fs::read(&path).expect("level"), saved, "nothing written");

    assert!(crate::command::undo_world(world));
    assert_eq!(
        names(world),
        vec!["NewRock", "OldRock"],
        "undo brings the edit back"
    );
    assert!(crate::command::redo_world(world));
    assert_eq!(names(world), vec!["OldRock"]);
    // Older entries still find their entity: undoing past the restore removes NewRock again
    // after undoing the restore.
    assert!(crate::command::undo_world(world));
    assert!(crate::command::undo_world(world));
    assert_eq!(
        names(world),
        vec!["OldRock"],
        "the spawn's undo found the respawned entity"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A level version restores only into its own open level; an unknown id is refused.
#[test]
fn a_level_version_needs_its_level_open() {
    let root = temp_project("level-not-open");
    let mut app = app_with_level(&root);
    let world = app.world_mut();
    let other = VersionTarget::level(&root, Some(&root.join("assets/maps/other.level.ron")));
    let version = record(
        &root,
        &save(&other, "( entities: [] )"),
        &HistoryPolicy::default(),
    )
    .expect("record")
    .expect("written");
    assert_eq!(
        restore(world, &version.id)
            .map(|_| ())
            .expect_err("not open")
            .code,
        "version_level_not_open"
    );
    assert_eq!(
        restore(world, "0123456789abcdef/0000000000001-000001-save")
            .map(|_| ())
            .expect_err("missing")
            .code,
        "no_such_version"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A test editor kind: one document in a resource.
#[derive(Resource, Default, Clone)]
struct FakeDoc {
    path: Option<PathBuf>,
    text: String,
    dirty: bool,
}

fn fake_dirty(world: &World) -> Vec<DocKey> {
    world
        .get_resource::<FakeDoc>()
        .filter(|d| d.dirty)
        .map(|d| vec![DocKey::new("fake", d.path.as_deref())])
        .unwrap_or_default()
}

fn fake_snapshot(world: &World) -> Vec<DocSnapshot> {
    world
        .get_resource::<FakeDoc>()
        .filter(|d| d.dirty)
        .map(|d| {
            vec![DocSnapshot {
                key: DocKey::new("fake", d.path.as_deref()),
                bytes: d.text.clone().into_bytes(),
            }]
        })
        .unwrap_or_default()
}

fn fake_restore(world: &mut World, key: &DocKey, bytes: &[u8]) -> Result<(), String> {
    world.insert_resource(FakeDoc {
        path: key.original.clone(),
        text: String::from_utf8(bytes.to_vec()).map_err(|e| e.to_string())?,
        dirty: true,
    });
    Ok(())
}

fn with_fake(world: &mut World, file: &Path, text: &str, dirty: bool) {
    world.resource_mut::<DocAutosaveRegistry>().register(DocProvider {
        kind: "fake",
        dirty: fake_dirty,
        snapshot: fake_snapshot,
        restore: fake_restore,
    });
    world.insert_resource(FakeDoc {
        path: Some(file.to_path_buf()),
        text: text.to_string(),
        dirty,
    });
}

/// P-325 for documents: a document autosave keeps a version, and a document that turns clean (its
/// editor saved it) keeps its saved file as a `save` version.
#[test]
fn document_autosaves_and_saves_keep_versions() {
    let root = temp_project("doc-hooks");
    let file = root.join("assets/a.fake");
    std::fs::write(&file, "on disk").expect("doc");
    let mut app = app_with_level(&root);
    let world = app.world_mut();
    with_fake(world, &file, "edited", true);
    tick_now(world, &root);
    let target = VersionTarget::doc(&root, &DocKey::new("fake", Some(&file)));
    assert_eq!(target.original.as_deref(), Some("assets/a.fake"));
    assert_eq!(
        texts(&root, &target),
        vec![(VersionSource::Autosave, "edited".to_string())]
    );

    // The editor saves: the file has the edit and the document is clean on the next frame.
    std::fs::write(&file, "edited and saved").expect("save");
    world.resource_mut::<FakeDoc>().dirty = false;
    tick_now(world, &root);
    assert_eq!(
        texts(&root, &target)[0],
        (VersionSource::Save, "edited and saved".to_string())
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A document version opens in its editor as unsaved edits, the unsaved state it replaced is kept as
/// a `before_restore` version, undo opens that state again, and the file is never written.
#[test]
fn a_document_version_restores_as_unsaved_edits() {
    let root = temp_project("doc-restore");
    let file = root.join("assets/a.fake");
    std::fs::write(&file, "on disk").expect("doc");
    let mut app = app_with_level(&root);
    let world = app.world_mut();
    with_fake(world, &file, "unsaved work", true);
    let target = VersionTarget::doc(&root, &DocKey::new("fake", Some(&file)));
    let version = record(&root, &save(&target, "old version"), &HistoryPolicy::default())
        .expect("record")
        .expect("written");
    let outcome = restore(world, &version.id).expect("restore");
    assert!(outcome.kept, "the unsaved work is kept as a version first");
    let doc = world.resource::<FakeDoc>().clone();
    assert_eq!((doc.text.as_str(), doc.dirty), ("old version", true));
    assert_eq!(
        std::fs::read_to_string(&file).expect("file"),
        "on disk",
        "nothing written"
    );
    assert!(
        texts(&root, &target).contains(&(VersionSource::BeforeRestore, "unsaved work".to_string())),
        "{:?}",
        texts(&root, &target)
    );
    let stack = world.resource::<crate::command::UndoStack>();
    assert_eq!(stack.undo_depth(), 1);
    assert!(crate::command::undo_world(world));
    assert_eq!(
        world.resource::<FakeDoc>().text,
        "unsaved work",
        "undo opens the replaced state"
    );

    // A kind no editor restores is refused.
    let lost = VersionTarget {
        kind: "gone".into(),
        ..target.clone()
    };
    let version = record(&root, &save(&lost, "x"), &HistoryPolicy::default())
        .expect("record")
        .expect("written");
    assert_eq!(
        restore(world, &version.id)
            .map(|_| ())
            .expect_err("no editor")
            .code,
        "version_no_editor"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The preview reads a version without touching the world: its text and, for a level, its
/// entity names.
#[test]
fn the_preview_is_read_only() {
    let root = temp_project("preview");
    let mut app = app_with_level(&root);
    let world = app.world_mut();
    spawn(world, "PreviewRock");
    tick_now(world, &root);
    let version = versions_of(&root, &level_target(&root))[0].clone();
    let depth = world.resource::<crate::command::UndoStack>().undo_depth();
    let shown = preview(&version).expect("preview");
    assert!(shown.text.as_deref().is_some_and(|t| t.contains("PreviewRock")));
    assert_eq!(shown.entities, Some(vec!["PreviewRock".to_string()]));
    assert!(!shown.truncated);
    assert_eq!(world.resource::<crate::command::UndoStack>().undo_depth(), depth);

    let big = VersionTarget::doc(&root, &DocKey::new("fake", Some(&root.join("assets/big.fake"))));
    let version = record(
        &root,
        &save(&big, &"é".repeat(PREVIEW_LIMIT)),
        &HistoryPolicy::default(),
    )
    .expect("record")
    .expect("written");
    let shown = preview(&version).expect("preview");
    assert!(shown.truncated);
    assert!(shown.text.as_deref().is_some_and(|t| t.len() <= PREVIEW_LIMIT));
    assert_eq!(shown.entities, None);
    let _ = std::fs::remove_dir_all(&root);
}

fn key(key: egui::Key) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    }
}

fn frame(
    ctx: &egui::Context,
    world: &World,
    root: &Path,
    panel: &mut VersionHistoryPanel,
    events: Vec<egui::Event>,
) -> Option<PanelAction> {
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(900.0, 700.0),
        )),
        events,
        ..Default::default()
    };
    let mut action = None;
    let mut output = ctx.run_ui(raw, |ui| {
        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(a) = panel_ui(ui, world, root, panel) {
                action = Some(a);
            }
        });
    });
    output.textures_delta.clear();
    action
}

/// The window's keyboard path (accessibility bar): Tab reaches the version row, Enter selects it,
/// Tab reaches Preview and Restore, and Enter presses them; no mouse.
#[test]
fn the_window_works_from_the_keyboard() {
    let root = temp_project("keyboard");
    let mut app = app_with_level(&root);
    let world = app.world_mut();
    spawn(world, "KeyRock");
    tick_now(world, &root);
    let version = versions_of(&root, &level_target(&root))[0].clone();
    let ctx = egui::Context::default();
    let mut panel = VersionHistoryPanel::default();
    frame(&ctx, world, &root, &mut panel, vec![]);
    // One target: no picker, so the first Tab stop is the version row; Enter selects it.
    let mut selected = false;
    for _ in 0..6 {
        frame(&ctx, world, &root, &mut panel, vec![key(egui::Key::Tab)]);
        frame(&ctx, world, &root, &mut panel, vec![key(egui::Key::Enter)]);
        if panel.selected.is_some() {
            selected = true;
            break;
        }
    }
    assert!(selected, "a Tab stop selects the version with Enter");
    assert_eq!(panel.selected.as_deref(), Some(version.id.as_str()));
    let focused = |ctx: &egui::Context| ctx.memory(|m| m.focused());
    let row = focused(&ctx).expect("the version row has focus");
    // The next stops are Preview, then Restore.
    frame(&ctx, world, &root, &mut panel, vec![key(egui::Key::Tab)]);
    let preview_stop = frame(&ctx, world, &root, &mut panel, vec![key(egui::Key::Enter)]);
    assert_eq!(preview_stop, Some(PanelAction::Preview(version.id.clone())));
    frame(&ctx, world, &root, &mut panel, vec![key(egui::Key::Tab)]);
    let restore = frame(&ctx, world, &root, &mut panel, vec![key(egui::Key::Enter)]);
    assert_eq!(restore, Some(PanelAction::Restore(version.id.clone())));
    let restore_button = focused(&ctx).expect("Restore has focus");
    // Review A5: with the preview shown, Tab moves into the read-only text and out of it again
    // (a code editor's text would keep Tab), so Tab comes back around to the version row. Each
    // key press is followed by an idle frame, as between real key presses: a text field locks
    // its keys only from the frame after it gained focus.
    panel.preview = Some(preview(&version).expect("preview"));
    frame(&ctx, world, &root, &mut panel, vec![]);
    frame(&ctx, world, &root, &mut panel, vec![key(egui::Key::Tab)]);
    frame(&ctx, world, &root, &mut panel, vec![]);
    let in_preview = focused(&ctx);
    assert!(
        in_preview.is_some() && in_preview != Some(restore_button),
        "Tab from Restore reaches the preview text"
    );
    let mut back_at_row = false;
    for _ in 0..8 {
        frame(&ctx, world, &root, &mut panel, vec![key(egui::Key::Tab)]);
        // A Tab past the last stop lands on the first one in the next frame.
        frame(&ctx, world, &root, &mut panel, vec![]);
        frame(&ctx, world, &root, &mut panel, vec![]);
        if focused(&ctx) == Some(row) {
            back_at_row = true;
            break;
        }
    }
    assert!(
        back_at_row,
        "Tab leaves the read-only preview and returns to the version row"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Review A4: an open window does not re-read the store every frame. Idle frames read nothing; a
/// new version, a picked target and a closed-and-reopened window each read the store once, and the
/// list then shows the change.
#[test]
fn the_open_window_reads_the_store_only_when_it_changed() {
    let root = temp_project("list-cache");
    let mut app = app_with_level(&root);
    let world = app.world_mut();
    spawn(world, "CacheRock");
    tick_now(world, &root);
    let level = level_target(&root);
    assert_eq!(versions_of(&root, &level).len(), 1);
    let ctx = egui::Context::default();
    let mut panel = VersionHistoryPanel {
        open: true,
        ..VersionHistoryPanel::default()
    };
    frame(&ctx, world, &root, &mut panel, vec![]);
    let first = list_reads(&root);
    assert!(first >= 1, "the first frame lists the store");
    for _ in 0..8 {
        frame(&ctx, world, &root, &mut panel, vec![]);
    }
    assert_eq!(list_reads(&root), first, "idle frames read nothing");
    // A new version (what the autosave worker writes) is listed at the next frame.
    let policy = HistoryPolicy::default();
    record(&root, &save(&level, "a manual save"), &policy).expect("record");
    let after_write = list_reads(&root);
    frame(&ctx, world, &root, &mut panel, vec![]);
    assert_eq!(list_reads(&root), after_write + 1, "one read after a write");
    assert_eq!(panel.listed().len(), 2, "the new version is listed");
    for _ in 0..4 {
        frame(&ctx, world, &root, &mut panel, vec![]);
    }
    assert_eq!(list_reads(&root), after_write + 1, "then idle again");
    // A document target appears; picking it re-reads once.
    let doc = VersionTarget::doc(&root, &DocKey::new("fake", Some(&root.join("assets/c.fake"))));
    record(&root, &save(&doc, "doc text"), &policy).expect("record");
    frame(&ctx, world, &root, &mut panel, vec![]);
    let before_pick = list_reads(&root);
    panel.target = Some(doc.id());
    frame(&ctx, world, &root, &mut panel, vec![]);
    frame(&ctx, world, &root, &mut panel, vec![]);
    assert_eq!(list_reads(&root), before_pick + 1, "a picked target reads once");
    assert_eq!(panel.listed().len(), 1);
    // Closing drops the list; opening again reads it fresh.
    panel.close();
    panel.open = true;
    frame(&ctx, world, &root, &mut panel, vec![]);
    assert_eq!(list_reads(&root), before_pick + 2, "reopening reads once");
    let _ = std::fs::remove_dir_all(&root);
}

/// The agent surface over the methods' handlers (the dispatcher e2e is
/// `tests/version_history_agent.rs`).
#[test]
fn the_agent_lists_previews_and_restores() {
    let root = temp_project("agent");
    let mut app = app_with_level(&root);
    let world = app.world_mut();
    spawn(world, "AgentRock");
    tick_now(world, &root);
    let call = |world: &mut World, method: &str, params: Value| {
        let (_, handler) = methods()
            .into_iter()
            .find(|(schema, _)| schema.name == method)
            .expect("registered");
        handler(world, &params)
    };
    let listed = call(world, "editor.versions", json!({})).expect("versions");
    assert_eq!(
        listed["target"]["original"], "assets/maps/main.level.ron",
        "{listed}"
    );
    let id = listed["versions"][0]["id"].as_str().expect("id").to_string();
    assert_eq!(listed["versions"][0]["source"], "autosave");
    let by_path = call(
        world,
        "editor.versions",
        json!({ "path": "assets/maps/main.level.ron" }),
    )
    .expect("by path");
    assert_eq!(by_path["versions"], listed["versions"]);
    let shown = call(world, "editor.version_preview", json!({ "id": id })).expect("preview");
    assert_eq!(shown["entities"], json!(["AgentRock"]));
    let restored = call(world, "editor.version_restore", json!({ "id": id })).expect("restore");
    assert_eq!(restored["restored"], json!(id));
    let code = |err: RpcError| {
        err.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    assert_eq!(
        code(call(world, "editor.version_restore", json!({})).expect_err("no id")),
        "invalid_version_id"
    );
    assert_eq!(
        code(call(world, "editor.version_preview", json!({ "id": "../x" })).expect_err("forged")),
        "no_such_version"
    );
    let dest = root.with_file_name(format!(
        "{}-copy",
        root.file_name().and_then(|n| n.to_str()).unwrap_or("p")
    ));
    let copied = call(
        world,
        "editor.duplicate_project",
        json!({ "dest": dest.display().to_string() }),
    )
    .expect("duplicate");
    assert!(dest.join("Regia.toml").is_file(), "{copied}");
    assert!(!dest.join(".regia").exists(), "editor state is not copied");
    assert_eq!(
        code(
            call(
                world,
                "editor.duplicate_project",
                json!({ "dest": dest.display().to_string() })
            )
            .expect_err("exists")
        ),
        "duplicate_dest_exists"
    );
    let _ = std::fs::remove_dir_all(&dest);
    let _ = std::fs::remove_dir_all(&root);
}

/// A document saved before any autosave of it (the session owns none) still keeps a `save`
/// version: the dirty lists are compared at most every `SAVE_CHECK_EVERY`, so the version comes at
/// the first check after the save.
#[test]
fn a_save_before_any_autosave_keeps_a_version_at_the_next_check() {
    let root = temp_project("doc-save-throttle");
    let file = root.join("assets/b.fake");
    std::fs::write(&file, "first save").expect("doc");
    let mut app = app_with_level(&root);
    let world = app.world_mut();
    with_fake(world, &file, "editing", true);
    let t0 = Instant::now();
    let at = |world: &mut World, now: Instant| {
        doc_autosave::tick(
            world,
            doc_autosave::TickInput {
                project_root: &root,
                now,
                force: false,
            },
        );
        doc_autosave::flush(world, FLUSH);
    };
    at(world, t0);
    assert!(
        world.resource::<doc_autosave::AutosaveState>().written.is_empty(),
        "no autosave yet"
    );
    world.resource_mut::<FakeDoc>().dirty = false;
    let target = VersionTarget::doc(&root, &DocKey::new("fake", Some(&file)));
    at(world, t0 + SAVE_CHECK_EVERY / 2);
    assert!(versions_of(&root, &target).is_empty(), "not checked again yet");
    at(world, t0 + SAVE_CHECK_EVERY);
    assert_eq!(
        texts(&root, &target),
        vec![(VersionSource::Save, "first save".to_string())]
    );
    let _ = std::fs::remove_dir_all(&root);
}
