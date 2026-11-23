// SPDX-License-Identifier: MPL-2.0
//! `Vfs::mount_pak` (spec 11 § 4, ADR-7232): a `.fpak` archive mounted read-only in the VFS serves
//! its manifest paths, lists their prefixes as directories, streams with seeking, refuses writes,
//! and a file that is not a pak is an error that keeps the previous mount.
//!
//! `cargo test -p regia_asset --test vfs_pak_mount`

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use regia_asset::{
    AssetId, FPAK_BLOB_ALIGNMENT, FPAK_VERSION, ManifestEntry, PakEntry, PakToc, encode_pak_header,
};
use regia_platform::{DirMount, MountPoint, Vfs, VfsDirEntry, VfsError, VfsPath};
use regia_test_utils::unique_temp_dir;

/// An uncompressed pak holding `files` (manifest path -> bytes).
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
    let toc_bytes = postcard::to_allocvec(&toc).unwrap_or_else(|e| panic!("toc: {e}"));
    data.extend_from_slice(&toc_bytes);
    let hash = *blake3::hash(&data[..toc_offset as usize]).as_bytes();
    let header = encode_pak_header(FPAK_VERSION, 0, toc_offset, toc_bytes.len() as u64, &hash);
    data[..60].copy_from_slice(&header);
    regia_test_utils::write_atomic(path, &data).unwrap_or_else(|e| panic!("write pak: {e}"));
}

fn vpath(text: &str) -> VfsPath {
    VfsPath::parse(text).unwrap_or_else(|e| panic!("{text}: {e}"))
}

#[test]
fn a_mounted_pak_serves_its_files_through_the_vfs() {
    regia_asset::install_vfs_pak_mounts();
    let dir = unique_temp_dir("vfs-pak-mount");
    let pak = dir.join("base.fpak");
    let rock: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
    write_pak(
        &pak,
        &[
            ("config/net.ron", b"(replication: ())"),
            ("textures/rock.ktx2", &rock),
            // A cook's project-relative path: served without the `assets/` segment.
            ("assets/textures/ui/icon.png", b"png"),
            ("readme.txt", b"hello"),
        ],
    );

    let mut vfs = Vfs::new();
    vfs.mount_pak(MountPoint::Assets, &pak)
        .unwrap_or_else(|e| panic!("mount_pak: {e}"));
    assert!(vfs.is_mounted(MountPoint::Assets));
    let described = vfs.describe_mount(MountPoint::Assets).unwrap_or_default();
    assert!(
        described.starts_with("pak ") && described.contains("4 files"),
        "{described}"
    );

    // Whole reads, by manifest path.
    let read = |p: &str| vfs.read_sync(&vpath(p)).map(|b| b.to_vec());
    assert_eq!(read("assets://readme.txt").ok(), Some(b"hello".to_vec()));
    assert_eq!(read("assets://textures/rock.ktx2").ok(), Some(rock.clone()));
    assert_eq!(
        read("assets://config/net.ron").ok(),
        Some(b"(replication: ())".to_vec())
    );
    assert!(matches!(
        read("assets://textures/missing.png"),
        Err(VfsError::NotFound { .. })
    ));

    // Exists for files, directories and the root.
    for p in [
        "assets://",
        "assets://textures",
        "assets://textures/ui",
        "assets://textures/ui/icon.png",
    ] {
        assert!(vfs.exists(&vpath(p)), "{p} exists");
    }
    assert!(!vfs.exists(&vpath("assets://textures/u")));

    // Directory listings, sorted, directories by their prefix.
    let entry = |name: &str, is_dir: bool| VfsDirEntry {
        name: name.to_owned(),
        is_dir,
    };
    assert_eq!(
        vfs.list_dir(&vpath("assets://"))
            .unwrap_or_else(|e| panic!("root: {e}")),
        vec![
            entry("config", true),
            entry("readme.txt", false),
            entry("textures", true)
        ]
    );
    assert_eq!(
        vfs.list_dir(&vpath("assets://textures"))
            .unwrap_or_else(|e| panic!("textures: {e}")),
        vec![entry("rock.ktx2", false), entry("ui", true)]
    );
    assert!(vfs.list_dir(&vpath("assets://nowhere")).is_err());

    // Streams seek.
    let mut stream = vfs
        .open_stream(&vpath("assets://textures/rock.ktx2"))
        .unwrap_or_else(|e| panic!("open_stream: {e}"));
    stream
        .seek(SeekFrom::Start(9_000))
        .unwrap_or_else(|e| panic!("seek: {e}"));
    let mut tail = Vec::new();
    stream
        .read_to_end(&mut tail)
        .unwrap_or_else(|e| panic!("read: {e}"));
    assert_eq!(tail, rock[9_000..]);

    // Read-only.
    assert!(matches!(
        vfs.write_atomic(&vpath("assets://readme.txt"), b"x"),
        Err(VfsError::ReadOnly { .. })
    ));

    // A pak can back a writable mount point too; writes are still refused by the backend.
    vfs.mount_pak(MountPoint::Cache, &pak)
        .unwrap_or_else(|e| panic!("mount_pak cache: {e}"));
    assert!(matches!(
        vfs.write_atomic(&vpath("cache://readme.txt"), b"x"),
        Err(VfsError::ReadOnly { .. })
    ));
}

#[test]
fn a_file_that_is_not_a_pak_is_an_error_and_keeps_the_previous_mount() {
    regia_asset::install_vfs_pak_mounts();
    let dir = unique_temp_dir("vfs-pak-bad");
    let not_pak = dir.join("notes.fpak");
    regia_test_utils::write_atomic(&not_pak, b"this is not a pak").unwrap_or_else(|e| panic!("write: {e}"));
    let mut vfs = Vfs::new();
    vfs.mount(MountPoint::Assets, DirMount::read_only(&dir));
    let before = vfs.describe_mount(MountPoint::Assets);
    match vfs.mount_pak(MountPoint::Assets, &not_pak) {
        Err(VfsError::PakOpen { pak, source }) => {
            assert_eq!(pak, not_pak);
            assert_eq!(source.kind(), std::io::ErrorKind::InvalidData, "{source}");
        }
        other => panic!("expected PakOpen, got {other:?}"),
    }
    match vfs.mount_pak(MountPoint::Assets, dir.join("missing.fpak")) {
        Err(VfsError::PakOpen { source, .. }) => assert_eq!(source.kind(), std::io::ErrorKind::NotFound),
        other => panic!("expected PakOpen, got {other:?}"),
    }
    assert_eq!(
        vfs.describe_mount(MountPoint::Assets),
        before,
        "the previous mount stays"
    );
}
