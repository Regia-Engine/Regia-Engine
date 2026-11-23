// SPDX-License-Identifier: MPL-2.0
//! What chunked packing puts in each pak: membership by glob, the per-chunk manifest, byte-for-byte
//! repeatability, empty chunks, a missing cooked file, and the migrator's argument errors.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use regia_asset::{
    AssetId, AssetManifest, COOKED_FORMAT_VERSION, CookedHeader, ManifestEntry, PakReader, cooked_file_name,
    encode_cooked_file,
};
use regia_asset_pipeline::{migrate_pak_meshes, pack_cooked_directory, pack_cooked_directory_with};
use regia_core::id::TypePathHash;
use regia_test_utils::unique_temp_dir;

const TYPE_PATH: &str = "regia_asset::CookedImage";

struct Cache {
    dir: std::path::PathBuf,
    ids: BTreeMap<&'static str, AssetId>,
}

fn cache(tag: &str, paths: &[&'static str], missing: &[&str]) -> Cache {
    let dir = unique_temp_dir(tag);
    let mut ids = BTreeMap::new();
    let mut entries = Vec::new();
    for path in paths {
        let id = AssetId::new_random();
        ids.insert(*path, id);
        entries.push(ManifestEntry {
            path: (*path).into(),
            id,
            type_path: TYPE_PATH.into(),
        });
        if missing.contains(path) {
            continue;
        }
        let payload = format!("payload of {path}").into_bytes();
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: TypePathHash::of(TYPE_PATH),
            asset_id: id,
            importer_version: 1,
            dependencies: Vec::new(),
            payload_len: payload.len() as u64,
        };
        std::fs::write(
            dir.join(cooked_file_name(id)),
            encode_cooked_file(&header, &payload).expect("encode"),
        )
        .expect("write");
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    AssetManifest { entries }.write_to_cache(&dir).expect("manifest");
    Cache { dir, ids }
}

fn chunks(spec: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
    spec.iter()
        .map(|(name, globs)| {
            (
                (*name).to_string(),
                globs.iter().map(|g| (*g).to_string()).collect(),
            )
        })
        .collect()
}

fn members(pak: &Path) -> BTreeSet<AssetId> {
    PakReader::open(pak)
        .expect("open")
        .toc
        .entries
        .iter()
        .map(|e| e.id)
        .collect()
}

fn manifest_paths(pak: &Path) -> BTreeSet<String> {
    PakReader::open(pak)
        .expect("open")
        .toc
        .manifest
        .iter()
        .map(|e| e.path.clone())
        .collect()
}

const PATHS: &[&str] = &[
    "assets/props/crate.png",
    "assets/props/barrel.png",
    "assets/props_hd/crate.png",
    "assets/levels/one/ground.png",
    "assets/ui/button.png",
];

#[test]
fn each_chunk_holds_exactly_its_glob_matches_and_its_own_manifest() {
    let c = cache("pakc-members", PATHS, &[]);
    let out = c.dir.join("out");
    let spec = chunks(&[
        ("props", &["assets/props/**"]),
        ("levels", &["assets/levels/**", "assets/ui/button.png"]),
    ]);
    let paks = pack_cooked_directory(&c.dir, &out, &spec, true).expect("pack");
    assert_eq!(paks.len(), 2);
    let by_name = |name: &str| {
        paks.iter()
            .find(|p| {
                p.file_name()
                    .is_some_and(|n| n == format!("chunk_{name}.fpak").as_str())
            })
            .unwrap_or_else(|| panic!("no pak for {name}: {paks:?}"))
    };
    let props = by_name("props");
    assert_eq!(
        members(props),
        BTreeSet::from([c.ids["assets/props/crate.png"], c.ids["assets/props/barrel.png"]]),
        "`props/**` must not take `props_hd/`"
    );
    assert_eq!(
        manifest_paths(props),
        BTreeSet::from([
            "assets/props/crate.png".to_string(),
            "assets/props/barrel.png".to_string()
        ])
    );
    let levels = by_name("levels");
    assert_eq!(
        members(levels),
        BTreeSet::from([
            c.ids["assets/levels/one/ground.png"],
            c.ids["assets/ui/button.png"]
        ])
    );
    // Every stored payload reads back as written.
    let reader = PakReader::open(props).expect("open");
    let id = c.ids["assets/props/crate.png"];
    let bytes = reader.read_decompressed(id).expect("read");
    assert!(bytes.ends_with(b"payload of assets/props/crate.png"));
    let _ = std::fs::remove_dir_all(&c.dir);
}

#[test]
fn a_chunk_that_matches_nothing_writes_no_pak_and_the_others_still_do() {
    let c = cache("pakc-empty", PATHS, &[]);
    let out = c.dir.join("out");
    let spec = chunks(&[("ghost", &["assets/nothing/**"]), ("ui", &["assets/ui/*.png"])]);
    let paks = pack_cooked_directory(&c.dir, &out, &spec, false).expect("pack");
    assert_eq!(paks.len(), 1);
    assert!(paks[0].ends_with("chunk_ui.fpak"));
    assert!(!out.join("chunk_ghost.fpak").exists());
    let _ = std::fs::remove_dir_all(&c.dir);
}

#[test]
fn chunks_with_only_empty_pattern_lists_pack_everything_into_base() {
    let c = cache("pakc-allempty", PATHS, &[]);
    let out = c.dir.join("out");
    let spec = chunks(&[("a", &[]), ("b", &[])]);
    let paks = pack_cooked_directory(&c.dir, &out, &spec, false).expect("pack");
    assert_eq!(paks.len(), 1);
    assert!(paks[0].ends_with("base.fpak"));
    assert_eq!(members(&paks[0]).len(), PATHS.len());
    let _ = std::fs::remove_dir_all(&c.dir);
}

#[test]
fn an_asset_matched_by_two_chunks_ships_in_both() {
    let c = cache("pakc-overlap", PATHS, &[]);
    let out = c.dir.join("out");
    let spec = chunks(&[("a", &["assets/ui/**"]), ("b", &["assets/**/button.png"])]);
    let paks = pack_cooked_directory(&c.dir, &out, &spec, false).expect("pack");
    assert_eq!(paks.len(), 2);
    for pak in &paks {
        assert_eq!(members(pak), BTreeSet::from([c.ids["assets/ui/button.png"]]));
    }
    let _ = std::fs::remove_dir_all(&c.dir);
}

#[test]
fn the_unmatched_placeholder_chunk_is_skipped() {
    let c = cache("pakc-unmatched", PATHS, &[]);
    let out = c.dir.join("out");
    let spec = chunks(&[("__unmatched", &["assets/**"]), ("ui", &["assets/ui/**"])]);
    let paks = pack_cooked_directory(&c.dir, &out, &spec, false).expect("pack");
    assert_eq!(paks.len(), 1);
    assert!(paks[0].ends_with("chunk_ui.fpak"));
    let _ = std::fs::remove_dir_all(&c.dir);
}

#[test]
fn packing_twice_gives_identical_bytes_for_every_pak() {
    let c = cache("pakc-repeat", PATHS, &[]);
    let spec = chunks(&[
        ("props", &["assets/props/**"]),
        ("rest", &["assets/ui/**", "assets/levels/**"]),
    ]);
    for compress in [true, false] {
        let first =
            pack_cooked_directory_with(&c.dir, &c.dir.join("o1"), &spec, compress, false).expect("pack");
        let second =
            pack_cooked_directory_with(&c.dir, &c.dir.join("o2"), &spec, compress, false).expect("pack");
        assert_eq!(first.len(), second.len());
        for (a, b) in first.iter().zip(&second) {
            assert_eq!(
                std::fs::read(a).expect("read"),
                std::fs::read(b).expect("read"),
                "{} (compress {compress})",
                a.display()
            );
        }
        let base_1 = pack_cooked_directory_with(&c.dir, &c.dir.join("b1"), &BTreeMap::new(), compress, false)
            .expect("base");
        let base_2 = pack_cooked_directory_with(&c.dir, &c.dir.join("b2"), &BTreeMap::new(), compress, false)
            .expect("base");
        assert_eq!(
            std::fs::read(&base_1[0]).expect("read"),
            std::fs::read(&base_2[0]).expect("read")
        );
    }
    let _ = std::fs::remove_dir_all(&c.dir);
}

#[test]
fn a_manifest_entry_without_a_cooked_file_is_left_out_of_the_pak_not_an_error() {
    let c = cache("pakc-missing", PATHS, &["assets/ui/button.png"]);
    let out = c.dir.join("out");
    let paks = pack_cooked_directory(&c.dir, &out, &BTreeMap::new(), false).expect("pack");
    let stored = members(&paks[0]);
    assert_eq!(stored.len(), PATHS.len() - 1);
    assert!(!stored.contains(&c.ids["assets/ui/button.png"]));
    let _ = std::fs::remove_dir_all(&c.dir);
}

#[test]
fn the_migrator_reports_a_missing_source_and_a_non_pak() {
    let dir = unique_temp_dir("pakc-migrate");
    assert!(migrate_pak_meshes(&dir.join("absent.fpak"), &dir.join("out.fpak"), true).is_err());
    let junk = dir.join("junk.fpak");
    std::fs::write(&junk, vec![7u8; 200]).expect("write");
    assert!(migrate_pak_meshes(&junk, &dir.join("out.fpak"), true).is_err());
    assert!(
        !dir.join("out.fpak").exists(),
        "a failed migration leaves no output"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
