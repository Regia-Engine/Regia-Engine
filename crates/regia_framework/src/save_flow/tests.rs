// SPDX-License-Identifier: MPL-2.0
//! Spec 91 § 6 save-flow tests: storages, requests, quicksave, blobs, slot info.

use std::sync::Mutex;

use regia_core::math::Vec3;
use regia_scene::Transform;

use super::*;
use crate::save_game::{SavePlugin, Saveable};

/// A saved component.
#[derive(Component, Reflect, Default, Clone, Debug, PartialEq)]
#[reflect(Component, Default)]
struct Coins(u32);

/// A saved resource.
#[derive(Resource, Reflect, Default, Clone, Debug, PartialEq)]
#[reflect(Resource, Default)]
struct Chapter(u32);

impl SaveableResource for Chapter {}

/// An in-memory storage: proves the flow hands a storage exactly the `encode` bytes.
#[derive(Default)]
struct MemoryStorage {
    slots: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl SaveStorage for Arc<MemoryStorage> {
    fn kind(&self) -> &'static str {
        "memory"
    }
    fn write(&self, slot: &str, bytes: &[u8]) -> Result<(), SaveError> {
        self.slots
            .lock()
            .map_err(|_| SaveError::Serialize("poisoned".into()))?
            .insert(slot.to_owned(), bytes.to_vec());
        Ok(())
    }
    fn read(&self, slot: &str) -> Result<Option<Vec<u8>>, SaveError> {
        Ok(self
            .slots
            .lock()
            .map_err(|_| SaveError::Deserialize("poisoned".into()))?
            .get(slot)
            .cloned())
    }
    fn delete(&self, slot: &str) -> Result<bool, SaveError> {
        Ok(self
            .slots
            .lock()
            .map_err(|_| SaveError::Serialize("poisoned".into()))?
            .remove(slot)
            .is_some())
    }
    fn list(&self) -> Result<Vec<String>, SaveError> {
        Ok(self
            .slots
            .lock()
            .map_err(|_| SaveError::Deserialize("poisoned".into()))?
            .keys()
            .cloned()
            .collect())
    }
}

/// A headless app with the save flow, a temp save directory and fixed 0.1 s frames.
fn app(dir: &std::path::Path) -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, SavePlugin::default(), SaveFlowPlugin))
        .register_type::<Coins>()
        .register_type::<Transform>()
        .register_saveable_resource::<Chapter>()
        .insert_resource(SaveDirectory(dir.to_path_buf()))
        .insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_millis(100),
        ));
    app
}

fn coins(app: &mut App) -> Vec<u32> {
    let world = app.world_mut();
    let mut values: Vec<u32> = world.query::<&Coins>().iter(world).map(|c| c.0).collect();
    values.sort_unstable();
    values
}

fn outcomes(app: &mut App) -> Vec<SaveOutcome> {
    let messages = app.world().resource::<Messages<SaveOutcome>>();
    let mut cursor = messages.get_cursor();
    cursor.read(messages).cloned().collect()
}

#[test]
fn file_storage_writes_reads_lists_and_deletes() {
    let dir = regia_test_utils::unique_temp_dir("save-flow-file");
    let files = FileSaveStorage::new(&dir);
    assert_eq!(files.kind(), "file");
    assert_eq!(files.read("A").ok().flatten(), None, "missing slot reads as None");
    assert!(files.list().is_ok_and(|names| names.is_empty()));
    files.write("B", b"bb").expect("write B");
    files.write("A", b"aa").expect("write A");
    assert_eq!(files.read("A").ok().flatten(), Some(b"aa".to_vec()));
    assert_eq!(
        files.list().ok(),
        Some(vec!["A".to_owned(), "B".to_owned()]),
        "sorted"
    );
    assert!(dir.join("A.save").is_file(), "the SaveGame::slot path");
    assert_eq!(files.delete("A").ok(), Some(true));
    assert_eq!(
        files.delete("A").ok(),
        Some(false),
        "deleting a missing slot is not an error"
    );
    let missing = FileSaveStorage::new(dir.join("never-created"));
    assert!(
        missing.list().is_ok_and(|names| names.is_empty()),
        "a missing dir lists nothing"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sanitize_matches_the_file_slot_rule() {
    assert_eq!(sanitize_slot_name("Chapter 1/../x"), "Chapter_1____x");
    assert_eq!(sanitize_slot_name("ok-name_2"), "ok-name_2");
    let dir = SaveDirectory(std::path::PathBuf::from("/nowhere"));
    assert_eq!(
        SaveGame::slot(&dir, "Chapter 1").name,
        sanitize_slot_name("Chapter 1")
    );
}

#[test]
fn save_and_load_through_requests_restore_the_world() {
    let dir = regia_test_utils::unique_temp_dir("save-flow-requests");
    let mut app = app(&dir);
    app.world_mut().spawn((
        Saveable,
        Coins(3),
        Transform::from_translation(Vec3::new(1.0, 2.0, 3.0)),
    ));
    app.insert_resource(Chapter(2));
    app.world_mut().write_message(SaveRequest::Save {
        slot: "Slot 1".into(),
    });
    app.update();
    let saved = outcomes(&mut app);
    assert_eq!(saved.len(), 1, "one outcome per request");
    assert_eq!(saved[0].op, SaveOp::Save);
    assert_eq!(saved[0].slot, "Slot_1", "the outcome names the sanitized slot");
    let result = saved[0].result.clone().expect("save works");
    assert_eq!(result.entity_count, 1);
    assert!(dir.join("Slot_1.save").is_file());

    // Change the world, then load in the next frame.
    let entities: Vec<Entity> = app
        .world_mut()
        .query_filtered::<Entity, With<Coins>>()
        .iter(app.world())
        .collect();
    for entity in entities {
        app.world_mut().despawn(entity);
    }
    app.world_mut().spawn((Saveable, Coins(99)));
    app.insert_resource(Chapter(7));
    app.world_mut().write_message(SaveRequest::Load {
        slot: "Slot 1".into(),
    });
    app.update();
    assert_eq!(
        coins(&mut app),
        vec![3],
        "the saved entity replaced the live ones"
    );
    assert_eq!(app.world().resource::<Chapter>(), &Chapter(2));
    let world = app.world_mut();
    let translation = world
        .query::<(&Coins, &Transform)>()
        .iter(world)
        .map(|(_, t)| t.translation)
        .next();
    assert_eq!(translation, Some(Vec3::new(1.0, 2.0, 3.0)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn requests_of_one_frame_run_in_arrival_order() {
    let dir = regia_test_utils::unique_temp_dir("save-flow-order");
    let mut app = app(&dir);
    app.insert_resource(Chapter(1));
    app.world_mut()
        .write_message(SaveRequest::Save { slot: "S".into() });
    app.world_mut()
        .write_message(SaveRequest::Delete { slot: "S".into() });
    app.world_mut()
        .write_message(SaveRequest::Load { slot: "S".into() });
    app.update();
    let all = outcomes(&mut app);
    let ops: Vec<SaveOp> = all.iter().map(|o| o.op).collect();
    assert_eq!(ops, [SaveOp::Save, SaveOp::Delete, SaveOp::Load]);
    assert!(all[0].result.is_ok());
    assert_eq!(
        all[1].result.as_ref().map(|r| r.bytes_written).ok(),
        Some(1),
        "deleted"
    );
    assert!(all[2].result.is_err(), "the slot was deleted before the load");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn quick_save_and_quick_load_use_the_quick_slot() {
    let dir = regia_test_utils::unique_temp_dir("save-flow-quick");
    let mut app = app(&dir);
    app.insert_resource(QuickSaveConfig {
        slot: "Q".into(),
        ..QuickSaveConfig::default()
    });
    app.insert_resource(Chapter(5));
    app.world_mut().write_message(SaveRequest::QuickSave);
    app.update();
    assert!(dir.join("Q.save").is_file(), "the configured quick slot");
    app.insert_resource(Chapter(0));
    app.world_mut().write_message(SaveRequest::QuickLoad);
    app.update();
    assert_eq!(app.world().resource::<Chapter>(), &Chapter(5));
    let last = outcomes(&mut app);
    assert!(last.iter().all(|o| o.quick && o.slot == "Q"), "{last:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn quick_actions_send_requests() {
    use regia_input::prelude::*;
    let dir = regia_test_utils::unique_temp_dir("save-flow-actions");
    let mut app = app(&dir);
    let config = InputConfig::new()
        .action("QuickSave", ActionKind::Button)
        .action("QuickLoad", ActionKind::Button)
        .binding(Binding::new("QuickSave", InputSource::key("F5")))
        .binding(Binding::new("QuickLoad", InputSource::key("F9")));
    app.add_plugins(InputPlugin::new(config));
    app.insert_resource(Chapter(4));
    app.update();
    assert!(!dir.join("Quick.save").exists(), "no press, no save");
    // The action fires on the next frame, through the same pipeline a key press feeds.
    let frame = app.world().resource::<Time>().frame_count() + 1;
    app.world_mut()
        .resource_mut::<regia_input::InputInjection>()
        .schedule("QuickSave".to_string(), Vec3::X, frame, 1);
    app.update();
    assert!(
        dir.join("Quick.save").is_file(),
        "the QuickSave action quicksaved"
    );
    let quick = outcomes(&mut app);
    assert!(quick.iter().any(|o| o.quick && o.op == SaveOp::Save), "{quick:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_custom_store_receives_the_encode_container_and_lists_headers() {
    let dir = regia_test_utils::unique_temp_dir("save-flow-memory");
    let mut app = app(&dir);
    let memory = Arc::new(MemoryStorage::default());
    app.insert_resource(SaveStore::new(memory.clone()));
    app.insert_resource(Chapter(3));
    let result = save_to_store(app.world_mut(), "Cloud").expect("save to memory");
    let bytes = memory.read("Cloud").ok().flatten().expect("stored");
    assert_eq!(bytes.len(), result.bytes_written);
    assert_eq!(
        &bytes[..4],
        crate::save_game::SAVE_MAGIC,
        "the file container, unchanged"
    );
    assert!(!dir.join("Cloud.save").exists(), "nothing went to disk");
    let listing = list_store(app.world()).expect("list");
    assert_eq!(listing.len(), 1);
    assert_eq!(listing[0].name, "Cloud");
    assert_eq!(
        listing[0].header.as_ref().map(|h| h.version),
        Some(crate::save_game::SAVE_FORMAT_VERSION)
    );
    app.insert_resource(Chapter(0));
    load_from_store(app.world_mut(), "Cloud").expect("load from memory");
    assert_eq!(app.world().resource::<Chapter>(), &Chapter(3));
    assert_eq!(
        format!("{:?}", app.world().resource::<SaveStore>()),
        "SaveStore(\"memory\")"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A memory storage whose writes fail while `fail` is set (a full disk, a cloud refusal).
#[derive(Default)]
struct FlakyStorage {
    inner: Arc<MemoryStorage>,
    fail: std::sync::atomic::AtomicBool,
}

impl SaveStorage for Arc<FlakyStorage> {
    fn kind(&self) -> &'static str {
        "flaky"
    }
    fn write(&self, slot: &str, bytes: &[u8]) -> Result<(), SaveError> {
        if self.fail.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(SaveError::Io(
                std::io::Error::other("disk full"),
                PathBuf::from(slot),
            ));
        }
        self.inner.write(slot, bytes)
    }
    fn read(&self, slot: &str) -> Result<Option<Vec<u8>>, SaveError> {
        self.inner.read(slot)
    }
    fn delete(&self, slot: &str) -> Result<bool, SaveError> {
        self.inner.delete(slot)
    }
    fn list(&self) -> Result<Vec<String>, SaveError> {
        self.inner.list()
    }
}

/// A save whose write fails leaves [`SaveInfo::save_count`] at the count of the last file
/// written, so the next good save writes the next count (audit framework5 #6).
#[test]
fn a_failed_save_does_not_advance_the_save_count() {
    let dir = regia_test_utils::unique_temp_dir("save-flow-failed-count");
    let mut app = app(&dir);
    let storage = Arc::new(FlakyStorage::default());
    app.insert_resource(SaveStore::new(storage.clone()));
    app.update();
    save_to_store(app.world_mut(), "A").expect("save 1");
    assert_eq!(app.world().resource::<SaveInfo>().save_count, 1);
    storage.fail.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(save_to_store(app.world_mut(), "A").is_err());
    assert_eq!(
        app.world().resource::<SaveInfo>().save_count,
        1,
        "the failed save wrote nothing"
    );
    storage.fail.store(false, std::sync::atomic::Ordering::Relaxed);
    save_to_store(app.world_mut(), "A").expect("save 2");
    assert_eq!(app.world().resource::<SaveInfo>().save_count, 2);
    app.insert_resource(SaveInfo::default());
    load_from_store(app.world_mut(), "A").expect("load");
    assert_eq!(app.world().resource::<SaveInfo>().save_count, 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn save_info_counts_saves_and_play_time_survives_the_round_trip() {
    let dir = regia_test_utils::unique_temp_dir("save-flow-info");
    let mut app = app(&dir);
    app.world_mut().resource_mut::<SaveInfo>().display_name = "Harbor".into();
    app.update();
    let start = app.world().resource::<SaveInfo>().play_time_secs;
    for _ in 0..4 {
        app.update();
    }
    let played = app.world().resource::<SaveInfo>().play_time_secs - start;
    assert!((played - 0.4).abs() < 1e-5, "four 0.1 s frames: {played}");
    let played = app.world().resource::<SaveInfo>().play_time_secs;
    save_to_store(app.world_mut(), "A").expect("save 1");
    save_to_store(app.world_mut(), "A").expect("save 2");
    assert_eq!(app.world().resource::<SaveInfo>().save_count, 2);
    app.insert_resource(SaveInfo::default());
    load_from_store(app.world_mut(), "A").expect("load");
    let info = app.world().resource::<SaveInfo>().clone();
    assert_eq!(info.save_count, 2, "the file holds the count it was written as");
    assert_eq!(info.display_name, "Harbor");
    assert_eq!(info.play_time_secs, played, "play time restored");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn blobs_round_trip_typed_and_raw() {
    #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
    struct Quest {
        stage: u8,
        flags: Vec<String>,
    }
    let dir = regia_test_utils::unique_temp_dir("save-flow-blobs");
    let mut app = app(&dir);
    {
        let mut blobs = app.world_mut().resource_mut::<SaveBlobs>();
        blobs
            .set_value(
                "quest",
                &Quest {
                    stage: 3,
                    flags: vec!["met_guide".into()],
                },
            )
            .expect("encode");
        blobs.set("raw", vec![1, 2, 3]);
    }
    save_to_store(app.world_mut(), "B").expect("save");
    app.insert_resource(SaveBlobs::default());
    load_from_store(app.world_mut(), "B").expect("load");
    let blobs = app.world().resource::<SaveBlobs>();
    assert_eq!(blobs.keys().collect::<Vec<_>>(), ["quest", "raw"], "sorted keys");
    assert_eq!(blobs.get("raw"), Some(&[1u8, 2, 3][..]));
    let quest: Quest = blobs.get_value("quest").expect("present").expect("decodes");
    assert_eq!(quest.stage, 3);
    assert!(blobs.get_value::<Quest>("absent").is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bad_requests_answer_with_errors_not_panics() {
    let dir = regia_test_utils::unique_temp_dir("save-flow-errors");
    let mut app = app(&dir);
    app.world_mut()
        .write_message(SaveRequest::Save { slot: String::new() });
    app.world_mut()
        .write_message(SaveRequest::Load { slot: "Nope".into() });
    app.world_mut()
        .write_message(SaveRequest::Delete { slot: "Nope".into() });
    app.update();
    let all = outcomes(&mut app);
    assert_eq!(all.len(), 3);
    assert!(
        all[0].result.as_ref().is_err_and(|e| e.contains("empty")),
        "{:?}",
        all[0]
    );
    assert!(
        all[1].result.as_ref().is_err_and(|e| e.contains("no save slot")),
        "{:?}",
        all[1]
    );
    assert_eq!(
        all[2].result.as_ref().map(|r| r.bytes_written).ok(),
        Some(0),
        "nothing deleted"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_file_without_the_new_resources_loads_and_keeps_them() {
    // A v1 file written by an app with no save flow (no SaveInfo / SaveBlobs entries).
    let dir = regia_test_utils::unique_temp_dir("save-flow-v1");
    let mut old = App::new();
    old.add_plugins((MinimalPlugins, SavePlugin::default()))
        .register_saveable_resource::<Chapter>()
        .insert_resource(Chapter(8));
    let bytes = SaveGame::encode(old.world_mut(), &SaveSpec::default()).expect("encode");
    std::fs::write(dir.join("Old.save"), &bytes).expect("write");

    let mut app = app(&dir);
    app.world_mut().resource_mut::<SaveInfo>().display_name = "Current".into();
    load_from_store(app.world_mut(), "Old").expect("old file loads");
    assert_eq!(app.world().resource::<Chapter>(), &Chapter(8));
    assert_eq!(
        app.world().resource::<SaveInfo>().display_name,
        "Current",
        "untouched"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn autosave_goes_to_the_installed_store() {
    let dir = regia_test_utils::unique_temp_dir("save-flow-autosave");
    let mut app = app(&dir);
    app.init_resource::<regia_platform::AppLifecycle>()
        .add_message::<regia_platform::LifecycleEvent>();
    let memory = Arc::new(MemoryStorage::default());
    app.insert_resource(SaveStore::new(memory.clone()));
    app.update();
    regia_platform::apply_transition(&mut app, regia_platform::LifecycleEvent::Suspended);
    assert!(
        memory.read("Auto").ok().flatten().is_some(),
        "the autosave reached the store"
    );
    assert!(!dir.join("Auto.save").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_oversized_previous_copy_is_refused_by_its_length() {
    // A `.prev` past the cap must be refused from its length. Reading it would allocate about
    // a gigabyte; the current copy is damaged so the load does ask for the previous generation.
    use crate::save_game::MAX_SAVE_FILE_BYTES;
    let dir = regia_test_utils::unique_temp_dir("save-flow-huge-prev");
    let mut app = app(&dir);
    app.insert_resource(Chapter(1));
    save_to_store(app.world_mut(), "Slot").expect("first save");
    app.insert_resource(Chapter(2));
    save_to_store(app.world_mut(), "Slot").expect("second save rotates the previous copy");
    let prev = dir.join("Slot.prev");
    assert!(prev.is_file(), "the first save was kept");
    let file = std::fs::File::create(&prev).expect("replace previous");
    file.set_len(MAX_SAVE_FILE_BYTES + 1).expect("sparse length");
    drop(file);
    std::fs::write(dir.join("Slot.save"), b"not a save").expect("damage current");
    let err = load_from_store(app.world_mut(), "Slot").expect_err("oversized previous");
    match err {
        SaveError::Corrupt(why) => {
            assert!(why.contains("larger than"), "{why}");
        }
        other => panic!("expected Corrupt, got {other:?}"),
    }
    let slots = list_store(app.world()).expect("list");
    let slot = slots.iter().find(|s| s.name == "Slot").expect("Slot is listed");
    assert!(slot.has_previous);
    assert!(
        slot.header.is_none(),
        "listing must not treat an oversized previous copy as a header"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Spec 91 § 2.1: play time is virtual time. A paused game adds nothing; slow motion adds the
/// scaled delta.
#[test]
fn play_time_stops_while_paused_and_follows_the_time_scale() {
    let dir = regia_test_utils::unique_temp_dir("save-flow-pause");
    let mut app = app(&dir);
    app.update();
    let start = app.world().resource::<SaveInfo>().play_time_secs;
    app.world_mut().resource_mut::<Time>().paused = true;
    for _ in 0..5 {
        app.update();
    }
    assert_eq!(
        app.world().resource::<SaveInfo>().play_time_secs,
        start,
        "five paused frames add no play time"
    );
    {
        let mut time = app.world_mut().resource_mut::<Time>();
        time.paused = false;
        time.time_scale = 0.5;
    }
    for _ in 0..4 {
        app.update();
    }
    let played = app.world().resource::<SaveInfo>().play_time_secs - start;
    assert!(
        (played - 0.2).abs() < 1e-5,
        "four 0.1 s frames at half speed: {played}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Spec 91 § 2.1: the quickload action (F9 in the templates) loads the quick slot through the
/// same request path; without a quick slot it answers with an error outcome, not a panic.
#[test]
fn the_quick_load_action_loads_the_quick_slot() {
    use regia_input::prelude::*;
    let dir = regia_test_utils::unique_temp_dir("save-flow-quickload-action");
    let mut app = app(&dir);
    let config = InputConfig::new()
        .action("QuickSave", ActionKind::Button)
        .action("QuickLoad", ActionKind::Button)
        .binding(Binding::new("QuickSave", InputSource::key("F5")))
        .binding(Binding::new("QuickLoad", InputSource::key("F9")));
    app.add_plugins(InputPlugin::new(config));
    app.insert_resource(Chapter(6));
    app.update();

    let press = |app: &mut App, action: &str| {
        let frame = app.world().resource::<Time>().frame_count() + 1;
        app.world_mut()
            .resource_mut::<regia_input::InputInjection>()
            .schedule(action.to_string(), Vec3::X, frame, 1);
        app.update();
    };
    // No quick slot yet: one failed load outcome.
    press(&mut app, "QuickLoad");
    let first = outcomes(&mut app);
    assert!(
        first
            .iter()
            .any(|o| o.quick && o.op == SaveOp::Load && o.result.is_err()),
        "{first:?}"
    );
    assert_eq!(app.world().resource::<Chapter>(), &Chapter(6));

    press(&mut app, "QuickSave");
    assert!(dir.join("Quick.save").is_file());
    app.insert_resource(Chapter(0));
    press(&mut app, "QuickLoad");
    assert_eq!(
        app.world().resource::<Chapter>(),
        &Chapter(6),
        "the QuickLoad action restored the quick slot"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Spec 91 § 2.1: an empty slot name (or one that sanitizes to nothing) is `SaveError::Io` of kind
/// `InvalidInput` for every store operation, and nothing is written.
#[test]
fn an_empty_slot_name_is_invalid_input_everywhere() {
    let dir = regia_test_utils::unique_temp_dir("save-flow-empty-slot");
    let mut app = app(&dir);
    let invalid = |r: Result<(), SaveError>| matches!(r, Err(SaveError::Io(ref io, _)) if io.kind() == std::io::ErrorKind::InvalidInput);
    assert!(invalid(save_to_store(app.world_mut(), "").map(|_| ())));
    assert!(invalid(load_from_store(app.world_mut(), "").map(|_| ())));
    assert!(invalid(delete_from_store(app.world(), "").map(|_| ())));
    assert_eq!(
        app.world().resource::<SaveInfo>().save_count,
        0,
        "a refused save does not count"
    );
    assert!(
        list_store(app.world()).is_ok_and(|slots| slots.is_empty()),
        "nothing was written"
    );
    // Garbage bytes have no header; a real container does.
    assert!(header_from_bytes(b"not a save").is_none());
    assert!(header_from_bytes(&[]).is_none());
    let bytes = SaveGame::encode(app.world_mut(), &SaveSpec::default()).expect("encode");
    assert!(header_from_bytes(&bytes).is_some());
    let _ = std::fs::remove_dir_all(&dir);
}
