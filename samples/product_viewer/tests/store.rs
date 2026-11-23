// SPDX-License-Identifier: MIT-0
//! The package store on real cooked packages, on disk: a push is found, a broken folder is listed with its
//! problem, tampering is refused at open, and a killed install leaves nothing behind.

use std::path::{Path, PathBuf};

use product_viewer::store::{DirSource, PackageStore, folder_name};
use regia_test_utils::unique_temp_dir;
use regia_viewer_pack::standin;

fn cooked(work: &Path) -> PathBuf {
    let set = standin::write(work).expect("stand-ins");
    let out = work.join("cooked");
    let options = regia_viewer_pack::cook::CookOptions {
        materials: Some(set.library.clone()),
        option_graph: set.option_graphs.first().cloned(),
        ..Default::default()
    };
    regia_viewer_pack::cook::pack(&set.kit_dir, &set.recipes[0], &out, &options).expect("cook");
    out
}

fn copy_folder(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("folder");
    for entry in std::fs::read_dir(from).expect("read").flatten() {
        std::fs::copy(entry.path(), to.join(entry.file_name())).expect("copy");
    }
}

#[test]
fn folder_names_follow_the_package_id() {
    assert_eq!(
        folder_name("House-A/ED MQBEZ").as_deref(),
        Some("house-a-ed-mqbez")
    );
    assert_eq!(folder_name(""), None);
    assert_eq!(folder_name("/"), None);
    assert_eq!(folder_name(".hidden"), None);
}

#[test]
fn a_pushed_folder_is_found_verified_and_the_first_root_wins() {
    let dir = unique_temp_dir("product-viewer-store-push");
    let package = cooked(&dir.join("work"));
    let (internal, external) = (dir.join("internal"), dir.join("external"));
    let store = PackageStore::new(vec![internal.clone(), external.clone()]);
    assert!(external.is_dir(), "the push target exists after the first launch");
    copy_folder(&package, &external.join("pushed"));
    let listed = store.list();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].slug, "pushed");
    assert!(listed[0].package_id.contains('/'));
    assert_eq!(listed[0].problem, None);
    assert!(store.open("pushed").is_ok());
    // The same name in the first root shadows it.
    copy_folder(&package, &internal.join("pushed"));
    assert_eq!(store.list().len(), 1);
    assert_eq!(store.list()[0].dir, internal.join("pushed"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_tampered_file_is_refused_at_open_and_a_broken_manifest_is_listed() {
    let dir = unique_temp_dir("product-viewer-store-tamper");
    let package = cooked(&dir.join("work"));
    let root = dir.join("store");
    let store = PackageStore::new(vec![root.clone()]);
    copy_folder(&package, &root.join("tampered"));
    let mesh = root.join("tampered/mesh.rvm");
    let mut bytes = std::fs::read(&mesh).expect("read");
    bytes[10] ^= 0xff;
    std::fs::write(&mesh, bytes).expect("write");
    assert_eq!(
        store.open("tampered").err().map(|e| e.code),
        Some("pack_hash_mismatch")
    );
    let missing = root.join("broken");
    std::fs::create_dir_all(&missing).expect("folder");
    std::fs::write(missing.join("manifest.json"), b"not json").expect("write");
    let broken = store
        .list()
        .into_iter()
        .find(|e| e.slug == "broken")
        .expect("listed");
    assert_eq!(broken.problem.as_deref(), Some("pack_bad_manifest"));
    assert_eq!(
        store.open("nothing").err().map(|e| e.code),
        Some("pack_missing_file")
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn an_install_replaces_atomically_and_leftovers_are_cleaned_at_start() {
    let dir = unique_temp_dir("product-viewer-store-install");
    let package = cooked(&dir.join("work"));
    let root = dir.join("store");
    let left = root.join(".install-x-1-0");
    std::fs::create_dir_all(&left).expect("folder");
    let store = PackageStore::new(vec![root.clone()]);
    assert!(
        !left.exists(),
        "a killed install's folder is gone at the next start"
    );
    let first = store
        .install(&mut DirSource(package.clone()), &mut |_, _| {})
        .expect("install");
    assert!(store.open(&first).is_ok());
    let second = store
        .install(&mut DirSource(package), &mut |_, _| {})
        .expect("install again");
    assert_eq!(first, second);
    assert_eq!(store.list().len(), 1);
    let _ = std::fs::remove_dir_all(dir);
}
