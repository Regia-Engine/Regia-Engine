// SPDX-License-Identifier: MPL-2.0
//! Reading a `.fracture.ron` costs about its typed parse (ADR-7703). `FractureSet::from_ron` and
//! `migrate_fracture` used to peek the version first with `regia_core::version::peek_ron_version`,
//! which parses the whole text into a `ron::Value`: a 40-chunk set (3.6 MB) took 193 s to read in a
//! debug build where its typed parse took 74 ms, and the editor's `fracture_tools` test, which reads
//! four such sets, ran out the GPU pre-gate's 600 s on every card. Counted by allocations, not
//! time: the value tree allocates a node per number, the typed parse a vector per array.
//!
//! Its own test binary: it installs the shared counting allocator and counts this thread's
//! requests ([`alloc_counter::measure`]).

use regia_asset_pipeline::fracture::{
    FRACTURE_FORMAT_VERSION, FractureSet, FractureSettings, box_mesh, fracture_mesh, migrate_fracture,
};
use regia_test_utils::alloc_counter::{self, CountingAlloc};

// The shared counting allocator (ADR-3291).
#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

/// An 8-chunk cut of a 2 m cube, as the cooker and the editor's panel write it.
fn set_text() -> (FractureSet, String) {
    let (positions, indices) = box_mesh([1.0, 1.0, 1.0]).expect("box");
    let set = fracture_mesh(&positions, &indices, &FractureSettings::random(8, 7)).expect("fracture");
    let text = set.to_ron().expect("ron");
    (set, text)
}

#[test]
fn reading_a_fracture_set_costs_about_its_typed_parse() {
    let (set, text) = set_text();
    assert_eq!(set.format_version, FRACTURE_FORMAT_VERSION);
    let (typed, typed_work) = alloc_counter::measure(|| ron::from_str::<FractureSet>(&text));
    assert_eq!(typed.expect("typed parse"), set);
    let (read, read_work) = alloc_counter::measure(|| FractureSet::from_ron(&text));
    assert_eq!(read.expect("from_ron"), set);
    let (migrated, migrate_work) = alloc_counter::measure(|| migrate_fracture(&text));
    assert_eq!(
        migrated.expect("migrate"),
        text,
        "the current version comes back unchanged"
    );
    eprintln!(
        "allocations: typed parse {}, from_ron {}, migrate_fracture {} ({} bytes of RON)",
        typed_work.allocs,
        read_work.allocs,
        migrate_work.allocs,
        text.len()
    );
    // The checks after the parse allocate nothing on a well-formed set (measured: from_ron the
    // typed parse's count, migrate_fracture one more for the returned copy). An eighth of the
    // typed parse is headroom; the whole-text value tree of a version peek costs about 1.35 typed
    // parses on its own (8 chunks: typed 10902, the old from_ron 25592, the old migrate 14691).
    let bar = typed_work.allocs + typed_work.allocs / 8;
    assert!(
        read_work.allocs <= bar,
        "from_ron does more than its typed parse: {} allocations against {}",
        read_work.allocs,
        typed_work.allocs
    );
    assert!(
        migrate_work.allocs <= bar,
        "migrate_fracture of a current set does more than its typed parse: {} allocations against {}",
        migrate_work.allocs,
        typed_work.allocs
    );
}

/// The answers of a peek-first read are kept: a text with no readable version gets the peek's
/// error, a newer version the typed parse's error or `UnsupportedVersion`, a corrupt
/// current-version text the typed error (`migrate_fracture` passes it through), a v1 text is
/// upgraded.
#[test]
fn a_read_answers_as_a_peek_first_read() {
    let (set, text) = set_text();
    let no_version = text.replacen("format_version", "formats_version", 1);
    let err = FractureSet::from_ron(&no_version).expect_err("no version");
    assert!(
        err.to_string().contains("no `version` or `format_version` field"),
        "{err}"
    );
    let newer = text.replacen(
        &format!("format_version: {FRACTURE_FORMAT_VERSION}"),
        "format_version: 99",
        1,
    );
    match FractureSet::from_ron(&newer) {
        Err(regia_asset_pipeline::fracture::FractureError::UnsupportedVersion { found, expected }) => {
            assert_eq!((found, expected), (99, FRACTURE_FORMAT_VERSION));
        }
        other => panic!("a newer version is refused by name: {other:?}"),
    }
    assert!(migrate_fracture(&newer).is_err());
    assert!(FractureSet::from_ron("( not ron").is_err());
    // A current-version text the typed parse refuses (a field renamed) but the peek can read: the
    // read answers with the typed parse's error, and `migrate_fracture` returns the text unchanged,
    // as the peek-first code did (it never typed-parsed a current-version text).
    let corrupt = text.replacen("source_volume", "source_volumes", 1);
    match FractureSet::from_ron(&corrupt) {
        Err(regia_asset_pipeline::fracture::FractureError::Format(msg)) => {
            assert!(msg.contains("source_volume"), "{msg}");
        }
        other => panic!("a corrupt current-version text is a format error: {other:?}"),
    }
    assert_eq!(
        migrate_fracture(&corrupt).expect("current version passes through"),
        corrupt
    );
    // v1: the same chunks without render meshes come back with them.
    let mut v1 = set.clone();
    v1.format_version = 1;
    for c in &mut v1.chunks {
        c.render = Default::default();
    }
    let v1_text = v1.to_ron().expect("v1 ron");
    let upgraded = FractureSet::from_ron(&v1_text).expect("v1 upgrades");
    assert_eq!(upgraded.format_version, FRACTURE_FORMAT_VERSION);
    assert_eq!(upgraded.chunks.len(), set.chunks.len());
    assert!(upgraded.chunks.iter().all(|c| !c.render.positions.is_empty()));
    let migrated = migrate_fracture(&v1_text).expect("v1 migrates");
    assert_eq!(
        FractureSet::from_ron(&migrated)
            .expect("migrated reads")
            .format_version,
        FRACTURE_FORMAT_VERSION
    );
}
