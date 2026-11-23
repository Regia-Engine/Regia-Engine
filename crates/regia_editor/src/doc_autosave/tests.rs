// SPDX-License-Identifier: MPL-2.0
//! Tests of the document autosave registry, index, worker and recovery (ADR-9770).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use regia_app::prelude::App;

use super::*;

const FLUSH: Duration = Duration::from_secs(60);

fn temp_project(label: &str) -> PathBuf {
    let dir = regia_test_utils::unique_temp_dir(&format!("doc-autosave-{label}"));
    std::fs::create_dir_all(dir.join("assets/materials")).expect("dir");
    dir
}

/// A test editor kind: one document in a resource, dirty while `dirty` is set.
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

const FAKE: DocProvider = DocProvider {
    kind: "fake",
    dirty: fake_dirty,
    snapshot: fake_snapshot,
    restore: fake_restore,
};

fn app_with_fake(root: &Path) -> App {
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    let world = app.world_mut();
    install(world);
    world.resource_mut::<DocAutosaveRegistry>().register(FAKE);
    world.insert_resource(FakeDoc {
        path: Some(root.join("assets/materials/a.fake")),
        text: "edited".into(),
        dirty: true,
    });
    app
}

fn tick_at(world: &mut World, root: &Path, now: Instant, force: bool) -> TickReport {
    tick(
        world,
        TickInput {
            project_root: root,
            now,
            force,
        },
    )
}

/// Every editor named by the brief that has a per-document dirty flag is registered.
#[test]
fn autosave_registry_lists_the_asset_editors() {
    let mut registry = DocAutosaveRegistry::default();
    crate::doc_autosave_providers::register_builtin(&mut registry);
    let kinds = registry.kinds();
    for kind in [
        "matgraph",
        "ui_design",
        "procgen",
        "behavior_tree",
        "state_tree",
        "table",
        "sound_cue",
        "vfx",
        "conversation",
        "anim_graph",
        "ui_anim",
        "sequence",
        "prefab",
        "shots",
    ] {
        assert!(kinds.contains(&kind), "{kind} missing from {kinds:?}");
    }
}

/// The tick writes a dirty document under `.regia/autosave/docs/` with an index row, only once
/// the interval passed, off the main thread; a save (the document turns clean) removes it.
#[test]
fn autosave_tick_writes_documents_and_a_save_removes_them() {
    let root = temp_project("tick");
    let mut app = app_with_fake(&root);
    let world = app.world_mut();
    world.resource_mut::<AutosaveState>().interval = Some(Duration::from_secs(60));
    let t0 = Instant::now();
    // The first dirty frame starts the clock; nothing is written before the interval.
    assert!(!tick_at(world, &root, t0, false).queued);
    assert!(!tick_at(world, &root, t0 + Duration::from_secs(59), false).queued);
    let report = tick_at(world, &root, t0 + Duration::from_secs(61), false);
    assert!(report.queued, "{report:?}");
    assert_eq!(report.docs.len(), 1);
    assert_eq!(flush(world, FLUSH), 1);
    let index = read_index(&root);
    assert_eq!(index.docs.len(), 1, "{index:?}");
    let row = &index.docs[0];
    assert_eq!(row.kind, "fake");
    assert_eq!(row.original.as_deref(), Some("assets/materials/a.fake"));
    assert_eq!(row.size, 6);
    let bytes = std::fs::read(docs_dir(&root).join(&row.file)).expect("bytes");
    assert_eq!(bytes, b"edited");
    // No temp files are left behind (atomic writes).
    let stray: Vec<_> = std::fs::read_dir(docs_dir(&root))
        .expect("dir")
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(stray.is_empty(), "{stray:?}");

    // A manual save: the document is clean, the next frame removes its autosave.
    world.resource_mut::<FakeDoc>().dirty = false;
    tick_at(world, &root, t0 + Duration::from_secs(62), false);
    flush(world, FLUSH);
    assert!(read_index(&root).docs.is_empty());
    assert!(!docs_dir(&root).exists(), "the empty docs folder goes too");
    let _ = std::fs::remove_dir_all(&root);
}

/// A crashed session's documents are offered, restored dirty at their original path with the same
/// bytes, and discarded per item.
#[test]
fn recovery_restores_and_discards_documents() {
    let root = temp_project("recover");
    let mut first = app_with_fake(&root);
    tick_at(first.world_mut(), &root, Instant::now(), true);
    flush(first.world_mut(), FLUSH);
    // A second document (untitled).
    write_docs(
        &root,
        &[DocSnapshot {
            key: DocKey::new("fake", None),
            bytes: b"untitled bytes".to_vec(),
        }],
    )
    .expect("write");
    drop(first);

    // Next launch.
    let mut app = App::new();
    let world = app.world_mut();
    install(world);
    world.resource_mut::<DocAutosaveRegistry>().register(FAKE);
    let offers = RecoveryOffers::scan(&root);
    assert_eq!(offers.docs.len(), 2, "{offers:?}");
    world.insert_resource(offers);
    let rows = world.resource::<RecoveryOffers>().rows();
    let titled = rows
        .iter()
        .find(|r| r.original.is_some())
        .expect("titled row")
        .id
        .clone();
    let untitled = rows
        .iter()
        .find(|r| r.original.is_none())
        .expect("untitled")
        .id
        .clone();

    let outcome = restore(world, std::slice::from_ref(&titled));
    assert_eq!(outcome.done, vec![titled.clone()], "{outcome:?}");
    let doc = world.resource::<FakeDoc>().clone();
    assert!(doc.dirty);
    assert_eq!(doc.text, "edited");
    assert_eq!(doc.path, Some(root.join("assets/materials/a.fake")));

    let outcome = discard(world, std::slice::from_ref(&untitled));
    assert_eq!(outcome.done, vec![untitled]);
    assert!(world.resource::<RecoveryOffers>().is_empty());
    // The discarded file is gone; the restored one stays until the document is saved.
    assert_eq!(read_index(&root).docs.len(), 1);
    let _ = std::fs::remove_dir_all(&root);
}

/// Restore all and an unknown id.
#[test]
fn recovery_restore_all_and_unknown_ids() {
    let root = temp_project("restore-all");
    write_docs(
        &root,
        &[DocSnapshot {
            key: DocKey::new("fake", Some(&root.join("assets/x.fake"))),
            bytes: b"x".to_vec(),
        }],
    )
    .expect("write");
    let mut app = App::new();
    let world = app.world_mut();
    install(world);
    world.resource_mut::<DocAutosaveRegistry>().register(FAKE);
    world.insert_resource(RecoveryOffers::scan(&root));
    let outcome = restore(world, &[]);
    assert_eq!(outcome.done.len(), 1, "{outcome:?}");
    let outcome = restore(world, &["doc:nope".to_string()]);
    assert_eq!(outcome.failed.len(), 1);
    let _ = std::fs::remove_dir_all(&root);
}

/// The UI animation editor takes part in document autosave: an edited library autosaves and
/// Restore gives byte-identical text at its original path, dirty until a save.
#[test]
fn autosave_ui_anim_restores_byte_identical_text() {
    use regia_ui::anim_timeline::{AnimKey, AnimValue};

    use crate::ui_anim_doc::{AnimOp, UiAnimDoc, UiAnimEditor};

    let root = temp_project("ui-anim");
    let path = root.join("assets/panel.uianim.ron");
    // A saved document, then an edit of the session that crashed (a new animation).
    let mut doc = UiAnimDoc::new_document(Some(path.clone()));
    doc.apply(AnimOp::AddAnimation {
        name: "panel::fade".into(),
        property: Some("opacity".into()),
    })
    .expect("add");
    doc.save(None).expect("save");
    let mut doc = UiAnimDoc::open(&path).expect("open");
    doc.apply(AnimOp::AddAnimation {
        name: "panel::pulse".into(),
        property: None,
    })
    .expect("edit");
    doc.apply(AnimOp::SetKey {
        name: "panel::pulse".into(),
        property: "opacity".into(),
        key: AnimKey {
            time: 0.5,
            value: AnimValue::Number(0.25),
            ease: Some("quad_in".to_owned()),
        },
    })
    .expect("key");
    assert!(doc.dirty());
    let expected = doc.text().expect("text").into_bytes();

    let mut first = App::new();
    let world = first.world_mut();
    install(world);
    let mut editor = UiAnimEditor::default();
    editor.set_doc(doc);
    world.insert_resource(editor);
    // One tick writes the document; the bytes are what a save writes.
    let report = tick_at(world, &root, Instant::now(), true);
    assert_eq!(report.docs.len(), 1, "{report:?}");
    assert_eq!(flush(world, FLUSH), 1);
    drop(first);

    // The next launch offers it; Restore gives byte-identical text, dirty at the original path.
    let mut app = App::new();
    let world = app.world_mut();
    install(world);
    let offers = RecoveryOffers::scan(&root);
    assert_eq!(offers.docs.len(), 1, "{offers:?}");
    assert_eq!(offers.docs[0].entry.kind, "ui_anim");
    world.insert_resource(offers);
    let outcome = restore(world, &[]);
    assert_eq!(outcome.done.len(), 1, "{outcome:?}");
    assert!(outcome.failed.is_empty(), "{outcome:?}");
    let doc = world
        .resource::<UiAnimEditor>()
        .doc()
        .expect("the library is open");
    assert!(doc.dirty(), "dirty until a save");
    assert_eq!(doc.path.as_deref(), Some(path.as_path()));
    assert_eq!(doc.text().expect("text").into_bytes(), expected);
    let _ = std::fs::remove_dir_all(&root);
}

/// A project without `.regia/autosave/docs/` opens unchanged: nothing offered, nothing created.
#[test]
fn autosave_absent_docs_folder_changes_nothing() {
    let root = temp_project("absent");
    let offers = RecoveryOffers::scan(&root);
    assert!(offers.is_empty());
    assert!(!autosave::autosave_dir(&root).exists());
    clear_docs_except(&root, &[]).expect("clear");
    assert!(!autosave::autosave_dir(&root).exists());
    let _ = std::fs::remove_dir_all(&root);
}

/// The clean exit keeps the documents still on offer and removes the rest.
#[test]
fn autosave_clean_exit_keeps_pending_offers() {
    let root = temp_project("exit");
    let pending = DocKey::new("fake", Some(&root.join("assets/p.fake")));
    let mine = DocKey::new("fake", Some(&root.join("assets/m.fake")));
    write_docs(
        &root,
        &[
            DocSnapshot {
                key: pending.clone(),
                bytes: b"p".to_vec(),
            },
            DocSnapshot {
                key: mine,
                bytes: b"m".to_vec(),
            },
        ],
    )
    .expect("write");
    clear_docs_except(&root, &[pending.id()]).expect("clear");
    let left: Vec<String> = read_index(&root).docs.into_iter().map(|d| d.id).collect();
    assert_eq!(left, vec![pending.id()]);
    let _ = std::fs::remove_dir_all(&root);
}

/// An edited index cannot point a restore outside the docs folder.
#[test]
fn autosave_index_paths_stay_inside_the_docs_folder() {
    let root = temp_project("escape");
    let index = DocIndex {
        version: INDEX_VERSION,
        docs: vec![DocIndexEntry {
            id: "x".into(),
            kind: "fake".into(),
            original: None,
            label: "x".into(),
            file: "../../../Regia.toml".into(),
            size: 1,
            saved_unix_ms: 0,
        }],
    };
    std::fs::create_dir_all(docs_dir(&root)).expect("dir");
    std::fs::write(root.join("Regia.toml"), "x").expect("toml");
    write_index(&root, &index).expect("index");
    assert!(doc_recovery_candidates(&root).is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

/// Item 2's measurement (hand-run, not a gate): the main-thread cost of one autosave tick on a
/// 20k-entity level, before (snapshot, encode and write on the main thread, the old
/// `autosave::write_autosave`) and after (snapshot here, encode and write on the worker).
/// `cargo test -p regia_editor --lib autosave_tick_cost_20k -- --ignored --nocapture`
#[test]
#[ignore = "measurement; prints numbers"]
fn autosave_tick_cost_20k_entities() {
    use crate::command::EditorCommand as _;
    let root = temp_project("cost");
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    let world = app.world_mut();
    let open = crate::level_io::new_level(world);
    world.insert_resource(open.clone());
    let mut undo = crate::command::UndoStack::new();
    for i in 0..20_000 {
        let mut spawn = crate::command::SpawnEntities::new_entity(
            world,
            regia_core::id::Guid::new_random(),
            &format!("Rock{i}"),
            regia_scene::prelude::Transform::default(),
        )
        .expect("spawn");
        spawn.apply(world);
        if i == 0 {
            undo.push(Box::new(spawn));
        }
    }
    world.insert_resource(undo);
    install(world);
    let mut before = Vec::new();
    let mut after = Vec::new();
    for round in 0..5 {
        let t = Instant::now();
        crate::autosave::write_autosave(world, &open, &root).expect("sync autosave");
        before.push(t.elapsed().as_micros());
        let report = tick_at(
            world,
            &root,
            Instant::now() + Duration::from_secs(600 * (round + 1)),
            true,
        );
        assert!(report.queued);
        after.push(u128::from(report.main_thread_us));
        flush(world, FLUSH);
    }
    before.sort_unstable();
    after.sort_unstable();
    println!(
        "autosave tick, 20000 entities, main thread: before (sync write) median {} us (min {}), after (worker) median {} us (min {})",
        before[2], before[0], after[2], after[0]
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// One crafted index row (`id` and `file` as given).
fn crafted_row(id: &str, file: &str) -> DocIndexEntry {
    DocIndexEntry {
        id: id.into(),
        kind: "fake".into(),
        original: None,
        label: "crafted".into(),
        file: file.into(),
        size: 1,
        saved_unix_ms: 0,
    }
}

/// Review sprint-rev-claude-50-k, autosave-recovery finding 1 (security): the index `id` column
/// was joined into a path unchecked (`remove_docs`, reached from Discard and from the clean exit),
/// so a crafted `index.ron` in a project the user opens deleted files outside the docs folder.
/// Rows whose `id` or `file` is not a plain name inside the folder (`../`, absolute paths,
/// Windows drive letters, backslashes, NUL) are never read, offered or removed through.
#[test]
fn autosave_index_ids_stay_inside_the_docs_folder() {
    let root = temp_project("escape-id");
    let docs = docs_dir(&root);
    std::fs::create_dir_all(&docs).expect("dir");
    // Files the crafted rows aim at, outside the docs folder.
    let up_one = autosave::autosave_dir(&root).join("one.autosave");
    let up_two = root.join(".regia/two.autosave");
    let outside = regia_test_utils::unique_temp_dir("doc-autosave-escape-outside");
    std::fs::create_dir_all(&outside).expect("outside");
    let absolute = outside.join("abs.autosave");
    for target in [&up_one, &up_two, &absolute] {
        std::fs::write(target, "keep").expect("target");
    }
    let absolute_id = outside.join("abs").to_string_lossy().into_owned();
    let good = DocKey::new("fake", Some(&root.join("assets/g.fake")));
    write_docs(
        &root,
        &[DocSnapshot {
            key: good.clone(),
            bytes: b"g".to_vec(),
        }],
    )
    .expect("write");
    let mut index = read_index(&root);
    let crafted_ids = [
        "../one".to_string(),
        "../../two".to_string(),
        absolute_id.clone(),
        "C:\\Windows\\x".to_string(),
        "C:x".to_string(),
        "..\\..\\two".to_string(),
        "a\0b".to_string(),
        String::new(),
        "0123456789ABCDEF".to_string(),
    ];
    for id in &crafted_ids {
        index.docs.push(crafted_row(id, "0123456789abcdef.autosave"));
    }
    // A valid-looking id with a crafted file column.
    for file in [
        "C:x.autosave",
        "C:\\x.autosave",
        "a\0b.autosave",
        "..",
        "",
        "/etc/passwd",
    ] {
        index.docs.push(crafted_row("fedcba9876543210", file));
    }
    write_index(&root, &index).expect("index");

    // The clean exit removes every autosave not on offer: never through a crafted id.
    clear_docs_except(&root, &[]).expect("clear");
    for target in [&up_one, &up_two, &absolute] {
        assert!(
            target.is_file(),
            "{} was deleted through a crafted id",
            target.display()
        );
    }
    // A direct removal of a crafted id reaches nothing either.
    remove_docs(&root, &crafted_ids).expect("remove");
    for target in [&up_one, &up_two, &absolute] {
        assert!(
            target.is_file(),
            "{} was deleted by remove_docs",
            target.display()
        );
    }

    // Reading: only the valid row survives (re-create it; the clear above removed it).
    write_index(&root, &index).expect("index");
    std::fs::write(docs.join(format!("{}.autosave", good.id())), "g").expect("bytes");
    let read: Vec<String> = read_index(&root).docs.into_iter().map(|d| d.id).collect();
    assert_eq!(read, vec![good.id()], "crafted rows are dropped");
    let offered: Vec<String> = doc_recovery_candidates(&root)
        .into_iter()
        .map(|d| d.entry.id)
        .collect();
    assert_eq!(offered, vec![good.id()]);

    // Discard all through the world (the recovery window's Discard all).
    let mut app = App::new();
    let world = app.world_mut();
    install(world);
    let mut offers = RecoveryOffers::scan(&root);
    // Even a row smuggled into the offers past the reader removes nothing outside.
    for id in &crafted_ids {
        offers.docs.push(DocRecovery {
            entry: crafted_row(id, "0123456789abcdef.autosave"),
            autosave: docs.join("0123456789abcdef.autosave"),
            original: None,
        });
    }
    world.insert_resource(offers);
    let _ = discard(world, &[]);
    for target in [&up_one, &up_two, &absolute] {
        assert!(target.is_file(), "{} was deleted by Discard", target.display());
    }
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&outside);
}

/// Review sprint-rev-claude-50-k, autosave-recovery finding 2: a job that panics on the worker
/// (a provider's or the level's encoding runs there) ended the thread. The jobs queued behind it
/// were lost without an error and `in_flight` never returned to 0. The worker reports the panic
/// as an error and keeps running the queue.
#[test]
fn autosave_worker_survives_a_panicking_job() {
    // A panic under another test's PIE flag would be filed as that test's `pie` panic.
    let _guard = crate::crash_reports::tests::lock();
    let root = temp_project("worker-panic");
    let mut worker = AutosaveWorker::spawn();
    worker.send(Job::Tick {
        root: root.clone(),
        level: Some(LevelWrite {
            path: autosave::autosave_dir(&root).join("panics.level.ron"),
            encode: Box::new(|| panic!("encode exploded")),
        }),
        docs: Vec::new(),
        level_target: None,
        history: crate::version_history::HistoryPolicy::default(),
    });
    let key = DocKey::new("fake", Some(&root.join("assets/after.fake")));
    worker.send(Job::Tick {
        root: root.clone(),
        level: None,
        docs: vec![DocSnapshot {
            key: key.clone(),
            bytes: b"after".to_vec(),
        }],
        level_target: None,
        history: crate::version_history::HistoryPolicy::default(),
    });
    let done = worker.flush(FLUSH);
    assert_eq!(worker.in_flight(), 0, "every queued job reported back: {done:?}");
    assert_eq!(done.len(), 2, "{done:?}");
    assert!(
        done[0].errors.iter().any(|e| e.contains("encode exploded")),
        "the panic is reported: {done:?}"
    );
    assert_eq!(done[1].docs, vec![key.id()], "the job behind the panic still ran");
    let index = read_index(&root);
    assert_eq!(index.docs.len(), 1, "{index:?}");
    // The worker is still the one running jobs (nothing fell back to the main thread).
    worker.send(Job::Remove {
        root: root.clone(),
        ids: vec![key.id()],
    });
    assert!(worker.inline.is_empty());
    let done = worker.flush(FLUSH);
    assert_eq!(done.len(), 1);
    assert_eq!(worker.in_flight(), 0);
    assert!(read_index(&root).docs.is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

/// Review sprint-rev-claude-50-k, autosave-recovery finding 3: the job queue was unbounded and
/// every due tick queued a full snapshot, so a worker slower than the interval (a large level)
/// let jobs pile up while the session stayed dirty. A due tick waits while an earlier tick is
/// still queued or running, and then writes the latest snapshot.
#[test]
fn autosave_ticks_do_not_pile_up_behind_a_slow_worker() {
    let root = temp_project("coalesce");
    let mut app = app_with_fake(&root);
    let world = app.world_mut();
    world.resource_mut::<AutosaveState>().interval = Some(Duration::from_secs(60));
    // A slow write holds the worker until the test releases it.
    let (release, held) = std::sync::mpsc::channel::<()>();
    world.resource_mut::<AutosaveState>().worker.send(Job::Tick {
        root: root.clone(),
        level: Some(LevelWrite {
            path: autosave::autosave_dir(&root).join("slow.level.ron"),
            encode: Box::new(move || {
                let _ = held.recv();
                Err("slow write released".into())
            }),
        }),
        docs: Vec::new(),
        level_target: None,
        history: crate::version_history::HistoryPolicy::default(),
    });
    let t0 = Instant::now();
    tick_at(world, &root, t0, false);
    for step in 1..=5_u64 {
        world.resource_mut::<FakeDoc>().text = format!("edit {step}");
        let report = tick_at(world, &root, t0 + Duration::from_secs(61 * step), false);
        assert!(
            !report.queued,
            "step {step}: a tick is still in flight: {report:?}"
        );
        assert_eq!(world.resource::<AutosaveState>().worker.in_flight(), 1);
    }
    drop(release);
    flush(world, FLUSH);
    // The deferred tick is still due: the next frame writes the latest snapshot.
    let report = tick_at(world, &root, t0 + Duration::from_secs(61 * 5 + 1), false);
    assert!(report.queued, "{report:?}");
    flush(world, FLUSH);
    let row = read_index(&root).docs.into_iter().next().expect("row");
    let bytes = std::fs::read(docs_dir(&root).join(&row.file)).expect("bytes");
    assert_eq!(bytes, b"edit 5");
    let _ = std::fs::remove_dir_all(&root);
}

/// Runs one editor agent method through its installed handler (the dispatcher's table).
fn call(world: &mut World, method: &str, params: serde_json::Value) -> serde_json::Value {
    let (_, handler) = crate::agent_ext::all_methods()
        .into_iter()
        .find(|(schema, _)| schema.name == method)
        .unwrap_or_else(|| panic!("{method} is installed"));
    handler(world, &params).unwrap_or_else(|e| panic!("{method}: {}", e.message))
}

/// The ids of the document autosaves indexed under `root`.
fn indexed_ids(root: &Path) -> Vec<String> {
    read_index(root).docs.into_iter().map(|d| d.id).collect()
}

/// A saved `.seq.ron` (w6 item 1): an unedited sequence writes nothing; a moved key autosaves the
/// text a save writes; the relaunch's Restore reopens it at its original path, dirty, with
/// byte-identical text; a manual save removes the autosave.
#[test]
fn autosave_sequence_restores_byte_identical_text() {
    use regia_framework::sequencer::SequencePlayer;
    use regia_framework::sequencer::edit::KeyEdit;

    use crate::agent_ext::EditorProject;
    use crate::seq_curve_panel::{SeqCurvePanel, edit_selected, open_sequence, save_sequence};

    const SEQ: &str = r#"(version: 1, name: "intro", tracks: [
        (name: "fade", target: "ui", property: "opacity",
            keys: Float([(time: 0.0, value: 0.0, ease: Bezier), (time: 2.0, value: 1.0)])),
    ])"#;
    let root = temp_project("sequence");
    std::fs::create_dir_all(root.join("cine")).expect("dir");
    let rel = "cine/intro.seq.ron";
    std::fs::write(root.join(rel), SEQ).expect("seq");

    let mut first = App::new();
    let world = first.world_mut();
    world.insert_resource(EditorProject {
        root: Some(root.clone()),
    });
    install(world);
    let mut panel = SeqCurvePanel::default();
    let player = open_sequence(world, &mut panel, rel).expect("open");
    world.insert_resource(panel);
    // Opened and not edited: nothing to write.
    let report = tick_at(world, &root, Instant::now(), true);
    assert!(
        report.docs.is_empty(),
        "an unedited sequence writes nothing: {report:?}"
    );
    flush(world, FLUSH);
    assert!(indexed_ids(&root).is_empty());

    // Move a key: the tick writes the text a save would write.
    let mut panel = world.remove_resource::<SeqCurvePanel>().expect("panel");
    edit_selected(
        world,
        &mut panel,
        KeyEdit::Move {
            index: 1,
            time: 1.5,
            value: 0.75,
        },
    )
    .expect("move a key");
    world.insert_resource(panel);
    let expected = world
        .get::<SequencePlayer>(player)
        .expect("player")
        .sequence()
        .to_ron()
        .expect("text")
        .into_bytes();
    let report = tick_at(world, &root, Instant::now(), true);
    assert_eq!(report.docs.len(), 1, "the edited sequence autosaves: {report:?}");
    assert_eq!(flush(world, FLUSH), 1);
    let row = read_index(&root).docs.into_iter().next().expect("row");
    assert_eq!(row.kind, "sequence");
    assert_eq!(row.original.as_deref(), Some(rel));
    assert_eq!(
        std::fs::read(docs_dir(&root).join(&row.file)).expect("bytes"),
        expected
    );
    drop(first);

    // The relaunch: Restore opens it at its original path, dirty, byte-identical.
    let mut app = App::new();
    let world = app.world_mut();
    world.insert_resource(EditorProject {
        root: Some(root.clone()),
    });
    install(world);
    let offers = RecoveryOffers::scan(&root);
    assert_eq!(offers.docs.len(), 1, "{offers:?}");
    world.insert_resource(offers);
    let outcome = restore(world, &[]);
    assert!(outcome.failed.is_empty(), "{outcome:?}");
    assert_eq!(outcome.done.len(), 1, "{outcome:?}");
    let panel = world.resource::<SeqCurvePanel>();
    let (&restored, path) = panel.paths.iter().next().expect("a sequence is open");
    assert_eq!(path, rel, "at its original path");
    assert_eq!(
        world
            .get::<SequencePlayer>(restored)
            .expect("player")
            .sequence()
            .to_ron()
            .expect("text")
            .into_bytes(),
        expected,
        "byte-identical text"
    );
    assert_eq!(dirty_documents(world).len(), 1, "dirty until a save");
    let state = call(world, "editor.seq_curve_state", serde_json::json!({}));
    assert_eq!(state["path"], rel, "{state}");
    assert_eq!(state["dirty"], true, "{state}");

    // A manual save: the next frame removes the autosave.
    let mut panel = world.remove_resource::<SeqCurvePanel>().expect("panel");
    save_sequence(world, &mut panel, None).expect("save");
    world.insert_resource(panel);
    assert!(dirty_documents(world).is_empty(), "clean after a save");
    let state = call(world, "editor.seq_curve_state", serde_json::json!({}));
    assert_eq!(state["dirty"], false, "{state}");
    tick_at(world, &root, Instant::now(), false);
    flush(world, FLUSH);
    assert!(indexed_ids(&root).is_empty(), "the save removed the autosave");
    assert_eq!(
        std::fs::read(root.join(rel)).expect("saved"),
        expected,
        "the save wrote the recovered text"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A saved `.shots.ron` (w7 item 3): an unedited shot list writes nothing; a timeline edit
/// autosaves the text a save writes; the relaunch's Restore puts it back at its original path,
/// dirty, with byte-identical text; a manual save removes the autosave.
#[test]
fn autosave_shot_list_restores_byte_identical_text() {
    use regia_framework::shots::ShotPlayer;

    use crate::agent_ext::EditorProject;

    let root = temp_project("shots");
    let rel = "assets/cine/intro.shots.ron";
    crate::content::create_asset(&root, crate::content::NewAssetKind::ShotList, rel).expect("shot list");
    let on_disk = std::fs::read(root.join(rel)).expect("file");

    let mut first = App::new();
    let world = first.world_mut();
    world.insert_resource(EditorProject {
        root: Some(root.clone()),
    });
    world.insert_resource(ShotPlayer::default());
    install(world);
    call(
        world,
        "editor.shot_panel_open",
        serde_json::json!({ "path": rel }),
    );
    let report = tick_at(world, &root, Instant::now(), true);
    assert!(
        report.docs.is_empty(),
        "an unedited shot list writes nothing: {report:?}"
    );
    flush(world, FLUSH);
    assert!(indexed_ids(&root).is_empty());

    // A fade from the timeline: the tick writes the text a save would write.
    call(
        world,
        "editor.seq_cut_edit",
        serde_json::json!({ "op": "fade", "shot": 0, "fade_in": 0.5 }),
    );
    let expected = world
        .resource::<ShotPlayer>()
        .sequence()
        .expect("loaded")
        .to_ron()
        .expect("text")
        .into_bytes();
    assert_ne!(expected, on_disk, "the fade changed the list");
    let report = tick_at(world, &root, Instant::now(), true);
    assert_eq!(report.docs.len(), 1, "the edited shot list autosaves: {report:?}");
    assert_eq!(flush(world, FLUSH), 1);
    let row = read_index(&root).docs.into_iter().next().expect("row");
    assert_eq!(row.kind, "shots");
    assert_eq!(row.original.as_deref(), Some(rel));
    assert_eq!(
        std::fs::read(docs_dir(&root).join(&row.file)).expect("bytes"),
        expected
    );
    drop(first);

    // The relaunch: Restore loads it at its original path, dirty, byte-identical.
    let mut app = App::new();
    let world = app.world_mut();
    world.insert_resource(EditorProject {
        root: Some(root.clone()),
    });
    install(world);
    let offers = RecoveryOffers::scan(&root);
    assert_eq!(offers.docs.len(), 1, "{offers:?}");
    world.insert_resource(offers);
    let outcome = restore(world, &[]);
    assert!(outcome.failed.is_empty(), "{outcome:?}");
    assert_eq!(outcome.done.len(), 1, "{outcome:?}");
    assert_eq!(
        world
            .resource::<ShotPlayer>()
            .sequence()
            .expect("restored")
            .to_ron()
            .expect("text")
            .into_bytes(),
        expected,
        "byte-identical text"
    );
    let state = call(world, "editor.shot_panel_state", serde_json::json!({}));
    assert_eq!(state["path"], rel, "at its original path: {state}");
    assert_eq!(state["dirty"], true, "dirty until a save: {state}");
    assert_eq!(dirty_documents(world).len(), 1);

    // A manual save: the next tick removes the autosave.
    call(world, "editor.shot_panel_save", serde_json::json!({}));
    assert!(dirty_documents(world).is_empty(), "clean after a save");
    tick_at(world, &root, Instant::now(), false);
    flush(world, FLUSH);
    assert!(indexed_ids(&root).is_empty(), "the save removed the autosave");
    assert_eq!(std::fs::read(root.join(rel)).expect("saved"), expected);
    let _ = std::fs::remove_dir_all(&root);
}

/// A world with the scene plugin, an open project and an empty open level.
fn scene_app(root: &Path) -> App {
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    let world = app.world_mut();
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(root.to_path_buf()),
    });
    let open = crate::level_io::new_level(world);
    world.insert_resource(open);
    world.insert_resource(crate::command::UndoStack::new());
    world.insert_resource(crate::prefab_edit::PrefabEditMode::default());
    install(world);
    app
}

/// A prefab file: one root with a child.
fn write_prefab(root: &Path) -> PathBuf {
    let path = root.join("assets/materials/crate.prefab.ron");
    let text = r#"( format_version: 1, entities: [
        ( guid: "8b6f2a11-0000-4000-8000-000000000001", name: "Root",
          components: { "regia_scene::transform::Transform": () } ),
        ( guid: "8b6f2a11-0000-4000-8000-000000000002", name: "Child", parent: Some("8b6f2a11-0000-4000-8000-000000000001"),
          components: { "regia_scene::transform::Transform": ( translation: (0.0, 1.0, 0.0) ) } ),
    ] )"#;
    std::fs::write(&path, text).expect("prefab");
    path
}

/// The entity named `name` (scene `Name`).
fn named(world: &mut World, name: &str) -> Entity {
    let mut query = world.query::<(Entity, &regia_scene::prelude::Name)>();
    query
        .iter(world)
        .find(|(_, n)| n.as_str() == name)
        .map(|(e, _)| e)
        .unwrap_or_else(|| panic!("no entity named {name}"))
}

/// Prefab edit mode (w6 item 2): an unedited prefab writes nothing; an added child autosaves the
/// prefab text a save would write; the relaunch's Restore re-enters prefab edit on that path with
/// the edit, dirty, and saving it writes the pre-crash edit byte for byte; `editor.close_prefab
/// {save: true}` removes the autosave.
#[test]
fn autosave_prefab_restores_the_edit_and_close_removes_it() {
    use crate::command::{EditorCommand as _, SpawnEntities};
    use crate::prefab_edit::{PrefabEditMode, enter_prefab_edit, save_prefab};

    let root = temp_project("prefab");
    let path = write_prefab(&root);
    let original = std::fs::read(&path).expect("original");

    let mut first = scene_app(&root);
    let world = first.world_mut();
    enter_prefab_edit(world, &path).expect("enter");
    let report = tick_at(world, &root, Instant::now(), true);
    assert!(
        report.docs.is_empty(),
        "an unedited prefab writes nothing: {report:?}"
    );
    assert!(
        report.level.is_none(),
        "prefab mode never autosaves the scratch level: {report:?}"
    );
    flush(world, FLUSH);

    // Add a child under the root (the outliner's path: a command on the undo stack).
    let parent = named(world, "Root");
    let mut spawn = SpawnEntities::new_entity(
        world,
        regia_core::id::Guid::new_random(),
        "Added",
        regia_scene::prelude::Transform::default(),
    )
    .expect("spawn");
    spawn.apply(world);
    let added = named(world, "Added");
    world.entity_mut(added).insert(ChildOf(parent));
    world
        .resource_mut::<crate::command::UndoStack>()
        .push(Box::new(spawn));
    let report = tick_at(world, &root, Instant::now(), true);
    assert_eq!(report.docs.len(), 1, "the edited prefab autosaves: {report:?}");
    assert!(report.level.is_none(), "{report:?}");
    assert_eq!(flush(world, FLUSH), 1);
    let row = read_index(&root).docs.into_iter().next().expect("row");
    assert_eq!(row.kind, "prefab");
    assert_eq!(row.original.as_deref(), Some("assets/materials/crate.prefab.ron"));
    let autosaved = std::fs::read(docs_dir(&root).join(&row.file)).expect("bytes");
    // What a save in the crashed session would have written (then the crash: the file on disk
    // is the original again).
    save_prefab(world).expect("save");
    let expected = std::fs::read(&path).expect("expected");
    assert_ne!(expected, original, "the edit changes the file");
    assert_eq!(autosaved, expected, "the autosave is the text a save writes");
    std::fs::write(&path, &original).expect("crash before the save");
    drop(first);

    // The relaunch: Restore re-enters prefab edit on that path with the edit, dirty.
    let mut app = scene_app(&root);
    let world = app.world_mut();
    let offers = RecoveryOffers::scan(&root);
    assert_eq!(offers.docs.len(), 1, "{offers:?}");
    world.insert_resource(offers);
    let outcome = restore(world, &[]);
    assert!(outcome.failed.is_empty(), "{outcome:?}");
    assert_eq!(
        world.resource::<PrefabEditMode>().editing(),
        Some(path.as_path()),
        "prefab edit on its original path"
    );
    let _ = named(world, "Added");
    assert_eq!(dirty_documents(world).len(), 1, "dirty until a save");
    let mode = call(world, "editor.prefab_mode", serde_json::json!({}));
    assert_eq!(mode["dirty"], true, "{mode}");

    // Close with save: the file is the pre-crash edit, byte for byte, and the autosave goes.
    let close = crate::agent_ext::all_methods()
        .into_iter()
        .find(|(schema, _)| schema.name == "editor.close_prefab")
        .expect("installed")
        .1;
    close(world, &serde_json::json!({ "save": true })).expect("close with save");
    let mode = call(world, "editor.prefab_mode", serde_json::json!({}));
    assert_eq!(mode["dirty"], false, "{mode}");
    assert_eq!(std::fs::read(&path).expect("saved"), expected);
    assert!(world.resource::<PrefabEditMode>().editing().is_none());
    tick_at(world, &root, Instant::now(), false);
    flush(world, FLUSH);
    assert!(
        indexed_ids(&root).is_empty(),
        "closing with a save removed the autosave"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Runtime spawns (w6 item 4): an entity `scene.spawn_primitive` made (an ECS `Name`, a
/// transform, no guid, no level) is in the level autosave with no shell frame before the tick.
#[test]
fn autosave_level_includes_runtime_spawns() {
    let root = temp_project("runtime-spawn");
    let mut app = scene_app(&root);
    let world = app.world_mut();
    world.spawn((
        regia_ecs::ecs::name::Name::new("Sphere"),
        regia_scene::prelude::Transform::default(),
    ));
    world.resource_mut::<crate::command::UndoStack>().mark_dirty();
    let report = tick_at(world, &root, Instant::now(), true);
    let level = report.level.clone().expect("the dirty level autosaves");
    flush(world, FLUSH);
    let text = std::fs::read_to_string(&level).expect("level autosave");
    assert!(
        text.contains("Sphere"),
        "the runtime spawn is in the autosave: {text}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// While a prefab is open the level autosave keeps the stashed level when it had unsaved edits
/// (never the mode's scratch level or the prefab's contents), and closing the prefab gives the
/// level its dirty flag back.
#[test]
fn autosave_in_prefab_mode_keeps_the_stashed_level() {
    use crate::command::{EditorCommand as _, SpawnEntities, UndoStack};
    use crate::prefab_edit::{enter_prefab_edit, exit_prefab_edit};

    let root = temp_project("prefab-level");
    let path = write_prefab(&root);
    let mut app = scene_app(&root);
    let world = app.world_mut();
    let mut spawn = SpawnEntities::new_entity(
        world,
        regia_core::id::Guid::new_random(),
        "LevelRock",
        regia_scene::prelude::Transform::default(),
    )
    .expect("spawn");
    spawn.apply(world);
    world.resource_mut::<UndoStack>().push(Box::new(spawn));
    enter_prefab_edit(world, &path).expect("enter");
    let report = tick_at(world, &root, Instant::now(), true);
    let level = report.level.clone().expect("the dirty stashed level autosaves");
    assert!(
        report.docs.is_empty(),
        "the prefab itself is unedited: {report:?}"
    );
    flush(world, FLUSH);
    let text = std::fs::read_to_string(&level).expect("level autosave");
    assert!(text.contains("LevelRock"), "the stashed level: {text}");
    assert!(!text.contains("Root"), "not the prefab's contents: {text}");
    exit_prefab_edit(world, false).expect("exit");
    assert!(
        world.resource::<UndoStack>().is_dirty(),
        "the level's unsaved edits are dirty again"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A sequence the Sequence Curves panel opened is a document, not level or prefab content: it
/// stays out of the prefab's contents and the stash, lives through a prefab session as the same
/// player, and is not adopted into the level by a save.
#[test]
fn sequence_documents_live_through_prefab_mode_and_level_saves() {
    use regia_framework::sequencer::SequencePlayer;

    use crate::prefab_edit::{enter_prefab_edit, exit_prefab_edit, save_prefab};
    use crate::seq_curve_panel::{SeqCurvePanel, open_sequence};

    let root = temp_project("seq-prefab");
    std::fs::create_dir_all(root.join("cine")).expect("dir");
    std::fs::write(
        root.join("cine/intro.seq.ron"),
        "(version: 2, markers: [(name: \"m\", time: 0.5)])",
    )
    .expect("seq");
    let path = write_prefab(&root);
    let mut app = scene_app(&root);
    let world = app.world_mut();
    let mut panel = SeqCurvePanel::default();
    let player = open_sequence(world, &mut panel, "cine/intro.seq.ron").expect("open");
    world.insert_resource(panel);
    // A frame's identity pass gives it a guid, as the shell's would.
    crate::visibility::ensure_scene_identity(world);

    enter_prefab_edit(world, &path).expect("enter");
    assert!(
        world.get::<SequencePlayer>(player).is_some(),
        "kept through entry"
    );
    save_prefab(world).expect("one root: the sequence is not part of the prefab");
    let saved = std::fs::read_to_string(&path).expect("prefab");
    assert!(!saved.contains("sequence:"), "{saved}");
    exit_prefab_edit(world, false).expect("exit");
    assert!(
        world.get::<SequencePlayer>(player).is_some(),
        "the same player after the close"
    );

    let open = world.resource::<crate::level_io::OpenLevel>().clone();
    let level_path = root.join("assets/l.level.ron");
    crate::level_io::save_level_file(world, &open, &level_path).expect("save");
    let text = std::fs::read_to_string(&level_path).expect("level");
    assert!(!text.contains("sequence:"), "not adopted into the level: {text}");
    let _ = std::fs::remove_dir_all(&root);
}

/// Lane w7-editor-smalls-z (6): while a prefab is open (a dirty level stashed), restoring a level
/// recovery row is refused (`editor.recovery_restore` answers `prefab_open`; `apply_restore`
/// itself returns the palette's wording) and changes neither the world, the stash nor the offer;
/// after the prefab closes the same restore works.
#[test]
fn level_restore_is_refused_while_a_prefab_is_open() {
    use crate::command::{EditorCommand as _, SpawnEntities, UndoStack};
    use crate::prefab_edit::{PrefabEditMode, enter_prefab_edit, exit_prefab_edit};

    let root = temp_project("prefab-level-restore");
    let path = write_prefab(&root);
    let spawn_named = |world: &mut World, name: &str| {
        let mut spawn = SpawnEntities::new_entity(
            world,
            regia_core::id::Guid::new_random(),
            name,
            regia_scene::prelude::Transform::default(),
        )
        .expect("spawn");
        spawn.apply(world);
        world.resource_mut::<UndoStack>().push(Box::new(spawn));
    };
    let has = |world: &mut World, name: &str| {
        let mut query = world.query::<&regia_scene::prelude::Name>();
        query.iter(world).any(|n| n.as_str() == name)
    };
    // The crashed session: a dirty level autosaved.
    let mut first = scene_app(&root);
    let world = first.world_mut();
    spawn_named(world, "Crashed");
    let report = tick_at(world, &root, Instant::now(), true);
    assert!(report.level.is_some(), "{report:?}");
    flush(world, FLUSH);
    drop(first);

    // The relaunch: a dirty level of its own, stashed by a prefab session.
    let mut app = scene_app(&root);
    let world = app.world_mut();
    spawn_named(world, "Stashed");
    world.insert_resource(RecoveryOffers::scan(&root));
    let level_id = world.resource::<RecoveryOffers>().rows()[0].id.clone();
    assert!(level_id.starts_with("level:"), "{level_id}");
    enter_prefab_edit(world, &path).expect("enter");
    let candidate = world.resource::<RecoveryOffers>().levels[0].clone();
    let err = crate::autosave::apply_restore(world, &candidate).expect_err("prefab open");
    assert_eq!(
        err,
        "Close the prefab editor first (editor.close_prefab) to restore an autosave"
    );
    let (_, handler) = crate::agent_ext::all_methods()
        .into_iter()
        .find(|(schema, _)| schema.name == "editor.recovery_restore")
        .expect("installed");
    let err = handler(world, &serde_json::json!({ "ids": [level_id] })).expect_err("refused");
    let code = err
        .data
        .as_ref()
        .and_then(|d| d.get("code"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    assert_eq!(code, "prefab_open", "{err:?}");
    let err = handler(world, &serde_json::json!({})).expect_err("every row includes the level");
    assert!(format!("{err:?}").contains("prefab_open"), "{err:?}");
    assert_eq!(
        world.resource::<PrefabEditMode>().editing(),
        Some(path.as_path()),
        "still editing the prefab"
    );
    assert!(has(world, "Root"), "the prefab's contents are untouched");
    assert!(
        !has(world, "Crashed"),
        "the autosave did not open beside the prefab"
    );
    assert!(!has(world, "Stashed"), "the stashed level stays stashed");
    assert_eq!(
        world.resource::<RecoveryOffers>().rows()[0].id,
        level_id,
        "the offer stays"
    );

    exit_prefab_edit(world, false).expect("exit");
    assert!(has(world, "Stashed"), "the stash comes back unchanged");
    assert!(!has(world, "Root"));
    let out = handler(world, &serde_json::json!({ "ids": [level_id] })).expect("restore");
    assert_eq!(out["restored"], serde_json::json!([level_id]), "{out}");
    assert!(has(world, "Crashed"), "the autosave is the open level");
    let _ = std::fs::remove_dir_all(&root);
}

/// Documents a test editor keeps behind its open one (P-322 part b).
#[derive(Resource, Default, Clone)]
struct FakeRetained(Vec<(PathBuf, String)>);

fn fake_retained_dirty(world: &World) -> Vec<DocKey> {
    world
        .get_resource::<FakeRetained>()
        .map(|r| r.0.iter().map(|(p, _)| DocKey::new("fake", Some(p))).collect())
        .unwrap_or_default()
}

fn fake_retained_snapshot(world: &World) -> Vec<DocSnapshot> {
    world
        .get_resource::<FakeRetained>()
        .map(|r| {
            r.0.iter()
                .map(|(p, text)| DocSnapshot {
                    key: DocKey::new("fake", Some(p)),
                    bytes: text.clone().into_bytes(),
                })
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn retained_documents_are_listed_and_written_once_beside_the_open_ones() {
    let root = temp_project("retained");
    let mut app = app_with_fake(&root);
    let world = app.world_mut();
    world
        .resource_mut::<DocAutosaveRegistry>()
        .register_retained(RetainedProvider {
            kind: "fake",
            dirty: fake_retained_dirty,
            snapshot: fake_retained_snapshot,
        });
    let open = root.join("assets/materials/a.fake");
    let behind = root.join("assets/materials/b.fake");
    // `a` is retained too (an older copy): the open document's key wins.
    world.insert_resource(FakeRetained(vec![
        (open.clone(), "stale".into()),
        (behind.clone(), "behind".into()),
    ]));
    let keys = dirty_documents(world);
    assert_eq!(
        keys,
        vec![
            DocKey::new("fake", Some(&open)),
            DocKey::new("fake", Some(&behind))
        ]
    );
    let snapshots = take_snapshots(world);
    assert_eq!(snapshots.len(), 2);
    assert_eq!(
        snapshots[0].bytes, b"edited",
        "the open document's bytes, not the stale copy"
    );
    assert_eq!(snapshots[1].bytes, b"behind");
    let report = tick(
        world,
        TickInput {
            project_root: &root,
            now: Instant::now(),
            force: true,
        },
    );
    assert_eq!(report.docs.len(), 2);
    flush(world, FLUSH);
    let index = read_index(&root);
    let behind_id = DocKey::new("fake", Some(&behind)).id();
    assert!(
        index.docs.iter().any(|e| e.id == behind_id),
        "the retained document is on disk"
    );
    let _ = std::fs::remove_dir_all(root);
}
