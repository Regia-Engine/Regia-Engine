// SPDX-License-Identifier: MPL-2.0
//! Threat-model finding 18 (ADR-3285): `SaveGame::load` caps a save at 1 GiB, but the slot store
//! read the whole file, and a load menu listing the slots read every `.save` whole. The first test
//! is the threat-model reproducer, un-ignored.

use regia_app::prelude::*;
use regia_framework::save_flow::{FileSaveStorage, SaveStorage, SaveStore, list_store};
use regia_framework::save_game::{
    MAX_SAVE_FILE_BYTES, SAVE_FORMAT_VERSION, SaveError, SaveGame, SavePlugin, SaveSpec,
};

/// A sparse file one byte over the cap: no disk blocks, and reading it whole would allocate 1 GiB.
fn oversized(dir: &std::path::Path, name: &str) {
    let file = std::fs::File::create(dir.join(name)).expect("create");
    file.set_len(MAX_SAVE_FILE_BYTES + 1).expect("sparse file");
}

/// Finding 18: the slot store read a save over the cap whole.
#[test]
fn finding_18_the_slot_store_keeps_the_save_size_cap() {
    let dir = regia_test_utils::unique_temp_dir("threat-save-cap");
    oversized(&dir, "x.save");
    let store = FileSaveStorage::new(&dir);
    assert!(store.read("x").is_err(), "a save over the cap was read whole");
    assert!(
        matches!(store.read("x"), Err(SaveError::Corrupt(message)) if message.contains("larger than")),
        "the cap's own error"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_slot_list_reads_only_headers_and_refuses_an_oversized_slot_by_size() {
    let dir = regia_test_utils::unique_temp_dir("threat-save-list");
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, SavePlugin::default()));
    let bytes = SaveGame::encode(app.world_mut(), &SaveSpec::default()).expect("encode an empty world");
    std::fs::write(dir.join("good.save"), &bytes).expect("write");
    oversized(&dir, "huge.save");
    std::fs::write(dir.join("junk.save"), b"not a save").expect("write");
    app.insert_resource(SaveStore::new(FileSaveStorage::new(&dir)));

    let listing = list_store(app.world()).expect("list");
    let names: Vec<&str> = listing.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["good", "huge", "junk"]);
    assert_eq!(
        listing[0].header.as_ref().map(|h| h.version),
        Some(SAVE_FORMAT_VERSION)
    );
    assert!(listing[1].header.is_none(), "over the cap: listed as damaged");
    assert!(listing[2].header.is_none());

    let store = FileSaveStorage::new(&dir);
    assert!(store.read_header("huge").is_err(), "refused by its size");
    assert!(store.read_header("missing").expect("not an error").is_none());
    let header = store.read_header("good").expect("readable").expect("valid");
    assert_eq!(header.version, SAVE_FORMAT_VERSION);
    // The store still loads what it lists.
    assert_eq!(store.read("good").expect("readable").as_deref(), Some(&bytes[..]));
    assert!(store.read("missing").expect("not an error").is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A slot just under the cap with a valid magic is checked in fixed chunks, not held whole: the
/// header comes from the first bytes and the CRC refuses the zero-filled rest.
#[test]
fn a_big_slot_under_the_cap_lists_without_its_payload() {
    let dir = regia_test_utils::unique_temp_dir("threat-save-big");
    let path = dir.join("big.save");
    {
        use std::io::Write as _;
        let mut file = std::fs::File::create(&path).expect("create");
        file.write_all(regia_framework::save_game::SAVE_MAGIC)
            .expect("magic");
        file.write_all(&SAVE_FORMAT_VERSION.to_le_bytes())
            .expect("version");
        file.set_len(64 * 1024 * 1024).expect("sparse 64 MiB");
    }
    let store = FileSaveStorage::new(&dir);
    assert!(
        store.read_header("big").expect("readable").is_none(),
        "the CRC fails"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
