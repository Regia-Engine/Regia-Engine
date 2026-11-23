// SPDX-License-Identifier: MPL-2.0
//! `mods://` (ADR-2418): load order and overrides on real folders and paks, confinement (`..`,
//! absolute paths, symbolic links at mount and after it), the content-set hash (pinned, so it is
//! stable across runs and platforms, and order-sensitive), and change detection.

use std::collections::BTreeMap;
use std::path::Path;

use regia_asset::mods_vfs::{
    ContentSetHash, MODS_LOBBY_KEY, ModSource, ModsErrorCode, ModsLimits, ModsVfs, content_set_hash,
};
use regia_asset::{
    AssetId, FPAK_BLOB_ALIGNMENT, FPAK_VERSION, ManifestEntry, PakEntry, PakToc, encode_pak_header,
};
use regia_test_utils::unique_temp_dir;

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

fn folder(tag: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
    let root = unique_temp_dir(tag);
    for (rel, text) in files {
        write(&root, rel, text);
    }
    root
}

/// An uncompressed pak holding `files` (manifest path → bytes).
fn write_pak(path: &Path, files: &[(&str, &[u8])]) {
    let mut data = vec![0u8; 60];
    let mut entries = Vec::new();
    let mut manifest = Vec::new();
    for (n, (rel, payload)) in files.iter().enumerate() {
        let rem = data.len() % FPAK_BLOB_ALIGNMENT as usize;
        if rem != 0 {
            data.resize(data.len() + (FPAK_BLOB_ALIGNMENT as usize - rem), 0);
        }
        let id = AssetId(regia_core::id::Guid(n as u128 + 1));
        let offset = data.len() as u64;
        data.extend_from_slice(payload);
        entries.push(PakEntry {
            id,
            type_tag: regia_core::id::TypePathHash::of("regia_asset::Blob"),
            offset,
            size: payload.len() as u64,
            compressed_size: payload.len() as u64,
            block_table_offset: 0,
        });
        manifest.push(ManifestEntry {
            path: (*rel).to_owned(),
            id,
            type_path: "regia_asset::Blob".into(),
        });
    }
    let toc_offset = data.len() as u64;
    let toc = PakToc {
        entries,
        manifest,
        dependencies: Vec::new(),
    };
    let toc_bytes = postcard::to_allocvec(&toc).expect("toc");
    data.extend_from_slice(&toc_bytes);
    let hash = *blake3::hash(&data[..toc_offset as usize]).as_bytes();
    let header = encode_pak_header(FPAK_VERSION, 0, toc_offset, toc_bytes.len() as u64, &hash);
    data[..60].copy_from_slice(&header);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, data).expect("write pak");
}

#[test]
fn later_folders_override_earlier_ones_in_the_declared_order() {
    let base = folder(
        "mods_base",
        &[
            ("textures/rock.txt", "base rock"),
            ("config/game.ron", "base cfg"),
        ],
    );
    let patch = folder(
        "mods_patch",
        &[("textures/rock.txt", "patch rock"), ("textures/new.txt", "new")],
    );
    let mut vfs = ModsVfs::new();
    vfs.mount_all([
        ("base", ModSource::Dir(base.clone())),
        ("patch", ModSource::Dir(patch.clone())),
    ])
    .expect("mount");
    assert_eq!(vfs.order(), ["base", "patch"]);
    assert_eq!(vfs.read("mods://textures/rock.txt").expect("rock"), b"patch rock");
    assert_eq!(vfs.read("mods://config/game.ron").expect("cfg"), b"base cfg");
    assert_eq!(vfs.read("mods://textures/new.txt").expect("new"), b"new");
    assert_eq!(vfs.conflicts().len(), 1);
    assert_eq!(vfs.conflicts()[0].path, "textures/rock.txt");
    assert_eq!(vfs.conflicts()[0].winner, "patch");
    assert_eq!(vfs.conflicts()[0].overridden, ["base"]);

    // Reorder: the base now wins.
    vfs.set_order(&["patch", "base"]).expect("order");
    assert_eq!(vfs.read("mods://textures/rock.txt").expect("rock"), b"base rock");
    assert_eq!(vfs.conflicts()[0].winner, "base");

    // Unmount: the other mod's file shows through again.
    vfs.unmount("base").expect("unmount");
    assert_eq!(vfs.read("mods://textures/rock.txt").expect("rock"), b"patch rock");
    assert!(vfs.conflicts().is_empty());
    assert_eq!(
        vfs.read("mods://config/game.ron").map_err(|e| e.code),
        Err(ModsErrorCode::NotFound)
    );
    let _ = std::fs::remove_dir_all(base);
    let _ = std::fs::remove_dir_all(patch);
}

#[test]
fn paths_never_leave_the_mount() {
    let outside = unique_temp_dir("mods_outside");
    write(&outside, "secret.txt", "secret");
    let root = folder("mods_confined", &[("a/b.txt", "b")]);
    let mut vfs = ModsVfs::new();
    vfs.mount("m", ModSource::Dir(root.clone())).expect("mount");
    let secret = outside.join("secret.txt");
    let absolute = secret.to_string_lossy().into_owned();
    for path in [
        "mods://../secret.txt".to_owned(),
        "mods://a/../../secret.txt".to_owned(),
        "mods://a/..".to_owned(),
        format!("mods://{absolute}"),
        absolute.clone(),
        "/etc/passwd".to_owned(),
        "C:/Windows/win.ini".to_owned(),
        "a\\..\\..\\secret.txt".to_owned(),
    ] {
        assert_eq!(
            vfs.read(&path).map_err(|e| e.code),
            Err(ModsErrorCode::PathEscape),
            "{path}"
        );
        assert!(!vfs.exists(&path), "{path}");
    }
    assert_eq!(
        vfs.read("assets://a/b.txt").map_err(|e| e.code),
        Err(ModsErrorCode::PathInvalid)
    );
    // A file added after the mount is not served: only indexed files exist.
    write(&root, "late.txt", "late");
    assert_eq!(
        vfs.read("mods://late.txt").map_err(|e| e.code),
        Err(ModsErrorCode::NotFound)
    );
    // The mount is read-only.
    assert_eq!(
        vfs.write("mods://a/b.txt", b"x").map_err(|e| e.code),
        Err(ModsErrorCode::ReadOnly)
    );
    // A pak whose manifest names a path outside the mod is refused whole.
    let pak = root.join("escape.fpak");
    write_pak(&pak, &[("ok.txt", b"ok"), ("../escape.txt", b"no")]);
    assert_eq!(
        vfs.mount("evil", ModSource::Pak(pak)).map_err(|e| e.code),
        Err(ModsErrorCode::PathEscape)
    );
    assert_eq!(vfs.order(), ["m"]);
    // In-memory mods follow the same rules.
    let mut files = BTreeMap::new();
    files.insert("/abs.txt".to_owned(), b"x".to_vec());
    assert_eq!(
        vfs.mount("mem", ModSource::Memory(files)).map_err(|e| e.code),
        Err(ModsErrorCode::PathEscape)
    );
    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_dir_all(outside);
}

#[cfg(unix)]
#[test]
fn symbolic_links_are_refused_at_mount_and_after_it() {
    use std::os::unix::fs::symlink;
    let outside = unique_temp_dir("mods_link_target");
    write(&outside, "secret.txt", "secret");

    // A file link inside the mod.
    let file_link = folder("mods_file_link", &[("ok.txt", "ok")]);
    symlink(outside.join("secret.txt"), file_link.join("leak.txt")).expect("symlink");
    let mut vfs = ModsVfs::new();
    assert_eq!(
        vfs.mount("a", ModSource::Dir(file_link.clone()))
            .map_err(|e| e.code),
        Err(ModsErrorCode::Symlink)
    );
    // A folder link inside the mod.
    let dir_link = folder("mods_dir_link", &[("ok.txt", "ok")]);
    symlink(&outside, dir_link.join("sub")).expect("symlink");
    assert_eq!(
        vfs.mount("b", ModSource::Dir(dir_link.clone()))
            .map_err(|e| e.code),
        Err(ModsErrorCode::Symlink)
    );
    assert!(vfs.is_empty());

    // The root itself may be a link (Workshop and launcher folders often are).
    let real = folder("mods_real_root", &[("sub/x.txt", "x")]);
    let link_root = unique_temp_dir("mods_link_root_parent").join("root");
    symlink(&real, &link_root).expect("symlink root");
    vfs.mount("c", ModSource::Dir(link_root.clone()))
        .expect("linked root mounts");
    assert_eq!(vfs.read("mods://sub/x.txt").expect("read"), b"x");

    // A link swapped in after the mount is refused on read.
    std::fs::remove_dir_all(real.join("sub")).expect("rm sub");
    symlink(&outside, real.join("sub")).expect("swap in a link");
    std::fs::rename(outside.join("secret.txt"), outside.join("x.txt")).expect("rename");
    assert_eq!(
        vfs.read("mods://sub/x.txt").map_err(|e| e.code),
        Err(ModsErrorCode::Symlink)
    );
    for dir in [file_link, dir_link, real, outside] {
        let _ = std::fs::remove_dir_all(dir);
    }
    if let Some(parent) = link_root.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

/// The pinned content-set hash of a fixed two-mod set. It must never change: lobbies of different
/// builds and platforms match on it. Changing it needs an ADR (a new derive-key version).
const PINNED_SET: &str = "78868f3213940bbd64d6f365e83cd52d1ada0966f5693f76653dc5afe7294b77";

fn fixed_set(order: &[&str]) -> ModsVfs {
    let mut a = BTreeMap::new();
    a.insert("data/units.ron".to_owned(), b"(hp: 100)".to_vec());
    a.insert("main.lua".to_owned(), b"print('a')".to_vec());
    let mut b = BTreeMap::new();
    b.insert("data/units.ron".to_owned(), b"(hp: 250)".to_vec());
    let mut vfs = ModsVfs::new();
    for id in order {
        let files = if *id == "alpha" { a.clone() } else { b.clone() };
        vfs.mount(id, ModSource::Memory(files)).expect("mount");
    }
    vfs
}

#[test]
fn the_content_set_hash_is_stable_and_follows_the_load_order() {
    let forward = fixed_set(&["alpha", "beta"]);
    let again = fixed_set(&["alpha", "beta"]);
    assert_eq!(forward.content_set_hash(), again.content_set_hash());
    assert_eq!(
        forward.content_set_hash().to_hex(),
        PINNED_SET,
        "the pinned hash moved"
    );
    let reversed = fixed_set(&["beta", "alpha"]);
    assert_ne!(reversed.content_set_hash(), forward.content_set_hash());
    // A reorder in place gives the same hash as mounting in that order.
    let mut reordered = fixed_set(&["alpha", "beta"]);
    reordered.set_order(&["beta", "alpha"]).expect("order");
    assert_eq!(reordered.content_set_hash(), reversed.content_set_hash());
    // The free function over the per-mod hashes is the same thing.
    let per_mod: Vec<ContentSetHash> = forward.mods().iter().map(|m| m.content_hash).collect();
    assert_eq!(content_set_hash(&per_mod), forward.content_set_hash());
    // A one-byte change moves it.
    let mut changed = fixed_set(&["alpha"]);
    let mut b = BTreeMap::new();
    b.insert("data/units.ron".to_owned(), b"(hp: 251)".to_vec());
    changed.mount("beta", ModSource::Memory(b)).expect("mount");
    assert_ne!(changed.content_set_hash(), forward.content_set_hash());
    // The lobby entry carries it.
    let (key, value) = forward.lobby_entry();
    assert_eq!(key, MODS_LOBBY_KEY);
    assert_eq!(value, forward.content_set_hash().to_hex());
}

#[test]
fn a_folder_and_a_pak_of_the_same_files_hash_the_same() {
    let files: [(&str, &[u8]); 2] = [("maps/one.map", b"map one"), ("units.ron", b"(hp: 3)")];
    let dir = unique_temp_dir("mods_same_dir");
    for (rel, bytes) in files {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, bytes).expect("write");
    }
    let pak_dir = unique_temp_dir("mods_same_pak");
    let pak = pak_dir.join("content.fpak");
    write_pak(&pak, &files);
    // A folder holding nothing but one pak is detected as that pak (a Workshop item).
    assert_eq!(
        ModSource::detect(&pak_dir).expect("detect"),
        ModSource::Pak(pak.clone())
    );
    assert_eq!(
        ModSource::detect(&dir).expect("detect"),
        ModSource::Dir(dir.clone())
    );
    let mut from_dir = ModsVfs::new();
    from_dir
        .mount("folder", ModSource::Dir(dir.clone()))
        .expect("dir");
    let mut from_pak = ModsVfs::new();
    from_pak
        .mount("pak", ModSource::detect(&pak_dir).expect("detect"))
        .expect("pak");
    assert_eq!(from_pak.read("mods://maps/one.map").expect("read"), b"map one");
    assert_eq!(from_dir.mods()[0].content_hash, from_pak.mods()[0].content_hash);
    assert_eq!(from_dir.content_set_hash(), from_pak.content_set_hash());
    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_dir_all(pak_dir);
}

#[test]
fn a_file_changed_after_the_mount_is_refused() {
    let root = folder("mods_changed", &[("a.txt", "one"), ("b.txt", "two")]);
    let mut vfs = ModsVfs::new();
    vfs.mount("m", ModSource::Dir(root.clone())).expect("mount");
    let hash = vfs.content_set_hash();
    write(&root, "a.txt", "ONE");
    assert_eq!(
        vfs.read("mods://a.txt").map_err(|e| e.code),
        Err(ModsErrorCode::Changed)
    );
    write(&root, "b.txt", "two plus");
    assert_eq!(
        vfs.read("mods://b.txt").map_err(|e| e.code),
        Err(ModsErrorCode::Changed)
    );
    std::fs::remove_file(root.join("b.txt")).expect("rm");
    assert_eq!(
        vfs.read("mods://b.txt").map_err(|e| e.code),
        Err(ModsErrorCode::Changed)
    );
    // A remount hashes what is there now.
    vfs.unmount("m").expect("unmount");
    vfs.mount("m", ModSource::Dir(root.clone())).expect("remount");
    assert_eq!(vfs.read("mods://a.txt").expect("read"), b"ONE");
    assert_ne!(vfs.content_set_hash(), hash);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn limits_are_enforced() {
    let root = folder("mods_limits", &[("a", "1"), ("b", "2"), ("c", "3")]);
    let mut vfs = ModsVfs::with_limits(ModsLimits {
        max_files_per_mod: 2,
        ..ModsLimits::default()
    });
    assert_eq!(
        vfs.mount("m", ModSource::Dir(root.clone())).map_err(|e| e.code),
        Err(ModsErrorCode::TooLarge)
    );
    let mut vfs = ModsVfs::with_limits(ModsLimits {
        max_mods: 1,
        ..ModsLimits::default()
    });
    vfs.mount("one", ModSource::Memory(BTreeMap::new())).expect("one");
    assert_eq!(
        vfs.mount("two", ModSource::Memory(BTreeMap::new()))
            .map_err(|e| e.code),
        Err(ModsErrorCode::TooLarge)
    );
    let _ = std::fs::remove_dir_all(root);
}

/// Review finding (ADR-2418): a file swapped for another one (even with the same bytes) or for a
/// FIFO after the mount is refused on read, and the FIFO does not hang the call.
#[cfg(unix)]
#[test]
fn a_swapped_file_or_fifo_is_refused_without_hanging() {
    let root = folder("mods_swap", &[("same.txt", "same"), ("pipe.txt", "pipe")]);
    let mut vfs = ModsVfs::new();
    vfs.mount("m", ModSource::Dir(root.clone())).expect("mount");
    // Same bytes, another file (an inode swap through rename).
    std::fs::write(root.join("same.txt.new"), "same").expect("write");
    std::fs::rename(root.join("same.txt.new"), root.join("same.txt")).expect("rename");
    assert_eq!(
        vfs.read("mods://same.txt").map_err(|e| e.code),
        Err(ModsErrorCode::Changed)
    );
    // A FIFO in place of a file: refused at once, nothing blocks waiting for a writer.
    std::fs::remove_file(root.join("pipe.txt")).expect("rm");
    let made = std::process::Command::new("mkfifo")
        .arg(root.join("pipe.txt"))
        .status()
        .is_ok_and(|s| s.success());
    if made {
        let err = vfs.read("mods://pipe.txt").expect_err("a FIFO is not served");
        assert!(
            matches!(err.code, ModsErrorCode::Io | ModsErrorCode::Changed),
            "{err:?}"
        );
        // A folder holding a FIFO does not mount either.
        let mut other = ModsVfs::new();
        assert_eq!(
            other.mount("x", ModSource::Dir(root.clone())).map_err(|e| e.code),
            Err(ModsErrorCode::Io)
        );
    }
    let _ = std::fs::remove_dir_all(root);
}

/// Review finding (asset2 correctness, finding 3): one mod's file can use the exact path
/// another mod's file lives under (`cfg` and `cfg/settings.ron`). Listing must still reach the
/// children; only a file with nothing under it reports "is a file, not a directory".
#[test]
fn list_dir_lists_children_when_a_file_uses_the_directory_name() {
    use regia_platform::VfsDirEntry;

    let mut file_mod = BTreeMap::new();
    file_mod.insert("cfg".to_owned(), b"a file named cfg".to_vec());
    let mut dir_mod = BTreeMap::new();
    dir_mod.insert("cfg/settings.ron".to_owned(), b"nested".to_vec());
    let mut vfs = ModsVfs::new();
    vfs.mount("file_mod", ModSource::Memory(file_mod)).expect("mount");
    vfs.mount("dir_mod", ModSource::Memory(dir_mod)).expect("mount");

    // The child is served and exists regardless of the shadowing file.
    assert_eq!(vfs.read("mods://cfg/settings.ron").expect("read"), b"nested");
    assert!(vfs.exists("mods://cfg"));
    assert!(vfs.exists("mods://cfg/settings.ron"));
    // At the root the entry is a directory (the child wins over the file).
    let root = vfs.list_dir("mods://").expect("root");
    assert_eq!(
        root,
        vec![VfsDirEntry {
            name: "cfg".to_owned(),
            is_dir: true,
        }]
    );
    // Descending into `cfg` lists the child instead of claiming the path is a file.
    let children = vfs.list_dir("mods://cfg").expect("children");
    assert_eq!(
        children,
        vec![VfsDirEntry {
            name: "settings.ron".to_owned(),
            is_dir: false,
        }]
    );

    // A file with nothing under it still reports "is a file, not a directory".
    let mut solo = BTreeMap::new();
    solo.insert("cfg".to_owned(), b"only a file".to_vec());
    let mut alone = ModsVfs::new();
    alone.mount("solo", ModSource::Memory(solo)).expect("mount");
    assert_eq!(
        alone.list_dir("mods://cfg").map_err(|e| e.code),
        Err(ModsErrorCode::NotFound)
    );
    // And a directory with neither a file nor children at the path reports "no such directory".
    assert_eq!(
        alone.list_dir("mods://missing").map_err(|e| e.code),
        Err(ModsErrorCode::NotFound)
    );
}

#[test]
fn a_pak_entry_without_a_file_name_is_refused() {
    let dir = unique_temp_dir("mods_empty_entry");
    let pak = dir.join("bad.fpak");
    write_pak(&pak, &[("ok.txt", b"ok"), (".", b"root")]);
    let mut vfs = ModsVfs::new();
    assert_eq!(
        vfs.mount("bad", ModSource::Pak(pak)).map_err(|e| e.code),
        Err(ModsErrorCode::PathInvalid)
    );
    let _ = std::fs::remove_dir_all(dir);
}
