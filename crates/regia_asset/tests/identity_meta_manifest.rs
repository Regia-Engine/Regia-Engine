// SPDX-License-Identifier: MPL-2.0
//! Public-surface tests for asset identity, `.meta` validation, the path manifest and the
//! content-migrator registry: error paths and invariants not covered by `formats.rs`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use regia_asset::{
    AssetId, AssetManifest, Assets, ContentMigrator, Handle, ManifestEntry, ManifestError, MetaError,
    MetaSidecar, MigrationError, MigratorRegistry, UntypedHandle, WeakHandle, asset_db_path,
    cooked_cache_dir, default_registry, meta_migration_sources, meta_path_for, normalise_asset_path,
};
use regia_test_utils::unique_temp_dir;

const NIL_GUID: &str = "00000000-0000-0000-0000-000000000000";

// ---- AssetId / Handle -------------------------------------------------------------------------

/// Sub-asset ids are a pure function of (parent, path) and differ per path and per parent.
#[test]
fn sub_asset_ids_are_deterministic_and_distinct() {
    let parent = AssetId::new_random();
    let a1 = AssetId::sub_asset(parent, "mesh/Body");
    let a2 = AssetId::sub_asset(parent, "mesh/Body");
    assert_eq!(a1, a2);
    assert_ne!(a1, AssetId::sub_asset(parent, "mesh/Head"));
    assert_ne!(a1, AssetId::sub_asset(AssetId::new_random(), "mesh/Body"));
    assert_ne!(a1, parent);
    assert!(!a1.is_nil());
}

/// The nil id is detected, and a nil parent still derives a non-nil child.
#[test]
fn nil_asset_id_behaviour() {
    assert!(AssetId::NIL.is_nil());
    assert_eq!(AssetId::default(), AssetId::NIL);
    assert!(!AssetId::new_random().is_nil());
    assert!(!AssetId::sub_asset(AssetId::NIL, "x").is_nil());
}

/// Display and `FromStr` round-trip; garbage is rejected rather than parsed as nil.
#[test]
fn asset_id_string_round_trip_and_rejects_garbage() {
    let id = AssetId::new_random();
    let parsed: AssetId = id.to_string().parse().expect("round trip");
    assert_eq!(parsed, id);
    assert!(format!("{id:?}").starts_with("AssetId("));
    assert!("not-a-guid".parse::<AssetId>().is_err());
    assert!("".parse::<AssetId>().is_err());
}

/// The invalid handle sentinel is flagged; a real handle is not.
#[test]
fn invalid_handle_sentinel() {
    assert!(Handle::<u32>::INVALID.is_invalid());
    let mut assets = Assets::<u32>::new();
    let h = assets.insert(AssetId::new_random(), 7);
    assert!(!h.is_invalid());
    assert_eq!(assets.get(&Handle::INVALID), None);
}

/// A handle with the wrong generation or an out-of-range index resolves to nothing.
#[test]
fn stale_or_out_of_range_handles_do_not_resolve() {
    let mut assets = Assets::<&'static str>::new();
    let id = AssetId::new_random();
    let h = assets.insert(id, "a");
    let mut stale = h;
    stale.generation += 1;
    assert_eq!(assets.get(&stale), None);
    let mut far = h;
    far.index = 1000;
    assert_eq!(assets.get(&far), None);
    assert_eq!(assets.get(&h), Some(&"a"));
}

/// `handle_for_id` finds loaded ids, returns `None` for unknown ones, and distinct ids get distinct slots.
#[test]
fn handle_for_id_lookup() {
    let mut assets = Assets::<u32>::new();
    let (a, b) = (AssetId::new_random(), AssetId::new_random());
    assert_eq!(assets.handle_for_id(a), None);
    let ha = assets.insert(a, 1);
    let hb = assets.insert(b, 2);
    assert_ne!(ha.index, hb.index);
    assert_eq!(assets.handle_for_id(a), Some(ha));
    assert_eq!(assets.handle_for_id(b), Some(hb));
    assert_eq!(assets.handle_for_id(AssetId::new_random()), None);
}

/// A weak handle exposes the same index/generation as the strong handle it came from.
#[test]
fn weak_handle_mirrors_strong() {
    let mut assets = Assets::<u32>::new();
    let h = assets.insert(AssetId::new_random(), 3);
    let weak = WeakHandle::new(h);
    assert_eq!(weak.inner(), h);
    assert_eq!(assets.get(&weak.inner()), Some(&3));
}

/// Untyped handles compare by all three fields.
#[test]
fn untyped_handle_equality() {
    let tag = regia_core::id::TypePathHash(1);
    let a = UntypedHandle {
        index: 1,
        generation: 1,
        type_tag: tag,
    };
    let mut b = a;
    assert_eq!(a, b);
    b.generation = 2;
    assert_ne!(a, b);
}

// ---- .meta validation -------------------------------------------------------------------------

/// A nil id is refused on validate, on serialize and on parse.
#[test]
fn meta_rejects_nil_id() {
    let meta = MetaSidecar::new("gltf", AssetId::NIL);
    assert!(matches!(meta.validate(), Err(MetaError::MissingId)));
    assert!(matches!(meta.to_ron_string(), Err(MetaError::MissingId)));

    let text = format!("(format_version: 1, id: \"{NIL_GUID}\", importer: \"gltf\")");
    assert!(matches!(
        MetaSidecar::from_ron(text.as_bytes()),
        Err(MetaError::MissingId)
    ));
}

/// An empty importer name is refused on validate and on parse.
#[test]
fn meta_rejects_empty_importer() {
    let meta = MetaSidecar::new("", AssetId::new_random());
    assert!(matches!(meta.validate(), Err(MetaError::MissingImporter)));
    let text = format!(
        "(format_version: 1, id: \"{}\", importer: \"\")",
        AssetId::new_random()
    );
    assert!(matches!(
        MetaSidecar::from_ron(text.as_bytes()),
        Err(MetaError::MissingImporter)
    ));
}

/// Version 0 has no registered migrator, so it fails with a migration error, not a panic.
#[test]
fn meta_version_zero_fails_migration() {
    let text = format!(
        "(format_version: 0, id: \"{}\", importer: \"gltf\")",
        AssetId::new_random()
    );
    assert!(matches!(
        MetaSidecar::from_ron(text.as_bytes()),
        Err(MetaError::Migration(_))
    ));
    let mut meta = MetaSidecar::new("gltf", AssetId::new_random());
    meta.format_version = 0;
    assert!(matches!(meta.migrate_to_latest(), Err(MetaError::Migration(_))));
    assert!(matches!(
        meta.validate(),
        Err(MetaError::UnsupportedVersion { found: 0, .. })
    ));
}

/// Malformed input of every shape returns `MetaError::Ron`, never panics.
#[test]
fn meta_rejects_malformed_input() {
    let cases: [&[u8]; 5] = [
        b"",
        b"\xff\xfe not utf8",
        b"(importer: \"gltf\")",
        b"[1, 2, 3]",
        b"(format_version: 1, id: ",
    ];
    for bytes in cases {
        assert!(
            matches!(MetaSidecar::from_ron(bytes), Err(MetaError::Ron(_))),
            "input {bytes:?}"
        );
    }
}

/// The version-too-new error carries both numbers.
#[test]
fn meta_newer_version_reports_found_and_supported() {
    let text = "(format_version: 99, anything: [1, 2])";
    match MetaSidecar::from_ron(text.as_bytes()) {
        Err(MetaError::UnsupportedVersion { found, supported }) => {
            assert_eq!(found, 99);
            assert_eq!(supported, regia_asset::META_FORMAT_VERSION);
        }
        other => panic!("expected UnsupportedVersion, got {other:?}"),
    }
}

/// `new` yields a valid current-version meta that survives a RON round trip.
#[test]
fn meta_new_is_valid_and_round_trips() {
    let mut meta = MetaSidecar::new("png", AssetId::new_random());
    meta.settings = Some("(srgb: true)".to_string());
    assert!(meta.validate().is_ok());
    let text = meta.to_ron_string().expect("serialize");
    let back = MetaSidecar::from_ron(text.as_bytes()).expect("parse");
    assert_eq!(back, meta);
}

/// v1 was the first version; v2 (ADR-7819, the `lossless_clips` setting) registers the one
/// `v1 → v2` step, so v1 is the only version a migration step leaves.
#[test]
fn meta_migrates_only_from_v1() {
    assert_eq!(meta_migration_sources(), vec![1]);
}

/// The sidecar path appends `.meta` to the full file name, keeping the directory and any extension.
#[test]
fn meta_path_appends_suffix() {
    assert_eq!(
        meta_path_for(Path::new("assets/models/helmet.gltf")),
        PathBuf::from("assets/models/helmet.gltf.meta")
    );
    assert_eq!(meta_path_for(Path::new("noext")), PathBuf::from("noext.meta"));
}

// ---- manifest ---------------------------------------------------------------------------------

fn entry(path: &str) -> ManifestEntry {
    ManifestEntry {
        path: path.to_string(),
        id: AssetId::new_random(),
        type_path: "regia_asset::CookedMesh".to_string(),
    }
}

/// `from_map` orders entries by path regardless of insertion order, so encoding is deterministic.
#[test]
fn manifest_from_map_sorts_by_path_and_encodes_deterministically() {
    let mut map = BTreeMap::new();
    let (b, a) = (entry("b/mesh.gltf"), entry("a/tex.png"));
    map.insert(b.path.clone(), b.clone());
    map.insert(a.path.clone(), a.clone());
    let manifest = AssetManifest::from_map(map);
    let paths: Vec<&str> = manifest.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, ["a/tex.png", "b/mesh.gltf"]);
    assert_eq!(manifest.id_for_path("a/tex.png"), Some(a.id));
    assert_eq!(manifest.id_for_path("b/mesh.gltf"), Some(b.id));
    assert_eq!(manifest.id_for_path("missing"), None);
    assert_eq!(
        manifest.to_postcard().expect("test setup"),
        manifest.clone().to_postcard().expect("test setup")
    );
}

/// Postcard round trip, and truncated or empty bytes error instead of panicking.
#[test]
fn manifest_postcard_round_trip_and_truncation() {
    let manifest = AssetManifest {
        entries: vec![entry("x.png"), entry("y.png")],
    };
    let bytes = manifest.to_postcard().expect("encode");
    assert_eq!(AssetManifest::from_postcard(&bytes).expect("decode"), manifest);
    assert!(AssetManifest::from_postcard(&bytes[..bytes.len() - 3]).is_err());
    assert!(AssetManifest::from_postcard(&[]).is_err());
}

/// An empty manifest round-trips and finds nothing.
#[test]
fn empty_manifest_round_trips() {
    let manifest = AssetManifest::default();
    let back =
        AssetManifest::from_postcard(&manifest.to_postcard().expect("test setup")).expect("test setup");
    assert_eq!(back, manifest);
    assert_eq!(back.id_for_path("anything"), None);
}

/// A missing cache file is `Ok(None)`; a written one reads back; a corrupt one is a `Postcard` error;
/// a missing directory on write is an `Io` error.
#[test]
fn manifest_cache_io_paths() {
    let dir = unique_temp_dir("asset-manifest");
    assert!(
        AssetManifest::read_from_cache(&dir)
            .expect("absent is ok")
            .is_none()
    );

    let manifest = AssetManifest {
        entries: vec![entry("a.png")],
    };
    manifest.write_to_cache(&dir).expect("write");
    assert_eq!(
        AssetManifest::read_from_cache(&dir).expect("test setup"),
        Some(manifest.clone())
    );

    std::fs::write(dir.join("asset_manifest.bin"), [0xff, 0xff, 0xff, 0xff, 0xff]).expect("test setup");
    assert!(matches!(
        AssetManifest::read_from_cache(&dir),
        Err(ManifestError::Postcard(_))
    ));

    let missing = dir.join("does-not-exist");
    assert!(matches!(
        manifest.write_to_cache(&missing),
        Err(ManifestError::Io(_))
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Paths normalise to `/` separators and drop interior `.` components.
#[test]
fn normalise_asset_path_uses_forward_slashes() {
    assert_eq!(normalise_asset_path(Path::new("a/b/c.png")), "a/b/c.png");
    assert_eq!(normalise_asset_path(Path::new("a/./b.png")), "a/b.png");
    assert_eq!(normalise_asset_path(Path::new("")), "");
}

/// Cache and database locations live under the project's `.regia` directory.
#[test]
fn project_layout_paths() {
    let root = Path::new("proj");
    assert_eq!(
        cooked_cache_dir(root, "linux"),
        PathBuf::from("proj").join(".regia").join("cooked").join("linux")
    );
    assert_eq!(
        asset_db_path(root),
        PathBuf::from("proj")
            .join(".regia")
            .join("assetdb")
            .join("index.bin")
    );
}

// ---- migrator registry ------------------------------------------------------------------------

struct Counting {
    name: &'static str,
    log: Arc<Log>,
    fail: bool,
}

/// Shared call log for the recording migrators.
#[derive(Default)]
struct Log(Mutex<Vec<&'static str>>);

impl ContentMigrator for Counting {
    fn name(&self) -> &'static str {
        self.name
    }

    fn migrate(&self, _project_root: &Path) -> Result<(), MigrationError> {
        if let Ok(mut log) = self.log.0.lock() {
            log.push(self.name);
        }
        if self.fail {
            return Err(MigrationError::Failed {
                name: self.name.to_string(),
                message: "boom".to_string(),
            });
        }
        Ok(())
    }
}

/// An empty registry reports nothing applied.
#[test]
fn empty_registry_reports_nothing() {
    let report = MigratorRegistry::new()
        .upgrade_project(Path::new("."))
        .expect("empty ok");
    assert!(report.applied.is_empty());
    assert!(report.notes.is_empty());
}

/// Migrators run in registration order and each lands in the report.
#[test]
fn registry_runs_in_registration_order() {
    let log = Arc::new(Log::default());
    let mut reg = MigratorRegistry::new();
    for name in ["first", "second", "third"] {
        reg.register(Box::new(Counting {
            name,
            log: Arc::clone(&log),
            fail: false,
        }));
    }
    let report = reg.upgrade_project(Path::new(".")).expect("ok");
    assert_eq!(report.applied, ["first", "second", "third"]);
    assert_eq!(report.notes, ["first: ok", "second: ok", "third: ok"]);
    assert_eq!(*log.0.lock().expect("test setup"), ["first", "second", "third"]);
}

/// The first failure stops the run: later migrators never execute and the error names the failing one.
#[test]
fn registry_stops_at_first_failure() {
    let log = Arc::new(Log::default());
    let mut reg = MigratorRegistry::new();
    for (name, fail) in [("ok", false), ("bad", true), ("never", false)] {
        reg.register(Box::new(Counting {
            name,
            log: Arc::clone(&log),
            fail,
        }));
    }
    match reg.upgrade_project(Path::new(".")) {
        Err(MigrationError::Failed { name, message }) => {
            assert_eq!(name, "bad");
            assert_eq!(message, "boom");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(*log.0.lock().expect("test setup"), ["ok", "bad"]);
}

/// The built-in registry lists the frozen formats in a stable order and succeeds on an empty project.
#[test]
fn default_registry_covers_known_formats() {
    let dir = unique_temp_dir("asset-migrate");
    let report = default_registry().upgrade_project(&dir).expect("no-op migrators");
    assert_eq!(
        report.applied,
        ["meta-v1", "material-ron-v2", "cooked-mesh-lod-metric"]
    );
    assert_eq!(report.notes.len(), 3);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A registration counter proves `register` does not run anything by itself.
#[test]
fn register_alone_does_not_run_migrators() {
    static RUNS: AtomicU32 = AtomicU32::new(0);
    struct Bump;
    impl ContentMigrator for Bump {
        fn name(&self) -> &'static str {
            "bump"
        }
        fn migrate(&self, _: &Path) -> Result<(), MigrationError> {
            RUNS.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }
    let mut reg = MigratorRegistry::new();
    reg.register(Box::new(Bump));
    assert_eq!(RUNS.load(Ordering::Relaxed), 0);
    reg.upgrade_project(Path::new(".")).expect("ok");
    assert_eq!(RUNS.load(Ordering::Relaxed), 1);
}
