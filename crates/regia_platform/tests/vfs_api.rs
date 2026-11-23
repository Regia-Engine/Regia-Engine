// SPDX-License-Identifier: MPL-2.0
//! The public [`Vfs`] surface: mount management, error variants and their messages, the default
//! trait methods of a custom [`VfsMount`], and `write_atomic` edge cases that the in-crate tests
//! (which use the directory backend only) do not reach.

use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use regia_platform::{
    DirMount, MountPoint, Vfs, VfsDirEntry, VfsError, VfsMount, VfsPath, VfsPathError, VfsReader,
};

/// A unique temporary directory (pid + counter), removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "regia-vfs-api-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn path(s: &str) -> VfsPath {
    VfsPath::parse(s).expect("valid test path")
}

/// An in-memory backend that only implements the required methods, so the trait's default
/// `write_atomic` and `host_path` are what the `Vfs` sees.
struct MemMount {
    name: &'static str,
    data: Vec<u8>,
}

impl VfsMount for MemMount {
    fn read(&self, relative: &str) -> io::Result<Vec<u8>> {
        if relative == self.name {
            Ok(self.data.clone())
        } else {
            Err(io::ErrorKind::NotFound.into())
        }
    }

    fn open(&self, relative: &str) -> io::Result<Box<dyn VfsReader>> {
        self.read(relative)
            .map(|bytes| Box::new(Cursor::new(bytes)) as Box<dyn VfsReader>)
    }

    fn exists(&self, relative: &str) -> bool {
        relative == self.name
    }

    fn list_dir(&self, relative: &str) -> io::Result<Vec<VfsDirEntry>> {
        if relative.is_empty() {
            Ok(vec![VfsDirEntry {
                name: self.name.to_owned(),
                is_dir: false,
            }])
        } else {
            Err(io::ErrorKind::NotFound.into())
        }
    }

    fn describe(&self) -> String {
        format!("mem {}", self.name)
    }
}

#[test]
fn mount_replace_unmount_and_describe() {
    let mut vfs = Vfs::new();
    for point in MountPoint::ALL {
        assert!(!vfs.is_mounted(point));
        assert_eq!(vfs.describe_mount(point), None);
    }
    vfs.mount(
        MountPoint::Assets,
        MemMount {
            name: "a.bin",
            data: vec![1],
        },
    );
    assert!(vfs.is_mounted(MountPoint::Assets));
    assert!(!vfs.is_mounted(MountPoint::User));
    assert_eq!(
        vfs.describe_mount(MountPoint::Assets).as_deref(),
        Some("mem a.bin")
    );
    assert_eq!(&*vfs.read_sync(&path("assets://a.bin")).expect("read"), [1]);

    // Mounting again replaces the backend.
    vfs.mount(
        MountPoint::Assets,
        MemMount {
            name: "b.bin",
            data: vec![2, 3],
        },
    );
    assert_eq!(
        vfs.describe_mount(MountPoint::Assets).as_deref(),
        Some("mem b.bin")
    );
    assert!(matches!(
        vfs.read_sync(&path("assets://a.bin")),
        Err(VfsError::NotFound { .. })
    ));
    assert_eq!(&*vfs.read_sync(&path("assets://b.bin")).expect("read"), [2, 3]);

    vfs.unmount(MountPoint::Assets);
    assert!(!vfs.is_mounted(MountPoint::Assets));
    assert!(matches!(
        vfs.read_sync(&path("assets://b.bin")),
        Err(VfsError::NotMounted { .. })
    ));
    // Unmounting an empty point is harmless.
    vfs.unmount(MountPoint::Assets);
}

#[test]
fn clones_share_mounts_and_debug_lists_only_mounted_points() {
    let mut vfs = Vfs::new();
    vfs.mount(
        MountPoint::Cache,
        MemMount {
            name: "x",
            data: Vec::new(),
        },
    );
    let copy = vfs.clone();
    assert!(copy.is_mounted(MountPoint::Cache));
    let debug = format!("{copy:?}");
    assert!(debug.contains("cache") && debug.contains("mem x"), "{debug}");
    assert!(!debug.contains("assets"), "{debug}");
    assert_eq!(format!("{:?}", Vfs::new()), "{}");
}

#[test]
fn custom_mount_defaults_are_read_only_without_a_host_path() {
    let mut vfs = Vfs::new();
    // `user://` is a writable mount point, but this backend does not implement writes.
    vfs.mount(
        MountPoint::User,
        MemMount {
            name: "save",
            data: vec![9],
        },
    );
    let save = path("user://save");
    assert!(matches!(
        vfs.write_atomic(&save, b"new"),
        Err(VfsError::ReadOnly { path }) if path == save
    ));
    assert_eq!(&*vfs.read_sync(&save).expect("unchanged"), [9]);
    assert_eq!(vfs.host_path(&save), None);
    // Nothing mounted: no host path either.
    assert_eq!(Vfs::new().host_path(&save), None);

    let entries = vfs.list_dir(&VfsPath::root(MountPoint::User)).expect("list");
    assert_eq!(
        entries,
        [VfsDirEntry {
            name: "save".into(),
            is_dir: false
        }]
    );
}

#[test]
fn open_stream_seeks_on_any_backend() {
    let mut vfs = Vfs::new();
    vfs.mount(
        MountPoint::Assets,
        MemMount {
            name: "digits",
            data: b"0123456789".to_vec(),
        },
    );
    let mut stream = vfs.open_stream(&path("assets://digits")).expect("open");
    assert_eq!(stream.seek(SeekFrom::End(-3)).expect("seek"), 7);
    let mut tail = String::new();
    stream.read_to_string(&mut tail).expect("read");
    assert_eq!(tail, "789");
    assert!(matches!(
        vfs.open_stream(&path("assets://missing")),
        Err(VfsError::NotFound { .. })
    ));
    assert!(matches!(
        Vfs::new().open_stream(&path("assets://digits")),
        Err(VfsError::NotMounted { .. })
    ));
}

#[test]
fn write_atomic_to_the_root_and_unmounted_writable_points() {
    let tmp = TempDir::new("write-edge");
    let mut vfs = Vfs::new();
    // Writable point, nothing mounted.
    assert!(matches!(
        vfs.write_atomic(&path("user://a.sav"), b"x"),
        Err(VfsError::NotMounted { .. })
    ));
    vfs.mount(MountPoint::User, DirMount::writable(tmp.path()));
    let err = vfs
        .write_atomic(&VfsPath::root(MountPoint::User), b"x")
        .expect_err("a mount root is not a file");
    match err {
        VfsError::Io { source, .. } => assert_eq!(source.kind(), io::ErrorKind::InvalidInput),
        other => panic!("expected Io, got {other:?}"),
    }
    // A read-only point is refused before the backend is consulted, even when unmounted.
    assert!(matches!(
        Vfs::new().write_atomic(&path("assets://a"), b"x"),
        Err(VfsError::ReadOnly { .. })
    ));
}

#[test]
fn write_atomic_overwrites_with_shorter_data_and_creates_nested_parents() {
    let tmp = TempDir::new("write-shrink");
    let mut vfs = Vfs::new();
    vfs.mount(MountPoint::Cache, DirMount::writable(tmp.path()));
    let target = path("cache://a/b/c/blob.bin");
    vfs.write_atomic(&target, &[7; 4096]).expect("long");
    vfs.write_atomic(&target, b"short").expect("short");
    assert_eq!(&*vfs.read_sync(&target).expect("read"), b"short");
    vfs.write_atomic(&target, b"").expect("empty");
    assert!(vfs.read_sync(&target).expect("read").is_empty());
    assert!(vfs.exists(&path("cache://a/b")));
    let host = vfs.host_path(&target).expect("dir mount has a host path");
    assert!(host.starts_with(tmp.path()) && host.ends_with("blob.bin"));
}

#[test]
fn exists_and_list_dir_distinguish_files_from_directories() {
    let tmp = TempDir::new("list-kinds");
    let mut vfs = Vfs::new();
    vfs.mount(MountPoint::User, DirMount::writable(tmp.path()));
    vfs.write_atomic(&path("user://saves/b.sav"), b"b")
        .expect("write");
    vfs.write_atomic(&path("user://saves/a.sav"), b"a")
        .expect("write");
    vfs.write_atomic(&path("user://saves/sub/c.sav"), b"c")
        .expect("write");
    let names: Vec<_> = vfs
        .list_dir(&path("user://saves"))
        .expect("list")
        .into_iter()
        .map(|e| (e.name, e.is_dir))
        .collect();
    assert_eq!(
        names,
        [
            ("a.sav".to_owned(), false),
            ("b.sav".to_owned(), false),
            ("sub".to_owned(), true)
        ]
    );
    assert!(vfs.exists(&path("user://saves/sub")));
    assert!(vfs.exists(&path("user://saves/a.sav")));
    assert!(!vfs.exists(&path("user://saves/zzz.sav")));
    // Listing a file is an error, not an empty directory.
    assert!(vfs.list_dir(&path("user://saves/a.sav")).is_err());
}

#[test]
fn errors_name_the_path_and_the_reason() {
    let missing = path("assets://textures/rock.ktx2");
    let not_found = VfsError::NotFound {
        path: missing.clone(),
    };
    assert_eq!(not_found.to_string(), "assets://textures/rock.ktx2: not found");
    let not_mounted = VfsError::NotMounted {
        path: missing.clone(),
    };
    assert_eq!(
        not_mounted.to_string(),
        "assets://textures/rock.ktx2: nothing mounted at assets://"
    );
    let read_only = VfsError::ReadOnly {
        path: missing.clone(),
    };
    assert!(
        read_only
            .to_string()
            .contains("only user:// and cache:// are writable")
    );
    let io = VfsError::Io {
        path: missing,
        source: io::Error::other("disk on fire"),
    };
    let text = io.to_string();
    assert!(
        text.starts_with("assets://textures/rock.ktx2: ") && text.contains("disk on fire"),
        "{text}"
    );
    assert!(std::error::Error::source(&io).is_some());
    assert_eq!(VfsError::Unsupported("nope").to_string(), "unsupported: nope");
    let invalid = VfsError::InvalidPath {
        path: "assets://../x".into(),
        reason: VfsPathError::ParentSegment,
    };
    assert!(invalid.to_string().contains("assets://../x") && invalid.to_string().contains(".."));
}

#[test]
fn mount_point_text_round_trips_and_only_user_and_cache_are_writable() {
    for point in MountPoint::ALL {
        assert_eq!(point.as_str().parse::<MountPoint>(), Ok(point));
        assert_eq!(point.to_string(), point.as_str());
        assert_eq!(
            point.is_writable(),
            matches!(point, MountPoint::User | MountPoint::Cache),
            "{point}"
        );
    }
    assert_eq!(
        "bogus".parse::<MountPoint>(),
        Err(VfsPathError::UnknownMount("bogus".into()))
    );
    assert_eq!(
        "".parse::<MountPoint>(),
        Err(VfsPathError::UnknownMount(String::new()))
    );
}

#[test]
fn vfs_path_conversions_and_ordering() {
    let from_str: VfsPath = "user://a/b".parse().expect("FromStr");
    let try_from = VfsPath::try_from("user://a/b").expect("TryFrom");
    assert_eq!(from_str, try_from);
    assert_eq!(VfsPath::new(MountPoint::User, "a/./b").expect("new"), from_str);
    assert_eq!(VfsPath::new(MountPoint::User, "/a"), Err(VfsPathError::Absolute));
    assert_eq!("user://a//b".parse::<VfsPath>(), Err(VfsPathError::EmptySegment));
    assert_eq!(VfsPath::root(MountPoint::Logs).to_string(), "logs://");
    assert_eq!(VfsPath::root(MountPoint::Logs).segments().count(), 0);
    // Paths work as map keys and sort deterministically.
    let mut set = std::collections::BTreeSet::new();
    for s in ["user://b", "assets://z", "user://a"] {
        set.insert(path(s));
    }
    let order: Vec<_> = set.iter().map(ToString::to_string).collect();
    assert_eq!(order, ["assets://z", "user://a", "user://b"]);
}
